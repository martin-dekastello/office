//! Claude status tracking via Claude Code hooks.
//!
//! Each tab is spawned with ISSUEDECK_TAB=<tab id>. Claude Code runs
//! `issuedeck hook` on lifecycle events; the hook writes
//! <data>/status/<tab id>.json, which the UI polls.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClaudeStatus {
    /// Process started, no hook seen yet.
    Starting,
    /// Waiting for a prompt.
    Idle,
    /// Thinking / running tools.
    Working,
    /// Blocked on you: permission prompt or question.
    Attention,
    /// Finished a turn (shown until you look at the tab).
    Done,
    /// Session ended / process exited.
    Exited,
}

impl ClaudeStatus {
    /// Higher = more urgent; used to roll tab statuses up to the issue.
    pub fn urgency(self) -> u8 {
        match self {
            ClaudeStatus::Attention => 5,
            ClaudeStatus::Done => 4,
            ClaudeStatus::Working => 3,
            ClaudeStatus::Starting => 2,
            ClaudeStatus::Idle => 1,
            ClaudeStatus::Exited => 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusFile {
    pub status: ClaudeStatus,
    pub session_id: Option<String>,
    pub message: Option<String>,
    pub event: String,
    /// Unix millis.
    pub ts: i64,
}

pub fn status_dir() -> PathBuf {
    crate::config::data_dir().join("status")
}

pub fn status_path(tab: &str) -> PathBuf {
    status_dir().join(format!("{tab}.json"))
}

pub fn read_status(tab: &str) -> Option<StatusFile> {
    let b = std::fs::read(status_path(tab)).ok()?;
    serde_json::from_slice(&b).ok()
}

pub fn clear_status(tab: &str) {
    let _ = std::fs::remove_file(status_path(tab));
}

/// Map a hook payload to a status. `None` = leave the status unchanged.
pub fn map_event(payload: &Value, prev: Option<ClaudeStatus>) -> Option<ClaudeStatus> {
    let ev = payload.get("hook_event_name").and_then(Value::as_str).unwrap_or("");
    match ev {
        "SessionStart" => Some(ClaudeStatus::Idle),
        "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => Some(ClaudeStatus::Working),
        "PermissionRequest" => Some(ClaudeStatus::Attention),
        "Notification" => {
            let kind = payload.get("notification_type").and_then(Value::as_str).unwrap_or("");
            let msg = payload.get("message").and_then(Value::as_str).unwrap_or("").to_lowercase();
            if kind == "permission_prompt"
                || kind == "elicitation_dialog"
                || msg.contains("permission")
                || msg.contains("needs your")
            {
                Some(ClaudeStatus::Attention)
            } else if kind == "idle_prompt" || msg.contains("waiting for your input") {
                // Keep an unseen Done as Done; otherwise it is simply idle.
                match prev {
                    Some(ClaudeStatus::Done) => None,
                    _ => Some(ClaudeStatus::Idle),
                }
            } else {
                None
            }
        }
        "Stop" => Some(ClaudeStatus::Done),
        "SessionEnd" => Some(ClaudeStatus::Exited),
        _ => None,
    }
}

/// `issuedeck hook`: called by Claude Code. Never fails loudly, so a global
/// hook install can't break claude sessions started outside issuedeck.
pub fn run_hook() -> Result<()> {
    let Ok(tab) = std::env::var("ISSUEDECK_TAB") else { return Ok(()) };
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let prev = read_status(&tab);
    let Some(status) = map_event(&payload, prev.as_ref().map(|p| p.status)) else {
        return Ok(());
    };
    let session_id = payload
        .get("session_id")
        .and_then(Value::as_str)
        .map(String::from)
        .or_else(|| prev.as_ref().and_then(|p| p.session_id.clone()));
    let file = StatusFile {
        status,
        session_id,
        message: payload.get("message").and_then(Value::as_str).map(String::from),
        event: payload
            .get("hook_event_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        ts: chrono::Utc::now().timestamp_millis(),
    };
    let dir = status_dir();
    std::fs::create_dir_all(&dir)?;
    let path = status_path(&tab);
    let tmp = dir.join(format!("{tab}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec(&file)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

const EVENTS: &[(&str, bool)] = &[
    ("SessionStart", false),
    ("UserPromptSubmit", false),
    ("PreToolUse", true),
    ("PostToolUse", true),
    ("PermissionRequest", true),
    ("Notification", false),
    ("Stop", false),
    ("SessionEnd", false),
];

fn hook_command() -> Result<String> {
    let exe = std::env::current_exe().context("locating issuedeck executable")?;
    Ok(format!("\"{}\" hook", exe.display()))
}

fn hooks_value(cmd: &str) -> Value {
    let mut hooks = serde_json::Map::new();
    for (ev, tool) in EVENTS {
        let mut entry = json!({ "hooks": [{ "type": "command", "command": cmd, "timeout": 5 }] });
        if *tool {
            entry["matcher"] = json!("*");
        }
        hooks.insert(ev.to_string(), json!([entry]));
    }
    Value::Object(hooks)
}

/// Write the settings file passed to `claude --settings`. Returns its path.
pub fn write_settings_file() -> Result<PathBuf> {
    let path = crate::config::data_dir().join("claude-hooks.json");
    std::fs::create_dir_all(path.parent().unwrap())?;
    let v = json!({ "hooks": hooks_value(&hook_command()?) });
    std::fs::write(&path, serde_json::to_vec_pretty(&v)?)?;
    Ok(path)
}

fn claude_settings_path() -> PathBuf {
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".claude"));
    base.join("settings.json")
}

/// `issuedeck install-hooks`: merge hooks into ~/.claude/settings.json
/// (fallback for setups where `--settings` injection is disabled).
pub fn install_global() -> Result<PathBuf> {
    let path = claude_settings_path();
    let mut root: Value = if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        std::fs::write(path.with_extension("json.issuedeck-backup"), &text)?;
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?
    } else {
        json!({})
    };
    let cmd = hook_command()?;
    let ours = hooks_value(&cmd);
    let obj = root.as_object_mut().context("settings.json is not an object")?;
    let hooks = obj.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().context("`hooks` is not an object")?;
    for (ev, entries) in ours.as_object().unwrap() {
        let list = hooks.entry(ev.clone()).or_insert_with(|| json!([]));
        let Some(arr) = list.as_array_mut() else { continue };
        let already = arr.iter().any(|e| {
            e.get("hooks")
                .and_then(Value::as_array)
                .map(|hs| hs.iter().any(|h| h.get("command").and_then(Value::as_str).is_some_and(|c| c.ends_with("issuedeck\" hook") || c == cmd)))
                .unwrap_or(false)
        });
        if !already {
            arr.extend(entries.as_array().unwrap().iter().cloned());
        }
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&path, serde_json::to_vec_pretty(&root)?)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping() {
        let s = |v: Value| map_event(&v, None);
        assert_eq!(s(json!({"hook_event_name":"UserPromptSubmit"})), Some(ClaudeStatus::Working));
        assert_eq!(s(json!({"hook_event_name":"Stop"})), Some(ClaudeStatus::Done));
        assert_eq!(
            s(json!({"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"})),
            Some(ClaudeStatus::Attention)
        );
        assert_eq!(
            map_event(&json!({"hook_event_name":"Notification","notification_type":"idle_prompt"}), Some(ClaudeStatus::Done)),
            None
        );
        assert_eq!(s(json!({"hook_event_name":"SubagentStop"})), None);
    }
}
