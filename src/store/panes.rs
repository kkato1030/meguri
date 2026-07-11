//! The pane registry: 1 issue = 1 pane (#13). A pane belongs to
//! `(project, issue)` and outlives individual runs; later runs on the same
//! issue reuse it, and the reaper reclaims it when the issue closes on the
//! forge. `agent_session_id` (the agent's native session, `claude --resume
//! <id>`) is saved before reclamation so closing a pane stays reversible.

use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};

use super::{Store, now};

#[derive(Debug, Clone)]
pub struct PaneRecord {
    pub project_id: String,
    pub issue_number: i64,
    pub mux_kind: Option<String>,
    pub mux_session: Option<String>,
    /// None once the pane was reclaimed (the row is kept for the saved
    /// session id).
    pub mux_pane_id: Option<String>,
    pub worktree_path: Option<String>,
    pub agent_session_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub reclaimed_at: Option<String>,
}

fn pane_from_row(row: &Row<'_>) -> rusqlite::Result<PaneRecord> {
    Ok(PaneRecord {
        project_id: row.get("project_id")?,
        issue_number: row.get("issue_number")?,
        mux_kind: row.get("mux_kind")?,
        mux_session: row.get("mux_session")?,
        mux_pane_id: row.get("mux_pane_id")?,
        worktree_path: row.get("worktree_path")?,
        agent_session_id: row.get("agent_session_id")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        reclaimed_at: row.get("reclaimed_at")?,
    })
}

