//! The pane registry: the issue is the unit of lifetime (#13, #92). A pane
//! belongs to `(project, issue, lane)` and outlives individual runs — one
//! `author` pane shared by every branch-editing loop of the issue, plus one
//! independent `pr-review` pane for the pr-reviewer — and the reaper reclaims
//! them when the issue closes on the forge. `agent_session_id` (the agent's
//! native session, `claude --resume <id>`) is kept per lane and survives
//! reclamation, so closing a pane stays reversible.

use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};

use super::{Store, now};

/// The lane every branch-editing loop shares (planner, worker, spec worker,
/// fixer, ci-fixer, conflict resolver — and the cleaner's standalone report
/// pane, which no other loop ever touches).
pub const LANE_AUTHOR: &str = "author";
/// The collab advisor lane (issue #111, ADR 0006 collab-advisor): the
/// plan-author advisor pane a worker consults over agmsg. Ephemeral — spawned
/// at worker execute, reaped at run end regardless of `keep_pane`, never
/// adopted/resumed, and never carries a saved `agent_session_id`.
pub const LANE_ADVISOR: &str = "advisor";

#[derive(Debug, Clone)]
pub struct PaneRecord {
    pub project_id: String,
    pub issue_number: i64,
    /// Lane within the issue (the kernel only uses [`LANE_AUTHOR`]).
    pub lane: String,
    pub mux_kind: Option<String>,
    pub mux_session: Option<String>,
    /// None once the pane was reclaimed (the row is kept for the saved
    /// session id).
    pub mux_pane_id: Option<String>,
    pub worktree_path: Option<String>,
    pub agent_session_id: Option<String>,
    /// Consecutive agent_quiet strikes on this lane's session (issue #245):
    /// bumped when a turn ends in `AgentQuiet`, reset to 0 by every completed
    /// turn. 2 strikes rotate the session (fresh spawn), 3 escalate to a
    /// human. Deliberately NOT reset when the strike-2 rotation clears the
    /// session — resetting there would make strike 3 unreachable.
    pub quiet_strikes: i64,
    pub created_at: String,
    pub updated_at: String,
    pub reclaimed_at: Option<String>,
}

