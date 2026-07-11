# meguri（巡り）

*日本語版は [README.ja.md](README.ja.md) をご覧ください。*

**Run AI coding agents on a loop — inside your terminal multiplexer, so you can step in anytime.**

meguri is a reimplementation of the ideas in [nexu-io/looper](https://github.com/nexu-io/looper) with one deliberate architectural difference: instead of headless one-shot agent runs (`claude --print …`), meguri runs each agent as a **live interactive session in a [herdr](https://herdr.dev) or tmux pane**. The orchestrator injects prompts and waits for results, while you can attach to the pane at any moment — watch, type extra instructions, answer permission dialogs, or take over completely — without breaking the loop.

```
GitHub issue (label: meguri:ready)
        │  discover & claim (meguri:working)
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
git push + PR (Closes #N) — labels settled
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

## Install & set up

Prereqs: `git`, [`gh`](https://cli.github.com) (authenticated), an agent CLI (`claude` by default), and a multiplexer — a running [herdr](https://herdr.dev) (recommended; native agent-state detection) or `tmux` (screen-heuristic fallback).

```bash
cargo install --path .   # or: cargo build --release
meguri init              # writes ~/.meguri/config.toml, creates the db
meguri doctor            # checks gh auth, mux, agent CLI
```

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
meguri logs <run>         # event trail + live pane tail
meguri serve              # read-only web dashboard on http://127.0.0.1:8607
meguri attach <run>       # jump into the agent's pane
meguri pause <run>        # stop injecting prompts; pane stays alive
meguri resume <run>
meguri takeover <run>     # orchestrator hands-off; you drive
meguri handback <run>
meguri stop <run>         # kill pane, release the claim, cancel
meguri prune              # reclaim panes + worktrees of closed issues (--dry-run / --force)
```

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

### Web dashboard

`meguri serve` starts a read-only dashboard at `http://127.0.0.1:8607` (override with `--port` / `--bind` or the `[server]` config section): a runs table like `meguri ps` with `awaiting_human` runs highlighted front and center, plus a per-run page with the event trail, a terminal-style pane tail, turn history, and the attach command ready to copy. It is an independent process that reads the same sqlite database — it works even when `meguri watch` is not running, and shows watch liveness from the heartbeat the scheduler writes each tick. There is no authentication, so it binds loopback by default; binding anything else prints a warning.

### Labels

| label | meaning |
|---|---|
| `meguri:ready` | you queue an issue for the worker loop |
| `meguri:plan` | you queue an issue for the planner loop (opt-in spec-first flow) |
| `meguri:spec-reviewing` | on the spec PR: awaiting review by the reviewer loop (or a human) |
| `meguri:spec-ready` | on the spec PR: review passed; the worker continues implementation |
| `meguri:working` | meguri claimed it (removed when the PR opens) |
| `meguri:hold` | discovery skips this issue |
| `meguri:needs-human` | meguri gave up; a comment explains why |
| `meguri:clean-report` | the cleaner loop's per-project report issue (put `meguri:hold` on it to pause the sweep) |

Discovery also honors GitHub-native issue dependencies (looper's ADR-0004): an issue *blocked by* another is skipped — silently, no label or comment — until every blocker is closed as **completed**. Blockers closed as *not planned* / *duplicate* don't count as resolved (the dependent issue awaits human re-triage), and unreadable blockers are treated as unresolved.

### Spec-first flow (opt-in)

Label an issue `meguri:plan` instead of `meguri:ready` and the **planner** loop investigates the repository and opens a *spec PR* (`Spec: <title>`) containing a single lightweight file, `docs/specs/issue-<N>.md` (acceptance criteria, files to touch, key decisions), labeled `meguri:spec-reviewing`. The **reviewer** loop then reviews the spec PR: findings are posted as a summary comment (push fixes and it re-reviews the new head; each head is reviewed only once), and a clean review flips the label to `meguri:spec-ready` — you can also flip it yourself. The worker then continues implementation **on the same branch and PR** — the spec and the implementation merge once, together. The spec itself is disposable review scaffolding: the spec worker deletes it as part of the implementation, so `docs/specs/` never accumulates on the default branch — anything worth keeping (design decisions, domain rules) is routed to an ADR (`docs/adr/`) or a permanent domain document instead.

### Cleaner (read-only repository sweeps)

The **cleaner** loop periodically walks the default branch head and reports accumulated divergence — spec/implementation drift, dead-code candidates, convention violations, stranded TODOs, stale remote branches, orphaned `meguri:working` labels — into a single per-project issue labeled `meguri:clean-report`. It never fixes anything: its only write is creating/updating that one issue (no pushes, no branch operations, no labels or comments elsewhere). The body is a snapshot rewritten on every sweep, with a hidden head-sha marker so the same head is never swept twice; a moved head triggers a new sweep only after `clean.interval_hours`. To act on a finding, open a regular issue and label it `meguri:plan` / `meguri:ready`; to silence a false positive, add a substring to `clean.ignore`; to pause the loop, put `meguri:hold` on the report issue.

Labels and comments on GitHub are the durable workflow state (looper's "Authority" principle); the local sqlite (`~/.meguri/meguri.sqlite`) only tracks run execution. Kill meguri any time — `meguri watch` recovers: live panes are re-adopted, dead runs resume from their last checkpointed step. Panes and worktrees live per issue (1 issue = 1 pane; later runs on the same issue reuse the live session): while watching, meguri reclaims the pane, worktree, and merged local branch of every issue that closes, saving the agent's native session id first so `claude --resume <id>` can restore the context. `meguri prune` does the same on demand for one-shot usage.

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
session = "meguri"     # herdr workspace label / tmux session name
# Panes live per issue (1 issue = 1 pane) and are reclaimed when the issue
# closes; the agent's native session id is saved first (claude --resume <id>).
# "never" kills the pane as soon as its run ends (high-throughput operation).
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

[server]
port = 8607            # meguri serve listen port
bind = "127.0.0.1"     # no auth — keep it loopback unless you know your network

[notifications]
macos = true           # page awaiting_human via a macOS notification (osascript)
# webhook_url = "https://example.com/hook"  # JSON POST: run id / issue / reason / attach
throttle_secs = 60     # min seconds between notifications for the same run

[pr]
draft = true   # open PRs as drafts; override per project with [projects.pr]

[clean]
interval_hours = 24     # min hours between cleaner sweeps (a moved head alone doesn't trigger one)
stale_branch_days = 30  # remote branches older than this are reported as stale
ignore = []             # substrings that silence false positives; override per project with [projects.clean]
```

## Development

```bash
cargo test                          # unit + tmux integration (skips w/o tmux)
MEGURI_TEST_HERDR=1 cargo test      # + herdr integration (needs live herdr)
```

The test suite drives the full loop with a scripted fake agent TUI (`tests/fixtures/fake_agent.sh`) against real tmux, real git worktrees, and a local bare origin — including blocked-dialog handling, lying-agent correction, validation feedback, and crash recovery.

## Status / roadmap

Seven loops run on GitHub today, mirroring looper's role model as `Loop` implementations sharing the same turn engine: the **worker** (issue → PR), the **planner** (`meguri:plan` issue → spec PR), the **reviewer** (`meguri:spec-reviewing` PR → summary review → `meguri:spec-ready`), the **spec worker** (`meguri:spec-ready` PR → implementation commits on the same branch and PR), the **fixer** (unresolved review comments on a meguri PR → fix commits pushed to it), the **conflict resolver** (a CONFLICTING meguri PR → the base branch merged, conflicts resolved, merge commit pushed), and the **cleaner** (periodic read-only sweep → divergence report in a single `meguri:clean-report` issue).

## License

MIT
