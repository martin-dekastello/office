//! The folder server end to end: start, refuse a second one, attach, detach,
//! quit, and `salash mcp` against it. Talks the wire protocol (src/proto.rs)
//! and MCP's JSON-RPC by hand.
#![cfg(unix)]

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_salash");

struct Env {
    root: PathBuf,
    folder: PathBuf,
}

impl Env {
    fn new() -> Env {
        Env::with("")
    }

    /// With these lines added to the config.
    fn with(config: &str) -> Env {
        // In /tmp rather than macOS's long $TMPDIR, so the socket fits in .salash.
        let id = uuid::Uuid::new_v4().simple().to_string();
        let root = std::fs::canonicalize("/tmp").unwrap().join(format!("salash-it-{}", &id[..12]));
        let folder = root.join("work");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::create_dir_all(root.join("vault")).unwrap();
        std::fs::create_dir_all(root.join("global")).unwrap();
        std::fs::write(
            root.join("config.toml"),
            format!("vault = {:?}\nworktrees_dir = {:?}\n{config}", root.join("vault"), root.join("wt")),
        )
        .unwrap();
        Env { root, folder }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(BIN);
        // Never the real config or the real global state.
        c.env("SALASH_CONFIG", self.root.join("config.toml"))
            .env("SALASH_DATA", self.root.join("global"))
            .env_remove("SALASH_TAB")
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
        self.folder.join(".salash/salash.sock")
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

/// A client's first message: its size.
fn hello(s: &mut UnixStream, cols: u16, rows: u16) {
    send(s, 2, &serde_json::to_vec(&Event::Resize(cols, rows)).unwrap());
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
    assert_eq!(welcome, [1, 2]); // mouse on by default, protocol version 2
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
    assert!(String::from_utf8_lossy(&out).contains("salash"));

    // Detaching one leaves salash running for the other.
    key(&mut b, 'D');
    let (why, _) = until(&mut b, 14);
    assert_eq!(why, b"detached");
    assert!(server.try_wait().unwrap().is_none());

    // Quitting ends it for everyone and removes the socket.
    key(&mut a, 'q');
    key(&mut a, 'y');
    let (why, _) = until(&mut a, 14);
    assert_eq!(why, b"salash quit");
    let st = wait_exit(&mut server).expect("server didn't quit");
    assert!(st.success());
    assert!(!env.socket().exists());
    assert!(env.folder.join(".salash/state.json").exists());
    assert_eq!(std::fs::read_to_string(env.folder.join(".salash/.gitignore")).unwrap(), "*\n");
}

#[test]
fn kill_stops_it_as_quitting_does() {
    let env = Env::new();
    let mut server = env.server();
    let mut a = env.connect();
    until(&mut a, 10);
    hello(&mut a, 100, 30);
    until(&mut a, 11);

    let pid = read_trim(&env.folder.join(".salash/server.lock"));
    assert_eq!(pid, server.id().to_string());
    let killed = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(killed.success());

    let (why, _) = until(&mut a, 14);
    assert_eq!(why, b"salash quit");
    let st = wait_exit(&mut server).expect("server didn't stop");
    assert!(st.success(), "{st:?}");
    assert!(!env.socket().exists());
    assert!(env.folder.join(".salash/state.json").exists());
}

#[test]
fn attach_without_a_server_says_how_to_start() {
    let env = Env::new();
    let o = env.run(&["--attach", env.folder.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(text(&o).contains("no salash running"), "{}", text(&o));
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

    let moved = std::fs::read_to_string(env.folder.join(".salash/state.json")).unwrap();
    assert!(moved.contains("\"claude_session\": \"s1\""), "{moved}");
    assert!(moved.contains("/somewhere"));
    assert!(env.folder.join(".salash/snapshots/t1.ansi").exists());
    assert!(!global.join("state.json").exists());
    assert_eq!(read_trim(&global.join("migrated")), env.folder.display().to_string());
}

fn read_trim(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap().trim().to_string()
}

// ---------- salash mcp ----------

/// `salash mcp` on piped stdio, spoken to in JSON-RPC lines as Claude Code does.
struct Mcp {
    child: Child,
    stdin: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<serde_json::Value>,
    next: u64,
}

impl Mcp {
    fn start(mut cmd: Command, folder: &Path) -> Mcp {
        use std::io::BufRead;
        let mut child = cmd
            .arg("mcp")
            .arg(folder)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let out = std::io::BufReader::new(child.stdout.take().unwrap());
        let (tx, lines) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for l in out.lines() {
                let Ok(l) = l else { return };
                if tx.send(serde_json::from_str(&l).expect("stdout is only JSON-RPC")).is_err() {
                    return;
                }
            }
        });
        let mut m = Mcp { child, stdin, lines, next: 0 };
        let init = m.call(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            }),
        );
        assert_eq!(init["serverInfo"]["name"], "salash", "{init}");
        m.notify("notifications/initialized");
        m
    }

