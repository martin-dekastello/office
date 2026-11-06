# office

Issues on the left, Claude Code tabs on the right, one Obsidian note per issue.

```
 office  ● 1                     │ ✳ 1:Refactor tile cache   ● 2:Fix loader   $ 3:shell
  Active 3  Done 1  Archive 0    │
                                 │  (live Claude Code session for the selected issue)
 ✳ TS-42                      3▤ │
   Přidat mapu dlaždic pro fotky │
 ● PROJ-118                   1▤ │
   Fix OAuth login loop          │
   LOCAL-3                       │
   Session naming request        │
```

- **Sorted by issue.** Each issue has its own tabs: Claude sessions, shells, and a note editor.
- **Claude status on every tab and issue**, set by Claude Code hooks:
  `✳` working (animated) · `●` amber = needs you (permission/question) ·
  `●` green = finished while you weren't looking · `○` idle · `✕` exited · `–` suspended.
  The status of the most urgent tab also shows on the issue in the sidebar.
  The terminal bell rings when a background tab needs you.
- **Notes live in Obsidian**: `<vault>/<notes_dir>/{active,done,archive}/<id>-<slug>.md`, one
  file per issue. Moving an issue to done/archive moves the file and updates `status:`.
  Each new Claude session is logged under `## Sessions` with its `claude --resume <id>` command.
- **One git worktree + branch per issue** (`<worktrees_dir>/<repo>/<id>-<slug>`), so all
  tabs of an issue share one checkout.
- **Sessions can be stopped and picked up again.** Press `z` to suspend an inactive Claude
  tab: its process stops and its RAM is freed, but you can still scroll its saved screen,
  and `⏎` continues it with `claude --resume <session>`. A Claude tab that exits or was
  running when you quit office ends up the same way. Closing one with `x` keeps it too:
  `R` reopens it.
- **GitHub (via `gh`) + Jira Cloud + local issues**, behind one `Provider` trait.
- **Cross-platform**: macOS, Linux, Windows (ConPTY). It's a single binary with its own
  terminal emulation, so it doesn't need tmux.

## Install

```sh
cargo install --path .
office               # first run writes the config and prints its path
office paths         # where config, data, notes and worktrees live
```

Requirements: `claude` on PATH, `git` for worktrees, `gh` (logged in) for GitHub sources,
`$JIRA_API_TOKEN` for Jira.

## Config

`~/.config/office/config.toml` on Linux, `~/Library/Application Support/office/config.toml`
on macOS, `%APPDATA%\office\config.toml` on Windows. Override with `$OFFICE_CONFIG`.

```toml
vault = "~/Documents/Obsidian"
notes_dir = "Issues"
worktrees_dir = "~/worktrees"
prefix = "ctrl-a"

[[github]]
repo = "johan/transport-sim"
path = "~/code/transport-sim"   # local clone used for worktrees
id_prefix = "TS"                # -> TS-42-<slug>.md
search = "assignee:@me"

[[jira]]
url = "https://yourcompany.atlassian.net"
email = "you@example.com"
token_env = "JIRA_API_TOKEN"
jql = "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
path = "~/code/backend"
```

To point one issue at a different repo, add `repo_path: ~/code/other` to that note's
frontmatter in Obsidian. To run a single Claude session somewhere else, press `C`. The tab
keeps that directory when it restarts or resumes, and no worktree is created for it.

## Keys

The prefix (default `Ctrl-a`, tmux-style) works everywhere. Press it twice to send it
to the terminal.

| Sidebar focused | |
|---|---|
| `j`/`k` | select issue |
| `⏎` | open issue: start or resume its active tab |
| `1` `2` `3` / `h` | view Active / Done / Archive |
| `c` `t` `e` | new Claude / shell / note-editor tab |
| `z` / `R` | suspend Claude tab / reopen closed Claude tab |
| `C` | new Claude tab in a directory you type (empty = issue default) |
| `o` / `w` | open note in Obsidian / issue in browser |
| `d` `a` `u` | move to done / archive / active |
| `n` | new local issue |
| `S` | sync GitHub/Jira |
| `x` / `q` | close tab / quit |

