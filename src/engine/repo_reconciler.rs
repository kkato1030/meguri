//! The Repo Kind reconciler (ADR 0012 slice 4, 決定3 / 決定6). A repo-scoped
//! `observe → next_step_repo → act` pass that will fold the three repo loops
//! (`cleaner` / `triage` / `routing_drift`) plus the managed-clone bootstrap
//! (`ensure_project_clone` → `Op(EnsureClone)`) into one level-triggered Kind,
//! mirroring `schedule.rs` (S2) and `issue_reconciler.rs` (S1–S3).
//!
//! This module ships the **pure decision core** first — the snapshot and the
//! total `next_step_repo` — so its ownership can be property-tested before the
//! observe/act wiring lands. Two clone helpers back 決定6's single readiness
//! contract: [`clone_needs_ensure`] (does this tick need an `EnsureClone`?) and
//! [`clone_ready`] (is the project ready for `repo_path` work after the act?).

use crate::gitops::CloneHealth;

/// The pure inputs [`next_step_repo`] decides on for one project's repo
/// identity. No wall-clock, no I/O: the observe reduces the managed-clone
/// health and the two scan-due predicates into these fields.
#[derive(Debug)]
pub struct RepoSnapshot {
    /// Managed-clone health, or `None` when the project pins `repo_path`
    /// explicitly (not a managed clone — there is nothing to ensure, so it is
    /// always ready). A `Some(_)` is the result of `gitops::clone_health`.
    pub clone_health: Option<CloneHealth>,
    /// A `cleaner` scan is due (head moved past the report marker and the
    /// interval elapsed, or nothing was ever scanned) — the pure form of
    /// `cleaner::needs_scan`.
    pub cleaner_due: bool,
    /// A `triage` scan is due (triage enabled, and the interval / new-issue /
    /// backlog / drift trigger fired) — the pure form of `triage::scan_due`.
    pub triage_due: bool,
}

/// A repo-scoped heavy-agent arm (ADR 0012 §4, `Agent`). Each maps to a
/// `runs.loop_kind` and its `run_*` recipe entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoArm {
    /// The read-only cleaner scan → one report issue (ADR 0003).
    Cleaner,
    /// The triage scan → report / advise / auto (ADR 0017).
    Triage,
}

impl RepoArm {
    /// The `runs.loop_kind` this arm dispatches to (the recipe's `KIND`).
    pub fn loop_kind(self) -> &'static str {
        match self {
            RepoArm::Cleaner => super::cleaner::KIND,
            RepoArm::Triage => super::triage::KIND,
        }
    }
}

/// meguri's own light repo operations (ADR 0012 §4, `Op`). `routing_drift` is
/// **not** here: it recomputes every resync regardless of what else is due, so
/// it is a per-resync act (like the Issue Kind's `reclaim_stale_claims`), never
/// the single owning step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoOp {
    /// Materialize the managed bare clone (決定6; the old `ensure_project_clone`).
    EnsureClone,
}

/// The single decision [`next_step_repo`] returns for one project's repo. The
/// repo is one identity, so exactly one step per resync — the next resync takes
/// the next action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoStep {
    Agent(RepoArm),
    Op(RepoOp),
    Wait(&'static str),
}

/// The pure decision (ADR 0012 §3). Precedence: a not-healthy managed clone
/// must be ensured before anything touches `repo_path` (決定6), then a due
/// cleaner scan, then a due triage scan, else idle. Exactly one owning step for
/// every observed state (the `no gap / no double` property).
pub fn next_step_repo(s: &RepoSnapshot) -> RepoStep {
    if clone_needs_ensure(s.clone_health.as_ref()) {
        return RepoStep::Op(RepoOp::EnsureClone);
    }
    if s.cleaner_due {
        return RepoStep::Agent(RepoArm::Cleaner);
    }
    if s.triage_due {
        return RepoStep::Agent(RepoArm::Triage);
    }
    RepoStep::Wait("nothing due")
}

/// Whether the managed clone must be (re)materialized before `repo_path` work
/// this tick. `None` (not managed) and `Healthy` proceed; `Absent` and `Broken`
/// need an `EnsureClone` (決定6). `Broken`'s act then fails — `ensure_bare_clone`
/// refuses to touch it — so the project drops out of the tick's `ready` set.
pub fn clone_needs_ensure(h: Option<&CloneHealth>) -> bool {
    matches!(h, Some(CloneHealth::Absent) | Some(CloneHealth::Broken(_)))
}

/// Whether the project is ready for `repo_path` work this tick, given the clone
/// health observed **after** any `EnsureClone` act ran (決定6's single readiness
/// contract). Only `Healthy` — or a non-managed project (`None`) — is ready; an
/// `Absent` that failed to clone or a `Broken` remnant is excluded.
pub fn clone_ready(h: Option<&CloneHealth>) -> bool {
    matches!(h, None | Some(CloneHealth::Healthy))
}

