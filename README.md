# salash

Issues on the left, Claude Code tabs on the right, one Obsidian note per issue.

The name comes from the Slovak word *salaš*, a shepherd's hut up in the mountain pastures.

```
 salash  ● 1                            │ ✳ 1:Refactor tile cache   ● 2:Fix loader   $ 3:shell
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
- **Scratch, for work outside any issue.** Press `` ` `` and a Scratch row appears at the top
  of the sidebar, where `c`, `C` and `t` open Claude and shell tabs that belong to no issue, for
  example to write up a new issue. They start in the default directory, with no worktree or note.
  Scratch stays first in every view and sort while it has tabs, and hides again once it has none.
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
  there itself. Salash never moves an issue because its GitHub or Jira status changed.
  Each new Claude session is logged under `## Sessions` with its `claude --resume <id>` command.
- **One git worktree + branch per issue** (`<worktrees_dir>/<repo>/<id>-<slug>`), so all
  tabs of an issue share one checkout. It's created when the issue's first Claude or shell tab
  opens, not before, so issues you never work on take no disk space. Bring your own script
  with `worktree_command`.
- **Sessions can be stopped and picked up again.** Press `z` to suspend an inactive Claude
  tab: its process stops and its RAM is freed, but you can still scroll its saved screen,
  and `⏎` continues it with `claude --resume <session>`. A Claude tab that was running when
  you quit salash ends up the same way. Closing one with `x` keeps it too: `R` reopens it.
- **Tabs close when their process exits**: quit Claude, `exit` a shell or close the note's
  editor and the tab goes away. A Claude session that exits can still be brought back with
  `R`. Only a Claude that fails to start stays open, so you can read why.
- **One office per folder, running in the background**, like a tmux server. `office ~/work`
  starts it and attaches. Closing the terminal, or `D` to detach, leaves every tab running, and
  `office --attach ~/work` picks it up again, from another terminal or over ssh. Several people
  can attach at once and share one view. See [docs/architecture.md](docs/architecture.md)
  and [docs/multi-client.md](docs/multi-client.md).
- **GitHub (via `gh`) + Jira Cloud + local issues**, behind one `Provider` trait.
- **Light and dark.** With `theme = "auto"` (the default) salash asks the terminal for its
  background colour at startup and matches it; set `"dark"` or `"light"` to choose. Programs in
  tabs that ask for the background (Claude Code, nvim) get the same answer.
- **Cross-platform**: macOS, Linux, Windows (ConPTY). It's a single binary with its own
  terminal emulation, so it doesn't need tmux.

## Install

```sh
cargo install --path .
office [folder]      # start office in a folder (default: here); first run writes the config
office paths         # where config, data, notes and worktrees live
```

Each folder's office keeps its tabs, sessions and socket in `<folder>/.office`, which ignores
itself in git. The first office you start takes over the tabs and sessions saved by older
versions of office.

Requirements: `claude` on PATH, `git` for worktrees, `gh` (logged in) for GitHub sources,
`$JIRA_API_TOKEN` for Jira.

## Config

`~/.config/office/config.toml` on Linux, `~/Library/Application Support/office/config.toml`
on macOS, `%APPDATA%\office\config.toml` on Windows. Override with `$OFFICE_CONFIG`.
A folder's `.office/config.toml` overrides any of it for that folder's office. For example,
give a team folder its own vault and sources.

```toml
vault = "~/Documents/Obsidian"
notes_dir = "Issues"
worktrees_dir = "~/worktrees"
prefix = "ctrl-a"
theme = "auto"                  # auto | dark | light

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

salash runs it as `make-worktree.sh <repo> <id>-<slug>` (e.g. `~/code/backend PROJ-118-fix-oauth-login-loop`)
from inside the repo, the first time an issue's Claude or shell tab opens. The second argument
works as a folder or branch name as is. `SALASH_REPO`, `SALASH_WORKTREE_NAME`,
`SALASH_WORKTREES_DIR` and `SALASH_ISSUE` hold the same and more. The script must print the
worktree's path as its **last line of output** (a relative path is taken from the repo). Send
anything else to stderr. salash remembers the path and runs the script again only if that
directory disappears.

It runs in the background and the tab shows a spinner until it's done. If it exits non-zero or
prints a path that doesn't exist, the tab shows its error output and `⏎` runs it again. salash
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

Issues with no repo start their tabs in the default directory, which is office's folder
until you change it. Press `p` to set it, in the same dialog as `C`. A tab keeps the directory
it started in, so changing the default only affects new tabs, and sessions always resume where
they began.

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
| `` ` `` | go to Scratch, for tabs outside any issue |
| `z` / `R` | suspend Claude tab / reopen closed Claude tab |
| `C` | new Claude tab in a directory you type (`Tab` completes, empty = issue default) |
| `p` | set the default directory for issues with no repo (`Tab` completes, empty = keep) |
| `o` / `w` | open note in Obsidian / issue in browser |
| `d` `a` `u` `T` | move to done / archive / active / to-do |
| `n` | new local issue |
| `S` | sync GitHub/Jira |
| `x` | close tab |
| `D` / `q` | detach (office keeps running) / quit office for everyone attached |

