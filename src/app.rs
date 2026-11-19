use crate::api::{self, Reply, Request};
use crate::complete::{self, Completion};
use crate::config::Config;
use crate::keys;
use crate::plugin;
use crate::proto::ServerMsg;
use crate::providers::{self, SyncReport};
use crate::state::{self, IssueState, SortMode, State, TabKind, TabState};
use crate::status::{self, ClaudeStatus};
use crate::term::{Pane, Snapshot};
use crate::transcript;
use crate::vault::{NewNote, Note, Stage, Vault};
use crate::worktree;
use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

/// How long after Esc a Claude tab may stay silent before it counts as interrupted.
const INTERRUPT_SETTLE_MS: i64 = 1500;
/// Closed Claude tabs kept per issue for reopening.
const MAX_CLOSED: usize = 10;
/// The issue id of Scratch: tabs that belong to no issue. It has no note.
pub const SCRATCH: &str = "scratch";

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

    /// Status as shown: "Done" turns into "Idle" once the tab has been on view
    /// for `seen_after_secs`.
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

fn scratch_shown(tabs: usize, selected: bool) -> bool {
    tabs > 0 || selected
}

/// Track the tab on view. True once the same tab has been on view for `after`.
fn dwell(on_view: &mut Option<(String, Instant)>, tab: Option<&str>, now: Instant, after: Duration) -> bool {
    match (on_view.as_ref(), tab) {
        (_, None) => {
            *on_view = None;
            false
        }
        (Some((id, since)), Some(t)) if id == t => now.saturating_duration_since(*since) >= after,
        (_, Some(t)) => {
            *on_view = Some((t.to_string(), now));
            after.is_zero()
        }
    }
}

/// Where a tab that needs the issue's checkout starts.
enum Workdir {
    Ready(PathBuf),
    /// The worktree is being created; the tab starts when it's done.
    Pending,
    /// Worktrees are off: the repo itself.
    Repo(PathBuf),
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

    /// Take a tab out, keeping the same tab active unless it was this one.
    fn remove_tab(&mut self, idx: usize) -> Tab {
        let t = self.tabs.remove(idx);
        if idx < self.active {
            self.active -= 1;
        }
        self.active = self.active.min(self.tabs.len().saturating_sub(1));
        t
    }

