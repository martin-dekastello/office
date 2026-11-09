use crate::api::{self, Reply, Request};
use crate::clipboard;
use crate::complete::{self, Completion};
use crate::config::Config;
use crate::keys;
use crate::providers::{self, SyncReport};
use crate::state::{self, IssueState, SortMode, State, TabKind, TabState};
use crate::status::{self, ClaudeStatus};
use crate::term::{Pane, Snapshot};
use crate::transcript;
use crate::vault::{NewNote, Note, Stage, Vault};
use crate::worktree;
use anyhow::Result;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// How long after Esc a Claude tab may stay silent before it counts as interrupted.
const INTERRUPT_SETTLE_MS: i64 = 1500;
/// Closed Claude tabs kept per issue for reopening.
const MAX_CLOSED: usize = 10;

pub struct Tab {
    pub st: TabState,
    pub pane: Option<Pane>,
    pub status: ClaudeStatus,
    pub status_ts: i64,
    pub seen_ts: i64,
    /// When Esc was sent while Claude was busy; cleared once that is resolved.
    pub esc_ts: Option<i64>,
    /// Set when the process exits, until the tab is checked on the next poll;
    /// true if it never got as far as a hook, as when `--resume` fails.
    exit_check: Option<bool>,
    /// Extra Claude arguments for the first launch only (mode, first prompt);
    /// a resumed session never gets them again.
    first_args: Vec<String>,
}

impl Tab {
    fn new(st: TabState) -> Tab {
        Tab { st, pane: None, status: ClaudeStatus::Exited, status_ts: 0, seen_ts: 0, esc_ts: None, exit_check: None, first_args: vec![] }
    }

    /// Status as shown: "Done" turns into "Idle" once you've looked at the tab.
    pub fn shown_status(&self) -> ClaudeStatus {
        if self.status == ClaudeStatus::Done && self.status_ts <= self.seen_ts {
            ClaudeStatus::Idle
        } else {
            self.status
        }
    }

    pub fn running(&self) -> bool {
        self.pane.as_ref().is_some_and(|p| p.exited.is_none())
    }

    /// A Claude session with no process: readable, and `--resume` continues it.
    pub fn suspended(&self) -> bool {
        self.st.kind == TabKind::Claude && self.pane.is_none() && self.st.claude_session.is_some()
    }

    /// False for a session with no messages yet, which `--resume` can't find.
    fn resumable(&self) -> bool {
        let Some(sid) = &self.st.claude_session else { return false };
        match status::read_status(&self.st.id) {
            Some(sf) if sf.session_id.as_ref() == Some(sid) => {
                sf.transcript.is_none_or(|t| Path::new(&t).exists())
            }
            _ => true,
        }
    }

    /// Save the screen, with the conversation above it if the terminal kept none.
    fn save_screen(&self) {
        let Some(p) = &self.pane else { return };
        let mut history = vec![];
        if !p.has_scrollback() {
            if let Some(t) = status::read_status(&self.st.id).and_then(|s| s.transcript) {
                history = transcript::history(Path::new(&t));
            }
        }
        state::write_snapshot(&self.st.id, &p.snapshot(&history));
    }

    /// Apply the hook status file. Returns the session id if it's a new one.
    fn sync_status(&mut self) -> Option<String> {
        let sf = status::read_status(&self.st.id)?;
        if sf.ts > self.status_ts || sf.status != self.status {
            self.status = sf.status;
            self.status_ts = sf.ts;
        }
        let sid = sf.session_id?;
        if self.st.claude_session.as_ref() == Some(&sid) {
            return None;
        }
        self.st.claude_session = Some(sid.clone());
        Some(sid)
    }

    fn busy(&self) -> bool {
        matches!(self.status, ClaudeStatus::Working | ClaudeStatus::Attention)
    }

    /// True once a pending Esc went unanswered by any hook. A hook after the
    /// Esc means Claude carried on, so the Esc is dropped.
    fn take_interrupt(&mut self, now: i64) -> bool {
        let Some(esc) = self.esc_ts else { return false };
        if !self.busy() || self.status_ts >= esc {
            self.esc_ts = None;
            return false;
        }
        if now - esc < INTERRUPT_SETTLE_MS {
            return false;
        }
        self.esc_ts = None;
        true
    }
}

/// A worktree being created in the background, and the tabs waiting to start in it.
pub struct WorktreeJob {
    rx: Receiver<Result<PathBuf, String>>,
    waiting: Vec<String>,
    /// Where the tabs start if creating it fails; None reports the failure instead.
    fallback: Option<PathBuf>,
}

#[derive(Default)]
pub struct IssueRt {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub worktree: Option<PathBuf>,
    pub closed: Vec<TabState>,
    pub creating: Option<WorktreeJob>,
    /// Why the worktree script last failed, until it's tried again.
    pub worktree_error: Option<String>,
}

/// How busy a tab keeps its issue, for the activity sort; higher first.
/// `status` is the shown status of a Claude tab whose process runs.
fn activity(kind: TabKind, status: Option<ClaudeStatus>) -> u8 {
    match (kind, status) {
        (TabKind::Claude, Some(ClaudeStatus::Attention)) => 5,
        (TabKind::Claude, Some(ClaudeStatus::Working | ClaudeStatus::Starting)) => 4,
        (TabKind::Claude, Some(ClaudeStatus::Done)) => 3,
        (TabKind::Claude, _) => 2,
        (TabKind::Shell, _) => 1,
        (TabKind::Notes, _) => 0,
    }
}

/// Where a tab that needs the issue's checkout starts.
enum Workdir {
    Ready(PathBuf),
    /// The worktree is being created; the tab starts when it's done.
    Pending,
    NoRepo,
}

impl IssueRt {
    /// The tab is waiting for its worktree to be created.
    pub fn awaits_worktree(&self, tab: &Tab) -> bool {
        self.creating.as_ref().is_some_and(|j| j.waiting.contains(&tab.st.id))
    }

    pub fn rollup(&self) -> Option<ClaudeStatus> {
        self.tabs
            .iter()
            .filter(|t| t.st.kind == TabKind::Claude && t.pane.is_some())
            .map(|t| t.shown_status())
            .max_by_key(|s| s.urgency())
    }

