use crate::api::{self, Reply, Request};
use crate::complete::{self, Completion};
use crate::config::Config;
use crate::keys;
use crate::plugin;
use crate::proto::ServerMsg;
use crate::providers::{self, SyncReport};
use crate::state::{self, IssueState, SortMode, State, TabKind, TabState};
use crate::status::{self, ClaudeStatus};
use crate::term::{self, Pane, Snapshot};
use crate::transcript;
use crate::vault::{NewNote, Note, Stage, Vault};
use crate::worktree;
use anyhow::Result;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::collections::{HashMap, HashSet};
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
    /// The session `I` opened this tab to resume, until its first hook
    /// records it as `claude_session`.
    resuming: Option<String>,
}

impl Tab {
    fn new(st: TabState) -> Tab {
        Tab { st, pane: None, status: ClaudeStatus::Exited, status_ts: 0, seen_ts: 0, esc_ts: None, exit_check: None, first_args: vec![], resuming: None }
    }

    /// The tab runs or holds this Claude session, or is starting it.
    fn has_session(&self, sid: &str) -> bool {
        self.st.claude_session.as_deref() == Some(sid) || self.resuming.as_deref() == Some(sid)
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

    /// What keeps the tab busy, as the confirm before stopping it names it;
    /// None if stopping it loses nothing under way. A shell is busy while a
    /// command runs in front of its prompt; an open note editor always is.
    fn busy_label(&self) -> Option<String> {
        if !self.running() {
            return None;
        }
        match self.st.kind {
            TabKind::Claude => match self.status {
                ClaudeStatus::Working => Some("Claude: working".into()),
                ClaudeStatus::Starting => Some("Claude: starting".into()),
                ClaudeStatus::Attention => Some("Claude: waiting for you".into()),
                _ => None,
            },
            TabKind::Shell => {
                let pid = self.pane.as_ref()?.foreground_pid()?;
                Some(format!("shell: {}", term::process_name(pid).unwrap_or_else(|| "running command".into())))
            }
            TabKind::Notes => Some("note editor".into()),
        }
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
    /// Closed Claude tabs for `R`. Only resumable sessions are ever put here
    /// (`close_tab_at`, `retire_tabs`), so they aren't checked again.
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
    ResumeId,
}

impl PromptKind {
    fn is_dir(&self) -> bool {
        matches!(self, PromptKind::ClaudeDir | PromptKind::DefaultDir)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub enum ConfirmKind {
    Quit,
    CloseTab,
    Suspend,
    /// Move the issue to Done or Archive, stopping its tabs, `busy` among them.
    Retire { issue: String, stage: Stage, busy: Vec<String> },
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
    /// The frame line under the tab bar.
    pub frame_rect: Rect,
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
    /// Busy tabs last reported left running on an issue moved to Done or
    /// Archive outside salash, so the rescan flashes only when that changes.
    busy_left: HashMap<String, Vec<String>>,
    /// Issues whose resume lines couldn't be written to the note, so the
    /// rescan retrying it flashes only the first failure.
    write_failed: HashSet<String>,
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

/// Issues in these stages are finished: their tabs are stopped, and no new ones open.
fn retires(st: Stage) -> bool {
    matches!(st, Stage::Done | Stage::Archive)
}

/// "1 tab", "3 busy tabs".
pub fn tabs_n(n: usize, adj: &str) -> String {
    format!("{n} {adj}{}", if n == 1 { "tab" } else { "tabs" })
}

/// Why an issue's stopped sessions are still in salash and not in its note.
fn not_written(id: &str, e: &str) -> String {
    format!("{id}: resume commands not written to the note, sessions kept for R: {e}")
}

/// A word as the shell reads it back: bare if it's only safe characters,
/// else quoted. Control characters, which would break the note's line, are
/// written as escapes in `$'…'`, which bash and zsh read.
fn shell_word(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+,:@%".contains(c)) {
        s.to_string()
    } else if s.chars().any(char::is_control) {
        let mut out = String::from("$'");
        for c in s.chars() {
            match c {
                '\\' | '\'' => out.extend(['\\', c]),
                // \x is the form every shell with `$'…'` reads; \u needs bash 4.2.
                c if c.is_ascii_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
                c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('\'');
        out
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// What was pasted into `I`: the session id itself, or a command holding
/// `--resume <id>`, such as a `## Sessions` line of the note. The directory
/// comes from a `cd <dir> &&` in front of it, if there is one.
fn parse_session_id(input: &str) -> Option<(String, Option<PathBuf>)> {
    let mut words = input.split_whitespace().map(|w| w.trim_matches('`'));
    let word = if input.contains("--resume") {
        words.skip_while(|w| *w != "--resume").nth(1)?
    } else {
        let w = words.next()?;
        if words.next().is_some() {
            return None;
        }
        w
    };
    // Claude's ids are hyphenated; the other forms the uuid crate takes aren't ids.
    let sid = (word.len() == 36).then(|| uuid::Uuid::try_parse(word).ok()).flatten()?.hyphenated().to_string();
    let dir = input.split_once("&&").and_then(|(before, _)| {
        let (_, dir) = before.rsplit_once("cd ")?;
        match shell_words::split(dir).ok()?.as_slice() {
            [d] => Some(PathBuf::from(d)),
            _ => None,
        }
    });
    Some((sid, dir))
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
            frame_rect: Rect::default(),
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
            busy_left: HashMap::new(),
            write_failed: HashSet::new(),
        }
    }

    /// The configured Claude command as the note's resume lines write it.
    fn claude_words(&self) -> String {
        match self.cfg.claude_command.as_slice() {
            [] => "claude".into(),
            cmd => cmd.iter().map(|w| shell_word(w)).collect::<Vec<_>>().join(" "),
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
        self.retire_moved();
        self.last_scan = Instant::now();
    }

    /// The issue is in Done or Archive. Scratch never is.
    fn retired(&self, id: &str) -> bool {
        self.notes.iter().any(|n| n.id == id && retires(n.stage))
    }

    /// Say so and return true if the issue is in Done or Archive, where no tab opens.
    fn refuse_retired(&mut self, id: &str) -> bool {
        let retired = self.retired(id);
        if retired {
            self.flash("Move to Active first (u)".into());
        }
        retired
    }

    /// The issue has tabs or closed tabs, loaded or only saved.
    fn has_tabs(&self, id: &str) -> bool {
        match self.rt.get(id) {
            Some(rt) => !rt.tabs.is_empty() || !rt.closed.is_empty(),
            None => self.state.issues.get(id).is_some_and(|s| !s.tabs.is_empty() || !s.closed.is_empty()),
        }
    }

    /// Stop the tabs of issues moved to Done or Archive outside salash (in
    /// Obsidian, or before salash did it on a move). Nobody confirmed that,
    /// so busy tabs keep running, and a later rescan stops them once idle.
    fn retire_moved(&mut self) {
        let notes: Vec<Note> = self.notes.iter().filter(|n| retires(n.stage) && self.has_tabs(&n.id)).cloned().collect();
        self.busy_left.retain(|id, _| notes.iter().any(|n| &n.id == id));
        self.write_failed.retain(|id| notes.iter().any(|n| &n.id == id));
        let (mut touched_selected, mut said, mut failed) = (false, vec![], false);
        for note in notes {
            let (stopped, busy, error) = self.retire_tabs(&note, false);
            touched_selected |= stopped > 0 && self.selected.as_deref() == Some(note.id.as_str());
            let stage = note.stage.label();
            if let Some(e) = error {
                if self.write_failed.insert(note.id.clone()) {
                    self.flash_error(not_written(&note.id, &e));
                    failed = true;
                }
                continue;
            }
            self.write_failed.remove(&note.id);
            if busy.is_empty() {
                self.busy_left.remove(&note.id);
                if stopped > 0 {
                    said.push(format!("{} is in {stage}: {} stopped", note.id, tabs_n(stopped, "")));
                }
            } else if self.busy_left.get(&note.id) != Some(&busy) {
                said.push(format!(
                    "{} is in {stage}: left {} running ({}), stopped once idle",
                    note.id,
                    tabs_n(busy.len(), "busy "),
                    busy.join(", ")
                ));
                self.busy_left.insert(note.id.clone(), busy);
            }
        }
        // One message for all, so one issue's doesn't hide another's; a failure goes first.
        if !said.is_empty() && !failed {
            self.flash(said.join("; "));
        }
        // Only when a tab on view went; a tab waiting for its worktree keeps focus.
        if touched_selected {
            self.after_issue_switch();
        }
    }

    /// Stop an issue's tabs as it goes to Done or Archive. First each Claude
    /// session, closed ones included, gets a line in the note to resume it by;
    /// then saved screens and the closed tabs go too. Unless `force`, busy tabs
    /// keep running. If the lines can't be written, or a kept note editor
    /// would save over them, the sessions are kept as closed tabs instead, for
    /// `R` and a later try. Returns how many tabs stopped, what keeps the rest
    /// busy, and why writing the note failed.
    fn retire_tabs(&mut self, note: &Note, force: bool) -> (usize, Vec<String>, Option<String>) {
        let rt = self.rt_for(&note.id);
        let active = rt.tabs.get(rt.active).map(|t| t.st.id.clone());
        let (mut keep, mut stop, mut busy) = (vec![], vec![], vec![]);
        for t in std::mem::take(&mut rt.tabs) {
            match t.busy_label().filter(|_| !force) {
                Some(b) => {
                    busy.push(b);
                    keep.push(t);
                }
                None => stop.push(t),
            }
        }
        let mut closed = std::mem::take(&mut rt.closed);
        let had_closed = !closed.is_empty();
        let worktree = rt.worktree.clone();
        // Read before the status files go: a session with no messages can't be resumed.
        let (sessions, others): (Vec<Tab>, Vec<Tab>) =
            stop.into_iter().partition(|t| t.st.kind == TabKind::Claude && t.resumable());
        let stopped = sessions.len() + others.len();

        let when = chrono::Local::now().format("%Y-%m-%d %H:%M");
        let claude = self.claude_words();
        let mut seen = HashSet::new();
        let mut lines = vec![];
        for st in sessions.iter().map(|t| &t.st).chain(&closed) {
            let Some(sid) = st.claude_session.as_deref().filter(|s| seen.insert(s.to_string())) else { continue };
            // A tab started in the worktree keeps no directory of its own.
            let cmd = match st.cwd.as_ref().or(worktree.as_ref()) {
                Some(dir) => format!("cd {} && {claude} --resume {sid}", shell_word(&dir.to_string_lossy())),
                None => format!("{claude} --resume {sid}"),
            };
            lines.push(format!("- {when} · stopped · {} · `{cmd}`", st.name));
        }
        let editor_open = keep.iter().any(|t| t.st.kind == TabKind::Notes);
        let mut error = None;
        let written = !editor_open
            && (lines.is_empty()
                || match self.vault.append_session_lines(note, &lines) {
                    Ok(()) => true,
                    Err(e) => {
                        error = Some(format!("{e:#}"));
                        false
                    }
                });
        if written {
            for st in &closed {
                state::remove_snapshot(&st.id);
            }
            closed.clear();
        }
        for t in &sessions {
            if written {
                state::remove_snapshot(&t.st.id);
            } else {
                t.save_screen();
            }
            status::clear_status(&t.st.id);
        }
        for t in &others {
            status::clear_status(&t.st.id);
            state::remove_snapshot(&t.st.id);
        }
        if !written {
            // Not trimmed to MAX_CLOSED: none of these is in the note yet.
            closed.extend(sessions.iter().map(|t| t.st.clone()));
        }
        let changed = stopped > 0 || written && had_closed;
        let rt = self.rt_for(&note.id);
        rt.tabs = keep;
        rt.active = active.and_then(|a| rt.tabs.iter().position(|t| t.st.id == a)).unwrap_or(0);
        rt.closed = closed;
        if changed {
            self.persist(&note.id);
        }
        (stopped, busy, error)
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

    /// Move the selected issue. Done and Archive stop its tabs, asking first
    /// if any of them is busy.
    fn move_stage(&mut self, st: Stage) {
        let Some(id) = self.selected_note().map(|n| n.id.clone()) else { return };
        if retires(st) {
            let busy: Vec<String> = self.rt_for(&id).tabs.iter().filter_map(Tab::busy_label).collect();
            if !busy.is_empty() {
                self.mode = Mode::Confirm(ConfirmKind::Retire { issue: id, stage: st, busy });
                return;
            }
        }
        self.do_move_stage(&id, st);
    }

    fn do_move_stage(&mut self, id: &str, st: Stage) {
        let Some(note) = self.notes.iter().find(|n| n.id == id).cloned() else { return };
        // Pick the neighbour before moving, so the cursor stays in place.
        let ids = self.rows();
        let pos = ids.iter().position(|i| i == &note.id).unwrap_or(0);
        match self.vault.move_to(&note, st) {
            Ok(moved) => {
                let mut msg = format!("{} → {}", note.id, st.label());
                // Before the rescan, which would leave busy tabs running.
                let mut error = None;
                if retires(st) {
                    let (stopped, _, e) = self.retire_tabs(&moved, true);
                    if stopped > 0 {
                        msg.push_str(&format!(" · {} stopped", tabs_n(stopped, "")));
                    }
                    error = e;
                }
                match error {
                    Some(e) => {
                        // The rescan below tries again; it needn't say so twice.
                        self.write_failed.insert(id.to_string());
                        self.flash_error(format!("{msg} · {}", not_written(id, &e)));
                    }
                    None => self.flash(msg),
                }
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
        if (note.is_none() && id != SCRATCH) || self.refuse_retired(id) {
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
        if self.refuse_retired(&id) {
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
        } else if self.retired(&req.issue) {
            return Reply::err(format!("{} is finished (Done or Archive): Move to Active first (u)", req.issue));
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

    /// Open a Claude tab on the selected issue running `claude --resume <id>`,
    /// where `c` would start one, or in the directory a pasted `cd` names.
    /// A session already in a tab, in any issue, is only shown, and one
    /// closed on this issue comes back as it was.
    fn resume_session(&mut self, input: String) {
        let Some((sid, dir)) = parse_session_id(&input) else {
            self.flash(format!("Not a session ID: {}", input.trim()));
            self.mode = Mode::Prompt(PromptKind::ResumeId, input);
            return;
        };
        let Some(id) = self.selected.clone() else { return };
        if self.refuse_retired(&id) {
            return;
        }
        if let Some((issue, idx)) = self.find_session(&sid) {
            if issue != id {
                if let Some(stage) = self.notes.iter().find(|n| n.id == issue).map(|n| n.stage) {
                    self.view = stage;
                }
                self.select_id(issue.clone());
            }
            self.switch_tab(idx);
            self.view_tab();
            self.flash(if issue == id { "That session is open in this tab".into() } else { format!("That session is open in {issue}") });
            return;
        }
        self.msg = None;
        self.start_work(&id);
        let rt = self.rt_for(&id);
        // A closed tab comes back as it was: its name, directory and saved screen.
        if let Some(i) = rt.closed.iter().position(|st| st.claude_session.as_deref() == Some(sid.as_str())) {
            rt.tabs.push(Tab::new(rt.closed.remove(i)));
            rt.active = rt.tabs.len() - 1;
            let idx = rt.active;
            self.persist(&id);
            self.spawn_pane(&id, idx);
        } else {
            // A new tab as `c` opens, so it starts in the same directory, and
            // its first hook records the session and writes its Sessions line.
            let cwd = dir.filter(|d| d.is_dir());
            let tab = self.add_tab(&id, TabKind::Claude, None, cwd, vec!["--resume".into(), sid.clone()], true);
            if let Some(t) = self.rt_for(&id).tabs.iter_mut().find(|t| t.st.id == tab) {
                t.resuming = Some(sid);
            }
        }
        self.focus = Focus::Terminal;
    }

    /// The issue and tab holding a Claude session, saved tabs of issues not
    /// loaded yet included.
    fn find_session(&mut self, sid: &str) -> Option<(String, usize)> {
        let saved: Vec<String> = self
            .state
            .issues
            .iter()
            .filter(|(id, s)| !self.rt.contains_key(*id) && s.tabs.iter().any(|t| t.claude_session.as_deref() == Some(sid)))
            .map(|(id, _)| id.clone())
            .collect();
        for id in saved {
            self.rt_for(&id);
        }
        self.rt.iter().find_map(|(id, rt)| Some((id.clone(), rt.tabs.iter().position(|t| t.has_session(sid))?)))
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
        if self.refuse_retired(&id) {
            return;
        }
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
                let claude = self.claude_words();
                let _ = self.vault.append_session_lines(&note, &[format!("- {when} · {name} · `{claude} --resume {sid}`")]);
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
                } else if self.frame_rect.contains(pos) {
                    if self.can_focus_terminal() {
                        self.focus = Focus::Terminal;
                    }
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
                        ConfirmKind::Retire { issue, stage, .. } => self.do_move_stage(&issue, stage),
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
                            PromptKind::ResumeId => self.resume_session(buf),
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
                // A typed character that isn't a sidebar key was likely meant for the tab.
                let typed = matches!(k.code, KeyCode::Char(_)) && k.modifiers.difference(KeyModifiers::SHIFT).is_empty();
                if !self.on_command(k) && typed && self.can_focus_terminal() {
                    self.flash(format!("Sidebar has focus — Tab or {} s to type in the terminal", self.prefix_label()));
                }
            }
        }
    }

    /// Commands shared by the sidebar (bare keys) and the prefix table.
    fn on_command(&mut self, k: KeyEvent) -> bool {
        match k.code {
            KeyCode::Char('c') => self.new_tab(TabKind::Claude),
            KeyCode::Char(c @ ('C' | 'I')) => match self.selected.clone() {
                None => self.flash("Select an issue first".into()),
                Some(id) if self.refuse_retired(&id) => {}
                Some(_) => {
                    let kind = if c == 'C' { PromptKind::ClaudeDir } else { PromptKind::ResumeId };
                    self.mode = Mode::Prompt(kind, String::new());
                }
            },
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
pub(crate) mod tests {
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
    pub(crate) struct TempVault(PathBuf);

    impl Drop for TempVault {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    /// A real App over a fresh vault holding `issues` in Active. Keep the
    /// returned guard alive for as long as the App.
    pub(crate) fn click_app(issues: &[&str]) -> (App, TempVault) {
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

    pub(crate) fn with_tabs(app: &mut App, id: &str, tabs: Vec<Tab>, active: usize) {
        app.rt.insert(id.to_string(), IssueRt { tabs, active, ..Default::default() });
    }

    /// A Claude tab with no pane and no session: never started.
    pub(crate) fn unstarted_tab() -> Tab {
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

    /// Draw `app` into a `w`×`h` buffer.
    fn render(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut t = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        t.draw(|f| crate::ui::draw(app, f)).unwrap();
        t.backend().buffer().clone()
    }

    fn row(buf: &ratatui::buffer::Buffer, y: u16, from: u16) -> String {
        (from..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// Issue `a` has two suspended tabs, so the terminal can take focus; `b` has none.
    fn focus_app(focus: Focus) -> App {
        let mut app = app(&[("a", &["t1", "t2"]), ("b", &[])]);
        for t in app.rt.get_mut("a").unwrap().tabs.iter_mut() {
            t.st.name = t.st.id.clone();
            t.st.claude_session = Some("s".into());
        }
        app.view = Stage::Active;
        app.focus = focus;
        app
    }

    #[test]
    fn moving_focus_never_resizes_the_terminal() {
        let mut app = focus_app(Focus::Sidebar);
        render(&mut app, 100, 30);
        let side = app.term_rect;
        app.focus = Focus::Terminal;
        render(&mut app, 100, 30);
        assert_eq!(app.term_rect, side);
        // Tab bar, frame line and status bar.
        assert_eq!(app.term_rect.height, 30 - 3);
        assert_eq!(app.term_rect.y, 2);
    }

    #[test]
    fn the_frame_is_heavy_only_with_the_terminal_focused() {
        // By shape, not colour.
        // With the first tab active the terminal's frame climbs up beside it.
        for (focus, active, top, joint, column, line) in [
            (Focus::Terminal, 0, "┃", "┣", "┃", "━"),
            (Focus::Terminal, 1, "│", "┢", "┃", "━"),
            (Focus::Sidebar, 0, "│", "├", "│", "─"),
        ] {
            let mut app = focus_app(focus);
            app.rt.get_mut("a").unwrap().active = active;
            let buf = render(&mut app, 100, 30);
            let x = app.sidebar_rect.right() - 1;
            assert_eq!(buf[(x, 0)].symbol(), top, "beside the tab bar");
            assert_eq!(buf[(x, 1)].symbol(), joint);
            assert_eq!(buf[(x, 15)].symbol(), column);
            assert_eq!(buf[(x, 29)].symbol(), "│", "beside the status bar stays thin");
            assert_eq!(row(&buf, 1, x + 1), line.repeat((99 - x) as usize));
        }
    }

    #[test]
    fn the_bar_and_the_tab_marker_follow_focus() {
        let mut app = focus_app(Focus::Sidebar);
        let buf = render(&mut app, 100, 30);
        let x = app.term_rect.x;
        let item = app.item_hits.iter().find(|(_, id)| id == "a").unwrap().0;
        assert_eq!(buf[(item.x, item.y)].symbol(), "▌");
        assert_eq!(buf[(x, 0)].symbol(), " ", "no marker while the sidebar has focus");
        let (t1, t2) = (app.tab_hits[0].0.x + 2, app.tab_hits[1].0.x + 2);
        assert_ne!(buf[(t1, 0)].bg, buf[(t2, 0)].bg, "the active tab still stands out");

        app.focus = Focus::Terminal;
        let buf = render(&mut app, 100, 30);
        let item = app.item_hits.iter().find(|(_, id)| id == "a").unwrap().0;
        assert_eq!(buf[(item.x, item.y)].symbol(), " ");
        assert_eq!(buf[(x - 1, 0)].symbol(), "┃", "the frame marks the first tab, in its column");
        assert_eq!(buf[(x, 0)].symbol(), " ");
        assert_eq!(buf[(app.tab_hits[1].0.x, 0)].symbol(), " ", "only the active tab is marked");

        app.rt.get_mut("a").unwrap().active = 1;
        let buf = render(&mut app, 100, 30);
        assert_eq!(buf[(x - 1, 0)].symbol(), "│");
        assert_eq!(buf[(x, 0)].symbol(), " ");
        assert_eq!(buf[(app.tab_hits[1].0.x, 0)].symbol(), "▌");
    }

    fn luma(c: ratatui::style::Color) -> f32 {
        let ratatui::style::Color::Rgb(r, g, b) = c else { panic!("{c:?} isn't RGB") };
        0.3 * r as f32 + 0.59 * g as f32 + 0.11 * b as f32
    }

    #[test]
    fn the_side_without_focus_is_dimmed() {
        // In the dark theme dimmed text is darker. The issue list dims; the
        // " salash" header and the key footer stay lit.
        let mut app = focus_app(Focus::Sidebar);
        let lit = render(&mut app, 100, 30);
        let item = app.item_hits.iter().find(|(_, id)| id == "a").unwrap().0;
        // The issue's title, on its second line.
        let (ix, iy) = (item.x + 3, item.y + 1);
        let (tx, sy) = (app.tab_hits[1].0.x + 3, 29);
        app.focus = Focus::Terminal;
        let dim = render(&mut app, 100, 30);
        assert!(luma(dim[(ix, iy)].fg) < luma(lit[(ix, iy)].fg), "issue list");
        assert_eq!(dim[(1, 0)].fg, lit[(1, 0)].fg, "header stays lit");
        assert_eq!(dim[(1, 25)].fg, lit[(1, 25)].fg, "footer stays lit");
        // And the other way: tab bar and status bar dim with the sidebar focused.
        assert!(luma(lit[(tx, 0)].fg) < luma(dim[(tx, 0)].fg), "tab bar");
        assert!(luma(lit[(99, sy)].bg) < luma(dim[(99, sy)].bg), "status bar");
    }

    #[test]
    fn clicking_the_frame_line_focuses_the_terminal() {
        let mut app = focus_app(Focus::Sidebar);
        render(&mut app, 100, 30);
        let (x, y) = (app.frame_rect.x + 5, app.frame_rect.y);
        app.handle(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(app.focus == Focus::Terminal);
    }

    #[test]
    fn closing_a_tab_names_it() {
        let mut app = focus_app(Focus::Sidebar);
        app.mode = Mode::Confirm(ConfirmKind::CloseTab);
        let buf = render(&mut app, 120, 30);
        assert!((0..30).any(|y| row(&buf, y, 0).contains("Close tab \"t1\"")));
    }

    #[test]
    fn dialogs_are_drawn_over_the_dimming() {
        let mut app = focus_app(Focus::Terminal);
        app.mode = Mode::Help;
        let buf = render(&mut app, 100, 40);
        // The help box's title sits over the sidebar; its border keeps the accent.
        let y = (0..40).find(|&y| row(&buf, y, 0).contains("Keys — any key")).unwrap();
        let x = (0..100).find(|&x| buf[(x, y)].symbol() == "╭").unwrap();
        assert_eq!(buf[(x, y)].fg, crate::theme::get().accent);
    }

    #[test]
    fn tiny_and_empty_screens_still_draw() {
        let mut app = focus_app(Focus::Terminal);
        for (w, h) in [(1, 1), (10, 3), (40, 5), (60, 6)] {
            render(&mut app, w, h);
        }
        app.selected = None;
        app.rt.clear();
        app.focus = Focus::Sidebar;
        render(&mut app, 80, 20);
    }

    #[test]
    fn a_key_the_sidebar_ignores_says_where_focus_is() {
        let press = |app: &mut App, code, mods| app.handle(Event::Key(KeyEvent::new(code, mods)));
        let mut app = focus_app(Focus::Sidebar);
        press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        let msg = app.msg.take().map(|m| m.0).unwrap_or_default();
        assert!(msg.starts_with("Sidebar has focus"), "{msg}");
        assert!(msg.contains(&format!("{} s", app.prefix_label())));
        assert!(app.focus == Focus::Sidebar && app.mode == Mode::Normal);

        press(&mut app, KeyCode::Char('Y'), KeyModifiers::SHIFT);
        assert!(app.msg.take().is_some(), "shift still counts as typing");

        press(&mut app, KeyCode::Char('y'), KeyModifiers::ALT);
        press(&mut app, KeyCode::Char('y'), KeyModifiers::META);
        press(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        assert!(app.msg.is_none(), "{:?}", app.msg);

        // Issue b has no tab to type in, so there's nothing to suggest.
        press(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(app.selected.as_deref(), Some("b"));
        press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(app.msg.is_none(), "{:?}", app.msg);

        app.focus = Focus::Terminal;
        press(&mut app, KeyCode::Char('y'), KeyModifiers::NONE);
        assert!(app.msg.as_ref().is_none_or(|m| !m.0.starts_with("Sidebar")));
    }

    #[test]
    fn change_notifications_follow_status_not_hook_noise() {
        use crate::query::{diff, fingerprints};
        let (mut app, _vault) = click_app(&["A-1"]);
        let base = fingerprints(&app);
        assert!(base.listed.is_empty(), "an issue without tabs isn't listed");
        with_tabs(&mut app, "A-1", vec![unstarted_tab()], 0);
        let opened = fingerprints(&app);
        let d = diff(&base, &opened);
        assert!(d.list, "{d:?}");
        assert_eq!(d.uris, ["salash://issue/A-1", "salash://issues", "salash://tab/t"]);
        assert!(diff(&opened, &fingerprints(&app)).uris.is_empty(), "nothing changed");

        // Looking at a tab or a hook's message doesn't notify; its status
        // does, for the tab and the issue that lists it, but not for the
        // list, which only counts tabs.
        app.rt.get_mut("A-1").unwrap().tabs[0].seen_ts = 99;
        assert_eq!(fingerprints(&app), opened);
        app.rt.get_mut("A-1").unwrap().tabs[0].status = ClaudeStatus::Working;
        let d = diff(&opened, &fingerprints(&app));
        assert_eq!((d.uris.as_slice(), d.list), (["salash://issue/A-1", "salash://tab/t"].map(String::from).as_slice(), false));

        // With two tabs, one changing while the rolled-up status stays the same.
        let mut second = unstarted_tab();
        second.st.id = "u".into();
        app.rt.get_mut("A-1").unwrap().tabs.push(second);
        let two = fingerprints(&app);
        app.rt.get_mut("A-1").unwrap().tabs[1].status = ClaudeStatus::Done;
        assert_eq!(app.rt["A-1"].rollup(), None);
        assert_eq!(diff(&two, &fingerprints(&app)).uris, ["salash://issue/A-1", "salash://tab/u"]);

        // Another tab made active: the issue and both tabs.
        let done = fingerprints(&app);
        app.rt.get_mut("A-1").unwrap().active = 1;
        assert_eq!(diff(&done, &fingerprints(&app)).uris, ["salash://issue/A-1", "salash://tab/t", "salash://tab/u"]);

        // A worktree is the issue's and the list's.
        let active = fingerprints(&app);
        app.rt.get_mut("A-1").unwrap().worktree = Some("/wt/A-1".into());
        assert_eq!(diff(&active, &fingerprints(&app)).uris, ["salash://issue/A-1", "salash://issues"]);

        // A title change is the issue's and the list's, not its tabs'.
        let working = fingerprints(&app);
        app.notes[0].title = "renamed".into();
        assert_eq!(diff(&working, &fingerprints(&app)).uris, ["salash://issue/A-1", "salash://issues"]);

        // The last tab closing: its URI goes and the issue leaves the list.
        let renamed = fingerprints(&app);
        app.rt.get_mut("A-1").unwrap().tabs.clear();
        let d = diff(&renamed, &fingerprints(&app));
        assert!(d.list && d.uris.contains(&"salash://tab/t".to_string()), "{d:?}");
    }

    // ---------- #28: Done and Archive stop their tabs ----------

    const S1: &str = "11111111-1111-4111-8111-111111111111";
    const S2: &str = "22222222-2222-4222-8222-222222222222";

    /// A tab of `kind` with a fresh id, so its status file and snapshot are its own.
    fn fresh(kind: TabKind, session: Option<&str>, cwd: Option<&str>) -> Tab {
        let mut t = unstarted_tab();
        t.st.id = uuid::Uuid::new_v4().to_string();
        t.st.kind = kind;
        t.st.claude_session = session.map(Into::into);
        t.st.cwd = cwd.map(PathBuf::from);
        t
    }

    /// A tab of `kind` whose pane runs `sleep` with no command in front of it.
    fn live(kind: TabKind, dir: &Path) -> Tab {
        let mut t = running_tab(dir);
        t.st.id = uuid::Uuid::new_v4().to_string();
        t.st.kind = kind;
        t
    }

    fn key(app: &mut App, c: char) {
        app.on_sidebar_key(KeyEvent::from(KeyCode::Char(c)));
    }

    fn answer(app: &mut App, c: char) {
        app.handle(Event::Key(KeyEvent::from(KeyCode::Char(c))));
    }

    fn note_of(app: &App, id: &str) -> Note {
        app.notes.iter().find(|n| n.id == id).cloned().expect("note")
    }

    fn note_text(app: &App, id: &str) -> String {
        std::fs::read_to_string(&note_of(app, id).path).unwrap()
    }

    fn flashed(app: &App) -> String {
        app.msg.as_ref().map(|m| m.0.clone()).unwrap_or_default()
    }

    #[test]
    fn done_stops_idle_tabs_and_keeps_resume_commands_in_the_note() {
        let (mut app, vault) = click_app(&["a", "b"]);
        app.cfg.claude_command = vec!["claude".into()];
        let suspended = fresh(TabKind::Claude, Some(S1), Some("/work/my repo"));
        let no_session = fresh(TabKind::Claude, None, None);
        let shell = live(TabKind::Shell, &vault.0);
        let shell_pid = shell.pane.as_ref().unwrap().pid().unwrap();
        let ids: Vec<String> = [&suspended, &no_session, &shell].iter().map(|t| t.st.id.clone()).collect();
        with_tabs(&mut app, "a", vec![suspended, no_session, shell], 2);
        let closed_id = uuid::Uuid::new_v4().to_string();
        {
            let rt = app.rt.get_mut("a").unwrap();
            rt.worktree = Some(PathBuf::from("/wt/a"));
            let mut st = fresh(TabKind::Claude, Some(S2), None).st;
            st.id = closed_id.clone();
            st.name = "old".into();
            rt.closed.push(st);
            // The same session closed twice is written once.
            rt.closed.push(fresh(TabKind::Claude, Some(S1), None).st);
        }
        state::write_snapshot(&ids[0], b"screen");
        state::write_snapshot(&closed_id, b"screen");
        app.select_id("a".into());

        key(&mut app, 'd');
        assert!(app.mode == Mode::Normal, "idle tabs need no confirmation");
        assert_eq!(note_of(&app, "a").stage, Stage::Done);
        assert_eq!(flashed(&app), "a → Done · 3 tabs stopped");
        assert!(app.rt["a"].tabs.is_empty() && app.rt["a"].closed.is_empty());
        let saved = &app.state.issues["a"];
        assert!(saved.tabs.is_empty() && saved.closed.is_empty(), "persisted, so they don't come back");
        assert_eq!(saved.worktree.as_deref(), Some(Path::new("/wt/a")), "the worktree stays");
        assert!(!state::snapshot_path(&ids[0]).exists() && !state::snapshot_path(&closed_id).exists());
        // The shell is gone: kill(0) on a reaped pid fails.
        assert_ne!(unsafe { libc::kill(shell_pid as i32, 0) }, 0, "shell still running");

        let text = note_text(&app, "a");
        assert!(text.contains(&format!(" · stopped · claude · `cd '/work/my repo' && claude --resume {S1}`")), "{text}");
        assert!(text.contains(&format!(" · stopped · old · `cd /wt/a && claude --resume {S2}`")), "{text}");
        assert_eq!(text.matches("--resume").count(), 2, "one line per session, none without one: {text}");
    }

    #[test]
    fn busy_tabs_are_named_and_no_leaves_everything_as_it_was() {
        let (mut app, vault) = click_app(&["a"]);
        let mut claude = live(TabKind::Claude, &vault.0);
        claude.status = ClaudeStatus::Working;
        let editor = live(TabKind::Notes, &vault.0);
        let idle = fresh(TabKind::Claude, None, None);
        with_tabs(&mut app, "a", vec![claude, editor, idle], 0);
        app.select_id("a".into());

        key(&mut app, 'd');
        let Mode::Confirm(ConfirmKind::Retire { issue, stage, busy }) = app.mode.clone() else { panic!("no confirm") };
        assert_eq!((issue.as_str(), stage), ("a", Stage::Done));
        assert_eq!(busy, ["Claude: working", "note editor"]);
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
        term.draw(|f| crate::ui::draw(&mut app, f)).unwrap();
        let screen: String = term.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert!(screen.contains("Stop 2 busy tabs (Claude: working, note"), "{screen}");

        answer(&mut app, 'n');
        assert!(app.mode == Mode::Normal);
        assert_eq!(note_of(&app, "a").stage, Stage::Active);
        assert_eq!(app.rt["a"].tabs.len(), 3);
        assert!(app.rt["a"].tabs[0].running());

        key(&mut app, 'a');
        assert!(matches!(app.mode, Mode::Confirm(ConfirmKind::Retire { stage: Stage::Archive, .. })));
        answer(&mut app, 'y');
        assert_eq!(note_of(&app, "a").stage, Stage::Archive);
        assert!(app.rt["a"].tabs.is_empty());
        assert_eq!(flashed(&app), "a → Archive · 3 tabs stopped");
    }

    #[test]
    fn a_failed_move_stops_nothing() {
        let (mut app, vault) = click_app(&["a"]);
        with_tabs(&mut app, "a", vec![fresh(TabKind::Claude, Some(S1), None)], 0);
        app.select_id("a".into());
        let path = note_of(&app, "a").path;
        let done = path.parent().unwrap().parent().unwrap().join(Stage::Done.dir());
        assert!(done.starts_with(&vault.0));
        std::fs::create_dir_all(&done).unwrap();
        std::fs::write(done.join(path.file_name().unwrap()), "taken").unwrap();

        key(&mut app, 'd');
        assert!(flashed(&app).starts_with("Move failed"), "{}", flashed(&app));
        assert_eq!(app.rt["a"].tabs.len(), 1);
        assert!(!note_text(&app, "a").contains("stopped"));
    }

    #[test]
    fn moving_back_brings_no_tabs_and_done_to_archive_has_none_to_stop() {
        let (mut app, _vault) = click_app(&["a"]);
        with_tabs(&mut app, "a", vec![fresh(TabKind::Claude, Some(S1), None)], 0);
        app.select_id("a".into());
        key(&mut app, 'd');
        app.set_view(Stage::Done);
        app.select_id("a".into());
        key(&mut app, 'a');
        assert_eq!(flashed(&app), "a → Archive", "no tabs left to stop");
        app.set_view(Stage::Archive);
        app.select_id("a".into());
        key(&mut app, 'u');
        assert_eq!(note_of(&app, "a").stage, Stage::Active);
        assert!(app.rt["a"].tabs.is_empty() && app.rt["a"].closed.is_empty());
    }

    #[test]
    fn a_move_made_outside_salash_stops_idle_tabs_and_leaves_busy_ones() {
        let (mut app, vault) = click_app(&["a", "b"]);
        let tabs = vec![fresh(TabKind::Claude, Some(S1), None), live(TabKind::Shell, &vault.0), live(TabKind::Notes, &vault.0)];
        with_tabs(&mut app, "a", tabs, 0);
        // "b" was never loaded: its tabs are only saved, as after a restart.
        app.state.issues.insert("b".into(), IssueState { tabs: vec![fresh(TabKind::Shell, None, None).st], ..Default::default() });
        for id in ["a", "b"] {
            app.vault.move_to(&note_of(&app, id), Stage::Done).unwrap();
        }

        app.rescan();
        let rt = &app.rt["a"];
        assert_eq!(rt.tabs.len(), 1, "only the editor is left");
        assert_eq!(rt.tabs[0].st.kind, TabKind::Notes);
        // The editor would save over a line written under it, so the session waits in closed.
        assert!(!note_text(&app, "a").contains("--resume"));
        assert_eq!(rt.closed.iter().map(|st| st.claude_session.as_deref()).collect::<Vec<_>>(), [Some(S1)]);
        assert!(app.state.issues["b"].tabs.is_empty(), "saved leftovers go too");
        assert_eq!(app.busy_left["a"], ["note editor"]);
        let m = flashed(&app);
        assert!(m.contains("a is in Done: left 1 busy tab running (note editor), stopped once idle") && m.contains("b is in Done: 1 tab stopped"), "{m}");

        // Busy tabs are retried every rescan, but said only once.
        app.msg = None;
        app.rescan();
        assert!(app.msg.is_none(), "flashed again: {}", flashed(&app));
        assert_eq!(app.rt["a"].tabs.len(), 1);

        // Once the editor quits, the next rescan stops it and writes the line.
        let p = app.rt.get_mut("a").unwrap().tabs[0].pane.as_mut().unwrap();
        p.kill();
        let start = Instant::now();
        while !p.poll_exit() {
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        app.rescan();
        assert!(app.rt["a"].tabs.is_empty() && app.rt["a"].closed.is_empty());
        assert_eq!(flashed(&app), "a is in Done: 1 tab stopped");
        assert_eq!(note_text(&app, "a").matches(&format!("--resume {S1}")).count(), 1);
        assert!(app.busy_left.is_empty());
    }

    #[test]
    fn a_note_that_cant_be_written_keeps_the_sessions() {
        use std::os::unix::fs::PermissionsExt;
        let (mut app, vault) = click_app(&["a"]);
        let mut claude = live(TabKind::Claude, &vault.0);
        claude.st.claude_session = Some(S1.into());
        let tab_id = claude.st.id.clone();
        with_tabs(&mut app, "a", vec![claude], 0);
        app.select_id("a".into());
        let path = note_of(&app, "a").path;
        // The move renames the file and rewrites it first, so lock it after.
        app.vault.move_to(&note_of(&app, "a"), Stage::Done).unwrap();
        app.notes = app.vault.scan();
        let path = path.parent().unwrap().parent().unwrap().join(Stage::Done.dir()).join(path.file_name().unwrap());
        let mode = |m| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(m)).unwrap();
        mode(0o444);

        app.rescan();
        let rt = &app.rt["a"];
        assert!(rt.tabs.is_empty(), "stopped all the same");
        assert_eq!(rt.closed.len(), 1, "kept for R");
        assert_eq!(rt.closed[0].id, tab_id);
        assert_eq!(app.state.issues["a"].closed.len(), 1, "persisted");
        assert!(state::snapshot_path(&tab_id).exists(), "its screen is kept");
        assert!(app.msg_error && flashed(&app).contains("not written"), "{}", flashed(&app));

        // Retried every rescan, said once.
        app.msg = None;
        app.rescan();
        assert!(app.msg.is_none(), "flashed again: {}", flashed(&app));

        mode(0o644);
        app.rescan();
        assert!(app.rt["a"].closed.is_empty());
        assert!(note_text(&app, "a").contains(&format!("--resume {S1}")));
        assert!(!state::snapshot_path(&tab_id).exists());
    }

    #[test]
    fn a_rescan_leaves_a_tab_waiting_for_its_worktree_focused() {
        let (mut app, _vault) = click_app(&["a"]);
        let t = fresh(TabKind::Claude, None, None);
        let (_tx, rx) = mpsc::channel();
        let waiting = vec![t.st.id.clone()];
        app.rt.insert("a".into(), IssueRt { tabs: vec![t], creating: Some(WorktreeJob { rx, waiting, fallback: None }), ..Default::default() });
        app.select_id("a".into());
        app.focus = Focus::Terminal;
        app.rescan();
        assert_eq!(app.focus, Focus::Terminal);
    }

    #[test]
    fn nothing_new_opens_on_a_finished_issue() {
        let (mut app, _vault) = click_app(&["a"]);
        app.vault.move_to(&note_of(&app, "a"), Stage::Done).unwrap();
        app.rescan();
        app.set_view(Stage::Done);
        app.select_id("a".into());
        for c in ['c', 'C', 't', 'e', 'I', 'R'] {
            app.msg = None;
            key(&mut app, c);
            assert_eq!(flashed(&app), "Move to Active first (u)", "key {c}");
            assert!(app.mode == Mode::Normal, "key {c}");
        }
        app.open_issue();
        assert!(app.rt["a"].tabs.is_empty());
        let req = Request { issue: "a".into(), kind: TabKind::Shell, cwd: None, name: None, mode: None, prompt: None };
        let reply = app.open_requested(req);
        assert!(!reply.ok && reply.error.unwrap().contains("Move to Active first"));
        // The note and the issue's page still open.
        key(&mut app, 'o');
        assert!(matches!(app.outbox.last(), Some(ServerMsg::Open(u)) if u.starts_with("obsidian://")));
    }

    #[test]
    fn a_session_id_is_read_from_what_was_pasted() {
        let id = S1;
        for ok in [
            id.to_string(),
            format!("  {id}\t"),
            id.to_uppercase(),
            format!("claude --resume {id}"),
            format!("`cd /x && claude --resume {id}`"),
            format!("- 2026-11-19 10:00 · stopped · claude · `cd /x && claude --resume {id}`"),
        ] {
            assert_eq!(parse_session_id(&ok).map(|(s, _)| s).as_deref(), Some(id), "{ok}");
        }
        for bad in ["", "   ", "hello", "11111111111141118111111111111111", "claude --resume", &format!("{id} {id}"), "{11111111-1111-4111-8111-111111111111}"] {
            assert_eq!(parse_session_id(bad), None, "{bad}");
        }
        let dir = |input: &str| parse_session_id(input).and_then(|(_, d)| d);
        assert_eq!(dir(S1), None);
        assert_eq!(dir(&format!("claude --resume {S1}")), None);
        assert_eq!(dir(&format!("- x · stopped · claude · `cd /wt/a && claude --resume {S1}`")), Some(PathBuf::from("/wt/a")));
        assert_eq!(dir(&format!("`cd '/my dir/it'\\''s' && claude --resume {S1}`")), Some(PathBuf::from("/my dir/it's")));
    }

    fn resume(app: &mut App, input: &str) {
        app.mode = Mode::Prompt(PromptKind::ResumeId, String::new());
        app.on_paste(input.into());
        app.handle(Event::Key(KeyEvent::from(KeyCode::Enter)));
    }

    #[test]
    fn resuming_an_open_session_shows_its_tab() {
        let (mut app, _vault) = click_app(&["a"]);
        with_tabs(&mut app, "a", vec![fresh(TabKind::Shell, None, None), fresh(TabKind::Claude, Some(S1), None)], 0);
        app.select_id("a".into());
        resume(&mut app, &format!("claude --resume {S1}"));
        assert_eq!(app.rt["a"].tabs.len(), 2, "no second tab");
        assert_eq!(app.rt["a"].active, 1);
        assert_eq!(app.focus, Focus::Terminal);
        assert!(app.rt["a"].tabs[1].pane.is_none(), "shown, not started");
    }

    #[test]
    fn resuming_a_closed_session_brings_its_tab_back() {
        let (mut app, _vault) = click_app(&["a"]);
        with_tabs(&mut app, "a", vec![], 0);
        let st = fresh(TabKind::Claude, Some(S1), Some("/nonexistent/dir")).st;
        app.rt.get_mut("a").unwrap().closed.push(st.clone());
        app.select_id("a".into());
        resume(&mut app, S1);
        let rt = &app.rt["a"];
        assert!(rt.closed.is_empty());
        assert_eq!(rt.tabs.len(), 1);
        assert_eq!(rt.tabs[0].st.id, st.id);
    }

    #[test]
    fn resuming_by_id_opens_a_claude_tab_and_starts_work() {
        let (mut app, vault) = click_app(&["a"]);
        // "claude" records its arguments.
        let argv = vault.0.join("argv");
        app.cfg.claude_command = vec!["sh".into(), "-c".into(), format!("echo \"$@\" > '{}'", argv.display()), "sh".into()];
        app.vault.move_to(&note_of(&app, "a"), Stage::Todo).unwrap();
        app.rescan();
        app.set_view(Stage::Todo);
        app.select_id("a".into());

        resume(&mut app, "not an id");
        assert!(flashed(&app).starts_with("Not a session ID"));
        assert!(app.mode == Mode::Prompt(PromptKind::ResumeId, "not an id".into()), "the prompt stays to fix it");
        assert!(app.rt["a"].tabs.is_empty());
        app.mode = Mode::Normal;

        resume(&mut app, S2);
        assert_eq!(note_of(&app, "a").stage, Stage::Active, "like c, it starts work");
        let t = &app.rt["a"].tabs[0];
        assert_eq!(t.st.kind, TabKind::Claude);
        assert_eq!(t.st.claude_session, None, "set by its first hook, which writes the Sessions line");
        assert_eq!(t.st.cwd.as_deref(), Some(app.default_dir().as_path()), "where c starts, not home");
        assert_eq!(app.focus, Focus::Terminal);
        let start = Instant::now();
        while !std::fs::read_to_string(&argv).is_ok_and(|a| a.trim_end().ends_with(&format!("--resume {S2}"))) {
            assert!(start.elapsed() < Duration::from_secs(5), "{:?}", std::fs::read_to_string(&argv));
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn scratch_can_resume_a_session() {
        let (mut app, _vault) = click_app(&["a"]);
        app.select_id(SCRATCH.into());
        key(&mut app, 'I');
        assert!(app.mode == Mode::Prompt(PromptKind::ResumeId, String::new()));
        app.mode = Mode::Normal;
        resume(&mut app, S1);
        assert_eq!(app.rt[SCRATCH].tabs.len(), 1);
        assert_eq!(app.rt[SCRATCH].tabs[0].st.kind, TabKind::Claude);
    }

    #[test]
    fn a_shell_word_is_quoted_only_when_it_must_be() {
        assert_eq!(shell_word("/a/b-c_d.e"), "/a/b-c_d.e");
        assert_eq!(shell_word("/a b"), "'/a b'");
        assert_eq!(shell_word("/it's"), r"'/it'\''s'");
        // A newline would end the note's line; the shell reads the escape back.
        assert_eq!(shell_word("/a\nb\t'c\\"), r"$'/a\x0ab\x09\'c\\'");
        assert!(!shell_word("/a\nb").contains('\n'));
    }

    #[test]
    fn the_note_line_uses_the_configured_claude_command() {
        let (mut app, _vault) = click_app(&["a"]);
        app.cfg.claude_command = vec!["npx".into(), "claude code".into()];
        with_tabs(&mut app, "a", vec![fresh(TabKind::Claude, Some(S1), Some("/w"))], 0);
        app.select_id("a".into());
        key(&mut app, 'd');
        assert!(note_text(&app, "a").contains(&format!("`cd /w && npx 'claude code' --resume {S1}`")), "{}", note_text(&app, "a"));
    }

    #[test]
    fn a_busy_shell_is_named_by_its_command() {
        let (mut app, vault) = click_app(&["a"]);
        let argv = ["sh".to_string(), "-i".to_string()];
        let env = [("PS1".to_string(), "prompt> ".to_string()), ("ENV".to_string(), "/dev/null".to_string())];
        let mut t = fresh(TabKind::Shell, None, None);
        t.pane = Some(Pane::spawn(&argv, &vault.0, &env, 10, 60).unwrap());
        let wait = |t: &Tab, f: &dyn Fn(&Tab) -> bool| {
            let start = Instant::now();
            while !f(t) {
                assert!(start.elapsed() < Duration::from_secs(5));
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait(&t, &|t| t.pane.as_ref().unwrap().snapshot(&[]).windows(7).any(|w| w == b"prompt>"));
        t.pane.as_mut().unwrap().write(b"sleep 30\r");
        wait(&t, &|t| t.busy_label().is_some_and(|b| b != "shell: running command"));
        with_tabs(&mut app, "a", vec![t], 0);
        app.select_id("a".into());
        key(&mut app, 'd');
        let Mode::Confirm(ConfirmKind::Retire { busy, .. }) = app.mode.clone() else { panic!("no confirm") };
        assert_eq!(busy, ["shell: sleep"]);
    }

    #[test]
    fn a_closed_session_whose_transcript_is_gone_leaves_no_line() {
        let (mut app, vault) = click_app(&["a"]);
        let t = fresh(TabKind::Claude, Some(S1), None);
        let sf = status::StatusFile {
            status: ClaudeStatus::Idle,
            session_id: Some(S1.into()),
            transcript: Some(vault.0.join("gone.jsonl").to_string_lossy().into_owned()),
            message: None,
            event: "Stop".into(),
            ts: 0,
        };
        std::fs::create_dir_all(status::status_dir()).unwrap();
        std::fs::write(status::status_path(&t.st.id), serde_json::to_vec(&sf).unwrap()).unwrap();
        with_tabs(&mut app, "a", vec![t], 0);
        app.select_id("a".into());
        assert!(!app.close_tab_at("a", 0), "nothing to reopen");
        assert!(app.rt["a"].closed.is_empty());
        key(&mut app, 'd');
        assert!(!note_text(&app, "a").contains("--resume"));
    }

    #[test]
    fn a_session_open_in_another_issue_is_shown_there() {
        let (mut app, _vault) = click_app(&["a", "b"]);
        with_tabs(&mut app, "a", vec![], 0);
        app.state.issues.insert("b".into(), IssueState { tabs: vec![fresh(TabKind::Shell, None, None).st, fresh(TabKind::Claude, Some(S1), None).st], ..Default::default() });
        app.select_id("a".into());
        resume(&mut app, S1);
        assert_eq!(app.selected.as_deref(), Some("b"));
        assert_eq!(app.rt["b"].active, 1);
        assert!(app.rt["a"].tabs.is_empty(), "no second process on the session");
        assert_eq!(flashed(&app), "That session is open in b");
    }

    #[test]
    fn resuming_twice_before_the_first_hook_opens_one_tab() {
        let (mut app, vault) = click_app(&["a"]);
        app.cfg.claude_command = vec!["sh".into(), "-c".into(), "sleep 30".into(), "sh".into()];
        app.select_id("a".into());
        let line = format!("`cd {} && claude --resume {S1}`", shell_word(&vault.0.to_string_lossy()));
        resume(&mut app, &line);
        assert_eq!(app.rt["a"].tabs[0].st.cwd.as_deref(), Some(vault.0.as_path()), "where the pasted cd goes");
        resume(&mut app, S1);
        assert_eq!(app.rt["a"].tabs.len(), 1);
        assert_eq!(flashed(&app), "That session is open in this tab");
    }
}
