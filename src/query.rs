//! What `salash mcp` asks a folder's server, and what the server answers.
//!
//! The server answers from the live state on its main loop, so every answer
//! is cheap to build: no vt100 replay of saved screens (the asker reads those
//! itself) and no git.

use crate::api::Request;
use crate::app::{App, IssueRt, Tab, SCRATCH};
use crate::state::{self, TabKind, TabState};
use crate::status::{self, ClaudeStatus};
use crate::vault::Stage;
use crate::proto::Changed;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

/// The only modes a Claude session may pick for a tab it opens over MCP: both
/// ask before every edit, so the new session never has fewer checks than the
/// one asking. With no mode the tab gets the user's own default. The others
/// (`acceptEdits`, `auto`, `dontAsk`, `bypassPermissions`) are for the user,
/// with `salash tab new`. An allow-list matched exactly, so anything it
/// doesn't know is refused.
pub const MODES_OVER_MCP: &[&str] = &["plan", "manual"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Query {
    /// Every issue, with its tabs.
    State,
    /// One issue, with its tabs.
    Issue(String),
    /// One tab.
    Tab(String),
    /// A tab's text: its last `lines` rows, reaching into scrollback if asked.
    Screen { tab: String, lines: usize, scrollback: bool },
    /// Open a tab, as `salash tab new` does.
    OpenTab(Request),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    State(State),
    Issue(Box<Issue>),
    Tab(Box<TabInfo>),
    /// Plain text, at most `term::MAX_TEXT` bytes.
    Screen(String),
    /// The tab has no process; this file holds its saved screen.
    Saved(PathBuf),
    /// The id of the tab opened.
    Opened(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub issues: Vec<Issue>,
    /// The issue selected in the sidebar.
    pub selected: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub title: String,
    /// None for Scratch, which has no note.
    pub stage: Option<Stage>,
    pub source: Option<String>,
    pub url: Option<String>,
    pub repo: Option<String>,
    pub note: Option<PathBuf>,
    pub worktree: Option<PathBuf>,
    pub created: Option<String>,
    pub moved: Option<String>,
    /// The most urgent status of its running Claude tabs, as the sidebar shows it.
    pub status: Option<ClaudeStatus>,
    pub tabs: Vec<TabInfo>,
    /// Closed Claude tabs that can be reopened, oldest first.
    pub closed: Vec<ClosedTab>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Process {
    Running,
    /// Exited and still open: a Claude that failed to start.
    Exited,
    /// A Claude session with no process; resuming continues it.
    Suspended,
    /// Waiting for the issue's worktree to be created.
    Waiting,
    /// Not started: a tab of an issue nobody opened since salash started.
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: String,
    pub issue: String,
    pub name: String,
    pub kind: TabKind,
    pub cwd: Option<PathBuf>,
    /// The issue's current tab.
    pub active: bool,
    /// On screen: the current tab of the selected issue.
    pub visible: bool,
    pub process: Process,
    pub claude: Option<ClaudeInfo>,
    /// The tab of whoever asked; only `salash mcp` sets it.
    #[serde(default, rename = "self", skip_serializing_if = "std::ops::Not::not")]
    pub own: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaudeInfo {
    pub session: Option<String>,
    pub status: ClaudeStatus,
    /// `done` turns into `idle` here once someone has looked at the tab.
    pub shown_status: ClaudeStatus,
    /// The last hook event, its message and when it came (RFC 3339).
    pub event: Option<String>,
    pub message: Option<String>,
    pub updated: Option<String>,
    pub transcript: Option<String>,
    /// Run in `cwd` to continue the session outside salash.
    pub resume: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClosedTab {
    pub id: String,
    pub name: String,
    pub session: Option<String>,
    pub cwd: Option<PathBuf>,
}

pub fn answer(app: &mut App, q: Query) -> Answer {
    match q {
        Query::State => Answer::State(state(app)),
        Query::Issue(id) => issue(app, &id).map_or_else(|| Answer::Error(format!("no issue {id}")), |i| Answer::Issue(Box::new(i))),
        Query::Tab(id) => tab(app, &id).map_or_else(|| Answer::Error(format!("no tab {id}")), |t| Answer::Tab(Box::new(t))),
        Query::Screen { tab, lines, scrollback } => screen(app, &tab, lines, scrollback),
        Query::OpenTab(req) => {
            if let Err(e) = req.check() {
                return Answer::Error(format!("{e:#}"));
            }
            if let Some(m) = req.mode.as_deref().filter(|m| !MODES_OVER_MCP.contains(m)) {
                return Answer::Error(format!(
                    "mode {m} is not allowed over MCP (only {}, or none for the user's default); \
                     ask the user to run `salash tab new --mode {m}`",
                    MODES_OVER_MCP.join(", ")
                ));
            }
            let reply = app.open_requested(req);
            match (reply.ok, reply.tab, reply.error) {
                (true, Some(tab), _) => Answer::Opened(tab),
                (_, _, e) => Answer::Error(e.unwrap_or_else(|| "failed".into())),
            }
        }
    }
}

fn closed(c: &[TabState]) -> Vec<ClosedTab> {
    c.iter()
        .map(|t| ClosedTab { id: t.id.clone(), name: t.name.clone(), session: t.claude_session.clone(), cwd: t.cwd.clone() })
        .collect()
}

fn claude_info(st: &TabState, status: ClaudeStatus, shown: ClaudeStatus) -> ClaudeInfo {
    let hook = status::read_status(&st.id);
    let updated = hook
        .as_ref()
        .and_then(|h| chrono::DateTime::from_timestamp_millis(h.ts))
        .map(|t| t.with_timezone(&chrono::Local).to_rfc3339_opts(chrono::SecondsFormat::Secs, false));
    ClaudeInfo {
        session: st.claude_session.clone(),
        status,
        shown_status: shown,
        event: hook.as_ref().map(|h| h.event.clone()),
        message: hook.as_ref().and_then(|h| h.message.clone()),
        updated,
        transcript: hook.and_then(|h| h.transcript),
        resume: st.claude_session.as_ref().map(|s| format!("claude --resume {s}")),
    }
}

fn process(t: &Tab, rt: &IssueRt) -> Process {
    if t.running() {
        Process::Running
    } else if t.pane.is_some() {
        Process::Exited
    } else if t.suspended() {
        Process::Suspended
    } else if rt.awaits_worktree(t) {
        Process::Waiting
    } else {
        Process::Stopped
    }
}

/// The process of a tab of an issue not opened since salash started.
fn saved_process(st: &TabState) -> Process {
    if st.kind == TabKind::Claude && st.claude_session.is_some() {
        Process::Suspended
    } else {
        Process::Stopped
    }
}

fn tab_info(issue: &str, t: &Tab, rt: &IssueRt, active: bool, visible: bool) -> TabInfo {
    let process = process(t, rt);
    let claude = (t.st.kind == TabKind::Claude).then(|| claude_info(&t.st, t.status, t.shown_status()));
    TabInfo {
        id: t.st.id.clone(),
        issue: issue.to_string(),
        name: t.st.name.clone(),
        kind: t.st.kind,
        cwd: t.st.cwd.clone(),
        active,
        visible,
        process,
        claude,
        own: false,
    }
}

/// A tab of an issue not opened since salash started: no process yet.
fn saved_tab_info(issue: &str, st: &TabState, active: bool) -> TabInfo {
    TabInfo {
        id: st.id.clone(),
        issue: issue.to_string(),
        name: st.name.clone(),
        kind: st.kind,
        cwd: st.cwd.clone(),
        active,
        visible: false,
        process: saved_process(st),
        claude: (st.kind == TabKind::Claude).then(|| claude_info(st, ClaudeStatus::Exited, ClaudeStatus::Exited)),
        own: false,
    }
}

/// Tabs, closed tabs, worktree and rolled-up status of an issue.
fn tabs_of(app: &App, id: &str) -> (Vec<TabInfo>, Vec<ClosedTab>, Option<PathBuf>, Option<ClaudeStatus>) {
    if let Some(rt) = app.rt.get(id) {
        let selected = app.selected.as_deref() == Some(id);
        let tabs = rt
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| tab_info(id, t, rt, i == rt.active, selected && i == rt.active))
            .collect();
        return (tabs, closed(&rt.closed), rt.worktree.clone(), rt.rollup());
    }
    match app.state.issues.get(id) {
        Some(s) => {
            let tabs = s.tabs.iter().enumerate().map(|(i, t)| saved_tab_info(id, t, i == s.active_tab)).collect();
            (tabs, closed(&s.closed), s.worktree.clone(), None)
        }
        None => (vec![], vec![], None, None),
    }
}

/// Scratch while it has tabs, then every issue with a note.
fn issue_ids(app: &App) -> impl Iterator<Item = &str> {
    (app.saved_tabs(SCRATCH) > 0).then_some(SCRATCH).into_iter().chain(app.notes.iter().map(|n| n.id.as_str()))
}

fn state(app: &App) -> State {
    State { issues: issue_ids(app).filter_map(|id| issue(app, id)).collect(), selected: app.selected.clone() }
}

/// An issue: one with a note, or Scratch while it has tabs.
fn issue(app: &App, id: &str) -> Option<Issue> {
    let (tabs, closed, worktree, status) = tabs_of(app, id);
    if id == SCRATCH {
        return (app.saved_tabs(SCRATCH) > 0).then(|| Issue {
            id: SCRATCH.into(),
            title: "Scratch".into(),
            stage: None,
            source: None,
            url: None,
            repo: None,
            note: None,
            worktree,
            created: None,
            moved: None,
            status,
            tabs,
            closed,
        });
    }
    let n = app.notes.iter().find(|n| n.id == id)?;
    Some(Issue {
        id: n.id.clone(),
        title: n.title.clone(),
        stage: Some(n.stage),
        source: Some(n.source.clone()),
        url: n.url.clone(),
        repo: n.repo.clone(),
        note: Some(n.path.clone()),
        worktree,
        created: n.created.clone(),
        moved: n.moved.clone(),
        status,
        tabs,
        closed,
    })
}

/// One open tab, built without the rest of the state.
fn tab(app: &App, id: &str) -> Option<TabInfo> {
    for (issue, rt) in &app.rt {
        if let Some(i) = rt.tabs.iter().position(|t| t.st.id == id) {
            let visible = app.selected.as_deref() == Some(issue.as_str()) && i == rt.active;
            return Some(tab_info(issue, &rt.tabs[i], rt, i == rt.active, visible));
        }
    }
    app.state.issues.iter().filter(|(issue, _)| !app.rt.contains_key(*issue)).find_map(|(issue, s)| {
        let i = s.tabs.iter().position(|t| t.id == id)?;
        Some(saved_tab_info(issue, &s.tabs[i], i == s.active_tab))
    })
}

/// What change notifications compare: a hash per resource URI of the fields
/// that resource gives, and the issues `resources/list` gives. Built from
/// memory only. Hook messages, when a tab was looked at and whether it is on
/// screen are left out, so routine tool use and clicking around don't
/// notify; screens are never notified.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fingerprints {
    pub uris: BTreeMap<String, u64>,
    pub listed: BTreeSet<String>,
}

fn hash(v: impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// The hash of what `tab_info` gives but `visible`, which follows the
/// sidebar, and what it reads from the hook's file.
fn tab_hash(st: &TabState, active: bool, process: Process, status: ClaudeStatus, shown: ClaudeStatus) -> u64 {
    hash((&st.name, st.kind, &st.cwd, active, process, status, shown, &st.claude_session))
}

pub fn fingerprints(app: &App) -> Fingerprints {
    let mut f = Fingerprints::default();
    let mut summaries = vec![];
    let scratch = (app.saved_tabs(SCRATCH) > 0).then_some((SCRATCH, None));
    for (id, note) in scratch.into_iter().chain(app.notes.iter().map(|n| (n.id.as_str(), Some(n)))) {
        let (tabs, closed, worktree, status): (Vec<(&str, u64)>, Vec<&str>, _, _) = match (app.rt.get(id), app.state.issues.get(id)) {
            (Some(rt), _) => (
                rt.tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (t.st.id.as_str(), tab_hash(&t.st, i == rt.active, process(t, rt), t.status, t.shown_status())))
                    .collect(),
                rt.closed.iter().map(|t| t.id.as_str()).collect(),
                rt.worktree.as_ref(),
                rt.rollup(),
            ),
            (None, Some(s)) => (
                s.tabs
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        (t.id.as_str(), tab_hash(t, i == s.active_tab, saved_process(t), ClaudeStatus::Exited, ClaudeStatus::Exited))
                    })
                    .collect(),
                s.closed.iter().map(|t| t.id.as_str()).collect(),
                s.worktree.as_ref(),
                None,
            ),
            (None, None) => (vec![], vec![], None, None),
        };
        let fields = note.map(|n| (n.stage, &n.title, &n.source, &n.url, &n.repo, &n.path, &n.created, &n.moved));
        let head = hash((id, fields, worktree, status));
        f.uris.insert(format!("salash://issue/{id}"), hash((head, &tabs, &closed)));
        // The list counts an issue's tabs rather than showing them.
        summaries.push((head, tabs.len()));
        if !tabs.is_empty() {
            f.listed.insert(id.to_string());
        }
        for (tab, h) in tabs {
            f.uris.insert(format!("salash://tab/{tab}"), h);
        }
    }
    f.uris.insert("salash://issues".into(), hash(&summaries));
    f
}

/// What changed from `old` to `new`: URIs that changed, came or went.
pub fn diff(old: &Fingerprints, new: &Fingerprints) -> Changed {
    let gone = old.uris.keys().filter(|u| !new.uris.contains_key(*u));
    let changed = new.uris.iter().filter(|(u, h)| old.uris.get(*u) != Some(h)).map(|(u, _)| u);
    Changed { uris: changed.chain(gone).cloned().collect(), list: old.listed != new.listed }
}

fn screen(app: &App, tab: &str, lines: usize, scrollback: bool) -> Answer {
    let found = app.rt.values().flat_map(|rt| rt.tabs.iter()).find(|t| t.st.id == tab);
    if let Some(p) = found.and_then(|t| t.pane.as_ref()) {
        return Answer::Screen(p.text(lines, scrollback));
    }
    let known = found.is_some()
        || app.state.issues.values().any(|s| s.tabs.iter().chain(&s.closed).any(|t| t.id == tab))
        || app.rt.values().any(|rt| rt.closed.iter().any(|t| t.id == tab));
    if !known {
        return Answer::Error(format!("no tab {tab}"));
    }
    let saved = state::snapshot_path(tab);
    if saved.exists() {
        Answer::Saved(saved)
    } else {
        Answer::Error(format!("tab {tab} isn't running and has no saved screen"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prints(uris: &[(&str, u64)], listed: &[&str]) -> Fingerprints {
        Fingerprints {
            uris: uris.iter().map(|(u, h)| (u.to_string(), *h)).collect(),
            listed: listed.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn diff_names_what_changed_came_or_went() {
        let old = prints(&[("a", 1), ("b", 2), ("c", 3)], &["x"]);
        assert_eq!(diff(&old, &old), Changed::default());
        let new = prints(&[("a", 1), ("b", 9), ("d", 4)], &["x"]);
        assert_eq!(diff(&old, &new), Changed { uris: vec!["b".into(), "d".into(), "c".into()], list: false });
        let fewer = prints(&[("a", 1), ("b", 2), ("c", 3)], &[]);
        assert_eq!(diff(&old, &fewer), Changed { uris: vec![], list: true });
        assert_eq!(diff(&Fingerprints::default(), &Fingerprints::default()), Changed::default());
    }
}
