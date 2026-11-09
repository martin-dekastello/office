# Server and clients

office runs one background **server per folder**. The server owns the tabs (their PTYs and
processes), the terminal emulation and the UI state. What you run in your terminal is a
**client**: it passes your keys and mouse to the server and prints what the server draws.
Closing the client, or losing the ssh connection, leaves the server and every tab running.

```
 terminal ── office (client) ──┐                     ┌── claude (PTY)
                               │  <folder>/.office/  │
 terminal ── office (client) ──┼── office.sock ──────┼── office server <folder>
                               │                     │
 ssh ─────── office (client) ──┘                     └── $SHELL (PTY) …
```

## Starting, attaching, detaching

| | |
|---|---|
| `office [folder]` | Starts the folder's server in the background and attaches. Fails if one is already running there. |
| `office --attach [folder]` | Attaches to the running server. Fails if there is none. |
| `D` in the sidebar, `prefix D` anywhere | Detaches this client. The server keeps going. |
| `q` | Quits the server for everyone: tabs stop, Claude tabs are saved and resume next time. |

The folder defaults to the current directory. `office` refusing to start a second server is
on purpose: two servers for one folder would fight over its state and its tabs. tmux uses
`d` for detach, but here `d` already moves an issue to Done.

## The `.office` folder

```
<folder>/.office/
  .gitignore         "*": the folder ignores itself, so a repo's own .gitignore stays untouched
  config.toml        optional; overrides ~/.config/office/config.toml key by key
  office.sock        the socket clients connect to
  server.lock        locked by the running server
  server.log         the server's stderr
  state.json         tabs, Claude session ids, worktrees, default directory
  snapshots/         saved screens of suspended tabs
  status/            Claude hook status, one file per tab
  requests/          `office tab new` requests
  claude-hooks.json  the --settings file Claude tabs get
```

The folder's `config.toml` is merged over the global config. Tables merge key by key, and
anything else replaces the global value, arrays included: a folder that lists `[[github]]`
sources gets only those, not the global ones as well.

Every tab gets `OFFICE_DATA=<folder>/.office`. That is how `office hook` and `office tab new`
running inside a tab reach their own office and not another folder's. Outside a tab, `office
tab new`, `new`, `sync`, `list` and `paths` use the nearest folder above the current directory
that has a `.office` (as git finds `.git`), or `--folder` for `tab new`.

If `<folder>/.office/office.sock` would be longer than a Unix socket path allows (about 100
bytes), the socket goes to `/tmp/office-<hash of the folder>.sock` instead. Every user works
out the same name.

## Moving from the single global office

The first time a server starts in a folder that has no `state.json` yet, it moves `state.json`,
`snapshots/` and `status/` out of the old global data dir (`office paths` showed it as `data`)
into that folder's `.office`. It leaves a `migrated` file there naming the folder, and does
this only once. Tab directories and worktrees are stored as absolute paths, so Claude sessions
resume from the directory they started in, exactly as before.

To undo it, quit office, then move `<folder>/.office/state.json` (with `snapshots/` and
`status/`) back to the global data dir and delete its `migrated` file.

## Inside the server

- `office server <folder>` (hidden) is the server. `office` starts it again from its own binary
  with `setsid` on Unix, or `DETACHED_PROCESS` on Windows, with no stdin or stdout and stderr
  going to `server.log`. It is a new process rather than a fork, because forking a process
  that already has threads isn't safe. zellij starts its server the same way.
- Holding `server.lock` (std `File::try_lock`) makes the server the only one for its folder.
  Any socket file it finds is stale, so it removes it and binds a new one.
- An accept thread gives each client two threads: one reads its input onto the main loop's
  channel, the other writes its output from a bounded queue. The main loop is the loop the TUI
  always had: `tick`, draw, wait up to 33 ms for input. It now reads input from that channel
  instead of the terminal.
- **Drawing**: `ui::draw` runs once per frame into an off-screen buffer (ratatui
  `TestBackend`) at the shared size. Each client has a ratatui `Terminal` with a fixed viewport
  at its own size. That terminal copies the shared buffer in, dots out whatever lies beyond it
  as tmux does, and its diff is the only part sent. A client that falls more than
  16 messages behind loses its queued output and gets a full redraw once it catches up, so a
  slow ssh link never holds up the server or the other clients.
- The bell is sent to every client. Copying (OSC 52 plus `pbcopy`/`wl-copy`/…) and opening a
  note or URL go back to the client whose input caused them and run there.
- When the server quits, it saves the sessions as before, tells every client, removes the
  socket and stops the tabs.

## Protocol (`src/proto.rs`)

Each frame is a tag byte, a big-endian `u32` length, then the payload.

| tag | direction | payload |
|---|---|---|
| 1 `Hello` | client → server | cols, rows (`u16` each) |
| 2 `Event` | client → server | a crossterm `Event` as JSON (key, mouse, paste, resize) |
| 10 `Welcome` | server → client | 1 byte: capture the mouse or not |
| 11 `Output` | server → client | bytes to print |
| 12 `Clipboard` | server → client | text to copy |
| 13 `Open` | server → client | URL to open |
| 14 `Exit` | server → client | why: `detached`, `office quit` |

## Prior art

- **tmux** ([tmux/tmux](https://github.com/tmux/tmux)) is the model: a server holding the
  sessions, clients on a Unix socket in `/tmp/tmux-UID/` (`server.c`, `client.c`,
  `server-client.c`). It daemonizes with `proc_fork_and_daemon` (`proc.c`), sizes windows with
  the `window-size` option, and shares between users with `-S` and `server-access`. One
  difference: a tmux client passes its terminal's file descriptors to the server, which then
  writes to your tty directly. office streams the rendered bytes over the socket instead, which
  fits crossterm and ratatui, and works the same on Windows named pipes.
- **zellij** ([zellij-org/zellij](https://github.com/zellij-org/zellij)) is closest in how it
  is built: it starts its server by running its own binary again, sends serialized messages
  over IPC (`zellij-utils/src/ipc.rs`), the server renders, and clients print. Its
  `mirror_session` option chooses between one view shared by every client and a view per
  client.
- **dtach** ([crigler/dtach](https://github.com/crigler/dtach)) and **abduco**
  ([martanne/abduco](https://github.com/martanne/abduco)) do detach and nothing else.
- Crates and std: [`interprocess`](https://github.com/kotauskas/interprocess) for local
  sockets (Unix domain sockets, Windows named pipes), crossterm's `serde` feature so events can
  cross the socket, and std `File::try_lock`. The [`daemonize`](https://github.com/knsd/daemonize)
  crate forks, which is why it isn't used.
- The folder that ignores itself is borrowed from pytest's `.pytest_cache/.gitignore`.
