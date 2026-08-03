//! The Issue Kind reconciler (ADR 0012): a level-triggered
//! **observe → next_step_issue → act** pass over every open issue, plus the
//! thin merged/closed observation that drives `reaper::finalize`. The PR-side
//! merge tail / fixer arms / auto-merge are dormant (docs/adr/STATUS.md);
//! merged-PR detection moves to the git protocol in the authority-inversion
//! slice (kernel-pruning-plan Phase 5).

use anyhow::Result;
use serde_json::json;

use super::Deps;
use crate::forge;

/// Head-branch prefix identifying meguri's own PRs.
pub const MEGURI_BRANCH_PREFIX: &str = "meguri/";

/// The tracked issue a PR closes, parsed strictly from the first body line
/// meguri always writes (`flow.rs`: `"Closes #{n}.\n\n..."`). Anything else is
/// None — a PR without both the `meguri/` branch convention and this link is
/// out of scope.
pub fn linked_issue(body: &str) -> Option<i64> {
    body.lines()
        .next()?
        .trim()
        .strip_prefix("Closes #")?
        .strip_suffix('.')?
        .parse::<i64>()
        .ok()
}

/// Watch-poll sweep: the thin merged/closed observation (one open-PR listing)
/// feeding the Finalize pass, then the issue-side reconcile. A per-identity
/// failure warns and is retried next poll; it never aborts the sweep.
pub async fn sweep(deps: &Deps) -> Result<()> {
    if deps.forge.is_none() {
        // Local mode has no PRs, but the Finalize pass (決定4) still runs: it
        // clears dead-pane mappings without a forge, and its worktree side
        // parks everything as StateUnknown (finding 4a — a local deliverable
        // is the branch + worktree, never reclaimed on "no open issue").
        if let Err(e) = super::reaper::finalize(deps, &std::collections::HashSet::new()).await {
            tracing::warn!("finalize failed for {}: {e:#}", deps.project.id);
        }
        // The local decider (決定1 / f1): local `TaskKey::Local` identities.
        if let Err(e) = reconcile_local(deps).await {
            tracing::warn!("local reconcile failed for {}: {e:#}", deps.project.id);
        }
        return Ok(());
    }

    // Op(Finalize): reclaim the local resources (panes / worktrees / merged
    // branches) of identities that reached terminal. The exclusion set is this
    // resync's own observation: an identity with an *open* meguri PR keeps its
    // resources even when the issue is closed (finding 4b).
    let open_pr_issues = super::reaper::open_meguri_pr_issues(deps).await;
    if let Err(e) = super::reaper::finalize(deps, &open_pr_issues).await {
        tracing::warn!("finalize failed for {}: {e:#}", deps.project.id);
    }

    // Issue-side reconcile (ADR 0012 §決定1): one bulk observe of every open
    // issue, the pure `next_step_issue` per identity, then enqueue the chosen
    // worker arm.
    if let Err(e) = reconcile_issues(deps, &open_pr_issues).await {
        tracing::warn!("issue-side reconcile failed for {}: {e:#}", deps.project.id);
    }
    Ok(())
}

pub async fn reconcile_issues(
    deps: &Deps,
    open_pr_issues: &std::collections::HashSet<i64>,
) -> Result<()> {
    let issues = deps.forge().list_open_issues().await?;
    for issue in issues {
        if let Err(e) = process_issue_identity(deps, &issue, open_pr_issues).await {
            tracing::warn!("issue reconcile failed for #{}: {e:#}", issue.number);
        }
    }
    Ok(())
}

