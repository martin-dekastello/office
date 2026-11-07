---
name: new-office-tab
description: Open a new Claude Code tab in the same office issue, seeded with a handoff brief of the relevant context from this conversation and started in a chosen permission mode (plan, auto, acceptEdits, manual, dontAsk, bypass). Use when the user says "/new-office-tab plan …", "spin off a new claude tab for …", or wants to hand a task to a parallel session while running inside office.
---

# Spin off a new Claude tab in office

`/new-office-tab [mode] <task…>` opens a new Claude tab in the office issue this session
belongs to. The tab runs in the given permission mode, and its first prompt is a handoff
brief: the relevant context from this conversation plus the task. You are the *calling*
session. The new one starts with no context, so the brief is everything it knows. The tab
opens in the background, so the user stays where they are.

## 1. Check you're inside office

Run `echo "$OFFICE_ISSUE"`. If it's empty, this session isn't running in an office tab.
Tell the user so, suggest `/new-claude-tmux`, and stop.

## 2. Parse mode and task

If the first word matches a mode (case-insensitive), it is the mode. Otherwise the mode is
`auto` and the whole argument is the task.

| word(s) | `--mode` |
|---|---|
| `plan` | `plan` |
| `auto` | `auto` |
| `accept`, `edits`, `acceptedits` | `acceptEdits` |
| `manual`, `default`, `ask` | `manual` |
| `dontask` | `dontAsk` |
| `bypass`, `yolo` | `bypassPermissions` |

Use `bypassPermissions` only when the user asked for it explicitly. If the task is empty,
ask the user for it before doing anything else.

## 3. Choose the working directory

Usually this is your own working directory, which is the issue's worktree. Pick another one
when this conversation established that the task belongs elsewhere, for example a different
repo or subproject. The directory must be an absolute path that exists. If two places are
equally plausible, ask the user one short question. Gather repo state from *that* directory
(`git -C <path> status --short`, `git -C <path> branch --show-current`,
`git -C <path> log --oneline -5`), not from your cwd.

## 4. Write the handoff brief

Write it to `${TMPDIR:-/tmp}/new-office-tab/<YYYYmmdd-HHMMSS>-<slug>.md`, creating the
directory if needed. From this conversation, distil only what the new task needs:

```markdown
# Handoff: <short title>

## Task
> <the user's words, verbatim>

<one-line restatement of what done looks like>

## Where things stand
You are running in `<path>` on branch `<branch>`, in a tab of office issue `<$OFFICE_ISSUE>`.
The issue's note is `$OFFICE_NOTE`.
<git status summary; relevant recent commits>

## Relevant context
- <decisions made and constraints/preferences the user stated>
- <files/symbols already identified, as path:line>
- <findings that save re-investigation; open questions>

## Out of scope / don't redo
- <work the original session owns or already finished>

<closing line — see below>
```

Rules:
- Keep it concise, about 150 lines at most: facts, not narrative. Name files and symbols precisely.
- Include only what serves this task and skip unrelated threads of the conversation. If the
  conversation has nothing relevant, say so and keep just the repo state.
- Never include secrets, tokens or credentials.
- Pick the closing line by mode. For `plan`: "Investigate and produce a plan before editing
  anything." For any other mode: "Start working on the task."
- The brief must start with `#`, not `-`.

## 5. Open the tab

```sh
"${OFFICE_BIN:-office}" tab new --issue "$OFFICE_ISSUE" --mode <mode> --name <name> \
  --cwd <path> --prompt-file <brief-file>
```

`<name>` is a short kebab-case slug of the task, 24 characters at most. It names both the tab
and the Claude session. office validates the mode and directory, and it passes the brief to
Claude as one argument, with no shell in between. On success the command prints
`tab:<id> issue:<id>`. On failure it prints the reason, such as "office doesn't seem to be
running". Report that to the user and don't retry with a different mechanism.

## 6. Report

In one or two lines, give the tab name, mode, working directory and brief path, and say how
to get there: the tab is at the end of this issue's tab bar (`prefix l`, or `prefix` plus its
number). Don't paste the brief back into the chat.
