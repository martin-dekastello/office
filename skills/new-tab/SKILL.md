---
name: new-tab
description: Open a new Claude Code tab in the same salash issue, seeded with a handoff brief of the relevant context from this conversation and started in a chosen permission mode (plan, auto, acceptEdits, manual, dontAsk, bypass). Use when the user says "/salash:new-tab plan …", "spin off a new claude tab for …", or wants to hand a task to a parallel session while running inside salash.
---

# Spin off a new Claude tab in salash

`/salash:new-tab [mode] <request…>` opens a new Claude tab in the salash issue this
session belongs to. The tab runs in the given permission mode, and its first prompt is a
handoff brief: the task, interpreted from the user's request, plus the context from this
conversation that it needs. You are the *calling* session. The new one starts with no
context, so the brief is everything it knows. The tab opens in the background, so the user
stays where they are.

## 1. Check you're inside salash

Run `echo "$SALASH_ISSUE"`. If it's empty, this session isn't running in a salash tab.
Tell the user so, suggest `/new-claude-tmux`, and stop.

## 2. Parse mode and request

If the first word matches a mode (case-insensitive), it is the mode. Otherwise the mode is
`auto` and the whole argument is the request.

| word(s) | `--mode` |
|---|---|
| `plan` | `plan` |
| `auto` | `auto` |
| `accept`, `edits`, `acceptedits` | `acceptEdits` |
| `manual`, `default`, `ask` | `manual` |
| `dontask` | `dontAsk` |
| `bypass`, `yolo` | `bypassPermissions` |

Use `bypassPermissions` only when the user asked for it explicitly. If the request is empty,
ask the user for it before doing anything else.

The request is the user's shorthand, written for you, who know the conversation. It is not
the task text for the new tab.

## 3. Interpret the request

Read the request against this conversation and work out the task it stands for:

- Resolve every reference to something the new session can't see: "this", "it", "the bug",
  "that file", "option B", "what we discussed". Name the concrete thing: files as
  `path:line`, symbols, commands, error messages, the decision that was made.
- Work out what the user wants done and what done looks like. Carry over constraints the
  conversation implies, such as how the user wants work delivered (worktree, PR, don't merge)
  or approaches they already rejected.
- Don't widen the scope. Interpret the request, don't add tasks the user didn't ask for.
- If the request is ambiguous in a way the conversation doesn't settle, ask the user one short
  question rather than guess.

## 4. Choose the working directory

Usually this is your own working directory, which is the issue's worktree. Pick another one
when this conversation established that the task belongs elsewhere, for example a different
repo or subproject. The directory must be an absolute path that exists. If two places are
equally plausible, ask the user one short question. Gather repo state from *that* directory
(`git -C <path> status --short`, `git -C <path> branch --show-current`,
`git -C <path> log --oneline -5`), not from your cwd.

## 5. Write the handoff brief

Write it to `$SALASH_DATA/briefs/<YYYYmmdd-HHMMSS>-<slug>.md`, creating the directory if
needed. That is salash's own data folder, not a shared temp dir.

```markdown
# Handoff: <short title>

## Task
<the task from step 3 as a self-contained instruction: what to do, to what, and why, with
every reference resolved>

**Done when:** <concrete acceptance criteria>

## Where things stand
You are running in `<path>` on branch `<branch>`, in a tab of salash issue `<$SALASH_ISSUE>`.
The issue's note is `$SALASH_NOTE`.
<git status summary; relevant recent commits>

## Relevant context
- <what the task statement leans on: the bug's symptoms and where it was found, the approach
  that was chosen and why>
- <decisions made and constraints/preferences the user stated>
- <files/symbols already identified, as path:line>
- <findings that save re-investigation; open questions>

## Out of scope / don't redo
- <work the original session owns or already finished>

<closing line — see below>
```

Rules:
- The new session must be able to act on the brief without asking what something refers to.
  Never copy the user's request in as the task.
- Keep it concise, about 150 lines at most: facts, not narrative. Name files and symbols precisely.
- Include only what serves this task and skip unrelated threads of the conversation. If the
  conversation has nothing relevant, say so and keep just the repo state.
- Never include secrets, tokens or credentials.
- Pick the closing line by mode. For `plan`: "Investigate and produce a plan before editing
  anything." For any other mode: "Start working on the task."
- The brief must start with `#`, not `-`.

## 6. Open the tab

```sh
"${SALASH_BIN:-salash}" tab new --issue "$SALASH_ISSUE" --mode <mode> --name <name> \
  --cwd <path> --prompt-file <brief-file>
```

`<name>` is a short kebab-case slug of the task, 24 characters at most. It names both the tab
and the Claude session. salash validates the mode and directory, and it passes the brief to
Claude as one argument, with no shell in between. On success the command prints
`tab:<id> issue:<id>`. On failure it prints the reason, such as "salash doesn't seem to be
running". Report that to the user and don't retry with a different mechanism.

## 7. Report

In one or two lines, give the tab name, mode, working directory and brief path, and say how
to get there: the tab is at the end of this issue's tab bar (`prefix l`, or `prefix` plus its
number). Don't paste the brief back into the chat.
