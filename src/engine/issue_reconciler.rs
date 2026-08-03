//! The task reconciler (ADR 0012 / kernel-pruning-plan Phase 5, 権威反転):
//! **the sqlite `tasks` table is the authority**; GitHub is read as a
//! low-frequency edge signal and written as a best-effort projection.
//!
//! - [`intake`] (github mode, on the intake cadence): import `meguri:ready`
//!   issues as task rows, sync `meguri:hold` ↔ held, and re-queue an
//!   escalated task once a human cleared `meguri:needs-human`. Two list
//!   requests per pass — the only recurring GitHub reads.
//! - [`sweep`] (every tick): the Finalize pass (reclaim panes / worktrees /
//!   merged branches of terminal identities) plus the task decider — queued
//!   rows become `queued` runs, dedup'd by the unique active-run indexes.

use anyhow::Result;
use serde_json::json;

use super::Deps;
use crate::forge;
use crate::tasks::{github_origin, has_unresolved_blockers, origin_issue};

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
        .trim_end_matches('.')
        .parse()
        .ok()
}

/// The intake pass (権威反転の入口, on the intake cadence): GitHub labels →
/// task rows. Read-only on the forge, idempotent, and the ONLY place issue
/// listings are read.
pub async fn intake(deps: &Deps) -> Result<()> {
    let Some(forge_ref) = deps.forge.as_ref() else {
        return Ok(());
    };

    // 1. `meguri:ready` → import (once per label application: a done row does
    //    not block a re-import, so re-adding the label re-queues the issue).
    let ready = forge_ref.list_issues_with_label(forge::LABEL_READY).await?;
    for issue in &ready {
        let origin = github_origin(issue.number);
        let live = deps
            .store
            .task_by_origin(&deps.project.id, &origin)?
            .is_some_and(|r| {
                matches!(
                    r.status.as_str(),
                    "queued" | "claimed" | "needs_human" | "held"
                )
            });
        if live {
            continue;
        }
        // Permanent suppression: a succeeded worker run covers this issue —
        // a lingering ready label (e.g. the complete-time projection write
        // failed) must not re-file a duplicate. An intentional re-run goes
        // through `meguri run --issue N`.
        if deps.store.issue_has_succeeded_run(
            &deps.project.id,
            super::worker::KIND,
            issue.number,
        )? {
            continue;
        }
        let row = deps.store.create_task(
            &deps.project.id,
            crate::tasks::TaskKind::Work.as_str(),
            &issue.title,
            &issue.body,
            &origin,
        )?;
        deps.store.emit(
            None,
            "task.imported",
            json!({ "task": row.id, "issue": issue.number, "title": issue.title }),
        )?;
    }

    // 2. `meguri:hold` ↔ held (the phone-operable emergency stop): a hold
    //    label holds the queued row; a removed hold releases it. In-flight
    //    runs are not interrupted — hold gates *dispatch* (stop a live run
    //    with `meguri stop`).
    let held_issues = forge_ref.list_issues_with_label(forge::LABEL_HOLD).await?;
    let held_set: std::collections::HashSet<i64> = held_issues.iter().map(|i| i.number).collect();
    for issue in &held_issues {
        if let Some(row) = deps
            .store
            .task_by_origin(&deps.project.id, &github_origin(issue.number))?
            && row.status == "queued"
            && deps.store.hold_task(row.id)?
        {
            deps.store.emit(
                None,
                "task.held",
                json!({ "task": row.id, "issue": issue.number }),
            )?;
        }
    }
    for row in deps.store.held_tasks(&deps.project.id)? {
        let Some(issue) = origin_issue(&row.origin) else {
            continue; // a locally held row is the CLI's to release
        };
        if !held_set.contains(&issue) && deps.store.unhold_task(row.id)? {
            deps.store.emit(
                None,
                "task.unheld",
                json!({ "task": row.id, "issue": issue }),
            )?;
        }
    }

    // 3. Retraction: a queued github row whose issue no longer carries
    //    `meguri:ready` was cancelled by the human — cancel the row, or the
    //    decider would re-file a run every tick that dies at the claim's
    //    label re-verification, forever. (A held row waits on its hold label;
    //    a claimed row is in flight and is the CLI's to stop.)
    let ready_set: std::collections::HashSet<i64> = ready.iter().map(|i| i.number).collect();
    for row in deps.store.queued_tasks_any(&deps.project.id)? {
        let Some(issue) = origin_issue(&row.origin) else {
            continue; // a locally seeded row has no label to sync
        };
        if !ready_set.contains(&issue)
            && !held_set.contains(&issue)
            && deps.store.cancel_task(row.id)?
        {
            deps.store.emit(
                None,
                "task.retracted",
                json!({ "task": row.id, "issue": issue }),
            )?;
        }
    }

    // 4. needs_human → queued once the human cleared the label (and the issue
    //    still carries `meguri:ready`). The ready listing is the signal: an
    //    escalated task's issue keeps its ready label (only `complete` drops
    //    it), so it appears in `ready` above.
    for issue in &ready {
        if issue.has_label(forge::LABEL_NEEDS_HUMAN) || held_set.contains(&issue.number) {
            continue;
        }
        if let Some(row) = deps
            .store
            .task_by_origin(&deps.project.id, &github_origin(issue.number))?
            && row.status == "needs_human"
            && deps.store.requeue_task(row.id)?
        {
            deps.store.emit(
                None,
                "task.requeued",
                json!({ "task": row.id, "issue": issue.number }),
            )?;
        }
    }
    Ok(())
}

