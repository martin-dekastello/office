# office

Issues on the left, Claude Code tabs on the right, one Obsidian note per issue.

```
 office  ● 1                            │ ✳ 1:Refactor tile cache   ● 2:Fix loader   $ 3:shell
  To-do 5  Active 3  Done 1  Archive 0  │
                                        │  (live Claude Code session for the selected issue)
 ✳ TS-42                             3▤ │
   Přidat mapu dlaždic pro fotky        │
 ● PROJ-118                          1▤ │
   Fix OAuth login loop                 │
   LOCAL-3                              │
   Session naming request               │
```

- **Sorted by issue.** Each issue has its own tabs: Claude sessions, shells, and a note editor.
- **Claude status on every tab and issue**, set by Claude Code hooks:
  `✳` working (animated) · `●` amber = needs you (permission/question) ·
  `●` green = finished while you weren't looking · `○` idle · `✕` exited · `–` suspended.
  The status of the most urgent tab also shows on the issue in the sidebar.
  The terminal bell rings when a background tab needs you.
- **Notes live in Obsidian**: `<vault>/<notes_dir>/{todo,active,done,archive}/<id>-<slug>.md`,
  one file per issue. Moving an issue to another stage moves the file and updates `status:`
  and `moved:`.
- **Sorting.** `s` cycles the issue list between newest created (the default), name, most
  recently moved to its stage, and activity. Activity puts an issue whose Claude needs you
  first, then working, then done (not seen yet), then idle or suspended Claude, then shells
  only, then everything else. Each issue ranks by its most urgent tab.
- **To-do vs Active.** New issues, synced or local, land in To-do. Active is what you're working
  on: move an issue there with `u`, or start a Claude session on a To-do issue and it moves
  there itself. Office never moves an issue because its GitHub or Jira status changed.
  Each new Claude session is logged under `## Sessions` with its `claude --resume <id>` command.
- **One git worktree + branch per issue** (`<worktrees_dir>/<repo>/<id>-<slug>`), so all
  tabs of an issue share one checkout. It's created when the issue's first Claude or shell tab
  opens, not before, so issues you never work on take no disk space. Bring your own script
  with `worktree_command`.
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

### Your own worktree script

If a plain `git worktree add` isn't how you set up a checkout, set `worktree_command`:

```toml
worktree_command = ["~/bin/make-worktree.sh"]
```

office runs it as `make-worktree.sh <repo> <id>-<slug>` (e.g. `~/code/backend PROJ-118-fix-oauth-login-loop`)
from inside the repo, the first time an issue's Claude or shell tab opens. The second argument
works as a folder or branch name as is. `OFFICE_REPO`, `OFFICE_WORKTREE_NAME`,
`OFFICE_WORKTREES_DIR` and `OFFICE_ISSUE` hold the same and more. The script must print the
worktree's path as its **last line of output** (a relative path is taken from the repo). Send
anything else to stderr. office remembers the path and runs the script again only if that
directory disappears.

It runs in the background and the tab shows a spinner until it's done. If it exits non-zero or
prints a path that doesn't exist, the tab shows its error output and `⏎` runs it again. office
doesn't fall back to the repo as it does when plain `git worktree add` fails.

```sh
#!/bin/sh
set -e
dest="$HOME/wt/$2"
[ -d "$dest" ] || git -C "$1" worktree add -b "feature/$2" "$dest" origin/main >&2
cp "$1/.env" "$dest/" 2>/dev/null || true
echo "$dest"
```

To point one issue at a different repo, add `repo_path: ~/code/other` to that note's
frontmatter in Obsidian. To run a single Claude session somewhere else, press `C`. `Tab`
completes the path as in zsh: it fills in what it can, lists the matches, then cycles through
them (`Shift-Tab` goes back, `/` enters the selected folder). The tab keeps that directory
when it restarts or resumes, and no worktree is created for it.

Issues with no repo start their tabs in the default directory, which is your home directory
until you change it. Press `p` to set it, in the same dialog as `C`. If you start office
somewhere else, it offers to make that directory the default: `⏎` accepts, `Esc` keeps the
current one, and it asks again next time. A tab keeps the directory it started in, so changing
the default only affects new tabs and sessions always resume where they began.

## Keys

The prefix (default `Ctrl-a`, tmux-style) works everywhere. Press it twice to send it
to the terminal.

| Sidebar focused | |
|---|---|
| `j`/`k` | select issue |
| `⏎` | open issue: start or resume its active tab |
| `0` `1` `2` `3` / `h` | view To-do / Active / Done / Archive |
| `s` | sort by created / name / moved / activity |
| `c` `t` `e` | new Claude / shell / note-editor tab |
| `z` / `R` | suspend Claude tab / reopen closed Claude tab |
| `C` | new Claude tab in a directory you type (`Tab` completes, empty = issue default) |
| `p` | set the default directory for issues with no repo (`Tab` completes, empty = keep) |
| `o` / `w` | open note in Obsidian / issue in browser |
| `d` `a` `u` `T` | move to done / archive / active / to-do |
| `n` | new local issue |
| `S` | sync GitHub/Jira |
| `x` / `q` | close tab / quit |

