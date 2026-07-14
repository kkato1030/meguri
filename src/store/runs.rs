use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};

use super::{Store, now};
use crate::tasks::TaskKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Interrupted,
    Succeeded,
    Failed,
    Cancelled,
    /// The run turned out to have nothing to do (e.g. the issue was
    /// de-labeled between discovery and claim) — terminal, no escalation.
    Skipped,
    /// The agent found a design decision must precede implementation and the
    /// issue was routed to the planner (issue #22) — terminal, not a failure.
    NeedsPlan,
    /// The planner split the issue into sub-issues instead of writing a spec
    /// (issue #24) — terminal, the second normal planner ending.
    Decomposed,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Interrupted => "interrupted",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::NeedsPlan => "needs_plan",
            Self::Decomposed => "decomposed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "interrupted" => Some(Self::Interrupted),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "skipped" => Some(Self::Skipped),
            "needs_plan" => Some(Self::NeedsPlan),
            "decomposed" => Some(Self::Decomposed),
            _ => None,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Interrupted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionState {
    AgentWorking,
    AwaitingHuman,
    HumanDriving,
    Paused,
}

impl InteractionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AgentWorking => "agent_working",
            Self::AwaitingHuman => "awaiting_human",
            Self::HumanDriving => "human_driving",
            Self::Paused => "paused",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "agent_working" => Some(Self::AgentWorking),
            "awaiting_human" => Some(Self::AwaitingHuman),
            "human_driving" => Some(Self::HumanDriving),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

/// Control channel written by CLI commands, honored by the orchestrator.
/// This is a *target* the orchestrator converges to; clearing it (NULL)
/// means "run normally" — so `resume` and `handback` just clear it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Paused,
    Stopped,
    Takeover,
}

impl DesiredState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Takeover => "takeover",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "paused" => Some(Self::Paused),
            "stopped" => Some(Self::Stopped),
            "takeover" => Some(Self::Takeover),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RunRecord {
    pub id: String,
    pub project_id: String,
    pub loop_kind: String,
    /// GitHub issue/PR number the run targets. `0` for local runs (their
    /// target is [`RunRecord::task_id`]); the DB column is NULL there.
    pub issue_number: i64,
    /// Local task id the run targets (local/silent runs); `None` for github
    /// runs. Exactly one of `issue_number`/`task_id` identifies the target.
    pub task_id: Option<i64>,
    pub issue_title: Option<String>,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub step: String,
    pub checkpoint_json: String,
    pub status: RunStatus,
    pub interaction_state: Option<InteractionState>,
    pub desired_state: Option<DesiredState>,
    pub mux_kind: Option<String>,
    pub mux_session: Option<String>,
    pub mux_pane_id: Option<String>,
    pub turn_no: i64,
    pub current_turn_id: Option<String>,
    /// Native session id of the agent CLI last seen in the run's pane
    /// (reported via the turn contract or the mux); used to `--resume`
    /// the conversation when the pane dies.
    pub agent_session_id: Option<String>,
    /// Launch profile pinned at the run's first pane spawn (role-based
    /// routing, issue #64) — or earlier, at the first `worktree_setup` hook
    /// run (issue #138), if the project configures one; both go through the
    /// same pin-aware resolver so whichever runs first is authoritative.
    /// NULL until something resolves it; once set, every later spawn,
    /// resume, or hook run of this run reuses it.
    pub agent_profile: Option<String>,
    /// Which routing arm the run took (routing 3/3, issue #66): `None` =
    /// mainline, `"explore"` = diverted to a comparison profile, `"escalated"`
    /// = climbed to a stronger profile mid-run. Keeps `meguri stats routing`
    /// able to separate the three even though escalation overwrites
    /// `agent_profile` in place.
    pub routing_arm: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub created_at: String,
    /// Normalized-body SHA-256 the run acted on (issue #142), set once the
    /// checkpoint's `issue_body` is settled. NULL for pre-#142 runs — treated
    /// as "matches any body" by [`Store::issue_processed_current_body`] so the
    /// old permanent-suppression behavior survives an upgrade.
    pub body_digest: Option<String>,
    /// The cadence bucket (issue label) this run consumed (issue #148), stamped
    /// in the creating INSERT and never changed. NULL for runs outside any
    /// cadence rule. [`Store::cadence_consumed`] counts non-skipped runs by
    /// this column within the window.
    pub cadence_label: Option<String>,
}

