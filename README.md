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
- **Sessions survive restarts**: tabs are saved, and a Claude tab is restarted with
  `claude --resume <session>` the next time you open it.
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
frontmatter in Obsidian.

## Keys

The prefix (default `Ctrl-a`, tmux-style) works everywhere. Press it twice to send it
to the terminal.

| Sidebar focused | |
|---|---|
| `j`/`k` | select issue |
| `⏎` | open issue: start or resume its active tab |
| `1` `2` `3` / `h` | view Active / Done / Archive |
| `c` `t` `e` | new Claude / shell / note-editor tab |
| `o` / `w` | open note in Obsidian / issue in browser |
| `d` `a` `u` | move to done / archive / active |
| `n` | new local issue |
| `S` | sync GitHub/Jira |

| `prefix` then | |
|---|---|
| `s` / `Tab` | toggle focus between sidebar and terminal |
| `1`–`9`, `h`/`l` | go to tab, previous/next tab |
| `j` / `k` | next/previous issue |
| `c` `t` `e` `o` `d` `a` `n` `S` | same as in the sidebar |
| `x`, `,` | close tab, rename tab |
| `[` `]` | scroll back/forward (the mouse wheel works too) |
| `?`, `q` | help, quit |

The mouse is captured so you can click issues and tabs. Hold Shift (Option in iTerm)
to select text.

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
  state.rs      persisted tabs and session IDs
```

## Known limits / next steps

- Sessions stop when you quit; Claude tabs resume from their session ID, shells don't. A
  background daemon would keep them running across UI restarts (like tmux detach).
- There's no mouse passthrough to apps inside a tab (the wheel scrolls office's scrollback).
- Claude Code fires no hook for some state changes. Esc (interrupt or rejected permission) is
  inferred: a busy tab goes idle if no hook follows within 1.5s. After you approve a permission
  prompt the tab stays amber until that tool finishes. A new worktree's trust dialog shows as
  starting (`◌`), not as needing you.
- Done/archived issues keep their worktree. Remove it with `git worktree remove` when you're done.
