//! Persisted UI state: per-issue worktree and tabs (with Claude session IDs
//! so tabs come back with `claude --resume` after a restart).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TabKind {
    Claude,
    Shell,
    Notes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabState {
    pub id: String,
    pub name: String,
    pub kind: TabKind,
    /// Set by the user with rename; stops auto-naming from the terminal title.
    #[serde(default)]
    pub custom_name: bool,
    #[serde(default)]
    pub claude_session: Option<String>,
    /// Chosen when the tab was created; overrides the issue's working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IssueState {
    pub worktree: Option<PathBuf>,
    pub tabs: Vec<TabState>,
    #[serde(default)]
    pub active_tab: usize,
    /// Closed Claude tabs that can be reopened, oldest first.
    #[serde(default)]
    pub closed: Vec<TabState>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub issues: BTreeMap<String, IssueState>,
    pub selected: Option<String>,
}

fn path() -> PathBuf {
    crate::config::data_dir().join("state.json")
}

pub fn snapshot_path(tab: &str) -> PathBuf {
    crate::config::data_dir().join("snapshots").join(format!("{tab}.ansi"))
}

pub fn write_snapshot(tab: &str, bytes: &[u8]) {
    let p = snapshot_path(tab);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("ansi.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}

pub fn remove_snapshot(tab: &str) {
    let _ = std::fs::remove_file(snapshot_path(tab));
}

impl State {
    pub fn load() -> State {
        std::fs::read(path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let p = path();
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let tmp = p.with_extension("json.tmp");
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_without_closed_tabs_still_loads() {
        let st: IssueState = serde_json::from_str(r#"{"worktree":null,"tabs":[]}"#).unwrap();
        assert!(st.closed.is_empty());
    }
}
