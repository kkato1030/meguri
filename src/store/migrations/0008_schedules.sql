-- Cron schedule bookkeeping (issue #146). One row per (project, schedule name)
-- once the sweep has observed the schedule at least once. The definition
-- itself lives in config.toml (hot-reloadable); this table holds only the
-- local scheduler state that must survive a restart:
--   * first_seen_at — when the sweep first saw this schedule; the lower bound
--     of the firing window, so a newly-added schedule does not backfill.
--   * last_fired_at — the upper bound of the last consumed window (a real
--     enqueue OR an overlap skip). NULL until the first consumed window.
--   * last_key      — the issue number (github) / task id (local) of the last
--     created item, for the default overlap guard's openness check.
CREATE TABLE IF NOT EXISTS schedule_state (
  project_id    TEXT NOT NULL,
  name          TEXT NOT NULL,
  first_seen_at TEXT NOT NULL,
  last_fired_at TEXT,
  last_key      INTEGER,
  PRIMARY KEY (project_id, name)
);
