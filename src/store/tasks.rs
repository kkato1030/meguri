//! The `tasks` table: the local queue/claim/escalation store that stands in
//! for GitHub labels in local/silent mode. `claim_task` is the atomic claim
//! at the heart of the [`TaskSource`](crate::tasks::TaskSource) contract.

use anyhow::Result;
use rusqlite::{Row, params};
use serde::Serialize;

use super::{Store, now};

/// One `tasks` row. Mirrors the columns in `0004_tasks.sql`.
#[derive(Debug, Clone, Serialize)]
pub struct TaskRow {
    pub id: i64,
    pub project_id: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub origin: String,
    pub status: String,
    pub reason: Option<String>,
    pub claimed_by: Option<String>,
    pub lease_until: Option<String>,
    pub created_at: String,
}

fn task_from_row(row: &Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        kind: row.get("kind")?,
        title: row.get("title")?,
        body: row.get("body")?,
        origin: row.get("origin")?,
        status: row.get("status")?,
        reason: row.get("reason")?,
        claimed_by: row.get("claimed_by")?,
        lease_until: row.get("lease_until")?,
        created_at: row.get("created_at")?,
    })
}

impl Store {
    /// Queue a task (`meguri add`). `kind` is "work" | "plan", `origin` is
    /// "local" or "github:<N>".
    pub fn create_task(
        &self,
        project_id: &str,
        kind: &str,
        title: &str,
        body: &str,
        origin: &str,
    ) -> Result<TaskRow> {
        let id = self.with_conn(|c| {
            c.execute(
                "INSERT INTO tasks (project_id, kind, title, body, origin, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
                params![project_id, kind, title, body, origin, now()],
            )?;
            Ok(c.last_insert_rowid())
        })?;
        Ok(self.get_task(id)?.expect("task just inserted"))
    }

