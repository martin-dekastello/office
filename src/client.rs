//! `office` in your terminal: it starts the folder's server if needed, then
//! passes your keys and mouse to it and prints what it draws. Copying and
//! opening links happen here, on your side of any ssh connection.

use crate::clipboard;
use crate::instance::Instance;
use crate::proto::{ClientMsg, ServerMsg};
use crate::server::Conn;
use anyhow::{bail, Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, terminal};
use interprocess::local_socket::{prelude::*, Stream};
use std::io::{BufReader, BufWriter, Write};
use std::time::{Duration, Instant};

/// How long a newly started server may take to open its socket.
const START_TIMEOUT: Duration = Duration::from_secs(5);

fn connect(inst: &Instance) -> std::io::Result<Stream> {
    Stream::connect(inst.socket_name()?)
}

/// `office [folder]`: start the folder's office in the background and attach.
pub fn start(inst: &Instance) -> Result<()> {
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
                let tail: Vec<&str> = log.lines().rev().take(10).collect();
                let tail: Vec<&str> = tail.into_iter().rev().collect();
                bail!("office didn't start in {}:\n{}", inst.folder.display(), tail.join("\n"));
            }
        }
    };
    run(conn)
}

/// `office --attach [folder]`: join the folder's running office.
pub fn attach(inst: &Instance) -> Result<()> {
    let conn = connect(inst).map_err(|_| {
        anyhow::anyhow!(
            "no office running in {f}\nstart it with: office {f}",
            f = inst.folder.display()
        )
    })?;
    run(conn)
}

/// Terminal modes the client turned on, undone in reverse on the way out.
#[derive(Default)]
struct Modes {
    mouse: bool,
    keyboard: bool,
}

fn setup() -> Result<()> {
    terminal::enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, cursor::Hide, EnableBracketedPaste)?;
    Ok(())
}

fn restore(modes: &Modes) {
    let mut out = std::io::stdout();
    if modes.keyboard {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    if modes.mouse {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(out, DisableBracketedPaste, cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();
}

fn run(conn: Stream) -> Result<()> {
    let conn = std::sync::Arc::new(conn);
    let (recv, send) = (Conn(conn.clone()), Conn(conn));
    let mut recv = BufReader::new(recv);
    // The server says how to set up the terminal before drawing anything.
    let mouse = match ServerMsg::read(&mut recv).context("talking to office")? {
        Some(ServerMsg::Welcome { mouse }) => mouse,
        Some(ServerMsg::Exit(why)) => bail!("{why}"),
        _ => bail!("office sent something unexpected"),
    };

    setup()?;
    let mut modes = Modes::default();
    let mut out = std::io::stdout();
    if mouse {
        modes.mouse = execute!(out, EnableMouseCapture).is_ok();
    }
    if matches!(terminal::supports_keyboard_enhancement(), Ok(true)) {
        modes.keyboard = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
    }
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore(&Modes { mouse: true, keyboard: true });
        prev_hook(info);
    }));

    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let mut send = BufWriter::new(send);
    ClientMsg::Hello { cols, rows }.write(&mut send).context("talking to office")?;
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
                let _ = open::that_detached(url);
            }
            Ok(Some(ServerMsg::Exit(why))) => break why,
            Ok(Some(ServerMsg::Welcome { .. })) => {}
            Ok(None) | Err(_) => break "office stopped".into(),
        }
    };
    restore(&modes);
    println!("[{why}]");
    Ok(())
}
