use crate::config::Config;
use crate::keys;
use crate::providers::{self, SyncReport};
use crate::state::{IssueState, State, TabKind, TabState};
use crate::status::{self, ClaudeStatus};
use crate::term::Pane;
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
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How long after Esc a Claude tab may stay silent before it counts as interrupted.
const INTERRUPT_SETTLE_MS: i64 = 1500;

pub struct Tab {
    pub st: TabState,
    pub pane: Option<Pane>,
    pub status: ClaudeStatus,
    pub status_ts: i64,
    pub seen_ts: i64,
    /// When Esc was sent while Claude was busy; cleared once that is resolved.
    pub esc_ts: Option<i64>,
}

impl Tab {
    fn new(st: TabState) -> Tab {
        Tab { st, pane: None, status: ClaudeStatus::Exited, status_ts: 0, seen_ts: 0, esc_ts: None }
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

#[derive(Default)]
pub struct IssueRt {
    pub tabs: Vec<Tab>,
    pub active: usize,
    pub worktree: Option<PathBuf>,
}

impl IssueRt {
    pub fn rollup(&self) -> Option<ClaudeStatus> {
        self.tabs
            .iter()
            .filter(|t| t.st.kind == TabKind::Claude && t.pane.is_some())
            .map(|t| t.shown_status())
            .max_by_key(|s| s.urgency())
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ConfirmKind {
    Quit,
    CloseTab,
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
    pub state: State,
    pub focus: Focus,
    pub mode: Mode,
    pub prefix: (KeyCode, KeyModifiers),
    pub msg: Option<(String, Instant)>,
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
    pub item_hits: Vec<(Rect, String)>,
    last_scan: Instant,
    last_status: Instant,
    last_spin: Instant,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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
            state,
            focus: Focus::Sidebar,
            mode: Mode::Normal,
            prefix,
            msg: None,
            sync_rx: None,
            syncing: false,
            spinner: 0,
            quit: false,
            hooks_settings,
            bell_pending: false,
            term_rect: Rect::default(),
            sidebar_rect: Rect::default(),
            tab_hits: vec![],
            item_hits: vec![],
            last_scan: Instant::now(),
            last_status: Instant::now(),
            last_spin: Instant::now(),
        };
        if let Some(n) = app.selected_note() {
            app.view = n.stage;
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
        app.start_sync();
        Ok(app)
    }

    pub fn flash(&mut self, s: String) {
        self.msg = Some((s, Instant::now()));
    }

    pub fn prefix_label(&self) -> String {
        match self.prefix.0 {
            KeyCode::Char(' ') => "C-Space".into(),
            KeyCode::Char(c) => format!("C-{c}"),
            _ => "prefix".into(),
        }
    }

    // ---------- issues ----------

    pub fn visible(&self) -> Vec<&Note> {
        self.notes.iter().filter(|n| n.stage == self.view).collect()
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
        match self.vault.create(NewNote { id: &id, title: &title, source: "local", url: None, repo: None }) {
            Ok((n, _)) => {
                self.rescan();
                self.view = Stage::Active;
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
                },
            );
            self.state.save();
        }
    }

    /// Working directory for an issue, creating its worktree on first use.
    fn workdir(&mut self, note: &Note) -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        if let Some(wt) = self.rt_for(&note.id).worktree.clone() {
            if wt.exists() {
                return wt;
            }
        }
        let repo = note
            .repo_path
            .clone()
            .or_else(|| self.cfg.repo_path_for(&note.source, note.repo.as_deref()));
        let Some(repo) = repo else { return home };
        match worktree::ensure(&repo, &self.cfg.worktrees_dir, &note.stem()) {
            Ok(p) => {
                self.rt_for(&note.id).worktree = Some(p.clone());
                self.persist(&note.id);
                p
            }
            Err(e) => {
                self.flash(format!("Worktree failed, using repo: {e:#}"));
                repo
            }
        }
    }

    fn spawn_pane(&mut self, id: &str, idx: usize) {
        let Some(note) = self.notes.iter().find(|n| n.id == id).cloned() else { return };
        let cwd = self.workdir(&note);
        let (rows, cols) = (self.term_rect.height.max(5), self.term_rect.width.max(20));
        let rt = self.rt_for(id);
        let Some(tab) = rt.tabs.get(idx) else { return };
        let st = tab.st.clone();
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
        let env = vec![
            ("OFFICE_TAB".to_string(), st.id.clone()),
            ("OFFICE_ISSUE".to_string(), note.id.clone()),
            ("OFFICE_NOTE".to_string(), note.path.to_string_lossy().into_owned()),
        ];
        status::clear_status(&st.id);
        let result = Pane::spawn(&argv, &cwd, &env, rows, cols);
        let rt = self.rt_for(id);
        let tab = &mut rt.tabs[idx];
        match result {
            Ok(p) => {
                tab.pane = Some(p);
                tab.status = if st.kind == TabKind::Claude { ClaudeStatus::Starting } else { ClaudeStatus::Idle };
                tab.status_ts = now_ms();
            }
            Err(e) => self.flash(format!("Spawn failed: {e:#}")),
        }
    }

    fn new_tab(&mut self, kind: TabKind) {
        let Some(id) = self.selected.clone() else {
            self.flash("Select an issue first".into());
            return;
        };
        let name = match kind {
            TabKind::Claude => "claude",
            TabKind::Shell => "shell",
            TabKind::Notes => "notes",
        };
        let rt = self.rt_for(&id);
        rt.tabs.push(Tab::new(TabState {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            kind,
            custom_name: false,
            claude_session: None,
        }));
        rt.active = rt.tabs.len() - 1;
        let idx = rt.active;
        self.persist(&id);
        self.spawn_pane(&id, idx);
        self.focus = Focus::Terminal;
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
        if let Some(p) = t.pane.as_mut() {
            p.kill();
        }
        status::clear_status(&t.st.id);
        rt.active = rt.active.min(rt.tabs.len().saturating_sub(1));
        if rt.tabs.is_empty() {
            self.focus = Focus::Sidebar;
        }
        self.persist(&id);
    }

    fn active_pane_mut(&mut self) -> Option<&mut Pane> {
        let id = self.selected.clone()?;
        let rt = self.rt.get_mut(&id)?;
        rt.tabs.get_mut(rt.active)?.pane.as_mut()
    }

    // ---------- periodic work ----------

    pub fn tick(&mut self) {
        self.poll_sync();
        if self.last_spin.elapsed() >= Duration::from_millis(120) {
            self.spinner = self.spinner.wrapping_add(1);
            self.last_spin = Instant::now();
        }
        if self.last_scan.elapsed() >= Duration::from_secs(3) {
            // Picks up notes moved/renamed/created in Obsidian.
            self.rescan();
        }
        if let Some((_, t)) = &self.msg {
            if t.elapsed() > Duration::from_secs(6) {
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
        let selected = self.selected.clone();
        let mut new_sessions: Vec<(String, String, String)> = vec![];
        let mut renamed: Vec<String> = vec![];
        for (issue, rt) in self.rt.iter_mut() {
            let active = rt.active;
            for (i, t) in rt.tabs.iter_mut().enumerate() {
                if t.pane.is_none() {
                    continue;
                }
                let visible = selected.as_deref() == Some(issue.as_str()) && i == active;
                let before = t.shown_status();
                let (exited, title, bell) = {
                    let p = t.pane.as_mut().unwrap();
                    (p.poll_exit(), p.title(), p.take_bell())
                };
                if exited {
                    t.status = ClaudeStatus::Exited;
                } else if t.st.kind == TabKind::Claude {
                    if let Some(sf) = status::read_status(&t.st.id) {
                        if sf.ts > t.status_ts || sf.status != t.status {
                            t.status = sf.status;
                            t.status_ts = sf.ts;
                        }
                        if let Some(sid) = sf.session_id {
                            if t.st.claude_session.as_deref() != Some(&sid) {
                                t.st.claude_session = Some(sid.clone());
                                new_sessions.push((issue.clone(), sid, t.st.name.clone()));
                            }
                        }
                    }
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

    fn on_mouse(&mut self, m: MouseEvent) {
        let pos = Position::new(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, i)) = self.tab_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                    self.switch_tab(i);
                    self.open_issue();
                } else if let Some((_, id)) = self.item_hits.iter().find(|(r, _)| r.contains(pos)).cloned() {
                    self.select_id(id);
                    self.focus = Focus::Sidebar;
                } else if self.term_rect.contains(pos) {
                    if self.active_pane_mut().is_some() {
                        self.focus = Focus::Terminal;
                    }
                } else if self.sidebar_rect.contains(pos) {
                    self.focus = Focus::Sidebar;
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = m.kind == MouseEventKind::ScrollUp;
                if self.term_rect.contains(pos) {
                    if let Some(p) = self.active_pane_mut() {
                        p.scroll_by(if up { 3 } else { -3 });
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
                    }
                }
            }
            Mode::Prompt(kind, mut buf) => match k.code {
                KeyCode::Esc => self.mode = Mode::Normal,
                KeyCode::Enter => {
                    self.mode = Mode::Normal;
                    match kind {
                        PromptKind::NewIssue => self.new_local_issue(buf),
                        PromptKind::RenameTab => self.rename_tab(buf),
                    }
                }
                KeyCode::Backspace => {
                    buf.pop();
                    self.mode = Mode::Prompt(kind, buf);
                }
                KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                    buf.push(c);
                    self.mode = Mode::Prompt(kind, buf);
                }
                _ => {}
            },
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
            KeyCode::Char('1') => self.set_view(Stage::Active),
            KeyCode::Char('2') => self.set_view(Stage::Done),
            KeyCode::Char('3') => self.set_view(Stage::Archive),
            KeyCode::Char('v') | KeyCode::Char('h') | KeyCode::Left => {
                let next = match self.view {
                    Stage::Active => Stage::Done,
                    Stage::Done => Stage::Archive,
                    Stage::Archive => Stage::Active,
                };
                self.set_view(next);
            }
            KeyCode::Tab | KeyCode::Esc => {
                if self.active_pane_mut().is_some() {
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
                let has_pane = self.active_pane_mut().is_some();
                self.focus = match self.focus {
                    Focus::Sidebar if has_pane => Focus::Terminal,
                    _ => Focus::Sidebar,
                };
            }
            KeyCode::Char(c @ '1'..='9') => {
                self.switch_tab(c as usize - '1' as usize);
                self.open_issue();
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.cycle_tab(1);
                self.open_issue();
            }
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('p') => {
                self.cycle_tab(-1);
                self.open_issue();
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
                let h = self.term_rect.height as isize / 2;
                if let Some(p) = self.active_pane_mut() {
                    p.scroll_by(h.max(1));
                }
            }
            KeyCode::Char(']') | KeyCode::PageDown => {
                let h = self.term_rect.height as isize / 2;
                if let Some(p) = self.active_pane_mut() {
                    p.scroll_by(-h.max(1));
                }
            }
            _ => {
                self.on_command(k);
            }
        }
    }

    /// When jumping issues from the terminal, stay in the terminal if the
    /// target issue already has a running tab.
    fn after_issue_switch(&mut self) {
        if self.focus == Focus::Terminal && self.active_pane_mut().is_none() {
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
}

/// Main loop.
pub fn run(app: &mut App, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    while !app.quit {
        app.tick();
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
        };
        Tab { status, status_ts, esc_ts, ..Tab::new(st) }
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
}
