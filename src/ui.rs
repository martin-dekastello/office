use crate::app::{App, ConfirmKind, Focus, Mode, PromptKind, Tab};
use crate::state::TabKind;
use crate::status::ClaudeStatus;
use crate::vault::Stage;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use tui_term::widget::{Cursor, PseudoTerminal};

const SIDEBAR_W: u16 = 34;
const SPIN: [&str; 6] = ["·", "✢", "✳", "✶", "✻", "✽"];

const BG_SIDE: Color = Color::Rgb(22, 22, 26);
const BG_SEL: Color = Color::Rgb(48, 48, 58);
const BG_TAB: Color = Color::Rgb(34, 34, 40);
const BG_TAB_ACTIVE: Color = Color::Rgb(78, 78, 90);
const FG_DIM: Color = Color::Rgb(120, 120, 132);
const FG: Color = Color::Rgb(205, 205, 214);
const ORANGE: Color = Color::Rgb(217, 119, 87); // Claude orange
const AMBER: Color = Color::Rgb(245, 180, 60);
const GREEN: Color = Color::Rgb(120, 200, 120);
const RED: Color = Color::Rgb(220, 95, 95);
const ACCENT: Color = Color::Rgb(140, 170, 255);

pub fn status_glyph(s: ClaudeStatus, spin: usize) -> (&'static str, Style) {
    match s {
        ClaudeStatus::Working => (SPIN[spin % SPIN.len()], Style::new().fg(ORANGE).add_modifier(Modifier::BOLD)),
        ClaudeStatus::Attention => ("●", Style::new().fg(AMBER).add_modifier(Modifier::BOLD)),
        ClaudeStatus::Done => ("●", Style::new().fg(GREEN)),
        ClaudeStatus::Idle => ("○", Style::new().fg(FG_DIM)),
        ClaudeStatus::Starting => ("◌", Style::new().fg(FG_DIM)),
        ClaudeStatus::Exited => ("✕", Style::new().fg(RED)),
    }
}

fn tab_glyph(t: &Tab, spin: usize) -> (&'static str, Style) {
    if t.pane.is_none() {
        return ("–", Style::new().fg(FG_DIM));
    }
    match t.st.kind {
        TabKind::Claude => status_glyph(t.shown_status(), spin),
        _ if t.status == ClaudeStatus::Exited => ("✕", Style::new().fg(RED)),
        TabKind::Shell => ("$", Style::new().fg(FG_DIM)),
        TabKind::Notes => ("✎", Style::new().fg(FG_DIM)),
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
            let title = match kind {
                PromptKind::NewIssue => " New local issue — title ",
                PromptKind::RenameTab => " Rename tab (empty = auto) ",
            };
            draw_prompt(f, area, title, &buf);
        }
        Mode::Confirm(kind) => {
            let q = match kind {
                ConfirmKind::Quit => "Quit issuedeck? Running sessions are stopped (Claude tabs resume next time). [y/N]",
                ConfirmKind::CloseTab => "Close this tab and stop its process? [y/N]",
            };
            draw_prompt(f, area, " Confirm ", q);
        }
        _ => {}
    }
}

