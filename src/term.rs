//! An embedded terminal: a child process on a PTY (ConPTY on Windows)
//! parsed by vt100 and rendered with tui-term.

use crate::keys;
use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct Callbacks {
    pub title: Option<String>,
    pub bell: bool,
    /// Replies to terminal queries (cursor position, device attributes).
    pub replies: Vec<Vec<u8>>,
}

impl vt100::Callbacks for Callbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = Some(String::from_utf8_lossy(title).into_owned());
    }
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bell = true;
    }
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let p0 = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, p0) {
            (None, 'n', 6) => {
                let (r, col) = screen.cursor_position();
                self.replies.push(format!("\x1b[{};{}R", r + 1, col + 1).into_bytes());
            }
            (None, 'n', 5) => self.replies.push(b"\x1b[0n".to_vec()),
            (None, 'c', 0) => self.replies.push(b"\x1b[?62;22c".to_vec()),
            (Some(b'>'), 'c', 0) => self.replies.push(b"\x1b[>0;0;0c".to_vec()),
            _ => {}
        }
    }
}

pub type Parser = vt100::Parser<Callbacks>;

pub struct Pane {
    pub parser: Arc<Mutex<Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    pub exited: Option<u32>,
    size: (u16, u16),
    pub dirty: Arc<AtomicBool>,
    /// Lines scrolled back from the bottom (0 = live).
    pub scroll: usize,
}

