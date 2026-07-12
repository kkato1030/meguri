-- Launch profile pinned for the run at its first pane spawn (issue #64,
-- role-based routing). NULL for runs created before this migration; the
-- flow resolves and backfills it lazily on the next spawn.
ALTER TABLE runs ADD COLUMN agent_profile TEXT;
