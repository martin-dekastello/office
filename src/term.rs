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
    /// Foreground/background colour queries, so programs can pick a light or
    /// dark look. Like tmux, answer with the outer terminal's colours.
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        let theme = crate::theme::get();
        let (r, g, b) = match params {
            [b"10", b"?"] => theme.pane_fg,
            [b"11", b"?"] => theme.pane_bg,
            _ => return,
        };
        let n = std::str::from_utf8(params[0]).unwrap_or_default();
        self.replies
            .push(format!("\x1b]{n};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\").into_bytes());
    }
}

pub type Parser = vt100::Parser<Callbacks>;

const SCROLLBACK: usize = 5000;
/// How far up from the bottom of an exited pane to look for Claude's resume hint.
const RESUME_HINT_LINES: usize = 6;

fn scroll_parser(p: &mut Parser, from: usize, delta: isize) -> usize {
    let want = (from as isize + delta).max(0) as usize;
    p.screen_mut().set_scrollback(want);
    // vt100 clamps to the available history.
    p.screen().scrollback()
}

/// Scrollback and screen as bytes for `Snapshot`: the width, `history`, then
/// one row per line. `history` is left for the replay to wrap.
fn encode(p: &mut Parser, history: &[u8]) -> Vec<u8> {
    let screen = p.screen_mut();
    let keep = screen.scrollback();
    let (rows, cols) = screen.size();
    let mut out = format!("{cols}\n").into_bytes();
    out.extend_from_slice(history);
    let mut push = |screen: &vt100::Screen, n: usize| {
        // A width past the edge turns off wrap tracking, which would emit
        // absolute cursor moves that only fit the original screen.
        for row in screen.rows_formatted(0, cols + 1).take(n) {
            out.extend_from_slice(&row);
            out.extend_from_slice(b"\x1b[m\r\n");
        }
    };
    screen.set_scrollback(usize::MAX);
    let mut left = screen.scrollback();
    while left > 0 {
        screen.set_scrollback(left);
        let n = left.min(rows as usize);
        push(screen, n);
        left -= n;
    }
    screen.set_scrollback(0);
    push(screen, rows as usize);
    screen.set_scrollback(keep);
    // A final newline would scroll the top row away on replay.
    out.truncate(out.len() - 2);
    out
}

/// The session id from Claude's exit hint, if it's at the end of the text.
/// Only the end counts: a session can print resume commands in its own output.
fn parse_resume_id(text: &str) -> Option<String> {
    text.trim_end().lines().rev().take(RESUME_HINT_LINES).find_map(|l| {
        let id = l.split("claude --resume ").nth(1)?.split_whitespace().next()?;
        uuid::Uuid::parse_str(id).ok().map(|_| id.to_string())
    })
}

/// The saved screen of a stopped pane, read-only.
pub struct Snapshot {
    pub parser: Parser,
    pub scroll: usize,
    pub rows: u16,
}

impl Snapshot {
    /// Rebuild at the saved width, so lines keep their shape in any pane.
    fn decode(bytes: &[u8], rows: u16) -> Option<Snapshot> {
        let nl = bytes.iter().position(|b| *b == b'\n')?;
        let cols: u16 = std::str::from_utf8(&bytes[..nl]).ok()?.parse().ok()?;
        let mut parser =
            vt100::Parser::new_with_callbacks(rows.max(2), cols, SCROLLBACK, Callbacks::default());
        parser.process(&bytes[nl + 1..]);
        Some(Snapshot { parser, scroll: 0, rows })
    }

    pub fn load(path: &Path, rows: u16) -> Option<Snapshot> {
        Snapshot::decode(&std::fs::read(path).ok()?, rows)
    }

    pub fn scroll_by(&mut self, delta: isize) {
        self.scroll = scroll_parser(&mut self.parser, self.scroll, delta);
    }
}

/// Rows that have scrolled off the top. vt100 has no getter, but it clamps.
fn history_len(p: &mut Parser) -> usize {
    let keep = p.screen().scrollback();
    p.screen_mut().set_scrollback(usize::MAX);
    let len = p.screen().scrollback();
    p.screen_mut().set_scrollback(keep);
    len
}

/// Text dragged over with the mouse. Lines count from the oldest row of
/// history, so it stays on the same text when the view moves.
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    anchor: (usize, u16),
    head: (usize, u16),
    alt: bool,
}

