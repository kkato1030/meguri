-- Issue #92: panes are keyed by (project, issue, role) — the issue is the
-- unit of lifetime, with one pane per lane under it (role = 'author' for
-- every branch-editing loop, 'review' for the reviewer's independent
-- read-only session). SQLite cannot ALTER a primary key, so the table is
-- rebuilt; existing rows migrate as 'author' (pre-role panes were all
-- author-shaped; rows old PR-number-keyed loops left behind become stale
-- author rows that the dead-pane sweep reclaims).
CREATE TABLE panes_new (
  project_id TEXT NOT NULL,
  issue_number INTEGER NOT NULL,
  role TEXT NOT NULL DEFAULT 'author',
  mux_kind TEXT,
  mux_session TEXT,
  mux_pane_id TEXT,
  worktree_path TEXT,
  agent_session_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  reclaimed_at TEXT,
  PRIMARY KEY (project_id, issue_number, role)
);

INSERT INTO panes_new (project_id, issue_number, role, mux_kind, mux_session,
                       mux_pane_id, worktree_path, agent_session_id,
                       created_at, updated_at, reclaimed_at)
SELECT project_id, issue_number, 'author', mux_kind, mux_session,
       mux_pane_id, worktree_path, agent_session_id,
       created_at, updated_at, reclaimed_at
FROM panes;

DROP TABLE panes;
ALTER TABLE panes_new RENAME TO panes;