impl RunRecord {
    /// The coordination-layer key this run targets: a local task id when the
    /// run has one, otherwise its github issue number.
    pub fn task_key(&self) -> TaskKey {
        match self.task_id {
            Some(id) => TaskKey::Local(id),
            None => TaskKey::Issue(self.issue_number),
        }
    }
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<RunRecord> {
    let status: String = row.get("status")?;
    let interaction: Option<String> = row.get("interaction_state")?;
    let desired: Option<String> = row.get("desired_state")?;
    Ok(RunRecord {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        loop_kind: row.get("loop_kind")?,
        // A local run stores NULL here (its target is task_id); map it to the
        // 0 sentinel so the issue-number path never mistakes it for a real
        // issue while keeping the field ergonomic for github runs.
        issue_number: row.get::<_, Option<i64>>("issue_number")?.unwrap_or(0),
        task_id: row.get("task_id")?,
        issue_title: row.get("issue_title")?,
        branch: row.get("branch")?,
        worktree_path: row.get("worktree_path")?,
        step: row.get("step")?,
        checkpoint_json: row.get("checkpoint_json")?,
        status: RunStatus::parse(&status).unwrap_or(RunStatus::Failed),
        interaction_state: interaction.as_deref().and_then(InteractionState::parse),
        desired_state: desired.as_deref().and_then(DesiredState::parse),
        mux_kind: row.get("mux_kind")?,
        mux_session: row.get("mux_session")?,
        mux_pane_id: row.get("mux_pane_id")?,
        turn_no: row.get("turn_no")?,
        current_turn_id: row.get("current_turn_id")?,
        agent_session_id: row.get("agent_session_id")?,
        agent_profile: row.get("agent_profile")?,
        routing_arm: row.get("routing_arm")?,
        error: row.get("error")?,
        started_at: row.get("started_at")?,
        finished_at: row.get("finished_at")?,
        created_at: row.get("created_at")?,
        body_digest: row.get("body_digest")?,
        cadence_label: row.get("cadence_label")?,
    })
}

/// One agent turn as recorded in the `turns` table (read path for the UI).
#[derive(Debug, Clone, Serialize)]
pub struct TurnRecord {
    pub id: String,
    pub run_id: String,
    pub turn_no: i64,
    pub purpose: String,
    pub prompt_path: Option<String>,
    pub result_json: Option<String>,
    pub outcome: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

impl Store {
    /// Create a run for the worker loop (the schema default `loop_kind`).
    pub fn create_run(
        &self,
        project_id: &str,
        issue_number: i64,
        issue_title: &str,
    ) -> Result<RunRecord> {
        self.create_run_for_loop(project_id, "worker", issue_number, issue_title)
    }

    pub fn create_run_for_loop(
        &self,
        project_id: &str,
        loop_kind: &str,
        issue_number: i64,
        issue_title: &str,
    ) -> Result<RunRecord> {
        self.create_run_for_loop_cadence(project_id, loop_kind, issue_number, issue_title, None)
    }