/// Watch-poll sweep (every tick): Finalize, then the task decider. A
/// per-identity failure warns and is retried next poll; it never aborts the
/// sweep.
pub async fn sweep(deps: &Deps) -> Result<()> {
    // Op(Finalize): reclaim the local resources (panes / worktrees / merged
    // branches) of identities that reached terminal (決定4).
    if let Err(e) = super::reaper::finalize(deps).await {
        tracing::warn!("finalize failed for {}: {e:#}", deps.project.id);
    }
    if let Err(e) = reconcile_tasks(deps).await {
        tracing::warn!("task reconcile failed for {}: {e:#}", deps.project.id);
    }
    Ok(())
}

/// The task decider: every queued row becomes a `queued` run — keyed by the
/// origin issue for a github task (runs/panes/worktrees stay issue-keyed), by
/// the row id otherwise. The unique active-run indexes are the atomic dedup;
/// a create failure is a benign race retried next resync.
async fn reconcile_tasks(deps: &Deps) -> Result<()> {
    let now = crate::store::format_epoch(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    for row in deps.store.queued_tasks(
        &deps.project.id,
        crate::tasks::TaskKind::Work.as_str(),
        &now,
    )? {
        match origin_issue(&row.origin) {
            Some(issue) => {
                // The dependency gate (fail-closed): an unresolved
                // `blocked_by` keeps the task queued. Read per actionable
                // candidate only — queued rows are few.
                if deps.forge.is_some() && has_unresolved_blockers(&**deps.forge(), issue).await {
                    continue;
                }
                if let Ok(run) = deps.store.create_run_for_loop(
                    &deps.project.id,
                    super::worker::KIND,
                    issue,
                    &row.title,
                ) {
                    deps.store.emit(
                        Some(&run.id),
                        "run.discovered",
                        json!({ "key": format!("Issue({issue})"), "title": row.title,
                                "loop": super::worker::KIND }),
                    )?;
                }
            }
            None => {
                if let Ok(run) = deps.store.create_run_for_task(
                    &deps.project.id,
                    super::worker::KIND,
                    row.id,
                    &row.title,
                ) {
                    deps.store.emit(
                        Some(&run.id),
                        "run.discovered",
                        json!({ "key": format!("Local({})", row.id), "title": row.title,
                                "loop": super::worker::KIND }),
                    )?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, ProjectConfig};
    use crate::forge::LABEL_READY;
    use crate::forge::fake::FakeForge;
    use crate::store::Store;

    fn deps_with(forge: Arc<FakeForge>) -> Deps {
        let project = ProjectConfig {
            id: "proj".into(),
            repo_path: "/tmp/unused".into(),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            profile: None,
            worktree_root: None,
            pr: None,
            worktree_setup: Default::default(),
            prompts: Default::default(),
        };
        Deps::with_github_source(
            Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge,
            Config::default(),
            project,
        )
    }

    #[test]
    fn linked_issue_parses_the_closes_line_strictly() {
        assert_eq!(linked_issue("Closes #5.\n\nBody."), Some(5));
        assert_eq!(linked_issue("Closes #5"), Some(5));
        assert_eq!(linked_issue("Refs #5.\n"), None);
        assert_eq!(linked_issue("Something else"), None);
    }

    #[tokio::test]
    async fn intake_imports_ready_issues_once() {
        let forge = Arc::new(FakeForge::with_issue(7, "T", "B", &[LABEL_READY]));
        let deps = deps_with(forge);
        intake(&deps).await.unwrap();
        let row = deps
            .store
            .task_by_origin("proj", "github:7")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "queued");
        // Idempotent: a second pass does not duplicate.
        intake(&deps).await.unwrap();
        let rows = deps.store.list_tasks("proj", true).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[tokio::test]
    async fn intake_syncs_hold_label_both_ways() {
        use crate::forge::{Forge, LABEL_HOLD};
        let forge = Arc::new(FakeForge::with_issue(7, "T", "B", &[LABEL_READY]));
        let deps = deps_with(forge.clone());
        intake(&deps).await.unwrap();

        forge.add_label(7, LABEL_HOLD).await.unwrap();
        intake(&deps).await.unwrap();
        let row = deps
            .store
            .task_by_origin("proj", "github:7")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "held", "hold label holds the queued row");

        forge.remove_label(7, LABEL_HOLD).await.unwrap();
        intake(&deps).await.unwrap();
        let row = deps
            .store
            .task_by_origin("proj", "github:7")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "queued", "removing the label releases it");
    }

    #[tokio::test]
    async fn intake_requeues_when_needs_human_label_cleared() {
        let forge = Arc::new(FakeForge::with_issue(7, "T", "B", &[LABEL_READY]));
        let deps = deps_with(forge.clone());
        intake(&deps).await.unwrap();
        let row = deps
            .store
            .task_by_origin("proj", "github:7")
            .unwrap()
            .unwrap();
        deps.store.escalate_task(row.id, "boom").unwrap();

        // The ready label is still on (complete never ran); the needs-human
        // label was cleared by the human → the row re-queues.
        intake(&deps).await.unwrap();
        let row = deps
            .store
            .task_by_origin("proj", "github:7")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "queued");
    }

    #[tokio::test]
    async fn intake_retracts_a_queued_row_when_ready_is_removed() {
        use crate::forge::Forge;
        let forge = Arc::new(FakeForge::with_issue(7, "T", "B", &[LABEL_READY]));
        let deps = deps_with(forge.clone());
        intake(&deps).await.unwrap();

        forge.remove_label(7, LABEL_READY).await.unwrap();
        intake(&deps).await.unwrap();
        let row = deps
            .store
            .task_by_origin("proj", "github:7")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "cancelled", "removing ready retracts the row");
        // No run is ever filed for it.
        reconcile_tasks(&deps).await.unwrap();
        assert!(deps.store.list_runs(true).unwrap().is_empty());

        // Re-adding the label imports a fresh row (a cancelled row is not live).
        forge.add_label(7, LABEL_READY).await.unwrap();
        intake(&deps).await.unwrap();
        let row = deps
            .store
            .task_by_origin("proj", "github:7")
            .unwrap()
            .unwrap();
        assert_eq!(row.status, "queued");
    }

    #[tokio::test]
    async fn decider_enqueues_issue_keyed_runs_for_github_tasks() {
        let forge = Arc::new(FakeForge::with_issue(7, "T", "B", &[LABEL_READY]));
        let deps = deps_with(forge);
        intake(&deps).await.unwrap();
        reconcile_tasks(&deps).await.unwrap();
        let runs = deps.store.list_runs(true).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].issue_number, 7);
        assert_eq!(runs[0].loop_kind, "worker");
        // Dedup: a second pass creates no second active run.
        reconcile_tasks(&deps).await.unwrap();
        assert_eq!(deps.store.list_runs(true).unwrap().len(), 1);
    }
}
