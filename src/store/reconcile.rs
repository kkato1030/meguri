//! Signal dedup for the reconcile loop (issue #142): the `issue_reconcile`
//! table records the last body digest for which `issue.body_changed` (and the
//! signal comment) was emitted, so a body edit waiting to be reprocessed does
//! not re-fire the event/comment on every poll tick. Both half A (the discover
//! guard in [`crate::tasks::LabelTaskSource`]) and half B (the poll sweep in
//! [`crate::engine::reconcile_body_edits`]) gate on it and share the same row
//! per issue.

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use serde_json::json;

use super::{Store, now};

impl Store {
    /// Emit a deduped `issue.body_changed` event for a body edit detected in
    /// discovery (half A, issue #142). Call only once suppression has lifted
    /// after a prior success. A no-op if this exact new body was already
    /// signaled (by an earlier tick or the half-B sweep), so the event does not
    /// pile up on every poll while the edit waits to be reprocessed.
    pub fn signal_body_changed_event(
        &self,
        project_id: &str,
        loop_kind: &str,
        issue_number: i64,
        digest: &str,
    ) -> Result<()> {
        if self.reconcile_needs_signal(project_id, issue_number, digest)? {
            let run_id = self.latest_succeeded_run_id(project_id, loop_kind, issue_number)?;
            self.emit(
                run_id.as_deref(),
                "issue.body_changed",
                json!({ "issue": issue_number, "loop": loop_kind, "digest": digest }),
            )?;
            self.mark_reconcile_signaled(project_id, issue_number, digest)?;
        }
        Ok(())
    }

    /// Whether the reconcile loop still needs to signal `digest` for this
    /// issue: true when nothing has been signaled yet, or a *different* digest
    /// was (the body changed again since the last signal). False once `digest`
    /// itself has been signaled — the dedup that keeps `issue.body_changed`
    /// from re-firing while the edit waits to be reprocessed.
    pub fn reconcile_needs_signal(
        &self,
        project_id: &str,
        issue_number: i64,
        digest: &str,
    ) -> Result<bool> {
        self.with_conn(|c| {
            let signaled: Option<String> = c
                .query_row(
                    "SELECT signaled_digest FROM issue_reconcile
                       WHERE project_id = ?1 AND issue_number = ?2",
                    params![project_id, issue_number],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(signaled.as_deref() != Some(digest))
        })
    }

    /// Record that `digest` has been signaled for this issue (upsert), so the
    /// next tick's discover/sweep does not re-emit until the body changes again.
    pub fn mark_reconcile_signaled(
        &self,
        project_id: &str,
        issue_number: i64,
        digest: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO issue_reconcile
                   (project_id, issue_number, signaled_digest, signaled_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(project_id, issue_number)
                   DO UPDATE SET signaled_digest = excluded.signaled_digest,
                                 signaled_at = excluded.signaled_at",
                params![project_id, issue_number, digest, now()],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_dedup_tracks_the_last_digest_per_issue() {
        let store = Store::open_in_memory().unwrap();
        // Nothing signaled yet: any digest needs a signal.
        assert!(store.reconcile_needs_signal("proj", 7, "aaa").unwrap());

        store.mark_reconcile_signaled("proj", 7, "aaa").unwrap();
        // Same digest is now deduped; a new body (new digest) still fires.
        assert!(!store.reconcile_needs_signal("proj", 7, "aaa").unwrap());
        assert!(store.reconcile_needs_signal("proj", 7, "bbb").unwrap());

        // Upsert moves the watermark to the new digest.
        store.mark_reconcile_signaled("proj", 7, "bbb").unwrap();
        assert!(!store.reconcile_needs_signal("proj", 7, "bbb").unwrap());
        assert!(store.reconcile_needs_signal("proj", 7, "aaa").unwrap());

        // Scoped by project and issue.
        assert!(store.reconcile_needs_signal("other", 7, "bbb").unwrap());
        assert!(store.reconcile_needs_signal("proj", 8, "bbb").unwrap());
    }
}