fn pane_from_row(row: &Row<'_>) -> rusqlite::Result<PaneRecord> {
    Ok(PaneRecord {
        project_id: row.get("project_id")?,
        issue_number: row.get("issue_number")?,
        lane: row.get("lane")?,
        mux_kind: row.get("mux_kind")?,
        mux_session: row.get("mux_session")?,
        mux_pane_id: row.get("mux_pane_id")?,
        worktree_path: row.get("worktree_path")?,
        agent_session_id: row.get("agent_session_id")?,
        quiet_strikes: row.get("quiet_strikes")?,
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
        lane: &str,
        mux_kind: &str,
        mux_session: &str,
        mux_pane_id: &str,
        worktree_path: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO panes (project_id, issue_number, lane, mux_kind, mux_session,
                                    mux_pane_id, worktree_path, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
                 ON CONFLICT (project_id, issue_number, lane) DO UPDATE SET
                   mux_kind = ?4, mux_session = ?5, mux_pane_id = ?6,
                   worktree_path = ?7, updated_at = ?8, reclaimed_at = NULL",
                params![
                    project_id,
                    issue_number,
                    lane,
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

    /// Register (or re-point) a lane's resumable session without a live pane
    /// (direct launch mode, issue #169): creates the row if the lane has
    /// never had one, or updates its worktree/session id if it has. Unlike
    /// [`Store::upsert_pane`], `mux_kind`/`mux_session`/`mux_pane_id` are left
    /// untouched (absent on a fresh row) — "lane" now means "issue-scoped
    /// resumable context", pane being merely optional.
    pub fn upsert_pane_session(
        &self,
        project_id: &str,
        issue_number: i64,
        lane: &str,
        worktree_path: &str,
        session_id: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO panes (project_id, issue_number, lane, worktree_path,
                                    agent_session_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT (project_id, issue_number, lane) DO UPDATE SET
                   worktree_path = ?4, agent_session_id = ?5, updated_at = ?6",
                params![
                    project_id,
                    issue_number,
                    lane,
                    worktree_path,
                    session_id,
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
        lane: &str,
    ) -> Result<Option<PaneRecord>> {
        self.with_conn(|c| {
            let pane = c
                .query_row(
                    "SELECT * FROM panes
                     WHERE project_id = ?1 AND issue_number = ?2 AND lane = ?3",
                    params![project_id, issue_number, lane],
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
                 ORDER BY issue_number, lane",
            )?;
            let panes = stmt
                .query_map([project_id], pane_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(panes)
        })
    }

    /// Live panes for an issue number across projects and lanes
    /// (`meguri attach <needle>` when no run matches anymore).
    pub fn panes_for_issue(&self, issue_number: i64) -> Result<Vec<PaneRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM panes WHERE issue_number = ?1 AND mux_pane_id IS NOT NULL
                 ORDER BY project_id, lane",
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
        lane: &str,
        session_id: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE panes SET agent_session_id = ?4, updated_at = ?5
                 WHERE project_id = ?1 AND issue_number = ?2 AND lane = ?3",
                params![project_id, issue_number, lane, session_id, now()],
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
        lane: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE panes SET mux_pane_id = NULL, reclaimed_at = ?4, updated_at = ?4
                 WHERE project_id = ?1 AND issue_number = ?2 AND lane = ?3",
                params![project_id, issue_number, lane, now()],
            )?;
            Ok(())
        })
    }

    /// Bump the lane's consecutive agent_quiet strike counter and return the
    /// new count (issue #245). Upserts so a lane that somehow lost its row
    /// still counts from 1 instead of silently no-oping.
    pub fn bump_pane_quiet_strikes(
        &self,
        project_id: &str,
        issue_number: i64,
        lane: &str,
    ) -> Result<i64> {
        self.with_conn(|c| {
            let strikes = c.query_row(
                "INSERT INTO panes (project_id, issue_number, lane, quiet_strikes,
                                    created_at, updated_at)
                 VALUES (?1, ?2, ?3, 1, ?4, ?4)
                 ON CONFLICT (project_id, issue_number, lane) DO UPDATE SET
                   quiet_strikes = quiet_strikes + 1, updated_at = ?4
                 RETURNING quiet_strikes",
                params![project_id, issue_number, lane, now()],
                |row| row.get(0),
            )?;
            Ok(strikes)
        })
    }

    /// Reset the lane's agent_quiet strike counter — called on every
    /// completed turn (the session proved it can still converse). No-op for
    /// a lane with no row.
    pub fn reset_pane_quiet_strikes(
        &self,
        project_id: &str,
        issue_number: i64,
        lane: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE panes SET quiet_strikes = 0, updated_at = ?4
                 WHERE project_id = ?1 AND issue_number = ?2 AND lane = ?3
                   AND quiet_strikes != 0",
                params![project_id, issue_number, lane, now()],
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
    fn upsert_pane_session_creates_a_paneless_row_and_updates_it() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_pane("demo", 7, LANE_AUTHOR).unwrap().is_none());

        // A direct-mode lane's first completed turn: no pane was ever
        // spawned, so plain save_pane_session (an UPDATE) would no-op.
        store
            .upsert_pane_session("demo", 7, LANE_AUTHOR, "/wt/demo/b1", Some("sess-1"))
            .unwrap();
        let pane = store.get_pane("demo", 7, LANE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id, None);
        assert_eq!(pane.mux_kind, None);
        assert_eq!(pane.worktree_path.as_deref(), Some("/wt/demo/b1"));
        assert_eq!(pane.agent_session_id.as_deref(), Some("sess-1"));

        // A later turn updates worktree + session without disturbing lane.
        store
            .upsert_pane_session("demo", 7, LANE_AUTHOR, "/wt/demo/b2", Some("sess-2"))
            .unwrap();
        let pane = store.get_pane("demo", 7, LANE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.worktree_path.as_deref(), Some("/wt/demo/b2"));
        assert_eq!(pane.agent_session_id.as_deref(), Some("sess-2"));
        assert_eq!(pane.lane, LANE_AUTHOR);
    }

    #[test]
    fn upsert_pane_session_preserves_an_existing_pane_mapping() {
        // A lane that already has a live pane (e.g. it ran in pane mode
        // before) keeps its mux_kind/mux_pane_id when a direct-style session
        // upsert touches the same row.
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_pane("demo", 7, LANE_AUTHOR, "tmux", "meguri", "%1", "/wt/a")
            .unwrap();
        store
            .upsert_pane_session("demo", 7, LANE_AUTHOR, "/wt/a", Some("sess-1"))
            .unwrap();
        let pane = store.get_pane("demo", 7, LANE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id.as_deref(), Some("%1"));
        assert_eq!(pane.agent_session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn pane_upsert_reuse_and_reclaim() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_pane("demo", 7, LANE_AUTHOR).unwrap().is_none());

        store
            .upsert_pane(
                "demo",
                7,
                LANE_AUTHOR,
                "tmux",
                "meguri",
                "%3",
                "/wt/demo/b1",
            )
            .unwrap();
        let pane = store.get_pane("demo", 7, LANE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id.as_deref(), Some("%3"));
        assert_eq!(pane.worktree_path.as_deref(), Some("/wt/demo/b1"));
        assert!(pane.reclaimed_at.is_none());

        // Reclaim keeps the row (and later the session id) but drops the pane.
        store
            .save_pane_session("demo", 7, LANE_AUTHOR, Some("sess-abc"))
            .unwrap();
        store.mark_pane_reclaimed("demo", 7, LANE_AUTHOR).unwrap();
        let pane = store.get_pane("demo", 7, LANE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id, None);
        assert!(pane.reclaimed_at.is_some());
        assert_eq!(pane.agent_session_id.as_deref(), Some("sess-abc"));
        assert!(store.list_panes("demo").unwrap().is_empty());

        // A respawn re-points the mapping and keeps the saved session id.
        store
            .upsert_pane(
                "demo",
                7,
                LANE_AUTHOR,
                "tmux",
                "meguri",
                "%9",
                "/wt/demo/b2",
            )
            .unwrap();
        let pane = store.get_pane("demo", 7, LANE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.mux_pane_id.as_deref(), Some("%9"));
        assert!(pane.reclaimed_at.is_none());
        assert_eq!(pane.agent_session_id.as_deref(), Some("sess-abc"));
        assert_eq!(store.list_panes("demo").unwrap().len(), 1);

        // Clearing the session id (a resume proved it dead) empties the slot.
        store
            .save_pane_session("demo", 7, LANE_AUTHOR, None)
            .unwrap();
        let pane = store.get_pane("demo", 7, LANE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.agent_session_id, None);
    }

    #[test]
    fn lanes_of_one_issue_are_independent() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_pane("demo", 7, LANE_AUTHOR, "tmux", "meguri", "%1", "/wt/a")
            .unwrap();
        store
            .upsert_pane("demo", 7, "pr-review", "tmux", "meguri", "%2", "/wt/r")
            .unwrap();
        assert_eq!(store.list_panes("demo").unwrap().len(), 2);

        // Reclaiming one lane leaves the other standing.
        store
            .save_pane_session("demo", 7, "pr-review", Some("sess-rev"))
            .unwrap();
        store.mark_pane_reclaimed("demo", 7, "pr-review").unwrap();
        let author = store.get_pane("demo", 7, LANE_AUTHOR).unwrap().unwrap();
        assert_eq!(author.mux_pane_id.as_deref(), Some("%1"));
        assert_eq!(author.agent_session_id, None);
        let review = store.get_pane("demo", 7, "pr-review").unwrap().unwrap();
        assert_eq!(review.mux_pane_id, None);
        assert_eq!(review.agent_session_id.as_deref(), Some("sess-rev"));
    }

    #[test]
    fn panes_are_scoped_by_project_issue_and_lane() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_pane("a", 1, LANE_AUTHOR, "tmux", "meguri", "%1", "/wt/a/1")
            .unwrap();
        store
            .upsert_pane("b", 1, LANE_AUTHOR, "tmux", "meguri", "%2", "/wt/b/1")
            .unwrap();
        store
            .upsert_pane("a", 1, "pr-review", "tmux", "meguri", "%3", "/wt/a/r1")
            .unwrap();
        assert_eq!(store.list_panes("a").unwrap().len(), 2);
        assert_eq!(store.panes_for_issue(1).unwrap().len(), 3);
        assert!(store.panes_for_issue(2).unwrap().is_empty());
    }

    #[test]
    fn quiet_strikes_bump_reset_and_upsert() {
        let store = Store::open_in_memory().unwrap();

        // Upsert path: bumping a lane with no row starts the count at 1.
        assert_eq!(
            store
                .bump_pane_quiet_strikes("demo", 7, LANE_AUTHOR)
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .bump_pane_quiet_strikes("demo", 7, LANE_AUTHOR)
                .unwrap(),
            2
        );
        let pane = store.get_pane("demo", 7, LANE_AUTHOR).unwrap().unwrap();
        assert_eq!(pane.quiet_strikes, 2);

        // A completed turn resets the count; the next quiet starts over at 1.
        store
            .reset_pane_quiet_strikes("demo", 7, LANE_AUTHOR)
            .unwrap();
        assert_eq!(
            store
                .get_pane("demo", 7, LANE_AUTHOR)
                .unwrap()
                .unwrap()
                .quiet_strikes,
            0
        );
        assert_eq!(
            store
                .bump_pane_quiet_strikes("demo", 7, LANE_AUTHOR)
                .unwrap(),
            1
        );

        // Lanes count independently.
        assert_eq!(
            store
                .bump_pane_quiet_strikes("demo", 7, "pr-review")
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .get_pane("demo", 7, LANE_AUTHOR)
                .unwrap()
                .unwrap()
                .quiet_strikes,
            1
        );
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
}
