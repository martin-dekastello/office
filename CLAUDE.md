# salash

A terminal app: issues on the left, Claude Code tabs on the right, one Obsidian note per issue.
See README.md for features and config.

## Architecture reference
tmux is a close cousin of this project: a multiplexer that owns PTYs, runs its own terminal
emulation, keeps sessions alive and lets you pick them up again, and shows status per window.
When a question is about that kind of architecture (PTY handling, terminal emulation,
scrollback, resizing, detach/resume, status lines, key handling), look at how tmux solves it and
cite what you found. Use it as a reference, not as something to copy.

## Prior art
If a problem is already solved on GitHub (a crate, library, algorithm or known pattern), give the
references with links and suggest it as a solution before building our own.