    /// Remember a closed tab, forgetting the oldest beyond MAX_CLOSED.
    fn push_closed(&mut self, st: TabState) {
        self.closed.push(st);
        if self.closed.len() > MAX_CLOSED {
            state::remove_snapshot(&self.closed.remove(0).id);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Terminal,
}

#[derive(Clone, PartialEq, Eq)]
pub enum PromptKind {
    NewIssue,
    RenameTab,
    ClaudeDir,
    DefaultDir,
    StartupDir,
}

impl PromptKind {
    fn is_dir(&self) -> bool {
        matches!(self, PromptKind::ClaudeDir | PromptKind::DefaultDir | PromptKind::StartupDir)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Quit,
    CloseTab,
    Suspend,
}

#[derive(Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Prefix,
    Prompt(PromptKind, String),
    Confirm(ConfirmKind),
    Help,
}

pub struct App {
    pub cfg: Config,
    pub vault: Vault,
    pub notes: Vec<Note>,
    pub view: Stage,
    pub selected: Option<String>,
    pub rt: HashMap<String, IssueRt>,
    /// Saved screen of the suspended tab on view, by tab id. Only one is ever
    /// loaded, so suspended tabs cost no memory.
    pub viewer: Option<(String, Snapshot)>,
    pub state: State,
    pub focus: Focus,
    pub mode: Mode,
    /// Tab matches on show in the directory prompt.
    pub completion: Option<Completion>,
    pub prefix: (KeyCode, KeyModifiers),
    pub msg: Option<(String, Instant)>,
    /// The message is an error: shown in red, for longer.
    pub msg_error: bool,
    sync_rx: Option<Receiver<SyncReport>>,
    pub syncing: bool,
    pub spinner: usize,
    pub quit: bool,
    hooks_settings: Option<PathBuf>,
    bell_pending: bool,
    // Hit areas from the last draw.
    pub term_rect: Rect,
    pub sidebar_rect: Rect,
    pub tab_hits: Vec<(Rect, usize)>,
    pub close_hits: Vec<(Rect, usize)>,
    pub quit_hit: Rect,
    /// A mouse press went to the pane, so its drag and release go there too.
    pane_drag: bool,
    /// The shell tab being selected in, and where the mouse last was.
    pane_select: Option<(String, Position)>,
    pub item_hits: Vec<(Rect, String)>,
    pub stage_hits: Vec<(Rect, Stage)>,
    last_scan: Instant,
    last_status: Instant,
    last_spin: Instant,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Absolute path of a typed directory, or None if it isn't one.
fn resolve_dir(input: &str) -> Option<PathBuf> {
    let dir = std::fs::canonicalize(crate::config::expand(Path::new(input))).ok()?;
    dir.is_dir().then_some(dir)
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// The saved default directory if it still exists, otherwise home.
fn usable_default(saved: Option<&Path>, home: PathBuf) -> PathBuf {
    saved.filter(|p| p.is_dir()).map_or(home, Path::to_path_buf)
}

/// The launch directory, if it isn't already the default.
fn startup_dir(launch: &Path, default: &Path) -> Option<PathBuf> {
    let launch = std::fs::canonicalize(launch).ok()?;
    (std::fs::canonicalize(default).ok().as_ref() != Some(&launch)).then_some(launch)
}

impl App {
    pub fn new(cfg: Config, first_run: bool) -> Result<App> {
        let vault = Vault::new(cfg.notes_root())?;
        let notes = vault.scan();
        let state = State::load();
        let hooks_settings = if cfg.inject_hooks { status::write_settings_file().ok() } else { None };
        let prefix = keys::parse_prefix(&cfg.prefix);
        let mut app = App {
            selected: state.selected.clone().filter(|id| notes.iter().any(|n| &n.id == id)),
            cfg,
            vault,
            notes,
            view: Stage::Active,
            rt: HashMap::new(),
            viewer: None,
            state,
            focus: Focus::Sidebar,
            mode: Mode::Normal,
            completion: None,
            prefix,
            msg: None,
            msg_error: false,
            sync_rx: None,
            syncing: false,
            spinner: 0,
            quit: false,
            hooks_settings,
            bell_pending: false,
            term_rect: Rect::default(),
            sidebar_rect: Rect::default(),
            tab_hits: vec![],
            close_hits: vec![],
            quit_hit: Rect::default(),
            pane_drag: false,
            pane_select: None,
            item_hits: vec![],
            stage_hits: vec![],
            last_scan: Instant::now(),
            last_status: Instant::now(),
            last_spin: Instant::now(),
        };
        if let Some(n) = app.selected_note() {
            app.view = n.stage;
        } else if app.count(Stage::Active) == 0 {
            app.view = Stage::Todo;
        }
        if app.selected.is_none() {
            app.selected = app.visible().first().map(|n| n.id.clone());
        }
        if let Some(id) = app.selected.clone() {
            app.rt_for(&id);
        }
        if first_run {
            app.flash(format!("Created {} — set your vault and sources there", crate::config::config_path().display()));
        } else if app.notes.is_empty() {
            app.flash("No issues yet: n = new local issue, S = sync GitHub/Jira".into());
        }
        let launch = std::env::current_dir().ok();
        if let Some(dir) = launch.and_then(|d| startup_dir(&d, &app.default_dir())) {
            app.mode = Mode::Prompt(PromptKind::StartupDir, crate::config::shorten(&dir));
        }
        app.start_sync();
        Ok(app)
    }

    pub fn flash(&mut self, s: String) {
        self.msg = Some((s, Instant::now()));
        self.msg_error = false;
    }

    pub fn flash_error(&mut self, s: String) {
        self.msg = Some((s, Instant::now()));
        self.msg_error = true;
    }

    pub fn prefix_label(&self) -> String {
        match self.prefix.0 {
            KeyCode::Char(' ') => "C-Space".into(),
            KeyCode::Char(c) => format!("C-{c}"),
            _ => "prefix".into(),
        }
    }

    // ---------- issues ----------

    /// Issues of the current stage, in the chosen order. `notes` is newest-created
    /// first, and the sorts are stable, so that breaks ties.
    pub fn visible(&self) -> Vec<&Note> {
        let mut v: Vec<&Note> = self.notes.iter().filter(|n| n.stage == self.view).collect();
        match self.state.sort {
            SortMode::Created => {}
            SortMode::Name => v.sort_by_cached_key(|n| n.title.to_lowercase()),
            SortMode::Moved => v.sort_by(|a, b| {
                let since = |n: &Note| n.moved.clone().or_else(|| n.created.clone());
                since(b).cmp(&since(a))
            }),
            SortMode::Activity => v.sort_by_cached_key(|n| std::cmp::Reverse(self.activity_of(&n.id))),
        }
        v
    }

    /// The most urgent activity among the issue's tabs. Tabs of an issue not
    /// loaded yet have no process.
    fn activity_of(&self, id: &str) -> u8 {
        match self.rt.get(id) {
            Some(rt) => rt
                .tabs
                .iter()
                .map(|t| activity(t.st.kind, t.running().then(|| t.shown_status())))
                .max(),
            None => self.state.issues.get(id).and_then(|s| s.tabs.iter().map(|t| activity(t.kind, None)).max()),
        }
        .unwrap_or(0)
    }

    pub fn selected_note(&self) -> Option<&Note> {
        let id = self.selected.as_ref()?;
        self.notes.iter().find(|n| &n.id == id)
    }

    pub fn count(&self, st: Stage) -> usize {
        self.notes.iter().filter(|n| n.stage == st).count()
    }

    fn rescan(&mut self) {
        self.notes = self.vault.scan();
        if self.selected_note().is_none() {
            self.selected = self.visible().first().map(|n| n.id.clone());
        }
        self.last_scan = Instant::now();
    }

    fn select_id(&mut self, id: String) {
        // Load saved tabs (without starting them) so the tab bar shows them.
        self.rt_for(&id);
        self.selected = Some(id);
        self.state.selected = self.selected.clone();
        self.state.save();
    }

    fn move_selection(&mut self, delta: isize) {
        let ids: Vec<String> = self.visible().iter().map(|n| n.id.clone()).collect();
        if ids.is_empty() {
            return;
        }
        let cur = self
            .selected
            .as_ref()
            .and_then(|s| ids.iter().position(|i| i == s))
            .unwrap_or(0) as isize;
        let next = (cur + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.select_id(ids[next].clone());
    }

    fn set_view(&mut self, st: Stage) {
        self.view = st;
        if self.selected_note().map(|n| n.stage) != Some(st) {
            if let Some(id) = self.visible().first().map(|n| n.id.clone()) {
                self.select_id(id);
            } else {
                self.selected = None;
            }
        }
    }

    fn move_stage(&mut self, st: Stage) {
        let Some(note) = self.selected_note().cloned() else { return };
        // Pick the neighbour before moving, so the cursor stays in place.
        let ids: Vec<String> = self.visible().iter().map(|n| n.id.clone()).collect();
        let pos = ids.iter().position(|i| i == &note.id).unwrap_or(0);
        match self.vault.move_to(&note, st) {
            Ok(_) => {
                self.flash(format!("{} → {}", note.id, st.label()));
                self.rescan();
                let next = ids
                    .get(pos + 1)
                    .or_else(|| pos.checked_sub(1).and_then(|p| ids.get(p)))
                    .cloned();
                if let Some(id) = next {
                    self.select_id(id);
                } else {
                    self.selected = None;
                }
                self.focus = Focus::Sidebar;
            }
            Err(e) => self.flash(format!("Move failed: {e:#}")),
        }
    }

    fn new_local_issue(&mut self, title: String) {
        let title = title.trim().to_string();
        if title.is_empty() {
            return;
        }
        let id = self.vault.next_local_id(&self.cfg.local_prefix);
        match self.vault.create(NewNote { id: &id, title: &title, source: "local", url: None, repo: None, stage: Stage::Todo }) {
            Ok((n, _)) => {
                self.rescan();
                self.view = Stage::Todo;
                self.select_id(n.id.clone());
                self.flash(format!("Created {}", n.path.display()));
            }
            Err(e) => self.flash(format!("Create failed: {e:#}")),
        }
    }

    fn open_obsidian(&mut self) {
        let Some(n) = self.selected_note() else { return };
        let url = format!(
            "obsidian://open?path={}",
            urlencoding::encode(&n.path.to_string_lossy())
        );
        if let Err(e) = open::that_detached(&url) {
            self.flash(format!("Could not open Obsidian: {e}"));
        }
    }

    // ---------- sync ----------

    pub fn start_sync(&mut self) {
        if self.syncing {
            return;
        }
        let providers = providers::from_config(&self.cfg);
        if providers.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(providers::fetch_all(&providers));
        });
        self.sync_rx = Some(rx);
        self.syncing = true;
    }

    fn poll_sync(&mut self) {
        let Some(rx) = &self.sync_rx else { return };
        let Ok(rep) = rx.try_recv() else { return };
        self.sync_rx = None;
        self.syncing = false;
        let mut created = 0;
        for i in &rep.fetched {
            let r = self.vault.create(NewNote {
                id: &i.id,
                title: &i.title,
                source: i.source,
                url: i.url.as_deref(),
                repo: i.repo.as_deref(),
                stage: Stage::Todo,
            });
            if matches!(r, Ok((_, true))) {
                created += 1;
            }
        }
        self.rescan();
        let mut m = format!("Synced {} issues, {} new", rep.fetched.len(), created);
        if !rep.errors.is_empty() {
            m.push_str(&format!(" — {}", rep.errors.join("; ")));
        }
        self.flash(m);
    }

    // ---------- tabs ----------

    fn rt_for(&mut self, id: &str) -> &mut IssueRt {
        if !self.rt.contains_key(id) {
            let st = self.state.issues.get(id).cloned().unwrap_or_default();
            let rt = IssueRt {
                active: st.active_tab.min(st.tabs.len().saturating_sub(1)),
                worktree: st.worktree.clone(),
                tabs: st.tabs.into_iter().map(Tab::new).collect(),
                closed: st.closed,
                ..Default::default()
            };
            self.rt.insert(id.to_string(), rt);
        }
        self.rt.get_mut(id).unwrap()
    }

    pub fn current_rt(&self) -> Option<&IssueRt> {
        self.rt.get(self.selected.as_ref()?)
    }

    pub fn current_tabs(&self) -> Option<(Vec<&Tab>, usize)> {
        let id = self.selected.as_ref()?;
        let rt = self.rt.get(id)?;
        Some((rt.tabs.iter().collect(), rt.active))
    }

    pub fn saved_tabs(&self, id: &str) -> usize {
        self.rt
            .get(id)
            .map(|r| r.tabs.len())
            .unwrap_or_else(|| self.state.issues.get(id).map(|s| s.tabs.len()).unwrap_or(0))
    }

    fn persist(&mut self, id: &str) {
        if let Some(rt) = self.rt.get(id) {
            self.state.issues.insert(
                id.to_string(),
                IssueState {
                    worktree: rt.worktree.clone(),
                    tabs: rt.tabs.iter().map(|t| t.st.clone()).collect(),
                    active_tab: rt.active,
                    closed: rt.closed.clone(),
                },
            );
            self.state.save();
        }
    }

    pub fn default_dir(&self) -> PathBuf {
        usable_default(self.state.default_dir.as_deref(), home())
    }

    /// Working directory for an issue's tab. The worktree is created the first
    /// time a tab needs it, in the background, and the tab starts once it exists.
    fn workdir(&mut self, note: &Note, tab: &str) -> Workdir {
        let rt = self.rt_for(&note.id);
        if let Some(wt) = rt.worktree.clone().filter(|p| p.exists()) {
            return Workdir::Ready(wt);
        }
        if let Some(job) = &mut rt.creating {
            if !job.waiting.iter().any(|t| t == tab) {
                job.waiting.push(tab.to_string());
            }
            return Workdir::Pending;
        }
        let repo = note
            .repo_path
            .clone()
            .or_else(|| self.cfg.repo_path_for(&note.source, note.repo.as_deref()));
        let Some(repo) = repo else { return Workdir::NoRepo };
        let command = self.cfg.worktree_command.clone();
        let dir = self.cfg.worktrees_dir.clone();
        let (stem, issue) = (note.stem(), note.id.clone());
        // Plain git falls back to the repo as it always has; a script the user
        // wrote reports its failure instead, as the repo is unlikely what they want.
        let fallback = command.is_empty().then(|| repo.clone());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let r = if command.is_empty() {
                worktree::ensure(&repo, &dir, &stem)
            } else {
                worktree::run_script(&command, &repo, &dir, &stem, &issue)
            };
            let _ = tx.send(r.map_err(|e| format!("{e:#}")));
        });
        let rt = self.rt_for(&note.id);
        rt.worktree_error = None;
        rt.creating = Some(WorktreeJob { rx, waiting: vec![tab.to_string()], fallback });
        Workdir::Pending
    }

    /// Start the tabs whose worktree has been created, or report why it wasn't.
    fn poll_worktrees(&mut self) {
        let ids: Vec<String> = self.rt.iter().filter(|(_, r)| r.creating.is_some()).map(|(id, _)| id.clone()).collect();
        for id in ids {
            let Some(rt) = self.rt.get_mut(&id) else { continue };
            let Some(job) = &rt.creating else { continue };
            let result = match job.rx.try_recv() {
                Ok(r) => r,
                Err(TryRecvError::Empty) => continue,
                Err(TryRecvError::Disconnected) => Err("worktree creation stopped unexpectedly".into()),
            };
            let Some(job) = rt.creating.take() else { continue };
            let cwd = match (result, job.fallback) {
                (Ok(p), _) => {
                    rt.worktree = Some(p.clone());
                    self.persist(&id);
                    p
                }
                (Err(e), Some(repo)) => {
                    self.flash(format!("Worktree failed, using repo: {e}"));
                    repo
                }
                (Err(e), None) => {
                    // The script's own last words usually say what went wrong.
                    let last = e.lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default().trim().to_string();
                    rt.worktree_error = Some(e);
                    self.flash_error(format!("{id}: worktree script failed: {last}"));
                    continue;
                }
            };
            let Some(note) = self.notes.iter().find(|n| n.id == id).cloned() else { continue };
            for tab in job.waiting {
                let rt = self.rt_for(&id);
                // It may have been closed, or started elsewhere, while it waited.
                if let Some(idx) = rt.tabs.iter().position(|t| t.st.id == tab && t.pane.is_none()) {
                    self.start_pane(&note, idx, cwd.clone());
                }
            }
        }
    }

    fn spawn_pane(&mut self, id: &str, idx: usize) {
        let Some(note) = self.notes.iter().find(|n| n.id == id).cloned() else { return };
        let Some(tab) = self.rt_for(id).tabs.get(idx) else { return };
        let (kind, tab_id) = (tab.st.kind, tab.st.id.clone());
        // A session can only be resumed from the directory it started in.
        let chosen = tab.st.cwd.clone();
        let cwd = match chosen.filter(|p| p.is_dir()) {
            Some(p) => p,
            // Editing the note needs no checkout, so it never creates one.
            None if kind == TabKind::Notes => match self.rt_for(id).worktree.clone().filter(|p| p.is_dir()) {
                Some(p) => p,
                None => self.default_dir(),
            },
            None => match self.workdir(&note, &tab_id) {
                Workdir::Ready(p) => p,
                Workdir::Pending => return,
                Workdir::NoRepo => {
                    let default = self.default_dir();
                    let Some(tab) = self.rt_for(id).tabs.get_mut(idx) else { return };
                    let dir = tab.st.settle_dir(&default, &home());
                    // Saved before Claude starts, so its session is never without one.
                    self.persist(id);
                    dir
                }
            },
        };
        self.start_pane(&note, idx, cwd);
    }

    fn start_pane(&mut self, note: &Note, idx: usize, cwd: PathBuf) {
        let id = note.id.as_str();
        let (rows, cols) = (self.term_rect.height.max(5), self.term_rect.width.max(20));
        let rt = self.rt_for(id);
        let Some(tab) = rt.tabs.get_mut(idx) else { return };
        let st = tab.st.clone();
        let first_args = std::mem::take(&mut tab.first_args);
        let argv: Vec<String> = match st.kind {
            TabKind::Claude => {
                let mut v = self.cfg.claude_command.clone();
                if let Some(s) = &self.hooks_settings {
                    v.push("--settings".into());
                    v.push(s.to_string_lossy().into_owned());
                }
                if let Some(sid) = &st.claude_session {
                    v.push("--resume".into());
                    v.push(sid.clone());
                } else {
                    v.extend(first_args);
                }
                v
            }
            TabKind::Shell => self.cfg.shell(),
            TabKind::Notes => {
                let mut v = self.cfg.editor();
                v.push(note.path.to_string_lossy().into_owned());
                v
            }
        };
        let mut env = vec![
            ("OFFICE_TAB".to_string(), st.id.clone()),
            ("OFFICE_ISSUE".to_string(), note.id.clone()),
            ("OFFICE_NOTE".to_string(), note.path.to_string_lossy().into_owned()),
        ];
        if let Ok(exe) = std::env::current_exe() {
            env.push(("OFFICE_BIN".to_string(), exe.to_string_lossy().into_owned()));
        }
        status::clear_status(&st.id);
        let result = Pane::spawn(&argv, &cwd, &env, rows, cols);
        let rt = self.rt_for(id);
        let tab = &mut rt.tabs[idx];
        match result {
            Ok(p) => {
                tab.pane = Some(p);
                tab.status = if st.kind == TabKind::Claude { ClaudeStatus::Starting } else { ClaudeStatus::Idle };
                tab.status_ts = now_ms();
                state::remove_snapshot(&st.id);
            }
            Err(e) => self.flash(format!("Spawn failed: {e:#}")),
        }
    }

    fn new_tab(&mut self, kind: TabKind) {
        self.new_tab_in(kind, None);
    }

    fn new_tab_in(&mut self, kind: TabKind, cwd: Option<PathBuf>) {
        let Some(id) = self.selected.clone() else {
            self.flash("Select an issue first".into());
            return;
        };
        if kind == TabKind::Claude {
            self.start_work(&id);
        }
        self.add_tab(&id, kind, None, cwd, vec![], true);
        self.focus = Focus::Terminal;
    }

    /// Add a tab to an issue and start it. Returns the new tab's id. Unless
    /// `activate`, the issue's active tab stays as it was.
    fn add_tab(
        &mut self,
        issue: &str,
        kind: TabKind,
        name: Option<String>,
        cwd: Option<PathBuf>,
        first_args: Vec<String>,
        activate: bool,
    ) -> String {
        let default = match kind {
            TabKind::Claude => "claude",
            TabKind::Shell => "shell",
            TabKind::Notes => "notes",
        };
        let tab_id = uuid::Uuid::new_v4().to_string();
        let mut tab = Tab::new(TabState {
            id: tab_id.clone(),
            custom_name: name.is_some(),
            name: name.unwrap_or_else(|| default.into()),
            kind,
            claude_session: None,
            cwd,
        });
        tab.first_args = first_args;
        let rt = self.rt_for(issue);
        rt.tabs.push(tab);
        let idx = rt.tabs.len() - 1;
        if activate {
            rt.active = idx;
        }
        self.persist(issue);
        self.spawn_pane(issue, idx);
        tab_id
    }

    /// Open tabs asked for with `office tab new`, in the background.
    fn poll_requests(&mut self) {
        for (id, req) in api::take_requests() {
            let reply = match req {
                Ok(req) => self.open_requested(req),
                Err(e) => Reply::err(format!("{e:#}")),
            };
            api::reply(&id, &reply);
        }
    }

    fn open_requested(&mut self, req: Request) -> Reply {
        if let Err(e) = req.check() {
            return Reply::err(format!("{e:#}"));
        }
        if !self.notes.iter().any(|n| n.id == req.issue) {
            return Reply::err(format!("no issue {}", req.issue));
        }
        let args = if req.kind == TabKind::Claude { req.claude_args() } else { vec![] };
        let tab = self.add_tab(&req.issue, req.kind, req.name.clone(), req.cwd.clone(), args, false);
        let rt = self.rt_for(&req.issue);
        // A tab waiting for its worktree starts once it exists.
        let waiting = rt.creating.as_ref().is_some_and(|j| j.waiting.contains(&tab));
        let Some(t) = rt.tabs.iter().find(|t| t.st.id == tab) else {
            return Reply::err("tab closed");
        };
        if t.pane.is_none() && !waiting {
            return Reply::err("tab didn't start, see office");
        }
        let name = t.st.name.clone();
        self.flash(format!("New tab \"{name}\" in {}", req.issue));
        Reply::ok(tab)
    }

    /// Starting a Claude session on a To-do issue moves it to Active. Runs
    /// before the pane spawns so OFFICE_NOTE points at the moved file.
    fn start_work(&mut self, id: &str) {
        let Some(note) = self.selected_note().filter(|n| n.id == id && n.stage == Stage::Todo).cloned() else {
            return;
        };
        match self.vault.move_to(&note, Stage::Active) {
            Ok(_) => {
                self.rescan();
                self.view = Stage::Active;
                self.flash(format!("{} → {}", note.id, Stage::Active.label()));
            }
            Err(e) => self.flash(format!("Move failed: {e:#}")),
        }
    }

    /// Start a Claude tab in a typed directory; empty means the issue's default.
    fn new_claude_in(&mut self, input: String) {
        let dir = input.trim();
        if dir.is_empty() {
            self.new_tab(TabKind::Claude);
        } else if let Some(p) = resolve_dir(dir) {
            self.msg = None;
            self.new_tab_in(TabKind::Claude, Some(p));
        } else {
            self.flash(format!("Not a directory: {dir}"));
            self.mode = Mode::Prompt(PromptKind::ClaudeDir, input);
        }
    }

    /// Save a typed directory as the app's default; empty keeps the current one.
    /// Tabs that already exist stay where they are.
    fn set_default_dir(&mut self, kind: PromptKind, input: String) {
        let dir = input.trim();
        if dir.is_empty() {
            return;
        }
        if let Some(p) = resolve_dir(dir) {
            self.flash(format!("Default directory: {}", crate::config::shorten(&p)));
            self.state.default_dir = Some(p);
            self.state.save();
        } else {
            self.flash(format!("Not a directory: {dir}"));
            self.mode = Mode::Prompt(kind, input);
        }
    }

    /// Open the issue: focus the terminal, starting/resuming the active tab.
    fn open_issue(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        let rt = self.rt_for(&id);
        if rt.tabs.is_empty() {
            self.new_tab(TabKind::Claude);
            return;
        }
        let idx = rt.active;
        if rt.tabs[idx].pane.is_none() {
            self.spawn_pane(&id, idx);
        }
        self.focus = Focus::Terminal;
    }

    /// Show the active tab after moving to it. A suspended tab stays stopped,
    /// so it can be read without starting Claude.
    fn view_tab(&mut self) {
        if self.active_tab().is_some_and(Tab::suspended) {
            self.focus = Focus::Terminal;
        } else {
            self.open_issue();
        }
    }

    fn switch_tab(&mut self, idx: usize) {
        let Some(id) = self.selected.clone() else { return };
        let rt = self.rt_for(&id);
        if idx < rt.tabs.len() {
            rt.active = idx;
            self.persist(&id);
        }
    }

    fn cycle_tab(&mut self, delta: isize) {
        let Some(rt) = self.current_rt() else { return };
        let n = rt.tabs.len() as isize;
        if n == 0 {
            return;
        }
        let next = ((rt.active as isize + delta).rem_euclid(n)) as usize;
        self.switch_tab(next);
    }

    fn close_tab(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        let rt = self.rt_for(&id);
        if rt.tabs.is_empty() {
            return;
        }
        let mut t = rt.tabs.remove(rt.active);
        let reopenable = t.st.kind == TabKind::Claude && t.resumable();
        if reopenable {
            t.save_screen();
        }
        if let Some(p) = t.pane.as_mut() {
            p.kill();
        }
        status::clear_status(&t.st.id);
        if reopenable {
            rt.push_closed(t.st);
        } else {
            state::remove_snapshot(&t.st.id);
        }
        rt.active = rt.active.min(rt.tabs.len().saturating_sub(1));
        if rt.tabs.is_empty() {
            self.focus = Focus::Sidebar;
        }
        self.persist(&id);
    }

    /// Bring back the most recently closed Claude tab, still suspended.
    fn reopen_tab(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        let rt = self.rt_for(&id);
        let Some(st) = rt.closed.pop() else {
            self.flash("No closed Claude tabs for this issue".into());
            return;
        };
        rt.tabs.push(Tab::new(st));
        rt.active = rt.tabs.len() - 1;
        self.persist(&id);
        self.focus = Focus::Terminal;
    }

    /// Save a tab's screen and stop its process, leaving it suspended.
    fn park(&mut self, id: &str, idx: usize) {
        let Some(t) = self.rt.get_mut(id).and_then(|rt| rt.tabs.get_mut(idx)) else { return };
        t.save_screen();
        t.pane = None;
        t.esc_ts = None;
        t.exit_check = None;
        self.persist(id);
    }

    fn request_suspend(&mut self) {
        let Some(t) = self.active_tab() else { return };
        if t.st.kind != TabKind::Claude {
            self.flash("Only Claude tabs can be suspended".into());
        } else if !t.running() {
            self.flash("This tab isn't running".into());
        } else if !t.resumable() {
            self.flash("Nothing to resume yet: this session has no messages".into());
        } else if t.busy() || t.status == ClaudeStatus::Starting {
            self.mode = Mode::Confirm(ConfirmKind::Suspend);
        } else {
            self.suspend_tab();
        }
    }

    fn suspend_tab(&mut self) {
        let Some(id) = self.selected.clone() else { return };
        let idx = self.rt_for(&id).active;
        self.park(&id, idx);
        self.flash("Suspended: process stopped, ⏎ resumes".into());
    }

    /// Before quitting: keep Claude tabs readable, and don't leave a session
    /// id behind that `--resume` can't find.
    fn save_sessions(&mut self) {
        let ids: Vec<String> = self.rt.keys().cloned().collect();
        for id in ids {
            let rt = self.rt_for(&id);
            for t in rt.tabs.iter_mut().filter(|t| t.st.kind == TabKind::Claude) {
                if t.pane.is_none() {
                    continue;
                }
                if t.resumable() {
                    t.save_screen();
                } else {
                    t.st.claude_session = None;
                }
            }
            self.persist(&id);
        }
    }

    fn active_tab(&self) -> Option<&Tab> {
        let rt = self.current_rt()?;
        rt.tabs.get(rt.active)
    }

    fn can_focus_terminal(&self) -> bool {
        self.active_tab().is_some_and(|t| t.pane.is_some() || t.suspended())
    }

    fn scroll_active(&mut self, delta: isize) {
        if let Some(p) = self.active_pane_mut() {
            p.scroll_by(delta);
        } else if let Some((_, s)) = self.viewer.as_mut() {
            s.scroll_by(delta);
        }
    }

    fn scroll_page(&mut self, up: bool) {
        let lines = (self.term_rect.height as isize / 2).max(1);
        if let Some(p) = self.active_pane_mut() {
            p.scroll_page(up, lines);
        } else {
            self.scroll_active(if up { lines } else { -lines });
        }
    }

    /// Load the saved screen of the suspended tab on view, and only that one.
    pub fn sync_viewer(&mut self) {
        let rows = self.term_rect.height.saturating_sub(1);
        let Some(tab) = self.active_tab().filter(|t| t.suspended()).map(|t| t.st.id.clone()) else {
            self.viewer = None;
            return;
        };
        if self.viewer.as_ref().is_some_and(|(id, s)| *id == tab && s.rows == rows) {
            return;
        }
        self.viewer = Snapshot::load(&state::snapshot_path(&tab), rows).map(|s| (tab, s));
    }

    fn active_pane_mut(&mut self) -> Option<&mut Pane> {
        let id = self.selected.clone()?;
        let rt = self.rt.get_mut(&id)?;
        rt.tabs.get_mut(rt.active)?.pane.as_mut()
    }

    // ---------- periodic work ----------

    pub fn tick(&mut self) {
        self.poll_sync();
        self.autoscroll_select();
        if self.last_spin.elapsed() >= Duration::from_millis(120) {
            self.spinner = self.spinner.wrapping_add(1);
            self.last_spin = Instant::now();
        }
        if self.last_scan.elapsed() >= Duration::from_secs(3) {
            // Picks up notes moved/renamed/created in Obsidian.
            self.rescan();
        }
        self.poll_worktrees();
        if let Some((_, t)) = &self.msg {
            if t.elapsed() > Duration::from_secs(if self.msg_error { 15 } else { 6 }) {
                self.msg = None;
            }
        }

        // Mark the visible tab as seen.
        let now = now_ms();
        if let Some(id) = self.selected.clone() {
            if let Some(rt) = self.rt.get_mut(&id) {
                if let Some(t) = rt.tabs.get_mut(rt.active) {
                    t.seen_ts = now;
                }
            }
        }

        if self.last_status.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last_status = Instant::now();
        self.poll_requests();
        let selected = self.selected.clone();
        let mut new_sessions: Vec<(String, String, String)> = vec![];
        let mut renamed: Vec<String> = vec![];
        let mut stopped: Vec<(String, usize)> = vec![];
        for (issue, rt) in self.rt.iter_mut() {
            let active = rt.active;
            for (i, t) in rt.tabs.iter_mut().enumerate() {
                if t.pane.is_none() {
                    continue;
                }
                let visible = selected.as_deref() == Some(issue.as_str()) && i == active;
                let before = t.shown_status();
                let (was_exited, exited, title, bell) = {
                    let p = t.pane.as_mut().unwrap();
                    (p.exited.is_some(), p.poll_exit(), p.title(), p.take_bell())
                };
                let claude = t.st.kind == TabKind::Claude;
                if claude && !was_exited {
                    if let Some(sid) = t.sync_status() {
                        new_sessions.push((issue.clone(), sid, t.st.name.clone()));
                    }
                }
                if exited {
                    if claude && !was_exited {
                        t.exit_check = Some(t.status == ClaudeStatus::Starting);
                    } else if let Some(no_hooks) = t.exit_check.take() {
                        // A poll later, so the reader has the last of the output.
                        let hint = t.pane.as_ref().and_then(Pane::resume_id);
                        if let (None, Some(sid)) = (&t.st.claude_session, &hint) {
                            t.st.claude_session = Some(sid.clone());
                            new_sessions.push((issue.clone(), sid.clone(), t.st.name.clone()));
                        }
                        if !t.resumable() {
                            t.st.claude_session = None;
                            renamed.push(issue.clone());
                        } else if !no_hooks || hint.is_some() {
                            stopped.push((issue.clone(), i));
                        }
                    }
                    t.status = ClaudeStatus::Exited;
                } else if claude {
                    if t.take_interrupt(now) {
                        status::mark_interrupted(&t.st.id);
                    }
                    if !t.st.custom_name {
                        if let Some(title) = title {
                            if title != "Claude Code" && title != t.st.name {
                                t.st.name = title;
                                renamed.push(issue.clone());
                            }
                        }
                    }
                }
                let after = t.shown_status();
                let changed_to_notice = after != before
                    && matches!(after, ClaudeStatus::Attention | ClaudeStatus::Done);
                if !visible && (changed_to_notice || bell) {
                    self.bell_pending = true;
                }
            }
        }
        for (issue, sid, name) in new_sessions {
            if let Some(note) = self.notes.iter().find(|n| n.id == issue).cloned() {
                let when = chrono::Local::now().format("%Y-%m-%d %H:%M");
                let _ = self.vault.append_session_line(
                    &note,
                    &format!("- {when} · {name} · `claude --resume {sid}`"),
                );
            }
            self.persist(&issue);
        }
        for (issue, idx) in stopped {
            self.park(&issue, idx);
        }
        renamed.sort();
        renamed.dedup();
        for issue in renamed {
            self.persist(&issue);
        }
    }

    pub fn ring_bell_if_needed(&mut self) {
        if self.bell_pending && self.cfg.bell {
            let mut out = std::io::stdout();
            let _ = out.write_all(b"\x07");
            let _ = out.flush();
        }
        self.bell_pending = false;
    }

    /// Keep panes of the visible issue sized to the terminal area.
    pub fn resize_visible(&mut self) {
        let r = self.term_rect;
        if r.width == 0 || r.height == 0 {
            return;
        }
        if let Some(id) = self.selected.clone() {
            if let Some(rt) = self.rt.get_mut(&id) {
                for t in rt.tabs.iter_mut() {
                    if let Some(p) = t.pane.as_mut() {
                        p.resize(r.height, r.width);
                    }
                }
            }
        }
    }

    // ---------- input ----------

    pub fn handle(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Paste(s) => self.on_paste(s),
            Event::Mouse(m) if self.cfg.mouse => self.on_mouse(m),
            _ => {}
        }
    }

