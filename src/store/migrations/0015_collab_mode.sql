-- Which collab plane governed a run (issue #121 measurement), so `meguri stats
-- collab` can compare the orchestration-plane durable signals of advisor-on vs
-- advisor-off runs while holding routing (profile, arm) constant.
-- Only runs that were meant to get an advisor stamp 'advisor'; every other run
-- leaves this NULL. NULL = 'off' (feature off / ineligible / pre-migration run)
-- — read as off at aggregation, the same backward-compatible shape routing_arm
-- (0014) uses. With `[collab]` absent no UPDATE ever fires, so existing rows are
-- untouched (inert regime, ADR 0006 / ADR 0017).
ALTER TABLE runs ADD COLUMN collab_mode TEXT;
