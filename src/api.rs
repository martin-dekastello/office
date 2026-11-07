//! Opening tabs from outside the UI.
//!
//! `office tab new` writes a request to <data>/requests/<id>.json; the UI
//! picks it up on its next poll, opens the tab, and answers with
//! <id>.reply.json.

use crate::state::TabKind;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long `office tab new` waits for the UI to answer.
const REPLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Values for `claude --permission-mode`.
pub const MODES: &[&str] = &["plan", "auto", "acceptEdits", "manual", "dontAsk", "bypassPermissions"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub issue: String,
    pub kind: TabKind,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub name: Option<String>,
    /// Claude only: permission mode for the first launch.
    #[serde(default)]
    pub mode: Option<String>,
    /// Claude only: first prompt.
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    #[serde(default)]
    pub tab: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Reply {
    pub fn ok(tab: String) -> Reply {
        Reply { ok: true, tab: Some(tab), error: None }
    }

    pub fn err(e: impl Into<String>) -> Reply {
        Reply { ok: false, tab: None, error: Some(e.into()) }
    }
}

impl Request {
    /// Reject what the UI couldn't act on, before it is sent.
    pub fn check(&self) -> Result<()> {
        if let Some(m) = &self.mode {
            if !MODES.contains(&m.as_str()) {
                bail!("unknown mode '{m}' (one of: {})", MODES.join(", "));
            }
        }
        if self.kind != TabKind::Claude && (self.mode.is_some() || self.prompt.is_some()) {
            bail!("--mode and --prompt-file are only for Claude tabs");
        }
        if let Some(d) = &self.cwd {
            if !d.is_absolute() || !d.is_dir() {
                bail!("not a directory: {}", d.display());
            }
        }
        Ok(())
    }

    /// Arguments for the first launch of a Claude tab. Never used for `--resume`.
    pub fn claude_args(&self) -> Vec<String> {
        let mut v = vec![];
        if let Some(m) = &self.mode {
            v.push("--permission-mode".into());
            v.push(m.clone());
        }
        if let Some(n) = &self.name {
            v.push("-n".into());
            v.push(n.clone());
        }
        if let Some(p) = &self.prompt {
            v.push(p.clone());
        }
        v
    }
}

pub fn requests_dir() -> PathBuf {
    crate::config::data_dir().join("requests")
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// `office tab new`: hand the request to the running UI and wait for its answer.
pub fn submit(req: &Request) -> Result<Reply> {
    submit_in(&requests_dir(), req, REPLY_TIMEOUT)
}

fn submit_in(dir: &Path, req: &Request, timeout: Duration) -> Result<Reply> {
    req.check()?;
    std::fs::create_dir_all(dir)?;
    let id = uuid::Uuid::new_v4().to_string();
    let path = dir.join(format!("{id}.json"));
    let reply = dir.join(format!("{id}.reply"));
    write_atomic(&path, &serde_json::to_vec(req)?).context("writing request")?;
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Ok(b) = std::fs::read(&reply) {
            let _ = std::fs::remove_file(&reply);
            return serde_json::from_slice(&b).context("reading reply");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Withdrawn, so a later office start doesn't open it by surprise.
    if std::fs::remove_file(&path).is_ok() {
        bail!("office doesn't seem to be running");
    }
    bail!("office took the request but didn't answer");
}

/// Requests waiting for the UI. Each is claimed by renaming it first, so two
/// office instances never both open it.
pub fn take_requests() -> Vec<(String, Result<Request>)> {
    take_in(&requests_dir())
}

fn take_in(dir: &Path) -> Vec<(String, Result<Request>)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut out = vec![];
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else { continue };
        let claimed = path.with_extension("claimed");
        if std::fs::rename(&path, &claimed).is_err() {
            continue;
        }
        let req = std::fs::read(&claimed)
            .context("reading request")
            .and_then(|b| serde_json::from_slice(&b).context("parsing request"));
        let _ = std::fs::remove_file(&claimed);
        out.push((id, req));
    }
    out
}

pub fn reply(id: &str, r: &Reply) {
    reply_in(&requests_dir(), id, r);
}

fn reply_in(dir: &Path, id: &str, r: &Reply) {
    if let Ok(b) = serde_json::to_vec(r) {
        let _ = write_atomic(&dir.join(format!("{id}.reply")), &b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> Request {
        Request { issue: "TS-1".into(), kind: TabKind::Claude, cwd: None, name: None, mode: None, prompt: None }
    }

    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("office-test-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn claude_args_put_the_prompt_last() {
        let r = Request { mode: Some("plan".into()), name: Some("probe".into()), prompt: Some("# hi".into()), ..req() };
        assert_eq!(r.claude_args(), ["--permission-mode", "plan", "-n", "probe", "# hi"]);
        assert!(req().claude_args().is_empty());
    }

    #[test]
    fn bad_requests_are_rejected_before_sending() {
        assert!(req().check().is_ok());
        assert!(Request { mode: Some("yolo".into()), ..req() }.check().is_err());
        assert!(Request { kind: TabKind::Shell, prompt: Some("x".into()), ..req() }.check().is_err());
        assert!(Request { cwd: Some("relative".into()), ..req() }.check().is_err());
        assert!(Request { cwd: Some(temp()), ..req() }.check().is_err());
    }

    #[test]
    fn a_request_round_trips_through_the_ui() {
        let dir = temp();
        let r = Request { prompt: Some("brief".into()), ..req() };
        let ui = {
            let dir = dir.clone();
            std::thread::spawn(move || loop {
                let got = take_in(&dir);
                if let Some((id, req)) = got.into_iter().next() {
                    let req = req.unwrap();
                    reply_in(&dir, &id, &Reply::ok(req.prompt.unwrap()));
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            })
        };
        let got = submit_in(&dir, &r, Duration::from_secs(5)).unwrap();
        ui.join().unwrap();
        assert_eq!(got, Reply::ok("brief".into()));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn an_unanswered_request_is_withdrawn() {
        let dir = temp();
        let e = submit_in(&dir, &req(), Duration::from_millis(100)).unwrap_err();
        assert!(e.to_string().contains("doesn't seem to be running"));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).ok();
    }
}
