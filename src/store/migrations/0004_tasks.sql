-- Local task coordination (issue #54): the queue/claim/escalation store that
-- stands in for GitHub labels in local/silent mode. github mode keeps NO row
-- here (labels stay the only source of truth); only local/silent tasks live
-- in this table. claimed_by / lease_until are unused on a single machine but
-- are present from day one so the Phase 4 remote-DB claim is a WHERE-clause
-- extension, not a schema change (ADR 0003).
CREATE TABLE tasks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id TEXT NOT NULL,
  kind TEXT NOT NULL DEFAULT 'work',          -- work | plan
  title TEXT NOT NULL,
  body TEXT NOT NULL DEFAULT '',
  origin TEXT NOT NULL DEFAULT 'local',        -- 'local' | 'github:<N>'
  status TEXT NOT NULL DEFAULT 'queued',       -- queued|claimed|done|needs_human|cancelled
  reason TEXT,                                 -- needs_human reason
  claimed_by TEXT,                             -- host id; fixed value in Phase 1
  lease_until TEXT,                            -- NULL (infinite) in Phase 1
  created_at TEXT NOT NULL
);

-- runs gains task_id and makes issue_number nullable: a github run keeps its
-- issue_number (task_id NULL), a local run its task_id (issue_number NULL), a
-- silent run both. sqlite cannot relax a column in place, so recreate the
-- table and copy the data over (the standard sqlite pattern).
ALTER TABLE runs RENAME TO runs_old;

CREATE TABLE runs (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  loop_kind TEXT NOT NULL DEFAULT 'worker',
  issue_number INTEGER,                        -- now nullable (local runs have none)
  task_id INTEGER,                             -- local/silent runs point at a tasks row
  issue_title TEXT,
  branch TEXT,
  worktree_path TEXT,
  step TEXT NOT NULL DEFAULT 'prepare-work',
  checkpoint_json TEXT NOT NULL DEFAULT '{}',
  status TEXT NOT NULL DEFAULT 'queued',
  interaction_state TEXT,
  desired_state TEXT,
  mux_kind TEXT,
  mux_session TEXT,
  mux_pane_id TEXT,
  turn_no INTEGER NOT NULL DEFAULT 0,
  current_turn_id TEXT,
  agent_session_id TEXT,
  error TEXT,
  started_at TEXT,
  finished_at TEXT,
  created_at TEXT NOT NULL
);

INSERT INTO runs (id, project_id, loop_kind, issue_number, issue_title, branch,
                  worktree_path, step, checkpoint_json, status, interaction_state,
                  desired_state, mux_kind, mux_session, mux_pane_id, turn_no,
                  current_turn_id, agent_session_id, error, started_at, finished_at,
                  created_at)
  SELECT id, project_id, loop_kind, issue_number, issue_title, branch,
         worktree_path, step, checkpoint_json, status, interaction_state,
         desired_state, mux_kind, mux_session, mux_pane_id, turn_no,
         current_turn_id, agent_session_id, error, started_at, finished_at,
         created_at
  FROM runs_old;

DROP TABLE runs_old;

-- The old single unique index (0001's runs_active_target) splits into two
-- partial indexes so github runs (keyed by issue_number) and local runs
-- (keyed by task_id) each get active-run exclusion without colliding. The
-- `active` predicate is 0001's `status IN ('queued','running','interrupted')`.
CREATE UNIQUE INDEX runs_active_issue
  ON runs(project_id, loop_kind, issue_number)
  WHERE status IN ('queued', 'running', 'interrupted') AND issue_number IS NOT NULL;

CREATE UNIQUE INDEX runs_active_task
  ON runs(project_id, loop_kind, task_id)
  WHERE status IN ('queued', 'running', 'interrupted') AND task_id IS NOT NULL;