impl Pane {
    pub fn spawn(
        argv: &[String],
        cwd: &Path,
        env: &[(String, String)],
        rows: u16,
        cols: u16,
    ) -> Result<Pane> {
        let rows = rows.max(2);
        let cols = cols.max(10);
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .context("opening pty")?;
        let mut cmd = CommandBuilder::new(argv.first().context("empty command")?);
        cmd.args(&argv[1..]);
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawning {}", argv.join(" ")))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer()?));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            5000,
            Callbacks::default(),
        )));
        let dirty = Arc::new(AtomicBool::new(true));
        {
            let parser = parser.clone();
            let dirty = dirty.clone();
            let writer = writer.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 16 * 1024];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            let replies = {
                                let mut p = parser.lock().unwrap();
                                p.process(&buf[..n]);
                                std::mem::take(&mut p.callbacks_mut().replies)
                            };
                            if !replies.is_empty() {
                                let mut w = writer.lock().unwrap();
                                for r in replies {
                                    let _ = w.write_all(&r);
                                }
                                let _ = w.flush();
                            }
                            dirty.store(true, Ordering::Relaxed);
                        }
                    }
                }
                dirty.store(true, Ordering::Relaxed);
            });
        }
        Ok(Pane {
            parser,
            writer,
            master: pair.master,
            child,
            exited: None,
            size: (rows, cols),
            dirty,
            scroll: 0,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        if self.scroll != 0 {
            self.scroll = 0;
            self.parser.lock().unwrap().screen_mut().set_scrollback(0);
        }
        let mut w = self.writer.lock().unwrap();
        let _ = w.write_all(bytes);
        let _ = w.flush();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(2);
        let cols = cols.max(10);
        if self.size == (rows, cols) {
            return;
        }
        self.size = (rows, cols);
        let _ = self
            .master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        self.parser.lock().unwrap().screen_mut().set_size(rows, cols);
    }

    /// Poll for process exit.
    pub fn poll_exit(&mut self) -> bool {
        if self.exited.is_none() {
            if let Ok(Some(st)) = self.child.try_wait() {
                self.exited = Some(st.exit_code());
                self.dirty.store(true, Ordering::Relaxed);
            }
        }
        self.exited.is_some()
    }

    /// Terminal title with Claude's leading spinner glyph stripped.
    pub fn title(&self) -> Option<String> {
        let p = self.parser.lock().unwrap();
        let t = p.callbacks().title.as_deref()?;
        let t = t.trim_start_matches(|c: char| !c.is_alphanumeric()).trim();
        (!t.is_empty()).then(|| t.to_string())
    }

    pub fn take_bell(&self) -> bool {
        std::mem::take(&mut self.parser.lock().unwrap().callbacks_mut().bell)
    }

    pub fn application_cursor(&self) -> bool {
        self.parser.lock().unwrap().screen().application_cursor()
    }

    pub fn bracketed_paste(&self) -> bool {
        self.parser.lock().unwrap().screen().bracketed_paste()
    }

    /// Report a mouse event (in pane coordinates) to the child. False if it
    /// didn't ask for this event.
    pub fn send_mouse(&mut self, m: &MouseEvent) -> bool {
        let bytes = {
            let p = self.parser.lock().unwrap();
            let s = p.screen();
            keys::encode_mouse(m, s.mouse_protocol_mode(), s.mouse_protocol_encoding())
        };
        match bytes {
            Some(b) => {
                self.write(&b);
                true
            }
            None => false,
        }
    }

    /// Scroll a page. Apps that draw their own screen get the key, so they
    /// choose how far to go; everything else moves our scrollback by `lines`.
    pub fn scroll_page(&mut self, up: bool, lines: isize) {
        let child_scrolls = {
            let p = self.parser.lock().unwrap();
            let s = p.screen();
            s.alternate_screen() || s.mouse_protocol_mode() != vt100::MouseProtocolMode::None
        };
        if child_scrolls {
            let code = if up { KeyCode::PageUp } else { KeyCode::PageDown };
            self.write(&keys::encode(&KeyEvent::new(code, KeyModifiers::NONE), false));
        } else {
            self.scroll_by(if up { lines } else { -lines });
        }
    }

    pub fn scroll_by(&mut self, delta: isize) {
        let mut p = self.parser.lock().unwrap();
        if p.screen().alternate_screen() {
            // No scrollback here, so send arrows like xterm's alternate scroll.
            let code = if delta > 0 { KeyCode::Up } else { KeyCode::Down };
            let key = keys::encode(&KeyEvent::new(code, KeyModifiers::NONE), p.screen().application_cursor());
            drop(p);
            self.write(&key.repeat(delta.unsigned_abs()));
            return;
        }
        let want = (self.scroll as isize + delta).max(0) as usize;
        p.screen_mut().set_scrollback(want);
        // vt100 clamps to the available history.
        self.scroll = p.screen().scrollback();
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        if self.exited.is_none() {
            let _ = self.child.kill();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crossterm::event::MouseEventKind;
    use std::time::{Duration, Instant};

    fn wait_until(mut done: impl FnMut() -> bool) {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A child that prints `setup`, then records everything it receives.
    struct Recorder {
        pane: Pane,
        log: std::path::PathBuf,
    }

    impl Recorder {
        fn start(setup: &str) -> Recorder {
            let log = std::env::temp_dir().join(format!("office-test-{}", uuid::Uuid::new_v4()));
            let script = format!("stty raw -echo; printf '{setup}'; echo ready; exec cat > '{}'", log.display());
            let argv = ["sh".to_string(), "-c".to_string(), script];
            let pane = Pane::spawn(&argv, &std::env::temp_dir(), &[], 5, 40).unwrap();
            wait_until(|| pane.parser.lock().unwrap().screen().contents().contains("ready"));
            wait_until(|| log.exists());
            Recorder { pane, log }
        }

        fn expect(&self, want: &[u8]) {
            wait_until(|| std::fs::read(&self.log).is_ok_and(|got| got == want));
        }
    }

    impl Drop for Recorder {
        fn drop(&mut self) {
            std::fs::remove_file(&self.log).ok();
        }
    }

    fn wheel() -> MouseEvent {
        MouseEvent { kind: MouseEventKind::ScrollUp, column: 2, row: 3, modifiers: KeyModifiers::NONE }
    }

    #[test]
    fn mouse_reaches_a_child_that_asked_for_it() {
        let mut r = Recorder::start("\\033[?1049h\\033[?1000h\\033[?1006h");
        assert!(r.pane.send_mouse(&wheel()));
        r.expect(b"\x1b[<64;3;4M");
    }

    #[test]
    fn alternate_screen_without_mouse_gets_keys() {
        let mut r = Recorder::start("\\033[?1049h");
        assert!(!r.pane.send_mouse(&wheel()));
        r.pane.scroll_by(2);
        r.pane.scroll_by(-1);
        r.pane.scroll_page(true, 10);
        r.expect(b"\x1b[A\x1b[A\x1b[B\x1b[5~");
    }

    #[test]
    fn main_screen_uses_local_scrollback() {
        let mut r = Recorder::start("1\\r\\n2\\r\\n3\\r\\n4\\r\\n5\\r\\n6\\r\\n7\\r\\n8\\r\\n");
        assert!(!r.pane.send_mouse(&wheel()));
        r.pane.scroll_by(2);
        assert_eq!(r.pane.scroll, 2);
        r.pane.scroll_page(true, 1);
        assert_eq!(r.pane.scroll, 3);
        r.pane.scroll_page(false, 3);
        assert_eq!(r.pane.scroll, 0);
    }
}
