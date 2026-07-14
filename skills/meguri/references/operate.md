# Operating meguri day to day

Assumes meguri is already set up for this repo — see `references/setup.md` if not.

## Queue work

- **GitHub-backed project:** label an issue `meguri:ready` (or `meguri:plan` for the
  spec-first flow) and it's picked up on the next `meguri watch` poll. Labels form two axes —
  phase (`meguri:plan` → `meguri:speccing` → `meguri:ready` → `meguri:implementing`) and
  ball-in-court (`meguri:working` / `meguri:needs-human` / `meguri:hold`, layered on top of
  the phase). An unlabeled issue means untriaged — see the README's Labels section for the
  full table if you need it.
- **Local-mode project:** `meguri add "<description>"` queues a task for the worker
  (`--file task.md` reads title + body from a file). `meguri tasks` lists open ones. The
  planner doesn't run in local mode yet, so avoid `meguri add --plan "..."` here — the task
  would just sit queued/dormant instead of being picked up.

## Watch what's running

- `meguri ps` — runs, interaction state, panes, at a glance.
- `meguri top` — tiled dashboard workspace of every active pane.
- `meguri logs <run>` — event trail plus a live pane tail.
- `meguri attach <issue>` — jump into that issue's live agent pane (or pass a run id); add
  `--review` for the spec reviewer's independent pane.

## Step in

The agent's real TUI is always live — attaching, reading, or typing never breaks the loop;
only durable signals (the result file, git state, labels) drive meguri's own decisions.

- `meguri pause <run>` / `meguri resume <run>` — stop/resume prompt injection without killing
  the pane.
- `meguri takeover <run>` — park the orchestrator and drive the session yourself;
  `meguri handback <run>` — hand it back with your work in context.
- `meguri stop <run>` — kill the pane, release the claim, cancel the run.

## Triage what needs a human

Filter for `meguri:needs-human` (issues, or `meguri tasks` / `meguri ps` in local mode) — that
is the whole human TODO list; the phase label underneath still shows *where* it stalled (spec
vs. implementation). Read the comment meguri left explaining why, act on it, then re-label or
re-run to pick the work back up.

## Clean up

`meguri prune` reclaims panes and worktrees of already-closed issues on demand (this also
happens automatically while `meguri watch` is running). Use `--dry-run` to preview, `--force`
to skip confirmation.

## Anything not covered here

`meguri --help`, `meguri doctor`, and the README (https://github.com/kkato1030/meguri) are the
source of truth for anything this doesn't cover — meguri moves fast pre-1.0, so don't assume
this file is exhaustive.