| `prefix` then | |
|---|---|
| `s` / `Tab` | toggle focus between sidebar and terminal |
| `1`–`9`, `h`/`l` | go to tab, previous/next tab |
| `j` / `k` | next/previous issue |
| `` ` `` | go to Scratch |
| `c` `C` `p` `t` `e` `z` `R` `o` `d` `a` `u` `T` `n` `S` | same as in the sidebar |
| `x`, `,` | close tab, rename tab |
| `[` `]` | scroll the tab back/forward (the mouse wheel works too) |
| `?`, `D`, `q` | help, detach, quit |

On a suspended tab, `⏎` resumes, and the wheel, `↑` `↓` `PgUp` `PgDn` `g` `G` and
`prefix [ ]` scroll. Moving to a suspended tab never starts it. `R` keeps the last 10 closed
Claude tabs per issue.

The mouse is captured so you can click issues, tabs, the To-do/Active/Done/Archive views,
a tab's `×` to close it, and `quit` at the top of the sidebar. Closing and quitting ask first. Inside a tab, the wheel, clicks and
drags go to the app when it handles the mouse itself, as Claude Code does with
`"tui": "fullscreen"`. Apps on the alternate screen that don't (`less`) get arrow keys from
the wheel. Anything else scrolls salash's scrollback.

In a shell tab, drag to select text. Letting go copies it, and the highlight stays until you
click or type. Hold the mouse above or below the tab to scroll while selecting. Your own
`office` client copies it, with `pbcopy`, `clip`, or `wl-copy`/`xclip`/`xsel`, and also OSC 52 for SSH. Anywhere else, including
Claude tabs and shell apps that handle the mouse themselves, hold Shift (Option in iTerm) to
select text.

## How the Claude status works

Each tab is started with `SALASH_TAB=<uuid>`. Claude tabs are launched as
`claude --settings <data>/claude-hooks.json`. That file registers `salash hook` for
`SessionStart`, `UserPromptSubmit`, `Pre/PostToolUse`, `PermissionRequest`,
`Notification`, `Stop` and `SessionEnd`. The hook writes `<data>/status/<uuid>.json`
and the UI polls it. The hook does nothing when `SALASH_TAB` isn't set, so it's safe to
install globally. If you'd rather not use `--settings`, set `inject_hooks = false` and run
`salash install-hooks`. That merges the hooks into `~/.claude/settings.json` and writes
a backup first.

Every tab also gets `OFFICE_ISSUE`, `OFFICE_NOTE` (the note's path), `OFFICE_BIN` (the
office executable) and `OFFICE_DATA` (its office's `.office`, where `<data>` is). You can tell
Claude "append a summary to $OFFICE_NOTE".

## Opening tabs from Claude

`salash tab new` opens a tab in an issue of the running salash. The tab opens in the
background, so whatever you're looking at stays on screen. Inside a tab, the issue defaults
to `$SALASH_ISSUE`:

```sh
salash tab new --mode plan --name probe --cwd ~/code/other --prompt-file brief.md
salash tab new --kind shell
salash tab new --issue scratch     # a tab outside any issue
```

`--mode` and `--prompt-file` apply only to the first launch. A suspended or restarted tab
resumes its session as usual and never gets the prompt again. The command waits up to 5s
for office to answer and fails if office isn't running. Inside a tab it goes to that tab's
office. Elsewhere it goes to the office of the nearest folder above with a `.office`, or the
one given with `--folder`.

`salash install-skill` adds the `/new-salash-tab` skill to `~/.claude/skills` (or
`$CLAUDE_CONFIG_DIR`). In a Claude tab, `/new-salash-tab plan <task>` writes a handoff brief
from the conversation and opens a new Claude tab in the same issue, started in that mode
with the brief as its first prompt. It's like `/new-claude-tmux`, but without tmux.

## CLI

```
office [folder]        # start the folder's office in the background and attach
office --attach [folder]  # attach to the folder's running office
office new <title>     # create a local issue note
office sync            # pull GitHub/Jira issues and create missing notes in todo/
office list
office install-hooks
office tab new [--issue ID] [--kind claude|shell|notes] [--cwd DIR] [--name NAME]
               [--mode MODE] [--prompt-file FILE] [--folder DIR]   # open a tab in a running office
office install-skill   # add /new-office-tab to ~/.claude/skills
```

## Layout

```
src/
  server.rs     the folder's background server: clients, shared view, main loop
  client.rs     `office` in your terminal: starts/attaches, forwards input, prints output
  proto.rs      client <-> server messages
  instance.rs   one office per folder: .office, socket, lock, moving old state
  app.rs        state, input routing, tab lifecycle, status polling
  ui.rs         ratatui rendering (sidebar, tab bar, terminal, popups)
  term.rs       PTY + vt100 pane (portable-pty, tui-term)
  keys.rs       key event -> terminal bytes
  clipboard.rs  copy to the system clipboard and OSC 52
  vault.rs      Obsidian notes (todo/active/done/archive), slugs, frontmatter
  providers.rs  GitHub (gh), Jira REST v3
  status.rs     Claude Code hook handler and settings
  api.rs        `office tab new` requests to the running office
  worktree.rs   per-issue git worktrees
  state.rs      persisted tabs, session IDs and saved screens
  transcript.rs conversation text from Claude's transcript, for saved screens
```

## Known limits / next steps

- Tabs keep running while you're detached, but quitting office stops them. Claude tabs
  resume from their session ID afterwards, shells don't.
- Everyone attached shares one view, sized to the smallest client. Window-size options,
  per-client views and read-only clients are planned in [docs/multi-client.md](docs/multi-client.md).
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