    fn on_paste(&mut self, s: String) {
        if let Mode::Prompt(_, buf) = &mut self.mode {
            buf.push_str(s.lines().next().unwrap_or(""));
            self.completion = None;
            return;
        }
        if self.focus == Focus::Terminal {
            if let Some(p) = self.active_pane_mut() {
                if p.bracketed_paste() {
                    let mut b = b"\x1b[200~".to_vec();
                    b.extend_from_slice(s.as_bytes());
                    b.extend_from_slice(b"\x1b[201~");
                    p.write(&b);
                } else {
                    p.write(s.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
                }
            }
        }
    }

    /// Offer a mouse event to the active pane, clamped to its area. False if
    /// the pane's app doesn't want it.
    fn forward_mouse(&mut self, m: MouseEvent) -> bool {
        let r = self.term_rect;
        if self.mode != Mode::Normal || r.is_empty() {
            return false;
        }
        let local = MouseEvent {
            column: m.column.clamp(r.left(), r.right() - 1) - r.left(),
            row: m.row.clamp(r.top(), r.bottom() - 1) - r.top(),
            ..m
        };
        self.active_pane_mut().is_some_and(|p| p.send_mouse(&local))
    }

    /// A press the pane's app didn't want starts a selection, in shell tabs only:
    /// Claude Code handles its own.
    fn start_select(&mut self, pos: Position) {
        let r = self.term_rect;
        self.pane_select = None;
        if self.pane_drag {
            return;
        }
        let Some(tab) = self.active_tab().filter(|t| t.st.kind == TabKind::Shell) else { return };
        self.pane_select = Some((tab.st.id.clone(), pos));
        if let Some(p) = self.active_pane_mut() {
            p.select_start(pos.y - r.top(), pos.x - r.left());
        }
    }

    /// The pane of the selection under way, unless its tab is no longer in view.
    fn select_pane(&mut self) -> Option<&mut Pane> {
        let id = self.pane_select.as_ref()?.0.clone();
        self.active_tab().filter(|t| t.st.id == id)?;
        self.active_pane_mut()
    }

    /// Stretch the selection to the mouse, clamped to the pane.
    fn extend_select(&mut self) {
        let r = self.term_rect;
        let Some((_, pos)) = self.pane_select.clone() else { return };
        if r.is_empty() {
            return;
        }
        let row = pos.y.clamp(r.top(), r.bottom() - 1) - r.top();
        let col = pos.x.clamp(r.left(), r.right() - 1) - r.left();
        if let Some(p) = self.select_pane() {
            p.select_to(row, col);
        }
    }

    /// Scroll while the mouse is held above or below the pane.
    fn autoscroll_select(&mut self) {
        let r = self.term_rect;
        let Some((_, pos)) = self.pane_select.clone() else { return };
        let delta = if pos.y < r.top() {
            1
        } else if pos.y >= r.bottom() {
            -1
        } else {
            return;
        };
        if let Some(p) = self.select_pane().filter(|p| !p.alternate_screen()) {
            p.scroll_by(delta);
            self.extend_select();
        }
    }

    fn finish_select(&mut self) {
        let Some(p) = self.select_pane() else {
            self.pane_select = None;
            return;
        };
        let text = p.selected_text();
        if text.is_none() {
            p.selection = None;
        }
        self.pane_select = None;
        if let Some(text) = text {
            let n = text.chars().count();
            self.flash(if clipboard::copy(&text) {
                format!("Copied {n} characters")
            } else {
                format!("Asked the terminal to copy {n} characters")
            });
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        // A click must not change which tab an open prompt applies to, but a
        // release still has to end any drag in progress.
        if self.mode != Mode::Normal && !matches!(m.kind, MouseEventKind::Up(_)) {
            return;
        }
        let pos = Position::new(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.quit_hit.contains(pos) {
                    self.mode = Mode::Confirm(ConfirmKind::Quit);
                } else if let Some((_, i)) = self.close_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                    self.switch_tab(i);
                    self.mode = Mode::Confirm(ConfirmKind::CloseTab);
                } else if let Some((_, i)) = self.tab_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                    self.switch_tab(i);
                    self.view_tab();
                } else if let Some((_, s)) = self.stage_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                    self.set_view(s);
                    self.focus = Focus::Sidebar;
                } else if let Some((_, id)) = self.item_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                    self.select_id(id);
                    self.focus = Focus::Sidebar;
                } else if self.term_rect.contains(pos) {
                    if self.can_focus_terminal() {
                        self.focus = Focus::Terminal;
                    }
                    self.pane_drag = self.forward_mouse(m);
                    self.start_select(pos);
                } else if self.sidebar_rect.contains(pos) {
                    self.focus = Focus::Sidebar;
                }
            }
            MouseEventKind::Down(_) if self.term_rect.contains(pos) => {
                self.pane_drag = self.forward_mouse(m);
            }
            MouseEventKind::Drag(_) if self.pane_drag => {
                self.forward_mouse(m);
            }
            MouseEventKind::Up(_) if self.pane_drag => {
                self.forward_mouse(m);
                self.pane_drag = false;
            }
            MouseEventKind::Drag(_) if self.pane_select.is_some() => {
                if let Some((_, last)) = &mut self.pane_select {
                    *last = pos;
                }
                self.extend_select();
            }
            MouseEventKind::Up(_) if self.pane_select.is_some() => self.finish_select(),
            MouseEventKind::Moved if self.term_rect.contains(pos) => {
                self.forward_mouse(m);
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if self.term_rect.contains(pos) {
                    if !self.forward_mouse(m) {
                        self.scroll_active(if up { 3 } else { -3 });
                        self.extend_select();
                    }
                } else if self.sidebar_rect.contains(pos) {
                    self.move_selection(if up { -1 } else { 1 });
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, k: KeyEvent) {
        match self.mode.clone() {
            Mode::Help => {
                self.mode = Mode::Normal;
            }
            Mode::Confirm(kind) => {
                self.mode = Mode::Normal;
                if matches!(k.code, KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter) {
                    match kind {
                        ConfirmKind::Quit => self.quit = true,
                        ConfirmKind::CloseTab => self.close_tab(),
                        ConfirmKind::Suspend => self.suspend_tab(),
                    }
                }
            }
            Mode::Prompt(kind, mut buf) => {
                let mut completion = self.completion.take();
                match k.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Enter => {
                        self.mode = Mode::Normal;
                        match kind {
                            PromptKind::NewIssue => self.new_local_issue(buf),
                            PromptKind::RenameTab => self.rename_tab(buf),
                            PromptKind::ClaudeDir => self.new_claude_in(buf),
                            PromptKind::DefaultDir | PromptKind::StartupDir => self.set_default_dir(kind, buf),
                        }
                    }
                    KeyCode::Tab | KeyCode::BackTab if kind.is_dir() => {
                        if !complete::tab(&mut buf, &mut completion, k.code == KeyCode::Tab) {
                            self.flash("No matching directories".into());
                        }
                        self.completion = completion;
                        self.mode = Mode::Prompt(kind, buf);
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                        self.mode = Mode::Prompt(kind, buf);
                    }
                    KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                        // A selected match already ends in a separator.
                        let cycling = completion.is_some_and(|c| c.selected.is_some());
                        if !(cycling && std::path::is_separator(c)) {
                            buf.push(c);
                        }
                        self.mode = Mode::Prompt(kind, buf);
                    }
                    _ => {}
                }
            }
            Mode::Prefix => {
                self.mode = Mode::Normal;
                self.on_prefix_command(k);
            }
            Mode::Normal => {
                if keys::is_prefix(&k, &self.prefix) {
                    self.mode = Mode::Prefix;
                    return;
                }
                match self.focus {
                    Focus::Sidebar => self.on_sidebar_key(k),
                    Focus::Terminal => self.on_terminal_key(k),
                }
            }
        }
    }

    fn on_terminal_key(&mut self, k: KeyEvent) {
        let Some(id) = self.selected.clone() else { return };
        let rt = self.rt_for(&id);
        let idx = rt.active;
        let Some(tab) = rt.tabs.get_mut(idx) else {
            if k.code == KeyCode::Enter {
                self.open_issue();
            }
            return;
        };
        if tab.suspended() {
            let page = (self.term_rect.height as isize / 2).max(1);
            match k.code {
                KeyCode::Enter => self.spawn_pane(&id, idx),
                KeyCode::Up => self.scroll_active(1),
                KeyCode::Down => self.scroll_active(-1),
                KeyCode::PageUp => self.scroll_active(page),
                KeyCode::PageDown => self.scroll_active(-page),
                KeyCode::Char('g') | KeyCode::Home => self.scroll_active(isize::MAX / 2),
                KeyCode::Char('G') | KeyCode::End => self.scroll_active(isize::MIN / 2),
                _ => {}
            }
            return;
        }
        if k.code == KeyCode::Esc && tab.st.kind == TabKind::Claude && tab.busy() {
            tab.esc_ts = Some(now_ms());
        }
        match tab.pane.as_mut() {
            Some(p) if p.exited.is_none() => {
                let bytes = keys::encode(&k, p.application_cursor());
                if !bytes.is_empty() {
                    p.write(&bytes);
                }
            }
            _ => {
                if k.code == KeyCode::Enter {
                    tab.pane = None;
                    self.spawn_pane(&id, idx);
                }
            }
        }
    }

    fn on_sidebar_key(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => self.move_selection(-10_000),
            KeyCode::Char('G') | KeyCode::End => self.move_selection(10_000),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.open_issue(),
            KeyCode::Char('s') => {
                self.state.sort = self.state.sort.next();
                self.state.save();
                self.flash(format!("Sort: {}", self.state.sort.label()));
            }
            KeyCode::Char('0') => self.set_view(Stage::Todo),
            KeyCode::Char('1') => self.set_view(Stage::Active),
            KeyCode::Char('2') => self.set_view(Stage::Done),
            KeyCode::Char('3') => self.set_view(Stage::Archive),
            KeyCode::Char('v') | KeyCode::Char('h') | KeyCode::Left => {
                let next = match self.view {
                    Stage::Todo => Stage::Active,
                    Stage::Active => Stage::Done,
                    Stage::Done => Stage::Archive,
                    Stage::Archive => Stage::Todo,
                };
                self.set_view(next);
            }
            KeyCode::Tab | KeyCode::Esc => {
                if self.can_focus_terminal() {
                    self.focus = Focus::Terminal;
                }
            }
            _ => {
                self.on_command(k);
            }
        }
    }

    /// Commands shared by the sidebar (bare keys) and the prefix table.
    fn on_command(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Char('c') => self.new_tab(TabKind::Claude),
            KeyCode::Char('C') => {
                if self.selected.is_some() {
                    self.mode = Mode::Prompt(PromptKind::ClaudeDir, String::new());
                } else {
                    self.flash("Select an issue first".into());
                }
            }
            KeyCode::Char('p') => self.mode = Mode::Prompt(PromptKind::DefaultDir, String::new()),
            KeyCode::Char('t') => self.new_tab(TabKind::Shell),
            KeyCode::Char('e') => self.new_tab(TabKind::Notes),
            KeyCode::Char('o') => self.open_obsidian(),
            KeyCode::Char('w') => {
                if let Some(u) = self.selected_note().and_then(|n| n.url.clone()) {
                    let _ = open::that_detached(u);
                } else {
                    self.flash("This issue has no URL".into());
                }
            }
            KeyCode::Char('n') | KeyCode::Char('N') => {
                self.mode = Mode::Prompt(PromptKind::NewIssue, String::new())
            }
            KeyCode::Char('d') => self.move_stage(Stage::Done),
            KeyCode::Char('a') => self.move_stage(Stage::Archive),
            KeyCode::Char('u') => self.move_stage(Stage::Active),
            KeyCode::Char('T') => self.move_stage(Stage::Todo),
            KeyCode::Char('S') | KeyCode::Char('r') => {
                if providers::from_config(&self.cfg).is_empty() {
                    self.flash("No [[github]] or [[jira]] sources in config".into());
                } else {
                    self.start_sync();
                    self.flash("Syncing…".into());
                }
            }
            KeyCode::Char('x') => {
                if self.current_rt().is_some_and(|r| !r.tabs.is_empty()) {
                    self.mode = Mode::Confirm(ConfirmKind::CloseTab);
                }
            }
            KeyCode::Char('z') => self.request_suspend(),
            KeyCode::Char('R') => self.reopen_tab(),
            KeyCode::Char(',') => {
                if let Some(rt) = self.current_rt() {
                    if let Some(t) = rt.tabs.get(rt.active) {
                        self.mode = Mode::Prompt(PromptKind::RenameTab, t.st.name.clone());
                    }
                }
            }
            KeyCode::Char('q') => self.mode = Mode::Confirm(ConfirmKind::Quit),
            KeyCode::Char('?') => self.mode = Mode::Help,
            _ => return false,
        }
        true
    }

    fn on_prefix_command(&mut self, k: KeyEvent) {
        if keys::is_prefix(&k, &self.prefix) {
            if let Some(p) = self.active_pane_mut() {
                let b = keys::encode(&k, false);
                p.write(&b);
            }
            return;
        }
        match k.code {
            KeyCode::Char('s') | KeyCode::Tab => {
                let can_focus = self.can_focus_terminal();
                self.focus = match self.focus {
                    Focus::Sidebar if can_focus => Focus::Terminal,
                    _ => Focus::Sidebar,
                };
            }
            KeyCode::Char(c @ '1'..='9') => {
                self.switch_tab(c as usize - '1' as usize);
                self.view_tab();
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.cycle_tab(1);
                self.view_tab();
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.cycle_tab(-1);
                self.view_tab();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.move_selection(1);
                self.after_issue_switch();
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.move_selection(-1);
                self.after_issue_switch();
            }
            KeyCode::Char('[') | KeyCode::PageUp => {
                self.scroll_page(true);
            }
            KeyCode::Char(']') | KeyCode::PageDown => {
                self.scroll_page(false);
            }
            _ => {
                self.on_command(k);
            }
        }
    }

    /// When jumping issues from the terminal, stay in the terminal if the
    /// target issue already has a running tab.
    fn after_issue_switch(&mut self) {
        if self.focus == Focus::Terminal && !self.can_focus_terminal() {
            self.focus = Focus::Sidebar;
        }
    }

    fn rename_tab(&mut self, name: String) {
        let Some(id) = self.selected.clone() else { return };
        let name = name.trim().to_string();
        let rt = self.rt_for(&id);
        if let Some(t) = rt.tabs.get_mut(rt.active) {
            t.st.custom_name = !name.is_empty();
            if !name.is_empty() {
                t.st.name = name;
            }
        }
        self.persist(&id);
    }

    pub fn running_claude_tabs(&self) -> usize {
        self.rt
            .values()
            .flat_map(|r| r.tabs.iter())
            .filter(|t| t.st.kind == TabKind::Claude && t.running())
            .count()
    }

    pub fn suspended_claude_tabs(&self) -> usize {
        let loaded = self.rt.values().flat_map(|r| r.tabs.iter()).filter(|t| t.suspended()).count();
        let saved = self
            .state
            .issues
            .iter()
            .filter(|(id, _)| !self.rt.contains_key(*id))
            .flat_map(|(_, s)| s.tabs.iter())
            .filter(|t| t.kind == TabKind::Claude && t.claude_session.is_some())
            .count();
        loaded + saved
    }
}