/// One issue identity through observe-reduce → `next_step_issue` → act.
async fn process_issue_identity(
    deps: &Deps,
    issue: &forge::Issue,
    open_pr_issues: &std::collections::HashSet<i64>,
) -> Result<()> {
    let snap = build_issue_snapshot(deps, issue, open_pr_issues).await?;
    match next_step_issue(&snap, Mode::Reconcile) {
        IssueStep::Agent(arm) => {
            // The issue-wide reservation was read into `issue_busy`; the
            // per-loop unique run index is the atomic backstop — a create
            // failure is a benign race, retried next resync.
            if let Ok(run) = deps.store.create_run_for_loop(
                &deps.project.id,
                arm.loop_kind(),
                issue.number,
                &issue.title,
            ) {
                deps.store.emit(
                    Some(&run.id),
                    "run.discovered",
                    json!({ "key": format!("Issue({})", issue.number),
                            "title": issue.title, "loop": arm.loop_kind() }),
                )?;
                deps.store.emit(
                    Some(&run.id),
                    "reconciler.enqueued",
                    json!({ "arm": arm.loop_kind(), "issue": issue.number }),
                )?;
            }
        }
        IssueStep::Wait(reason) | IssueStep::Skip(reason) => {
            tracing::debug!("issue #{}: reconciler — {reason}", issue.number);
        }
    }
    Ok(())
}

/// Reduce one open issue to the pure [`IssueSnapshot`]. Shared by the
/// reconcile pass and the operator surface (manual `run`, ADR 0016).
pub async fn build_issue_snapshot(
    deps: &Deps,
    issue: &forge::Issue,
    open_pr_issues: &std::collections::HashSet<i64>,
) -> Result<IssueSnapshot> {
    let has = |l: &str| issue.has_label(l);
    let mut snap = IssueSnapshot {
        human_stop: has(forge::LABEL_HOLD) || has(forge::LABEL_NEEDS_HUMAN),
        has_open_meguri_pr: open_pr_issues.contains(&issue.number),
        // `working` label OR run liveness (finding 3: the label skip is
        // doubled with `issue_busy` so a stale label from a crashed run
        // cannot deadlock, and a live run without its label cannot double).
        issue_busy: has(forge::LABEL_WORKING)
            || deps
                .store
                .issue_has_active_author_run(&deps.project.id, issue.number)?,
        has_ready: has(forge::LABEL_READY),
        has_implementing: has(forge::LABEL_IMPLEMENTING),
        already_shipped: false,
        deps_unmet: false,
    };
    // The discovery gates cost API calls (dependencies), so they are only
    // evaluated when the decider would otherwise reach a new-work arm — the
    // same laziness the old per-label discover had.
    let reaches_new_work =
        !snap.human_stop && !snap.has_open_meguri_pr && !snap.issue_busy && snap.has_ready;
    if reaches_new_work {
        match deps
            .task_source
            .evaluate_issue(super::worker::KIND, issue)
            .await?
        {
            crate::tasks::GateVerdict::Pass => {}
            crate::tasks::GateVerdict::Shipped => snap.already_shipped = true,
            crate::tasks::GateVerdict::Blocked => snap.deps_unmet = true,
        }
    }
    Ok(snap)
}

/// The local-mode reconcile (決定1 / f1, third decider): the local task
/// source's discover is the bulk observation, `next_step_local` decides, the
/// unique (project, loop, task) run index dedups the enqueue.
async fn reconcile_local(deps: &Deps) -> Result<()> {
    for task in deps
        .task_source
        .discover(crate::tasks::TaskKind::Work)
        .await?
    {
        let crate::tasks::TaskKey::Local(id) = task.key else {
            continue;
        };
        // The source already applied the local gates (not-before, status);
        // the snapshot reflects the observed post-gate state.
        let snap = LocalSnapshot {
            human_stop: false,
            issue_busy: false,
            already_shipped: false,
            deps_unmet: false,
        };
        if let LocalStep::Agent(LocalArm::Worker) = next_step_local(&snap, Mode::Reconcile)
            && let Ok(run) = deps.store.create_run_for_task(
                &deps.project.id,
                super::worker::KIND,
                id,
                &task.title,
            )
        {
            deps.store.emit(
                Some(&run.id),
                "run.discovered",
                json!({ "key": format!("Local({id})"), "title": task.title,
                        "loop": super::worker::KIND }),
            )?;
            deps.store.emit(
                Some(&run.id),
                "reconciler.enqueued",
                json!({ "arm": super::worker::KIND, "task": id }),
            )?;
        }
    }
    Ok(())
}

