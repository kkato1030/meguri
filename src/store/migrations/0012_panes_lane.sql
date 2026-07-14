-- Issue #168: the pane/session isolation unit is "lane" (ADR 0004's concept),
-- not "role" — "role" is reserved for the routing config's kind-of-work
-- grouping (issue #167). SQLite cannot ALTER a column that is part of the
-- primary key, so the table is rebuilt with `lane` in place of `role`, and
-- the stored lane values are remapped to the new vocabulary in the same
-- pass: 'review' (the guard's lane) -> 'pr-review', 'impl-review' (the
-- worker's internal self-review lane) -> 'self-review'.
CREATE TABLE panes_new (
  project_id TEXT NOT NULL,
  issue_number INTEGER NOT NULL,
  lane TEXT NOT NULL DEFAULT 'author',
  mux_kind TEXT,
  mux_session TEXT,
  mux_pane_id TEXT,
  worktree_path TEXT,
  agent_session_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  reclaimed_at TEXT,
  PRIMARY KEY (project_id, issue_number, lane)
);

INSERT INTO panes_new (project_id, issue_number, lane, mux_kind, mux_session,
                       mux_pane_id, worktree_path, agent_session_id,
                       created_at, updated_at, reclaimed_at)
SELECT project_id, issue_number,
       CASE role
         WHEN 'review' THEN 'pr-review'
         WHEN 'impl-review' THEN 'self-review'
         ELSE role
       END,
       mux_kind, mux_session, mux_pane_id, worktree_path, agent_session_id,
       created_at, updated_at, reclaimed_at
FROM panes;

DROP TABLE panes;
ALTER TABLE panes_new RENAME TO panes;
