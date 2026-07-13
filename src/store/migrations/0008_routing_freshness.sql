-- routing (2/3), issue #65 — continuous freshness inspection.

-- Layer 1: CLI version drift. doctor UPSERTs the detected version of each CLI
-- on every run; a change in the major number between runs flags a possible
-- behavior shift ("re-evaluate routing"). One row per command.
CREATE TABLE IF NOT EXISTS cli_versions (
  command    TEXT PRIMARY KEY,
  version    TEXT NOT NULL,
  major      INTEGER,
  checked_at TEXT NOT NULL
);

-- Layer 2: current outcome-drift state, one row per (project, role, profile).
-- The scheduler's sweep UPSERTs the current verdict here; doctor/top/stats
-- read `active=1` rows scoped by project_id. `events` can't back the read
-- path — it has no project_id and drift is a run-independent aggregate — so a
-- dedicated state table owns "the current state" and "cleared → disappears".
-- Threshold-crossing transitions are still journalled to `events` for history.
CREATE TABLE IF NOT EXISTS routing_drift (
  project_id    TEXT NOT NULL,
  loop_kind     TEXT NOT NULL,
  agent_profile TEXT NOT NULL DEFAULT '',   -- '' = unrouted (NULL can't sit in a composite PK)
  active        INTEGER NOT NULL,           -- 1 = unresolved / 0 = recovered
  metric_json   TEXT NOT NULL DEFAULT '{}', -- before/after metrics
  detected_at   TEXT NOT NULL,              -- when active last flipped 0→1
  updated_at    TEXT NOT NULL,
  PRIMARY KEY (project_id, loop_kind, agent_profile)
);