// ===========================================================================
// Issue-side decider (ADR 0012 slice 4, 決定1). The PR-side above owns open
// meguri PRs; this side owns the pre-PR / non-open-PR issue lifecycle
// (`plan`→planner, `ready`→worker, merged spec PR→handoff), plus a local-task
// decider for local mode. Pure functions with their own property tests; the
// observe/enqueue wiring (single-issue snapshot, issue-wide reservation,
// arm-tagged claim) folds planner/worker/plan_handoff here in a following step.
// The types are issue-scoped (`Issue*`) so they do not disturb the PR-side
// `Snapshot`/`Step`/`Arm`/`Op` above; a later step unifies the vocabulary.
// ===========================================================================

/// How the decider was reached: the normal watch resync, or an explicit manual
/// `meguri run` (ADR 0016). `ManualRun` bypasses the *discovery throttles*
/// (`already_shipped` / cadence window) — a human override — but never the
/// safety gates (human stop / busy / not-before), per finding 2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Reconcile,
    ManualRun,
}

/// A pre-PR / non-open-PR issue arm (ADR 0012 §4, `Agent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueArm {
    /// `meguri:ready` → implement (worker recipe).
    Worker,
}

impl IssueArm {
    /// The `runs.loop_kind` this arm dispatches to (the recipe's `KIND`).
    pub fn loop_kind(self) -> &'static str {
        match self {
            IssueArm::Worker => super::worker::KIND,
        }
    }
}

/// The decision [`next_step_issue`] returns for one issue identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueStep {
    Agent(IssueArm),
    Wait(&'static str),
    Skip(&'static str),
}

/// The pure inputs [`next_step_issue`] decides on: one issue's full label set
/// reduced to phase booleans, plus the ownership/serialization gates and the
/// discovery gate predicates for the chosen new-work arm (決定1). Deliberately
/// total so a property test can enumerate it.
#[derive(Debug, Clone, Copy)]
pub struct IssueSnapshot {
    /// A human parked/paused the issue (`hold` / `needs-human`, spec axis).
    /// Respected even under `ManualRun` (finding 2).
    pub human_stop: bool,
    /// The issue has an **open meguri PR** — the ownership boundary hands it to
    /// the PR-side `next_step`; the issue side stays off it (決定1).
    pub has_open_meguri_pr: bool,
    /// A live author-lane run already owns the issue (`issue_has_active_author_run`).
    pub issue_busy: bool,
    /// Phase labels present. Priority `ready` > `implementing`; multiple set
    /// (manual drift) still resolves to one arm.
    pub has_ready: bool,
    pub has_implementing: bool,
    /// Discovery gates for the chosen worker arm (現 `LabelTaskSource`
    /// と同じ判定関数の結果を畳んだ純入力):
    /// already shipped (a succeeded run covers this issue).
    pub already_shipped: bool,
    /// A `blocked_by` dependency is still open.
    pub deps_unmet: bool,
}

/// The pure decision (ADR 0012 §3, 決定1). Precedence: the ownership /
/// serialization gates first (human stop, open-PR boundary, busy), then the
/// single phase arm by priority, with the discovery gates applied to the chosen
/// new-work arm. Every observed state is owned by exactly one step.
pub fn next_step_issue(s: &IssueSnapshot, mode: Mode) -> IssueStep {
    // Human stop is final for every arm and honored even under ManualRun.
    if s.human_stop {
        return IssueStep::Wait("human stop (hold/needs-human)");
    }
    // Ownership boundary: an open meguri PR is the PR side's (決定1). A stray
    // open-PR speccing issue lands here too, so the boundary is total.
    if s.has_open_meguri_pr {
        return IssueStep::Skip("owned by its open PR");
    }
    // A live author-lane run already owns the issue — stay off it (serialize).
    if s.issue_busy {
        return IssueStep::Skip("a live run owns the issue");
    }
    // Phase priority: exactly one arm. `hold`/`needs-human` was folded into
    // `human_stop` above, so the ladder is ready > implementing.
    if s.has_ready {
        return gated_new_work(IssueArm::Worker, s, mode);
    }
    if s.has_implementing {
        return IssueStep::Skip("implementing (in progress)");
    }
    IssueStep::Skip("no actionable phase label")
}

/// Apply the discovery gates to the chosen worker arm. The dependency gate
/// holds under both modes (fail-closed); `already_shipped` is the discovery
/// throttle a manual run bypasses.
fn gated_new_work(arm: IssueArm, s: &IssueSnapshot, mode: Mode) -> IssueStep {
    if s.deps_unmet {
        return IssueStep::Wait("blocked by an open dependency");
    }
    if mode == Mode::Reconcile && s.already_shipped {
        return IssueStep::Skip("already shipped");
    }
    IssueStep::Agent(arm)
}

/// A local-task arm — local mode has no planner/PR, so only the worker (決定1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalArm {
    Worker,
}

