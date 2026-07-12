//! The pane registry: the issue is the unit of lifetime (#13, #92). A pane
//! belongs to `(project, issue, role)` and outlives individual runs — one
//! `author` pane shared by every branch-editing loop of the issue, plus one
//! independent `review` pane for the reviewer — and the reaper reclaims them
//! when the issue closes on the forge. `agent_session_id` (the agent's
//! native session, `claude --resume <id>`) is kept per lane and survives
//! reclamation, so closing a pane stays reversible.

use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};

use super::{Store, now};

/// The lane every branch-editing loop shares (planner, worker, spec worker,
/// fixer, ci-fixer, conflict resolver — and the cleaner's standalone report
/// pane, which no other loop ever touches).
pub const ROLE_AUTHOR: &str = "author";
/// The reviewer's independent lane: separate pane, separate session, but
/// keyed by the same issue so it stays discoverable and resumable.
pub const ROLE_REVIEW: &str = "review";

#[derive(Debug, Clone)]
pub struct PaneRecord {
    pub project_id: String,
    pub issue_number: i64,
    /// Lane within the issue: [`ROLE_AUTHOR`] or [`ROLE_REVIEW`].
    pub role: String,
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
        role: row.get("role")?,
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
    /// Register (or re-point) the lane's pane after a spawn. Clears any
    /// previous reclamation but keeps the saved agent session id — it stays
    /// the lane's latest resumable context until a newer one is saved.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_pane(
        &self,
        project_id: &str,
        issue_number: i64,
        role: &str,
        mux_kind: &str,
        mux_session: &str,
        mux_pane_id: &str,
        worktree_path: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO panes (project_id, issue_number, role, mux_kind, mux_session,
                                    mux_pane_id, worktree_path, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT (project_id, issue_number, role) DO UPDATE SET
                   mux_kind = ?4, mux_session = ?5, mux_pane_id = ?6,
                   worktree_path = ?7, updated_at = ?8, reclaimed_at = NULL",
                params![
                    project_id,
                    issue_number,
                    role,
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

    pub fn get_pane(
        &self,
        project_id: &str,
        issue_number: i64,
        role: &str,
    ) -> Result<Option<PaneRecord>> {
        self.with_conn(|c| {
            let pane = c
                .query_row(
                    "SELECT * FROM panes
                     WHERE project_id = ?1 AND issue_number = ?2 AND role = ?3",
                    params![project_id, issue_number, role],
                    pane_from_row,
                )
                .optional()?;
            Ok(pane)
        })
    }

    /// The project's live pane mappings across lanes (reclaimed rows
    /// excluded).
    pub fn list_panes(&self, project_id: &str) -> Result<Vec<PaneRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM panes WHERE project_id = ?1 AND mux_pane_id IS NOT NULL
                 ORDER BY issue_number, role",
            )?;
            let panes = stmt
                .query_map([project_id], pane_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(panes)
        })
    }

    /// Live panes for an issue number across projects and lanes
    /// (`meguri attach <N>` when no run matches anymore).
    pub fn panes_for_issue(&self, issue_number: i64) -> Result<Vec<PaneRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM panes WHERE issue_number = ?1 AND mux_pane_id IS NOT NULL
                 ORDER BY project_id, role",
            )?;
            let panes = stmt
                .query_map([issue_number], pane_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(panes)
        })
    }

    /// Save (or clear, with `None`) the agent's native session id for the
    /// lane's pane. Written after every completed turn and before a pane is
    /// killed, so the lane's context stays resumable; cleared when a resume
    /// proved the id dead.
    pub fn save_pane_session(
        &self,
        project_id: &str,
        issue_number: i64,
        role: &str,
        session_id: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE panes SET agent_session_id = ?4, updated_at = ?5
                 WHERE project_id = ?1 AND issue_number = ?2 AND role = ?3",
                params![project_id, issue_number, role, session_id, now()],
            )?;
            Ok(())
        })
    }

    /// Detach the lane's pane mapping after reclamation; the row (and its
    /// saved session id) survives for `claude --resume`.
    pub fn mark_pane_reclaimed(
        &self,
        project_id: &str,
        issue_number: i64,
        role: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE panes SET mux_pane_id = NULL, reclaimed_at = ?4, updated_at = ?4
                 WHERE project_id = ?1 AND issue_number = ?2 AND role = ?3",
                params![project_id, issue_number, role, now()],
            )?;
            Ok(())
        })
    }

    /// Whether any loop currently has an active run on the issue — an active
    /// run owns its lane's pane, so the reaper must not touch the issue's
    /// panes.
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
        assert!(store.get_pane("demo", 7, ROLE_AUTHOR).unwrap().is_none());

        store
            .upsert_pane(
                "demo",
                7,
                ROLE_AUTHOR,
                "tmux",
                "meguri",
                "%3",
                "/wt/demo/b1",
            )
            .unwrap();
        let pane = store.get_pane("demo", 7, ROLE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id.as_deref(), Some("%3"));
        assert_eq!(pane.worktree_path.as_deref(), Some("/wt/demo/b1"));
        assert!(pane.reclaimed_at.is_none());

        // Reclaim keeps the row (and later the session id) but drops the pane.
        store
            .save_pane_session("demo", 7, ROLE_AUTHOR, Some("sess-abc"))
            .unwrap();
        store.mark_pane_reclaimed("demo", 7, ROLE_AUTHOR).unwrap();
        let pane = store.get_pane("demo", 7, ROLE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id, None);
        assert!(pane.reclaimed_at.is_some());
        assert_eq!(pane.agent_session_id.as_deref(), Some("sess-abc"));
        assert!(store.list_panes("demo").unwrap().is_empty());

        // A respawn re-points the mapping and keeps the saved session id.
        store
            .upsert_pane(
                "demo",
                7,
                ROLE_AUTHOR,
                "tmux",
                "meguri",
                "%9",
                "/wt/demo/b2",
            )
            .unwrap();
        let pane = store.get_pane("demo", 7, ROLE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id.as_deref(), Some("%9"));
        assert!(pane.reclaimed_at.is_none());
        assert_eq!(pane.agent_session_id.as_deref(), Some("sess-abc"));
        assert_eq!(store.list_panes("demo").unwrap().len(), 1);

        // Clearing the session id (a resume proved it dead) empties the slot.
        store
            .save_pane_session("demo", 7, ROLE_AUTHOR, None)
            .unwrap();
        let pane = store.get_pane("demo", 7, ROLE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.agent_session_id, None);
    }

    #[test]
    fn lanes_of_one_issue_are_independent() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_pane("demo", 7, ROLE_AUTHOR, "tmux", "meguri", "%1", "/wt/a")
            .unwrap();
        store
            .upsert_pane("demo", 7, ROLE_REVIEW, "tmux", "meguri", "%2", "/wt/r")
            .unwrap();
        assert_eq!(store.list_panes("demo").unwrap().len(), 2);

        // Reclaiming one lane leaves the other standing.
        store
            .save_pane_session("demo", 7, ROLE_REVIEW, Some("sess-rev"))
            .unwrap();
        store.mark_pane_reclaimed("demo", 7, ROLE_REVIEW).unwrap();
        let author = store.get_pane("demo", 7, ROLE_AUTHOR).unwrap().unwrap();
        assert_eq!(author.mux_pane_id.as_deref(), Some("%1"));
        assert_eq!(author.agent_session_id, None);
        let review = store.get_pane("demo", 7, ROLE_REVIEW).unwrap().unwrap();
        assert_eq!(review.mux_pane_id, None);
        assert_eq!(review.agent_session_id.as_deref(), Some("sess-rev"));
    }

    #[test]
    fn panes_are_scoped_by_project_issue_and_role() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_pane("a", 1, ROLE_AUTHOR, "tmux", "meguri", "%1", "/wt/a/1")
            .unwrap();
        store
            .upsert_pane("b", 1, ROLE_AUTHOR, "tmux", "meguri", "%2", "/wt/b/1")
            .unwrap();
        store
            .upsert_pane("a", 1, ROLE_REVIEW, "tmux", "meguri", "%3", "/wt/a/r1")
            .unwrap();
        assert_eq!(store.list_panes("a").unwrap().len(), 2);
        assert_eq!(store.panes_for_issue(1).unwrap().len(), 3);
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
        // Simulate a pre-0004 database: apply only 0001, record a run with a
        // pane, then let Store::open run the 0004 backfill (and the 0005
        // role rebuild on top).
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
        let pane = store.get_pane("demo", 7, ROLE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id.as_deref(), Some("%2"), "newest run wins");
        assert_eq!(pane.worktree_path.as_deref(), Some("/wt/demo/b"));
        assert_eq!(pane.role, ROLE_AUTHOR);
    }

    #[test]
    fn migration_carries_0004_panes_into_the_author_lane() {
        // Simulate a 0004-era database: panes exist without a role column;
        // 0005 must rebuild them as author rows, keeping the saved session.
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
            for (name, sql) in [
                ("0001_init", include_str!("migrations/0001_init.sql")),
                (
                    "0002_heartbeats",
                    include_str!("migrations/0002_heartbeats.sql"),
                ),
                (
                    "0003_agent_session",
                    include_str!("migrations/0003_agent_session.sql"),
                ),
                ("0004_panes", include_str!("migrations/0004_panes.sql")),
            ] {
                conn.execute_batch(sql).unwrap();
                conn.execute(
                    "INSERT INTO schema_migrations (name, applied_at) VALUES (?1, ?2)",
                    params![name, now()],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO panes (project_id, issue_number, mux_kind, mux_session,
                                    mux_pane_id, worktree_path, agent_session_id,
                                    created_at, updated_at)
                 VALUES ('demo', 7, 'tmux', 'meguri', '%5', '/wt/demo/b',
                         'sess-old', ?1, ?1)",
                [now()],
            )
            .unwrap();
        }

        let store = Store::open(&db).unwrap();
        let pane = store.get_pane("demo", 7, ROLE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.role, ROLE_AUTHOR);
        assert_eq!(pane.mux_pane_id.as_deref(), Some("%5"));
        assert_eq!(pane.agent_session_id.as_deref(), Some("sess-old"));
        assert!(store.get_pane("demo", 7, ROLE_REVIEW).unwrap().is_none());
    }
}
