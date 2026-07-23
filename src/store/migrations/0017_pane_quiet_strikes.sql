-- Issue #245: count consecutive agent_quiet strikes per lane so the flow can
-- rotate an unrecoverable session (2 strikes) and escalate to a human (3)
-- instead of resume-looping forever. Reset to 0 on every completed turn.
ALTER TABLE panes ADD COLUMN quiet_strikes INTEGER NOT NULL DEFAULT 0;