impl Selection {
    fn new(p: &mut Parser, row: u16, col: u16) -> Selection {
        let at = Selection::point(p, row, col);
        Selection { anchor: at, head: at, alt: p.screen().alternate_screen() }
    }

    fn point(p: &mut Parser, row: u16, col: u16) -> (usize, u16) {
        (history_len(p) - p.screen().scrollback() + row as usize, col)
    }

    fn extend(&mut self, p: &mut Parser, row: u16, col: u16) {
        self.head = Selection::point(p, row, col);
    }

    fn ordered(&self) -> ((usize, u16), (usize, u16)) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    /// The cells to highlight in the current view, as (row, from col, to col).
    pub fn spans(&self, p: &mut Parser) -> Vec<(u16, u16, u16)> {
        if self.alt != p.screen().alternate_screen() {
            return Vec::new();
        }
        let (start, end) = self.ordered();
        let top = history_len(p) - p.screen().scrollback();
        let (rows, cols) = p.screen().size();
        (0..rows)
            .filter_map(|row| {
                let line = top + row as usize;
                if line < start.0 || line > end.0 {
                    return None;
                }
                let from = if line == start.0 { start.1 } else { 0 };
                let to = if line == end.0 { end.1 + 1 } else { cols };
                Some((row, from, to.min(cols)))
            })
            .collect()
    }