/// Observe one project's managed-clone health, or `None` when it is not a
/// managed clone (an explicit `repo_path`, or non-github mode) — there is
/// nothing to ensure, so it is always ready.
pub async fn observe_clone_health(deps: &super::Deps) -> Option<CloneHealth> {
    if deps.project.mode != crate::config::ProjectMode::Github
        || !deps.config.is_managed_clone(&deps.project)
    {
        return None;
    }
    let slug = deps.project.repo_slug.clone()?;
    Some(crate::gitops::clone_health(&deps.repo_path(), &slug).await)
}

/// 決定6's single readiness contract: evaluate the `EnsureClone` Op for one
/// project and return whether it is ready for `repo_path` work this tick. This
/// is the Repo Kind reconcile's first Op, run before every other Kind — the
/// level-triggered replacement for the scheduler's old bootstrap gate. A
/// not-healthy managed clone runs the act (`ensure_project_clone`); the project
/// is ready iff `Healthy` afterwards (a `Broken` remnant or a failed clone
/// stays not-ready, its reason emitted on `repo.clone.failed` for `doctor`).
pub async fn reconcile_ready(deps: &super::Deps) -> bool {
    // observe → decide (the EnsureClone part of `next_step_repo`).
    let health = observe_clone_health(deps).await;
    if !clone_needs_ensure(health.as_ref()) {
        return true; // Healthy or non-managed — ready without acting.
    }
    // act: `Op(EnsureClone)`. `ensure_project_clone` re-checks and clones an
    // `Absent`, bails on a `Broken`, and emits `repo.cloned` / `repo.clone.failed`
    // itself. Ready iff it succeeds (→ `Healthy`).
    match super::ensure_project_clone(deps).await {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("clone prep failed for {}: {e:#}", deps.project.id);
            false
        }
    }
}

