-- 1 issue = 1 pane (#13): the pane outlives individual runs and is keyed by
-- (project, issue). `agent_session_id` is the agent's native session id
-- (claude --resume <id>), saved before the pane is reclaimed so closing it
-- stays reversible.
CREATE TABLE IF NOT EXISTS panes (
  project_id TEXT NOT NULL,
  issue_number INTEGER NOT NULL,
  mux_kind TEXT,
  mux_session TEXT,
  mux_pane_id TEXT,
  worktree_path TEXT,
  agent_session_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  reclaimed_at TEXT,
  PRIMARY KEY (project_id, issue_number)
);

-- Backfill from the newest run per (project, issue) that recorded a pane, so
-- panes spawned before this migration stay attached to their issue.
INSERT OR IGNORE INTO panes (project_id, issue_number, mux_kind, mux_session,
                             mux_pane_id, worktree_path, created_at, updated_at)
SELECT r.project_id, r.issue_number, r.mux_kind, r.mux_session, r.mux_pane_id,
       r.worktree_path, r.created_at, r.created_at
FROM runs r
WHERE r.mux_pane_id IS NOT NULL
  AND r.created_at = (SELECT MAX(r2.created_at) FROM runs r2
                      WHERE r2.project_id = r.project_id
                        AND r2.issue_number = r.issue_number
                        AND r2.mux_pane_id IS NOT NULL);
