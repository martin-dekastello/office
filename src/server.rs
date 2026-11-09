//! The server of one folder's office: it owns the tabs and keeps them running
//! with or without clients, like a tmux server.
//!
//! Every client sees the same view. The UI is drawn once, at the size of the
//! smallest client (tmux's `window-size smallest`), then copied into a ratatui
//! terminal per client, whose diff becomes the bytes sent to it. See
//! docs/architecture.md.

use crate::app::App;
use crate::config::{self, Config};
use crate::instance::{self, Instance};
use crate::proto::{ClientMsg, ServerMsg};
use anyhow::{Context, Result};
use crossterm::event::Event;
use interprocess::local_socket::{prelude::*, ListenerOptions};
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::{Terminal, TerminalOptions, Viewport};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{BufReader, BufWriter, Read, Write};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const FRAME: Duration = Duration::from_millis(33);
/// How many messages a client may fall behind by. Past that its output is
/// dropped and it gets a full redraw once it catches up, so a slow link
/// never holds up the others.
const BACKLOG: usize = 16;
/// How long quitting waits for clients to get their last message.
const GOODBYE: Duration = Duration::from_secs(1);
/// Starts a client's screen afresh.
const CLEAR: &[u8] = b"\x1b[0m\x1b[H\x1b[2J";

enum Incoming {
    Connected(u64, SyncSender<ServerMsg>, JoinHandle<()>),
    Msg(u64, ClientMsg),
    Gone(u64),
}

/// Where a client's terminal writes, to be taken and sent after each draw.
#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Sink {
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.0.borrow_mut())
    }
}

type ClientTerm = Terminal<CrosstermBackend<Sink>>;

struct Client {
    out: SyncSender<ServerMsg>,
    writer: JoinHandle<()>,
    /// Columns and rows, once it said hello.
    size: Option<(u16, u16)>,
    /// What its screen shows, for the diff. None means it needs a full redraw.
    term: Option<ClientTerm>,
    sink: Sink,
}

impl Client {
    /// Send a message; false if the client is gone.
    fn send(&mut self, m: ServerMsg) -> bool {
        match self.out.try_send(m) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.term = None;
                true
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

struct Server {
    clients: BTreeMap<u64, Client>,
    /// The UI drawn once per frame at the shared size.
    shared: Option<Terminal<TestBackend>>,
    mouse: bool,
}

/// `office server <folder>`: run in the background until quit.
pub fn run(inst: Instance) -> Result<()> {
    inst.ensure_dir()?;
    let _lock = inst.lock()?;
    let global = config::global_data_dir();
    let moved = instance::migrate(&global, &inst.dir)?;
    config::set_data_dir(inst.dir.clone());
    let (cfg, first_run) = Config::load_for(Some(&inst.dir))?;
    // No terminal to ask here, so "auto" falls back to dark.
    crate::theme::init(&cfg.theme);
    let mouse = cfg.mouse;
    let mut app = App::new(cfg, first_run, inst.folder.clone())?;
    if moved {
        app.flash(format!("Your tabs and sessions moved here from {}", config::shorten(&global)));
    }
    let rx = listen(&inst)?;
    eprintln!("office serving {}", inst.folder.display());

    let mut srv = Server { clients: BTreeMap::new(), shared: None, mouse };
    while !app.quit {
        app.tick();
        app.sync_viewer();
        srv.render(&mut app);
        app.resize_visible();
        if app.take_bell() {
            srv.broadcast(ServerMsg::Output(b"\x07".to_vec()));
        }
        match rx.recv_timeout(FRAME) {
            Ok(m) => {
                srv.on(m, &mut app);
                // Take bursts (fast typing, paste) before the next frame.
                while let Ok(m) = rx.try_recv() {
                    srv.on(m, &mut app);
                    if app.quit {
                        break;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    app.save_sessions();
    app.state.selected = app.selected.clone();
    app.state.save();
    srv.goodbye("office quit");
    // While the lock is held, so a new server's socket is never the one removed.
    #[cfg(unix)]
    let _ = std::fs::remove_file(inst.socket_path());
    eprintln!("office quit");
    Ok(())
    // Dropping the app kills the tabs' processes.
}

/// Socket permissions follow `.office`: whoever may enter it may attach.
#[cfg(unix)]
fn socket_mode(dir: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::fs::metadata(dir).map(|m| m.permissions().mode()).unwrap_or(0o700);
    let mut mode = 0o600;
    if dir & 0o010 != 0 {
        mode |= 0o060;
    }
    if dir & 0o001 != 0 {
        mode |= 0o006;
    }
    mode
}

/// Accept clients in the background. Each gets a thread reading its input and
/// one writing its output; everything arrives on the returned channel.
fn listen(inst: &Instance) -> Result<Receiver<Incoming>> {
    // Holding the lock means any socket file left there is stale.
    #[cfg(unix)]
    let _ = std::fs::remove_file(inst.socket_path());
    let listener = ListenerOptions::new()
        .name(inst.socket_name()?)
        .create_sync()
        .context("creating the socket")?;
    // After bind, as macOS can't set it before; until then the umask applies,
    // which only ever allows less.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(inst.socket_path(), std::fs::Permissions::from_mode(socket_mode(&inst.dir)))
            .context("setting the socket's permissions")?;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut next = 0;
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            next += 1;
            if !serve(next, conn, &tx) {
                break;
            }
        }
    });
    Ok(rx)
}

/// One end of a connection shared by its reading and writing threads.
pub struct Conn(pub Arc<interprocess::local_socket::Stream>);

impl Read for Conn {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0).read(b)
    }
}

impl Write for Conn {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        (&*self.0).write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.0).flush()
    }
}