fn draw_sidebar(app: &mut App, f: &mut Frame, area: Rect) {
    let focused = app.focus == Focus::Sidebar;
    let block = Block::new()
        .borders(Borders::RIGHT)
        .border_style(Style::new().fg(if focused { ACCENT } else { Color::Rgb(50, 50, 58) }))
        .style(Style::new().bg(BG_SIDE));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [head, stages, list, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(inner);

    let attention: usize = app
        .rt
        .values()
        .flat_map(|r| r.tabs.iter())
        .filter(|t| t.pane.is_some() && t.shown_status() == ClaudeStatus::Attention)
        .count();
    let mut h = vec![Span::styled(" issuedeck", Style::new().fg(FG).add_modifier(Modifier::BOLD))];
    if app.syncing {
        h.push(Span::styled(format!("  {} sync", SPIN[app.spinner % 6]), Style::new().fg(FG_DIM)));
    }
    if attention > 0 {
        h.push(Span::styled(format!("  ● {attention}"), Style::new().fg(AMBER).add_modifier(Modifier::BOLD)));
    }
    f.render_widget(Paragraph::new(Line::from(h)), head);

    let mut st = vec![Span::raw(" ")];
    for s in Stage::ALL {
        let label = format!(" {} {} ", s.label(), app.count(s));
        let style = if s == app.view {
            Style::new().fg(Color::Black).bg(FG).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(FG_DIM)
        };
        st.push(Span::styled(label, style));
    }
    f.render_widget(Paragraph::new(Line::from(st)), Rect { height: 1, ..stages });

    // Issue list, two lines per issue.
    app.item_hits.clear();
    let notes: Vec<_> = app.visible().into_iter().cloned().collect();
    let per_page = (list.height / 2).max(1) as usize;
    let sel_idx = app
        .selected
        .as_ref()
        .and_then(|s| notes.iter().position(|n| &n.id == s))
        .unwrap_or(0);
    let offset = sel_idx.saturating_sub(per_page - 1);
    let w = list.width as usize;
    if notes.is_empty() {
        let hint = match app.view {
            Stage::Active => " No active issues.\n n  new local issue\n S  sync GitHub/Jira",
            _ => " Empty.",
        };
        f.render_widget(Paragraph::new(hint).style(Style::new().fg(FG_DIM)), list);
    }
    for (row, n) in notes.iter().enumerate().skip(offset).take(per_page) {
        let y = list.y + ((row - offset) * 2) as u16;
        let r = Rect { x: list.x, y, width: list.width, height: 2 };
        let selected = app.selected.as_deref() == Some(n.id.as_str());
        let bg = if selected { BG_SEL } else { BG_SIDE };
        let rollup = app.rt.get(&n.id).and_then(|r| r.rollup());
        let (g, gs) = match rollup {
            Some(s) => status_glyph(s, app.spinner),
            None => (" ", Style::new()),
        };
        let tabs = app.saved_tabs(&n.id);
        let bar = if selected && focused { "▌" } else { " " };
        let id_style = match rollup {
            Some(ClaudeStatus::Attention) => Style::new().fg(AMBER).add_modifier(Modifier::BOLD),
            _ if selected => Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            _ => Style::new().fg(FG).add_modifier(Modifier::BOLD),
        };
        let count = if tabs > 0 { format!("{tabs}▤ ") } else { String::new() };
        let left_w = 3 + n.id.chars().count();
        let pad = w.saturating_sub(left_w + count.chars().count());
        let l1 = Line::from(vec![
            Span::styled(bar, Style::new().fg(ACCENT)),
            Span::styled(format!("{g} "), gs),
            Span::styled(truncate(&n.id, w.saturating_sub(8)), id_style),
            Span::raw(" ".repeat(pad)),
            Span::styled(count, Style::new().fg(FG_DIM)),
        ]);
        let l2 = Line::from(vec![
            Span::styled(bar, Style::new().fg(ACCENT)),
            Span::raw("  "),
            Span::styled(truncate(&n.title, w.saturating_sub(4)), Style::new().fg(if selected { FG } else { FG_DIM })),
        ]);
        f.render_widget(Paragraph::new(vec![l1, l2]).style(Style::new().bg(bg)), r);
        app.item_hits.push((r, n.id.clone()));
    }

    let p = app.prefix_label();
    let foot_text = if focused {
        vec![
            Line::from(Span::styled(" ⏎ open c claude t shell e note", Style::new().fg(FG_DIM))),
            Line::from(Span::styled(" o obsidian d done n new ? keys", Style::new().fg(FG_DIM))),
        ]
    } else {
        vec![
            Line::from(Span::styled(format!(" {p} s sidebar {p} j/k issue"), Style::new().fg(FG_DIM))),
            Line::from(Span::styled(format!(" {p} 1-9 tab {p} c new {p} ?"), Style::new().fg(FG_DIM))),
        ]
    };
    f.render_widget(Paragraph::new(foot_text), foot);
}

fn draw_tabbar(app: &mut App, f: &mut Frame, area: Rect) {
    app.tab_hits.clear();
    f.render_widget(Block::new().style(Style::new().bg(Color::Rgb(16, 16, 20))), area);
    let Some((tabs, active)) = app.current_tabs() else {
        let msg = if app.selected.is_some() {
            " no tabs — ⏎ start Claude · c claude · t shell · e note"
        } else {
            ""
        };
        f.render_widget(Paragraph::new(Span::styled(msg, Style::new().fg(FG_DIM))), area);
        return;
    };
    if tabs.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(" no tabs — ⏎ start Claude · c claude · t shell · e note", Style::new().fg(FG_DIM))),
            area,
        );
        return;
    }
    let n = tabs.len().max(1) as u16;
    let max_w = ((area.width / n).saturating_sub(1)).clamp(10, 30) as usize;
    let mut spans = vec![];
    let mut x = area.x;
    let mut hits = vec![];
    for (i, t) in tabs.iter().enumerate() {
        let (g, gs) = tab_glyph(t, app.spinner);
        let is_active = i == active;
        let attention = t.st.kind == TabKind::Claude && t.pane.is_some() && t.shown_status() == ClaudeStatus::Attention;
        let bg = if is_active { BG_TAB_ACTIVE } else { BG_TAB };
        let fg = if attention {
            AMBER
        } else if is_active {
            Color::White
        } else {
            Color::Rgb(160, 160, 172)
        };
        let label = truncate(&format!("{}:{}", i + 1, t.st.name), max_w.saturating_sub(4));
        let text_style = Style::new().fg(fg).bg(bg).add_modifier(if is_active { Modifier::BOLD } else { Modifier::empty() });
        let parts = [
            Span::styled(" ", Style::new().bg(bg)),
            Span::styled(g, gs.bg(bg)),
            Span::styled(format!(" {label} "), text_style),
        ];
        let width: u16 = parts.iter().map(|s| s.width() as u16).sum();
        if x + width > area.x + area.width {
            spans.push(Span::styled(" …", Style::new().fg(FG_DIM)));
            break;
        }
        hits.push((Rect { x, y: area.y, width, height: 1 }, i));
        spans.extend(parts);
        spans.push(Span::raw(" "));
        x += width + 1;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    app.tab_hits = hits;
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
                "{}  {}\n\n{}\n\n⏎ start a Claude session for this issue\nc  new Claude tab   t  shell tab   e  edit note   o  open in Obsidian",
                n.id,
                n.title,
                n.path.display()
            ),
            None => "Select or create an issue (n).".into(),
        };
        f.render_widget(
            Paragraph::new(text)
                .alignment(Alignment::Center)
                .style(Style::new().fg(FG_DIM))
                .wrap(Wrap { trim: false }),
            centered(area, area.width.saturating_sub(4), 8),
        );
        return;
    };
    let Some(pane) = &tab.pane else {
        let what = match (&tab.st.kind, &tab.st.claude_session) {
            (TabKind::Claude, Some(sid)) => format!("Claude session {sid} is suspended.\n\n⏎ resume it"),
            (TabKind::Claude, None) => "Claude tab is not running.\n\n⏎ start it".into(),
            _ => "Tab is not running.\n\n⏎ start it".into(),
        };
        f.render_widget(
            Paragraph::new(format!("{}\n\n{what}", tab.st.name))
                .alignment(Alignment::Center)
                .style(Style::new().fg(FG_DIM))
                .wrap(Wrap { trim: false }),
            centered(area, area.width.saturating_sub(4), 7),
        );
        return;
    };
    let exited = pane.exited;
    let scroll = pane.scroll;
    {
        let parser = pane.parser.lock().unwrap();
        let mut cursor = Cursor::default();
        if !focused || exited.is_some() || scroll > 0 {
            cursor.hide();
        }
        f.render_widget(PseudoTerminal::new(parser.screen()).cursor(cursor), area);
    }
    if let Some(code) = exited {
        let r = Rect { y: area.y + area.height.saturating_sub(1), height: 1, ..area };
        f.render_widget(Clear, r);
        f.render_widget(
            Paragraph::new(format!(" process exited ({code}) — ⏎ restart{} · {} x close", if tab.st.claude_session.is_some() { "/resume" } else { "" }, app.prefix_label()))
                .style(Style::new().fg(Color::Black).bg(AMBER)),
            r,
        );
    } else if scroll > 0 {
        let label = format!(" ↑ {scroll} ");
        let w = label.len() as u16;
        let r = Rect { x: area.x + area.width.saturating_sub(w), y: area.y, width: w, height: 1 };
        f.render_widget(Paragraph::new(label).style(Style::new().fg(Color::Black).bg(ACCENT)), r);
    }
}

