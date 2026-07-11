CREATE TABLE IF NOT EXISTS projects (
  id TEXT PRIMARY KEY,
  repo_path TEXT NOT NULL,
  repo_slug TEXT NOT NULL,
  default_branch TEXT NOT NULL DEFAULT 'main',
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS runs (
  id TEXT PRIMARY KEY,
  project_id TEXT NOT NULL,
  loop_kind TEXT NOT NULL DEFAULT 'worker',
  issue_number INTEGER NOT NULL,
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
  error TEXT,
  started_at TEXT,
  finished_at TEXT,
  created_at TEXT NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS runs_active_target
  ON runs(project_id, loop_kind, issue_number)
  WHERE status IN ('queued', 'running', 'interrupted');

CREATE TABLE IF NOT EXISTS turns (
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

CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ts TEXT NOT NULL,
  run_id TEXT,
  kind TEXT NOT NULL,
  data_json TEXT NOT NULL DEFAULT '{}'
);