    /// Like [`Store::create_run_for_loop`] but stamps the cadence bucket
    /// (issue #148) in the *same* INSERT that creates the run — never a
    /// follow-up UPDATE. If the process died between run creation and a
    /// separate stamp, a NULL-labelled `sns` run would slip past the window
    /// COUNT and the next tick would consume the bucket a second time; folding
    /// the stamp into the INSERT removes that crash window entirely. Only the
    /// scheduler's issue branch and manual `meguri run` pass a non-None bucket.
    pub fn create_run_for_loop_cadence(
        &self,
        project_id: &str,
        loop_kind: &str,
        issue_number: i64,
        issue_title: &str,
        cadence_label: Option<&str>,
    ) -> Result<RunRecord> {
        let id = uuid::Uuid::new_v4().to_string();
        // Short ids are friendlier CLI handles; keep full uuid uniqueness.
        let id = format!("run-{}", &id[..8]);
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO runs (id, project_id, loop_kind, issue_number, issue_title,
                                   status, created_at, cadence_label)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6, ?7)",
                params![
                    id,
                    project_id,
                    loop_kind,
                    issue_number,
                    issue_title,
                    now(),
                    cadence_label
                ],
            )?;
            Ok(())
        })?;
        Ok(self.get_run(&id)?.expect("run just inserted"))
    }

    /// Count consumption of a cadence bucket within its window (issue #148):
    /// non-`skipped` runs for `(project, label)` created at or after
    /// `window_start`. `skipped` runs are benign races that touched nothing, so
    /// they do not consume the quota; every other status counts as one attempt
    /// (success or failure alike — ADR 0011). `window_start` is epoch seconds;
    /// `runs.created_at` is our RFC3339 UTC shape, whose lexicographic order is
    /// chronological, so the bound is formatted the same way before comparison.
    pub fn cadence_consumed(
        &self,
        project_id: &str,
        label: &str,
        window_start: u64,
    ) -> Result<i64> {
        let bound = super::format_epoch(window_start);
        self.with_conn(|c| {
            let count = c.query_row(
                "SELECT COUNT(*) FROM runs
                   WHERE project_id = ?1 AND cadence_label = ?2
                     AND created_at >= ?3 AND status != 'skipped'",
                params![project_id, label, bound],
                |row| row.get(0),
            )?;
            Ok(count)
        })
    }

    /// Create a run targeting a local task (local/silent mode). `issue_number`
    /// is left NULL; `task_id` carries the target, and the `runs_active_task`
    /// partial index enforces one active run per (project, loop, task).
    pub fn create_run_for_task(
        &self,
        project_id: &str,
        loop_kind: &str,
        task_id: i64,
        title: &str,
    ) -> Result<RunRecord> {
        let id = uuid::Uuid::new_v4().to_string();
        let id = format!("run-{}", &id[..8]);
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO runs (id, project_id, loop_kind, task_id, issue_title,
                                   status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'queued', ?6)",
                params![id, project_id, loop_kind, task_id, title, now()],
            )?;
            Ok(())
        })?;
        Ok(self.get_run(&id)?.expect("run just inserted"))
    }

    pub fn get_run(&self, id: &str) -> Result<Option<RunRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM runs WHERE id = ?1")?;
            let mut rows = stmt.query([id])?;
            match rows.next()? {
                Some(row) => Ok(Some(run_from_row(row)?)),
                None => Ok(None),
            }
        })
    }

    /// Resolve a run by exact id, unique prefix, or — if the input parses as
    /// a number — the single active run for that issue number.
    pub fn find_run(&self, needle: &str) -> Result<Option<RunRecord>> {
        if let Some(run) = self.get_run(needle)? {
            return Ok(Some(run));
        }
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM runs WHERE id LIKE ?1 || '%' ORDER BY created_at DESC LIMIT 2",
            )?;
            let matches: Vec<RunRecord> = stmt
                .query_map([needle], run_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            if matches.len() == 1 {
                return Ok(Some(matches[0].clone()));
            }
            if let Ok(issue) = needle.parse::<i64>() {
                let mut stmt = c.prepare(
                    "SELECT * FROM runs WHERE issue_number = ?1
                       AND status IN ('queued','running','interrupted')
                     ORDER BY created_at DESC LIMIT 1",
                )?;
                let mut rows = stmt.query([issue])?;
                if let Some(row) = rows.next()? {
                    return Ok(Some(run_from_row(row)?));
                }
            }
            Ok(None)
        })
    }

    /// Whether the issue has already been shipped by a succeeded run of the
    /// given loop — used by watch discovery to avoid re-filing (and duplicate
    /// PRs) after the success de-labeled the issue. Scoped by loop kind so
    /// e.g. a planner success doesn't block a later worker run on the same
    /// issue. `meguri run --issue N` bypasses this.
    pub fn issue_has_succeeded_run(
        &self,
        project_id: &str,
        loop_kind: &str,
        issue_number: i64,
    ) -> Result<bool> {
        self.with_conn(|c| {
            let exists = c
                .prepare(
                    "SELECT 1 FROM runs WHERE project_id = ?1 AND loop_kind = ?2
                       AND issue_number = ?3 AND status = 'succeeded' LIMIT 1",
                )?
                .exists(params![project_id, loop_kind, issue_number])?;
            Ok(exists)
        })
    }

    /// Whether a succeeded run of `loop_kind` already covers the issue's
    /// *current* body (issue #142): either a succeeded run whose `body_digest`
    /// matches `digest`, or a legacy NULL-digest succeeded run (pre-#142 —
    /// treated as "matches any body" so the old permanent suppression survives
    /// an upgrade). The body-aware replacement for [`Store::issue_has_succeeded_run`]
    /// in discovery: when this returns false while `issue_has_succeeded_run` is
    /// true, the body changed since it was last processed — the suppression
    /// lifts and the reconcile loop signals the edit.
    pub fn issue_processed_current_body(
        &self,
        project_id: &str,
        loop_kind: &str,
        issue_number: i64,
        digest: &str,
    ) -> Result<bool> {
        self.with_conn(|c| {
            let exists = c
                .prepare(
                    "SELECT 1 FROM runs WHERE project_id = ?1 AND loop_kind = ?2
                       AND issue_number = ?3 AND status = 'succeeded'
                       AND (body_digest IS NULL OR body_digest = ?4) LIMIT 1",
                )?
                .exists(params![project_id, loop_kind, issue_number, digest])?;
            Ok(exists)
        })
    }

    /// The most recent succeeded run of `loop_kind` for an issue (issue #142):
    /// the run whose work a later body edit invalidates. `issue.body_changed`
    /// is emitted against it so the signal shows up under `meguri logs` on that
    /// run, not orphaned with a NULL run id.
    pub fn latest_succeeded_run_id(
        &self,
        project_id: &str,
        loop_kind: &str,
        issue_number: i64,
    ) -> Result<Option<String>> {
        self.with_conn(|c| {
            let id = c
                .query_row(
                    "SELECT id FROM runs WHERE project_id = ?1 AND loop_kind = ?2
                       AND issue_number = ?3 AND status = 'succeeded'
                     ORDER BY created_at DESC LIMIT 1",
                    params![project_id, loop_kind, issue_number],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
            Ok(id)
        })
    }

    /// Record the normalized-body digest a run acted on (issue #142). Written
    /// once the checkpoint's `issue_body` is settled, in the flow step shared
    /// by every flavor, so a loop with a custom claim path (the spec worker)
    /// still stamps its succeeded runs.
    pub fn set_run_body_digest(&self, id: &str, digest: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET body_digest = ?2 WHERE id = ?1",
                params![id, digest],
            )?;
            Ok(())
        })
    }

    /// Number of succeeded runs of a loop for one issue/PR — the conflict
    /// resolver's resolve budget: a PR that keeps re-conflicting after this
    /// many successful resolves stops being rediscovered instead of looping
    /// forever. Skipped/failed runs don't consume the budget (benign races
    /// and escalations have their own convergence).
    pub fn succeeded_run_count(
        &self,
        project_id: &str,
        loop_kind: &str,
        issue_number: i64,
    ) -> Result<i64> {
        self.with_conn(|c| {
            let count = c.query_row(
                "SELECT COUNT(*) FROM runs WHERE project_id = ?1 AND loop_kind = ?2
                   AND issue_number = ?3 AND status = 'succeeded'",
                params![project_id, loop_kind, issue_number],
                |row| row.get(0),
            )?;
            Ok(count)
        })
    }

    /// The branch of the most recent run of `loop_kind` for an issue, if one
    /// recorded a branch. The separate-mode handoff sweep (ADR 0008) uses it to
    /// find the planner's spec PR branch and check whether it merged.
    pub fn branch_for_issue(
        &self,
        project_id: &str,
        loop_kind: &str,
        issue_number: i64,
    ) -> Result<Option<String>> {
        self.with_conn(|c| {
            let branch = c
                .query_row(
                    "SELECT branch FROM runs WHERE project_id = ?1 AND loop_kind = ?2
                       AND issue_number = ?3 AND branch IS NOT NULL
                     ORDER BY created_at DESC LIMIT 1",
                    params![project_id, loop_kind, issue_number],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten();
            Ok(branch)
        })
    }

    /// Whether the issue has already retreated to planning once before, via
    /// an earlier `needs_plan` run of the given loop — the "same issue twice"
    /// leg of the worker's vibration guard (issue #135). A spec-file check
    /// alone misses issues that were re-labeled `ready` without a spec ever
    /// landing on disk (e.g. planning resolved the ambiguity some other way);
    /// this catches that case so a second retreat escalates to a human
    /// instead of bouncing `ready` ⇄ `plan` forever.
    pub fn issue_has_needs_plan_run(
        &self,
        project_id: &str,
        loop_kind: &str,
        issue_number: i64,
    ) -> Result<bool> {
        self.with_conn(|c| {
            let exists = c
                .prepare(
                    "SELECT 1 FROM runs WHERE project_id = ?1 AND loop_kind = ?2
                       AND issue_number = ?3 AND status = 'needs_plan' LIMIT 1",
                )?
                .exists(params![project_id, loop_kind, issue_number])?;
            Ok(exists)
        })
    }

    /// Runs that own a worktree, matched by branch name or recorded path
    /// (newest first). Both keys are tried because the reaper resolves
    /// worktrees from `git worktree list`, whose paths may be canonicalized
    /// differently than what was stored.
    pub fn runs_for_worktree(
        &self,
        project_id: &str,
        branch: Option<&str>,
        worktree_path: &str,
    ) -> Result<Vec<RunRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM runs WHERE project_id = ?1
                   AND (branch = ?2 OR worktree_path = ?3)
                 ORDER BY created_at DESC",
            )?;
            let runs = stmt
                .query_map(params![project_id, branch, worktree_path], run_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(runs)
        })
    }

    pub fn list_runs(&self, active_only: bool) -> Result<Vec<RunRecord>> {
        self.with_conn(|c| {
            let sql = if active_only {
                "SELECT * FROM runs WHERE status IN ('queued','running','interrupted')
                 ORDER BY created_at DESC"
            } else {
                "SELECT * FROM runs ORDER BY created_at DESC LIMIT 50"
            };
            let mut stmt = c.prepare(sql)?;
            let runs = stmt
                .query_map([], run_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(runs)
        })
    }

    pub fn update_run_status(
        &self,
        id: &str,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|c| {
            let (started, finished) = match status {
                RunStatus::Running => (Some(now()), None),
                RunStatus::Succeeded
                | RunStatus::Failed
                | RunStatus::Cancelled
                | RunStatus::Skipped
                | RunStatus::NeedsPlan
                | RunStatus::Decomposed => (None, Some(now())),
                _ => (None, None),
            };
            c.execute(
                "UPDATE runs SET status = ?2, error = COALESCE(?3, error),
                   started_at = COALESCE(?4, started_at),
                   finished_at = COALESCE(?5, finished_at)
                 WHERE id = ?1",
                params![id, status.as_str(), error, started, finished],
            )?;
            Ok(())
        })
    }

    pub fn update_run_step(&self, id: &str, step: &str, checkpoint_json: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET step = ?2, checkpoint_json = ?3 WHERE id = ?1",
                params![id, step, checkpoint_json],
            )?;
            Ok(())
        })
    }

    pub fn update_run_worktree(&self, id: &str, branch: &str, worktree_path: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET branch = ?2, worktree_path = ?3 WHERE id = ?1",
                params![id, branch, worktree_path],
            )?;
            Ok(())
        })
    }

    pub fn update_run_mux(&self, id: &str, kind: &str, session: &str, pane: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET mux_kind = ?2, mux_session = ?3, mux_pane_id = ?4 WHERE id = ?1",
                params![id, kind, session, pane],
            )?;
            Ok(())
        })
    }

    /// Pin the run's launch profile (role-based routing). Written once, at the
    /// first pane spawn; later spawns and resumes read it back.
    pub fn update_run_agent_profile(&self, id: &str, profile: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET agent_profile = ?2 WHERE id = ?1",
                params![id, profile],
            )?;
            Ok(())
        })
    }

    /// Record the run's routing arm (routing 3/3, issue #66): `Some("explore")`
    /// or `Some("escalated")`, or `None` to leave it on the mainline. Written
    /// when a run is diverted to an explore profile, or when it escalates
    /// (unless it is already an explore run — explore takes priority).
    pub fn update_run_routing_arm(&self, id: &str, arm: Option<&str>) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET routing_arm = ?2 WHERE id = ?1",
                params![id, arm],
            )?;
            Ok(())
        })
    }

    /// Record (or clear, with None) the run's native agent session id.
    pub fn update_run_agent_session(&self, id: &str, session: Option<&str>) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET agent_session_id = ?2 WHERE id = ?1",
                params![id, session],
            )?;
            Ok(())
        })
    }

    pub fn update_interaction_state(
        &self,
        id: &str,
        state: Option<InteractionState>,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET interaction_state = ?2 WHERE id = ?1",
                params![id, state.map(|s| s.as_str())],
            )?;
            Ok(())
        })
    }

    /// Parked reviews for the dashboard (ADR 0009 / issue #153): review runs
    /// that ended `Succeeded`, still carry `AwaitingHuman`, and actually
    /// emitted `review.awaiting_human`. The event is the discriminator: a
    /// turn-scoped `AwaitingHuman` that merely lingered onto a `Succeeded`
    /// pr-reviewer run (Impl, or combined Plan) never emits it, so it must not
    /// show as a parked review. `interaction_state='awaiting_human'` keeps
    /// cleared parks out; `status='succeeded'` guards against aborted runs.
    pub fn list_parked_reviews(&self) -> Result<Vec<RunRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM runs
                 WHERE status = 'succeeded'
                   AND interaction_state = 'awaiting_human'
                   AND EXISTS (SELECT 1 FROM events e
                               WHERE e.run_id = runs.id
                                 AND e.kind = 'review.awaiting_human')
                 ORDER BY created_at DESC",
            )?;
            let runs = stmt
                .query_map([], run_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(runs)
        })
    }

    /// Clear the parked-review signal on every `Succeeded` run of an issue
    /// (ADR 0009). Called when a fresh review round supersedes the prior head,
    /// and when the issue closes — so a stale park leaves the dashboard.
    /// Returns how many runs were cleared.
    pub fn clear_parked_reviews_for_issue(
        &self,
        project_id: &str,
        issue_number: i64,
    ) -> Result<usize> {
        self.with_conn(|c| {
            let n = c.execute(
                "UPDATE runs SET interaction_state = NULL
                 WHERE project_id = ?1 AND issue_number = ?2
                   AND status = 'succeeded' AND interaction_state = 'awaiting_human'",
                params![project_id, issue_number],
            )?;
            Ok(n)
        })
    }

    pub fn set_desired_state(&self, id: &str, state: Option<DesiredState>) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET desired_state = ?2 WHERE id = ?1",
                params![id, state.map(|s| s.as_str())],
            )?;
            Ok(())
        })
    }

    pub fn read_desired_state(&self, id: &str) -> Result<Option<DesiredState>> {
        self.with_conn(|c| {
            let s: Option<String> =
                c.query_row("SELECT desired_state FROM runs WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?;
            Ok(s.as_deref().and_then(DesiredState::parse))
        })
    }

    pub fn begin_turn(
        &self,
        run_id: &str,
        turn_id: &str,
        purpose: &str,
        prompt_path: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET turn_no = turn_no + 1, current_turn_id = ?2 WHERE id = ?1",
                params![run_id, turn_id],
            )?;
            let turn_no: i64 =
                c.query_row("SELECT turn_no FROM runs WHERE id = ?1", [run_id], |r| {
                    r.get(0)
                })?;
            c.execute(
                "INSERT INTO turns (id, run_id, turn_no, purpose, prompt_path, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![turn_id, run_id, turn_no, purpose, prompt_path, now()],
            )?;
            Ok(())
        })
    }

    pub fn finish_turn(
        &self,
        turn_id: &str,
        outcome: &str,
        result_json: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE turns SET outcome = ?2, result_json = ?3, finished_at = ?4 WHERE id = ?1",
                params![turn_id, outcome, result_json, now()],
            )?;
            Ok(())
        })
    }

    pub fn list_turns(&self, run_id: &str) -> Result<Vec<TurnRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, run_id, turn_no, purpose, prompt_path, result_json,
                        outcome, started_at, finished_at
                 FROM turns WHERE run_id = ?1 ORDER BY turn_no ASC",
            )?;
            let turns = stmt
                .query_map([run_id], |row| {
                    Ok(TurnRecord {
                        id: row.get(0)?,
                        run_id: row.get(1)?,
                        turn_no: row.get(2)?,
                        purpose: row.get(3)?,
                        prompt_path: row.get(4)?,
                        result_json: row.get(5)?,
                        outcome: row.get(6)?,
                        started_at: row.get(7)?,
                        finished_at: row.get(8)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(turns)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 7, "Fix the bug").unwrap();
        assert_eq!(run.status, RunStatus::Queued);
        assert_eq!(run.issue_number, 7);

        store
            .update_run_status(&run.id, RunStatus::Running, None)
            .unwrap();
        store
            .update_run_step(&run.id, "execute", "{\"a\":1}")
            .unwrap();
        store
            .update_run_mux(&run.id, "tmux", "meguri", "%3")
            .unwrap();
        store
            .update_interaction_state(&run.id, Some(InteractionState::AgentWorking))
            .unwrap();

        let got = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(got.status, RunStatus::Running);
        assert_eq!(got.step, "execute");
        assert_eq!(got.mux_pane_id.as_deref(), Some("%3"));
        assert_eq!(got.interaction_state, Some(InteractionState::AgentWorking));
    }

    #[test]
    fn unique_active_run_per_issue() {
        let store = Store::open_in_memory().unwrap();
        store.create_run("demo", 7, "t").unwrap();
        assert!(store.create_run("demo", 7, "t").is_err());
    }

    #[test]
    fn active_run_uniqueness_is_scoped_by_loop_kind() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 7, "t").unwrap();
        assert_eq!(run.loop_kind, "worker");

        // Same issue under another loop: allowed.
        let other = store
            .create_run_for_loop("demo", "reviewer", 7, "t")
            .unwrap();
        assert_eq!(other.loop_kind, "reviewer");

        // Same (project, loop, issue) while active: rejected.
        assert!(
            store
                .create_run_for_loop("demo", "reviewer", 7, "t")
                .is_err()
        );
    }

    #[test]
    fn find_run_by_prefix_and_issue() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 42, "t").unwrap();
        let prefix = &run.id[..6];
        assert_eq!(store.find_run(prefix).unwrap().unwrap().id, run.id);
        assert_eq!(store.find_run("42").unwrap().unwrap().id, run.id);
        assert!(store.find_run("nope").unwrap().is_none());
    }

    #[test]
    fn issue_has_succeeded_run_tracks_terminal_success() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 9, "t").unwrap();
        assert!(!store.issue_has_succeeded_run("demo", "worker", 9).unwrap());

        store
            .update_run_status(&run.id, RunStatus::Skipped, None)
            .unwrap();
        assert!(!store.issue_has_succeeded_run("demo", "worker", 9).unwrap());

        let run2 = store.create_run("demo", 9, "t").unwrap();
        store
            .update_run_status(&run2.id, RunStatus::Succeeded, None)
            .unwrap();
        assert!(store.issue_has_succeeded_run("demo", "worker", 9).unwrap());
        // Scoped: another loop's discovery on the same issue is unaffected.
        assert!(!store.issue_has_succeeded_run("demo", "planner", 9).unwrap());
        assert!(!store.issue_has_succeeded_run("other", "worker", 9).unwrap());
        assert!(!store.issue_has_succeeded_run("demo", "worker", 10).unwrap());
    }

    #[test]
    fn issue_processed_current_body_is_digest_aware_with_null_legacy() {
        let store = Store::open_in_memory().unwrap();

        // A legacy succeeded run with a NULL digest suppresses any body
        // (preserves the old permanent-suppression behavior on upgrade).
        let legacy = store.create_run("demo", 9, "t").unwrap();
        store
            .update_run_status(&legacy.id, RunStatus::Succeeded, None)
            .unwrap();
        assert!(
            store
                .issue_processed_current_body("demo", "worker", 9, "any")
                .unwrap()
        );

        // A run that recorded a digest covers only that exact body.
        let run = store.create_run("demo", 10, "t").unwrap();
        store.set_run_body_digest(&run.id, "aaa").unwrap();
        store
            .update_run_status(&run.id, RunStatus::Succeeded, None)
            .unwrap();
        assert!(
            store
                .issue_processed_current_body("demo", "worker", 10, "aaa")
                .unwrap()
        );
        // A different body is no longer covered → suppression lifts.
        assert!(
            !store
                .issue_processed_current_body("demo", "worker", 10, "bbb")
                .unwrap()
        );
        // No succeeded run at all: never suppressed.
        assert!(
            !store
                .issue_processed_current_body("demo", "worker", 11, "aaa")
                .unwrap()
        );
    }

    #[test]
    fn cadence_consumed_counts_non_skipped_stamped_runs_in_window() {
        let store = Store::open_in_memory().unwrap();
        // Window start well before now; every fresh run's created_at is inside.
        let start = 0;

        // Unstamped run: not counted.
        store.create_run_for_loop("demo", "worker", 1, "t").unwrap();
        assert_eq!(store.cadence_consumed("demo", "sns", start).unwrap(), 0);

        // Two stamped runs, one succeeded one failed: both count (attempt, not
        // outcome).
        let a = store
            .create_run_for_loop_cadence("demo", "worker", 2, "t", Some("sns"))
            .unwrap();
        store
            .update_run_status(&a.id, RunStatus::Succeeded, None)
            .unwrap();
        let b = store
            .create_run_for_loop_cadence("demo", "worker", 3, "t", Some("sns"))
            .unwrap();
        store
            .update_run_status(&b.id, RunStatus::Failed, None)
            .unwrap();
        assert_eq!(store.cadence_consumed("demo", "sns", start).unwrap(), 2);

        // A skipped stamped run does not count.
        let c = store
            .create_run_for_loop_cadence("demo", "worker", 4, "t", Some("sns"))
            .unwrap();
        store
            .update_run_status(&c.id, RunStatus::Skipped, None)
            .unwrap();
        assert_eq!(store.cadence_consumed("demo", "sns", start).unwrap(), 2);

        // Scoped by project and label.
        assert_eq!(store.cadence_consumed("other", "sns", start).unwrap(), 0);
        assert_eq!(store.cadence_consumed("demo", "nl", start).unwrap(), 0);

        // A window start in the far future excludes everything.
        assert_eq!(
            store
                .cadence_consumed("demo", "sns", 32_000_000_000)
                .unwrap(),
            0
        );
    }

    #[test]
    fn succeeded_run_count_counts_only_terminal_successes() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.succeeded_run_count("demo", "worker", 9).unwrap(), 0);

        // Terminal statuses only: an active run would trip the unique
        // (project, loop, issue) index on the next create.
        for status in [RunStatus::Skipped, RunStatus::Failed, RunStatus::Cancelled] {
            let run = store.create_run("demo", 9, "t").unwrap();
            store.update_run_status(&run.id, status, None).unwrap();
        }
        assert_eq!(store.succeeded_run_count("demo", "worker", 9).unwrap(), 0);

        for _ in 0..2 {
            let run = store.create_run("demo", 9, "t").unwrap();
            store
                .update_run_status(&run.id, RunStatus::Succeeded, None)
                .unwrap();
        }
        assert_eq!(store.succeeded_run_count("demo", "worker", 9).unwrap(), 2);
        // Scoped by loop, project, and issue.
        assert_eq!(store.succeeded_run_count("demo", "planner", 9).unwrap(), 0);
        assert_eq!(store.succeeded_run_count("other", "worker", 9).unwrap(), 0);
        assert_eq!(store.succeeded_run_count("demo", "worker", 10).unwrap(), 0);
    }

    #[test]
    fn runs_for_worktree_matches_branch_or_path() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 5, "t").unwrap();
        store
            .update_run_worktree(&run.id, "meguri/5-t-abc123", "/wt/demo/meguri-5-t-abc123")
            .unwrap();

        let by_branch = store
            .runs_for_worktree("demo", Some("meguri/5-t-abc123"), "/other/path")
            .unwrap();
        assert_eq!(by_branch.len(), 1);
        assert_eq!(by_branch[0].id, run.id);

        let by_path = store
            .runs_for_worktree("demo", None, "/wt/demo/meguri-5-t-abc123")
            .unwrap();
        assert_eq!(by_path.len(), 1);

        assert!(
            store
                .runs_for_worktree(
                    "other",
                    Some("meguri/5-t-abc123"),
                    "/wt/demo/meguri-5-t-abc123"
                )
                .unwrap()
                .is_empty(),
            "scoped by project"
        );
        assert!(
            store
                .runs_for_worktree("demo", Some("meguri/9-x-ffffff"), "/nope")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn desired_state_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 1, "t").unwrap();
        store
            .set_desired_state(&run.id, Some(DesiredState::Paused))
            .unwrap();
        assert_eq!(
            store.read_desired_state(&run.id).unwrap(),
            Some(DesiredState::Paused)
        );
        store.set_desired_state(&run.id, None).unwrap();
        assert_eq!(store.read_desired_state(&run.id).unwrap(), None);
    }

    #[test]
    fn agent_session_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 1, "t").unwrap();
        assert_eq!(run.agent_session_id, None);

        store
            .update_run_agent_session(&run.id, Some("sess-abc"))
            .unwrap();
        let got = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(got.agent_session_id.as_deref(), Some("sess-abc"));

        store.update_run_agent_session(&run.id, None).unwrap();
        let got = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(got.agent_session_id, None);
    }

    #[test]
    fn agent_profile_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 1, "t").unwrap();
        // Runs start with no pinned profile (migration 0004 backfills NULL).
        assert_eq!(run.agent_profile, None);

        store
            .update_run_agent_profile(&run.id, "claude-sonnet")
            .unwrap();
        let got = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(got.agent_profile.as_deref(), Some("claude-sonnet"));
    }

    #[test]
    fn turns_recorded() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 1, "t").unwrap();
        store
            .begin_turn(&run.id, "turn-1", "execute", "/tmp/p.md")
            .unwrap();
        store.finish_turn("turn-1", "success", Some("{}")).unwrap();
        let got = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(got.turn_no, 1);
        assert_eq!(got.current_turn_id.as_deref(), Some("turn-1"));
    }

    #[test]
    fn list_turns_in_turn_order() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 1, "t").unwrap();
        assert!(store.list_turns(&run.id).unwrap().is_empty());

        store
            .begin_turn(&run.id, "turn-1", "execute", "/tmp/p1.md")
            .unwrap();
        store.finish_turn("turn-1", "success", Some("{}")).unwrap();
        store
            .begin_turn(&run.id, "turn-2", "validate-fix", "/tmp/p2.md")
            .unwrap();

        let turns = store.list_turns(&run.id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].turn_no, 1);
        assert_eq!(turns[0].purpose, "execute");
        assert_eq!(turns[0].outcome.as_deref(), Some("success"));
        assert!(turns[0].finished_at.is_some());
        assert_eq!(turns[1].turn_no, 2);
        assert_eq!(turns[1].outcome, None);
    }

    #[test]
    fn run_record_serializes_snake_case_states() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 5, "t").unwrap();
        store
            .update_interaction_state(&run.id, Some(InteractionState::AwaitingHuman))
            .unwrap();
        store
            .set_desired_state(&run.id, Some(DesiredState::Paused))
            .unwrap();
        store
            .update_run_status(&run.id, RunStatus::Running, None)
            .unwrap();

        let got = store.get_run(&run.id).unwrap().unwrap();
        let v = serde_json::to_value(&got).unwrap();
        assert_eq!(v["status"], "running");
        assert_eq!(v["interaction_state"], "awaiting_human");
        assert_eq!(v["desired_state"], "paused");
        assert!(v["started_at"].is_string());
        assert!(v["finished_at"].is_null());
    }

    /// Make a run parked: succeeded, awaiting a human, park event emitted.
    fn park(store: &Store, issue: i64) -> String {
        let run = store
            .create_run_for_loop("demo", "pr-reviewer", issue, "t")
            .unwrap();
        store
            .update_interaction_state(&run.id, Some(InteractionState::AwaitingHuman))
            .unwrap();
        store
            .emit(
                Some(&run.id),
                "review.awaiting_human",
                serde_json::json!({}),
            )
            .unwrap();
        store
            .update_run_status(&run.id, RunStatus::Succeeded, None)
            .unwrap();
        run.id
    }

    #[test]
    fn list_parked_reviews_needs_succeeded_awaiting_and_the_event() {
        let store = Store::open_in_memory().unwrap();
        let parked = park(&store, 7);

        // Turn-linger: succeeded + awaiting_human but no park event (an Impl or
        // combined-Plan pr-reviewer whose own turn raised awaiting_human). Must
        // not show.
        let linger = store
            .create_run_for_loop("demo", "pr-reviewer", 8, "t")
            .unwrap();
        store
            .update_interaction_state(&linger.id, Some(InteractionState::AwaitingHuman))
            .unwrap();
        store
            .update_run_status(&linger.id, RunStatus::Succeeded, None)
            .unwrap();

        // Aborted while awaiting a human: the park event is present but the run
        // did not end succeeded. Must not show.
        for (issue, status) in [
            (9, RunStatus::Cancelled),
            (10, RunStatus::Failed),
            (11, RunStatus::Skipped),
        ] {
            let r = store
                .create_run_for_loop("demo", "pr-reviewer", issue, "t")
                .unwrap();
            store
                .update_interaction_state(&r.id, Some(InteractionState::AwaitingHuman))
                .unwrap();
            store
                .emit(Some(&r.id), "review.awaiting_human", serde_json::json!({}))
                .unwrap();
            store.update_run_status(&r.id, status, None).unwrap();
        }

        let list = store.list_parked_reviews().unwrap();
        assert_eq!(list.len(), 1, "only the genuine park shows");
        assert_eq!(list[0].id, parked);
    }

    #[test]
    fn clear_parked_reviews_for_issue_drops_it_from_the_list() {
        let store = Store::open_in_memory().unwrap();
        park(&store, 7);
        park(&store, 8);
        assert_eq!(store.list_parked_reviews().unwrap().len(), 2);

        let cleared = store.clear_parked_reviews_for_issue("demo", 7).unwrap();
        assert_eq!(cleared, 1);
        let list = store.list_parked_reviews().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].issue_number, 8);

        // A different project's issue #8 is untouched by another project's clear.
        assert_eq!(store.clear_parked_reviews_for_issue("other", 8).unwrap(), 0);
        assert_eq!(store.list_parked_reviews().unwrap().len(), 1);
    }
}
