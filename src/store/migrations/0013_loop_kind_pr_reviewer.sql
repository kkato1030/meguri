-- Issue #168: the guard loop's internal name follows its routing role
-- (issue #167: "guard" -> "pr-reviewer"). Pure data migration — no schema
-- change — so existing runs resume, budget counts, and reaper judgment keep
-- working under the new `runs.loop_kind` value.
UPDATE runs SET loop_kind = 'pr-reviewer' WHERE loop_kind = 'guard';
