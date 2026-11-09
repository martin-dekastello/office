use crate::app::{App, ConfirmKind, Focus, IssueRt, Mode, PromptKind, Tab, SCRATCH};
use crate::complete::Completion;
use crate::state::{SortMode, TabKind};
use crate::status::ClaudeStatus;
use crate::term::Snapshot;
use crate::theme::{self, Theme};
use crate::vault::Stage;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use tui_term::widget::{Cursor, PseudoTerminal};

const SIDEBAR_W: u16 = 34;
const MAX_MATCHES: usize = 8;
const SPIN: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];

fn colors() -> &'static Theme {
    theme::get()
}

pub fn status_glyph(s: ClaudeStatus, spin: usize) -> (&'static str, Style) {
    match s {
        ClaudeStatus::Working => (SPIN[spin % SPIN.len()], Style::new().fg(colors().orange).add_modifier(Modifier::BOLD)),
        ClaudeStatus::Attention => ("●", Style::new().fg(colors().amber).add_modifier(Modifier::BOLD)),
        ClaudeStatus::Done => ("●", Style::new().fg(colors().green)),
        ClaudeStatus::Idle => ("○", Style::new().fg(colors().fg_dim)),
        ClaudeStatus::Starting => ("◌", Style::new().fg(colors().fg_dim)),
        ClaudeStatus::Exited => ("✕", Style::new().fg(colors().red)),
    }
}

