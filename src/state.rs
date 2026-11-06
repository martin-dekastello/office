//! Persisted UI state: per-issue worktree and tabs (with Claude session IDs
//! so tabs come back with `claude --resume` after a restart).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

impl TabState {
    /// Directory for a tab on an issue with no repo. A session only resumes where
    /// it started, so the default is for new tabs and is kept once chosen. A session
    /// with no directory predates the default, so it started in home.
    pub fn settle_dir(&mut self, default: &Path, home: &Path) -> PathBuf {
        let dir = if self.claude_session.is_some() { home } else { default };
        self.cwd.get_or_insert_with(|| dir.to_path_buf());
        dir.to_path_buf()
    }
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
    /// Where tabs start on issues with no repo. None means home.
    #[serde(default)]
    pub default_dir: Option<PathBuf>,
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

    fn tab(session: Option<&str>, cwd: Option<&str>) -> TabState {
        TabState {
            id: "t".into(),
            name: "claude".into(),
            kind: TabKind::Claude,
            custom_name: false,
            claude_session: session.map(Into::into),
            cwd: cwd.map(Into::into),
        }
    }

    #[test]
    fn state_without_closed_tabs_still_loads() {
        let st: IssueState = serde_json::from_str(r#"{"worktree":null,"tabs":[]}"#).unwrap();
        assert!(st.closed.is_empty());
    }

    #[test]
    fn state_from_before_the_default_dir_still_loads() {
        let json = r#"{"issues":{"i":{"worktree":null,"tabs":[
            {"id":"t","name":"claude","kind":"claude","claude_session":"s"}
        ]}},"selected":"i"}"#;
        let st: State = serde_json::from_str(json).unwrap();
        assert_eq!(st.default_dir, None);
        assert_eq!(st.issues["i"].tabs[0].claude_session.as_deref(), Some("s"));
        assert_eq!(st.issues["i"].tabs[0].cwd, None);
    }

    #[test]
    fn a_new_tab_starts_in_the_default_and_stays_there() {
        let mut t = tab(None, None);
        assert_eq!(t.settle_dir(Path::new("/work"), Path::new("/home")), Path::new("/work"));
        assert_eq!(t.cwd.as_deref(), Some(Path::new("/work")));

        t.claude_session = Some("s".into());
        t.settle_dir(Path::new("/elsewhere"), Path::new("/home"));
        assert_eq!(t.cwd.as_deref(), Some(Path::new("/work")));
    }

    #[test]
    fn a_saved_session_with_no_directory_resumes_from_home() {
        let mut t = tab(Some("s"), None);
        assert_eq!(t.settle_dir(Path::new("/work"), Path::new("/home")), Path::new("/home"));
        assert_eq!(t.cwd.as_deref(), Some(Path::new("/home")));
    }

    #[test]
    fn a_session_keeps_its_directory_while_it_is_missing() {
        let mut t = tab(Some("s"), Some("/unmounted"));
        assert_eq!(t.settle_dir(Path::new("/work"), Path::new("/home")), Path::new("/home"));
        assert_eq!(t.cwd.as_deref(), Some(Path::new("/unmounted")));
    }
}