fn serve(id: u64, conn: interprocess::local_socket::Stream, tx: &Sender<Incoming>) -> bool {
    let conn = Arc::new(conn);
    let (recv, send) = (Conn(conn.clone()), Conn(conn));
    let (out, orx) = mpsc::sync_channel::<ServerMsg>(BACKLOG);
    let writer = std::thread::spawn(move || {
        let mut send = BufWriter::new(send);
        for m in orx {
            if m.write(&mut send).is_err() {
                break;
            }
        }
    });
    if tx.send(Incoming::Connected(id, out, writer)).is_err() {
        return false;
    }
    let tx = tx.clone();
    std::thread::spawn(move || {
        let mut recv = BufReader::new(recv);
        while let Ok(Some(m)) = ClientMsg::read(&mut recv) {
            if tx.send(Incoming::Msg(id, m)).is_err() {
                return;
            }
        }
        let _ = tx.send(Incoming::Gone(id));
    });
    true
}

/// Copy the shared view into a client's frame, dotting out what lies beyond it.
fn paint(out: &mut Buffer, shared: &Buffer) {
    let (a, s) = (out.area, shared.area);
    let beyond = Style::new().fg(Color::DarkGray);
    for y in a.top()..a.bottom() {
        for x in a.left()..a.right() {
            if x < s.right() && y < s.bottom() {
                out[(x, y)] = shared[(x, y)].clone();
            } else {
                out[(x, y)].reset();
                out[(x, y)].set_symbol("·").set_style(beyond);
            }
        }
    }
}

/// The size every client can show in full: the smallest of each.
fn shared_size(sizes: impl Iterator<Item = (u16, u16)>) -> Option<(u16, u16)> {
    sizes.reduce(|(c, r), (c2, r2)| (c.min(c2), r.min(r2)))
}

