-- ADR 0012 slice 3 (#223): the reconciler's local execution progress and the
-- fixer-family exclusion index. Local execution progress (backoff timing) is
-- not forge-recoverable, so it lives in sqlite alongside schedule_state.

-- backoffQ: per (project, item = canonical issue, arm) exponential spacing of
-- the fix ping-pong. `baseline_attempt` is the succeeded-run count captured at
-- the start of the current symptom episode; `scheduled_attempt` is the
-- high-water mark of succeeded runs already spaced (so the same succeeded run
-- is never counted twice); the exponent is `scheduled_attempt - baseline_attempt`
-- (episode-relative — clear resets it). `next_visible_at` is epoch seconds.
CREATE TABLE reconciler_backoff (
  project_id       TEXT    NOT NULL,
  item_key         INTEGER NOT NULL,
  arm              TEXT    NOT NULL,
  baseline_attempt INTEGER NOT NULL,
  scheduled_attempt INTEGER NOT NULL,
  next_visible_at  INTEGER NOT NULL,
  PRIMARY KEY (project_id, item_key, arm)
);

-- Fold existing duplicates BEFORE creating the unique index (finding 2): the
-- current per-loop_kind index (0007_tasks) lets one issue hold an active Fixer
-- AND CiFixer run at once, so an upgrade may already violate the family-wide
-- index. Creating it over a violation aborts migrate() and blocks store
-- startup. Keep the newest active run per (project, issue) — created_at DESC,
-- then started_at DESC, then id as a same-timestamp tie-breaker — and terminate
-- the rest as `cancelled` (an existing terminal RunStatus; NOT `succeeded`, so
-- succeeded_run_count budgets are untouched), recording the reason in `error`
-- and stamping `finished_at`. `cancelled` is not active and not redispatched,
-- so the terminated runs do not revive; the reconciler re-derives (and
-- re-enqueues one, respecting the index) on the next resync if the symptom
-- persists.
UPDATE runs
   SET status = 'cancelled',
       error = 'superseded by reconciler family exclusion (migration 0016)',
       finished_at = strftime('%Y-%m-%dT%H:%M:%SZ', 'now')
 WHERE loop_kind IN ('conflict-resolver', 'ci-fixer', 'fixer')
   AND status IN ('queued', 'running', 'interrupted')
   AND issue_number IS NOT NULL
   AND id NOT IN (
     SELECT keep.id FROM runs keep
      WHERE keep.loop_kind IN ('conflict-resolver', 'ci-fixer', 'fixer')
        AND keep.status IN ('queued', 'running', 'interrupted')
        AND keep.issue_number IS NOT NULL
        AND keep.project_id = runs.project_id
        AND keep.issue_number = runs.issue_number
      ORDER BY keep.created_at DESC, keep.started_at DESC, keep.id DESC
      LIMIT 1
   );

-- At most one active fixer-family run per (project, issue), across the three
-- arms — the atomic authority the claim marker projects onto the forge (§7).
CREATE UNIQUE INDEX runs_active_fixer_family
  ON runs(project_id, issue_number)
  WHERE loop_kind IN ('conflict-resolver', 'ci-fixer', 'fixer')
    AND status IN ('queued', 'running', 'interrupted')
    AND issue_number IS NOT NULL;
