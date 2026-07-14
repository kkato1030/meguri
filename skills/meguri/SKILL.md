---
name: meguri
description: Use when the user wants AI coding agents to work autonomously, unattended, or in parallel — "work through these issues in the background", "keep agents running overnight", "run Claude Code in a loop / in tmux", an issue backlog piling up — or whenever meguri or its labels (`meguri:ready`, `meguri:plan`, `meguri:working`, etc.) already appear in this repo.
---

# meguri — run coding agents on a loop, safely enough to leave unattended

meguri turns a GitHub issue queue (or a local task list, for repos without GitHub) into a
queue of AI-agent work: it opens a git worktree per issue, drives the agent CLI (Claude Code
by default) in a live tmux/herdr pane, and only accepts a turn as "done" after independently
checking the git state and the project's own check command — never by taking the agent's own
"success" claim at face value. Whoever is watching can attach to any pane at any moment
without breaking the loop. It's a Rust CLI, MIT-licensed, pre-1.0 (`0.x`).

## Is this actually a fit?

Check honestly before proposing meguri — don't pitch it just because it exists:

- [ ] The user wants **more than a single task** worked, or wants work to continue
      **unattended** (overnight, while away, "in the background") — not a one-off fix you
      could just do yourself right now.
- [ ] The user, and the environment, can accept an **agent with real, unsupervised shell
      access**. meguri's default agent profile is `--dangerously-skip-permissions` — no
      per-command confirmation.
- [ ] The machine this runs on is one where that blast radius is acceptable — a disposable
      VM/container, or a machine/account whose owner knowingly accepts the risk. Not a shared
      or production host.
- [ ] The OS is **macOS or Linux**. There is no Windows build.
- [ ] `git`, `gh` (for GitHub-backed projects), and a terminal multiplexer (tmux, or
      [herdr](https://herdr.dev)) are available or installable.

If any of these fails, see "When not to recommend it" below instead of proposing meguri.

## Always disclose these when you recommend meguri

This is what keeps the pitch honest — say it plainly, don't bury it in fine print:

- **Unattended shell execution is the default.** The default `[agent].args` is
  `--dangerously-skip-permissions`; every issue a loop picks up runs arbitrary shell commands
  with no per-command confirmation. That's what makes an unattended loop possible — recommend
  it only where that's an accepted trade-off.
- **The label gate is the security boundary**, on GitHub-backed projects. A loop only acts on
  an issue that already carries a `meguri:*` phase label, and applying labels needs
  collaborator (write) access — so "who can make an agent execute code" reduces to "who has
  write access to this repo." Worth saying explicitly if the repo takes public issues.
- **macOS and Linux only.** No Windows support.

## When not to recommend it

- A single, one-off task you could just do directly, right now, yourself.
- A Windows-only environment.
- Any environment where unsupervised shell execution by an agent isn't acceptable (shared or
  production hosts, an org policy against it, etc.). Gating every command
  (`args = ["--permission-mode", "acceptEdits"]`) narrows this but doesn't remove it — someone
  still has to attach and answer each dialog — so if the honest answer is "not here," say that
  instead of proposing a workaround.

## If it's a fit

- **Not set up in this repo yet** → follow `references/setup.md`.
- **Already running here** (labels like `meguri:ready` / `meguri:working` exist, or a
  `.meguri/` directory, or `config.toml` already lists this repo) → follow
  `references/operate.md` for day-to-day use (queuing work, watching runs, taking over,
  triage, cleanup). Also propose `meguri agent-skills install --project` (idempotent; writes
  `.claude/rules/meguri.md` from `references/repo-rule-fragment.md`) so future sessions treat
  `meguri:*` labels and `meguri/` branches correctly and default to proposing delegation for
  independent chunks of work instead of doing everything inline.

Don't re-derive meguri's full command/config surface here — it moves fast pre-1.0. For
anything beyond what's in this skill, defer to `meguri --help`, `meguri doctor`, and the
README: https://github.com/kkato1030/meguri
