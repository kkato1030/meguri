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
- **A run can't weaken its own completion contract.** The verification command can live in the repo's [`meguri.toml`](#repo-config--project-intrinsic-settings-in-meguritoml-optional), so meguri reads it from the run's worktree **once, at the run's start, and pins it** for the run's life. Editing `meguri.toml` mid-run — or `update-ref`-ing a branch — does not change the `check_command` that run is held to; a crash-and-resume reuses the pinned value rather than re-reading a since-tampered worktree. The guarantee is scoped honestly: it's *"an in-flight run's contract is fixed once it starts,"* not full isolation of an adversarial agent (which shares the host's git dir and credentials, and is out of scope). A weaker contract can only reach a run by being committed to the branch it runs on, where it shows up in the PR diff for the human merge gate to catch (ADR 0011).

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

### Let coding agents propose meguri

meguri ships a Claude Code **skill** so a coding agent can notice when a repo would benefit from
meguri and offer to set it up — honestly, disclosing the unattended-shell trade-off up front (see
[ADR 0009](docs/adr/0009-agent-skill-distribution-symptom-trigger-honest-pitch.md) and
[ADR 0012](docs/adr/0012-acquisition-skill-as-apm-subpath-github-ref.md)). Two channels, by whether
meguri already runs in the repo:

- **Not using meguri yet** — install the skill at the **user level** with
  [apm](https://github.com/microsoft/apm), so an agent can suggest meguri in any repo, even one that
  has never seen it:

  ```bash
  # replace vX.Y.Z with the latest release tag: https://github.com/kkato1030/meguri/releases/latest
  apm install -g --target claude kkato1030/meguri/skills/meguri#vX.Y.Z
  ```

  `--target claude` is not optional: without it apm deploys only to `~/.agents/skills/`, which Claude
  Code doesn't read, so the skill never fires. Pin the ref to a release tag (`#vX.Y.Z`) — an unpinned
  ref tracks `main` and drifts.

- **Already running meguri here** — the retention counterpart is `meguri agent-skills install`, backed
  by the same embedded `skills/meguri/` source so the installed copy always matches your `meguri`
  build:

  ```bash
  meguri agent-skills install            # ~/.claude/skills/meguri/ — the same skill as above,
                                          # refreshed from this binary (currently --target claude only)
  meguri agent-skills install --project  # .claude/rules/meguri.md in the current repo — day-2
                                          # operating rules for a repo already running meguri;
                                          # safe to re-run (idempotent)
  meguri agent-skills status             # installed? does it match this binary's embedded copy?
  ```

  `meguri init` offers the user-level install interactively. Neither command silently overwrites a
  file you hand-edited — it shows the diff and asks for `--force`.

## Use

```bash
# capture: turn a one-line memo into an issue (AI refines it afterwards)
meguri add "login redirect goes to the wrong page"

# one-shot: work a single issue
meguri run --project myproj --issue 42

# or keep watching: label an issue `meguri:ready` and meguri picks it up
meguri watch

meguri ps                 # runs, interaction state, panes
meguri schedules          # cron schedules: definition, last fire, next fire
meguri top                # build a dashboard workspace of tiled agent panes & attach
meguri logs <run>         # event trail + live pane tail
meguri attach <issue>     # jump into the issue's agent pane (or pass a run id)
meguri attach <issue> --review  # the pr-reviewer's independent pane
meguri pause <run>        # stop injecting prompts; pane stays alive
meguri resume <run>
meguri takeover <run>     # orchestrator hands-off; you drive
meguri handback <run>
meguri stop <run>         # kill pane, release the claim, cancel
meguri prune              # reclaim panes + worktrees of closed issues (--dry-run / --force)
```

### Intake (`meguri add`)

The first thing that clogs is filing the work item. `meguri add "<one line>"`
lowers that to a single command — and does the right thing for the project's
mode.

**github mode** — it creates the issue immediately, straight through
`create_issue`, never via the LLM, and prints the number and URL. Then,
best-effort, a headless agent reads the repo and refines the title and body;
the original memo is always kept verbatim at the bottom, so refine is only
scaffolding and your words keep authoring authority. Capture never waits on or
fails from the AI: if the agent is missing, refine fails, or you hit Ctrl-C,
the raw issue still stands. The default is unlabeled = untriaged (watch ignores
it); label it `meguri:plan` / `meguri:ready` later, or pass `--plan` /
`--ready` now. `--raw` skips refine entirely. Refine works out of the box with
the default `claude` CLI; if you swap `command` to another CLI, set that
profile's `headless_args` too or refine is skipped (raw capture only) —
`meguri doctor` flags it.

**local mode** — it queues a task in meguri's sqlite instead (see below);
`--file` reads a markdown task and `--not-before` holds it until a time.
`--plan` is rejected: local mode has no planner yet (issue #54), so a plan
task would never be picked up — use a github-mode project for planner work.

`--project` is inferred from the cwd (the project whose `repo_path` contains
it); pass it explicitly when ambiguous.

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
meguri tasks                                           # list open tasks (needs_human highlighted)
meguri watch                                           # picks tasks up within one poll interval
```

(`meguri add --plan` is github-only: local mode has no planner yet — issue #54.)

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

Plus bookkeeping / opt-in labels: `meguri:clean-report` and `meguri:triage-report` mark the cleaner and triage loops' per-project report issues (put `meguri:hold` on either to pause that sweep), `meguri:automerge` opts an issue (the worker copies it onto the PR) or a PR directly into GitHub-native auto-merge (see [Auto-merge (opt-in)](#auto-merge-opt-in) below), and `meguri:triage-ready` / `meguri:triage-plan` / `meguri:triage-needs-human` are triage's `advise`-mode proposals (see [Triage](#triage-read-only-recommendation-sweeps) below) — outside worker/planner discovery's vocabulary, so promote one by applying the real label yourself.

The **PR side** stays as it was: a spec PR carries `meguri:spec-reviewing` (awaiting review) then `meguri:spec-ready` (review passed; implementation continues) — these live on the PR, independent of the issue's phase label. CI-red and merge-readiness aren't mirrored to labels (GitHub shows them natively); a `meguri:awaiting-merge` PR label can be added later if needed.

New meguri labels are created with their scheme color automatically. If a label was created before this scheme (all generic blue), recolor it once with `gh label edit <name> --color <hex>` (e.g. `gh label edit meguri:implementing --color 0E8A16`) — meguri does not recolor existing labels on every sweep, so it never clobbers a color you set on purpose.

Discovery also honors GitHub-native issue dependencies (looper's ADR-0004): an issue *blocked by* another is skipped — silently, no label or comment — until every blocker is closed as **completed**. Blockers closed as *not planned* / *duplicate* don't count as resolved (the dependent issue awaits human re-triage), and unreadable blockers are treated as unresolved.

### Spec-first flow (opt-in)

Label an issue `meguri:plan` instead of `meguri:ready` and the **planner** loop investigates the repository and opens a *spec PR* (`Spec: <title>`) containing a single lightweight file, `docs/specs/issue-<N>.md` (acceptance criteria, files to touch, key decisions), labeled `meguri:spec-reviewing`. The spec's depth is **adaptive** ([ADR 0010](docs/adr/0010-adaptive-spec-depth.md)): the planner picks `normal` or a deeper `design` spec by uncertainty × blast radius, and any change that touches persistent state or a public contract is vetoed into carrying migration & rollback sections — the reason for the chosen depth is recorded in the spec or PR. The optional **pr-reviewer** review (below) reviews the spec PR and, when clean, flips the label to `meguri:spec-ready` — you can also flip it yourself. What happens next depends on `plan_delivery` (ADR 0008):

- **`separate`** (default) — two PRs. The spec/ADR PR is reviewed and **merged on its own** (it references its issue with a non-closing `Refs #N`, so merging it does not close the issue); a merged spec PR flips the issue `speccing → ready` and the **worker** implements it in a fresh PR, reading the landed spec and pruning it as part of the implementation.
- **`combined`** — one PR. The **spec worker** takes over the spec PR's branch and stacks the implementation on it (the #98 morph); spec and implementation merge once, together.

Either way the spec itself is disposable review scaffolding: it is deleted as part of the implementation, so `docs/specs/` never accumulates on the default branch — anything worth keeping (design decisions, domain rules) is routed to an ADR (`docs/adr/`) or a permanent domain document instead.

**Decomposition proposals** ([ADR 0016](docs/adr/0016-decompose-through-spec-review-gate-then-materialize.md)) — when the planner finds an issue too big for one spec (independent deliverables you'd want to review and roll back as separate PRs), it writes a *decomposition proposal* spec instead of an implementation spec: the parent goal, a requirement-coverage table, the child list + dependency graph, and rollout order, plus one machine-readable ` ```json meguri-children ` block. This goes through the **same spec-review gate** as any spec (so the split, and which child covers each requirement, are reviewed before anything is filed). Once the proposal PR is approved (`spec-ready` on the head the pr-reviewer actually reviewed), a lightweight **materializer** sweep files the child issues, wires GitHub-native `blocked_by` dependencies, applies each child's phase label (`meguri:ready` / `meguri:plan`, or none for a `human` step), and turns the parent into an unlabeled **tracking** issue — then closes the disposable proposal PR unmerged (the children + dependencies are the durable state; discovery's existing dependency gate then sequences the rollout). Materialization is idempotent: a partial run is resumed from the parent dependency graph without ever creating a duplicate issue. Set `decompose.materialize_enabled = false` to hold approved proposals for a human. Decomposition is one level only (a child cannot be decomposed again).

### Review: internal self-review (always) + GitHub pr-reviewer (optional)

Spec and implementation are symmetric (ADR 0008): both run a **mandatory internal self-review** before the PR opens, and both can enable an **optional external pr-reviewer** on the opened PR.

**Internal self-review** is an *internal phase* (ADR 0006): the author reviews its own work before the PR is ever pushed, so the review→fix ping-pong never touches GitHub. Between `validate` and `open-pr` a **review turn** reads the local diff and writes `{verdict, findings[]}`, applying every configured lens (`review.lenses`, default `correctness / tests / simplicity / security`); if there are findings, a **fix turn** addresses them and commits, the project check re-runs, and it loops back to review. Convergence is bounded by a *local* rounds counter (`review.max_rounds`), not a forge marker; past the cap the PR is published anyway (the pr-reviewer / human merge gate is the backstop). Nothing is posted to the conversation — the review turn runs under the `self-reviewer` routing role (so it can be a different model than the author), and the outcome is recorded off the conversation timeline: a `meguri/self-review` commit status on the pushed head and a folded `<details>` in the PR body. Set `review.enabled = false` to skip it (e.g. an external bot covers reviews).

**GitHub pr-reviewer** is the optional external review loop (`runs.loop_kind = "pr-reviewer"`), toggled per project × kind (`review.guard.plan` — on by default, the old spec reviewer — and `review.guard.impl` — off by default). It reviews the opened PR under an independent `pr-reviewer` routing role and records its verdict the same way — a `meguri/pr-review` commit status + a folded PR-body `<details>` — **never inline threads**, so the **fixer** never reacts to it and the AI↔AI ping-pong stays retired. The plan review also drives the spec labels (clean → `spec-ready`). For a human, a red pr-review check is *advisory* (it does not block the merge unless you make `meguri/pr-review` a required check); for auto-merge it is a *gate* (below).

Because the AI never creates review threads, the **fixer** naturally picks up only human and external-bot threads — GitHub stays the review transport exactly where a human sits.

### Cleaner (read-only repository sweeps)

The **cleaner** loop periodically walks the default branch head and reports accumulated divergence — spec/implementation drift, dead-code candidates, convention violations, stranded TODOs, stale remote branches, orphaned `meguri:working` labels — into a single per-project issue labeled `meguri:clean-report`. It never fixes anything: its only write is creating/updating that one issue (no pushes, no branch operations, no labels or comments elsewhere). The body is a snapshot rewritten on every sweep, with a hidden head-sha marker so the same head is never swept twice; a moved head triggers a new sweep only after `clean.interval_hours`. To act on a finding, open a regular issue and label it `meguri:plan` / `meguri:ready`; to silence a false positive, add a substring to `clean.ignore`; to pause the loop, put `meguri:hold` on the report issue.

### Triage (read-only recommendation sweeps)

Discovery still waits on a human to label issues `meguri:ready` / `meguri:plan`. The **triage** loop is the first step toward automating that last bit of hand-labeling, staged from read-only up (ADR 0006). It looks at every *untriaged* open issue (open, no engaged `meguri:` workflow label, not held, no unresolved blocker) and writes a recommendation for each — how meguri should handle it (`ready` / `plan` / `needs-human` / `hold` / `skip`), its confidence, and the rough size — into a single per-project issue labeled `meguri:triage-report`. The body is a snapshot table rewritten on every sweep, rate-limited by `triage.interval_hours`; a new issue triggers a fresh sweep even while the default-branch head is still (so a freshly filed issue is triaged without waiting for the next push). To silence a bad recommendation, add a substring to `triage.ignore`; to pause the loop, put `meguri:hold` on the report issue. Triage is **opt-in**: it does nothing until you set `triage.mode` (default `off`, because it automates a decision, not just an observation).

- **`report`** (v0) — **only recommends**: it never labels or comments on the issues it triages, so a wrong call breaks nothing. You adopt a recommendation by applying `meguri:ready` / `meguri:plan` yourself; the existing loops take it from there.
- **`advise`** (v1, ADR 0015) — everything `report` does, plus: `ready` / `plan` / `needs-human` recommendations also get a proposal label (`meguri:triage-ready` / `-plan` / `-needs-human`) and one evidence comment (confidence, complexity, rationale, missing info) directly on the recommended issue. Still not a decision — the proposal labels sit outside worker/planner discovery's vocabulary (discovery matches the exact real labels, never a `meguri:` prefix), so a wrong proposal cannot start work on its own. Promote a proposal by applying the real label yourself; reject it by removing the proposal label — meguri won't re-propose it until the issue's content (title + body) actually changes (a hidden hash marker in the evidence comment tracks this, so the same recommendation is never re-posted). `triage.max_actions_per_tick` (default 3) caps how many issues one sweep proposes to; the rest carry over to the next sweep.
- **`auto`** (v2, issue #88) — applying the real labels directly is future work.

### Reconcile (issue body edits are a re-attention signal)

Once an issue has been shipped by a succeeded run, meguri stops rediscovering it — otherwise every poll would re-file the same work. But that suppression used to be permanent: editing the issue's description afterwards changed nothing. The **reconcile** loop makes the suppression *body-aware* (comparing a whitespace-normalized digest of the body, so a mere label change — which also bumps GitHub's `updatedAt` — is ignored, and a whitespace-only edit doesn't count). A materially edited body lifts the suppression and emits a durable `issue.body_changed` event (visible in `meguri logs`); a poll sweep also leaves one comment on the already-`implementing` issue nudging you to re-label it `meguri:ready` if a re-run is wanted.

**A body edit is a signal, not a trigger.** It never launches an agent on its own — the execution gate stays the collaborator-applied phase label (the same [label gate](#labels) that bounds prompt-injection: "who can get an agent to execute" = "who has write access"). Editing the body only makes the issue *eligible* again; a collaborator still has to (re-)apply `meguri:ready`. Both the signal and the comment fire at most once per distinct new body, so a pending edit never floods the log. Turn the whole loop off with `reconcile.body_edits = false`, or keep detection but silence the comment with `reconcile.signal_comment = false`.

Labels and comments on GitHub are the durable workflow state (looper's "Authority" principle); the local sqlite (`~/.meguri/meguri.sqlite`) only tracks run execution. Kill meguri any time — `meguri watch` recovers: live panes are re-adopted, dead runs resume from their last checkpointed step. Panes, sessions, and worktrees live per issue — one **author** pane shared by every branch-editing loop (planner → spec fixer → worker/spec worker → fixer/ci fixer/conflict resolver continue in the same live claude session) plus one independent **pr-review** pane for the pr-reviewer (and a transient **self-review** pane while a run self-reviews). After every completed turn meguri saves the agent's native session id on the issue's lane, so even if a pane dies while idle, the next run resumes the same conversation (`claude --resume <id>`); while watching, meguri reclaims the panes, worktree, and merged local branch of every issue that closes. `meguri prune` does the same on demand for one-shot usage.

Per-loop lifetimes at a glance:

| loop | trigger | key | worktree | normal end | pane |
|---|---|---|---|---|---|
| planner (author) | `meguri:plan` issue | issue | new branch | self-review → spec PR → `spec-reviewing` | kept |
| pr-reviewer (pr-review) | reviewable PR (spec or impl), head unreviewed | issue + `pr-review` | read-only detached, fixed at `pr-reviewer-<issue>` | `meguri/pr-review` status + PR-body `<details>`; plan clean → `spec-ready` | kept (independent) |
| spec fixer (author) | `spec-reviewing` PR, head's plan review red | issue (from branch) | attached to the PR head | revised spec pushed (≤3 rounds) | kept — continues the author pane |
| spec worker (author) | `spec-ready` PR (combined delivery only) | issue (from branch) | takes over the PR branch | implementation → same PR | kept — continues the author pane |
| worker (author) | `meguri:ready` issue | issue | new branch | self-review → PR `Closes #N` | kept |
| fixer (author) | unresolved PR threads | issue (from branch) | attached to the PR head | replies on threads for re-review | kept — continues the author pane |
| ci fixer (author) | red CI on a meguri PR | issue (from branch) | attached to the PR head | fix pushed (≤3 rounds) | kept — continues the author pane |
| conflict resolver (author) | CONFLICTING meguri PR | issue (from branch) | attached to the PR head | base merged & pushed (≤3) | kept — continues the author pane |
| cleaner (standalone) | report issue + default-branch movement | report issue | read-only detached | report issue rewritten | self-reclaimed |
| triage (standalone) | report issue + default-branch/new-issue movement (opt-in) | report issue | read-only detached | report issue rewritten | self-reclaimed |

### Auto-merge (opt-in)

meguri never decides "safe to merge" — it arms GitHub-native auto-merge (`gh pr merge --auto`) on eligible PRs and lets GitHub (branch protection + required checks) decide when to merge (see `docs/adr/0003-auto-merge-github-native-arm-only.md`). It is off by default and gated behind two opt-ins: the master switch `[pr.auto_merge].enabled`, and (unless `opt_in = "all"`) the `meguri:automerge` label. Put the label on an *issue* and the worker copies it onto the PR (opening that PR non-draft); put it straight on a PR and it works too.

Riding the watch poll, a sweep arms a PR when **all** of these hold: it's a `meguri/` branch linked to its issue via `Closes #N.`; it carries no `meguri:hold` / `meguri:needs-human` / `meguri:working` / `meguri:spec-reviewing` / `meguri:spec-ready` label (auto-merge never fires mid-spec); it has zero unresolved review threads; and the repository allows auto-merge with the configured strategy (and, when required, required-checks branch protection). When the **impl pr-reviewer** is enabled it is a gate (ADR 0008): the sweep only arms a head whose `meguri/pr-review` status is success — a failure escalates to `meguri:needs-human`, an absent/pending status simply waits (and with the pr-reviewer disabled there is no status to require, so nothing deadlocks). The arm is pinned to the reviewed head with `--match-head-commit`, and a marker comment (`<!-- meguri:automerge armed head=<sha> -->`) makes it idempotent and respects a human who later disables auto-merge — that head is never re-armed (a new push re-evaluates). If GitHub already reports the PR mergeable when meguri goes to arm it, meguri finalizes the merge on GitHub's own verdict instead.

```toml
[pr.auto_merge]
enabled = false                  # master switch
mode = "native"                  # native (arm GitHub auto-merge) | orchestrator (meguri merges itself)
strategy = "squash"              # squash | merge | rebase (no fallback if the repo forbids it)
require_branch_protection = true # refuse to arm without required-checks branch protection
opt_in = "label"                 # label (needs meguri:automerge) | all (every eligible meguri PR)
```

When `enabled = true`, `meguri watch` and `meguri doctor` **fail fast** if the repo can't honor auto-merge (auto-merge disabled, strategy not allowed, or protection missing) rather than degrading silently at merge time. Two caveats, both with the same escape hatch (`require_branch_protection = false`): protection detection uses the **classic branch-protection API only** (rulesets aren't detected), and reading it needs an **admin-scoped token** (a non-admin token gets HTTP 403, which meguri surfaces rather than treating as "unprotected"). To make meguri's own review a merge precondition, enable the **impl pr-reviewer** (`review.guard.impl = true`): auto-merge then only arms a PR whose `meguri/pr-review` status is success (ADR 0008). With the impl pr-reviewer off there is no such gate, so an opt-in PR can merge on green required checks before meguri has externally reviewed it — rely on branch protection (and the mandatory internal self-review) for the bar you want.

**`mode` — native vs orchestrator.** The default `native` is described above: meguri only arms, GitHub decides. But **private repos on the Free plan cannot enable "Allow auto-merge" at all** (the API silently ignores the PATCH) and have no branch protection, so `native` always fails fast there — the same constraint meguri itself hit in `docs/adr/0004-automerge-gate-renovate-side-on-free-private.md`. `mode = "orchestrator"` is the fallback for exactly those repos: the eligibility gate is identical (same branch / link / label / thread checks), but instead of arming, **meguri merges the PR itself** (`gh pr merge --squash`-equivalent, pinned to the reviewed head) as soon as GitHub reports it `MERGEABLE`. `CONFLICTING` goes to the conflict-resolver and `UNKNOWN` waits for the next sweep. Because there is no server-side gate, orchestrator mode **explicitly accepts meguri's own pre-PR verification (`check_command` + self-review) as the only gate** (`docs/adr/0009-auto-merge-orchestrator-side-merge-on-free-private.md`); `meguri doctor` prints a reminder to that effect. Orchestrator mode requires `require_branch_protection = false` (config validation rejects the contradiction). Keep `native` wherever "Allow auto-merge" *can* be enabled — a server-side gate is always stronger than an in-process one.

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

[triage]
mode = "off"                # off (default, opt-in) | report (v0 read-only) | advise (v1) | auto (v2)
interval_hours = 6          # min hours between triage sweeps
ignore = []                 # substrings that silence bad recommendations; override per project with [projects.triage]
max_actions_per_tick = 3    # advise mode only: max issues proposed-to (label + comment) per sweep

[review]
enabled = true    # kill switch for the internal self-review phase (plan + impl)
max_rounds = 3    # max self-review rounds per run; past the cap the PR is published as-is
lenses = ["correctness", "tests", "simplicity", "security"]  # the multi-lens perspectives (ADR 0008)
# (the old impl_enabled / impl_max_rounds keys still load as aliases)

[review.guard]    # the optional external GitHub pr-reviewer review, per kind (ADR 0008)
plan = true       # review the spec/ADR PR (the old mandatory spec reviewer) — on by default
impl = false      # review the implementation PR — off by default (opt-in; external-bot compatible)

[reconcile]
body_edits = true      # detect that a shipped issue's body was edited and treat it as a re-attention signal
signal_comment = true  # also leave a "re-label meguri:ready" nudge comment (false = the durable event only)
```

Plan-first delivery is chosen per project with `plan_delivery` (default `separate` = two PRs; `combined` = the #98 one-PR morph); like `[pr]` and `[clean]`, `[projects.review]` overrides the whole `[review]` section at once.

`[projects.pr]` overrides the whole `[pr]` section at once (not key-by-key): a project that sets `[projects.pr]` gets the defaults for anything it omits, `[pr.auto_merge]` included.

### Repo config — project-intrinsic settings in `meguri.toml` (optional)

Some settings describe the **repo itself**, not the host that runs it: how to verify its work (`check_command`), what language its deliverables use (`language`), whether its PRs open as drafts (`pr.draft`). Instead of copying those into every host's `config.toml`, put them in a `meguri.toml` at the **repo root** — versioned with the code, identical on every host. Opt-in: a repo with no `meguri.toml` behaves exactly as before.

```toml
# <repo>/meguri.toml
language = "日本語"
check_command = "cargo test"

[pr]
draft = false        # repo-eligible; auto_merge here is a `meguri doctor` error (host-only)
```

**The boundary** (ADR 0011): a repo may declare only **project-intrinsic facts that affect its own runs**. Anything that names *other* repos, binds to a host machine or token, or is a trust declaration stays host-only — so `repo_slug`, `mode`, `default_branch`, `[[workspaces]]`, `[agent]`, `[routing]`, `pr.auto_merge`, and the rest are rejected in `meguri.toml` (a host-only key is a `meguri doctor` error, never silently ignored). Initial repo-eligible keys: `check_command`, `language`, `pr.draft`.

**Precedence** is `built-in default < host global section < repo meguri.toml < host [projects.*] override` — the host always wins last, so an operator can override a repo's setting locally. A broken or invalid `meguri.toml` is warned about and treated as absent (the run continues on host config; the process never dies).

**When it takes effect**: the values are read from the run's worktree once at the start of each run and pinned for that run's life (see [Security](#security)). So a change reaches runs by landing on a branch: merge to the default branch and every later run picks it up; commit to a PR branch and that PR's own run uses it (config-with-code). Editing `meguri.toml` mid-run does not affect the run in flight. See ADR 0011 for the full model.

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

See [docs/ops/apm-worktree-setup.md](docs/ops/apm-worktree-setup.md) for the wired-up, dogfooded example (#139). Note that `apm install --frozen` rewrites `apm.lock.yaml` (a tracked file) on every run, so `commands` needs a trailing `git checkout -- apm.lock.yaml` — otherwise the clean-tree check fails on a diff the agent never touched (`exclude` only suppresses untracked paths, it can't help here).

Commands run with the worktree as `cwd` and get `MEGURI_ROLE` (the run's loop kind — `worker`, `fixer`, `pr-reviewer`, …), `MEGURI_PROFILE` (its resolved launch profile), and `MEGURI_ISSUE` (the target issue/task number) in the environment, so a script can specialize per role. Write commands idempotently — they may run several times against the same worktree.

### Scheduled enqueue (`[[projects.schedules]]`, optional)

For time-driven operation — a daily production task, a weekly tidy — a project can carry cron **schedules** that periodically enqueue work. A schedule only *puts one item on the queue* (a labeled issue in github mode, a local task in local mode); the existing worker/planner loops consume it exactly as if you had filed it by hand. meguri does **not** run arbitrary commands on a timer — enqueue is the whole job, execution stays the loops' (ADR 0009). This makes it the recurring-work counterpart to `meguri add`, evaluated by `meguri watch` on every poll tick.

```toml
[[projects.schedules]]
name = "daily-tidy"              # unique within the project
cron = "0 9 * * *"              # standard 5-field cron, interpreted as UTC
kind = "ready"                  # "ready" → worker (meguri:ready) | "plan" → planner (meguri:plan, github only)
title = "Daily tidy {{date}}"  # template; the only variable is {{date}} (the fire date, YYYY-MM-DD UTC)
body_file = "ops/daily-tidy.md" # repo-relative body file — or `body = "..."` inline (exactly one)
# allow_overlap = false         # default: skip firing while this schedule's last issue/task is still open
```

- **Cron is UTC** and evaluated at poll-interval granularity (5 fields: minute hour day-of-month month day-of-week; `*`, ranges, `*/n` steps, and lists supported). Want local time? Offset the expression yourself; a per-schedule timezone is a later addition.
- **Catch-up is folded.** The last-fired time is persisted in sqlite (not config, so a hot-reload edit to the definition never loses it). If `watch` was down across several occurrences, the schedule fires **once** on the next tick, not once per missed occurrence — the cron-daemon rule. A newly-added schedule never backfills the past: its first tick just records "seen".
- **Overlap guard.** By default a schedule skips (but still consumes that occurrence — no backfill when it later closes) while its previous issue/task is still open, so a slow item doesn't pile up duplicates. Set `allow_overlap = true` to fire every occurrence regardless.
- **Provenance.** Each fired item carries a hidden `<!-- meguri:schedule name=<name> -->` marker in its body (local tasks also get `origin = schedule:<name>`).
- Definitions are hot-reloaded (#73): add or change a schedule and it takes effect on the next tick, no `watch` restart. `meguri doctor` validates the cron expression, name uniqueness, body exclusivity, and `body_file` existence; `meguri schedules` lists each definition with its last and next fire.

Since local mode has no planner, `kind = "plan"` is github-only — a local `plan` schedule is rejected at config load (the task would never be consumed).

### Throttling discovery: not-before and cadence (`[[projects.cadence]]`, optional)

Enqueue is only half of time-driven operation; the other half is pacing *consumption*. Discovery normally drains the queue as fast as the slot budget allows, so two kinds of time-bound work need a brake (issue #148). Both skip **silently** — no label, no comment on the forge, exactly like a blocked GitHub-native dependency — and both are visible in `meguri tasks`.

- **not-before** — "don't start before this instant." In github mode put a hidden marker in the issue body; in local mode pass `--not-before`:

  ```
  <!-- meguri:not-before 2026-07-20 -->          # a bare date is midnight UTC
  <!-- meguri:not-before 2026-07-20T09:00:00Z --># or a full RFC3339 UTC instant
  ```
  ```sh
  meguri add --not-before 2026-07-20 "Launch post"
  ```
  A garbled date fails **closed** (the task stays held, surfaced in `meguri tasks`) rather than leaking early.

- **cadence** — "consume this label at most N per window." Declare per-label rate limits; discovery counts consumption from the local run history (never from GitHub — labels are workflow state, execution records are local) and holds the label once the window is full:

  ```toml
  [[projects.cadence]]
  label = "sns"          # a github issue label
  max_per_day = 1        # at most one per UTC calendar day
  # — or a rolling window instead of a calendar day: —
  # per_hours = 168
  # max = 1
  ```
  Cadence is github-only (local tasks carry no labels). Consumption counts every attempt except a benign skip — a failed run still spends the day's slot, so a broken post can't retry past the media's rate limit. An issue matching two rules fails closed (a run counts toward one bucket only); `meguri run --issue N` bypasses the gate but still counts toward the window. `meguri doctor` shows each rule's current window usage.

### Role-based agent routing (optional)

`[routing.roles]` steers **6 routing roles** — the "which model should do this kind of work?" question a human actually asks. They are coarser than the internal loop kinds (`runs.loop_kind`, still tracked one-per-loop for budget counting and `meguri stats routing`); several loop kinds share a role's cost/quality shape:

| role | question | internal loop(s) / phase |
|---|---|---|
| `planner` | plan / write the spec | `planner` |
| `worker` | implement | `worker`, `spec-worker` |
| `fixer` | make a PR mergeable | `fixer`, `ci-fixer`, `conflict-resolver` |
| `self-reviewer` | internal review before the PR is public | the self-review phase (inside the worker/planner flow) |
| `pr-reviewer` | advisory review on a published PR (auto-merge gate) | the pr-reviewer loop |
| `cleaner` | hygiene sweep | `cleaner` |

By default every role runs the single `[agent]` profile (now named the `default` profile); you can define **named profiles** and route each role to a different CLI/model. The planner's spec steers every downstream turn but costs little; the worker burns the bulk of the tokens; the fixer only touches small diffs — so routing keys on the role, not on an estimated issue difficulty.

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
pr-reviewer = "codex"     # (the old `reviewer` / `spec-reviewer` / `guard` keys still work as aliases)
# self-reviewer = "codex"  # the model for the internal self-review turn (plan + impl)
# worker = "claude-sonnet"
```

- **`[routing]` is the switch.** Without it, meguri behaves exactly as before — every role runs `default`, no CLI detection. Defining `[agents.profiles.*]` alone changes nothing; profiles stay inert until `[routing]` references them.
- **auto** applies a built-in 2026-07 recommendation table (`planner` → `claude-opus`, `self-reviewer`/`pr-reviewer` → `codex` then `claude-opus`, `worker`/`fixer` → `claude-sonnet`, `cleaner` → `default`), each chain filtered by `command --version` detection and always ending at `default`. `claude-opus`, `claude-sonnet`, and `codex` are built in, so `mode = "auto"` works with no `[agents.profiles]` at all.
- **manual** turns the table off: roles you don't list run `default`.
- **Explicit always wins, loudly.** A `[routing.roles]` entry must resolve — an undefined profile, an undetected CLI, or an unknown role name aborts `meguri watch` / `meguri run` at startup (never a silent fallback). Route a single role back to the old behavior with `worker = "default"` (never detected). Config keys from before the role redesign (`reviewer`, `spec-reviewer`, `guard`, `impl-reviewer`, `self-review`, `spec-worker`, `conflict-resolver`, `ci-fixer`) still resolve as aliases of the new names.
- The profile chosen at a run's first pane spawn is pinned to `runs.agent_profile` (shown in `meguri ps`'s PROFILE column and the `serve` API) and reused for every later spawn and resume. `meguri doctor` lists all profiles with their detection results and the final role→profile resolution.

### Role preambles (`[prompts]`, optional)

Standing project discipline — "read this guardrail before you start", "follow this editorial persona", "don't commit anything that misses this quality bar" — is the same for every issue, not per-issue. `[prompts]` injects it into the turn prompt, keyed by routing role, so the worker gets quality bars, the planner gets planning guidelines, the reviewer gets audit lenses. The value is a **repo-relative** path; the file's contents are embedded at the top of the prompt (a preface — the completion contract stays last and wins).

```toml
[prompts]                          # top-level default (applies to every project)
all = "ops/agents/guardrails.md"   # shared by every role
worker = "ops/agents/worker.md"    # keys are the 6 routing roles (worker/planner/fixer/self-reviewer/pr-reviewer/cleaner)

[projects.prompts]                 # per-project override, per key
planner = "ops/agents/planner.md"
```

- **Embedded, not referenced** — the discipline reaches the agent whether the profile is Claude or Codex, and whether or not the agent bothers to open the file (that CLI-independence is the point; [ADR 0012](docs/adr/0012-role-preamble-injected-into-turn-prompt.md)).
- **`all` then the role**, both injected; per-project entries override the top-level one **per key** (the same role vocabulary and aliases as `[routing.roles]`; an unknown role key aborts config load).
- **Missing is non-fatal** — a path that doesn't exist (or a symlink that escapes the worktree) is skipped with a warning and a `prompt.preamble_missing` event; the turn still runs. `meguri doctor` reports configured paths that don't resolve inside the clone.
- **When to reach for it vs. `CLAUDE.md`**: if the same always-on context suffices for every role and only Claude runs, [agent instructions (apm)](#agent-instructions-apm) / `CLAUDE.md` already covers it — use `[prompts]` when you need per-role text or CLI-independent delivery, and keep the files short (bulky context belongs in `CLAUDE.md`).

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

Re-run both after editing anything under `.apm/instructions/` or `apm.yml`. A real `apm install` also rewrites `apm.lock.yaml`'s `local_deployed_files` / `local_deployed_file_hashes` to match whatever is currently deployed on disk; since those track the gitignored compiled files, don't commit that diff — run `git checkout apm.lock.yaml` before committing (re-running `apm lock` does *not* clear these fields; they're carried over from the existing lockfile). meguri now has a generic [worktree setup hook](#worktree-setup-hook-optional) (`[projects.worktree_setup]`) that can run this build automatically on every worktree preparation, and it's wired up for meguri's own loops too (#139; see [docs/ops/apm-worktree-setup.md](docs/ops/apm-worktree-setup.md) for the setup and the results of dogfooding it).

## Status / roadmap

Ten loops run on GitHub today, mirroring looper's role model as `Loop` implementations sharing the same turn engine: the **worker** (issue → self-review → PR), the **planner** (`meguri:plan` issue → self-review → spec PR), the **pr-reviewer** (a reviewable PR, spec or impl → summary review recorded as a `meguri/pr-review` commit status + a folded PR-body `<details>`; the plan review also flips `spec-reviewing → spec-ready`), the **spec fixer** (a `meguri:spec-reviewing` PR whose head's plan review settled red → the pr-reviewer's findings fed back to the author lane → a revised spec pushed to the same PR, which the pr-reviewer re-reviews; a spec PR still red after 3 revision rounds escalates to `meguri:needs-human`), the **spec worker** (`meguri:spec-ready` PR under combined delivery → implementation commits on the same branch and PR), the **fixer** (unresolved review comments on a meguri PR → fix commits pushed to it), the **ci fixer** (a meguri PR whose CI checks settled red → failed job logs fed to the agent → fix commits pushed; a PR still red after 3 fix rounds escalates to `meguri:needs-human`), the **conflict resolver** (a CONFLICTING meguri PR → the base branch merged, conflicts resolved, merge commit pushed), the **cleaner** (periodic read-only sweep → divergence report in a single `meguri:clean-report` issue), and the **triage** loop (opt-in sweep → recommendations for untriaged open issues in a single `meguri:triage-report` issue; `advise` mode also proposes a `meguri:triage-*` label + evidence comment on each recommended issue). The mandatory internal **self-review** (ADR 0006/0008) is not a loop but a phase both the worker and planner run in the run's worktree before the PR opens; a light **plan→impl handoff** sweep advances separate-delivery specs (`speccing → ready` once the spec PR merges). Both are off the conversation timeline.

**Versioning.** meguri is pre-1.0 (`0.x`) and follows [SemVer](https://semver.org): while on `0.x` the public API and CLI are not yet stable, so a minor bump (`0.y`) may carry breaking changes and patches (`0.y.z`) stay compatible; `1.0.0` is when stability is promised. Pin an exact version if you depend on current behavior.

**Releases.** Releases are tag-driven (ADR 0007): a maintainer bumps the version, refreshes `CHANGELOG.md`, and pushes a `vX.Y.Z` tag; `.github/workflows/release.yml` then builds the macOS arm64 / Linux x86_64 binaries, attaches them to a GitHub Release with git-cliff-generated notes, and (once the crate is set up) publishes to crates.io via OIDC Trusted Publishing. Because a pushed tag *is* the release trigger, tag deliberately — a mistaken tag ships a release.

## Contributing

Bug reports and PRs from humans are welcome — normal fork & PR flow, no
`meguri:*` labels to worry about. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