impl Store {
    /// Register (or re-point) the issue's pane after a spawn. Clears any
    /// previous reclamation but keeps the saved agent session id — it stays
    /// the issue's latest resumable context until a newer one is saved.
    pub fn upsert_pane(
        &self,
        project_id: &str,
        issue_number: i64,
        mux_kind: &str,
        mux_session: &str,
        mux_pane_id: &str,
        worktree_path: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO panes (project_id, issue_number, mux_kind, mux_session,
                                    mux_pane_id, worktree_path, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT (project_id, issue_number) DO UPDATE SET
                   mux_kind = ?3, mux_session = ?4, mux_pane_id = ?5,
                   worktree_path = ?6, updated_at = ?7, reclaimed_at = NULL",
                params![
                    project_id,
                    issue_number,
                    mux_kind,
                    mux_session,
                    mux_pane_id,
                    worktree_path,
                    now()
                ],
            )?;
            Ok(())
        })
    }

    pub fn get_pane(&self, project_id: &str, issue_number: i64) -> Result<Option<PaneRecord>> {
        self.with_conn(|c| {
            let pane = c
                .query_row(
                    "SELECT * FROM panes WHERE project_id = ?1 AND issue_number = ?2",
                    params![project_id, issue_number],
                    pane_from_row,
                )
                .optional()?;
            Ok(pane)
        })
    }

    /// The project's live pane mappings (reclaimed rows excluded).
    pub fn list_panes(&self, project_id: &str) -> Result<Vec<PaneRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM panes WHERE project_id = ?1 AND mux_pane_id IS NOT NULL
                 ORDER BY issue_number",
            )?;
            let panes = stmt
                .query_map([project_id], pane_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(panes)
        })
    }

    /// Live panes for an issue number across projects (`meguri attach <N>`
    /// when no run matches anymore).
    pub fn panes_for_issue(&self, issue_number: i64) -> Result<Vec<PaneRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM panes WHERE issue_number = ?1 AND mux_pane_id IS NOT NULL
                 ORDER BY project_id",
            )?;
            let panes = stmt
                .query_map([issue_number], pane_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(panes)
        })
    }

    /// Save the agent's native session id for the issue's pane (called before
    /// the pane is killed, so the context stays resumable).
    pub fn save_pane_session(
        &self,
        project_id: &str,
        issue_number: i64,
        session_id: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE panes SET agent_session_id = ?3, updated_at = ?4
                 WHERE project_id = ?1 AND issue_number = ?2",
                params![project_id, issue_number, session_id, now()],
            )?;
            Ok(())
        })
    }

    /// Detach the pane mapping after reclamation; the row (and its saved
    /// session id) survives for `claude --resume`.
    pub fn mark_pane_reclaimed(&self, project_id: &str, issue_number: i64) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE panes SET mux_pane_id = NULL, reclaimed_at = ?3, updated_at = ?3
                 WHERE project_id = ?1 AND issue_number = ?2",
                params![project_id, issue_number, now()],
            )?;
            Ok(())
        })
    }

    /// Whether any loop currently has an active run on the issue — an active
    /// run owns its pane, so the reaper must not touch it.
    pub fn issue_has_active_run(&self, project_id: &str, issue_number: i64) -> Result<bool> {
        self.with_conn(|c| {
            let exists = c
                .prepare(
                    "SELECT 1 FROM runs WHERE project_id = ?1 AND issue_number = ?2
                       AND status IN ('queued','running','interrupted') LIMIT 1",
                )?
                .exists(params![project_id, issue_number])?;
            Ok(exists)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_upsert_reuse_and_reclaim() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_pane("demo", 7).unwrap().is_none());

        store
            .upsert_pane("demo", 7, "tmux", "meguri", "%3", "/wt/demo/b1")
            .unwrap();
        let pane = store.get_pane("demo", 7).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id.as_deref(), Some("%3"));
        assert_eq!(pane.worktree_path.as_deref(), Some("/wt/demo/b1"));
        assert!(pane.reclaimed_at.is_none());

        // Reclaim keeps the row (and later the session id) but drops the pane.
        store.save_pane_session("demo", 7, "sess-abc").unwrap();
        store.mark_pane_reclaimed("demo", 7).unwrap();
        let pane = store.get_pane("demo", 7).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id, None);
        assert!(pane.reclaimed_at.is_some());
        assert_eq!(pane.agent_session_id.as_deref(), Some("sess-abc"));
        assert!(store.list_panes("demo").unwrap().is_empty());

        // A respawn re-points the mapping and keeps the saved session id.
        store
            .upsert_pane("demo", 7, "tmux", "meguri", "%9", "/wt/demo/b2")
            .unwrap();
        let pane = store.get_pane("demo", 7).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id.as_deref(), Some("%9"));
        assert!(pane.reclaimed_at.is_none());
        assert_eq!(pane.agent_session_id.as_deref(), Some("sess-abc"));
        assert_eq!(store.list_panes("demo").unwrap().len(), 1);
    }

    #[test]
    fn panes_are_scoped_by_project_and_issue() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_pane("a", 1, "tmux", "meguri", "%1", "/wt/a/1")
            .unwrap();
        store
            .upsert_pane("b", 1, "tmux", "meguri", "%2", "/wt/b/1")
            .unwrap();
        assert_eq!(store.list_panes("a").unwrap().len(), 1);
        assert_eq!(store.panes_for_issue(1).unwrap().len(), 2);
        assert!(store.panes_for_issue(2).unwrap().is_empty());
    }

    #[test]
    fn issue_has_active_run_tracks_any_loop() {
        let store = Store::open_in_memory().unwrap();
        assert!(!store.issue_has_active_run("demo", 7).unwrap());
        let run = store.create_run_for_loop("demo", "fixer", 7, "t").unwrap();
        assert!(store.issue_has_active_run("demo", 7).unwrap());
        store
            .update_run_status(&run.id, crate::store::RunStatus::Succeeded, None)
            .unwrap();
        assert!(!store.issue_has_active_run("demo", 7).unwrap());
    }

    #[test]
    fn migration_backfills_panes_from_latest_run() {
        // Simulate a pre-0002 database: apply only 0001, record a run with a
        // pane, then let Store::open run the 0002 backfill.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meguri.sqlite");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                   name TEXT PRIMARY KEY, applied_at TEXT NOT NULL
                 );",
            )
            .unwrap();
            conn.execute_batch(include_str!("migrations/0001_init.sql"))
                .unwrap();
            conn.execute(
                "INSERT INTO schema_migrations (name, applied_at) VALUES ('0001_init', ?1)",
                [now()],
            )
            .unwrap();
            for (id, pane, created) in [
                ("run-old", "%1", "2026-01-01T00:00:00Z"),
                ("run-new", "%2", "2026-01-02T00:00:00Z"),
            ] {
                conn.execute(
                    "INSERT INTO runs (id, project_id, issue_number, status, mux_kind,
                                       mux_session, mux_pane_id, worktree_path, created_at)
                     VALUES (?1, 'demo', 7, 'succeeded', 'tmux', 'meguri', ?2,
                             '/wt/demo/b', ?3)",
                    params![id, pane, created],
                )
                .unwrap();
            }
        }

        let store = Store::open(&db).unwrap();
        let pane = store.get_pane("demo", 7).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id.as_deref(), Some("%2"), "newest run wins");
        assert_eq!(pane.worktree_path.as_deref(), Some("/wt/demo/b"));
    }
}
