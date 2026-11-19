# salash

Issues on the left, Claude Code tabs on the right, one Obsidian note per issue.

The name comes from the Slovak word *salaš*, a shepherd's hut up in the mountain pastures.

```
 salash  ▲ 1                            │ ◐ 1:Refactor tile cache   ● 2:Fix loader   $ 3:shell
  To-do 5  Active 3  Done 1  Archive 0  │
                                        │  (live Claude Code session for the selected issue)
 ◐ TS-42                             3▤ │
   Přidat mapu dlaždic pro fotky        │
 ▲ PROJ-118                          1▤ │
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
  `◐` yellow = working (animated) · `▲` amber = needs you (permission/question) ·
  `●` green = finished and not seen yet (a tab counts as seen once it has been on view for
  `seen_after_secs`, default 5) · `○` idle · `✕` exited · `–` suspended.
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
  with `worktree_command`, or set `worktrees = false` to manage checkouts yourself: tabs then
  start in the issue's repo.
- **Sessions can be stopped and picked up again.** Press `z` to suspend an inactive Claude
  tab: its process stops and its RAM is freed, but you can still scroll its saved screen,
  and `⏎` continues it with `claude --resume <session>`. A Claude tab that was running when
  you quit salash ends up the same way. Closing one with `x` keeps it too: `R` reopens it.
- **Tabs close when their process exits**: quit Claude, `exit` a shell or close the note's
  editor and the tab goes away. A Claude session that exits can still be brought back with
  `R`. Only a Claude that fails to start stays open, so you can read why.
- **One salash per folder, running in the background**, like a tmux server. `salash ~/work`
  starts it and attaches. Closing the terminal, or `D` to detach, leaves every tab running, and
  `salash --attach ~/work` picks it up again, from another terminal or over ssh. Several people
  can attach at once and share one view. See [docs/architecture.md](docs/architecture.md)
  and [docs/multi-client.md](docs/multi-client.md).
- **GitHub (via `gh`) + Jira Cloud + local issues**, behind one `Provider` trait.
- **Light and dark.** With `theme = "auto"` (the default) salash asks the terminal for its
  background colour at startup and matches it; set `"dark"` or `"light"` to choose. Programs in
  tabs that ask for the background (Claude Code, nvim) get the same answer.
- **macOS and Linux.** Windows isn't supported yet. It's a single binary with its own
  terminal emulation, so it doesn't need tmux.

## Install

Download the binary for your platform from
[Releases](https://github.com/martin-dekastello/office/releases): macOS (Apple silicon),
Linux arm64 or Linux x86_64. The Linux builds are static (musl), so they run on any distro.

```sh
tar xzf salash-v0.0.2-<target>.tar.gz
mv salash ~/.local/bin/          # or anywhere on PATH
xattr -d com.apple.quarantine ~/.local/bin/salash   # macOS only: the binary isn't signed
```

Or build from source:

```sh
cargo install --path .
```

Then:

```sh
salash [folder]      # start salash in a folder (default: here); first run writes the config
salash paths         # where config, data, notes and worktrees live
```

Each folder's salash keeps its tabs, sessions and socket in `<folder>/.salash`, which ignores
itself in git. The first salash you start takes over the tabs and sessions saved by older
versions of salash.

Requirements: `claude` on PATH, `git` for worktrees, `gh` for GitHub sources (with a token
from salash, see [Tokens](#tokens), or its own login), a Jira API token for Jira.

## Config

`~/.config/salash/config.toml` on Linux, `~/Library/Application Support/salash/config.toml`
on macOS. Override with `$SALASH_CONFIG`.
A folder's `.salash/config.toml` overrides some of it for that folder's salash: `vault`,
`notes_dir`, `prefix`, `local_prefix`, `bell`, `mouse`, `worktrees` and `[[github]]` sources
(without `path` and `token_env`; for a repo your own config also lists, yours are kept). For
example, give a team folder its own vault and sources. Commands, repo paths, `token_env` and Jira
only come from your own config, because a repo you clone can ship a `.salash/config.toml`.
salash ignores the rest and says so when it starts.

```toml
vault = "~/Documents/Obsidian"
notes_dir = "Issues"
worktrees = true                # false = tabs start in the issue's repo, no worktree
worktrees_dir = "~/worktrees"
prefix = "ctrl-a"
theme = "auto"                  # auto | dark | light
seen_after_secs = 5             # a finished tab stays unread until shown this long; 0 = at once

