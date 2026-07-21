//! The reconciler's local execution progress (ADR 0012 slice 3, #223):
//! per-`(project, item, arm)` exponential backoff for the fixer ping-pong, and
//! the family-wide active-run helpers. Backoff timing is not forge-recoverable,
//! so it lives here in sqlite (the `reconciler_backoff` table), alongside
//! `schedule_state`. The engine owns the episode arithmetic; this module just
//! reads and writes the row.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};

use super::Store;

/// One backoff row: the episode baseline and the spacing high-water mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffRow {
    /// `succeeded_run_count` captured when the current symptom episode opened.
    pub baseline_attempt: i64,
    /// The highest `succeeded_run_count` already spaced (once-per-run brake).
    pub scheduled_attempt: i64,
    /// Epoch seconds before which the PR×arm stays invisible to enqueue.
    pub next_visible_at: i64,
}

impl Store {
    /// The backoff row for a PR×arm, or `None` when the episode has not opened.
    pub fn get_backoff(
        &self,
        project_id: &str,
        item_key: i64,
        arm: &str,
    ) -> Result<Option<BackoffRow>> {
        self.with_conn(|c| {
            let row = c
                .query_row(
                    "SELECT baseline_attempt, scheduled_attempt, next_visible_at
                       FROM reconciler_backoff
                      WHERE project_id = ?1 AND item_key = ?2 AND arm = ?3",
                    params![project_id, item_key, arm],
                    |r| {
                        Ok(BackoffRow {
                            baseline_attempt: r.get(0)?,
                            scheduled_attempt: r.get(1)?,
                            next_visible_at: r.get(2)?,
                        })
                    },
                )
                .optional()?;
            Ok(row)
        })
    }

    /// Whether a PR×arm is currently spaced out (a row exists whose
    /// `next_visible_at` is still in the future).
    pub fn backoff_active(
        &self,
        project_id: &str,
        item_key: i64,
        arm: &str,
        now: i64,
    ) -> Result<bool> {
        self.with_conn(|c| {
            let active: bool = c
                .prepare(
                    "SELECT 1 FROM reconciler_backoff
                      WHERE project_id = ?1 AND item_key = ?2 AND arm = ?3
                        AND next_visible_at > ?4",
                )?
                .exists(params![project_id, item_key, arm, now])?;
            Ok(active)
        })
    }