    fn notify(&mut self, method: &str) {
        writeln!(self.stdin, "{}", serde_json::json!({ "jsonrpc": "2.0", "method": method })).unwrap();
    }

    /// The result of a request; panics on a JSON-RPC error.
    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next += 1;
        let id = self.next;
        let req = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(self.stdin, "{req}").unwrap();
        loop {
            let m = self.lines.recv_timeout(Duration::from_secs(10)).expect("no answer from salash mcp");
            if m["id"] == id {
                assert!(m.get("error").is_none(), "{method}: {m}");
                return m["result"].clone();
            }
        }
    }

    /// A tool's text, and whether it is an error.
    fn tool(&mut self, name: &str, args: serde_json::Value) -> (String, bool) {
        let r = self.call("tools/call", serde_json::json!({ "name": name, "arguments": args }));
        (r["content"][0]["text"].as_str().unwrap_or_default().to_string(), r["isError"] == true)
    }

    fn ok(&mut self, name: &str, args: serde_json::Value) -> serde_json::Value {
        let (text, err) = self.tool(name, args);
        assert!(!err, "{name}: {text}");
        serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text))
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A server stopped when the test ends, as it would outlive it.
struct Stop(Child);

impl Drop for Stop {
    fn drop(&mut self) {
        let _ = Command::new("kill").args(["-TERM", &self.0.id().to_string()]).status();
        wait_exit(&mut self.0);
    }
}

/// Wait for a condition on the server's state, as tabs start in the background.
fn eventually<T>(mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "timed out");
        std::thread::sleep(Duration::from_millis(100));
    }
}

const FAKES: &str = r#"
shell_command = ["sh", "-c", 'printf "\033[1;31m日本\033[m hello\n"; exec cat']
claude_command = ["sh", "-c", 'printf "%s\n" "$@" > argv.txt; exec cat', "claude"]
"#;

#[test]
fn mcp_without_a_server_says_to_start_salash() {
    let env = Env::new();
    let o = env.run(&["mcp", env.folder.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(text(&o).contains("no salash running for") && text(&o).contains("start salash first"), "{}", text(&o));
    assert!(o.stdout.is_empty(), "stdout is for JSON-RPC only");
}

#[test]
fn mcp_against_another_version_says_how_to_stop_it() {
    let env = Env::new();
    std::fs::create_dir_all(env.folder.join(".salash")).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(env.socket()).unwrap();
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            send(&mut s, 10, &[1, 1]); // a version-1 server
            std::thread::sleep(Duration::from_secs(2));
        }
    });
    let o = env.run(&["mcp", env.folder.to_str().unwrap()]);
    assert!(!o.status.success());
    assert!(text(&o).contains("another version") && text(&o).contains("kill $(cat"), "{}", text(&o));
}

