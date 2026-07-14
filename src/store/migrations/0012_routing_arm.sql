-- Which routing "arm" a run took (routing 3/3, issue #66), so `meguri stats
-- routing` can separate the mainline from explore canaries and escalated runs.
-- NULL = the mainline arm (the ordinary pick); 'explore' = diverted to a
-- comparison profile; 'escalated' = climbed to a stronger profile mid-run.
-- NULL for runs created before this migration — read as mainline, the same
-- backward-compatible shape `agent_profile` uses.
ALTER TABLE runs ADD COLUMN routing_arm TEXT;