/// The decision for a local (`TaskKey::Local`) identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalStep {
    Agent(LocalArm),
    Wait(&'static str),
    Skip(&'static str),
}

/// The pure inputs for a local task (a subset of [`IssueSnapshot`]: no phase
/// labels / PR / handoff — local mode has none).
#[derive(Debug, Clone, Copy)]
pub struct LocalSnapshot {
    pub human_stop: bool,
    pub issue_busy: bool,
    pub already_shipped: bool,
    pub deps_unmet: bool,
}

/// The pure decision for a local task (決定1, third decider). Same gate ladder
/// as the issue side's worker arm, but the only arm is the worker.
pub fn next_step_local(s: &LocalSnapshot, mode: Mode) -> LocalStep {
    if s.human_stop {
        return LocalStep::Wait("human stop (hold/needs-human)");
    }
    if s.issue_busy {
        return LocalStep::Skip("a live run owns the task");
    }
    if s.deps_unmet {
        return LocalStep::Wait("blocked by an open dependency");
    }
    if mode == Mode::Reconcile && s.already_shipped {
        return LocalStep::Skip("already shipped");
    }
    LocalStep::Agent(LocalArm::Worker)
}

#[cfg(test)]
mod issue_side_tests {
    use super::*;

    /// A baseline: a plain `ready` issue with no gates tripped, not owned by a
    /// PR, not busy. Tests tweak one field.
    fn ready_snapshot() -> IssueSnapshot {
        IssueSnapshot {
            human_stop: false,
            has_open_meguri_pr: false,
            issue_busy: false,
            has_ready: true,
            has_implementing: false,
            already_shipped: false,
            deps_unmet: false,
        }
    }

    #[test]
    fn phase_priority_picks_exactly_one_arm() {
        // ready → worker.
        assert_eq!(
            next_step_issue(&ready_snapshot(), Mode::Reconcile),
            IssueStep::Agent(IssueArm::Worker)
        );
        assert_eq!(IssueArm::Worker.loop_kind(), super::super::worker::KIND);
    }

    #[test]
    fn ownership_and_serialization_gates_come_first() {
        // Human stop wins even under ManualRun.
        let stopped = IssueSnapshot {
            human_stop: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&stopped, Mode::ManualRun),
            IssueStep::Wait("human stop (hold/needs-human)")
        );
        // An open meguri PR hands the issue to the PR side.
        let owned = IssueSnapshot {
            has_open_meguri_pr: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&owned, Mode::Reconcile),
            IssueStep::Skip("owned by its open PR")
        );
        // A live author-lane run serializes.
        let busy = IssueSnapshot {
            issue_busy: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&busy, Mode::Reconcile),
            IssueStep::Skip("a live run owns the issue")
        );
    }

    #[test]
    fn discovery_gates_hold_under_reconcile_and_manual_bypasses_throttles() {
        // Blocked / already-shipped do not enqueue under Reconcile.
        let shipped = IssueSnapshot {
            already_shipped: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&shipped, Mode::Reconcile),
            IssueStep::Skip("already shipped")
        );
        let blocked = IssueSnapshot {
            deps_unmet: true,
            ..ready_snapshot()
        };
        assert_eq!(
            next_step_issue(&blocked, Mode::Reconcile),
            IssueStep::Wait("blocked by an open dependency")
        );
        // ManualRun bypasses already_shipped …
        assert_eq!(
            next_step_issue(&shipped, Mode::ManualRun),
            IssueStep::Agent(IssueArm::Worker)
        );
        // … but a dependency block still holds (fail-closed).
        assert_eq!(
            next_step_issue(&blocked, Mode::ManualRun),
            IssueStep::Wait("blocked by an open dependency")
        );
    }

    #[test]
    fn local_decider_only_yields_worker_and_respects_the_same_gates() {
        let base = LocalSnapshot {
            human_stop: false,
            issue_busy: false,
            already_shipped: false,
            deps_unmet: false,
        };
        assert_eq!(
            next_step_local(&base, Mode::Reconcile),
            LocalStep::Agent(LocalArm::Worker)
        );
        assert_eq!(
            next_step_local(
                &LocalSnapshot {
                    human_stop: true,
                    ..base
                },
                Mode::ManualRun
            ),
            LocalStep::Wait("human stop (hold/needs-human)")
        );
        // ManualRun bypasses already_shipped for a local task too.
        assert_eq!(
            next_step_local(
                &LocalSnapshot {
                    already_shipped: true,
                    ..base
                },
                Mode::ManualRun
            ),
            LocalStep::Agent(LocalArm::Worker)
        );
    }

    #[test]
    fn ownership_is_total_exactly_one_step_over_the_phase_space() {
        // Enumerate the observed issue-side state space and assert next_step_issue
        // always returns exactly the expected single owning step (no gap, no
        // double) under both modes. The phase powerset × the gate flags × PR
        // ownership × busy is the state space; the expected owner mirrors the
        // precedence ladder.
        for &human_stop in &[true, false] {
            for &has_open_pr in &[true, false] {
                for &busy in &[true, false] {
                    for &ready in &[true, false] {
                        for &implementing in &[true, false] {
                            for &shipped in &[true, false] {
                                for &deps in &[true, false] {
                                    for mode in [Mode::Reconcile, Mode::ManualRun] {
                                        let s = IssueSnapshot {
                                            human_stop,
                                            has_open_meguri_pr: has_open_pr,
                                            issue_busy: busy,
                                            has_ready: ready,
                                            has_implementing: implementing,
                                            already_shipped: shipped,
                                            deps_unmet: deps,
                                        };
                                        assert_eq!(
                                            next_step_issue(&s, mode),
                                            expected(&s, mode),
                                            "{s:?} {mode:?}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The reference precedence, independently spelled out, that the property
    /// test above checks `next_step_issue` against.
    fn expected(s: &IssueSnapshot, mode: Mode) -> IssueStep {
        if s.human_stop {
            return IssueStep::Wait("human stop (hold/needs-human)");
        }
        if s.has_open_meguri_pr {
            return IssueStep::Skip("owned by its open PR");
        }
        if s.issue_busy {
            return IssueStep::Skip("a live run owns the issue");
        }
        let gated = |arm: IssueArm| {
            if s.deps_unmet {
                IssueStep::Wait("blocked by an open dependency")
            } else if mode == Mode::Reconcile && s.already_shipped {
                IssueStep::Skip("already shipped")
            } else {
                IssueStep::Agent(arm)
            }
        };
        if s.has_ready {
            gated(IssueArm::Worker)
        } else if s.has_implementing {
            IssueStep::Skip("implementing (in progress)")
        } else {
            IssueStep::Skip("no actionable phase label")
        }
    }
}
