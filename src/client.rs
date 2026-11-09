//! `office` in your terminal: it starts the folder's server if needed, then
//! passes your keys and mouse to it and prints what it draws. Copying and
//! opening links happen here, on your side of any ssh connection.

use crate::clipboard;
use crate::instance::Instance;
use crate::proto::{ClientMsg, ServerMsg, VERSION};
use crate::server::Conn;
use anyhow::{bail, Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, terminal};
use interprocess::local_socket::{prelude::*, Stream};
use std::io::{BufReader, BufWriter, Write};
use std::time::{Duration, Instant};

/// How long a newly started server may take to open its socket.
const START_TIMEOUT: Duration = Duration::from_secs(5);

fn connect(inst: &Instance) -> Result<Stream> {
    #[cfg(unix)]
    inst.check_socket()?;
    Ok(Stream::connect(inst.socket_name()?)?)
}

/// `office [folder]`: start the folder's office in the background and attach.
pub fn start(inst: &Instance) -> Result<()> {
    #[cfg(unix)]
    inst.check_socket()?;
    if connect(inst).is_ok() {
        bail!(
            "office is already running in {f}\nattach with: office --attach {f}",
            f = inst.folder.display()
        );
    }
    inst.ensure_dir()?;
    crate::server::spawn(inst)?;
    let start = Instant::now();
    let conn = loop {
        match connect(inst) {
            Ok(c) => break c,
            Err(_) if start.elapsed() < START_TIMEOUT => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                let log = std::fs::read_to_string(inst.log_path()).unwrap_or_default();
                let lines: Vec<&str> = log.lines().collect();
                let tail = lines[lines.len().saturating_sub(10)..].join("\n");
                bail!("office didn't start in {}:\n{tail}", inst.folder.display());
            }
        }
    };
    run(inst, conn)
}

/// `office --attach [folder]`: join the folder's running office.
pub fn attach(inst: &Instance) -> Result<()> {
    #[cfg(unix)]
    inst.check_socket()?;
    let conn = connect(inst).map_err(|_| {
        anyhow::anyhow!(
            "no office running in {f}\nstart it with: office {f}",
            f = inst.folder.display()
        )
    })?;
    run(inst, conn)
}

/// Undo the terminal modes the client turned on, in reverse.
fn restore(mouse: bool, keyboard: bool) {
    let mut out = std::io::stdout();
    if keyboard {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if mouse {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(out, DisableBracketedPaste, cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

fn run(inst: &Instance, conn: Stream) -> Result<()> {
    let conn = std::sync::Arc::new(conn);
    let (recv, send) = (Conn(conn.clone()), Conn(conn));
    let mut recv = BufReader::new(recv);
    // The server says how to set up the terminal before drawing anything.
    let mouse = match ServerMsg::read(&mut recv).context("talking to office")? {
        Some(ServerMsg::Welcome { version, mouse }) if version == VERSION => mouse,
        Some(ServerMsg::Welcome { .. }) => bail!(
            "office in {} is another version than this one\nstop it with: kill $(cat {}), then start it again",
            inst.folder.display(),
            inst.lock_path().display()
        ),
        Some(ServerMsg::Exit(why)) => bail!("{why}"),
        _ => bail!("office sent something unexpected"),
    };

    terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    if let Err(e) = execute!(out, EnterAlternateScreen, cursor::Hide, EnableBracketedPaste) {
        restore(false, false);
        return Err(e.into());
    }
    let mouse = mouse && execute!(out, EnableMouseCapture).is_ok();
    let keyboard = matches!(terminal::supports_keyboard_enhancement(), Ok(true))
        && execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))
            .is_ok();
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore(mouse, keyboard);
        prev_hook(info);
    }));

    // The size goes first, as a resize.
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let mut send = BufWriter::new(send);
    if let Err(e) = ClientMsg::Event(Event::Resize(cols, rows)).write(&mut send) {
        restore(mouse, keyboard);
        return Err(e).context("talking to office");
    }
    // Input goes on its own thread; it ends with the process.
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if ClientMsg::Event(ev).write(&mut send).is_err() {
                return;
            }
        }
    });

    let why = loop {
        match ServerMsg::read(&mut recv) {
            Ok(Some(ServerMsg::Output(b))) => {
                let _ = out.write_all(&b);
                let _ = out.flush();
            }
            Ok(Some(ServerMsg::Clipboard(text))) => {
                clipboard::copy(&text);
            }
            Ok(Some(ServerMsg::Open(url))) => {
                if openable(&url) {
                    let _ = open::that_detached(url);
                }
            }
            Ok(Some(ServerMsg::Exit(why))) => break why,
            Ok(Some(ServerMsg::Welcome { .. })) => {}
            Ok(None) | Err(_) => break "office stopped".into(),
        }
    };
    restore(mouse, keyboard);
    println!("[{why}]");
    Ok(())
}

/// Links office opens: web pages for issues and notes in Obsidian. Not files,
/// apps or other schemes, whatever the server asks for.
fn openable(url: &str) -> bool {
    let scheme = url.split_once("://").map(|(s, _)| s.to_ascii_lowercase());
    matches!(scheme.as_deref(), Some("http" | "https" | "obsidian"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_and_obsidian_links_open() {
        assert!(openable("https://example.com/o/r/issues/1"));
        assert!(openable("HTTP://example.com"));
        assert!(openable("obsidian://open?path=%2Fv%2Fx.md"));
        assert!(!openable("file:///etc/passwd"));
        assert!(!openable("/Applications/Calculator.app"));
        assert!(!openable("javascript:alert(1)"));
        assert!(!openable("smb://host/share"));
        assert!(!openable(""));
    }
}
