-- Native agent session id (e.g. a Claude Code session UUID) last reported
-- for the run's pane; lets recovery respawn with `claude --resume <id>`
-- instead of re-injecting the full prompt.
ALTER TABLE runs ADD COLUMN agent_session_id TEXT;
