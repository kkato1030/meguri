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
- **Silence is nudged, not punished.** A quiet agent gets a capped number of reminder lines. If it stays quiet, meguri assumes the *session* (not the work) is broken and heals itself: one more try on the same session, then a fresh session, and only when even that stays quiet is a human paged — with a sanitized pane tail attached for diagnosis. Resumes are gated the same way: a session whose transcript outgrew its context window is never resumed into a 400-loop, and a pane whose agent exited to a bare shell is never typed into (ADR 0029). meguri never auto-fails a run for being slow.
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
- **Issue bodies are prompt input.** The full issue body (and comments a loop reads) is injected into the agent's prompt verbatim, so on a repo where anyone can open issues, a malicious one is a prompt-injection attempt against an agent with shell access. The mitigation is the [label gate](#labels): a loop only acts on an issue that already carries the `meguri:ready` label, and applying labels needs collaborator (write) access — so "who can get an agent to execute" reduces to "who has write access to this repo," not "who can open an issue." Weigh that when granting collaborator access, and don't label untrusted issues `meguri:ready` yourself.
- **Completion is verified independently, not screen-scraped.** As described in [The completion contract](#the-completion-contract) above, meguri never takes the agent's own "success" claim at face value — it re-checks git state, commits ahead of base, and the project's `check_command` before treating a run as done. This bounds (but doesn't eliminate) the damage a compromised or misled agent can do: it can still act inside the worktree during a run, but it can't talk meguri into merging bad state just by writing "success" to the result file.
- **A run can't weaken its own completion contract.** The verification command (`check_command`) lives in the host's `~/.meguri/config.toml`, never in the repository being worked on — so nothing an agent commits inside a worktree can change the checks its own run is held to.
- **Pre-flight prime is folder-trust only, and tool-free.** A `claude` CLI stalls on a first-run "trust the files in this folder?" prompt for each fresh worktree, and meguri never reads the screen to answer it. So just before an interactive pane spawns, meguri runs one headless `claude` turn in that worktree (a "pre-flight prime") so the CLI records folder trust for the path. This turn runs **without** `--dangerously-skip-permissions`, under a meguri-owned deny-all `--settings` file plus `--strict-mcp-config`, so it executes **no** tool — a hostile `CLAUDE.md` in the worktree can't drive Bash/Edit/MCP before the pane starts, even against a permissive inherited config. It carries the pane's own `--model` and only writes folder trust, never the config-dir-level "Bypass Permissions" acceptance (that stays a one-time step doctor points you at). It runs at most once per worktree, and never fails a launch — if it errors, times out, or the CLI is too old to support the deny flags, the pane just launches as before. Disable it per profile with `preflight = []`. Setting an explicit non-empty `preflight` bypasses the safe default and is your responsibility — a yolo one is injection-unguarded and meguri warns at startup.

Found a vulnerability in meguri itself? See [SECURITY.md](SECURITY.md).

## Install & set up

Prereqs: `git`, [`gh`](https://cli.github.com) (authenticated), an agent CLI (`claude` by default), and a multiplexer — a running [herdr](https://herdr.dev) (recommended; native agent-state detection) or `tmux` (screen-heuristic fallback). These runtime prerequisites are the same however you install meguri — a prebuilt binary still needs `git`/`gh`/a multiplexer on the host.

Platform: meguri runs on macOS and Linux.

```bash
cargo install --path .            # or: cargo build --release
meguri init                       # writes ~/.meguri/config.toml (no live projects yet), creates the db
meguri doctor                     # checks gh auth, mux, agent CLI
```

Other ways to get the binary:

- **Prebuilt binary** — download the archive for your platform (macOS arm64 / Linux x86_64) from the [latest GitHub Release](https://github.com/kkato1030/meguri/releases/latest), verify its `.sha256`, extract, and put `meguri` on your `PATH`.
- **crates.io** — `cargo install meguri` (once the crate is published; see [Status / roadmap](#status--roadmap)).

**Add a project by editing `config.toml`.** `meguri init` writes a minimal `~/.meguri/config.toml` with **no live projects** (the `[[projects]]` stub is commented out). Uncomment and edit it — meguri operates on a clone you maintain yourself, pointed at by the required `repo_path`:

```toml
[[projects]]
id = "myproj"
repo_path = "/abs/path/to/clone"  # required: the clone meguri cuts worktrees from
repo_slug = "owner/repo"          # required unless mode = "local"
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
meguri logs <run>         # event trail + live pane tail
meguri attach <issue>     # jump into the issue's agent pane (or pass a run id)
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
`create_issue`, and prints the number and URL. The default is unlabeled = not
queued (watch ignores it); label it `meguri:ready` later, or pass `--ready`
now — that also imports the task into meguri's local queue immediately,
without waiting for the next intake pass.

**local mode** — it queues a task in meguri's sqlite instead (see below);
`--file` reads a markdown task.

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

A local run works on a `meguri/t<task-id>-<slug>-<hash>` branch; on success it leaves the verified commits there and flips the task to `done` — nothing is pushed. A failed run marks the task `needs_human` with a reason (shown by `meguri tasks` / `meguri ps`), and the next run re-claims it and clears the flag. Review the branch yourself and merge when happy (`meguri review` / `accept` land in a later phase).

> **Single machine only.** The local sqlite is the *single source of truth*, so run exactly one meguri host per repo (the watch lock enforces one process per host). The vocabulary and contract for a future multi-host `TaskSource` are fixed in [ADR 0003](docs/adr/0003-tasksource-task-moves-run-pins.md).

### Keep it running

`meguri watch` stays in the foreground. To survive closing the shell, run it
under your own supervisor — a tmux/herdr pane, `nohup`, or a launchd/systemd
unit you manage. Whatever you choose, the watch process holds an exclusive
lock (`~/.meguri/daemon/watch.lock`), so a second scheduler fails loudly
instead of double-driving runs.

### Labels

Since the authority inversion (権威反転), **meguri's queue authority is its local
sqlite `tasks` table, not GitHub**. Labels are read as a low-frequency edge
signal (an intake pass every `scheduler.intake_interval_secs`, default 120s)
and written back as a best-effort projection — a failed label write never
stops a run, and the queue never depends on a per-tick GitHub read.

Inputs (you apply, intake reads):

| label | color | meaning |
|---|---|---|
| `meguri:ready` | 🔵 blue | queue this issue for the worker loop (imported as a task row on the next intake pass) |
| `meguri:hold` | ⚪ grey | emergency stop, phone-operable: a held task is not dispatched (in-flight runs keep going — use `meguri stop` for those). Removing the label releases it |

Projections (meguri writes, best-effort):

| label | color | meaning |
|---|---|---|
| `meguri:working` | 🟡 yellow | an agent is working on it right now (the claim) |
| `meguri:implementing` | 🟢 green | an implementation PR is open |
| `meguri:needs-human` | 🔴 red | a human needs to look; a comment explains why. Clearing it (while `meguri:ready` is still on) re-queues the task on the next intake pass |

Filtering on 🔴 `meguri:needs-human` gives you a clean human-TODO list. New
meguri labels are created with their scheme color automatically; recolor a
pre-existing generic-blue one once with `gh label edit <name> --color <hex>` —
meguri never clobbers a color you set on purpose.

Discovery also honors GitHub-native issue dependencies: an issue *blocked by*
another is skipped — silently, no label or comment — until every blocker is
closed as **completed**. Blockers closed as *not planned* / *duplicate* don't
count as resolved (the dependent issue awaits human re-triage), and unreadable
blockers are treated as unresolved.

Once an issue has been shipped by a succeeded run, meguri stops re-importing
it even if the ready label lingers (a duplicate-PR guard); re-adding
`meguri:ready` after the task completed normally queues a fresh run.

Kill meguri any time — `meguri watch` recovers: live panes are re-adopted,
dead runs resume from their last checkpointed step. Panes, sessions, and
worktrees live per issue; after every completed turn meguri saves the agent's
native session id, so even if a pane dies while idle, the next run resumes the
same conversation (`claude --resume <id>`). While watching, meguri reclaims
the panes, worktree, and merged local branch of every issue that closes;
`meguri prune` does the same on demand.

## Configuration

Every key has a built-in default, so `config.toml` only needs `[[projects]]` plus whatever you want to override — `meguri init` writes a minimal template on exactly that premise.

`meguri watch` re-reads `config.toml` on every poll tick, so edits take effect for the runs spawned after them — no restart (in-flight runs keep the config they started with). An invalid edit (bad TOML, no projects) is rejected with a log warning and the last good config stays in effect. One exception is bound to the process lifetime and needs a restart, which the log points out: `mux.kind` / `mux.session`.

The defaults:

```toml
# Language for agent-authored deliverables (PR descriptions, summaries).
# Free-form text, e.g. "日本語" or "Japanese"; omit to leave the agent to its
# default (usually English). Override per project with `language` in [[projects]].
language = "日本語"

[mux]
kind = "auto"          # auto | herdr | tmux
session = "meguri"     # base label; each project gets its own workspace
                       # `meguri:<project>` (herdr) / `meguri-<project>` (tmux),
                       # so issue tabs don't intermingle.
# Panes live per issue and are reclaimed when the issue closes; the agent's
# native session id is saved first (claude --resume <id>). "never" kills the
# pane as soon as its run ends (high-throughput operation).
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
intake_interval_secs = 120  # how often the GitHub intake pass reads labels;
                            # the queue authority itself is the local sqlite

[pr]
draft = true   # open PRs as drafts; override per project with [projects.pr]
```

`[projects.pr]` overrides the whole `[pr]` section at once (not key-by-key).

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

Commands run with the worktree as `cwd` and get `MEGURI_ROLE` (the run's loop kind — `worker`), `MEGURI_PROFILE` (its resolved launch profile), and `MEGURI_ISSUE` (the target issue/task number) in the environment. Write commands idempotently — they may run several times against the same worktree.

### Agent profiles (optional)

By default every run launches the single `[agent]` profile (named `default`).
You can define **named profiles** — one CLI's launch bundle, same shape as
`[agent]` — and pick one per project with `profile` in `[[projects]]`:

```toml
[agents.profiles.claude-opus]
command = "claude"
args = ["--dangerously-skip-permissions", "--model", "opus"]
resume_args = ["--resume"]
# preflight = []   # opt out of the launch-time folder-trust prime (see Security)

[agents.profiles.codex]
command = "codex"
args = ["--yolo"]
resume_args = ["resume"]

[[projects]]
id = "myproj"
repo_path = "/abs/path/to/clone"
repo_slug = "owner/repo"
profile = "codex"    # omit for the default [agent] profile
```

`claude-opus`, `claude-sonnet`, and `codex` are built in, so a project can
reference them with no `[agents.profiles]` at all. A `profile` entry must
name a defined profile — an unknown name aborts `meguri watch` / `meguri run`
at startup (never a silent fallback). The profile chosen at a run's first pane
spawn is pinned to the run (shown in `meguri ps`'s PROFILE column) and reused
for every later spawn and resume. `meguri doctor` lists all profiles and each
project's resolution.

### Prompt preambles (`[prompts]`, optional)

Standing project discipline — "read this guardrail before you start", "don't
commit anything that misses this quality bar" — is the same for every issue,
not per-issue. `[prompts]` injects it into the turn prompt: the value is a
**repo-relative** path whose contents are embedded at the top of the prompt
(a preface — the completion contract stays last and wins).

```toml
[prompts]                          # top-level default (applies to every project)
all = "ops/agents/guardrails.md"   # shared preamble
worker = "ops/agents/worker.md"    # the worker loop's own preamble

[projects.prompts]                 # per-project override, per key
worker = "ops/agents/worker.md"
```

- **Embedded, not referenced** — the discipline reaches the agent whether the
  profile is Claude or Codex, and whether or not the agent bothers to open the
  file.
- **`all` then `worker`**, both injected; per-project entries override the
  top-level one **per key** (an unknown key aborts config load).
- **Missing is non-fatal** — a path that doesn't exist (or a symlink that
  escapes the worktree) is skipped with a warning; the turn still runs.
- If the same always-on context suffices and only Claude runs,
  [agent instructions (apm)](#agent-instructions-apm) / `CLAUDE.md` already
  covers it — use `[prompts]` for CLI-independent delivery, and keep the files
  short.

## Development

```bash
cargo test                          # unit + tmux integration (skips w/o tmux)
MEGURI_TEST_HERDR=1 cargo test      # + herdr integration (needs live herdr)
```

The test suite drives the full loop with a scripted fake agent TUI (`tests/fixtures/fake_agent.sh`) against real tmux, real git worktrees, and a local bare origin — including blocked-dialog handling, lying-agent correction, validation feedback, and crash recovery.

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

meguri deliberately runs a **single loop**: the **worker** (a queued task →
a verified PR, or a verified local branch in local mode). An earlier iteration
grew ten loops (planner, reviewers, a fixer family, cleaner, triage, …) whose
surface outpaced its validation; in 2026-08 the project was pruned back to
this kernel (see [docs/design/kernel-pruning-plan.md](docs/design/kernel-pruning-plan.md)).
The removed mechanisms live on as dormant ADRs
([docs/adr/STATUS.md](docs/adr/STATUS.md)): each returns only when the failure
it solved is actually re-observed under the kernel, with evidence, a declared
deletion condition, and a human review gate.

**Versioning.** meguri is pre-1.0 (`0.x`) and follows [SemVer](https://semver.org): while on `0.x` the public API and CLI are not yet stable, so a minor bump (`0.y`) may carry breaking changes and patches (`0.y.z`) stay compatible; `1.0.0` is when stability is promised. Pin an exact version if you depend on current behavior.

**Releases.** Releases are tag-driven (ADR 0007): a maintainer bumps the version, refreshes `CHANGELOG.md`, and pushes a `vX.Y.Z` tag; `.github/workflows/release.yml` then builds the macOS arm64 / Linux x86_64 binaries, attaches them to a GitHub Release with git-cliff-generated notes, and (once the crate is set up) publishes to crates.io via OIDC Trusted Publishing. Because a pushed tag *is* the release trigger, tag deliberately — a mistaken tag ships a release.

## Contributing

Bug reports and PRs from humans are welcome — normal fork & PR flow, no
`meguri:*` labels to worry about. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