#[test]
fn mcp_answers_from_the_running_server() {
    let env = Env::with(FAKES);
    // A suspended Claude tab in Scratch, with a saved screen.
    let dir = env.folder.join(".salash");
    std::fs::create_dir_all(dir.join("snapshots")).unwrap();
    let state = format!(
        r#"{{"issues":{{"scratch":{{"worktree":null,"tabs":[{{"id":"t1","name":"old","kind":"claude","claude_session":"s1","cwd":{:?}}}]}}}}}}"#,
        env.folder
    );
    std::fs::write(dir.join("state.json"), state).unwrap();
    std::fs::write(dir.join("snapshots/t1.ansi"), "40\nsaved \x1b[1mscreen\x1b[m").unwrap();
    let _server = Stop(env.server());
    env.connect();

    let mut m = Mcp::start(env.cmd(), &env.folder);
    let tools = m.call("tools/list", serde_json::json!({}));
    let mut names: Vec<&str> = tools["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["get_issue", "get_tab", "list_issues", "list_tabs", "open_tab", "read_screen"]);

    // Outside a tab there is no "this tab" to default to.
    let (e, err) = m.tool("read_screen", serde_json::json!({}));
    assert!(err && e.contains("not inside a salash tab"), "{e}");

    let issues = m.ok("list_issues", serde_json::json!({}));
    assert_eq!(issues[0]["id"], "scratch", "{issues}");
    assert_eq!(issues[0]["tabs"], 1);
    let old = m.ok("get_tab", serde_json::json!({ "id": "t1" }));
    assert_eq!(old["process"], "suspended", "{old}");
    assert_eq!(old["claude"]["resume"], "claude --resume s1");
    let saved = m.ok("read_screen", serde_json::json!({ "tab": "t1" }));
    assert_eq!(saved, "saved screen\n");

    // A shell tab: opened over MCP, then read back as plain text.
    let opened = m.ok("open_tab", serde_json::json!({ "issue": "scratch", "kind": "shell" }));
    let shell = opened["tab"].as_str().unwrap().to_string();
    let screen = eventually(|| {
        let (t, _) = m.tool("read_screen", serde_json::json!({ "tab": shell }));
        t.contains("hello").then_some(t)
    });
    assert_eq!(screen, "日本 hello\n");
    let tabs = m.ok("list_tabs", serde_json::json!({ "issue": "scratch" }));
    assert_eq!(tabs.as_array().unwrap().len(), 2, "{tabs}");
    let read = m.call("resources/read", serde_json::json!({ "uri": format!("salash://tab/{shell}/screen") }));
    assert_eq!(read["contents"][0]["text"], "日本 hello\n");
    let listed = m.call("resources/list", serde_json::json!({}));
    assert!(listed.to_string().contains("salash://issue/scratch"), "{listed}");

    // The same checks as `salash tab new`, and modes Claude may not choose for itself.
    let (e, err) = m.tool("open_tab", serde_json::json!({ "issue": "scratch", "mode": "yolo" }));
    assert!(err && e.contains("unknown mode"), "{e}");
    for mode in ["bypassPermissions", "dontAsk"] {
        let (e, err) = m.tool("open_tab", serde_json::json!({ "issue": "scratch", "mode": mode }));
        assert!(err && e.contains("not allowed over MCP") && e.contains("salash tab new --mode"), "{e}");
    }
    let (e, err) = m.tool("get_issue", serde_json::json!({ "id": "NOPE-1" }));
    assert!(err && e.contains("no issue NOPE-1"), "{e}");

    // A Claude tab gets salash mcp, as one argument, after the hooks.
    m.ok("open_tab", serde_json::json!({ "issue": "scratch", "mode": "plan", "prompt": "hi there" }));
    let argv = eventually(|| std::fs::read_to_string(env.folder.join("argv.txt")).ok().filter(|a| a.contains("hi there")));
    let config = dir.join("claude-mcp.json");
    let args: Vec<&str> = argv.lines().collect();
    assert!(args.contains(&format!("--mcp-config={}", config.display()).as_str()), "{argv}");
    assert!(args.contains(&"--settings"), "{argv}");
    assert_eq!(args.last(), Some(&"hi there"), "the prompt stays last: {argv}");
    let config: serde_json::Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    assert_eq!(config["mcpServers"]["salash"]["args"], serde_json::json!(["mcp", env.folder]));

    // Inside that shell tab, "this tab" is the default and is marked.
    let mut own = env.cmd();
    own.env("SALASH_TAB", &shell).env("SALASH_ISSUE", "scratch").env("SALASH_DATA", &dir);
    let mut inside = Mcp::start(own, &env.folder);
    let me = inside.ok("get_tab", serde_json::json!({}));
    assert_eq!(me["id"], shell.as_str());
    assert_eq!(me["self"], true);
    assert_eq!(inside.ok("read_screen", serde_json::json!({})), "日本 hello\n");
    assert_eq!(inside.ok("get_issue", serde_json::json!({}))["id"], "scratch");

    // A killed `salash mcp` leaves the server serving the next one.
    drop(m);
    drop(inside);
    let mut again = Mcp::start(env.cmd(), &env.folder);
    assert_eq!(again.ok("list_issues", serde_json::json!({}))[0]["tabs"], 3);
}

#[test]
fn mcp_false_leaves_claude_without_it() {
    let env = Env::with(&format!("{FAKES}mcp = false\n"));
    let _server = Stop(env.server());
    env.connect();
    let mut m = Mcp::start(env.cmd(), &env.folder);
    m.ok("open_tab", serde_json::json!({ "issue": "scratch", "prompt": "go" }));
    let argv = eventually(|| std::fs::read_to_string(env.folder.join("argv.txt")).ok().filter(|a| a.contains("go")));
    assert!(!argv.contains("--mcp-config"), "{argv}");
    assert!(!env.folder.join(".salash/claude-mcp.json").exists());
}