[[github]]
repo = "johan/transport-sim"
path = "~/code/transport-sim"   # local clone used for worktrees
id_prefix = "TS"                # -> TS-42-<slug>.md
search = "assignee:@me"
token_env = "WORK_GH_TOKEN"     # optional: env var holding the token (never the token itself)
# worktrees = false             # this source only; overrides the global setting

[[jira]]
url = "https://yourcompany.atlassian.net"
email = "you@example.com"
token_env = "JIRA_API_TOKEN"
jql = "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
path = "~/code/backend"
```

### Tokens

Token values never go into `config.toml`. For each source salash looks, in this order, at:

1. the env var named by the source's `token_env`, if set;
2. the OS keychain: service `salash`, account `github:owner/repo` or
   `jira:https://yourcompany.atlassian.net` (macOS Keychain; on Linux the Secret Service,
   i.e. GNOME Keyring or KWallet);
3. `$GH_TOKEN` / `$JIRA_API_TOKEN` (`$GH_ENTERPRISE_TOKEN` for a GitHub Enterprise Server
   repo, `repo = "ghe.corp/owner/repo"`);
4. GitHub only: whatever `gh` finds itself (`$GITHUB_TOKEN`, then `gh auth login`), as before.

A source with its own `token_env` stops after the keychain: if neither has a token, its sync
fails instead of using `$GH_TOKEN`, `$JIRA_API_TOKEN` or `gh`'s login, which are likely another
account's.

A GitHub token goes to `gh` as `GH_TOKEN` (`GH_ENTERPRISE_TOKEN` for Enterprise Server) in that
one command's environment, never on its command line. Tabs don't get it.

```
salash token set github:owner/repo      # prompts without echo, or reads stdin
salash token set jira:https://yourcompany.atlassian.net < token.txt
salash token status                     # where each source's token comes from (never the token)
salash token get github:owner/repo      # print it, e.g. for GH_TOKEN=$(...)
salash token delete github:owner/repo
```

The keychain is read on every sync, so `salash token set` applies at the next `S` without a
restart. Env vars are different: a running salash keeps the environment it started with.
A keychain that can't be read (no Secret Service, a locked keychain over SSH, no answer within
10 seconds, after which it is skipped until that read answers) is skipped quietly and the env
vars still work; `salash token status` says why. `salash token set` and `delete` give up after
60 seconds. What `salash token get` prints in a terminal stays in its scrollback and, in a
salash tab, in the saved screen under `.salash/snapshots/`; it warns you when that happens.

**Give tokens the least they need.**