fn tab_glyph(t: &Tab, rt: Option<&IssueRt>, spin: usize) -> (&'static str, Style) {
    if rt.is_some_and(|r| r.awaits_worktree(t)) {
        return (SPIN[spin % SPIN.len()], Style::new().fg(colors().fg_dim));
    }
    if t.pane.is_none() {
        return ("–", Style::new().fg(colors().fg_dim));
    }
    match t.st.kind {
        TabKind::Claude => status_glyph(t.shown_status(), spin),
        _ if t.status == ClaudeStatus::Exited => ("✕", Style::new().fg(colors().red)),
        TabKind::Shell => ("$", Style::new().fg(colors().fg_dim)),
        TabKind::Notes => ("✎", Style::new().fg(colors().fg_dim)),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

pub fn draw(app: &mut App, f: &mut Frame) {
    let area = f.area();
    let [side, main] = Layout::horizontal([Constraint::Length(SIDEBAR_W), Constraint::Min(20)]).areas(area);
    let [tabbar, term, statusbar] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(main);
    app.sidebar_rect = side;
    app.term_rect = term;

    draw_sidebar(app, f, side);
    draw_tabbar(app, f, tabbar);
    draw_terminal(app, f, term);
    draw_statusbar(app, f, statusbar);

    match app.mode.clone() {
        Mode::Help => draw_help(app, f, area),
        Mode::Prompt(kind, buf) => {
            let default = crate::config::shorten(&app.default_dir());
            let title: String = match kind {
                PromptKind::NewIssue => " New local issue — title ".into(),
                PromptKind::RenameTab => " Rename tab (empty = auto) ".into(),
                PromptKind::ClaudeDir => " New Claude tab — directory (Tab completes, empty = issue default) ".into(),
                PromptKind::DefaultDir => format!(" Default directory — Tab completes, empty keeps {default} "),
            };
            draw_prompt(f, area, &title, &buf, app.completion.as_ref());
        }
        Mode::Confirm(kind) => {
            let q = match kind {
                ConfirmKind::Quit => "Quit office for everyone attached? Sessions stop; Claude tabs resume next time. (D detaches instead) [y/N]",
                ConfirmKind::CloseTab => "Close this tab and stop its process? (R reopens Claude tabs) [y/N]",
                ConfirmKind::Suspend => "Claude isn't idle, so this interrupts it. Suspend anyway? [y/N]",
            };
            draw_prompt(f, area, " Confirm ", q, None);
        }
        _ => {}
    }
}

fn draw_sidebar(app: &mut App, f: &mut Frame, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let block = Block::new()
        .borders(Borders::RIGHT)
        .border_style(Style::new().fg(if focused { colors().accent } else { colors().border }))
        .style(Style::new().bg(colors().bg_side));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [head, stages, list, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(5),
    ])
    .areas(inner);

    let attention: usize = app
        .rt
        .values()
        .flat_map(|r| r.tabs.iter())
        .filter(|t| t.pane.is_some() && t.shown_status() == ClaudeStatus::Attention)
        .count();
    let mut h = vec![Span::styled(" salash", Style::new().fg(colors().fg).add_modifier(Modifier::BOLD))];
    if app.syncing {
        h.push(Span::styled(format!("  {} sync", SPIN[app.spinner % 6]), Style::new().fg(colors().fg_dim)));
    }
    if attention > 0 {
        h.push(Span::styled(format!("  ● {attention}"), Style::new().fg(colors().amber).add_modifier(Modifier::BOLD)));
    }
    let head_used: usize = h.iter().map(Span::width).sum();
    f.render_widget(Paragraph::new(Line::from(h)), head);

    let quit_w = 6.min(head.width);
    app.quit_hit = Rect { x: head.right() - quit_w, width: quit_w, ..head };
    f.render_widget(Paragraph::new(Span::styled(" quit ", Style::new().fg(colors().fg_dim))), app.quit_hit);

    // The sort, unless it's the default; dropped when the header is full.
    if app.state.sort != SortMode::Created {
        let label = format!("↕ {} ", app.state.sort.label());
        let lw = label.chars().count() as u16;
        if head_used as u16 + lw < head.width - quit_w {
            let r = Rect { x: app.quit_hit.x - lw, width: lw, ..head };
            f.render_widget(Paragraph::new(Span::styled(label, Style::new().fg(colors().fg_dim))), r);
        }
    }

    let full: usize = Stage::ALL.iter().map(|s| s.label().len() + app.count(*s).to_string().len() + 3).sum();
    let short = full + 1 > stages.width as usize;
    let mut st = vec![Span::raw(if short { "" } else { " " })];
    let mut x = stages.x + u16::from(!short);
    app.stage_hits.clear();
    for s in Stage::ALL {
        let name = if short { s.short() } else { s.label() };
        let label = format!(" {} {} ", name, app.count(s));
        let w = label.chars().count() as u16;
        app.stage_hits.push((Rect { x, y: stages.y, width: w, height: 1 }.intersection(stages), s));
        x = x.saturating_add(w);
        let style = if s == app.view {
            Style::new().fg(colors().on_accent).bg(colors().fg).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(colors().fg_dim)
        };
        st.push(Span::styled(label, style));
    }
    f.render_widget(Paragraph::new(Line::from(st)), Rect { height: 1, ..stages });

    // Issue list, two lines per issue, with Scratch first: (id shown, title, row id).
    app.item_hits.clear();
    let scratch = app.shows_scratch().then(|| ("Scratch".to_string(), "tabs outside any issue".to_string(), SCRATCH.to_string()));
    let rows: Vec<_> = scratch
        .into_iter()
        .chain(app.visible().into_iter().map(|n| (n.id.clone(), n.title.clone(), n.id.clone())))
        .collect();
    let per_page = (list.height / 2).max(1) as usize;
    let sel_idx = app
        .selected
        .as_ref()
        .and_then(|s| rows.iter().position(|r| &r.2 == s))
        .unwrap_or(0);
    let offset = sel_idx.saturating_sub(per_page - 1);
    let w = list.width as usize;
    if rows.is_empty() {
        let hint = match app.view {
            Stage::Todo => " Nothing to do.\n n  new local issue\n S  sync GitHub/Jira",
            Stage::Active => " Nothing in progress.\n c  on a To-do issue starts it\n u  move an issue here",
            _ => " Empty.",
        };
        f.render_widget(Paragraph::new(hint).style(Style::new().fg(colors().fg_dim)), list);
    }
    for (row, (label, title, id)) in rows.iter().enumerate().skip(offset).take(per_page) {
        let y = list.y + ((row - offset) * 2) as u16;
        let r = Rect { x: list.x, y, width: list.width, height: 2 };
        let selected = app.selected.as_deref() == Some(id.as_str());
        let bg = if selected { colors().bg_sel } else { colors().bg_side };
        let rollup = app.rt.get(id).and_then(|r| r.rollup());
        let (g, gs) = match rollup {
            Some(s) => status_glyph(s, app.spinner),
            None => (" ", Style::new()),
        };
        let tabs = app.saved_tabs(id);
        let bar = if selected && focused { "▌" } else { " " };
        let id_style = match rollup {
            Some(ClaudeStatus::Attention) => Style::new().fg(colors().amber).add_modifier(Modifier::BOLD),
            _ if selected => Style::new().fg(colors().fg_strong).add_modifier(Modifier::BOLD),
            _ => Style::new().fg(colors().fg).add_modifier(Modifier::BOLD),
        };
        let count = if tabs > 0 { format!("{tabs}▤ ") } else { String::new() };
        let left_w = 3 + label.chars().count();
        let pad = w.saturating_sub(left_w + count.chars().count());
        let l1 = Line::from(vec![
            Span::styled(bar, Style::new().fg(colors().accent)),
            Span::styled(format!("{g} "), gs),
            Span::styled(truncate(label, w.saturating_sub(8)), id_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(count, Style::new().fg(colors().fg_dim)),
        ]);
        let l2 = Line::from(vec![
            Span::styled(bar, Style::new().fg(colors().accent)),
            Span::raw("  "),
            Span::styled(truncate(title, w.saturating_sub(4)), Style::new().fg(if selected { colors().fg } else { colors().fg_dim })),
        ]);
        f.render_widget(Paragraph::new(vec![l1, l2]).style(Style::new().bg(bg)), r);
        app.item_hits.push((r, id.clone()));
    }

    let p = app.prefix_label();
    let foot_text = if focused {
        vec![
            Line::from(Span::styled(" ⏎ open c claude t shell e note", Style::new().fg(colors().fg_dim))),
            Line::from(Span::styled(" o obsidian d done n new ? keys", Style::new().fg(colors().fg_dim))),
            Line::from(Span::styled(" x close tab p default dir q quit", Style::new().fg(colors().fg_dim))),
            Line::from(Span::styled(" z suspend R reopen ` scratch", Style::new().fg(colors().fg_dim))),
            Line::from(Span::styled(" D detach", Style::new().fg(colors().fg_dim))),
        ]
    } else {
        vec![
            Line::from(Span::styled(format!(" {p} s sidebar {p} j/k issue"), Style::new().fg(colors().fg_dim))),
            Line::from(Span::styled(format!(" {p} 1-9 tab {p} c new {p} ?"), Style::new().fg(colors().fg_dim))),
            Line::from(Span::styled(format!(" {p} x close {p} q quit {p} p dir"), Style::new().fg(colors().fg_dim))),
            Line::from(Span::styled(format!(" {p} z suspend {p} R reopen"), Style::new().fg(colors().fg_dim))),
        ]
    };
    f.render_widget(Paragraph::new(foot_text), foot);
}

fn draw_tabbar(app: &mut App, f: &mut Frame, area: Rect) {
    app.tab_hits.clear();
    app.close_hits.clear();
    f.render_widget(Block::new().style(Style::new().bg(colors().bg_tabbar)), area);
    let hint = match app.selected.as_deref() {
        Some(SCRATCH) => " no tabs — ⏎ start Claude · c claude · t shell",
        Some(_) => " no tabs — ⏎ start Claude · c claude · t shell · e note",
        None => "",
    };
    let Some((tabs, active)) = app.current_tabs().filter(|(t, _)| !t.is_empty()) else {
        f.render_widget(Paragraph::new(Span::styled(hint, Style::new().fg(colors().fg_dim))), area);
        return;
    };
    let n = tabs.len().max(1) as u16;
    let max_w = ((area.width / n).saturating_sub(1)).clamp(10, 30) as usize;
    let mut spans = vec![];
    let mut x = area.x;
    let mut hits = vec![];
    let mut close_hits = vec![];
    for (i, t) in tabs.iter().enumerate() {
        let (g, gs) = tab_glyph(t, app.current_rt(), app.spinner);
        let is_active = i == active;
        let attention = t.st.kind == TabKind::Claude && t.pane.is_some() && t.shown_status() == ClaudeStatus::Attention;
        let bg = if is_active { colors().bg_tab_active } else { colors().bg_tab };
        let fg = if attention {
            colors().amber
        } else if is_active {
            colors().fg_strong
        } else {
            colors().fg_tab
        };
        let label = truncate(&format!("{}:{}", i + 1, t.st.name), max_w.saturating_sub(6));
        let text_style = Style::new().fg(fg).bg(bg).add_modifier(if is_active { Modifier::BOLD } else { Modifier::empty() });
        let parts = [
            Span::styled(" ", Style::new().bg(bg)),
            Span::styled(g, gs.bg(bg)),
            Span::styled(format!(" {label} "), text_style),
            Span::styled("× ", Style::new().fg(colors().fg_dim).bg(bg)),
        ];
        let width: u16 = parts.iter().map(|s| s.width() as u16).sum();
        if x + width > area.x + area.width {
            spans.push(Span::styled(" …", Style::new().fg(colors().fg_dim)));
            break;
        }
        hits.push((Rect { x, y: area.y, width, height: 1 }, i));
        close_hits.push((Rect { x: x + width - 2, y: area.y, width: 2, height: 1 }, i));
        spans.extend(parts);
        spans.push(Span::raw(" "));
        x += width + 1;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    app.tab_hits = hits;
    app.close_hits = close_hits;
}

fn draw_terminal(app: &mut App, f: &mut Frame, area: Rect) {
    let focused = app.focus == Focus::Terminal && app.mode == Mode::Normal;
    let tab = app
        .selected
        .as_ref()
        .and_then(|id| app.rt.get(id))
        .and_then(|rt| rt.tabs.get(rt.active));
    let Some(tab) = tab else {
        let text = match app.selected_note() {
            Some(n) => format!(
                "{}  {}\n\n{}\n\n⏎ start a Claude session for this issue\nc  new Claude tab   C  …in a chosen directory\nt  shell tab   e  edit note   o  open in Obsidian",
                n.id,
                n.title,
                n.path.display()
            ),
            None if app.scratch_selected() => format!(
                "Scratch: shells and Claude sessions outside any issue\n\nThey start in {}\n\n⏎ / c  new Claude tab   C  …in a chosen directory\nt  shell tab",
                crate::config::shorten(&app.default_dir())
            ),
            None => "Select or create an issue (n).".into(),
        };
        f.render_widget(
            Paragraph::new(text)
                .alignment(Alignment::Center)
                .style(Style::new().fg(colors().fg_dim))
                .wrap(Wrap { trim: false }),
            centered(area, area.width.saturating_sub(4), 9),
        );
        return;
    };
    let Some(pane) = &tab.pane else {
        if let Some((_, snap)) = app.viewer.as_ref().filter(|(id, _)| *id == tab.st.id) {
            draw_snapshot(snap, &app.prefix_label(), f, area);
            return;
        }
        let rt = app.current_rt();
        let what = match (&tab.st.kind, &tab.st.claude_session) {
            _ if rt.is_some_and(|r| r.awaits_worktree(tab)) => "Creating the worktree…".into(),
            (_, None) if rt.and_then(|r| r.worktree_error.as_ref()).is_some() => format!(
                "Worktree script failed:\n\n{}\n\n⏎ try again",
                rt.and_then(|r| r.worktree_error.as_deref()).unwrap_or_default()
            ),
            (TabKind::Claude, Some(sid)) => format!("Claude session {sid} is suspended.\n\n⏎ resume it"),
            (TabKind::Claude, None) => "Claude tab is not running.\n\n⏎ start it".into(),
            _ => "Tab is not running.\n\n⏎ start it".into(),
        };
        let text = format!("{}\n\n{what}", tab.st.name);
        let h = (text.lines().count() as u16 + 2).max(7);
        f.render_widget(
            Paragraph::new(text)
                .alignment(Alignment::Center)
                .style(Style::new().fg(colors().fg_dim))
                .wrap(Wrap { trim: false }),
            centered(area, area.width.saturating_sub(4), h),
        );
        return;
    };
    let exited = pane.exited;
    let scroll = pane.scroll;
    {
        let mut parser = pane.parser.lock().unwrap();
        let mut cursor = Cursor::default();
        if !focused || exited.is_some() || scroll > 0 {
            cursor.hide();
        }
        f.render_widget(PseudoTerminal::new(parser.screen()).cursor(cursor), area);
        if let Some(sel) = &pane.selection {
            let style = Style::new().fg(colors().on_accent).bg(colors().accent);
            for (row, from, to) in sel.spans(&mut parser) {
                f.buffer_mut().set_style(Rect::new(area.x + from, area.y + row, to - from, 1).intersection(area), style);
            }
        }
    }
    if let Some(code) = exited {
        let r = Rect { y: area.y + area.height.saturating_sub(1), height: 1, ..area };
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(format!(" process exited ({code}) — ⏎ restart{} · {} x close", if tab.st.claude_session.is_some() { "/resume" } else { "" }, app.prefix_label()))
                .style(Style::new().fg(colors().on_accent).bg(colors().amber)),
            r,
        );
    } else if scroll > 0 {
        draw_scroll_label(f, area, scroll);
    }
}

/// A suspended tab's saved screen. The bar gets its own row, right under
/// Claude's input box, which otherwise looks live.
fn draw_snapshot(snap: &Snapshot, prefix: &str, f: &mut Frame, area: Rect) {
    let [screen, bar] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    let mut cursor = Cursor::default();
    cursor.hide();
    f.render_widget(PseudoTerminal::new(snap.parser.screen()).cursor(cursor), screen);
    f.render_widget(
        Paragraph::new(format!(" – suspended · read-only · ⏎ resume · ↑↓ scroll · {prefix} x close"))
            .style(Style::new().fg(colors().on_accent).bg(colors().accent)),
        bar,
    );
    if snap.scroll > 0 {
        draw_scroll_label(f, screen, snap.scroll);
    }
}

fn draw_scroll_label(f: &mut Frame, area: Rect, scroll: usize) {
    let label = format!(" ↑ {scroll} ");
    let w = label.len() as u16;
    let r = Rect { x: area.x + area.width.saturating_sub(w), y: area.y, width: w, height: 1 };
    f.render_widget(Paragraph::new(label).style(Style::new().fg(colors().on_accent).bg(colors().accent)), r);
}

fn draw_statusbar(app: &mut App, f: &mut Frame, area: Rect) {
    f.render_widget(Block::new().style(Style::new().bg(colors().bg_status)), area);
    let mut left = vec![];
    if app.mode == Mode::Prefix {
        left.push(Span::styled(format!(" {} ", app.prefix_label()), Style::new().fg(colors().on_accent).bg(colors().orange).add_modifier(Modifier::BOLD)));
    }
    if let Some((m, _)) = &app.msg {
        let color = if app.msg_error { colors().red } else { colors().amber };
        left.push(Span::styled(format!(" {m}"), Style::new().fg(color)));
    } else if let Some(n) = app.selected_note() {
        left.push(Span::styled(format!(" {} ", n.id), Style::new().fg(colors().green).add_modifier(Modifier::BOLD)));
        left.push(Span::styled(n.title.clone(), Style::new().fg(colors().fg)));
    } else if app.scratch_selected() {
        left.push(Span::styled(" Scratch ", Style::new().fg(colors().green).add_modifier(Modifier::BOLD)));
    }
    let wt = app
        .current_rt()
        .and_then(|r| r.tabs.get(r.active).and_then(|t| t.st.cwd.as_ref()).or(r.worktree.as_ref()))
        .and_then(|p| p.file_name())
        .map(|n| format!("⎇ {}", n.to_string_lossy()))
        .unwrap_or_default();
    let running = app.running_claude_tabs();
    let suspended = match app.suspended_claude_tabs() {
        0 => String::new(),
        n => format!("{n} suspended  "),
    };
    let right = format!("{wt}  {running} claude  {suspended}{} ? help ", app.prefix_label());
    let rw = right.chars().count() as u16;
    f.render_widget(Paragraph::new(Line::from(left)), Rect { width: area.width.saturating_sub(rw + 1), ..area });
    f.render_widget(
        Paragraph::new(Span::styled(right, Style::new().fg(colors().fg_dim))).alignment(Alignment::Right),
        area,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

/// The end of `s` if it's over `max` chars, so the text being typed stays visible.
fn tail(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    let kept: String = s.chars().skip(len + 1 - max).collect();
    format!("…{kept}")
}

fn draw_prompt(f: &mut Frame, area: Rect, title: &str, body: &str, matches: Option<&Completion>) {
    let rows = matches.map_or(0, |m| m.names.len().min(MAX_MATCHES));
    let r = centered(area, 70, 3 + rows as u16);
    let width = r.width.saturating_sub(2) as usize;
    f.render_widget(Clear, r);
    let mut b = Block::bordered().border_type(BorderType::Rounded).title(title).border_style(Style::new().fg(colors().accent));
    let mut lines = vec![Line::raw(tail(&format!("{body}▏"), width))];
    if let Some(m) = matches {
        let start = m.selected.map_or(0, |s| (s + 1).saturating_sub(rows));
        for (i, name) in m.names.iter().enumerate().skip(start).take(rows) {
            let name = truncate(name, width);
            let style = if m.selected == Some(i) { Style::new().bg(colors().bg_sel) } else { Style::new().fg(colors().fg_dim) };
            lines.push(Line::styled(format!("{name:<width$}"), style));
        }
        if m.names.len() > rows {
            let at = m.selected.map_or(0, |s| s + 1);
            b = b.title_bottom(Line::raw(format!(" {at}/{} ", m.names.len())).right_aligned());
        }
    }
    f.render_widget(Paragraph::new(lines).block(b).style(Style::new().fg(colors().fg).bg(colors().bg_side)), r);
}

fn draw_help(app: &App, f: &mut Frame, area: Rect) {
    let p = app.prefix_label();
    let rows: Vec<(String, &str)> = vec![
        ("Sidebar (focused)".into(), ""),
        ("j/k ↑/↓".into(), "select issue"),
        ("⏎ / l".into(), "open issue (start/resume active tab)"),
        ("0/1/2/3, h/v".into(), "view To-do / Active / Done / Archive"),
        ("s".into(), "sort by created / name / moved / activity"),
        ("Tab / Esc".into(), "back to terminal"),
        ("x / D / q".into(), "close tab / detach / quit"),
        ("".into(), ""),
        (format!("Anywhere: {p} then…"), ""),
        ("s / Tab".into(), "toggle sidebar ↔ terminal focus"),
        ("c / t / e".into(), "new Claude / shell / note-editor tab"),
        ("C".into(), "new Claude tab in a chosen directory (Tab completes)"),
        ("p".into(), "set default directory (no-repo issues)"),
        ("1-9, h/l".into(), "go to tab, prev/next tab"),
        ("j / k".into(), "next/prev issue"),
        ("`".into(), "Scratch: tabs outside any issue"),
        ("x  ,".into(), "close tab, rename tab"),
        ("z / R".into(), "suspend Claude tab / reopen closed tab"),
        ("o / w".into(), "open note in Obsidian / issue in browser"),
        ("d / a / u / T".into(), "move issue to done / archive / active / to-do"),
        ("n".into(), "new local issue"),
        ("S / r".into(), "sync GitHub/Jira"),
        ("[ / ]".into(), "scroll the tab back / forward (also mouse wheel)"),
        (p.to_string(), "send the prefix key itself"),
        ("D".into(), "detach: office keeps running, `office --attach` returns"),
        ("q".into(), "quit office for everyone attached"),
        ("".into(), ""),
        ("Status".into(), "✳ working  ● needs you  ● done  ○ idle  ✕ exited"),
        ("".into(), "– suspended (⏎ resumes, scrolling still works)"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                Line::from(Span::styled(k.clone(), Style::new().fg(colors().accent).add_modifier(Modifier::BOLD)))
            } else {
                Line::from(vec![
                    Span::styled(format!("  {k:<14}"), Style::new().fg(colors().fg).add_modifier(Modifier::BOLD)),
                    Span::styled(v.to_string(), Style::new().fg(colors().fg_dim)),
                ])
            }
        })
        .collect();
    let r = centered(area, 66, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().border_type(BorderType::Rounded).title(" Keys — any key to close ").border_style(Style::new().fg(colors().accent)))
            .style(Style::new().bg(colors().bg_side)),
        r,
    );
}
