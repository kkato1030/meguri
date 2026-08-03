//! The Repo Kind reconciler (ADR 0012 slice 4, 決定3 / 決定6), reduced to the
//! managed-clone bootstrap: `observe → clone_needs_ensure → Op(EnsureClone)`.
//! The scan arms (`cleaner` / `triage`) and the per-resync `routing_drift` act
//! are dormant (docs/adr/STATUS.md); two clone helpers back 決定6's single
//! readiness contract: [`clone_needs_ensure`] (does this tick need an
//! `EnsureClone`?) and [`clone_ready`] (is the project ready for `repo_path`
//! work after the act?).

use crate::gitops::CloneHealth;

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
    // observe → decide (the EnsureClone part of the repo step).
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
            worktree_setup: Default::default(),
            prompts: Default::default(),
        };
        let deps = super::super::Deps::with_github_source(
            Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            Arc::new(crate::forge::fake::FakeForge::default()),
            Config::default(),
            project,
        );
        assert!(observe_clone_health(&deps).await.is_none());
        assert!(reconcile_ready(&deps).await);
    }

    #[test]
    fn clone_helpers_are_exhaustive_over_the_three_states() {
        // Cover all three CloneHealth variants plus the non-managed None.
        // needs_ensure and ready are complementary on the states that matter:
        // Healthy/None are ready and need nothing; Absent needs ensure and
        // (until cloned) is not ready; Broken needs ensure and is never ready.
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
}
