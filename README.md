# meguri（巡り）

*日本語版は [README.ja.md](README.ja.md) をご覧ください。*

**Run AI coding agents on a loop — inside your terminal multiplexer, so you can step in anytime.**

meguri is a reimplementation of the ideas in [nexu-io/looper](https://github.com/nexu-io/looper) with one deliberate architectural difference: instead of headless one-shot agent runs (`claude --print …`), meguri runs each agent as a **live interactive session in a [herdr](https://herdr.dev) or tmux pane**. The orchestrator injects prompts and waits for results, while you can attach to the pane at any moment — watch, type extra instructions, answer permission dialogs, or take over completely — without breaking the loop.

```
GitHub issue (label: meguri:ready)
        │  discover & claim (+meguri:working)
        ▼
git worktree (meguri/<issue>-<slug>-<hash>)
        │
        ▼
┌─ herdr / tmux pane ─────────────────┐
│ $ claude                            │   orchestrator: inject prompt,
│ > Read .meguri/prompt-….md and      │   wait for .meguri/result.json,
│   carry it out completely.          │   verify commits, run checks
│ ⏺ working…                          │
│                                     │◀─ you: attach anytime, type,
└─────────────────────────────────────┘   answer dialogs, take over
        │  verified commits + checks pass
        ▼
git push + PR (Closes #N) — phase swapped to meguri:implementing
```

## Why interactive sessions?

Headless loops fail opaquely: the agent hits a permission prompt, stalls, or goes down a wrong path, and all you get is a log. In meguri the agent's real TUI is always there:

- **Blocked ≠ failed.** When the agent shows a permission/question dialog, meguri flags the run `awaiting_human` and tells you how to attach — timers stop, nothing is killed.
- **Human input is never an error.** You can attach and type mid-run; the orchestrator only acts on durable signals (the result file, git state, labels), so it tolerates and absorbs your interventions.
- **Silence is nudged, not punished.** A quiet agent gets a capped number of reminder lines, then a human is paged. meguri never auto-fails a run for being slow.
- **Takeover/handback.** `meguri takeover <run>` parks the orchestrator; you drive the same session; `meguri handback <run>` resumes the loop with your work in context.

## The completion contract

meguri never parses the agent's screen to decide success. Each turn writes a prompt file into the worktree and instructs the agent to finish by writing:

```json
// .meguri/result.json
{"turn_id": "<uuid>", "status": "success | failure | needs_human", "summary": "…"}
```

Stale turn ids are ignored; results claiming success are **independently verified** (clean tree, commits ahead of the base branch, project check command passes) before meguri moves on. Verification failures come back to the agent as corrective turns.

## Security

meguri's core trade-off is unattended execution, and that's worth understanding before you point it at a repo.

- **The agent gets real shell access.** The default `[agent].args` includes `--dangerously-skip-permissions`, so once a loop picks up an issue, the agent runs arbitrary commands in its worktree — git, cargo, network calls, anything the CLI allows — with no per-command confirmation. That's what makes an unattended loop possible; it also means you should only run meguri somewhere you're fine with an agent having that level of access (a disposable VM or container, or a machine/account whose blast radius you accept). If you'd rather gate every command, set `args = ["--permission-mode", "acceptEdits"]` (see [Configuration](#configuration)) and answer dialogs by attaching to the pane.
- **Issue bodies are prompt input.** The full issue body (and comments a loop reads) is injected into the agent's prompt verbatim, so on a repo where anyone can open issues, a malicious one is a prompt-injection attempt against an agent with shell access. The mitigation is the [label gate](#labels): a loop only acts on an issue that already carries a `meguri:*` phase label (`meguri:plan` / `meguri:ready`), and applying labels needs collaborator (write) access — so "who can get an agent to execute" reduces to "who has write access to this repo," not "who can open an issue." Weigh that when granting collaborator access, and don't label untrusted issues `meguri:ready` yourself.
- **Completion is verified independently, not screen-scraped.** As described in [The completion contract](#the-completion-contract) above, meguri never takes the agent's own "success" claim at face value — it re-checks git state, commits ahead of base, and the project's `check_command` before treating a run as done. This bounds (but doesn't eliminate) the damage a compromised or misled agent can do: it can still act inside the worktree during a run, but it can't talk meguri into merging bad state just by writing "success" to the result file.

Found a vulnerability in meguri itself? See [SECURITY.md](SECURITY.md).

## Install & set up

Prereqs: `git`, [`gh`](https://cli.github.com) (authenticated), an agent CLI (`claude` by default), and a multiplexer — a running [herdr](https://herdr.dev) (recommended; native agent-state detection) or `tmux` (screen-heuristic fallback). These runtime prerequisites are the same however you install meguri — a prebuilt binary still needs `git`/`gh`/a multiplexer on the host.

Platform: core meguri (CLI, `watch`, all loops) runs on macOS and Linux; `meguri daemon install` (the `launchd` supervisor, see [Keep it running](#keep-it-running-daemon)) is macOS-only.

```bash
cargo install --path .   # or: cargo build --release
meguri init              # writes ~/.meguri/config.toml, creates the db
meguri doctor            # checks gh auth, mux, agent CLI
```

Other ways to get the binary:

- **Prebuilt binary** — download the archive for your platform (macOS arm64 / Linux x86_64) from the [latest GitHub Release](https://github.com/kkato1030/meguri/releases/latest), verify its `.sha256`, extract, and put `meguri` on your `PATH`.
- **crates.io** — `cargo install meguri` (once the crate is published; see [Status / roadmap](#status--roadmap)).

`meguri init` writes a minimal `~/.meguri/config.toml` with this project stub — fill it in:

```toml
[[projects]]
id = "myproj"
repo_path = "/abs/path/to/clone"
repo_slug = "owner/repo"
# default_branch = "main"
# check_command = "cargo test"   # recommended: meguri runs this itself
```

Everything else is optional: write a section/key only to override its default (see [Configuration](#configuration)).

## Use

```bash
# one-shot: work a single issue
meguri run --project myproj --issue 42

# or keep watching: label an issue `meguri:ready` and meguri picks it up
meguri watch

meguri ps                 # runs, interaction state, panes
meguri top                # build a dashboard workspace of tiled agent panes & attach
meguri logs <run>         # event trail + live pane tail
meguri attach <issue>     # jump into the issue's agent pane (or pass a run id)
meguri attach <issue> --review  # the spec reviewer's independent pane
meguri pause <run>        # stop injecting prompts; pane stays alive
meguri resume <run>
meguri takeover <run>     # orchestrator hands-off; you drive
meguri handback <run>
meguri stop <run>         # kill pane, release the claim, cancel
meguri prune              # reclaim panes + worktrees of closed issues (--dry-run / --force)
```

### Local mode (no GitHub, no labels)

For repos whose labels you can't (or won't) touch, run a project **entirely locally**: the task queue, claim, escalation, and completion live in meguri's sqlite instead of GitHub labels, and the deliverable is a verified local branch instead of a PR. Set `mode = "local"` — `repo_slug` becomes optional and `meguri doctor` stops requiring `gh`:

```toml
[[projects]]
id = "work"
repo_path = "/abs/path/to/repo"
mode = "local"          # "github" (default) | "local"
default_branch = "main"
check_command = "cargo test"
# deliver = "branch"    # local default: verified commits on a local branch (no push, no PR)
```

Queue and track work with the local task commands instead of labels:

```bash
meguri add "Add a --json flag to the export command"   # queue a task
meguri add --file task.md                              # first heading → title, rest → body
meguri add --plan "Design the export format"           # queue for the planner instead of the worker
meguri tasks                                           # list open tasks (needs_human highlighted)
meguri watch                                           # picks tasks up within one poll interval
```

A local run works on a `meguri/t<task-id>-<slug>-<hash>` branch; on success it leaves the verified commits there and flips the task to `done` — nothing is pushed. A failed run marks the task `needs_human` with a reason (shown by `meguri tasks` / `meguri ps`), and the next run re-claims it and clears the flag. Review the branch yourself and merge when happy (`meguri review` / `accept` land in a later phase).

> **Single machine only (through Phase 3).** Local mode's local sqlite is the *single source of truth*, so run exactly one meguri host per repo. Coordinating several hosts against a shared task queue is Phase 4 (a remote-DB `TaskSource` with leases); the vocabulary and contract are fixed in [ADR 0003](docs/adr/0003-tasksource-task-moves-run-pins.md). The `silent` mode (read issues, never write labels), `deliver = "patch"`, and `meguri review`/`accept`/`reject` are later phases too.

### Keep it running (daemon)

`meguri watch` stays in the foreground; to survive closing the shell, detach it:

```bash
meguri daemon start       # spawn watch detached (log: ~/.meguri/logs/watch.log)
meguri daemon status      # pid / mode / liveness / log location / active runs
meguri daemon logs -f     # tail the daemon log
meguri daemon restart
meguri daemon stop        # SIGTERM; kill-safe, recovery resumes on next start
```

On macOS, hand supervision to launchd so the watch also survives logout,
reboot, and crashes:

```bash
meguri daemon install --mode launchd   # generate + bootstrap a user LaunchAgent
meguri daemon uninstall                # bootout + remove the plist
```

The LaunchAgent bakes in your current `PATH` (and `HERDR_SOCKET_PATH` /
`MEGURI_HOME` if set), so `gh`, `tmux`/`herdr`, and the agent CLI resolve under
launchd; its log goes to `~/.meguri/logs/launchd.log`. Restart policy and
throttle come from the `[daemon]` config section — after changing them, re-run
`meguri daemon install`. Other platforms get an explicit error (no silent
fallback); systemd user units are planned.

Whatever the mode, the watch process holds an exclusive lock
(`~/.meguri/daemon/watch.lock`), so a second scheduler — foreground or
detached — fails loudly instead of double-driving runs.

### Labels

meguri's issue labels form **two axes** (see [ADR 0005](docs/adr/0005-issue-labels-two-axis-phase-and-ball.md)). **Axis 1 — the phase**: a meguri-engaged issue always carries exactly one phase label, from queued right through to close. **Axis 2 — the ball** (who holds it): these layer *on top of* the phase without removing it, so "who's stuck" and "where it's stuck" are both legible. The upshot: **an unlabeled issue means one thing — untriaged** (you decide whether a human or meguri takes it), and filtering on 🔴 `meguri:needs-human` gives you a clean human-TODO list.

**Axis 1 — phase** (exactly one on an engaged issue):

| label | color | meaning |
|---|---|---|
| `meguri:plan` | 🔵 blue | queued for the planner loop (opt-in spec-first flow; you apply it) |
| `meguri:speccing` | 🟣 purple | a spec PR is open (reviewing / ready detail lives on the PR) |
| `meguri:ready` | 🔵 blue | queued for the worker loop (you apply it, or set after spec approval) |
| `meguri:implementing` | 🟢 green | an implementation PR is open (CI fixing, review, awaiting merge included) |

**Axis 2 — ball / who holds it** (layered on top of the phase; none = waiting on a loop's next poll):

| label | color | meaning |
|---|---|---|
| `meguri:working` | 🟡 yellow | an agent is working on it right now (the claim) |
| `meguri:needs-human` | 🔴 red | a human needs to look; a comment explains why (the phase label stays, so you can see *whether it stalled in spec or in implementation*) |
| `meguri:hold` | ⚪ grey | intentionally paused by a human; discovery skips it |

Plus two bookkeeping / opt-in labels: `meguri:clean-report` marks the cleaner loop's per-project report issue (put `meguri:hold` on it to pause the sweep), and `meguri:automerge` opts an issue (the worker copies it onto the PR) or a PR directly into GitHub-native auto-merge (see [Auto-merge (opt-in)](#auto-merge-opt-in) below).

The **PR side** stays as it was: a spec PR carries `meguri:spec-reviewing` (awaiting review) then `meguri:spec-ready` (review passed; implementation continues) — these live on the PR, independent of the issue's phase label. CI-red and merge-readiness aren't mirrored to labels (GitHub shows them natively); a `meguri:awaiting-merge` PR label can be added later if needed.

New meguri labels are created with their scheme color automatically. If a label was created before this scheme (all generic blue), recolor it once with `gh label edit <name> --color <hex>` (e.g. `gh label edit meguri:implementing --color 0E8A16`) — meguri does not recolor existing labels on every sweep, so it never clobbers a color you set on purpose.

Discovery also honors GitHub-native issue dependencies (looper's ADR-0004): an issue *blocked by* another is skipped — silently, no label or comment — until every blocker is closed as **completed**. Blockers closed as *not planned* / *duplicate* don't count as resolved (the dependent issue awaits human re-triage), and unreadable blockers are treated as unresolved.

### Spec-first flow (opt-in)

Label an issue `meguri:plan` instead of `meguri:ready` and the **planner** loop investigates the repository and opens a *spec PR* (`Spec: <title>`) containing a single lightweight file, `docs/specs/issue-<N>.md` (acceptance criteria, files to touch, key decisions), labeled `meguri:spec-reviewing`. The spec's depth is **adaptive** ([ADR 0010](docs/adr/0010-adaptive-spec-depth.md)): the planner picks `normal` or a deeper `design` spec by uncertainty × blast radius, and any change that touches persistent state or a public contract is vetoed into carrying migration & rollback sections — the reason for the chosen depth is recorded in the spec or PR. The **spec reviewer** loop then reviews the spec PR: findings are posted as a summary comment (push fixes and it re-reviews the new head; each head is reviewed only once), and a clean review flips the label to `meguri:spec-ready` — you can also flip it yourself. The worker then continues implementation **on the same branch and PR** — the spec and the implementation merge once, together. The spec itself is disposable review scaffolding: the spec worker deletes it as part of the implementation, so `docs/specs/` never accumulates on the default branch — anything worth keeping (design decisions, domain rules) is routed to an ADR (`docs/adr/`) or a permanent domain document instead.

### Self-review (internal AI review of the diff)

The AI review of the **implementation diff** is an *internal loop* (ADR 0006): the worker reviews its own diff before the PR is ever pushed, so the review→fix ping-pong never touches GitHub. Between `validate` and `open-pr` the worker runs a self-review phase in its own worktree — a **review turn** reads `git diff <base>...HEAD` locally and writes `{verdict, findings[]}`; if there are findings, a **fix turn** addresses them and commits, the project check re-runs, and it loops back to review. Convergence is bounded by a *local* rounds counter (`review.max_rounds`), not a forge marker; if the cap is hit without a clean verdict the PR is published anyway (the human merge gate is the backstop) with a single footer line noting the non-convergence. Nothing is posted: no threads, no comments, no polling — the human opens a PR that has already been self-reviewed, and the PR conversation stays a clean human/external-review-only space. The review turn runs under the `impl-reviewer` routing profile, so it can still be a different model than the author doing the fixes. Running an external review bot instead? Set `review.enabled = false`.

Because the AI no longer creates review threads, the **fixer** naturally picks up only human and external-bot threads — GitHub stays the review transport exactly where a human sits.

### Cleaner (read-only repository sweeps)

The **cleaner** loop periodically walks the default branch head and reports accumulated divergence — spec/implementation drift, dead-code candidates, convention violations, stranded TODOs, stale remote branches, orphaned `meguri:working` labels — into a single per-project issue labeled `meguri:clean-report`. It never fixes anything: its only write is creating/updating that one issue (no pushes, no branch operations, no labels or comments elsewhere). The body is a snapshot rewritten on every sweep, with a hidden head-sha marker so the same head is never swept twice; a moved head triggers a new sweep only after `clean.interval_hours`. To act on a finding, open a regular issue and label it `meguri:plan` / `meguri:ready`; to silence a false positive, add a substring to `clean.ignore`; to pause the loop, put `meguri:hold` on the report issue.

Labels and comments on GitHub are the durable workflow state (looper's "Authority" principle); the local sqlite (`~/.meguri/meguri.sqlite`) only tracks run execution. Kill meguri any time — `meguri watch` recovers: live panes are re-adopted, dead runs resume from their last checkpointed step. Panes, sessions, and worktrees live per issue — one **author** pane shared by every branch-editing loop (planner → worker/spec worker → fixer/ci fixer/conflict resolver continue in the same live claude session) plus one independent **review** pane for the spec reviewer (and a transient **impl-review** pane while the worker self-reviews). After every completed turn meguri saves the agent's native session id on the issue's lane, so even if a pane dies while idle, the next run resumes the same conversation (`claude --resume <id>`); while watching, meguri reclaims the panes, worktree, and merged local branch of every issue that closes. `meguri prune` does the same on demand for one-shot usage.

Per-loop lifetimes at a glance:

| loop | trigger | key | worktree | normal end | pane |
|---|---|---|---|---|---|
| planner (author) | `meguri:plan` issue | issue | new branch | spec PR → `spec-reviewing` | kept |
| spec reviewer (review) | `spec-reviewing` PR, head unreviewed | issue + `review` | read-only detached, fixed at `review-<issue>` | clean → `spec-ready` / findings → wait for push | kept (independent) |
| spec worker (author) | `spec-ready` PR | issue (from branch) | takes over the PR branch | implementation → same PR | kept — continues the author pane |
| worker (author) | `meguri:ready` issue | issue | new branch | self-review → PR `Closes #N` | kept |
| fixer (author) | unresolved PR threads | issue (from branch) | attached to the PR head | replies on threads for re-review | kept — continues the author pane |
| ci fixer (author) | red CI on a meguri PR | issue (from branch) | attached to the PR head | fix pushed (≤3 rounds) | kept — continues the author pane |
| conflict resolver (author) | CONFLICTING meguri PR | issue (from branch) | attached to the PR head | base merged & pushed (≤3) | kept — continues the author pane |
| cleaner (standalone) | report issue + default-branch movement | report issue | read-only detached | report issue rewritten | self-reclaimed |

### Auto-merge (opt-in)

meguri never decides "safe to merge" — it arms GitHub-native auto-merge (`gh pr merge --auto`) on eligible PRs and lets GitHub (branch protection + required checks) decide when to merge (see `docs/adr/0003-auto-merge-github-native-arm-only.md`). It is off by default and gated behind two opt-ins: the master switch `[pr.auto_merge].enabled`, and (unless `opt_in = "all"`) the `meguri:automerge` label. Put the label on an *issue* and the worker copies it onto the PR (opening that PR non-draft); put it straight on a PR and it works too.

Riding the watch poll, a sweep arms a PR when **all** of these hold: it's a `meguri/` branch linked to its issue via `Closes #N.`; it carries no `meguri:hold` / `meguri:needs-human` / `meguri:working` / `meguri:spec-reviewing` / `meguri:spec-ready` label (auto-merge never fires mid-spec); it has zero unresolved review threads; and the repository allows auto-merge with the configured strategy (and, when required, required-checks branch protection). The arm is pinned to the reviewed head with `--match-head-commit`, and a marker comment (`<!-- meguri:automerge armed head=<sha> -->`) makes it idempotent and respects a human who later disables auto-merge — that head is never re-armed (a new push re-evaluates). If GitHub already reports the PR mergeable when meguri goes to arm it, meguri finalizes the merge on GitHub's own verdict instead.

```toml
[pr.auto_merge]
enabled = false                  # master switch
strategy = "squash"              # squash | merge | rebase (no fallback if the repo forbids it)
require_branch_protection = true # refuse to arm without required-checks branch protection
opt_in = "label"                 # label (needs meguri:automerge) | all (every eligible meguri PR)
```

When `enabled = true`, `meguri watch` and `meguri doctor` **fail fast** if the repo can't honor auto-merge (auto-merge disabled, strategy not allowed, or protection missing) rather than degrading silently at merge time. Two caveats, both with the same escape hatch (`require_branch_protection = false`): protection detection uses the **classic branch-protection API only** (rulesets aren't detected), and reading it needs an **admin-scoped token** (a non-admin token gets HTTP 403, which meguri surfaces rather than treating as "unprotected"). Note also the review gap until auto-merge 3/3: the reviewer gate (`require_clean_review`) that makes meguri's own review a precondition arrives in a later issue, so until then an opt-in PR can merge on green required checks before meguri has reviewed it — rely on branch protection for the bar you want.

## Configuration

Every key has a built-in default, so `config.toml` only needs `[[projects]]` plus whatever you want to override — `meguri init` writes a minimal template on exactly that premise.

`meguri watch` re-reads `config.toml` on every poll tick, so edits take effect for the runs spawned after them — no daemon restart (in-flight runs keep the config they started with). An invalid edit (bad TOML, no projects) is rejected with a log warning and the last good config stays in effect. Two exceptions are bound to the process lifetime and need a restart, which the log points out: `mux.kind` / `mux.session` (restart `meguri watch`) and the `[daemon]` section (re-run `meguri daemon install`).

The defaults:

```toml
# Language for agent-authored deliverables (PR descriptions, summaries, specs, reviews).
# Free-form text, e.g. "日本語" or "Japanese"; omit to leave the agent to its
# default (usually English). Override per project with `language` in [[projects]].
language = "日本語"

[mux]
kind = "auto"          # auto | herdr | tmux
session = "meguri"     # base label; each project gets its own workspace
                       # `meguri:<project>` (herdr) / `meguri-<project>` (tmux),
                       # so issue tabs don't intermingle. Bare `meguri` is the
                       # cross-project `meguri top` view.
# Panes live per issue (one author pane + one review pane) and are reclaimed
# when the issue closes; the agent's native session id is saved first
# (claude --resume <id>). "never" kills the pane as soon as its run ends
# (high-throughput operation). Any other value is rejected at load.
keep_pane = "until-issue-closed"  # also: never

[agent]
command = "claude"
# Default is yolo: the agent runs in an isolated worktree, and an autonomous
# loop stalls if it asks permission for every git/cargo command. To gate each
# command instead, set args = ["--permission-mode", "acceptEdits"] and answer
# dialogs by attaching to the pane.
args = ["--dangerously-skip-permissions"]

[limits]
idle_grace_secs = 90        # silence before a nudge
nudge_limit = 2             # nudges before paging a human
max_turn_runtime_secs = 2700
result_grace_secs = 60      # wait for Working→Idle after result appears
validate_turns = 3          # fix attempts for a failing check_command

[scheduler]
poll_interval_secs = 60
max_concurrent_runs = 2

[daemon]
restart_policy = "on-failure"  # launchd KeepAlive: never | on-failure | always
throttle_secs = 10             # launchd ThrottleInterval (secs between restarts)

[notifications]
macos = true           # page awaiting_human via a macOS notification (osascript)
# webhook_url = "https://example.com/hook"  # JSON POST: run id / issue / reason / attach
throttle_secs = 60     # min seconds between notifications for the same run

[pr]
draft = true   # open PRs as drafts; override per project with [projects.pr]

[pr.auto_merge]        # GitHub-native auto-merge, opt-in (see "Auto-merge" above)
enabled = false
strategy = "squash"    # squash | merge | rebase
require_branch_protection = true
opt_in = "label"       # label | all

[clean]
interval_hours = 24     # min hours between cleaner sweeps (a moved head alone doesn't trigger one)
stale_branch_days = 30  # remote branches older than this are reported as stale
ignore = []             # substrings that silence false positives; override per project with [projects.clean]

[review]
enabled = true    # kill switch for the worker's self-review phase (internal AI review of the diff)
max_rounds = 3    # max self-review rounds per run; past the cap the PR is published as-is
# (the old impl_enabled / impl_max_rounds keys still load as aliases)
```

`[projects.pr]` overrides the whole `[pr]` section at once (not key-by-key): a project that sets `[projects.pr]` gets the defaults for anything it omits, `[pr.auto_merge]` included.

### Workspaces — related projects, cross-repo decomposition (optional)

A **workspace** is a static grouping of related projects (a repo split/merge, an API + its client, a repo-designed greenfield). It is purely declarative — no runtime state, and it **never appears in the execution path** (worktree, pane, branch, and verification are unchanged; a `run` stays single-repo). Opt-in: a config with no `[[workspaces]]` behaves exactly as before.

```toml
[[workspaces]]
id = "shop"
projects = ["shop-api", "shop-web", "shop-infra"]   # each must be a defined [[projects]] id; a project joins at most one workspace
```

A workspace does exactly three things:

1. **Decompose scope** — the planner's decompose ending ([spec-first flow](#spec-first-flow-opt-in)) may file a child issue into a workspace sibling by setting `"project": "<sibling id>"` on the child (default: the parent's own repo). The parent (tracking) issue always stays in its own repo. A child that names a repo outside the workspace is rejected — issue-filing scope lives in config (the host operator), never in the issue body (a write-privileged input), which keeps "who runs work" and "who decides scope" separate (ADR 0009).
2. **Cross-repo ordering** — meguri wires GitHub-native `blocked_by` across sibling repos, so a child in one repo can block a child in another; discovery's existing dependency gate then sequences them (an unreadable blocker stays blocking, the safe side).
3. **Display grouping** — `meguri ps` / `meguri top` group their rows by workspace.

For a step meguri cannot perform itself (creating a repo, changing visibility, rewriting history, …), a decompose child with `"kind": "human"` is filed with **no trigger label**: discovery never drives it, and a human closes it — unblocking its dependents. `meguri doctor` lists each workspace and its members. See ADR 0009 for the rationale.

### Worktree setup hook (optional)

`[projects.worktree_setup]` runs a project's own commands every time meguri prepares a worktree — not just the first time, but every create/attach/re-point, since `attach_worktree`/`create_review_worktree` can wipe untracked files via `reset --hard` + `clean -fd` on reuse. meguri stays agnostic to what runs here (ADR 0003); apm (see [Agent instructions (apm)](#agent-instructions-apm)) is one example use case, not a built-in integration:

```toml
[projects.worktree_setup]
commands = ["apm install --frozen"]        # sh -c, run in order; a failing command stops the rest
exclude = [".claude/rules", "AGENTS.md"]   # appended to .git/info/exclude, alongside the always-on .meguri/
required = false                           # true escalates a failing command to a run failure (default: warn + continue)
timeout_secs = 300                         # per-command; commands may fetch over the network
```

Commands run with the worktree as `cwd` and get `MEGURI_ROLE` (the run's loop kind — `worker`, `fixer`, `spec-reviewer`, …), `MEGURI_PROFILE` (its resolved launch profile), and `MEGURI_ISSUE` (the target issue/task number) in the environment, so a script can specialize per role. Write commands idempotently — they may run several times against the same worktree.

### Role-based agent routing (optional)

By default every role — planner, spec-reviewer, impl-reviewer, worker, spec-worker, fixer, conflict-resolver — runs the single `[agent]` profile. That profile is now the `default` profile; you can define **named profiles** and route each role to a different CLI/model. Roles have stable cost/quality shapes (the planner's spec steers every downstream turn but costs little; the worker burns the bulk of the tokens; the fixer only touches small diffs), so routing keys on the role, not on an estimated issue difficulty.

```toml
# A profile is one CLI's launch bundle — same shape as [agent].
[agents.profiles.claude-opus]
command = "claude"
args = ["--dangerously-skip-permissions", "--model", "opus"]
resume_args = ["--resume"]

[agents.profiles.claude-sonnet]
command = "claude"
args = ["--dangerously-skip-permissions", "--model", "sonnet"]

[agents.profiles.codex]
command = "codex"
args = ["--yolo"]
resume_args = ["resume"]

[routing]
mode = "auto"        # auto | manual (default auto once [routing] exists)

[routing.roles]      # explicit picks always beat auto; per-role overrides
spec-reviewer = "codex"   # (the old `reviewer` key still works as an alias)
# impl-reviewer = "codex"  # the model for the worker's internal self-review turn
# worker = "claude-sonnet"
```

- **`[routing]` is the switch.** Without it, meguri behaves exactly as before — every role runs `default`, no CLI detection. Defining `[agents.profiles.*]` alone changes nothing; profiles stay inert until `[routing]` references them.
- **auto** applies a built-in 2026-07 recommendation table (planner → `claude-opus`, spec-reviewer/impl-reviewer → `codex` then `claude-opus`, worker/spec-worker/fixer/conflict-resolver → `claude-sonnet`), each chain filtered by `command --version` detection and always ending at `default`. `claude-opus`, `claude-sonnet`, and `codex` are built in, so `mode = "auto"` works with no `[agents.profiles]` at all.
- **manual** turns the table off: roles you don't list run `default`.
- **Explicit always wins, loudly.** A `[routing.roles]` entry must resolve — an undefined profile, an undetected CLI, or an unknown role name aborts `meguri watch` / `meguri run` at startup (never a silent fallback). Route a single role back to the old behavior with `worker = "default"` (never detected).
- The profile chosen at a run's first pane spawn is pinned to `runs.agent_profile` (shown in `meguri ps`'s PROFILE column and the `serve` API) and reused for every later spawn and resume. `meguri doctor` lists all profiles with their detection results and the final role→profile resolution.

## Development

```bash
cargo test                          # unit + tmux integration (skips w/o tmux)
MEGURI_TEST_HERDR=1 cargo test      # + herdr integration (needs live herdr)
```

The test suite drives the full loop with a scripted fake agent TUI (`tests/fixtures/fake_agent.sh`) against real tmux, real git worktrees, and a local bare origin — including blocked-dialog handling, lying-agent correction, validation feedback, and crash recovery.

For the designer-facing map of how the loops fit together — the full pipeline, dispatch priority, per-loop lifecycle, and an ADR index — see [docs/architecture/loops.md](docs/architecture/loops.md). This README stays the user-facing "how to use it" side; that doc is the "why it's structured this way" side.

### Agent instructions (apm)

meguri's own repo-specific instructions for AI coding agents (Claude Code / Codex) are sourced from [microsoft/apm](https://github.com/microsoft/apm) (`apm.yml`, `apm.lock.yaml`, `.apm/instructions/`) rather than hand-written `CLAUDE.md` / `AGENTS.md` files. The compiled artifacts (`CLAUDE.md`, `AGENTS.md`, `.claude/rules/`, `.codex/`, `apm_modules/`, `.agents/`) are gitignored — a one-line instructions edit shouldn't produce a regeneration diff on every parallel worktree/PR (see [ADR 0008](docs/adr/0008-agent-instructions-via-apm.md)). To build them locally:

```bash
brew install microsoft/apm/apm   # or: curl -sSL https://aka.ms/apm-unix | sh
apm install                      # deploys .apm/instructions/ -> .claude/rules/
apm compile                      # generates AGENTS.md (+ src/AGENTS.md) for Codex
```

Order matters: `apm compile` skips `CLAUDE.md` only because the preceding `apm install` already populated `.claude/rules/` (Claude Code reads that directly, so `apm` dedupes `CLAUDE.md` out). Compile first, or compile against an empty tree (e.g. `--root <scratch-dir>` for isolated verification), and it generates `CLAUDE.md`/`src/CLAUDE.md` too, since there's nothing to dedupe against yet. `apm install --dry-run` doesn't preview this step either — dry-run only reports on `apm`/`mcp` package dependencies (this repo has none), not the local `.apm/instructions/` integration; a real (non-dry-run) `apm install` is what actually deploys `.claude/rules/`.

Re-run both after editing anything under `.apm/instructions/` or `apm.yml`. A real `apm install` also rewrites `apm.lock.yaml`'s `local_deployed_files` / `local_deployed_file_hashes` to match whatever is currently deployed on disk; since those track the gitignored compiled files, don't commit that diff — run `git checkout apm.lock.yaml` before committing (re-running `apm lock` does *not* clear these fields; they're carried over from the existing lockfile). meguri now has a generic [worktree setup hook](#worktree-setup-hook-optional) (`[projects.worktree_setup]`) that can run this build automatically on every worktree preparation; wiring it up for meguri's own loops is tracked separately (#139).

## Status / roadmap

Eight loops run on GitHub today, mirroring looper's role model as `Loop` implementations sharing the same turn engine: the **worker** (issue → self-review → PR), the **planner** (`meguri:plan` issue → spec PR), the **spec reviewer** (`meguri:spec-reviewing` PR → summary review → `meguri:spec-ready`), the **spec worker** (`meguri:spec-ready` PR → implementation commits on the same branch and PR), the **fixer** (unresolved review comments on a meguri PR → fix commits pushed to it), the **ci fixer** (a meguri PR whose CI checks settled red → failed job logs fed to the agent → fix commits pushed; a PR still red after 3 fix rounds escalates to `meguri:needs-human`), the **conflict resolver** (a CONFLICTING meguri PR → the base branch merged, conflicts resolved, merge commit pushed), and the **cleaner** (periodic read-only sweep → divergence report in a single `meguri:clean-report` issue). AI review of the *implementation* diff is no longer a loop but an internal phase of the worker (**self-review**, ADR 0006): it runs in the run's worktree and never touches the forge.

**Versioning.** meguri is pre-1.0 (`0.x`) and follows [SemVer](https://semver.org): while on `0.x` the public API and CLI are not yet stable, so a minor bump (`0.y`) may carry breaking changes and patches (`0.y.z`) stay compatible; `1.0.0` is when stability is promised. Pin an exact version if you depend on current behavior.

**Releases.** Releases are tag-driven (ADR 0007): a maintainer bumps the version, refreshes `CHANGELOG.md`, and pushes a `vX.Y.Z` tag; `.github/workflows/release.yml` then builds the macOS arm64 / Linux x86_64 binaries, attaches them to a GitHub Release with git-cliff-generated notes, and (once the crate is set up) publishes to crates.io via OIDC Trusted Publishing. Because a pushed tag *is* the release trigger, tag deliberately — a mistaken tag ships a release.

## Contributing

Bug reports and PRs from humans are welcome — normal fork & PR flow, no
`meguri:*` labels to worry about. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