- **GitHub:** a fine-grained token with **Issues: read** and **Metadata: read**, for only the
  repositories you sync ("Only select repositories"). [This link](https://github.com/settings/personal-access-tokens/new?name=salash&description=salash+issue+sync&issues=read&metadata=read)
  opens the form with those filled in; add `&target_name=<org>` for an organization's repos (the
  error salash shows when `gh` fails has that link for the source's owner). Organizations may
  have to allow or approve fine-grained tokens. A fine-grained token's owner must be you or an
  organization you belong to; for a repo you only collaborate on, use a classic token
  ([link](https://github.com/settings/tokens/new?description=salash&scopes=repo)). It needs the
  `repo` scope, which can read and write all your repositories, so prefer fine-grained otherwise.
- **Jira:** at <https://id.atlassian.com/manage-profile/security/api-tokens> choose **Create API
  token with scopes**, name it, pick an expiry (1–365 days), pick the **Jira** app and only the
  **`read:jira-work`** scope, then copy the token. Atlassian has no link that fills this in. A
  scoped token only works through `api.atlassian.com/ex/jira/<cloudId>`; the site answers it
  with no issues and `X-Seraph-LoginReason: AUTHENTICATED_FAILED`. salash takes that (or a 401)
  as a refusal, reads the site's cloudId from `<site>/_edge/tenant_info`, retries there and uses
  the gateway directly from then on. Classic tokens keep working against the site. The `url`
  has to be `https://`.

On macOS, a salash binary that changed (an upgrade or a rebuild) may get asked "salash wants to
use your confidential information stored in … in your keychain"; choose Always Allow. On Linux
the Secret Service must be running when salash starts; a keyring daemon started later is only
seen after restarting salash.

### Turning worktrees off

With `worktrees = false`, salash never runs `git worktree add` or `worktree_command`: an issue's
tabs start in its repo (`repo_path:` from the note, else the source's `path`, else
`local_repo`), and issues with no repo start in the default directory as before. Several issues
on the same repo then share that one checkout, so switching branches is up to you. Set
`worktrees` on a `[[github]]` or `[[jira]]` source to override the global value for that
source only.

A fresh config is written with `worktrees = false`; a config without the key keeps them on, so
upgrading changes nothing. Turning the setting off only stops *new* worktrees: an issue whose
worktree salash already made keeps using it, and every tab resumes in the directory it started
in, whatever the setting is now. Changes take effect when salash restarts.

### Your own worktree script

If a plain `git worktree add` isn't how you set up a checkout, set `worktree_command` (it's
ignored when worktrees are off):

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

Issues with no repo start their tabs in the default directory, which is salash's folder
until you change it. Press `p` to set it, in the same dialog as `C`. A tab keeps the directory
it started in, so changing the default only affects new tabs, and sessions always resume where
they began.

## Keys

The prefix (default `Ctrl-a`, tmux-style) works everywhere. Press it twice to send it
to the terminal.

Keys go to either the sidebar or the terminal, and you can tell which at a glance:

- **Terminal focused:** a heavy frame (`┃` `━`) in the accent colour runs along the
  terminal's left edge and under the tab bar, the active tab has a `▌` marker, and the
  issue list is dimmed (its header and key hints stay lit).
- **Sidebar focused:** the frame is thin (`│` `─`), the selected issue has a `▌` bar, and the
  tab bar, terminal and status bar are dimmed. The active tab keeps its background, without
  the marker, so you can see which tab `x`, `z` and `,` act on.

Dimming blends salash's own colours and true-colour text toward the background; text in the
terminal's default or palette colours gets the faint attribute, so your terminal fades it
with its own palette. The frame line is always there, so moving focus never resizes the
tab; clicking it moves focus to the terminal. A printable key the sidebar doesn't use is
dropped, and when there is a tab to type in, the status bar reminds you to press `Tab` or
`<prefix> s`.

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
| `D` / `q` | detach (salash keeps running) / quit salash for everyone attached |

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
a tab's `×` to close it, and `quit` at the top of the sidebar. Clicking an issue jumps into
its last-used tab when that tab has a process (running, or exited with its last screen) or
is suspended. A suspended or exited tab stays stopped until `⏎`. An issue with no started
tab only gets selected, and a click never starts a tab. To get back to the sidebar, press
`prefix s` or click a view name. Closing and quitting ask first. Inside a tab, the wheel, clicks and
drags go to the app when it handles the mouse itself, as Claude Code does with
`"tui": "fullscreen"`. Apps on the alternate screen that don't (`less`) get arrow keys from
the wheel. Anything else scrolls salash's scrollback.

In a shell tab, drag to select text. Letting go copies it, and the highlight stays until you
click or type. Hold the mouse above or below the tab to scroll while selecting. Your own
`salash` client copies it, with `pbcopy`, `clip`, or `wl-copy`/`xclip`/`xsel`, and also OSC 52 for SSH. Anywhere else, including
Claude tabs and shell apps that handle the mouse themselves, hold Shift (Option in iTerm) to
select text.

## How the Claude status works

Each tab is started with `SALASH_TAB=<uuid>`. Claude tabs are launched as
`claude --settings <data>/claude-hooks.json --plugin-dir <data>/plugin`. The plugin holds
salash's skills (see below) and is always passed. The settings file registers `salash hook` for
`SessionStart`, `UserPromptSubmit`, `Pre/PostToolUse`, `PermissionRequest`,
`Notification`, `Stop` and `SessionEnd`. The hook writes `<data>/status/<uuid>.json`
and the UI polls it. They also get `--mcp-config=<data>/claude-mcp.json` (see below). The hook
does nothing when `SALASH_TAB` isn't set, so it's safe to install globally. If you'd rather not
use `--settings`, set `inject_hooks = false` and run `salash install-hooks`. That merges the
hooks into `~/.claude/settings.json` and writes a backup first.

Every tab also gets `SALASH_ISSUE`, `SALASH_NOTE` (the note's path), `SALASH_BIN` (the
salash executable) and `SALASH_DATA` (its folder's `.salash`, where `<data>` is). You can tell
Claude "append a summary to $SALASH_NOTE".

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
for salash to answer and fails if salash isn't running. Inside a tab it goes to that tab's
salash. Elsewhere it goes to the salash of the nearest folder above with a `.salash`, or the
one given with `--folder`.

salash's own skills come as a Claude Code plugin named `salash`, built into the binary. Every
time it starts, salash writes it to `<data>/plugin`, so it always matches the salash you have,
and its Claude tabs load it with `--plugin-dir`. There is nothing to install, and Claude
sessions outside salash never see it. In a Claude tab, `/salash:new-salash-tab plan <task>`
writes a handoff brief from the conversation and opens a new Claude tab in the same issue,
started in that mode with the brief as its first prompt. It's like `/new-claude-tmux`, but
without tmux. Loading a plugin with `--plugin-dir` needs Claude Code 2.1.74 or later.

Older salash versions had `salash install-skill`. If you ran it, you can delete
`~/.claude/skills/new-salash-tab`: until you do, the old copy shows up as `/new-salash-tab`
next to `/salash:new-salash-tab`, and it doesn't change when salash is upgraded.

## Claude sees salash (MCP)

`salash mcp [folder]` is an MCP server for Claude Code. It asks the folder's running salash
about its issues, tabs, sessions and their status, and what a tab shows, and it can open tabs.
The server stays the only owner of tabs and state. `salash mcp` fails if no salash runs
there (`no salash running for <folder>; start salash first`) and never starts one.

salash's own Claude tabs get it without any setup, with `--mcp-config` next to `--settings`.
To turn that off, set `mcp = false` in your config. For Claude sessions outside salash:

```sh
claude mcp add salash -- salash mcp ~/work
```

| Tool | |
|---|---|
| `list_issues` | issues with stage, provider and URL, note, worktree and branch, the most urgent Claude status, tab count; filter by `stage` or `status`. Archive only when asked for |
| `get_issue` | one issue with its tabs and its closed tabs (which `R` reopens) |
| `list_tabs`, `get_tab` | tab id, name, kind, cwd, running/suspended/exited, and for Claude its session, status, last hook event and message, transcript and `claude --resume` command |
| `read_screen` | what a tab shows, as plain text, like `tmux capture-pane -p`; with `scrollback`, the history above. A suspended tab gives its saved screen. Capped at 2000 lines and 32 KiB |
| `open_tab` | the same as `salash tab new`: issue, kind, cwd, name, mode, prompt |

The same comes as resources: `salash://issues`, `salash://issue/{id}`, `salash://tab/{id}` and
`salash://tab/{id}/screen`. Inside a salash tab, tools without an issue or tab default to the
caller's own, and the caller's tab is marked `"self": true`.

Any Claude in a salash tab can read **every** tab's screen this way, in every issue: the same
as anyone who can attach to salash. If that's too much, set `mcp = false`. `open_tab` takes
only the modes `plan` and `manual`, or none for your default, so a session can't start another
with fewer checks than its own. Run `salash tab new --mode …` yourself for the others.

## CLI

```
salash [folder]        # start the folder's salash in the background and attach
salash --attach [folder]  # attach to the folder's running salash
salash new <title>     # create a local issue note
salash sync            # pull GitHub/Jira issues and create missing notes in todo/
salash token set|get|delete <source>   # keep a source's token in the OS keychain
salash token status    # where each source's token comes from
salash list
salash install-hooks
salash tab new [--issue ID] [--kind claude|shell|notes] [--cwd DIR] [--name NAME]
               [--mode MODE] [--prompt-file FILE] [--folder DIR]   # open a tab in a running salash
salash mcp [folder]    # MCP server on stdio for Claude Code (see above)
```

## Layout

```
src/
  server.rs     the folder's background server: clients, shared view, main loop
  client.rs     `salash` in your terminal: starts/attaches, forwards input, prints output
  proto.rs      client <-> server messages
  query.rs      what `salash mcp` asks the server, and the answers
  mcp.rs        `salash mcp`: the MCP server (rmcp) on stdio
  instance.rs   one salash per folder: .salash, socket, lock, moving old state
  app.rs        state, input routing, tab lifecycle, status polling
  ui.rs         ratatui rendering (sidebar, tab bar, terminal, popups)
  term.rs       PTY + vt100 pane (portable-pty, tui-term)
  keys.rs       key event -> terminal bytes
  clipboard.rs  copy to the system clipboard and OSC 52
  vault.rs      Obsidian notes (todo/active/done/archive), slugs, frontmatter
  providers.rs  GitHub (gh), Jira REST v3
  status.rs     Claude Code hook handler and settings
  plugin.rs     salash's skills as a Claude Code plugin, written to <data>/plugin
  api.rs        `salash tab new` requests to the running salash
  worktree.rs   per-issue git worktrees
  state.rs      persisted tabs, session IDs and saved screens
  transcript.rs conversation text from Claude's transcript, for saved screens
  token.rs      GitHub/Jira tokens from env vars and the OS keychain, `salash token`
```

## Known limits / next steps

- Tabs keep running while you're detached, but quitting salash stops them. Claude tabs
  resume from their session ID afterwards, shells don't. `kill $(cat <folder>/.salash/server.lock)`
  stops a server from outside the same way `q` does.
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

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