| `prefix` then | |
|---|---|
| `s` / `Tab` | toggle focus between sidebar and terminal |
| `1`–`9`, `h`/`l` | go to tab, previous/next tab |
| `j` / `k` | next/previous issue |
| `c` `C` `t` `e` `z` `R` `o` `d` `a` `n` `S` | same as in the sidebar |
| `x`, `,` | close tab, rename tab |
| `[` `]` | scroll the tab back/forward (the mouse wheel works too) |
| `?`, `q` | help, quit |

On a suspended tab, `⏎` resumes, and the wheel, `↑` `↓` `PgUp` `PgDn` `g` `G` and
`prefix [ ]` scroll. Moving to a suspended tab never starts it. `R` keeps the last 10 closed
Claude tabs per issue.

The mouse is captured so you can click issues and tabs, a tab's `×` to close it, and `quit`
at the top of the sidebar. Both ask first. Inside a tab, the wheel, clicks and
drags go to the app when it handles the mouse itself, as Claude Code does with
`"tui": "fullscreen"`. Apps on the alternate screen that don't (`less`) get arrow keys from
the wheel. Anything else scrolls office's scrollback. Hold Shift (Option in iTerm) to select text.

## How the Claude status works

Each tab is started with `OFFICE_TAB=<uuid>`. Claude tabs are launched as
`claude --settings <data>/claude-hooks.json`. That file registers `office hook` for
`SessionStart`, `UserPromptSubmit`, `Pre/PostToolUse`, `PermissionRequest`,
`Notification`, `Stop` and `SessionEnd`. The hook writes `<data>/status/<uuid>.json`
and the UI polls it. The hook does nothing when `OFFICE_TAB` isn't set, so it's safe to
install globally. If you'd rather not use `--settings`, set `inject_hooks = false` and run
`office install-hooks`. That merges the hooks into `~/.claude/settings.json` and writes
a backup first.

Every tab also gets `OFFICE_ISSUE` and `OFFICE_NOTE` (the note's path). You can tell
Claude "append a summary to $OFFICE_NOTE".

## CLI

```
office                 # TUI
office new <title>     # create a local issue note
office sync            # pull GitHub/Jira issues and create missing notes
office list
office install-hooks
```

## Layout

```
src/
  app.rs        state, input routing, tab lifecycle, status polling
  ui.rs         ratatui rendering (sidebar, tab bar, terminal, popups)
  term.rs       PTY + vt100 pane (portable-pty, tui-term)
  keys.rs       key event -> terminal bytes
  vault.rs      Obsidian notes (active/done/archive), slugs, frontmatter
  providers.rs  GitHub (gh), Jira REST v3
  status.rs     Claude Code hook handler and settings
  worktree.rs   per-issue git worktrees
  state.rs      persisted tabs, session IDs and saved screens
  transcript.rs conversation text from Claude's transcript, for saved screens
```

## Known limits / next steps

- Sessions stop when you quit; Claude tabs resume from their session ID, shells don't. A
  background daemon would keep them running across UI restarts (like tmux detach).
- OSC 52 isn't passed on, so text selected by dragging inside fullscreen Claude Code doesn't
  reach the clipboard. Select with Shift (Option in iTerm) instead.
- Saved screens are plain files in `<data>/snapshots`, deleted on resume or when the tab is
  dropped. A pane narrower than the saved width clips lines instead of rewrapping them.
- Claude's fullscreen mode (`"tui": "fullscreen"`) leaves no scrollback, so a suspended tab
  shows the conversation from its transcript above the last screen: messages and tool calls,
  not tool output. This needs the hooks.
- Claude Code fires no hook for some state changes. Esc (interrupt or rejected permission) is
  inferred: a busy tab goes idle if no hook follows within 1.5s. After you approve a permission
  prompt the tab stays amber until that tool finishes. A new worktree's trust dialog shows as
  starting (`◌`), not as needing you.
- Done/archived issues keep their worktree. Remove it with `git worktree remove` when you're done.
