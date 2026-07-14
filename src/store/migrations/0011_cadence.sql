-- Time-driven discovery throttles (issue #148).
--
-- runs.cadence_label — the cadence bucket a run consumed, stamped in the same
--   INSERT that creates the run (never a follow-up UPDATE), so a crash right
--   after run creation can never leave a NULL-labelled run that the window
--   COUNT misses (which would let the next tick over-consume the bucket). NULL
--   for runs outside any cadence rule. Consumption is counted from here, not
--   from GitHub (Authority principle: labels are workflow state, execution
--   records are local — ADR 0011).
-- tasks.not_before — local-mode "earliest start" instant (RFC3339 UTC), the
--   local counterpart of the github body marker. NULL = no gate.
ALTER TABLE runs ADD COLUMN cadence_label TEXT;
ALTER TABLE tasks ADD COLUMN not_before TEXT;

-- The window COUNT keys on (project, cadence_label, created_at).
CREATE INDEX IF NOT EXISTS idx_runs_cadence
  ON runs(project_id, cadence_label, created_at);