    /// The selected text, a screen at a time because vt100 only reads what's in view.
    fn text(&self, p: &mut Parser) -> String {
        let mut out = String::new();
        if self.alt != p.screen().alternate_screen() {
            return out;
        }
        let (start, end) = self.ordered();
        let history = history_len(p);
        let keep = p.screen().scrollback();
        let (rows, cols) = p.screen().size();
        let mut line = start.0;
        while line <= end.0 {
            p.screen_mut().set_scrollback(history.saturating_sub(line));
            let top = history - p.screen().scrollback();
            let last = end.0.min(top + rows as usize - 1);
            if line > last {
                break;
            }
            let (first_row, last_row) = ((line - top) as u16, (last - top) as u16);
            let from = if line == start.0 { start.1 } else { 0 };
            let to = if last == end.0 { end.1 + 1 } else { cols };
            let s = p.screen();
            out.push_str(&s.contents_between(first_row, from, last_row, to.min(cols)));
            if last < end.0 && !s.row_wrapped(last_row) {
                out.push('\n');
            }
            line = last + 1;
        }
        p.screen_mut().set_scrollback(keep);
        out
    }
}

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
    pub selection: Option<Selection>,
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
            SCROLLBACK,
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
            selection: None,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        self.selection = None;
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
        self.selection = None;
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

    pub fn alternate_screen(&self) -> bool {
        self.parser.lock().unwrap().screen().alternate_screen()
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
        self.scroll = scroll_parser(&mut p, self.scroll, delta);
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub fn select_start(&mut self, row: u16, col: u16) {
        self.selection = Some(Selection::new(&mut self.parser.lock().unwrap(), row, col));
    }

    pub fn select_to(&mut self, row: u16, col: u16) {
        if let Some(sel) = &mut self.selection {
            sel.extend(&mut self.parser.lock().unwrap(), row, col);
        }
    }

    /// None if nothing was dragged over, or only blank cells were.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.selection.filter(|s| s.anchor != s.head)?;
        Some(sel.text(&mut self.parser.lock().unwrap())).filter(|t| !t.trim().is_empty())
    }

    pub fn snapshot(&self, history: &[u8]) -> Vec<u8> {
        encode(&mut self.parser.lock().unwrap(), history)
    }

    /// False on the alternate screen, or if nothing has scrolled off yet.
    pub fn has_scrollback(&self) -> bool {
        history_len(&mut self.parser.lock().unwrap()) > 0
    }

    /// The session id Claude printed on exit.
    pub fn resume_id(&self) -> Option<String> {
        let mut p = self.parser.lock().unwrap();
        let keep = p.screen().scrollback();
        p.screen_mut().set_scrollback(0);
        let text = p.screen().contents();
        p.screen_mut().set_scrollback(keep);
        parse_resume_id(&text)
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        if self.exited.is_none() {
            let _ = self.child.kill();
            // Reap it, or it stays a zombie until salash quits.
            let _ = self.child.wait();
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
            let log = std::env::temp_dir().join(format!("salash-test-{}", uuid::Uuid::new_v4()));
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
    fn typing_and_resizing_clear_the_selection() {
        let mut r = Recorder::start("");
        r.pane.select_start(0, 0);
        r.pane.write(b"x");
        assert!(r.pane.selection.is_none());
        r.pane.select_start(0, 0);
        r.pane.resize(6, 40);
        assert!(r.pane.selection.is_none());
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

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    fn parser(rows: u16, cols: u16) -> Parser {
        vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, Callbacks::default())
    }

    fn numbered(p: &mut Parser, n: usize) {
        for i in 0..n {
            p.process(format!("line {i}\r\n").as_bytes());
        }
    }

    fn selected(p: &mut Parser, from: (u16, u16), to: (u16, u16)) -> String {
        let mut sel = Selection::new(p, from.0, from.1);
        sel.extend(p, to.0, to.1);
        sel.text(p)
    }

    #[test]
    fn selection_reads_part_of_a_line() {
        let mut p = parser(5, 20);
        p.process(b"hello world\r\n");
        assert_eq!(selected(&mut p, (0, 6), (0, 10)), "world");
    }

    #[test]
    fn selection_spans_lines_in_either_direction() {
        let mut p = parser(5, 20);
        numbered(&mut p, 3);
        assert_eq!(selected(&mut p, (0, 5), (2, 3)), "0\nline 1\nline");
        assert_eq!(selected(&mut p, (2, 3), (0, 5)), "0\nline 1\nline");
    }

    #[test]
    fn selection_joins_wrapped_lines() {
        let mut p = parser(5, 10);
        p.process(b"0123456789abcdef\r\nnext\r\n");
        assert_eq!(selected(&mut p, (0, 0), (2, 3)), "0123456789abcdef\nnext");
    }

    #[test]
    fn selection_keeps_wide_characters() {
        let mut p = parser(5, 20);
        p.process("a日本b\r\n".as_bytes());
        assert_eq!(selected(&mut p, (0, 0), (0, 5)), "a日本b");
    }

    #[test]
    fn selection_can_be_longer_than_a_screen() {
        let mut p = parser(5, 20);
        numbered(&mut p, 30);
        p.screen_mut().set_scrollback(usize::MAX);
        let mut sel = Selection::new(&mut p, 1, 0);
        p.screen_mut().set_scrollback(0);
        sel.extend(&mut p, 3, 6);
        p.screen_mut().set_scrollback(7);
        let want: Vec<String> = (1..30).map(|i| format!("line {i}")).collect();
        assert_eq!(sel.text(&mut p), want.join("\n"));
        assert_eq!(p.screen().scrollback(), 7);
    }

    #[test]
    fn selection_joins_a_line_wrapped_across_a_screen_boundary() {
        let mut p = parser(2, 4);
        p.process(b"aaaabbbbccccdddd\r\nend\r\n");
        p.screen_mut().set_scrollback(usize::MAX);
        let mut sel = Selection::new(&mut p, 0, 0);
        p.screen_mut().set_scrollback(0);
        sel.extend(&mut p, 0, 2);
        assert_eq!(sel.text(&mut p), "aaaabbbbccccdddd\nend");
    }

    #[test]
    fn selection_stays_on_its_text_when_output_arrives() {
        let mut p = parser(4, 20);
        numbered(&mut p, 3);
        let mut sel = Selection::new(&mut p, 1, 0);
        sel.extend(&mut p, 1, 5);
        assert_eq!(sel.spans(&mut p), [(1, 0, 6)]);
        numbered(&mut p, 1);
        assert_eq!(sel.spans(&mut p), [(0, 0, 6)]);
        assert_eq!(sel.text(&mut p), "line 1");
        numbered(&mut p, 5);
        assert!(sel.spans(&mut p).is_empty());
        assert_eq!(sel.text(&mut p), "line 1");
    }

    #[test]
    fn selection_fills_the_rows_it_passes_through() {
        let mut p = parser(5, 20);
        numbered(&mut p, 3);
        let mut sel = Selection::new(&mut p, 2, 4);
        sel.extend(&mut p, 0, 2);
        assert_eq!(sel.spans(&mut p), [(0, 2, 20), (1, 0, 20), (2, 0, 5)]);
    }

    #[test]
    fn selection_belongs_to_the_screen_it_was_made_on() {
        let mut p = parser(5, 20);
        numbered(&mut p, 2);
        let mut sel = Selection::new(&mut p, 0, 0);
        sel.extend(&mut p, 0, 5);
        p.process(b"\x1b[?1049h");
        assert!(sel.spans(&mut p).is_empty());
        assert_eq!(sel.text(&mut p), "");
    }

    #[test]
    fn snapshot_keeps_history_past_one_page() {
        let mut p = parser(5, 40);
        numbered(&mut p, 23);
        p.screen_mut().set_scrollback(2);
        let bytes = encode(&mut p, b"");
        assert_eq!(p.screen().scrollback(), 2);

        let mut s = Snapshot::decode(&bytes, 5).unwrap();
        assert!(s.parser.screen().contents().contains("line 22"));
        s.scroll_by(10_000);
        assert_eq!(s.scroll, 19);
        assert!(s.parser.screen().contents().starts_with("line 0\n"));
        s.scroll_by(-10_000);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn snapshot_keeps_the_bottom_at_any_height() {
        let mut p = parser(10, 40);
        numbered(&mut p, 30);
        let bytes = encode(&mut p, b"");
        for rows in [4, 10, 25] {
            let s = Snapshot::decode(&bytes, rows).unwrap();
            assert!(s.parser.screen().contents().contains("line 29"), "{rows} rows");
        }
    }

    #[test]
    fn snapshot_keeps_colours_and_wrapped_lines() {
        let mut p = parser(5, 10);
        p.process(b"\x1b[31mred\x1b[m plain\r\n");
        p.process(b"0123456789abcdef\r\n");
        p.process("\x1b[2C\x1b[42m \x1b[m日本\r\n".as_bytes());
        let s = Snapshot::decode(&encode(&mut p, b""), 5).unwrap();
        let screen = s.parser.screen();
        assert_eq!(screen.cell(0, 0).unwrap().fgcolor(), vt100::Color::Idx(1));
        assert_eq!(screen.cell(0, 4).unwrap().fgcolor(), vt100::Color::Default);
        let got: Vec<String> = screen.rows(0, 10).collect();
        assert_eq!(got[0], "red plain");
        assert_eq!(got[1], "0123456789");
        assert_eq!(got[2], "abcdef");
        assert_eq!(screen.cell(3, 2).unwrap().bgcolor(), vt100::Color::Idx(2));
        assert_eq!(screen.cell(3, 3).unwrap().contents(), "日");
    }

    #[test]
    fn history_scrolls_above_an_alternate_screen() {
        let mut p = parser(4, 10);
        p.process(b"\x1b[?1049hscreen");

        let mut s = Snapshot::decode(&encode(&mut p, b"0123456789abc\r\nold\r\n"), 4).unwrap();
        assert!(s.parser.screen().contents().starts_with("screen"));
        s.scroll_by(10_000);
        let got: Vec<String> = s.parser.screen().rows(0, 10).collect();
        assert_eq!(got[..3], ["0123456789", "abc", "old"]);
    }

    #[test]
    fn answers_colour_queries() {
        let mut p = parser(4, 10);
        p.process(b"\x1b]11;?\x07\x1b]10;?\x1b\\\x1b]12;?\x07");
        let bg = crate::theme::get().pane_bg;
        let replies = std::mem::take(&mut p.callbacks_mut().replies);
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0], format!("\x1b]11;rgb:{0:02x}{0:02x}/{1:02x}{1:02x}/{2:02x}{2:02x}\x1b\\", bg.0, bg.1, bg.2).into_bytes());
        assert!(replies[1].starts_with(b"\x1b]10;rgb:"));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(Snapshot::decode(b"", 5).is_none());
        assert!(Snapshot::decode(b"not a width\nrow", 5).is_none());
    }

    #[test]
    fn resume_id_comes_only_from_the_end() {
        let id = "f482d56d-0174-4d64-ba0c-10a88424b79e";
        let hint = format!("Resume this session with:\nclaude --resume {id}\n\n\n");
        assert_eq!(parse_resume_id(&hint).as_deref(), Some(id));

        let old = format!("{hint}{}", "output\n".repeat(RESUME_HINT_LINES + 1));
        assert_eq!(parse_resume_id(&old), None);
        assert_eq!(parse_resume_id("claude --resume <session>\n"), None);
    }
}
