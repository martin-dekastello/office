# Several clients, several people

Any number of clients can attach to a folder's office: your own second terminal, or other
people sshing into a shared server and running `office --attach /srv/project`. This page covers
how that works today and which settings will come later.

## How it works now

There is **one view**, the same for every client, like several tmux clients on the same
window:

- **Same state.** Everyone sees the same selected issue, tab, focus, open prompt or dialog,
  and help screen. Anyone's keys and mouse drive it. When two people type at once, their keys
  interleave.
- **Same size: the smallest client wins.** The UI and every tab's PTY are sized to the
  smallest columns and the smallest rows among the attached clients (tmux's
  `window-size smallest`). A larger client sees that area, with `·` filling the rest, as tmux
  does. When the smallest client leaves, everyone gets bigger again. Programs in tabs (Claude,
  shells, editors) get a resize each time.
- **Shared prefix state.** If one person presses the prefix, the next key from *anyone*
  completes it.
- **Per client**: terminal setup (mouse capture, bracketed paste, keyboard protocol),
  copying, and opening notes or URLs. Text you select is copied by your own client (OSC 52,
  plus `pbcopy`/`wl-copy`/… where it runs), so it reaches your terminal, even over ssh. `o`
  (Obsidian) and `w` (browser) open on the machine your client runs on. Over ssh that is the
  server, so they only help when you sit at it.
- **Bell**: every client hears it.
- **Detach** (`D`) affects only the client that pressed it. **Quit** (`q`) ends office for
  everyone, and the confirmation says so.

## Who can attach, and as whom

- The socket's permissions follow `<folder>/.office`. The owner can always attach. Group
  members can if they may write to `.office` (group `w` and `x`), and anyone can if everyone
  may. Being able to only enter it isn't enough: `.office` is created with the usual umask, so
  it is often `755`, and that must not hand out a shell. To share with a team:

  ```sh
  chgrp -R devs /srv/project/.office && chmod -R g+rwX /srv/project/.office
  ```

  The socket's mode is set when the server starts, so restart office after changing this.
- **Everyone attached acts as the user who started the server.** Claude, shells and worktree
  scripts all run as that user, with that user's credentials (`gh` login, `$JIRA_API_TOKEN`,
  Claude account, ssh agent). Being able to attach is the same as being able to run commands
  as that user. Grant it accordingly. tmux works the same way, which is why it added
  `server-access`.
- The server and its tabs keep the environment of the `office` that started them: `PATH`,
  `SSH_AUTH_SOCK`, tokens. Attaching later doesn't change it, much as tmux behaves without
  `update-environment`.
- Config: the server reads the starting user's `~/.config/office/config.toml` plus the
  folder's `.office/config.toml`. On a shared server, put team settings (vault, GitHub
  sources, prefix) in the folder's file. Commands, repo paths and Jira stay in the starting
  user's own config; the folder's file can't set them.
- Windows: the named pipe is created with the default security descriptor, so other users
  on the same machine may be able to open it. Treat a Windows machine as single-user until
  the listener sets an owner-only descriptor (interprocess's `SecurityDescriptor::deserialize`
  with SDDL `D:P(A;;GA;;;OW)`, passed through `ListenerOptionsExt::security_descriptor`).

## Settings for later

None of these exist yet. They're written down here so later work fits together.

### `window_size`

How the shared size is chosen, after tmux's `window-size`:

```toml
window_size = "smallest"   # now: everyone sees all of it; larger clients get dots
# "largest"  biggest client; smaller ones see the top-left part and have to scroll/pan
# "latest"   the client that last typed sets the size
# "manual"   fixed, with window_cols / window_rows
```

`latest` suits a team best, as the person typing gets a full screen. It's what tmux has used by
default since 2.9. It needs a resize per change of typist, which Claude Code redraws well but
which can scramble a shell that's printing.

### `aggressive_resize`

Today every tab of the visible issue is resized to the shared size, while background issues
keep their last size until someone looks at them. tmux's `aggressive-resize` sizes a window
only to the clients *currently looking at it*. office would only use it with per-client views
(below), where different people can be on different issues.

### Per-client views (`mirror = false`)

Each client gets its own selected issue, tab, focus, prompt and prefix state, like zellij with
`mirror_session false`. It needs:

- `App` split into shared state (issues, tabs, PTYs) and per-client view state (`selected`,
  `view`, `focus`, `mode`, hit areas, scroll positions).
- One `ui::draw` per client instead of one shared frame.
- A rule for a PTY that two clients look at in different sizes. Use the smallest of those
  clients, as `aggressive_resize` would.

### Read-only clients

`office --attach --read-only`, like `tmux attach -r`. The server would ignore input from such
a client, except for detaching. Useful for watching or demos.

### An access list

Users or groups allowed to attach, and whether they are read-only, checked against the socket
peer's credentials (`interprocess` can read them). tmux 3.3 does this with `server-access`. It
gives finer control than file permissions.

### Seeing who's here

Show the attached clients (user, terminal size, idle time) in the status bar or with
`office clients`, and let the owner disconnect one, like tmux's `list-clients` /
`detach-client -t`.