    /// Remember a closed tab, forgetting the oldest beyond MAX_CLOSED.
    fn push_closed(&mut self, st: TabState) {
        self.closed.push(st);
        if self.closed.len() > MAX_CLOSED {
            state::remove_snapshot(&self.closed.remove(0).id);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
}

impl PromptKind {
    fn is_dir(&self) -> bool {
        matches!(self, PromptKind::ClaudeDir | PromptKind::DefaultDir)
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
    /// The folder this salash runs in: where tabs on issues with no repo start.
    pub folder: PathBuf,
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
    /// The client whose input this was asked to detach.
    pub detach: bool,
    /// For the client whose input this is: what to copy or open on its side.
    pub outbox: Vec<ServerMsg>,
    hooks_settings: Option<PathBuf>,
    /// The `--mcp-config` file Claude tabs get, unless `mcp = false`.
    mcp_config: Option<PathBuf>,
    /// salash's plugin (its skills), passed as `claude --plugin-dir`.
    plugin_dir: Option<PathBuf>,
    bell_pending: bool,
    // Hit areas from the last draw.
    pub term_rect: Rect,
    pub sidebar_rect: Rect,
    pub tab_hits: Vec<(Rect, usize)>,
    pub close_hits: Vec<(Rect, usize)>,
    pub quit_hit: Rect,
    /// The sort label in the sidebar header; a click reverses the sort.
    pub sort_hit: Rect,
    /// A mouse press went to the pane, so its drag and release go there too.
    pane_drag: bool,
    /// The shell tab being selected in, and where the mouse last was.
    pane_select: Option<(String, Position)>,
    pub item_hits: Vec<(Rect, String)>,
    pub stage_hits: Vec<(Rect, Stage)>,
    last_scan: Instant,
    last_status: Instant,
    last_spin: Instant,
    /// The tab on view and since when: it counts as seen after
    /// `seen_after_secs`. One for everyone while all clients share one view.
    /// Monotonic, so a clock step or sleep doesn't mark it seen.
    on_view: Option<(String, Instant)>,
    /// The selected issue and since when (ms, like `seen_ts`): a Done seen
    /// meanwhile keeps its rank, so the list doesn't reorder under the cursor.
    held: Option<(String, i64)>,
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

/// Claude's argv: the configured command, salash's hooks settings, plugin and
/// MCP config when given, then `--resume <session>` or, on a first launch, `first_args`.
fn claude_argv(
    cmd: &[String],
    settings: Option<&Path>,
    plugin: Option<&Path>,
    mcp: Option<&Path>,
    session: Option<&str>,
    first_args: Vec<String>,
) -> Vec<String> {
    let mut v = cmd.to_vec();
    if let Some(s) = settings {
        v.push("--settings".into());
        v.push(s.to_string_lossy().into_owned());
    }
    if let Some(p) = plugin {
        v.push("--plugin-dir".into());
        v.push(p.to_string_lossy().into_owned());
    }
    // One argument: `--mcp-config` takes several values, so a separate path
    // would take a first prompt after it as another.
    if let Some(m) = mcp {
        v.push(format!("--mcp-config={}", m.display()));
    }
    match session {
        Some(sid) => {
            v.push("--resume".into());
            v.push(sid.into());
        }
        None => v.extend(first_args),
    }
    v
}

/// The saved default directory if it still exists, otherwise salash's folder.
fn usable_default(saved: Option<&Path>, folder: &Path) -> PathBuf {
    saved.filter(|p| p.is_dir()).unwrap_or(folder).to_path_buf()
}

impl App {
    pub fn new(cfg: Config, first_run: bool, folder: PathBuf) -> Result<App> {
        let vault = Vault::new(cfg.notes_root())?;
        let notes = vault.scan();
        let state = State::load();
        let hooks_settings = if cfg.inject_hooks { status::write_settings_file().ok() } else { None };
        let mcp_config = if cfg.mcp { status::write_mcp_file(&folder).ok() } else { None };
        // Not tied to inject_hooks: --plugin-dir leaves ~/.claude alone.
        let plugin_dir = crate::config::data_dir().join("plugin");
        let plugin_dir = match plugin::write_to(&plugin_dir) {
            Ok(()) => Some(plugin_dir),
            Err(e) => {
                eprintln!("writing salash's Claude plugin: {e:#}");
                None
            }
        };
        let mut app = App::with_parts(cfg, folder, vault, notes, state, hooks_settings, mcp_config, plugin_dir);
        if let Some(n) = app.selected_note() {
            app.view = n.stage;
        } else if app.count(Stage::Active) == 0 {
            app.view = Stage::Todo;
        }
        if app.selected.is_none() {
            app.selected = app.rows().first().cloned();
        }
        if let Some(id) = app.selected.clone() {
            app.rt_for(&id);
        }
        if first_run {
            app.flash(format!("Created {} — set your vault and sources there", crate::config::config_path().display()));
        } else if app.notes.is_empty() {
            app.flash("No issues yet: n = new local issue, S = sync GitHub/Jira".into());
        }
        app.start_sync();
        Ok(app)
    }

    /// The app over what `new` loaded, with no disk access and no sync.
    #[allow(clippy::too_many_arguments)]
    fn with_parts(cfg: Config, folder: PathBuf, vault: Vault, notes: Vec<Note>, state: State, hooks_settings: Option<PathBuf>, mcp_config: Option<PathBuf>, plugin_dir: Option<PathBuf>) -> App {
        let prefix = keys::parse_prefix(&cfg.prefix);
        App {
            selected: state.selected.clone().filter(|id| {
                notes.iter().any(|n| &n.id == id)
                    || (id == SCRATCH && state.issues.get(SCRATCH).is_some_and(|s| !s.tabs.is_empty()))
            }),
            cfg,
            folder,
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
            detach: false,
            outbox: vec![],
            hooks_settings,
            mcp_config,
            plugin_dir,
            bell_pending: false,
            term_rect: Rect::default(),
            sidebar_rect: Rect::default(),
            tab_hits: vec![],
            close_hits: vec![],
            quit_hit: Rect::default(),
            sort_hit: Rect::default(),
            pane_drag: false,
            pane_select: None,
            item_hits: vec![],
            stage_hits: vec![],
            last_scan: Instant::now(),
            last_status: Instant::now(),
            last_spin: Instant::now(),
            on_view: None,
            held: None,
        }
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
    /// first, and the sorts are stable, so that breaks ties. Reversing flips the
    /// key only: ties stay newest-created first, and undated issues stay last.
    pub fn visible(&self) -> Vec<&Note> {
        let mut v: Vec<&Note> = self.notes.iter().filter(|n| n.stage == self.view).collect();
        let rev = self.state.sort_reversed;
        let dir = |o: std::cmp::Ordering| if rev { o.reverse() } else { o };
        match self.state.sort {
            SortMode::Created => {
                if rev {
                    v.reverse();
                }
            }
            SortMode::Id => v.sort_by(|a, b| dir(natord::compare_ignore_case(&a.id, &b.id))),
            SortMode::Name => v.sort_by(|a, b| dir(a.title.to_lowercase().cmp(&b.title.to_lowercase()))),
            SortMode::Moved => {
                let since = |n: &Note| n.moved.clone().or_else(|| n.created.clone());
                v.sort_by(|a, b| {
                    let (a, b) = (since(a), since(b));
                    a.is_none().cmp(&b.is_none()).then_with(|| dir(b.cmp(&a)))
                });
            }
            SortMode::Activity => {
                let mut ranked: Vec<(u8, &Note)> = v.into_iter().map(|n| (self.activity_of(&n.id), n)).collect();
                ranked.sort_by(|a, b| dir(b.0.cmp(&a.0)));
                v = ranked.into_iter().map(|(_, n)| n).collect();
            }
        }
        v
    }

    /// The sort as the sidebar header shows it, with an arrow for its direction.
    pub fn sort_label(&self) -> String {
        let up = self.state.sort.ascending() != self.state.sort_reversed;
        format!("{} {}", if up { "↑" } else { "↓" }, self.state.sort.label())
    }

    fn flip_sort(&mut self) {
        self.state.sort_reversed = !self.state.sort_reversed;
        self.state.save();
        self.flash(format!("Sort: {}", self.sort_label()));
    }

    /// The most urgent activity among the issue's tabs. Tabs of an issue not
    /// loaded yet have no process.
    fn activity_of(&self, id: &str) -> u8 {
        match self.rt.get(id) {
            Some(rt) => rt
                .tabs
                .iter()
                .map(|t| activity(t.st.kind, t.running().then(|| self.ranked_status(id, t))))
                .max(),
            None => self.state.issues.get(id).and_then(|s| s.tabs.iter().map(|t| activity(t.kind, None)).max()),
        }
        .unwrap_or(0)
    }

    /// Status a tab ranks by in the activity sort: as shown, except that a
    /// Done seen while its issue stayed selected still ranks as Done.
    fn ranked_status(&self, id: &str, t: &Tab) -> ClaudeStatus {
        match &self.held {
            Some((h, since)) if h == id && t.status == ClaudeStatus::Done && t.seen_ts >= *since => ClaudeStatus::Done,
            _ => t.shown_status(),
        }
    }

    /// Scratch shows while it has tabs, or while it's selected so a first tab can be opened.
    pub fn shows_scratch(&self) -> bool {
        scratch_shown(self.saved_tabs(SCRATCH), self.scratch_selected())
    }

    pub fn scratch_selected(&self) -> bool {
        self.selected.as_deref() == Some(SCRATCH)
    }

    /// Ids of the sidebar rows: Scratch first in every view and sort, then the issues.
    pub fn rows(&self) -> Vec<String> {
        let scratch = self.shows_scratch().then(|| SCRATCH.to_string());
        scratch.into_iter().chain(self.visible().iter().map(|n| n.id.clone())).collect()
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
        if self.selected_note().is_none() && !self.scratch_selected() {
            self.selected = self.rows().first().cloned();
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
        let ids = self.rows();
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
        // Scratch is in every view, so it stays selected.
        if !self.scratch_selected() && self.selected_note().map(|n| n.stage) != Some(st) {
            if let Some(id) = self.rows().first().cloned() {
                self.select_id(id);
            } else {
                self.selected = None;
            }
        }
    }

    fn move_stage(&mut self, st: Stage) {
        let Some(note) = self.selected_note().cloned() else { return };
        // Pick the neighbour before moving, so the cursor stays in place.
        let ids = self.rows();
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
        self.outbox.push(ServerMsg::Open(url));
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
        usable_default(self.state.default_dir.as_deref(), &self.folder)
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
        if !self.cfg.worktrees_for(&note.source, note.repo.as_deref()) {
            return Workdir::Repo(repo);
        }
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
                    self.start_pane(&id, Some(&note), idx, cwd.clone());
                }
            }
        }
    }

    fn spawn_pane(&mut self, id: &str, idx: usize) {
        // Only Scratch has no note.
        let note = self.notes.iter().find(|n| n.id == id).cloned();
        if note.is_none() && id != SCRATCH {
            return;
        }
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
            // Scratch has no repo, so it starts where an issue with no repo would.
            None => match note.as_ref().map_or(Workdir::NoRepo, |n| self.workdir(n, &tab_id)) {
                Workdir::Ready(p) => p,
                Workdir::Pending => return,
                // Kept in the tab, so it resumes here even once worktrees are back on.
                // No home fallback as in settle_dir: a session on an issue with a repo
                // started in the repo or in its worktree, and that is returned first.
                Workdir::Repo(repo) => {
                    let Some(tab) = self.rt_for(id).tabs.get_mut(idx) else { return };
                    let dir = tab.st.cwd.get_or_insert(repo).clone();
                    self.persist(id);
                    dir
                }
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
        self.start_pane(id, note.as_ref(), idx, cwd);
    }

    /// Start a tab's process. `note` is None only for Scratch.
    fn start_pane(&mut self, id: &str, note: Option<&Note>, idx: usize, cwd: PathBuf) {
        let (rows, cols) = (self.term_rect.height.max(5), self.term_rect.width.max(20));
        let rt = self.rt_for(id);
        let Some(tab) = rt.tabs.get_mut(idx) else { return };
        let st = tab.st.clone();
        let first_args = std::mem::take(&mut tab.first_args);
        let argv: Vec<String> = match st.kind {
            TabKind::Claude => claude_argv(
                &self.cfg.claude_command,
                self.hooks_settings.as_deref(),
                self.plugin_dir.as_deref(),
                self.mcp_config.as_deref(),
                st.claude_session.as_deref(),
                first_args,
            ),
            TabKind::Shell => self.cfg.shell(),
            TabKind::Notes => {
                let Some(note) = note else { return };
                let mut v = self.cfg.editor();
                v.push(note.path.to_string_lossy().into_owned());
                v
            }
        };
        let mut env = vec![
            ("SALASH_TAB".to_string(), st.id.clone()),
            ("SALASH_ISSUE".to_string(), id.to_string()),
            // Hooks and `salash tab new` in the tab reach this salash, not another folder's.
            ("SALASH_DATA".to_string(), crate::config::data_dir().to_string_lossy().into_owned()),
        ];
        if let Some(note) = note {
            env.push(("SALASH_NOTE".to_string(), note.path.to_string_lossy().into_owned()));
        }
        if let Ok(exe) = std::env::current_exe() {
            env.push(("SALASH_BIN".to_string(), exe.to_string_lossy().into_owned()));
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
        if kind == TabKind::Notes && id == SCRATCH {
            self.flash("Scratch has no note".into());
            return;
        }
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

    /// Open tabs asked for with `salash tab new`, in the background.
    fn poll_requests(&mut self) {
        for (id, req) in api::take_requests() {
            let reply = match req {
                Ok(req) => self.open_requested(req),
                Err(e) => Reply::err(format!("{e:#}")),
            };
            api::reply(&id, &reply);
        }
    }

    pub fn open_requested(&mut self, req: Request) -> Reply {
        if let Err(e) = req.check() {
            return Reply::err(format!("{e:#}"));
        }
        if req.issue == SCRATCH {
            if req.kind == TabKind::Notes {
                return Reply::err("scratch has no note");
            }
        } else if !self.notes.iter().any(|n| n.id == req.issue) {
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
            return Reply::err("tab didn't start, see salash");
        }
        let name = t.st.name.clone();
        self.flash(format!("New tab \"{name}\" in {}", req.issue));
        Reply::ok(tab)
    }

    /// Starting a Claude session on a To-do issue moves it to Active. Runs
    /// before the pane spawns so SALASH_NOTE points at the moved file.
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
        let idx = self.rt_for(&id).active;
        self.close_tab_at(&id, idx);
    }

    /// Close a tab. A Claude session that can be resumed is kept for `R`.
    /// Returns whether it was.
    fn close_tab_at(&mut self, id: &str, idx: usize) -> bool {
        let rt = self.rt_for(id);
        if idx >= rt.tabs.len() {
            return false;
        }
        let mut t = rt.remove_tab(idx);
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
        let emptied = rt.tabs.is_empty() && self.selected.as_deref() == Some(id);
        self.persist(id);
        if emptied {
            self.focus = Focus::Sidebar;
            // An empty Scratch hides, so move to the first issue.
            if id == SCRATCH {
                self.selected = self.visible().first().map(|n| n.id.clone());
                if let Some(next) = self.selected.clone() {
                    self.select_id(next);
                }
            }
        }
        reopenable
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
    pub fn save_sessions(&mut self) {
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

    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        let rt = self.rt.get_mut(self.selected.as_ref()?)?;
        rt.tabs.get_mut(rt.active)
    }

    fn active_pane_mut(&mut self) -> Option<&mut Pane> {
        self.active_tab_mut()?.pane.as_mut()
    }

    /// Mark the tab on view as seen once it has stayed on view for
    /// `seen_after_secs`, so flicking past a finished tab doesn't count as
    /// reading it. Switching issue or tab changes the id on view, which
    /// restarts the timer; so does an overlay covering the pane.
    fn mark_seen(&mut self, now: Instant) {
        if self.held.as_ref().map(|(id, _)| id) != self.selected.as_ref() {
            self.held = self.selected.clone().map(|id| (id, now_ms()));
        }
        let covered = !matches!(self.mode, Mode::Normal | Mode::Prefix);
        let tab = if covered { None } else { self.active_tab().map(|t| t.st.id.clone()) };
        if dwell(&mut self.on_view, tab.as_deref(), now, Duration::from_secs(self.cfg.seen_after_secs)) {
            if let Some(t) = self.active_tab_mut() {
                t.seen_ts = now_ms();
            }
        }
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

        self.mark_seen(Instant::now());
        let now = now_ms();

        if self.last_status.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.last_status = Instant::now();
        self.poll_requests();
        let selected = self.selected.clone();
        let mut new_sessions: Vec<(String, String, String)> = vec![];
        let mut renamed: Vec<String> = vec![];
        // Tabs whose process exited, to close. As in tmux with `remain-on-exit`
        // off, only a Claude that failed to start stays, to show why.
        let mut ended: Vec<(String, usize)> = vec![];
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
                    if !claude && !was_exited {
                        ended.push((issue.clone(), i));
                    } else if claude && !was_exited {
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
                        }
                        if !no_hooks || hint.is_some() {
                            ended.push((issue.clone(), i));
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
        renamed.sort();
        renamed.dedup();
        for issue in renamed {
            self.persist(&issue);
        }
        // Highest index first, so the ones still to close keep theirs.
        ended.sort_by_key(|e| std::cmp::Reverse(e.1));
        for (issue, idx) in ended {
            if self.close_tab_at(&issue, idx) && selected.as_deref() == Some(issue.as_str()) {
                self.flash("Claude exited, tab closed: R reopens it".into());
            }
        }
    }

    /// True if the clients' terminals should ring.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.bell_pending) && self.cfg.bell
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
            if let Some(t) = self.active_tab_mut().filter(|t| t.pane.is_some()) {
                t.seen_ts = now_ms();
            }
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
            self.flash(format!("Copied {} characters", text.chars().count()));
            self.outbox.push(ServerMsg::Clipboard(text));
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
                } else if self.sort_hit.contains(pos) {
                    self.flip_sort();
                    self.focus = Focus::Sidebar;
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
                    // Unlike ⏎, a click never starts, restarts or resumes a tab.
                    self.select_id(id);
                    self.focus_tab_if_live();
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
                    self.after_issue_switch();
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
                            PromptKind::DefaultDir => self.set_default_dir(kind, buf),
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
                    // Typing into the tab means you've read it.
                    tab.seen_ts = now_ms();
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
                self.state.sort_reversed = false;
                self.state.save();
                self.flash(format!("Sort: {}", self.sort_label()));
            }
            KeyCode::Char('i') => self.flip_sort(),
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
                    self.outbox.push(ServerMsg::Open(u));
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
            KeyCode::Char('`') => {
                self.select_id(SCRATCH.into());
                self.focus_tab_if_live();
            }
            KeyCode::Char('q') => self.mode = Mode::Confirm(ConfirmKind::Quit),
            KeyCode::Char('D') => self.detach = true,
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

    /// Focus the active tab if it has a pane (running or exited) or a saved
    /// screen, else the sidebar. Never starts, restarts or resumes anything.
    fn focus_tab_if_live(&mut self) {
        self.focus = if self.can_focus_terminal() { Focus::Terminal } else { Focus::Sidebar };
    }

    /// When moving between issues from the terminal (prefix `j`/`k`, the
    /// sidebar wheel), stay in the terminal only if the target's tab is live.
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

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn claude_gets_hooks_plugin_and_mcp_before_its_own_args() {
        let (s, p, m) = (Path::new("/d/claude-hooks.json"), Path::new("/d/plugin"), Path::new("/d/claude-mcp.json"));
        let first = claude_argv(&strs(&["claude"]), Some(s), Some(p), Some(m), None, strs(&["--permission-mode", "plan", "brief"]));
        assert_eq!(
            first,
            strs(&[
                "claude",
                "--settings",
                "/d/claude-hooks.json",
                "--plugin-dir",
                "/d/plugin",
                "--mcp-config=/d/claude-mcp.json",
                "--permission-mode",
                "plan",
                "brief"
            ])
        );
        let resumed = claude_argv(&strs(&["claude"]), Some(s), Some(p), Some(m), Some("sid"), strs(&["ignored"]));
        assert_eq!(
            resumed,
            strs(&["claude", "--settings", "/d/claude-hooks.json", "--plugin-dir", "/d/plugin", "--mcp-config=/d/claude-mcp.json", "--resume", "sid"])
        );
    }

    #[test]
    fn without_injection_claude_gets_neither() {
        let v = claude_argv(&strs(&["npx", "claude"]), None, None, None, Some("sid"), vec![]);
        assert_eq!(v, strs(&["npx", "claude", "--resume", "sid"]));
    }

    #[test]
    fn resolve_dir_accepts_only_directories() {
        let dir = std::env::temp_dir().join(format!("salash-test-{}", uuid::Uuid::new_v4()));
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
    fn a_deleted_default_falls_back_to_the_folder() {
        let dir = std::env::temp_dir().join(format!("salash-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let folder = Path::new("/srv/team");

        assert_eq!(usable_default(Some(&dir), folder), dir);
        assert_eq!(usable_default(None, folder), folder);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(usable_default(Some(&dir), folder), folder);
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
    fn a_done_tab_reads_as_idle_only_once_seen() {
        let mut t = tab(ClaudeStatus::Done, 1000, None);
        assert_eq!(t.shown_status(), ClaudeStatus::Done);
        t.seen_ts = 999;
        assert_eq!(t.shown_status(), ClaudeStatus::Done);
        t.seen_ts = 1000;
        assert_eq!(t.shown_status(), ClaudeStatus::Idle);
        let mut w = tab(ClaudeStatus::Working, 1000, None);
        w.seen_ts = 5000;
        assert_eq!(w.shown_status(), ClaudeStatus::Working);
    }

    #[test]
    fn a_tab_counts_as_seen_after_staying_on_view() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let after = Duration::from_secs(5);
        let mut v = None;
        assert!(!dwell(&mut v, Some("a"), at(0), after));
        assert!(!dwell(&mut v, Some("a"), at(4999), after));
        assert!(dwell(&mut v, Some("a"), at(5000), after));
        // Another tab, then back: the timer starts again.
        assert!(!dwell(&mut v, Some("b"), at(6000), after));
        assert!(!dwell(&mut v, Some("a"), at(7000), after));
        assert!(!dwell(&mut v, Some("a"), at(11999), after));
        assert!(dwell(&mut v, Some("a"), at(12000), after));
        // Nothing on view resets it too.
        assert!(!dwell(&mut v, None, at(12500), after));
        assert!(!dwell(&mut v, Some("a"), at(13000), after));
        // A zero delay counts at once.
        assert!(dwell(&mut None, Some("a"), at(0), Duration::ZERO));
    }

    fn note(id: &str) -> Note {
        Note {
            id: id.into(),
            title: id.into(),
            stage: Stage::Active,
            path: PathBuf::new(),
            source: "local".into(),
            url: None,
            repo: None,
            repo_path: None,
            created: None,
            moved: None,
        }
    }

    /// Issues "a" and "b", each with Done tabs named by `tabs`, "a" selected.
    /// Built in memory: no vault, state file or sync.
    fn app(tabs: &[(&str, &[&str])]) -> App {
        let notes = tabs.iter().map(|(id, _)| note(id)).collect();
        let vault = Vault { root: PathBuf::from("/nonexistent") };
        let mut app = App::with_parts(Config::default(), PathBuf::from("/"), vault, notes, State::default(), None, None, None);
        for (id, ids) in tabs {
            let tabs = ids
                .iter()
                .map(|t| {
                    let mut tab = tab(ClaudeStatus::Done, 1, None);
                    tab.st.id = (*t).into();
                    tab
                })
                .collect();
            app.rt.insert((*id).into(), IssueRt { tabs, ..Default::default() });
        }
        app.selected = Some(tabs[0].0.into());
        app
    }

    fn shown(app: &App, issue: &str, idx: usize) -> ClaudeStatus {
        app.rt[issue].tabs[idx].shown_status()
    }

    #[test]
    fn flicking_past_an_issue_keeps_its_done_unread() {
        let mut app = app(&[("a", &["a1"]), ("b", &["b1"])]);
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        app.mark_seen(at(0));
        app.mark_seen(at(3));
        app.selected = Some("b".into());
        app.mark_seen(at(4));
        assert_eq!(shown(&app, "a", 0), ClaudeStatus::Done);
        // Back on "a": the timer starts again from there.
        app.selected = Some("a".into());
        app.mark_seen(at(5));
        app.mark_seen(at(9));
        assert_eq!(shown(&app, "a", 0), ClaudeStatus::Done);
        app.mark_seen(at(10));
        assert_eq!(shown(&app, "a", 0), ClaudeStatus::Idle);
        assert_eq!(shown(&app, "b", 0), ClaudeStatus::Done);
    }

    #[test]
    fn switching_tab_restarts_the_timer_and_marks_the_tab_on_view() {
        let mut app = app(&[("a", &["a1", "a2"])]);
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        app.mark_seen(at(0));
        app.rt.get_mut("a").unwrap().active = 1;
        app.mark_seen(at(4));
        app.mark_seen(at(8));
        assert_eq!(shown(&app, "a", 1), ClaudeStatus::Done);
        app.mark_seen(at(9));
        assert_eq!(shown(&app, "a", 0), ClaudeStatus::Done);
        assert_eq!(shown(&app, "a", 1), ClaudeStatus::Idle);
    }

    #[test]
    fn no_tab_on_view_clears_the_timer() {
        let mut app = app(&[("a", &["a1"])]);
        app.mark_seen(Instant::now());
        assert!(app.on_view.is_some());
        app.rt.get_mut("a").unwrap().tabs.clear();
        app.mark_seen(Instant::now());
        assert!(app.on_view.is_none());
    }

    #[test]
    fn a_zero_delay_marks_the_tab_seen_at_once() {
        let mut app = app(&[("a", &["a1"])]);
        app.cfg.seen_after_secs = 0;
        app.mark_seen(Instant::now());
        assert_eq!(shown(&app, "a", 0), ClaudeStatus::Idle);
    }

    #[test]
    fn an_overlay_hides_the_tab() {
        let mut app = app(&[("a", &["a1"])]);
        let t0 = Instant::now();
        app.mode = Mode::Help;
        app.mark_seen(t0);
        app.mark_seen(t0 + Duration::from_secs(10));
        assert_eq!(shown(&app, "a", 0), ClaudeStatus::Done);
        // Closing it starts the timer.
        app.mode = Mode::Normal;
        app.mark_seen(t0 + Duration::from_secs(11));
        app.mark_seen(t0 + Duration::from_secs(16));
        assert_eq!(shown(&app, "a", 0), ClaudeStatus::Idle);
    }

    #[test]
    fn a_done_seen_while_selected_keeps_its_rank_until_you_move() {
        let mut app = app(&[("a", &["a1"]), ("b", &["b1"])]);
        let t0 = Instant::now();
        app.mark_seen(t0);
        app.mark_seen(t0 + Duration::from_secs(5));
        let a1 = |app: &App| app.ranked_status("a", &app.rt["a"].tabs[0]);
        assert_eq!(shown(&app, "a", 0), ClaudeStatus::Idle);
        assert_eq!(a1(&app), ClaudeStatus::Done);
        app.selected = Some("b".into());
        app.mark_seen(t0 + Duration::from_secs(6));
        assert_eq!(a1(&app), ClaudeStatus::Idle);
        // "b" was only flicked past: it ranks as Done.
        assert_eq!(app.ranked_status("b", &app.rt["b"].tabs[0]), ClaudeStatus::Done);
    }

    #[test]
    fn scratch_hides_while_empty_and_unselected() {
        assert!(!scratch_shown(0, false));
        assert!(scratch_shown(0, true));
        assert!(scratch_shown(2, false));
    }

    #[test]
    fn closed_tabs_are_capped_oldest_first() {
        let mut rt = IssueRt::default();
        for i in 0..MAX_CLOSED + 2 {
            let mut st = tab(ClaudeStatus::Exited, 0, None).st;
            st.id = format!("salash-test-no-snapshot-{i}");
            rt.push_closed(st);
        }
        assert_eq!(rt.closed.len(), MAX_CLOSED);
        assert_eq!(rt.closed[0].id, "salash-test-no-snapshot-2");
    }

    #[test]
    fn removing_a_tab_keeps_the_active_one() {
        let rt_with = |active| {
            let mut rt = IssueRt::default();
            for i in 0..3 {
                let mut t = tab(ClaudeStatus::Exited, 0, None);
                t.st.id = i.to_string();
                rt.tabs.push(t);
            }
            rt.active = active;
            rt
        };
        let active = |rt: &IssueRt| rt.tabs[rt.active].st.id.clone();

        let mut rt = rt_with(2);
        rt.remove_tab(0);
        assert_eq!(active(&rt), "2");
        let mut rt = rt_with(0);
        rt.remove_tab(2);
        assert_eq!(active(&rt), "0");
        // The active one itself: the next tab, or the one before the last.
        let mut rt = rt_with(1);
        rt.remove_tab(1);
        assert_eq!(active(&rt), "2");
        rt.remove_tab(1);
        assert_eq!(active(&rt), "0");
        rt.remove_tab(0);
        assert_eq!(rt.active, 0);
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

    /// Never run on a click: if one ever spawned, it would not be Claude.
    const MUST_NOT_SPAWN: &str = "/nonexistent/salash-test-must-not-spawn";

    /// A vault dir removed on drop, even when an assertion fails first.
    struct TempVault(PathBuf);

    impl Drop for TempVault {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// A real App over a fresh vault holding `issues` in Active. Keep the
    /// returned guard alive for as long as the App.
    fn click_app(issues: &[&str]) -> (App, TempVault) {
        // The data dir (state.json, snapshots, status files) is set once per
        // test binary. A fixed dir under target/ keeps it away from the real
        // instance without leaking a new dir into /tmp on every run. Tests
        // share it, so they assert on in-memory state only.
        crate::config::set_data_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("target/salash-unit-data"));
        let vault = TempVault(std::env::temp_dir().join(format!("salash-test-{}", uuid::Uuid::new_v4())));
        let cfg = Config {
            vault: vault.0.clone(),
            inject_hooks: false,
            claude_command: vec![MUST_NOT_SPAWN.into()],
            shell_command: vec![MUST_NOT_SPAWN.into()],
            ..Default::default()
        };
        let mut app = App::new(cfg, false, vault.0.clone()).unwrap();
        // Another test may have saved state in the shared dir; start clean.
        app.state = State::default();
        app.rt.clear();
        for id in issues {
            let note = NewNote { id, title: id, source: "local", url: None, repo: None, stage: Stage::Active };
            app.vault.create(note).unwrap();
        }
        app.rescan();
        app.view = Stage::Active;
        app.selected = None;
        app.msg = None;
        (app, vault)
    }

    fn with_tabs(app: &mut App, id: &str, tabs: Vec<Tab>, active: usize) {
        app.rt.insert(id.to_string(), IssueRt { tabs, active, ..Default::default() });
    }

    /// A Claude tab with no pane and no session: never started.
    fn unstarted_tab() -> Tab {
        tab(ClaudeStatus::Exited, 0, None)
    }

    fn suspended_tab() -> Tab {
        let mut t = unstarted_tab();
        t.st.claude_session = Some("s".into());
        t
    }

    /// A tab whose pane runs `sleep`; dropping the pane kills it.
    fn running_tab(dir: &Path) -> Tab {
        let argv = ["sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
        let mut t = unstarted_tab();
        t.pane = Some(Pane::spawn(&argv, dir, &[], 5, 40).unwrap());
        t
    }

    /// Nothing tried to spawn: a failed spawn would leave `pane == None` but flash.
    fn assert_no_spawn(app: &App) {
        assert!(app.msg.is_none(), "unexpected flash: {:?}", app.msg.as_ref().map(|m| &m.0));
    }

    /// Lay the screen out so the mouse hit rects are where the real UI draws them.
    fn draw(app: &mut App) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        term.draw(|f| crate::ui::draw(app, f)).unwrap();
    }

    fn mouse(app: &mut App, kind: MouseEventKind, pos: Position) {
        app.handle(Event::Mouse(MouseEvent { kind, column: pos.x, row: pos.y, modifiers: KeyModifiers::NONE }));
    }

    /// An App over notes with these ids, keeping its state away from the real instance.
    fn sort_app(ids: &[&str]) -> App {
        crate::config::set_data_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("target/salash-unit-data"));
        let tabs: Vec<(&str, &[&str])> = ids.iter().map(|id| (*id, &[][..])).collect();
        app(&tabs)
    }

    fn order(app: &App) -> Vec<String> {
        app.visible().iter().map(|n| n.id.clone()).collect()
    }

    #[test]
    fn id_sort_groups_sources_and_counts_numbers() {
        let mut app = sort_app(&["office-10", "LOCAL-2", "office-2", "PROJ-1", "local-10"]);
        app.state.sort = SortMode::Id;
        assert_eq!(order(&app), ["LOCAL-2", "local-10", "office-2", "office-10", "PROJ-1"]);
        assert_eq!(app.sort_label(), "↑ id");
        app.state.sort_reversed = true;
        assert_eq!(order(&app), ["PROJ-1", "office-10", "office-2", "local-10", "LOCAL-2"]);
        assert_eq!(app.sort_label(), "↓ id");
    }

    #[test]
    fn i_reverses_the_sort_and_s_starts_the_next_one_unreversed() {
        let mut app = sort_app(&["b", "a"]);
        assert_eq!(app.sort_label(), "↓ created");
        app.on_sidebar_key(KeyEvent::from(KeyCode::Char('i')));
        assert_eq!(order(&app), ["a", "b"]);
        assert_eq!(app.sort_label(), "↑ created");
        app.on_sidebar_key(KeyEvent::from(KeyCode::Char('s')));
        assert_eq!(app.state.sort, SortMode::Id);
        assert!(!app.state.sort_reversed);
        assert_eq!(order(&app), ["a", "b"]);
    }

    #[test]
    fn clicking_the_sort_label_reverses_it() {
        let mut app = sort_app(&["a", "b"]);
        app.state.sort = SortMode::Name;
        draw(&mut app);
        let r = app.sort_hit;
        assert!(r.width > 0, "sort label drawn");
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), Position::new(r.x, r.y));
        assert!(app.state.sort_reversed);
        assert_eq!(order(&app), ["b", "a"]);
        mouse(&mut app, MouseEventKind::Down(MouseButton::Left), Position::new(r.right() - 1, r.y));
        assert!(!app.state.sort_reversed);
    }

    #[test]
    fn the_sort_label_is_clickable_when_it_fits_exactly() {
        // " salash" + "  ⠋ sync" + " ↓ activity " + " quit " fill the header.
        let mut app = sort_app(&["a"]);
        app.state.sort = SortMode::Activity;
        app.syncing = true;
        draw(&mut app);
        assert_eq!(app.sort_hit.right(), app.quit_hit.x);
        assert_eq!(app.sort_hit.x, 15, "the label starts right after the header text");
        assert!(app.sort_hit.width > 0, "sort label drawn");
    }

    #[test]
    fn reversing_keeps_ties_newest_created_first() {
        // `notes` is newest-created first: "new" was created after "old".
        let mut app = sort_app(&["new", "old", "busy"]);
        app.rt.get_mut("busy").unwrap().tabs.push(tab(ClaudeStatus::Attention, 1, None));
        app.state.sort = SortMode::Activity;
        assert_eq!(order(&app), ["busy", "new", "old"]);
        app.state.sort_reversed = true;
        assert_eq!(order(&app), ["new", "old", "busy"]);

        for n in &mut app.notes {
            n.title = if n.id == "busy" { "a".into() } else { "same".into() };
        }
        app.state.sort = SortMode::Name;
        assert_eq!(order(&app), ["new", "old", "busy"]);
        app.state.sort_reversed = false;
        assert_eq!(order(&app), ["busy", "new", "old"]);
    }

    #[test]
    fn undated_issues_stay_last_in_the_moved_sort() {
        let mut app = sort_app(&["undated", "early", "late"]);
        for n in &mut app.notes {
            n.moved = match n.id.as_str() {
                "early" => Some("2026-01-01T00:00:00".into()),
                "late" => Some("2026-02-01T00:00:00".into()),
                _ => None,
            };
        }
        app.state.sort = SortMode::Moved;
        assert_eq!(order(&app), ["late", "early", "undated"]);
        app.state.sort_reversed = true;
        assert_eq!(order(&app), ["early", "late", "undated"]);
    }

    #[test]
    fn sort_label_shows_each_modes_direction() {
        let mut app = sort_app(&["a"]);
        for (mode, label, reversed) in [
            (SortMode::Created, "↓ created", "↑ created"),
            (SortMode::Id, "↑ id", "↓ id"),
            (SortMode::Name, "↑ name", "↓ name"),
            (SortMode::Moved, "↓ moved", "↑ moved"),
            (SortMode::Activity, "↓ activity", "↑ activity"),
        ] {
            app.state.sort = mode;
            app.state.sort_reversed = false;
            assert_eq!(app.sort_label(), label);
            app.state.sort_reversed = true;
            assert_eq!(app.sort_label(), reversed);
        }
    }

    /// Left-click the issue's row.
    fn click(app: &mut App, id: &str) {
        draw(app);
        let r = app.item_hits.iter().find(|(_, i)| i == id).map(|(r, _)| *r).expect("issue row drawn");
        mouse(app, MouseEventKind::Down(MouseButton::Left), Position::new(r.x, r.y));
    }

    /// Scroll the wheel down once over the sidebar's issue list.
    fn wheel_down_sidebar(app: &mut App) {
        draw(app);
        let pos = app.item_hits.first().expect("issue row drawn").0;
        let pos = Position::new(pos.x, pos.y);
        mouse(app, MouseEventKind::ScrollDown, pos);
    }

    #[test]
    fn clicking_an_issue_focuses_its_last_active_tab() {
        let (mut app, vault) = click_app(&["a"]);
        with_tabs(&mut app, "a", vec![unstarted_tab(), running_tab(&vault.0)], 1);

        click(&mut app, "a");
        assert_eq!(app.selected.as_deref(), Some("a"));
        assert_eq!(app.focus, Focus::Terminal);
        assert_eq!(app.rt["a"].active, 1);
        assert!(app.rt["a"].tabs[0].pane.is_none());
        assert_no_spawn(&app);

        // Clicking the issue that is already selected also jumps into its tab.
        app.focus = Focus::Sidebar;
        click(&mut app, "a");
        assert_eq!(app.selected.as_deref(), Some("a"));
        assert_eq!(app.focus, Focus::Terminal);
        assert_no_spawn(&app);
    }

    #[test]
    fn clicking_an_issue_shows_a_suspended_tab_without_resuming_it() {
        let (mut app, _vault) = click_app(&["a"]);
        with_tabs(&mut app, "a", vec![suspended_tab()], 0);

        click(&mut app, "a");
        assert_eq!(app.focus, Focus::Terminal);
        let rt = &app.rt["a"];
        assert_eq!(rt.tabs.len(), 1);
        assert!(rt.tabs[0].pane.is_none() && rt.tabs[0].suspended());
        assert_no_spawn(&app);
    }

    #[test]
    fn clicking_an_issue_shows_an_exited_tab_without_restarting_it() {
        let (mut app, vault) = click_app(&["a"]);
        let mut exited = running_tab(&vault.0);
        exited.pane.as_mut().unwrap().exited = Some(1);
        with_tabs(&mut app, "a", vec![exited], 0);

        click(&mut app, "a");
        assert_eq!(app.focus, Focus::Terminal);
        let rt = &app.rt["a"];
        assert_eq!(rt.tabs.len(), 1);
        assert_eq!(rt.tabs[0].pane.as_ref().and_then(|p| p.exited), Some(1));
        assert_no_spawn(&app);
    }

    #[test]
    fn clicking_an_issue_without_a_live_tab_stays_in_the_sidebar() {
        let (mut app, _vault) = click_app(&["empty", "unstarted"]);
        with_tabs(&mut app, "unstarted", vec![unstarted_tab()], 0);

        click(&mut app, "empty");
        assert_eq!(app.selected.as_deref(), Some("empty"));
        assert_eq!(app.focus, Focus::Sidebar);
        assert!(app.rt["empty"].tabs.is_empty() && app.rt["empty"].worktree.is_none());

        click(&mut app, "unstarted");
        assert_eq!(app.selected.as_deref(), Some("unstarted"));
        assert_eq!(app.focus, Focus::Sidebar);
        assert!(app.rt["unstarted"].tabs[0].pane.is_none());
        assert_no_spawn(&app);
    }

    #[test]
    fn moving_from_a_tab_to_an_issue_without_a_live_tab_focuses_the_sidebar() {
        // The sidebar wheel and prefix j both leave a focused tab for the next issue.
        let moves: [fn(&mut App); 2] = [wheel_down_sidebar, |app| {
            let (code, mods) = app.prefix;
            app.handle(Event::Key(KeyEvent::new(code, mods)));
            app.handle(Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)));
        }];
        for move_down in moves {
            let (mut app, _vault) = click_app(&["a", "b", "c"]);
            with_tabs(&mut app, "a", vec![suspended_tab()], 0);
            with_tabs(&mut app, "b", vec![suspended_tab()], 0);
            with_tabs(&mut app, "c", vec![unstarted_tab()], 0);
            click(&mut app, "a");
            assert_eq!(app.focus, Focus::Terminal);

            // A suspended tab is live: stay in the terminal.
            move_down(&mut app);
            assert_eq!(app.selected.as_deref(), Some("b"));
            assert_eq!(app.focus, Focus::Terminal);

            // An unstarted tab is not: back to the sidebar.
            move_down(&mut app);
            assert_eq!(app.selected.as_deref(), Some("c"));
            assert_eq!(app.focus, Focus::Sidebar);
            assert!(app.rt.values().flat_map(|rt| &rt.tabs).all(|t| t.pane.is_none()));
            assert_no_spawn(&app);
        }
    }

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("salash-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// One issue "a" with a Claude tab "t" that has no directory yet. State is saved to a
    /// temp dir, never to the `$SALASH_DATA` of the salash these tests may run in.
    fn worktree_app(cfg: Config, note: Note) -> App {
        crate::config::set_data_dir(std::env::temp_dir().join(format!("salash-test-data-{}", std::process::id())));
        let vault = Vault { root: PathBuf::from("/nonexistent") };
        let mut app = App::with_parts(cfg, PathBuf::from("/"), vault, vec![note], State::default(), None, None, None);
        let mut t = tab(ClaudeStatus::Idle, 0, None);
        t.st.id = "t".into();
        app.rt.insert("a".into(), IssueRt { tabs: vec![t], ..Default::default() });
        app
    }

    fn github_note(repo: &str) -> Note {
        Note { source: "github".into(), repo: Some(repo.into()), ..note("a") }
    }

    fn workdir_of(app: &mut App) -> Workdir {
        let n = app.notes[0].clone();
        app.workdir(&n, "t")
    }

    #[test]
    fn with_worktrees_off_a_tab_starts_in_the_repo() {
        let repo = temp_dir();
        let cfg = Config { worktrees: false, local_repo: Some(repo.clone()), ..Config::default() };
        let mut app = worktree_app(cfg, note("a"));
        assert!(matches!(workdir_of(&mut app), Workdir::Repo(p) if p == repo));
        assert!(app.rt["a"].creating.is_none());
        std::fs::remove_dir_all(repo).ok();
    }

    #[test]
    fn with_worktrees_off_the_notes_repo_path_wins() {
        let (local, own) = (temp_dir(), temp_dir());
        let cfg = Config { worktrees: false, local_repo: Some(local.clone()), ..Config::default() };
        let mut app = worktree_app(cfg, Note { repo_path: Some(own.clone()), ..note("a") });
        assert!(matches!(workdir_of(&mut app), Workdir::Repo(p) if p == own));
        std::fs::remove_dir_all(local).ok();
        std::fs::remove_dir_all(own).ok();
    }

    #[test]
    fn with_worktrees_off_an_issue_with_no_repo_still_has_none() {
        let cfg = Config { worktrees: false, ..Config::default() };
        let mut app = worktree_app(cfg, note("a"));
        assert!(matches!(workdir_of(&mut app), Workdir::NoRepo));
    }

    #[test]
    fn a_source_can_turn_worktrees_off_for_its_issues_only() {
        let repo = temp_dir();
        let cfg: Config = toml::from_str(&format!(
            r#"
            [[github]]
            repo = "a/off"
            path = "{0}"
            worktrees = false
            [[github]]
            repo = "a/on"
            path = "{0}"
            "#,
            repo.display()
        ))
        .unwrap();
        let mut off = worktree_app(cfg.clone(), github_note("a/off"));
        assert!(matches!(workdir_of(&mut off), Workdir::Repo(p) if p == repo));
        // Not a git repo, so the worktree the other source asks for fails in the background.
        let mut on = worktree_app(cfg, github_note("a/on"));
        assert!(matches!(workdir_of(&mut on), Workdir::Pending));
        assert!(on.rt["a"].creating.is_some());
    }

    #[test]
    fn a_worktree_made_before_keeps_being_used_with_worktrees_off() {
        let (repo, wt) = (temp_dir(), temp_dir());
        let cfg = Config { worktrees: false, local_repo: Some(repo.clone()), ..Config::default() };
        let mut app = worktree_app(cfg, note("a"));
        app.rt.get_mut("a").unwrap().worktree = Some(wt.clone());
        assert!(matches!(workdir_of(&mut app), Workdir::Ready(p) if p == wt));
        std::fs::remove_dir_all(repo).ok();
        std::fs::remove_dir_all(wt).ok();
    }

    #[test]
    fn a_tab_started_in_the_repo_keeps_it_for_resume() {
        let repo = temp_dir();
        let cfg = Config {
            worktrees: false,
            local_repo: Some(repo.clone()),
            claude_command: vec!["/nonexistent/claude".into()],
            ..Config::default()
        };
        let mut app = worktree_app(cfg, note("a"));
        app.spawn_pane("a", 0);
        assert_eq!(app.rt["a"].tabs[0].st.cwd.as_deref(), Some(repo.as_path()));
        // Saved before the process starts, so a resume after turning worktrees on finds it.
        assert_eq!(app.state.issues["a"].tabs[0].cwd.as_deref(), Some(repo.as_path()));
        app.cfg.worktrees = true;
        assert!(matches!(workdir_of(&mut app), Workdir::Pending));
        let mut again = worktree_app(app.cfg.clone(), note("a"));
        again.rt.get_mut("a").unwrap().tabs[0].st.cwd = app.state.issues["a"].tabs[0].cwd.clone();
        again.spawn_pane("a", 0);
        assert!(again.rt["a"].creating.is_none(), "a tab with a directory never asks for a worktree");
        std::fs::remove_dir_all(repo).ok();
    }
}