/// The Repo Kind reconcile's per-resync pass over one ready project (ADR 0012
/// §決定3): the light `Op(RoutingDrift)` act (the pure-sqlite routing-drift
/// recompute, every resync), then the scan observation → [`next_step_repo`] →
/// the cleaner / triage arm enqueue. The caller (`Scheduler`) only runs this
/// on a `ready` project, so the clone health is already `Healthy`/non-managed
/// — the snapshot reflects that with `None`.
pub async fn reconcile_repo(deps: &super::Deps) -> anyhow::Result<()> {
    super::routing_drift::sweep(deps)?;
    if deps.forge.is_none() {
        return Ok(()); // the scan arms are forge-driven; local mode has none
    }
    // observe: the two scan-due predicates (the old discovers' read-only
    // bodies), each carrying its report-issue key.
    let cleaner_report = super::cleaner::observe_scan_due(deps).await?;
    let triage_report = super::triage::observe_scan_due(deps).await?;
    let snap = RepoSnapshot {
        clone_health: None,
        cleaner_due: cleaner_report.is_some(),
        triage_due: triage_report.is_some(),
    };
    // decide + step policy (決定10): a disabled arm waits, exactly one owner.
    let step = match next_step_repo(&snap) {
        RepoStep::Agent(arm) => {
            let p = &deps.config.reconciler.policy;
            let allowed = match arm {
                RepoArm::Cleaner => p.cleaner,
                RepoArm::Triage => p.triage,
            };
            if allowed {
                RepoStep::Agent(arm)
            } else {
                deps.store.emit(
                    None,
                    "reconciler.policy_disabled",
                    serde_json::json!({ "arm": arm.loop_kind() }),
                )?;
                RepoStep::Wait("policy disabled")
            }
        }
        other => other,
    };
    // act: enqueue the arm's run keyed to its report issue (or 0 before the
    // first report exists) — the same key the old discovery used. The unique
    // (project, loop, issue) run index dedups.
    if let RepoStep::Agent(arm) = step {
        let (report, title) = match arm {
            RepoArm::Cleaner => (cleaner_report, super::cleaner::REPORT_TITLE),
            RepoArm::Triage => (triage_report, super::triage::REPORT_TITLE),
        };
        let issue = report.unwrap_or(0);
        if let Ok(run) =
            deps.store
                .create_run_for_loop(&deps.project.id, arm.loop_kind(), issue, title)
        {
            deps.store.emit(
                Some(&run.id),
                "run.discovered",
                serde_json::json!({ "key": format!("Issue({issue})"), "title": title,
                        "loop": arm.loop_kind() }),
            )?;
            deps.store.emit(
                Some(&run.id),
                "reconciler.enqueued",
                serde_json::json!({ "arm": arm.loop_kind(), "issue": issue }),
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn non_managed_project_is_ready_without_acting() {
        // A project pinning `repo_path` is not a managed clone: observe returns
        // None, so reconcile_ready is ready without running any EnsureClone act.
        use crate::config::{Config, ProjectConfig};
        use crate::store::Store;
        use std::sync::Arc;
        let project = ProjectConfig {
            id: "proj".into(),
            repo_path: Some("/tmp/unused".into()),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            check_command: None,
            worktree_root: None,
            language: None,
            pr: None,
            clean: None,
            triage: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup: Default::default(),
            schedules: Vec::new(),
            autonomy: None,
            cadence: Vec::new(),
            prompts: Default::default(),
            notify: None,
        };
        let deps = super::super::Deps::with_label_source(
            Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            Arc::new(crate::forge::fake::FakeForge::default()),
            Config::default(),
            project,
        );
        assert!(observe_clone_health(&deps).await.is_none());
        assert!(reconcile_ready(&deps).await);
    }

    fn snap(
        clone_health: Option<CloneHealth>,
        cleaner_due: bool,
        triage_due: bool,
    ) -> RepoSnapshot {
        RepoSnapshot {
            clone_health,
            cleaner_due,
            triage_due,
        }
    }

    #[test]
    fn clone_helpers_are_exhaustive_over_the_three_states() {
        // f7: use the real gitops::CloneHealth and cover all three variants plus
        // the non-managed None. needs_ensure and ready are complementary on the
        // states that matter: Healthy/None are ready and need nothing; Absent
        // needs ensure and (until cloned) is not ready; Broken needs ensure and
        // is never ready.
        for (h, need, ready) in [
            (None, false, true),
            (Some(CloneHealth::Healthy), false, true),
            (Some(CloneHealth::Absent), true, false),
            (Some(CloneHealth::Broken("bad remote".into())), true, false),
        ] {
            assert_eq!(clone_needs_ensure(h.as_ref()), need, "{h:?}");
            assert_eq!(clone_ready(h.as_ref()), ready, "{h:?}");
        }
    }

    #[test]
    fn ensure_clone_preempts_every_scan() {
        // A not-healthy clone is the top priority no matter what else is due.
        for &cleaner in &[true, false] {
            for &triage in &[true, false] {
                for h in [CloneHealth::Absent, CloneHealth::Broken("x".into())] {
                    assert_eq!(
                        next_step_repo(&snap(Some(h), cleaner, triage)),
                        RepoStep::Op(RepoOp::EnsureClone),
                    );
                }
            }
        }
    }

    #[test]
    fn cleaner_precedes_triage_precedes_idle_once_clone_is_healthy() {
        // Healthy (or non-managed) clone: the scan precedence is cleaner > triage
        // > idle, and the loop_kind routing matches the recipe KINDs.
        for h in [None, Some(CloneHealth::Healthy)] {
            let cleaner = next_step_repo(&snap(mk(&h), true, true));
            assert_eq!(cleaner, RepoStep::Agent(RepoArm::Cleaner));
            let triage = next_step_repo(&snap(mk(&h), false, true));
            assert_eq!(triage, RepoStep::Agent(RepoArm::Triage));
            let idle = next_step_repo(&snap(mk(&h), false, false));
            assert_eq!(idle, RepoStep::Wait("nothing due"));
        }
        assert_eq!(RepoArm::Cleaner.loop_kind(), super::super::cleaner::KIND);
        assert_eq!(RepoArm::Triage.loop_kind(), super::super::triage::KIND);
    }

    /// Rebuild an `Option<CloneHealth>` for reuse across a loop iteration
    /// (`CloneHealth` is not `Clone`).
    fn mk(h: &Option<CloneHealth>) -> Option<CloneHealth> {
        match h {
            None => None,
            Some(CloneHealth::Healthy) => Some(CloneHealth::Healthy),
            Some(CloneHealth::Absent) => Some(CloneHealth::Absent),
            Some(CloneHealth::Broken(s)) => Some(CloneHealth::Broken(s.clone())),
        }
    }

    #[test]
    fn ownership_is_total_exactly_one_step() {
        // Enumerate the observed state space and assert next_step_repo always
        // returns exactly the expected single owning step (no gap, no double).
        let states = [
            None,
            Some(CloneHealth::Healthy),
            Some(CloneHealth::Absent),
            Some(CloneHealth::Broken("r".into())),
        ];
        for h in &states {
            for &cleaner in &[true, false] {
                for &triage in &[true, false] {
                    let step = next_step_repo(&snap(mk(h), cleaner, triage));
                    let expected = if clone_needs_ensure(h.as_ref()) {
                        RepoStep::Op(RepoOp::EnsureClone)
                    } else if cleaner {
                        RepoStep::Agent(RepoArm::Cleaner)
                    } else if triage {
                        RepoStep::Agent(RepoArm::Triage)
                    } else {
                        RepoStep::Wait("nothing due")
                    };
                    assert_eq!(step, expected, "{h:?} cleaner={cleaner} triage={triage}");
                }
            }
        }
    }
}
