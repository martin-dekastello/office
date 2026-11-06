//! An embedded terminal: a child process on a PTY (ConPTY on Windows)
//! parsed by vt100 and rendered with tui-term.

use anyhow::{Context, Result};
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

    pub fn scroll_by(&mut self, delta: isize) {
        let mut p = self.parser.lock().unwrap();
        if p.screen().alternate_screen() {
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