fn draw_statusbar(app: &mut App, f: &mut Frame, area: Rect) {
    f.render_widget(Block::new().style(Style::new().bg(Color::Rgb(28, 28, 34))), area);
    let mut left = vec![];
    if app.mode == Mode::Prefix {
        left.push(Span::styled(format!(" {} ", app.prefix_label()), Style::new().fg(Color::Black).bg(ORANGE).add_modifier(Modifier::BOLD)));
    }
    if let Some((m, _)) = &app.msg {
        left.push(Span::styled(format!(" {m}"), Style::new().fg(AMBER)));
    } else if let Some(n) = app.selected_note() {
        left.push(Span::styled(format!(" {} ", n.id), Style::new().fg(GREEN).add_modifier(Modifier::BOLD)));
        left.push(Span::styled(n.title.clone(), Style::new().fg(FG)));
    }
    let wt = app
        .current_rt()
        .and_then(|r| r.worktree.as_ref())
        .and_then(|p| p.file_name())
        .map(|n| format!("⎇ {}", n.to_string_lossy()))
        .unwrap_or_default();
    let running = app.running_claude_tabs();
    let right = format!("{wt}  {running} claude  {} ? help ", app.prefix_label());
    let rw = right.chars().count() as u16;
    f.render_widget(Paragraph::new(Line::from(left)), Rect { width: area.width.saturating_sub(rw + 1), ..area });
    f.render_widget(
        Paragraph::new(Span::styled(right, Style::new().fg(FG_DIM))).alignment(Alignment::Right),
        area,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_prompt(f: &mut Frame, area: Rect, title: &str, body: &str) {
    let r = centered(area, 70, 3);
    f.render_widget(Clear, r);
    let b = Block::bordered().border_type(BorderType::Rounded).title(title).border_style(Style::new().fg(ACCENT));
    f.render_widget(Paragraph::new(format!("{body}▏")).block(b).style(Style::new().fg(FG).bg(BG_SIDE)), r);
}

fn draw_help(app: &App, f: &mut Frame, area: Rect) {
    let p = app.prefix_label();
    let rows: Vec<(String, &str)> = vec![
        ("Sidebar (focused)".into(), ""),
        ("j/k ↑/↓".into(), "select issue"),
        ("⏎ / l".into(), "open issue (start/resume active tab)"),
        ("1/2/3, h/v".into(), "view Active / Done / Archive"),
        ("Tab / Esc".into(), "back to terminal"),
        ("".into(), ""),
        (format!("Anywhere: {p} then…"), ""),
        ("s / Tab".into(), "toggle sidebar ↔ terminal focus"),
        ("c / t / e".into(), "new Claude / shell / note-editor tab"),
        ("1-9, h/l".into(), "go to tab, prev/next tab"),
        ("j / k".into(), "next/prev issue"),
        ("x  ,".into(), "close tab, rename tab"),
        ("o / w".into(), "open note in Obsidian / issue in browser"),
        ("d / a / u".into(), "move issue to done / archive / active"),
        ("n".into(), "new local issue"),
        ("S / r".into(), "sync GitHub/Jira"),
        ("[ / ]".into(), "scroll back / forward (also mouse wheel)"),
        (p.to_string(), "send the prefix key itself"),
        ("q".into(), "quit"),
        ("".into(), ""),
        ("Status".into(), "✳ working  ● needs you  ● done  ○ idle  ✕ exited"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, v)| {
            if v.is_empty() {
                Line::from(Span::styled(k.clone(), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)))
            } else {
                Line::from(vec![
                    Span::styled(format!("  {k:<14}"), Style::new().fg(FG).add_modifier(Modifier::BOLD)),
                    Span::styled(v.to_string(), Style::new().fg(FG_DIM)),
                ])
            }
        })
        .collect();
    let r = centered(area, 66, lines.len() as u16 + 2);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::bordered().border_type(BorderType::Rounded).title(" Keys — any key to close ").border_style(Style::new().fg(ACCENT)))
            .style(Style::new().bg(BG_SIDE)),
        r,
    );
}