impl Server {
    fn on(&mut self, m: Incoming, app: &mut App) {
        match m {
            Incoming::Connected(id, out, writer) => {
                let mut c = Client { out, writer, size: None, term: None, sink: Sink::default() };
                if c.send(ServerMsg::Welcome { mouse: self.mouse }) {
                    self.clients.insert(id, c);
                }
            }
            Incoming::Msg(id, ClientMsg::Hello { cols, rows } | ClientMsg::Event(Event::Resize(cols, rows))) => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.size = Some((cols.max(1), rows.max(1)));
                    c.term = None;
                }
            }
            Incoming::Msg(id, ClientMsg::Event(ev)) => {
                if !self.clients.contains_key(&id) {
                    return;
                }
                app.handle(ev);
                let outbox = std::mem::take(&mut app.outbox);
                let detach = std::mem::take(&mut app.detach);
                let Some(c) = self.clients.get_mut(&id) else { return };
                let alive = outbox.into_iter().all(|m| c.send(m));
                if detach {
                    // Its writer sends this, then ends as the client is dropped.
                    c.send(ServerMsg::Exit("detached".into()));
                }
                if detach || !alive {
                    self.clients.remove(&id);
                }
            }
            Incoming::Gone(id) => {
                self.clients.remove(&id);
            }
        }
    }

    fn broadcast(&mut self, m: ServerMsg) {
        self.clients.retain(|_, c| c.send(m.clone()));
    }

    fn render(&mut self, app: &mut App) {
        let Some((cols, rows)) = shared_size(self.clients.values().filter_map(|c| c.size)) else { return };
        let area = Rect::new(0, 0, cols, rows);
        if self.shared.as_ref().is_none_or(|t| t.backend().buffer().area != area) {
            self.shared = Terminal::new(TestBackend::new(cols, rows)).ok();
        }
        let Some(shared) = self.shared.as_mut() else { return };
        if shared.draw(|f| crate::ui::draw(app, f)).is_err() {
            return;
        }
        let view = shared.backend().buffer();
        self.clients.retain(|_, c| {
            let Some((cols, rows)) = c.size else { return true };
            if c.term.is_none() {
                let opts = TerminalOptions { viewport: Viewport::Fixed(Rect::new(0, 0, cols, rows)) };
                c.sink.take();
                let _ = c.sink.write_all(CLEAR);
                c.term = Terminal::with_options(CrosstermBackend::new(c.sink.clone()), opts).ok();
            }
            let Some(term) = c.term.as_mut() else { return true };
            if term.draw(|f| paint(f.buffer_mut(), view)).is_err() {
                c.term = None;
                return true;
            }
            let bytes = c.sink.take();
            bytes.is_empty() || c.send(ServerMsg::Output(bytes))
        });
    }

    /// Tell every client office is gone, giving each a moment to hear it.
    fn goodbye(self, reason: &str) {
        let deadline = Instant::now() + GOODBYE;
        let writers: Vec<JoinHandle<()>> = self
            .clients
            .into_values()
            .map(|mut c| {
                c.send(ServerMsg::Exit(reason.into()));
                c.writer
            })
            .collect();
        for w in writers {
            while !w.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Start the folder's server as a background process that outlives this
/// terminal. A fresh exec rather than a fork, which isn't safe once threads exist.
pub fn spawn(inst: &Instance) -> Result<()> {
    use std::process::{Command, Stdio};
    let log = std::fs::File::create(inst.log_path())
        .with_context(|| format!("creating {}", inst.log_path().display()))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("server")
        .arg(&inst.folder)
        .current_dir(&inst.folder)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session: no controlling terminal, so closing this one
        // (or the ssh connection) doesn't hang it up.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().context("starting the office server")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smallest_client_sets_the_size() {
        assert_eq!(shared_size([].into_iter()), None);
        assert_eq!(shared_size([(200, 50)].into_iter()), Some((200, 50)));
        assert_eq!(shared_size([(200, 40), (120, 60), (150, 30)].into_iter()), Some((120, 30)));
    }

    #[test]
    fn a_larger_client_sees_the_view_then_dots() {
        let mut view = Buffer::empty(Rect::new(0, 0, 2, 1));
        view[(0, 0)].set_symbol("a");
        view[(1, 0)].set_symbol("b");
        let mut out = Buffer::empty(Rect::new(0, 0, 3, 2));
        paint(&mut out, &view);
        let row = |y| (0..3).map(|x| out[(x, y)].symbol().to_string()).collect::<String>();
        assert_eq!(row(0), "ab·");
        assert_eq!(row(1), "···");
    }

    #[test]
    fn rendering_for_a_client_needs_no_terminal() {
        let opts = TerminalOptions { viewport: Viewport::Fixed(Rect::new(0, 0, 4, 2)) };
        let sink = Sink::default();
        let mut t = Terminal::with_options(CrosstermBackend::new(sink.clone()), opts).unwrap();
        let mut view = Buffer::empty(Rect::new(0, 0, 4, 2));
        view[(1, 1)].set_symbol("x");
        t.draw(|f| paint(f.buffer_mut(), &view)).unwrap();
        assert!(String::from_utf8_lossy(&sink.take()).contains('x'));
        // Nothing changed, so nothing more to send.
        t.draw(|f| paint(f.buffer_mut(), &view)).unwrap();
        assert!(!String::from_utf8_lossy(&sink.take()).contains('x'));
    }
}