| `prefix` then | |
|---|---|
| `s` / `Tab` | toggle focus between sidebar and terminal |
| `1`–`9`, `h`/`l` | go to tab, previous/next tab |
| `j` / `k` | next/previous issue |
| `c` `C` `p` `t` `e` `z` `R` `o` `d` `a` `u` `T` `n` `S` | same as in the sidebar |
| `x`, `,` | close tab, rename tab |
| `[` `]` | scroll the tab back/forward (the mouse wheel works too) |
| `?`, `q` | help, quit |

On a suspended tab, `⏎` resumes, and the wheel, `↑` `↓` `PgUp` `PgDn` `g` `G` and
`prefix [ ]` scroll. Moving to a suspended tab never starts it. `R` keeps the last 10 closed
Claude tabs per issue.

The mouse is captured so you can click issues, tabs, the To-do/Active/Done/Archive views,
a tab's `×` to close it, and `quit` at the top of the sidebar. Closing and quitting ask first. Inside a tab, the wheel, clicks and
drags go to the app when it handles the mouse itself, as Claude Code does with
`"tui": "fullscreen"`. Apps on the alternate screen that don't (`less`) get arrow keys from
the wheel. Anything else scrolls office's scrollback.

In a shell tab, drag to select text. Letting go copies it, and the highlight stays until you
click or type. Hold the mouse above or below the tab to scroll while selecting. Copying uses
`pbcopy`, `clip`, or `wl-copy`/`xclip`/`xsel`, and also OSC 52 for SSH. Anywhere else, including
Claude tabs and shell apps that handle the mouse themselves, hold Shift (Option in iTerm) to
select text.

## How the Claude status works

Each tab is started with `OFFICE_TAB=<uuid>`. Claude tabs are launched as
`claude --settings <data>/claude-hooks.json`. That file registers `office hook` for
`SessionStart`, `UserPromptSubmit`, `Pre/PostToolUse`, `PermissionRequest`,
`Notification`, `Stop` and `SessionEnd`. The hook writes `<data>/status/<uuid>.json`
and the UI polls it. The hook does nothing when `OFFICE_TAB` isn't set, so it's safe to
install globally. If you'd rather not use `--settings`, set `inject_hooks = false` and run
`office install-hooks`. That merges the hooks into `~/.claude/settings.json` and writes
a backup first.

Every tab also gets `OFFICE_ISSUE`, `OFFICE_NOTE` (the note's path) and `OFFICE_BIN` (the
office executable). You can tell Claude "append a summary to $OFFICE_NOTE".

## Opening tabs from Claude

`office tab new` opens a tab in an issue of the running office. The tab opens in the
background, so whatever you're looking at stays on screen. Inside a tab, the issue defaults
to `$OFFICE_ISSUE`:

```sh
office tab new --mode plan --name probe --cwd ~/code/other --prompt-file brief.md
office tab new --kind shell
```

`--mode` and `--prompt-file` apply only to the first launch. A suspended or restarted tab
resumes its session as usual and never gets the prompt again. The command waits up to 5s
for office to answer and fails if office isn't running.

`office install-skill` adds the `/new-office-tab` skill to `~/.claude/skills` (or
`$CLAUDE_CONFIG_DIR`). In a Claude tab, `/new-office-tab plan <task>` writes a handoff brief
from the conversation and opens a new Claude tab in the same issue, started in that mode
with the brief as its first prompt. It's like `/new-claude-tmux`, but without tmux.

## CLI

```
office                 # TUI
office new <title>     # create a local issue note
office sync            # pull GitHub/Jira issues and create missing notes in todo/
office list
office install-hooks
office tab new [--issue ID] [--kind claude|shell|notes] [--cwd DIR] [--name NAME]
               [--mode MODE] [--prompt-file FILE]   # open a tab in the running office
office install-skill   # add /new-office-tab to ~/.claude/skills
```

## Layout

```
src/
  app.rs        state, input routing, tab lifecycle, status polling
  ui.rs         ratatui rendering (sidebar, tab bar, terminal, popups)
  term.rs       PTY + vt100 pane (portable-pty, tui-term)
  keys.rs       key event -> terminal bytes
  clipboard.rs  copy to the system clipboard and OSC 52
  vault.rs      Obsidian notes (todo/active/done/archive), slugs, frontmatter
  providers.rs  GitHub (gh), Jira REST v3
  status.rs     Claude Code hook handler and settings
  api.rs        `office tab new` requests to the running UI
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
