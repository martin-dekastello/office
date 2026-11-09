//! The folder server end to end: start, refuse a second one, attach, detach,
//! quit. Talks the wire protocol (src/proto.rs) by hand.
#![cfg(unix)]

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_office");

struct Env {
    root: PathBuf,
    folder: PathBuf,
}

impl Env {
    fn new() -> Env {
        // In /tmp rather than macOS's long $TMPDIR, so the socket fits in .office.
        let id = uuid::Uuid::new_v4().simple().to_string();
        let root = std::fs::canonicalize("/tmp").unwrap().join(format!("office-it-{}", &id[..12]));
        let folder = root.join("work");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(root.join("vault")).unwrap();
        std::fs::create_dir_all(root.join("global")).unwrap();
        std::fs::write(
            root.join("config.toml"),
            format!("vault = {:?}\nworktrees_dir = {:?}\n", root.join("vault"), root.join("wt")),
        )
        .unwrap();
        Env { root, folder }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        // Never the real config or the real global state.
        c.env("OFFICE_CONFIG", self.root.join("config.toml"))
            .env("OFFICE_DATA", self.root.join("global"))
            .env_remove("OFFICE_TAB")
            .current_dir(&self.root);
        c
    }

    fn server(&self) -> Child {
        let log = std::fs::File::create(self.root.join("server.log")).unwrap();
        self.cmd().arg("server").arg(&self.folder).stderr(log).spawn().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    fn socket(&self) -> PathBuf {
        self.folder.join(".office/office.sock")
    }

    fn connect(&self) -> UnixStream {
        let start = Instant::now();
        loop {
            match UnixStream::connect(self.socket()) {
                Ok(s) => {
                    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    return s;
                }
                Err(_) if start.elapsed() < Duration::from_secs(10) => std::thread::sleep(Duration::from_millis(50)),
                Err(e) => panic!("no socket: {e}\n{}", std::fs::read_to_string(self.root.join("server.log")).unwrap_or_default()),
            }
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).ok();
    }
}

fn send(s: &mut UnixStream, tag: u8, payload: &[u8]) {
    let mut b = vec![tag];
    b.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    b.extend_from_slice(payload);
    s.write_all(&b).unwrap();
}

fn hello(s: &mut UnixStream, cols: u16, rows: u16) {
    let mut b = cols.to_be_bytes().to_vec();
    b.extend_from_slice(&rows.to_be_bytes());
    send(s, 1, &b);
}

fn key(s: &mut UnixStream, c: char) {
    let ev = Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    send(s, 2, &serde_json::to_vec(&ev).unwrap());
}

fn recv(s: &mut UnixStream) -> Option<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    s.read_exact(&mut head).ok()?;
    let mut b = vec![0u8; u32::from_be_bytes(head[1..].try_into().unwrap()) as usize];
    s.read_exact(&mut b).ok()?;
    Some((head[0], b))
}

/// Read until a message with this tag, returning what printed on the way.
fn until(s: &mut UnixStream, tag: u8) -> (Vec<u8>, Vec<u8>) {
    let mut printed = vec![];
    loop {
        let (t, b) = recv(s).expect("connection ended");
        if t == tag {
            return (b, printed);
        }
        if t == 11 {
            printed.extend_from_slice(&b);
        }
    }
}

fn wait_exit(c: &mut Child) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(10) {
        if let Some(st) = c.try_wait().unwrap() {
            return Some(st);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn text(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned() + &String::from_utf8_lossy(&o.stdout)
}

#[test]
fn one_server_per_folder_with_attach_detach_and_quit() {
    let env = Env::new();
    let mut server = env.server();
    let mut a = env.connect();
    let (welcome, _) = until(&mut a, 10);
    assert_eq!(welcome, [1]); // mouse on by default
    hello(&mut a, 100, 30);
    let (_, _) = until(&mut a, 11);

    // The folder is taken: starting again fails and says how to attach.
    let again = env.run(&[env.folder.to_str().unwrap()]);
    assert!(!again.status.success());
    assert!(text(&again).contains("--attach"), "{}", text(&again));
    let second = env.run(&["server", env.folder.to_str().unwrap()]);
    assert!(!second.status.success());
    assert!(text(&second).contains("already running"), "{}", text(&second));

    // A second client sees the same view, sized to the smaller of the two.
    let mut b = env.connect();
    until(&mut b, 10);
    hello(&mut b, 80, 20);
    let (out, _) = until(&mut b, 11);
    assert!(String::from_utf8_lossy(&out).contains("office"));

    // Detaching one leaves office running for the other.
    key(&mut b, 'D');
    let (why, _) = until(&mut b, 14);
    assert_eq!(why, b"detached");
    assert!(server.try_wait().unwrap().is_none());

    // Quitting ends it for everyone and removes the socket.
    key(&mut a, 'q');
    key(&mut a, 'y');
    let (why, _) = until(&mut a, 14);
    assert_eq!(why, b"office quit");
    let st = wait_exit(&mut server).expect("server didn't quit");
    assert!(st.success());
    assert!(!env.socket().exists());
    assert!(env.folder.join(".office/state.json").exists());
    assert_eq!(std::fs::read_to_string(env.folder.join(".office/.gitignore")).unwrap(), "*\n");
}

#[test]
fn attach_without_a_server_says_how_to_start() {
    let env = Env::new();
    let o = env.run(&["--attach", env.folder.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(text(&o).contains("no office running"), "{}", text(&o));
}

#[test]
fn global_state_moves_into_the_first_folder() {
    let env = Env::new();
    let global = env.root.join("global");
    let state = r#"{"issues":{"X-1":{"worktree":null,"tabs":[{"id":"t1","name":"claude","kind":"claude","claude_session":"s1","cwd":"/somewhere"}]}},"selected":"X-1"}"#;
    std::fs::write(global.join("state.json"), state).unwrap();
    std::fs::create_dir_all(global.join("snapshots")).unwrap();
    std::fs::write(global.join("snapshots/t1.ansi"), "80\nscreen").unwrap();

    let mut server = env.server();
    let mut a = env.connect();
    until(&mut a, 10);
    hello(&mut a, 100, 30);
    key(&mut a, 'q');
    key(&mut a, 'y');
    until(&mut a, 14);
    wait_exit(&mut server).expect("server didn't quit");

    let moved = std::fs::read_to_string(env.folder.join(".office/state.json")).unwrap();
    assert!(moved.contains("\"claude_session\": \"s1\""), "{moved}");
    assert!(moved.contains("/somewhere"));
    assert!(env.folder.join(".office/snapshots/t1.ansi").exists());
    assert!(!global.join("state.json").exists());
    assert_eq!(read_trim(&global.join("migrated")), env.folder.display().to_string());
}

fn read_trim(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap().trim().to_string()
}