    pub fn get_task(&self, id: i64) -> Result<Option<TaskRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM tasks WHERE id = ?1")?;
            let mut rows = stmt.query([id])?;
            match rows.next()? {
                Some(row) => Ok(Some(task_from_row(row)?)),
                None => Ok(None),
            }
        })
    }

    /// All tasks for a project, newest first. `include_terminal` also lists
    /// done/cancelled tasks (`meguri tasks --all`).
    pub fn list_tasks(&self, project_id: &str, include_terminal: bool) -> Result<Vec<TaskRow>> {
        self.with_conn(|c| {
            let sql = if include_terminal {
                "SELECT * FROM tasks WHERE project_id = ?1 ORDER BY id DESC"
            } else {
                "SELECT * FROM tasks WHERE project_id = ?1
                   AND status NOT IN ('done', 'cancelled') ORDER BY id DESC"
            };
            let mut stmt = c.prepare(sql)?;
            let tasks = stmt
                .query_map([project_id], task_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(tasks)
        })
    }

    /// Actionable tasks of a kind: queued or needs_human (a re-claim clears
    /// the escalation, mirroring the label version). Oldest first.
    pub fn discover_tasks(&self, project_id: &str, kind: &str) -> Result<Vec<TaskRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM tasks WHERE project_id = ?1 AND kind = ?2
                   AND status IN ('queued', 'needs_human') ORDER BY id ASC",
            )?;
            let tasks = stmt
                .query_map(params![project_id, kind], task_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(tasks)
        })
    }

    /// Atomically claim a task. The `WHERE status IN ('queued','needs_human')`
    /// predicate is the whole atomicity story: a second claim of the same
    /// task updates zero rows and returns `None`. Claiming clears `reason`
    /// (an escalated task's re-claim un-escalates it). Phase 4's remote DB
    /// adds `OR lease_until < now()` here — the contract does not change.
    pub fn claim_task(&self, id: i64, project_id: &str, host: &str) -> Result<Option<TaskRow>> {
        let claimed = self.with_conn(|c| {
            let affected = c.execute(
                "UPDATE tasks SET status = 'claimed', claimed_by = ?3, reason = NULL,
                   lease_until = NULL
                 WHERE id = ?1 AND project_id = ?2 AND status IN ('queued', 'needs_human')",
                params![id, project_id, host],
            )?;
            Ok(affected == 1)
        })?;
        if claimed { self.get_task(id) } else { Ok(None) }
    }

    /// Release a claim: back to the queue (`meguri stop`, needs-plan demotion).
    pub fn release_task(&self, id: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET status = 'queued', claimed_by = NULL, lease_until = NULL
                 WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
    }

    /// Hand the task to a human: `status='needs_human'` + the durable reason.
    pub fn escalate_task(&self, id: i64, reason: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET status = 'needs_human', reason = ?2, claimed_by = NULL,
                   lease_until = NULL
                 WHERE id = ?1",
                params![id, reason],
            )?;
            Ok(())
        })
    }

    /// Mark a task done (a deliverable exists).
    pub fn complete_task(&self, id: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE tasks SET status = 'done', claimed_by = NULL, lease_until = NULL
                 WHERE id = ?1",
                params![id],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::LOCAL_HOST;

    #[test]
    fn add_and_list_tasks() {
        let store = Store::open_in_memory().unwrap();
        let t = store
            .create_task("proj", "work", "Do a thing", "body here", "local")
            .unwrap();
        assert_eq!(t.status, "queued");
        assert_eq!(t.kind, "work");

        let open = store.list_tasks("proj", false).unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].title, "Do a thing");
        // Scoped by project.
        assert!(store.list_tasks("other", false).unwrap().is_empty());
    }

    #[test]
    fn discover_returns_queued_and_needs_human_of_the_kind() {
        let store = Store::open_in_memory().unwrap();
        let work = store.create_task("proj", "work", "w", "", "local").unwrap();
        store.create_task("proj", "plan", "p", "", "local").unwrap();
        let escalated = store.create_task("proj", "work", "e", "", "local").unwrap();
        store.escalate_task(escalated.id, "stuck").unwrap();

        let work_tasks = store.discover_tasks("proj", "work").unwrap();
        let ids: Vec<i64> = work_tasks.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![work.id, escalated.id], "queued + needs_human");
        assert_eq!(store.discover_tasks("proj", "plan").unwrap().len(), 1);
    }

    #[test]
    fn claim_is_atomic_second_claim_is_none() {
        let store = Store::open_in_memory().unwrap();
        let t = store.create_task("proj", "work", "t", "", "local").unwrap();

        let first = store.claim_task(t.id, "proj", LOCAL_HOST).unwrap();
        assert!(first.is_some());
        assert_eq!(first.unwrap().status, "claimed");
        // A claimed task is no longer claimable.
        assert!(
            store
                .claim_task(t.id, "proj", LOCAL_HOST)
                .unwrap()
                .is_none()
        );
        // Wrong project never claims it.
        let t2 = store
            .create_task("proj", "work", "t2", "", "local")
            .unwrap();
        assert!(
            store
                .claim_task(t2.id, "other", LOCAL_HOST)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn needs_human_task_reclaims_and_clears_reason() {
        let store = Store::open_in_memory().unwrap();
        let t = store.create_task("proj", "work", "t", "", "local").unwrap();
        store.escalate_task(t.id, "the reason").unwrap();

        let escalated = store.get_task(t.id).unwrap().unwrap();
        assert_eq!(escalated.status, "needs_human");
        assert_eq!(escalated.reason.as_deref(), Some("the reason"));

        // Re-claim succeeds and clears the reason (acceptance criterion 5).
        let reclaimed = store.claim_task(t.id, "proj", LOCAL_HOST).unwrap().unwrap();
        assert_eq!(reclaimed.status, "claimed");
        assert_eq!(reclaimed.reason, None);
        assert_eq!(reclaimed.claimed_by.as_deref(), Some(LOCAL_HOST));
    }

    #[test]
    fn lifecycle_release_escalate_complete() {
        let store = Store::open_in_memory().unwrap();
        let t = store.create_task("proj", "work", "t", "", "local").unwrap();
        store.claim_task(t.id, "proj", LOCAL_HOST).unwrap();

        store.release_task(t.id).unwrap();
        assert_eq!(store.get_task(t.id).unwrap().unwrap().status, "queued");

        store.claim_task(t.id, "proj", LOCAL_HOST).unwrap();
        store.complete_task(t.id).unwrap();
        assert_eq!(store.get_task(t.id).unwrap().unwrap().status, "done");
        // Done tasks drop out of the default listing.
        assert!(store.list_tasks("proj", false).unwrap().is_empty());
        assert_eq!(store.list_tasks("proj", true).unwrap().len(), 1);
    }
}
