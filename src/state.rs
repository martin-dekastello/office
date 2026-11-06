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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IssueState {
    pub worktree: Option<PathBuf>,
    pub tabs: Vec<TabState>,
    #[serde(default)]
    pub active_tab: usize,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub issues: BTreeMap<String, IssueState>,
    pub selected: Option<String>,
}

fn path() -> PathBuf {
    crate::config::data_dir().join("state.json")
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
