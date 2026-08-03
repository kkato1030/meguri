-- Schema v1 (kernel-pruning Phase 6): the whole kernel schema in one shot.
-- The pre-v1 migration chain (0001..0017) was collapsed here; a pre-v1
-- meguri.sqlite is not upgradable — runs are ephemeral, delete it and restart.

-- The task queue — the workflow authority (権威反転, Phase 5). GitHub issues
-- are imported as rows by the intake pass; local mode inserts rows directly.
CREATE TABLE tasks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id TEXT NOT NULL,
  kind TEXT NOT NULL DEFAULT 'work',
  title TEXT NOT NULL,
  body TEXT NOT NULL DEFAULT '',
  origin TEXT NOT NULL DEFAULT 'local',   -- 'local' | 'github:<N>'
  status TEXT NOT NULL DEFAULT 'queued',  -- queued|claimed|done|needs_human|held|cancelled
  reason TEXT,                            -- needs_human reason
  claimed_by TEXT,                        -- host id
  lease_until TEXT,                       -- NULL = infinite
  not_before TEXT,                        -- infra-failure parking (ADR 0028)
  created_at TEXT NOT NULL
);

-- One run = one delivery attempt for a task (an issue-keyed loop execution).
CREATE TABLE runs (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  loop_kind TEXT NOT NULL DEFAULT 'worker',
  issue_number INTEGER,                   -- NULL for local runs
  task_id INTEGER,                        -- local runs point at a tasks row
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
  agent_profile TEXT,
  error TEXT,
  started_at TEXT,
  finished_at TEXT,
  created_at TEXT NOT NULL
);

-- At most one active run per (project, loop, issue) / (project, loop, task):
-- the atomic dedup the task decider relies on.
CREATE UNIQUE INDEX runs_active_issue
  ON runs(project_id, loop_kind, issue_number)
  WHERE status IN ('queued', 'running', 'interrupted') AND issue_number IS NOT NULL;
CREATE UNIQUE INDEX runs_active_task
  ON runs(project_id, loop_kind, task_id)
  WHERE status IN ('queued', 'running', 'interrupted') AND task_id IS NOT NULL;

-- One turn = one prompt handed to the agent + its result.json outcome.
CREATE TABLE turns (
  id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  turn_no INTEGER NOT NULL,
  purpose TEXT NOT NULL,
  prompt_path TEXT,
  result_json TEXT,
  outcome TEXT,
  started_at TEXT NOT NULL,
  finished_at TEXT
);

-- Append-only observability log (`meguri events`).
CREATE TABLE events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  run_id TEXT,
  kind TEXT NOT NULL,
  data_json TEXT NOT NULL DEFAULT '{}'
);

-- Issue-scoped pane registry (ADR 0029): the conversable session an issue's
-- turns resume into, and the pane it lives in.
CREATE TABLE panes (
  project_id TEXT NOT NULL,
  issue_number INTEGER NOT NULL,
  lane TEXT NOT NULL DEFAULT 'author',
  mux_kind TEXT,
  mux_session TEXT,
  mux_pane_id TEXT,
  worktree_path TEXT,
  agent_session_id TEXT,
  quiet_strikes INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  reclaimed_at TEXT,
  PRIMARY KEY (project_id, issue_number, lane)
);

-- Liveness beacons (`meguri doctor` checks the watch heartbeat).
CREATE TABLE heartbeats (
  name TEXT PRIMARY KEY,
  ts TEXT NOT NULL
);