    /// Upsert the backoff row (the engine computed the episode arithmetic).
    pub fn upsert_backoff(
        &self,
        project_id: &str,
        item_key: i64,
        arm: &str,
        row: BackoffRow,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO reconciler_backoff
                   (project_id, item_key, arm, baseline_attempt, scheduled_attempt, next_visible_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(project_id, item_key, arm) DO UPDATE SET
                   baseline_attempt = excluded.baseline_attempt,
                   scheduled_attempt = excluded.scheduled_attempt,
                   next_visible_at = excluded.next_visible_at",
                params![
                    project_id,
                    item_key,
                    arm,
                    row.baseline_attempt,
                    row.scheduled_attempt,
                    row.next_visible_at
                ],
            )?;
            Ok(())
        })
    }

    /// Drop the backoff row on a positive symptom resolution — the next symptom
    /// opens a fresh episode with the exponent back at 0.
    pub fn clear_backoff(&self, project_id: &str, item_key: i64, arm: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "DELETE FROM reconciler_backoff
                  WHERE project_id = ?1 AND item_key = ?2 AND arm = ?3",
                params![project_id, item_key, arm],
            )?;
            Ok(())
        })
    }

    /// Whether an active fixer-family run (any arm) exists for a PR's canonical
    /// issue — the DB side of the claim's family exclusion (§7). `active` is the
    /// `runs_active_fixer_family` predicate.
    pub fn fixer_family_active(&self, project_id: &str, issue_number: i64) -> Result<bool> {
        self.with_conn(|c| {
            let active: bool = c
                .prepare(
                    "SELECT 1 FROM runs
                      WHERE project_id = ?1 AND issue_number = ?2
                        AND loop_kind IN ('conflict-resolver', 'ci-fixer', 'fixer')
                        AND status IN ('queued', 'running', 'interrupted')",
                )?
                .exists(params![project_id, issue_number])?;
            Ok(active)
        })
    }

    /// Whether a live **author-lane** run exists for an issue — the reconciler's
    /// run-liveness "busy" gate (ADR 0027 / f3): the branch-editing loops
    /// (worker / planner / spec-worker / spec-fixer / fixer / ci-fixer /
    /// conflict-resolver) share the issue's author pane + worktree, so the
    /// reconciler must not act while one is running. `pr-reviewer` is excluded —
    /// it runs in its own lane on a detached worktree, so it never conflicts.
    /// Keying on the run (not the `meguri:working` label) means a stale label
    /// left by a crashed run never deadlocks recovery: a terminal / missing run
    /// reads as not-busy, so the arms, budget escalation, and the stuck backstop
    /// all resume.
    pub fn issue_has_active_author_run(&self, project_id: &str, issue_number: i64) -> Result<bool> {
        self.with_conn(|c| {
            let active: bool = c
                .prepare(
                    "SELECT 1 FROM runs
                      WHERE project_id = ?1 AND issue_number = ?2
                        AND status IN ('queued', 'running', 'interrupted')
                        AND loop_kind != 'pr-reviewer'",
                )?
                .exists(params![project_id, issue_number])?;
            Ok(active)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::RunStatus;

    #[test]
    fn backoff_upsert_read_and_clear_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.get_backoff("p", 7, "ci-fixer").unwrap(), None);
        assert!(!store.backoff_active("p", 7, "ci-fixer", 100).unwrap());

        store
            .upsert_backoff(
                "p",
                7,
                "ci-fixer",
                BackoffRow {
                    baseline_attempt: 1,
                    scheduled_attempt: 2,
                    next_visible_at: 500,
                },
            )
            .unwrap();
        assert_eq!(
            store.get_backoff("p", 7, "ci-fixer").unwrap(),
            Some(BackoffRow {
                baseline_attempt: 1,
                scheduled_attempt: 2,
                next_visible_at: 500,
            })
        );
        // Visible after 500, spaced before it.
        assert!(store.backoff_active("p", 7, "ci-fixer", 499).unwrap());
        assert!(!store.backoff_active("p", 7, "ci-fixer", 500).unwrap());

        store.clear_backoff("p", 7, "ci-fixer").unwrap();
        assert_eq!(store.get_backoff("p", 7, "ci-fixer").unwrap(), None);
    }

    #[test]
    fn migration_0016_folds_preexisting_family_duplicates() {
        // Forward-safety (finding 2): an upgrade may already have a Fixer AND a
        // CiFixer active on the same issue (the old per-loop_kind index allowed
        // it). Migration 0016 must fold each (project, issue) group to one active
        // run — keeping the newest by created_at — BEFORE the unique index, or
        // startup aborts. We simulate the pre-index state on a raw connection,
        // then run the migration and assert the outcome.
        use rusqlite::Connection;
        let conn = Connection::open_in_memory().unwrap();
        // Apply every migration up to but not including 0016.
        for (name, sql) in super::super::MIGRATIONS {
            if *name == "0016_reconciler_backoff" {
                break;
            }
            conn.execute_batch(sql).unwrap();
        }
        // Two active fixer-family runs on issue 9 (different arms) with staggered
        // created_at, plus a lone conflict run on issue 10.
        let insert = |id: &str, kind: &str, issue: i64, status: &str, created: &str| {
            conn.execute(
                "INSERT INTO runs (id, project_id, loop_kind, issue_number, status, created_at)
                 VALUES (?1, 'proj', ?2, ?3, ?4, ?5)",
                rusqlite::params![id, kind, issue, status, created],
            )
            .unwrap();
        };
        insert("run-old", "fixer", 9, "interrupted", "2026-01-01T00:00:00Z");
        insert("run-new", "ci-fixer", 9, "running", "2026-01-02T00:00:00Z");
        insert(
            "run-solo",
            "conflict-resolver",
            10,
            "queued",
            "2026-01-01T00:00:00Z",
        );

        // Apply 0016 (cleanup + CREATE UNIQUE INDEX) — must not error.
        let sql = super::super::MIGRATIONS
            .iter()
            .find(|(n, _)| *n == "0016_reconciler_backoff")
            .unwrap()
            .1;
        conn.execute_batch(sql)
            .expect("migration 0016 must apply cleanly over pre-existing duplicates");

        // Issue 9: exactly one active run remains — the newest (run-new).
        let active_9: Vec<String> = conn
            .prepare(
                "SELECT id FROM runs WHERE issue_number = 9
                   AND status IN ('queued','running','interrupted')",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(active_9, vec!["run-new".to_string()]);
        // The folded run is `cancelled` (not succeeded) with a reason + finished_at.
        let (status, error, finished): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, error, finished_at FROM runs WHERE id = 'run-old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "cancelled");
        assert!(error.unwrap().contains("migration 0016"));
        assert!(finished.is_some());
        // The lone run and unrelated issues are untouched.
        let active_10: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE issue_number = 10
                   AND status IN ('queued','running','interrupted')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(active_10, 1);
    }

    #[test]
    fn fixer_family_active_spans_arms() {
        let store = Store::open_in_memory().unwrap();
        assert!(!store.fixer_family_active("proj", 9).unwrap());
        let run = store.create_run_for_loop("proj", "fixer", 9, "t").unwrap();
        // A Fixer run makes the family active — a CiFixer for the same issue
        // must see it (cross-arm exclusion).
        assert!(store.fixer_family_active("proj", 9).unwrap());
        // Terminating it frees the family.
        store
            .update_run_status(&run.id, RunStatus::Succeeded, None)
            .unwrap();
        assert!(!store.fixer_family_active("proj", 9).unwrap());
    }
}