/// Main loop.
pub fn run(app: &mut App, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    while !app.quit {
        app.tick();
        app.sync_viewer();
        terminal.draw(|f| crate::ui::draw(app, f))?;
        app.resize_visible();
        app.ring_bell_if_needed();
        if event::poll(Duration::from_millis(33))? {
            app.handle(event::read()?);
            // Drain bursts (fast typing, paste) before the next frame.
            while event::poll(Duration::ZERO)? {
                app.handle(event::read()?);
                if app.quit {
                    break;
                }
            }
        }
    }
    app.save_sessions();
    app.state.selected = app.selected.clone();
    app.state.save();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab(status: ClaudeStatus, status_ts: i64, esc_ts: Option<i64>) -> Tab {
        let st = TabState {
            id: "t".into(),
            name: "claude".into(),
            kind: TabKind::Claude,
            custom_name: false,
            claude_session: None,
            cwd: None,
        };
        Tab { status, status_ts, esc_ts, ..Tab::new(st) }
    }

    #[test]
    fn resolve_dir_accepts_only_directories() {
        let dir = std::env::temp_dir().join(format!("office-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("file");
        std::fs::write(&file, "").unwrap();

        let got = resolve_dir(dir.to_str().unwrap()).unwrap();
        assert!(got.is_absolute() && got.is_dir());
        assert_eq!(resolve_dir(file.to_str().unwrap()), None);
        assert_eq!(resolve_dir(dir.join("missing").to_str().unwrap()), None);
        assert_eq!(resolve_dir("~"), dirs::home_dir().and_then(|h| std::fs::canonicalize(h).ok()));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn startup_asks_only_outside_the_default() {
        let dir = std::env::temp_dir().join(format!("office-test-{}", uuid::Uuid::new_v4()));
        let (a, b) = (dir.join("a"), dir.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        assert_eq!(startup_dir(&a, &a), None);
        assert_eq!(startup_dir(&a.join("../a"), &a), None);
        assert_eq!(startup_dir(&a, &b), std::fs::canonicalize(&a).ok());
        assert_eq!(startup_dir(&dir.join("missing"), &a), None);
        #[cfg(unix)]
        {
            let link = dir.join("link");
            std::os::unix::fs::symlink(&a, &link).unwrap();
            assert_eq!(startup_dir(&link, &a), None);
        }

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_deleted_default_falls_back_to_home() {
        let dir = std::env::temp_dir().join(format!("office-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let home = PathBuf::from("/home");

        assert_eq!(usable_default(Some(&dir), home.clone()), dir);
        assert_eq!(usable_default(None, home.clone()), home);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(usable_default(Some(&dir), home.clone()), home);
    }

    #[test]
    fn only_a_claude_tab_with_a_session_is_suspended() {
        let mut t = tab(ClaudeStatus::Exited, 0, None);
        assert!(!t.suspended());
        t.st.claude_session = Some("s".into());
        assert!(t.suspended());
        t.st.kind = TabKind::Shell;
        assert!(!t.suspended());
    }

    #[test]
    fn closed_tabs_are_capped_oldest_first() {
        let mut rt = IssueRt::default();
        for i in 0..MAX_CLOSED + 2 {
            let mut st = tab(ClaudeStatus::Exited, 0, None).st;
            st.id = format!("office-test-no-snapshot-{i}");
            rt.push_closed(st);
        }
        assert_eq!(rt.closed.len(), MAX_CLOSED);
        assert_eq!(rt.closed[0].id, "office-test-no-snapshot-2");
    }

    #[test]
    fn interrupt_settles_only_without_hooks() {
        let mut t = tab(ClaudeStatus::Attention, 100, Some(1000));
        assert!(!t.take_interrupt(1000 + INTERRUPT_SETTLE_MS - 1));
        assert!(t.take_interrupt(1000 + INTERRUPT_SETTLE_MS));
        assert_eq!(t.esc_ts, None);

        // A hook after the Esc: Claude carried on.
        let mut t = tab(ClaudeStatus::Working, 1200, Some(1000));
        assert!(!t.take_interrupt(9000));
        assert_eq!(t.esc_ts, None);

        // Already resolved some other way.
        let mut t = tab(ClaudeStatus::Done, 100, Some(1000));
        assert!(!t.take_interrupt(9000));
    }

    #[test]
    fn the_most_urgent_tab_ranks_the_issue() {
        use ClaudeStatus::*;
        let level = |tabs: &[(TabKind, Option<ClaudeStatus>)]| tabs.iter().map(|(k, s)| activity(*k, *s)).max().unwrap_or(0);
        let busy = level(&[(TabKind::Shell, None), (TabKind::Claude, Some(Idle)), (TabKind::Claude, Some(Working))]);
        let done = level(&[(TabKind::Claude, Some(Done))]);
        let idle = level(&[(TabKind::Claude, Some(Idle)), (TabKind::Shell, None)]);
        let shell = level(&[(TabKind::Shell, None), (TabKind::Notes, None)]);
        assert!(level(&[(TabKind::Claude, Some(Attention))]) > busy);
        assert!(busy > done && done > idle && idle > shell && shell > level(&[]));
        // A suspended session counts as an inactive Claude, as does one that exited.
        assert_eq!(activity(TabKind::Claude, None), idle);
        assert_eq!(activity(TabKind::Claude, Some(Exited)), idle);
        assert_eq!(level(&[(TabKind::Notes, None)]), 0);
    }
}
