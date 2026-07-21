//! The shared issue→PR flow every loop drives: claim the labeled issue →
//! worktree → interactive agent turns in a mux pane → verified commits → PR.
//! Steps are checkpointed in `runs.step` so an interrupted run resumes where
//! it left off. Loop-specific behavior (trigger label, prompts, extra
//! verification, PR shape, label settling) plugs in via [`Flavor`].

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Deps, StoreControl, WorkerOutcome, lane_for_loop};
use crate::agent_session;
use crate::config::{Deliver, LaunchMode};
use crate::forge;
use crate::gitops;
use crate::launch;
use crate::mux::{PaneId, PaneSpec};
use crate::notify::Notification;
use crate::routing;
use crate::store::{InteractionState, LANE_AUTHOR, RunRecord, RunStatus};
use crate::tasks::{self, TaskKey};
use crate::turn::{
    TurnConfig, TurnEngine, TurnOutcome, TurnResultFile, TurnStatus, prepare_turn,
    prepare_turn_isolated,
};

pub const STEP_PREPARE_WORK: &str = "prepare-work";
pub const STEP_PREPARE_WORKTREE: &str = "prepare-worktree";
pub const STEP_EXECUTE: &str = "execute";
pub const STEP_VALIDATE: &str = "validate";
/// The worker's internal review→fix loop (ADR 0006), between `validate` and
/// `open-pr`. Only loops whose flavor opts in ([`Flavor::self_reviews`]) run
/// it; the rest step straight from `validate` to `open-pr`.
pub const STEP_SELF_REVIEW: &str = "self-review";
pub const STEP_OPEN_PR: &str = "open-pr";

/// Which side of the symmetric plan/impl loop a run is on (ADR 0008). The
/// self-review turn frames its lenses differently for a spec/ADR document
/// (Plan) than for a code diff (Impl); the pr-reviewer reads it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Plan,
    #[default]
    Impl,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Impl => "impl",
        }
    }

    /// Whether the project's guard is enabled for this kind (ADR 0008 §1).
    pub fn guard_enabled(self, review: &crate::config::ReviewConfig) -> bool {
        match self {
            Self::Plan => review.guard.plan,
            Self::Impl => review.guard.impl_enabled,
        }
    }
}

/// What makes a loop's flow different from another's; everything else
/// (claiming, checkpointing, turns, validation, escalation) is shared.
/// The default method bodies implement the issue-triggered "new branch, new
/// PR" shape the worker and planner share; PR-targeted loops (the fixer, the
/// spec worker) override the claim, worktree, and escalation hooks.
#[async_trait]
pub trait Flavor: Send + Sync {
    /// Label that queues an issue for this loop; re-checked at claim time by
    /// the default [`Flavor::prepare_work`].
    fn trigger_label(&self) -> &'static str;

    /// Which side of the symmetric loop this flavor drives (ADR 0008): the
    /// planner is `Plan`, everything else `Impl`. Steers the self-review
    /// framing (spec document vs code diff).
    fn kind(&self) -> Kind {
        Kind::Impl
    }

    /// Whether this loop runs the internal self-review phase (ADR 0006/0008)
    /// between `validate` and `open-pr`. Default: no (the historical
    /// straight-to-PR shape). The worker and the planner opt in — self-review
    /// is symmetric across plan and impl (ADR 0008).
    fn self_reviews(&self) -> bool {
        false
    }

    /// Whether this loop's PR should auto-close its issue on merge (`Closes #N`
    /// vs the non-closing `Refs #N`). Default: yes — an implementation PR
    /// closes its issue. The planner overrides it in separate delivery: the
    /// spec/ADR PR merges on its own and must NOT close the issue (the handoff
    /// then flips it to `ready`, ADR 0008 §6).
    fn pr_closes_issue(&self, deps: &Deps) -> bool {
        let _ = deps;
        true
    }

    /// Claim the run's target and fill the checkpoint. Default: the
    /// coordination layer's atomic claim (label re-verification + working
    /// label in github mode, an atomic DB update in local mode).
    async fn prepare_work(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &mut Checkpoint,
    ) -> Result<PreparedWork> {
        claim_task(deps, run, cp).await
    }

    /// Set up the run's worktree and persist branch/path. Default: a new
    /// run-scoped branch off the project's default branch.
    async fn prepare_worktree(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        create_branch_worktree(deps, run, cp).await
    }

    /// Prompt body for the execute turn.
    fn execute_prompt(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
        worktree: &Path,
    ) -> String;

    /// Loop-specific check of an already git-verified (clean, committed)
    /// worktree; the Err text is fed back to the agent as a corrective
    /// prompt.
    fn verify_work(
        &self,
        run: &RunRecord,
        cp: &Checkpoint,
        worktree: &Path,
    ) -> std::result::Result<(), String>;

    /// Base ref the execute step counts commits against. Default: the
    /// project's default branch (new-branch loops); the fixer counts against
    /// the PR branch's pushed tip instead.
    fn verify_base(&self, deps: &Deps, run: &RunRecord) -> String {
        let _ = run;
        deps.project.default_branch.clone()
    }

    /// Whether this loop's execute turn may (re)establish `cp.subject`
    /// (issue #136). Implementation-shaping turns — the worker, the
    /// planner, the spec worker's takeover — do; fix-family turns whose job
    /// is to amend without changing the nature of the change (the fixer,
    /// the ci-fixer, the conflict resolver) must not, so the PR title stays
    /// fixed to whatever turn established it instead of flapping with every
    /// fix's wording. Default: yes.
    fn sets_subject(&self) -> bool {
        true
    }

    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String;

    /// Settle forge labels once the PR exists. Re-run on resume, so keep it
    /// idempotent.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()>;

    /// Settle the PR's presentation (title/body) once it exists. Default:
    /// no-op — new-PR loops set both at create time (via [`Flavor::pr_title`]
    /// and [`compose_pr_body`]), so there is nothing to transition. Branch
    /// takeovers whose PR was authored by another loop (the spec worker)
    /// override this to move the PR from that loop's presentation to their
    /// own. Re-run on resume, so keep it idempotent.
    async fn settle_presentation(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
    ) -> Result<()> {
        let _ = (deps, run, cp);
        Ok(())
    }

    /// Release the claim marker on `meguri stop`. Default: the coordination
    /// layer's release (drop `meguri:working` / requeue the task).
    async fn release_claim(&self, deps: &Deps, run: &RunRecord) {
        let _ = deps.task_source.release(&run.task_key()).await;
    }

    /// Failure escalation ("Authority": the durable record of why the run
    /// stopped lives with the task). Default: the coordination layer's
    /// escalate (needs-human label + comment / `status='needs_human'` +
    /// reason), with a launch-mode-aware attach hint (issue #169).
    async fn escalate(&self, deps: &Deps, run: &RunRecord, reason: &str) {
        super::escalation::escalate_task(deps, run, reason).await;
    }

    /// The agent ended its execute turn with `needs_plan`: a design decision
    /// must precede implementation (issue #22). Returns the run's terminal
    /// outcome. Default: no plan handoff exists for this loop — a human must
    /// look (only the worker overrides this to demote the issue to
    /// `meguri:plan`).
    async fn on_needs_plan(
        &self,
        deps: &Deps,
        run: &RunRecord,
        worktree: &Path,
        reason: &str,
    ) -> Result<WorkerOutcome> {
        let _ = (deps, worktree);
        Err(NeedsHuman(format!(
            "agent asked for a plan on issue #{} but this loop has no plan \
             handoff: {reason}",
            run.issue_number
        ))
        .into())
    }

    /// The agent ended its execute turn with `decompose`: the issue is too
    /// big for one deliverable and should be split into sub-issues
    /// (issue #24). Returns the run's terminal outcome. Default: no
    /// decompose handoff exists for this loop — a human must look (only the
    /// planner overrides this to file the sub-issues).
    async fn on_decompose(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
        result: &TurnResultFile,
    ) -> Result<WorkerOutcome> {
        let _ = (deps, cp);
        Err(NeedsHuman(format!(
            "agent asked to decompose issue #{} but this loop has no \
             decompose handoff: {}",
            run.issue_number, result.summary
        ))
        .into())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Checkpoint {
    #[serde(default)]
    pub issue_title: String,
    #[serde(default)]
    pub issue_body: String,
    #[serde(default)]
    pub fix_turns_used: u32,
    #[serde(default)]
    pub pr_url: Option<String>,
    #[serde(default)]
    pub pr_number: Option<i64>,
    /// Agent's one-paragraph summary from the verified execute turn
    /// (fallback PR body).
    #[serde(default)]
    pub summary: String,
    /// Agent-authored PR/commit subject from the verified execute turn
    /// (issue #136); `pr_title()` prefers this over `issue_title` when
    /// present. Only set by turns whose [`Flavor::sets_subject`] is true.
    #[serde(default)]
    pub subject: Option<String>,
    /// Agent-authored PR description (Markdown) from the verified execute turn.
    #[serde(default)]
    pub pr_body: Option<String>,
    /// Existing PR head branch to attach to (fixer runs).
    #[serde(default)]
    pub head_branch: Option<String>,
    /// Base-branch tip pinned at claim time (conflict-resolver runs); the
    /// merge target the agent must bring in and verify_work checks for.
    #[serde(default)]
    pub base_sha: Option<String>,
    /// Review threads the run set out to address (fixer runs); replied to
    /// after the push.
    #[serde(default)]
    pub thread_ids: Vec<String>,
    /// The tracked issue carries `meguri:automerge` (auto-merge 1/3, #41):
    /// open the PR non-draft and copy the label onto it so the auto-merger
    /// sweep can arm it. Recorded at claim time.
    #[serde(default)]
    pub automerge: bool,
    /// Self-review rounds already spent this run (ADR 0006). Local-only state
    /// — the internal loop never touches the forge; convergence is bounded by
    /// this counter, not a forge marker.
    #[serde(default)]
    pub self_review_rounds: u32,
    /// Open findings from the ledger, mirrored here every persist (issue #212).
    /// The ledger ([`self_review_ledger`]) is the source of truth; this vec is
    /// kept as a **rollback safety valve** — a binary rolled back past #212 reads
    /// only this field, so an old binary resuming still sees unresolved findings.
    /// Retire the mirror once the rollback target includes #212.
    #[serde(default)]
    pub self_review_pending: Vec<super::self_review::Finding>,
    /// The cumulative self-review findings ledger (issue #212, ADR 0022): one
    /// entry per finding across all rounds, each with its reviewer-confirmed
    /// status (open/fixed/waived), the author's latest disposition, and how many
    /// fix turns it has been through. Convergence is "no open entry left"; a real
    /// ping-pong is an entry still open after two fix turns.
    #[serde(default)]
    pub self_review_ledger: Vec<super::self_review::LedgerEntry>,
    /// The HEAD the last review turn looked at (issue #212). Round 2+ passes the
    /// incremental diff `self_review_last_head..HEAD` on top of the full base
    /// diff, so the reviewer sees exactly what the fix turns changed.
    #[serde(default)]
    pub self_review_last_head: Option<String>,
    /// One entry per self-review round (ADR 0008): what the folded PR-body
    /// `<details>` renders. Carried in the checkpoint so a resumed run keeps
    /// the history it already built.
    #[serde(default)]
    pub self_review_log: Vec<super::self_review::RoundRecord>,
    /// Set once self-review reached a **clean** verdict: the phase converged and
    /// the PR may open. Persisted so a resume distinguishes a clean-at-cap
    /// checkpoint (rounds == max but done) from a genuinely unconverged one
    /// (issue #176). Kept clean-only on purpose: the cap→final-fix path
    /// (issue #212) does NOT set this, so a binary rolled back past #212 sees
    /// `converged == false` and escalates the unreviewed fix instead of
    /// publishing it as clean.
    #[serde(default)]
    pub self_review_converged: bool,
    /// Set once the cap→final-fix path published (issue #212, ADR 0022): the last
    /// fix turn was not re-reviewed (check_command + tree verification passed).
    /// Distinct from [`self_review_converged`] on purpose (rollback safety valve,
    /// above). Read by `compose_pr_body` / `post_self_review_status` to record
    /// the non-re-review in the PR footer and commit status, and by the phase's
    /// resume short-circuit alongside `self_review_converged`.
    #[serde(default)]
    pub self_review_final_fix_unreviewed: bool,
    /// Set (and persisted) the moment the phase commits to the cap→final-fix path,
    /// before the final fix turn runs (issue #212). It makes the branch decision
    /// crash-safe: a resume mid-final-fix routes straight back to the final-fix
    /// path instead of re-running ping-pong detection — the final fix bumps a
    /// finding's `fix_attempts`, which would otherwise look like a ping-pong and
    /// wrongly escalate. Cleared implicitly once
    /// [`self_review_final_fix_unreviewed`] is set (the phase is done).
    #[serde(default)]
    pub self_review_final_fix_started: bool,
    /// How many times this run has escalated its launch profile (routing 3/3,
    /// issue #66) — a counter for observability (it rides the `run.escalated`
    /// event's `level`), not a chain index: the next target is derived from the
    /// currently-pinned profile's position in the chain. Survives a resume so a
    /// crash mid-escalation doesn't re-climb.
    #[serde(default)]
    pub escalation_level: u32,
    /// Validation-fix turns run under the *current* pinned profile since it was
    /// (re)pinned (routing 3/3, issue #66). Reset to 0 on each escalation, and
    /// that reset is persisted *before* the pin advances — so escalation only
    /// fires once the current profile has actually had a fix turn
    /// (`>= 1`). This is what makes escalation crash-safe: a resume after the
    /// pin advanced but before the fix turn ran sees `0` and lets the new pin
    /// try, instead of skipping it to the next chain entry.
    #[serde(default)]
    pub pin_fix_turns: u32,
    /// The repo `meguri.toml` values pinned at claim time (issue #165): read
    /// once from the worktree at the first worktree-ready point, then reused
    /// unchanged for the run's life (a since-tampered worktree or ref cannot
    /// weaken the completion contract; ADR 0011). `None` = not yet resolved;
    /// `Some(RepoConfig::default())` = read, but no `meguri.toml` (or it was
    /// invalid and fell back to "as if absent").
    #[serde(default)]
    pub repo_config: Option<crate::config::RepoConfig>,
}

/// Error kind signalling "a human needs to look"; the run is failed on the
/// forge with the needs-human label and an explanatory comment.
#[derive(Debug, thiserror::Error)]
#[error("needs human: {0}")]
pub struct NeedsHuman(pub String);

pub async fn run_flow(deps: &Deps, run_id: &str, flavor: &dyn Flavor) -> Result<WorkerOutcome> {
    let run = deps
        .store
        .get_run(run_id)?
        .with_context(|| format!("run {run_id} not found"))?;

    deps.store
        .update_run_status(run_id, RunStatus::Running, None)?;
    deps.store.emit(
        Some(run_id),
        "run.started",
        json!({ "issue": run.issue_number, "step": run.step }),
    )?;

    let result = drive(deps, &run, flavor).await;
    // Reap the collab advisor (issue #111) on every terminal path — success,
    // needs-plan, decompose, stop, interrupt, failure — so it never outlives
    // the worker run, independent of `keep_pane` (ADR 0006 principle 3). No-op
    // when collab is off or there is no advisor.
    release_advisor(deps, &run).await;
    match result {
        Ok(outcome) => {
            match &outcome {
                WorkerOutcome::Succeeded { pr_url } => {
                    deps.store
                        .update_run_status(run_id, RunStatus::Succeeded, None)?;
                    deps.store
                        .emit(Some(run_id), "run.succeeded", json!({ "pr": pr_url }))?;
                }
                WorkerOutcome::Stopped => {
                    finalize_cancelled(deps, &run, flavor).await?;
                }
                WorkerOutcome::Interrupted(reason) => {
                    deps.store
                        .update_run_status(run_id, RunStatus::Interrupted, Some(reason))?;
                    deps.store.emit(
                        Some(run_id),
                        "run.interrupted",
                        json!({ "reason": reason }),
                    )?;
                }
                WorkerOutcome::Skipped(reason) => {
                    deps.store
                        .update_run_status(run_id, RunStatus::Skipped, Some(reason))?;
                    deps.store
                        .emit(Some(run_id), "run.skipped", json!({ "reason": reason }))?;
                }
                WorkerOutcome::NeedsPlan(reason) => {
                    deps.store
                        .update_run_status(run_id, RunStatus::NeedsPlan, Some(reason))?;
                    deps.store
                        .emit(Some(run_id), "run.needs_plan", json!({ "reason": reason }))?;
                }
                WorkerOutcome::Decomposed(reason) => {
                    deps.store
                        .update_run_status(run_id, RunStatus::Decomposed, Some(reason))?;
                    deps.store
                        .emit(Some(run_id), "run.decomposed", json!({ "reason": reason }))?;
                }
            }
            Ok(outcome)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            deps.store
                .update_run_status(run_id, RunStatus::Failed, Some(&msg))?;
            deps.store
                .emit(Some(run_id), "run.failed", json!({ "error": msg }))?;
            flavor.escalate(deps, &run, &msg).await;
            Err(e)
        }
    }
}

async fn drive(deps: &Deps, run: &RunRecord, flavor: &dyn Flavor) -> Result<WorkerOutcome> {
    let mut checkpoint: Checkpoint = serde_json::from_str(&run.checkpoint_json).unwrap_or_default();
    let mut step = run.step.clone();

    if step == STEP_PREPARE_WORK {
        match flavor.prepare_work(deps, run, &mut checkpoint).await? {
            PreparedWork::Claimed => {}
            PreparedWork::Skip(reason) => return Ok(WorkerOutcome::Skipped(reason)),
        }
        // Record the normalized-body digest this run acted on (issue #142), in
        // the shared step — not in a flavor's claim path — so a loop with a
        // custom prepare_work (the spec worker) still stamps its succeeded runs
        // instead of leaving them NULL (which the discover guard would read as
        // permanently suppressed). Only github issue runs carry an issue body;
        // local tasks have none.
        if let TaskKey::Issue(_) = run.task_key() {
            deps.store
                .set_run_body_digest(&run.id, &tasks::body_digest(&checkpoint.issue_body))?;
        }
        step = save_step(deps, run, STEP_PREPARE_WORKTREE, &checkpoint)?;
    }

    if step == STEP_PREPARE_WORKTREE {
        flavor.prepare_worktree(deps, run, &checkpoint).await?;
        step = save_step(deps, run, STEP_EXECUTE, &checkpoint)?;
    }

    // Re-read: prepare_worktree persisted branch/worktree_path.
    let run = deps
        .store
        .get_run(&run.id)?
        .context("run vanished mid-drive")?;
    let worktree = PathBuf::from(
        run.worktree_path
            .clone()
            .context("run has no worktree path")?,
    );

    // Repo config (issue #165): read the worktree's `meguri.toml` exactly once,
    // here at the first worktree-ready point, and pin it to the checkpoint. The
    // pin persists *before* any agent turn runs, so a run cannot weaken its own
    // completion contract by editing `meguri.toml` (or `update-ref`-ing a ref)
    // mid-run, and a crash→resume reuses the pin instead of re-reading a
    // since-tampered worktree. See ADR 0011.
    if checkpoint.repo_config.is_none() {
        let pinned = match crate::config::RepoConfig::load_from_worktree(&worktree) {
            Ok(opt) => opt.unwrap_or_default(),
            Err(e) => {
                tracing::warn!(
                    "run {}: ignoring invalid {}/meguri.toml: {e:#} — continuing with host config only",
                    run.id,
                    worktree.display()
                );
                deps.store.emit(
                    Some(&run.id),
                    "repo_config.invalid",
                    json!({ "error": format!("{e:#}") }),
                )?;
                crate::config::RepoConfig::default()
            }
        };
        checkpoint.repo_config = Some(pinned);
        // Persist the pin at the current step before proceeding: the completion
        // contract must be fixed before the first agent turn can touch it.
        save_step(deps, &run, &step, &checkpoint)?;
    }

    // Fold the pinned repo config into a run-scoped `Deps` so every downstream
    // step (execute / validate / self-review / deliver) resolves the effective
    // 4-layer project config through the unchanged `*_for` / `deps.project`
    // consumers. `deps_owned` outlives the borrow; when there's nothing to fold,
    // the original `deps` is kept as-is.
    let deps_owned;
    let deps: &Deps = match checkpoint.repo_config.as_ref() {
        Some(repo) if repo.has_values() => {
            deps_owned = deps.with_repo_config(repo);
            &deps_owned
        }
        _ => deps,
    };

    if step == STEP_EXECUTE {
        // Collab measurement (issue #121): stamp the intended collab plane so
        // `meguri stats collab` can compare advisor-on vs advisor-off. Only the
        // 'advisor' case is written; other runs stay NULL (read as 'off'), which
        // keeps the inert regime when `[collab]` is off. Independent of the
        // best-effort spawn below — we record the intended plane, not spawn luck.
        if crate::collab::run_gets_advisor(&deps.config, &run) {
            deps.store
                .update_run_collab_mode(&run.id, crate::collab::COLLAB_MODE_ADVISOR)?;
        }
        // Collab advisor (issue #111): spawn the plan-author advisor before the
        // worker's turns so its consult block can join the first prompt.
        // Best-effort — a failure leaves the worker untouched. Re-run on resume
        // re-embodies the advisor ("捨てて張り直す").
        ensure_advisor(deps, &run, &worktree, &checkpoint).await;
        match execute(deps, &run, &mut checkpoint, &worktree, flavor).await? {
            StepFlow::Continue => {}
            StepFlow::Stopped => return Ok(WorkerOutcome::Stopped),
            StepFlow::Interrupted(r) => return Ok(WorkerOutcome::Interrupted(r)),
            StepFlow::NeedsPlan(reason) => {
                let outcome = flavor.on_needs_plan(deps, &run, &worktree, &reason).await?;
                finish_pane(deps, &run).await;
                return Ok(outcome);
            }
            StepFlow::Decompose(result) => {
                let outcome = flavor
                    .on_decompose(deps, &run, &checkpoint, &result)
                    .await?;
                finish_pane(deps, &run).await;
                return Ok(outcome);
            }
        }
        step = save_step(deps, &run, STEP_VALIDATE, &checkpoint)?;
    }

    if step == STEP_VALIDATE {
        match validate(deps, &run, &mut checkpoint, &worktree, STEP_VALIDATE).await? {
            StepFlow::Continue => {}
            StepFlow::Stopped => return Ok(WorkerOutcome::Stopped),
            StepFlow::Interrupted(r) => return Ok(WorkerOutcome::Interrupted(r)),
            StepFlow::NeedsPlan(reason) => {
                // Unreachable: validate() escalates a needs_plan fix turn
                // (the work is already committed by then).
                return Err(NeedsHuman(format!(
                    "agent asked for a plan during validation on issue #{}: {reason}",
                    run.issue_number
                ))
                .into());
            }
            StepFlow::Decompose(result) => {
                // Unreachable: validate() escalates a decompose fix turn
                // (the work is already committed by then).
                return Err(NeedsHuman(format!(
                    "agent asked to decompose during validation on issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
        }
        step = save_step(deps, &run, STEP_SELF_REVIEW, &checkpoint)?;
    }

    if step == STEP_SELF_REVIEW {
        if flavor.self_reviews() && deps.config.review_for(&deps.project).enabled {
            match super::self_review::self_review(deps, &run, &mut checkpoint, &worktree, flavor)
                .await?
            {
                StepFlow::Continue => {}
                StepFlow::Stopped => return Ok(WorkerOutcome::Stopped),
                StepFlow::Interrupted(r) => return Ok(WorkerOutcome::Interrupted(r)),
                StepFlow::NeedsPlan(reason) => {
                    // needs_plan makes no sense once work is committed and
                    // under self-review — a human looks.
                    return Err(NeedsHuman(format!(
                        "agent asked for a plan during self-review on issue #{}: {reason}",
                        run.issue_number
                    ))
                    .into());
                }
                StepFlow::Decompose(result) => {
                    return Err(NeedsHuman(format!(
                        "agent asked to decompose during self-review on issue #{}: {}",
                        run.issue_number, result.summary
                    ))
                    .into());
                }
            }
        }
        step = save_step(deps, &run, STEP_OPEN_PR, &checkpoint)?;
    }

    if step == STEP_OPEN_PR {
        // `deliver` dispatches on the project's `deliver` setting (issue #54):
        // a PR (open_pr), or just the verified branch. `finish_pane` applies
        // the issue-scoped pane lifetime (issue #92) either way.
        let artifact = deliver(deps, &run, &mut checkpoint, &worktree, flavor).await?;
        finish_pane(deps, &run).await;
        return Ok(WorkerOutcome::Succeeded { pr_url: artifact });
    }

    bail!("unknown step {step:?}");
}

pub(crate) enum StepFlow {
    Continue,
    Stopped,
    Interrupted(String),
    /// The agent's execute turn ended with `needs_plan` (+ the reason); the
    /// flavor's [`Flavor::on_needs_plan`] decides the terminal outcome.
    NeedsPlan(String),
    /// The agent's execute turn ended with `decompose` (the full result
    /// carries the proposed children); the flavor's
    /// [`Flavor::on_decompose`] decides the terminal outcome.
    Decompose(TurnResultFile),
}

/// Apply the keep_pane policy when a run succeeds. The default
/// ("until-issue-closed") keeps the pane for the reaper, which reclaims it
/// once the issue closes on the forge; "never" releases it right away
/// (high-throughput operation). Unknown values are rejected at config load.
/// `keep_pane` is a pane-mode-only setting (ADR 0012): a direct-mode lane has
/// no live pane row, so [`super::reaper::release_pane`] below is already a
/// no-op for it — no special-casing needed here.
pub(crate) async fn finish_pane(deps: &Deps, run: &RunRecord) {
    if deps.config.mux.keep_pane == "never" {
        super::reaper::release_pane(
            deps,
            run.issue_number,
            lane_for_loop(&run.loop_kind),
            "keep_pane = never",
        )
        .await;
    }
}

/// Spawn the collab advisor pane for a worker run, best-effort (issue #111,
/// ADR 0006 collab-advisor). A no-op unless `[collab] mode = "advisor"`, the
/// loop kind is advisor-eligible (worker / spec-worker), and the target is a
/// github issue. A failure never fails the run — it is logged and emitted, and
/// the worker proceeds with no consult block (the prompt then matches
/// collab-off byte for byte, since [`advisor_consult_section`] reads the live
/// pane as its "spawn succeeded" signal).
///
/// The advisor is re-embodied, not resumed: any surviving advisor pane is torn
/// down and a fresh one spawned ("捨てて張り直す"), so a resume or restart never
/// adopts a stale individual. The same applies to its cwd — the bare directory
/// is deleted and recreated empty on every spawn, so files left by a crashed
/// advisor (or one whose pane alone was reaped) never leak into the fresh one.
pub(crate) async fn ensure_advisor(deps: &Deps, run: &RunRecord, worktree: &Path, cp: &Checkpoint) {
    if !crate::collab::run_gets_advisor(&deps.config, run) {
        return;
    }
    if let Err(e) = spawn_advisor(deps, run, worktree, cp).await {
        tracing::warn!(
            "collab advisor spawn failed for issue #{}: {e:#} — worker proceeds without it",
            run.issue_number
        );
        let _ = deps.store.emit(
            Some(&run.id),
            "collab.advisor_spawn_failed",
            json!({ "issue": run.issue_number, "error": format!("{e:#}") }),
        );
    }
}

async fn spawn_advisor(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    cp: &Checkpoint,
) -> Result<()> {
    let issue = run.issue_number;
    let (profile_name, profile) = resolve_advisor_profile(deps, run)?;

    // Seed material: the merged spec if present, else the issue body. meguri
    // reads it (the advisor needs no repo access).
    let spec_rel = super::planner::spec_rel_path(issue);
    let material = std::fs::read_to_string(worktree.join(&spec_rel))
        .unwrap_or_else(|_| format!("# {}\n\n{}", cp.issue_title, cp.issue_body));
    let team = crate::collab::team_name(&deps.project.id, issue);
    let seed = crate::collab::advisor_seed_prompt(issue, &team, &material);

    // A bare, git-unregistered empty directory: read-only by wiring (#121) —
    // there is no repo copy to write into.
    let root = deps
        .project
        .worktree_root
        .clone()
        .unwrap_or_else(crate::config::worktrees_root);
    let advisor_dir = root.join(&deps.project.id).join(format!("advisor-{issue}"));
    // Re-embody, never reuse (ADR 0006 「捨てて張り直す」): a crashed advisor or
    // a reaper orphan sweep (which kills only the pane) can leave files behind,
    // and the fresh individual must not see a stale one's leftovers. Tear the
    // directory down and recreate it empty.
    match std::fs::remove_dir_all(&advisor_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!("cannot clear stale advisor dir {}", advisor_dir.display())
            });
        }
    }
    std::fs::create_dir_all(&advisor_dir)
        .with_context(|| format!("cannot create advisor dir {}", advisor_dir.display()))?;

    // From here on the cwd exists but no LANE_ADVISOR row does yet, and
    // `release_advisor` returns early without one — so a failure below would
    // strand the directory forever. Sweep it here to keep the advisor
    // ephemeral even when its best-effort spawn fails.
    let spawned = spawn_advisor_pane(
        deps,
        run,
        &advisor_dir,
        &profile_name,
        &profile,
        seed,
        &team,
    )
    .await
    .with_context(|| format!("cannot spawn advisor pane for issue #{issue}"));
    if spawned.is_err() {
        let _ = std::fs::remove_dir_all(&advisor_dir);
    }
    spawned
}

/// The fallible tail of [`spawn_advisor`], split out so the caller can sweep
/// the already-created advisor cwd on any failure.
async fn spawn_advisor_pane(
    deps: &Deps,
    run: &RunRecord,
    advisor_dir: &Path,
    profile_name: &str,
    profile: &crate::config::AgentProfile,
    seed: String,
    team: &str,
) -> Result<()> {
    let issue = run.issue_number;

    // Re-embody, never resume: tear down any surviving advisor pane first
    // (release_pane_record guards the advisor lane against saving a session id).
    super::reaper::release_pane(deps, issue, crate::store::LANE_ADVISOR, "advisor respawn").await;

    deps.mux.ensure_session().await?;

    let mut command = vec![profile.command.clone()];
    command.extend(profile.args.iter().cloned());
    command.push(seed);

    // The advisor pane also launches an interactive CLI in a fresh directory,
    // so it hits the same first-run folder-trust gate (issue #235 f9). Prime it
    // with the same machinery, in the advisor cwd, and pin the config-dir so
    // prime and pane agree (f1).
    let config_dir = crate::config::effective_config_dir();
    preflight_and_emit(deps, run, profile, profile_name, advisor_dir, &config_dir).await;

    let mut env = vec![(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    )];
    if let Some(hint) = &profile.herdr_agent_hint {
        env.push(("HERDR_AGENT".to_string(), hint.clone()));
    }
    let pane = deps
        .mux
        .spawn_pane(&PaneSpec {
            title: format!("meguri#{issue}:advisor"),
            cwd: advisor_dir.to_path_buf(),
            command,
            env,
        })
        .await?;
    // The mux can hand back a pane id even when the agent command died right
    // away (tmux/herdr report the pane, not the process). Registering the row
    // below is what makes `advisor_consult_section` advertise the advisor to
    // the worker, so verify liveness first: a dead pane is a failed spawn.
    if !deps.mux.pane_alive(&pane).await.unwrap_or(false) {
        let _ = deps.mux.kill_pane(&pane).await;
        bail!("advisor pane {} died immediately after spawn", pane.0);
    }
    // Register on the advisor lane only. Unlike spawn_agent_pane we do NOT
    // update_run_mux — the run's own pane is the worker's, not this one.
    //
    // Failures past this point must not leak the live pane: without the
    // LANE_ADVISOR row neither release_advisor nor the reaper can see it, so
    // erring out would strand an untracked agent forever. Kill the pane before
    // the row exists; once it does, funnel through release_pane so the row is
    // reclaimed together with the pane.
    if let Err(e) = deps.store.upsert_pane(
        &deps.project.id,
        issue,
        crate::store::LANE_ADVISOR,
        deps.mux.kind().as_str(),
        &deps.config.mux.session,
        &pane.0,
        &advisor_dir.to_string_lossy(),
    ) {
        let _ = deps.mux.kill_pane(&pane).await;
        return Err(e).with_context(|| format!("cannot register advisor pane {}", pane.0));
    }
    if let Err(e) = deps.store.emit(
        Some(&run.id),
        "collab.advisor_spawned",
        json!({ "issue": issue, "pane": pane.0, "profile": profile_name,
                "team": team, "mux": deps.mux.kind().as_str() }),
    ) {
        super::reaper::release_pane(
            deps,
            issue,
            crate::store::LANE_ADVISOR,
            "advisor spawn failed after registration",
        )
        .await;
        return Err(e).context("cannot emit collab.advisor_spawned");
    }
    Ok(())
}

/// Resolve the advisor's launch profile (issue #111): inherit the profile the
/// plan author actually ran under — the pin on the issue's latest succeeded
/// run of the advisor role (default `planner`). Fall back to a fresh
/// resolution of the advisor role when there is no such pin or it has vanished
/// from config. Best-effort: unlike the run's own `resolve_run_profile`, a
/// vanished pin is not a loud error here (advisor spawn is best-effort).
fn resolve_advisor_profile(
    deps: &Deps,
    run: &RunRecord,
) -> Result<(String, crate::config::AgentProfile)> {
    let role = crate::collab::advisor_role(&deps.config);
    if let Some(name) =
        deps.store
            .latest_succeeded_agent_profile(&deps.project.id, role, run.issue_number)?
    {
        match crate::routing::profile_by_name(&deps.config, &name) {
            Ok(profile) => return Ok((name, profile)),
            Err(_) => {
                let _ = deps.store.emit(
                    Some(&run.id),
                    "collab.advisor_profile_fallback",
                    json!({ "issue": run.issue_number, "vanished_profile": name, "role": role }),
                );
            }
        }
    }
    let name = crate::routing::resolve(&deps.config, role, &crate::routing::detect_command)?;
    let profile = crate::routing::profile_by_name(&deps.config, &name)?;
    Ok((name, profile))
}

/// Reap the advisor pane at the end of a worker run (issue #111): on every
/// terminal path, independent of `keep_pane` (ADR 0006 principle 3). No-op when
/// collab is off, the loop is not advisor-eligible, or no advisor pane exists.
/// The session id is not saved on release (guarded in `release_pane_record`),
/// and the bare advisor dir is removed.
pub(crate) async fn release_advisor(deps: &Deps, run: &RunRecord) {
    if !crate::collab::run_gets_advisor(&deps.config, run) {
        return;
    }
    let Some(record) = deps
        .store
        .get_pane(
            &deps.project.id,
            run.issue_number,
            crate::store::LANE_ADVISOR,
        )
        .ok()
        .flatten()
    else {
        return;
    };
    super::reaper::release_pane(
        deps,
        run.issue_number,
        crate::store::LANE_ADVISOR,
        "worker run ended",
    )
    .await;
    if let Some(dir) = record.worktree_path {
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The advisor consult block for the worker's execute prompt (issue #111 §4),
/// or empty when it must not appear: collab off, loop not advisor-eligible, or
/// the advisor pane is not live (spawn failed). Reading the live pane is the
/// "spawn succeeded" signal, so a failed spawn yields the same bytes as
/// collab-off — never advertising an absent advisor.
pub(crate) fn advisor_consult_section(deps: &Deps, run: &RunRecord) -> String {
    if !crate::collab::run_gets_advisor(&deps.config, run) {
        return String::new();
    }
    let live = deps
        .store
        .get_pane(
            &deps.project.id,
            run.issue_number,
            crate::store::LANE_ADVISOR,
        )
        .ok()
        .flatten()
        .and_then(|p| p.mux_pane_id)
        .is_some();
    if !live {
        return String::new();
    }
    let team = crate::collab::team_name(&deps.project.id, run.issue_number);
    crate::collab::worker_consult_block(&team)
}

/// The `Notification::reason` (and `review.awaiting_human` marker) for a
/// parked review — a finished review run waiting on a human. ADR 0009.
pub(crate) const REASON_REVIEW_PARKED: &str = "spec_review_parked";

/// Park a finished review run on a human (ADR 0009 / issue #153). The run has
/// ended (or is about to end) `Succeeded`, but the PR now waits on a human —
/// plan findings the author must fix, or a clean spec PR the human must merge.
/// Raise an *active* signal off the conversation timeline:
///
/// 1. set the run's `interaction_state` to `AwaitingHuman` (survives the
///    `Succeeded` status — `update_run_status` never clears it), so the
///    dashboard's parked-review query surfaces it;
/// 2. emit `review.awaiting_human` — the durable proof the park ran, and what
///    the dashboard query keys off (not the state alone);
/// 3. page a human via the throttled notifier, pointing at the PR (not a
///    pane: the review pane is released as the run finishes).
///
/// Kind- and verdict-agnostic on purpose: both pr-reviewer(Plan) branches and
/// a future spec_fixer round-limit escalation call this one seam. Notify is
/// best-effort — a delivery failure never fails the run.
pub(crate) async fn signal_review_parked(
    deps: &Deps,
    run: &RunRecord,
    pr_number: i64,
    pr_url: &str,
    verdict: &str,
    head_sha: &str,
) {
    if let Err(e) = deps
        .store
        .update_interaction_state(&run.id, Some(InteractionState::AwaitingHuman))
    {
        tracing::warn!("cannot park run {} on a human: {e:#}", run.id);
    }
    let _ = deps.store.emit(
        Some(&run.id),
        "review.awaiting_human",
        json!({ "pr": pr_number, "url": pr_url, "verdict": verdict, "head": head_sha }),
    );
    deps.notifier
        .notify(&Notification::awaiting_human(
            run.id.clone(),
            run.issue_number,
            run.issue_title.clone(),
            REASON_REVIEW_PARKED,
            None,
            Some(pr_url.to_string()),
        ))
        .await;
}

/// The PR this run claimed, from its persisted checkpoint (release/escalate
/// hooks get the run record as of drive start, so re-read the store). None
/// before prepare-work filled it — escalation then falls back to the
/// canonical issue.
pub(crate) fn claimed_pr(deps: &Deps, run_id: &str) -> Option<i64> {
    let run = deps.store.get_run(run_id).ok().flatten()?;
    serde_json::from_str::<Checkpoint>(&run.checkpoint_json)
        .ok()
        .and_then(|cp| cp.pr_number)
}

/// `meguri stop`: cancel the run, release the claim, release the pane (its
/// session id is saved first, so the context stays resumable).
async fn finalize_cancelled(deps: &Deps, run: &RunRecord, flavor: &dyn Flavor) -> Result<()> {
    deps.store
        .update_run_status(&run.id, RunStatus::Cancelled, None)?;
    flavor.release_claim(deps, run).await;
    super::reaper::release_pane(
        deps,
        run.issue_number,
        lane_for_loop(&run.loop_kind),
        "stopped by user",
    )
    .await;
    deps.store.emit(Some(&run.id), "run.cancelled", json!({}))?;
    Ok(())
}

/// The escalation comment's closing "how to look at this" sentence (issue
/// #169): pane attach for a pane-mode lane (unchanged wording), or — for a
/// direct-mode lane — a `claude --resume <session-id>` hint built from the
/// lane's own saved session, falling back to a plain "no pane, no session
/// yet" note when none was ever recorded.
pub(crate) fn attach_hint(deps: &Deps, run: &RunRecord) -> String {
    let lane = lane_for_loop(&run.loop_kind);
    let routing_role = routing::routing_role_for_loop(&run.loop_kind);
    match launch::resolve(&deps.config, routing_role) {
        LaunchMode::Pane => tasks::DEFAULT_ATTACH_HINT.to_string(),
        LaunchMode::Direct => {
            let session = deps
                .store
                .get_pane(&deps.project.id, run.issue_number, lane)
                .ok()
                .flatten()
                .and_then(|p| p.agent_session_id);
            match session {
                Some(id) => format!(
                    "This role runs headless (no pane to attach to) — resume its context \
                     with `claude --resume {id}` in the run's worktree, or see `meguri ps`."
                ),
                None => "This role runs headless (no pane to attach to) and no resumable \
                     session was recorded yet — see `meguri ps`."
                    .to_string(),
            }
        }
    }
}

/// Failure escalation on the forge ("Authority": the durable record of why
/// the run stopped lives on the issue, not in meguri's local state). Used by
/// forge loops that escalate on the issue directly (the spec worker); the
/// worker/planner default escalate goes through the task source instead.
/// Thin alias for [`super::escalation::escalate_issue`] — the central helper
/// (issue #176) — kept so the many forge-loop call sites read unchanged. The
/// helper posts the generic pane-attach hint; its callers (spec worker, and the
/// fixer family's before-PR-claimed fallback) all default to `pane` launch mode
/// (issue #169, ADR 0012's recommendation table).
pub(crate) async fn escalate_on_forge(deps: &Deps, issue: i64, reason: &str) {
    super::escalation::escalate_issue(deps, issue, reason).await;
}

fn save_step(deps: &Deps, run: &RunRecord, step: &str, cp: &Checkpoint) -> Result<String> {
    deps.store
        .update_run_step(&run.id, step, &serde_json::to_string(cp)?)?;
    Ok(step.to_string())
}

/// What prepare-work decided: the target was claimed (checkpoint filled),
/// or the run should end quietly because it is no longer actionable.
pub enum PreparedWork {
    Claimed,
    Skip(String),
}

/// Default prepare-work: the coordination layer's single atomic claim
/// (github: label re-verification + `meguri:working` + needs-human clear;
/// local: the atomic `tasks` UPDATE). `None` is a benign race — the task
/// changed between discovery and claim (e.g. another run shipped it, or a
/// second host took it) — so skip, don't escalate.
async fn claim_task(deps: &Deps, run: &RunRecord, cp: &mut Checkpoint) -> Result<PreparedWork> {
    let key = run.task_key();
    // Pass the bucket the run was stamped with at creation so the label source
    // can reject a claim whose issue no longer belongs to that cadence bucket
    // (issue #148) — a benign race, like a de-labeled trigger.
    match deps
        .task_source
        .claim(&key, tasks::LOCAL_HOST, run.cadence_label.as_deref())
        .await?
    {
        Some(task) => {
            // Carry the auto-merge opt-in from the issue to the PR (auto-merge
            // 1/3, #41): recorded now, applied in open-pr (non-draft + label
            // copy). The coordination layer decides it (github: the
            // `meguri:automerge` label; local: always off).
            cp.automerge = task.automerge;
            cp.issue_title = task.title;
            cp.issue_body = task.body;
            deps.store.emit(
                Some(&run.id),
                "issue.claimed",
                json!({ "key": format!("{key:?}") }),
            )?;
            Ok(PreparedWork::Claimed)
        }
        None => Ok(PreparedWork::Skip(format!(
            "task {key:?} is no longer claimable (raced, de-labeled, or already taken)"
        ))),
    }
}

/// Default prepare-worktree: a new run-scoped branch off the default branch.
/// The branch name encodes the target so a resume (and Phase 4's cross-host
/// re-claim) can find it: `meguri/<issue>-…` for github, `meguri/t<id>-…`
/// for local tasks.
async fn create_branch_worktree(deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
    let branch = run.branch.clone().unwrap_or_else(|| match run.task_key() {
        TaskKey::Issue(n) => gitops::branch_name(n, &cp.issue_title, &run.id),
        TaskKey::Local(id) => gitops::task_branch_name(id, &cp.issue_title, &run.id),
    });
    let root = deps
        .project
        .worktree_root
        .clone()
        .unwrap_or_else(crate::config::worktrees_root);
    let wt = gitops::worktree_path(&root, &deps.project.id, &branch);
    gitops::create_worktree(
        &deps.repo_path(),
        &wt,
        &branch,
        &deps.project.default_branch,
        &deps.project.worktree_setup.exclude,
    )
    .await?;
    deps.store
        .update_run_worktree(&run.id, &branch, &wt.to_string_lossy())?;
    deps.store.emit(
        Some(&run.id),
        "worktree.created",
        json!({ "branch": branch, "path": wt.to_string_lossy() }),
    )?;
    run_worktree_setup(deps, run, &wt).await
}

/// Attach the run's worktree to an existing PR head branch instead of
/// cutting a new one (branch-takeover loops: fixer, spec worker).
pub(crate) async fn attach_pr_worktree(
    deps: &Deps,
    run: &RunRecord,
    cp: &Checkpoint,
) -> Result<()> {
    let branch = run
        .branch
        .clone()
        .or_else(|| cp.head_branch.clone())
        .context("checkpoint has no PR head branch")?;
    let root = deps
        .project
        .worktree_root
        .clone()
        .unwrap_or_else(crate::config::worktrees_root);
    let wt = gitops::worktree_path(&root, &deps.project.id, &branch);
    gitops::attach_worktree(
        &deps.repo_path(),
        &wt,
        &branch,
        &deps.project.worktree_setup.exclude,
    )
    .await?;
    deps.store
        .update_run_worktree(&run.id, &branch, &wt.to_string_lossy())?;
    deps.store.emit(
        Some(&run.id),
        "worktree.attached",
        json!({ "branch": branch, "path": wt.to_string_lossy() }),
    )?;
    run_worktree_setup(deps, run, &wt).await
}

/// Env vars passed to `worktree_setup` commands: the run's role (its loop
/// kind — `worker`, `fixer`, `spec-reviewer`, …), the launch profile that
/// role resolves to, and the target issue/task number. Lets a user script
/// specialize per role (e.g. skip a heavy step for `cleaner`). `MEGURI_PROFILE`
/// goes through [`resolve_run_profile`] — the same pin-aware resolution the
/// run's pane spawn uses — rather than re-resolving routing from scratch, so
/// it can't drift from the profile the agent actually launches under (e.g.
/// if routing config or CLI detection changes between the two).
fn worktree_setup_env(deps: &Deps, run: &RunRecord) -> Result<[(&'static str, String); 3]> {
    let (profile_name, _) = resolve_run_profile(deps, run)?;
    Ok([
        ("MEGURI_ROLE", run.loop_kind.clone()),
        ("MEGURI_PROFILE", profile_name),
        ("MEGURI_ISSUE", run.task_key().number().to_string()),
    ])
}

/// Generic post-worktree-preparation hook (`[projects.worktree_setup]`,
/// agent 指示基盤 2/3 — issue #138): runs the project's configured commands
/// with the worktree as cwd. Called on every worktree preparation, not just
/// the first — `attach_worktree` / `create_review_worktree` can wipe
/// untracked files via `reset --hard` + `clean -fd` on reuse, so generated
/// artifacts (e.g. `apm install --frozen` output) must be regenerated each
/// time. Commands are expected to be idempotent, and a failing command stops
/// the remaining ones in the list. Failure is a warning by default (the run
/// continues); `worktree_setup.required = true` escalates it to a run
/// failure.
pub(crate) async fn run_worktree_setup(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
) -> Result<()> {
    let setup = &deps.project.worktree_setup;
    if setup.commands.is_empty() {
        return Ok(());
    }
    let env = worktree_setup_env(deps, run)?;
    let timeout = Duration::from_secs(setup.timeout_secs);

    for command in &setup.commands {
        deps.store.emit(
            Some(&run.id),
            "worktree_setup.running",
            json!({ "command": command }),
        )?;

        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command).current_dir(worktree);
        for (key, value) in &env {
            cmd.env(key, value);
        }
        // `output()` spawns eagerly; wrapping it in `timeout` only drops the
        // *future* on expiry, which would otherwise leave the shell (and
        // whatever it started) running as an orphan. `kill_on_drop` makes
        // dropping the child on that cancellation actually kill it.
        cmd.kill_on_drop(true);

        let failure = match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(out)) if out.status.success() => None,
            Ok(Ok(out)) => Some(format!(
                "`{command}` exited {}: {}",
                out.status
                    .code()
                    .map_or_else(|| "signal".to_string(), |c| c.to_string()),
                String::from_utf8_lossy(&out.stderr).trim()
            )),
            Ok(Err(e)) => Some(format!("spawning `{command}`: {e}")),
            Err(_) => Some(format!(
                "`{command}` timed out after {}s",
                setup.timeout_secs
            )),
        };

        let Some(reason) = failure else {
            deps.store.emit(
                Some(&run.id),
                "worktree_setup.ok",
                json!({ "command": command }),
            )?;
            continue;
        };

        deps.store.emit(
            Some(&run.id),
            "worktree_setup.failed",
            json!({ "command": command, "reason": reason }),
        )?;
        if setup.required {
            bail!("worktree_setup command {reason}");
        }
        tracing::warn!(
            "worktree_setup command {reason} (continuing: worktree_setup.required = false)"
        );
        break;
    }
    Ok(())
}

fn turn_engine(deps: &Deps) -> TurnEngine {
    TurnEngine {
        mux: deps.mux.clone(),
        cfg: TurnConfig::from_limits(&deps.config.limits),
    }
}

/// How long a resume-spawned pane is watched for instant death: an unknown
/// or expired session id makes the agent CLI print an error and exit within
/// a few seconds, which is the signal to fall back to full re-injection.
const RESUME_PROBE: Duration = Duration::from_secs(5);
const RESUME_PROBE_INTERVAL: Duration = Duration::from_millis(500);

/// The pane `run_turn` operates on, and how it came to exist.
struct EnsuredPane {
    pane: PaneId,
    /// Trigger already delivered as the agent's initial prompt argument.
    freshly_spawned: bool,
    /// Spawned with the agent's native `--resume`; a pane death without a
    /// result then means the stored session id is no longer trustworthy.
    resumed: bool,
}

/// Get the lane's pane, spawning it (with the trigger as the agent's
/// initial prompt argument) if it doesn't exist or died. The pane is keyed
/// by `(project, issue, lane)` and outlives runs (issue #92): every
/// branch-editing loop of the issue (planner, worker, fixer, …) shares the
/// author lane's live session, while the pr-reviewer keeps its own pr-review
/// lane. When the lane has a native agent session id on record, a fresh
/// spawn resumes it (`claude --resume <id> <trigger>`) so the agent keeps
/// its conversation context; a resume that dies on the spot falls back to
/// the plain full-prompt spawn.
async fn ensure_pane(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    lane: &Lane,
    initial_trigger: &str,
) -> Result<EnsuredPane> {
    let lane_name = lane.lane.as_str();
    let worktree_str = worktree.to_string_lossy();
    if let Some(record) = deps
        .store
        .get_pane(&deps.project.id, run.issue_number, lane_name)?
        && let Some(id) = &record.mux_pane_id
    {
        let pane = PaneId(id.clone());
        if record.mux_kind.as_deref() == Some(deps.mux.kind().as_str())
            && deps.mux.pane_alive(&pane).await.unwrap_or(false)
        {
            if record.worktree_path.as_deref() == Some(worktree_str.as_ref()) {
                // Adopt the lane's live pane for this run.
                deps.store.update_run_mux(
                    &run.id,
                    deps.mux.kind().as_str(),
                    &deps.config.mux.session,
                    &pane.0,
                )?;
                return Ok(EnsuredPane {
                    pane,
                    freshly_spawned: false,
                    resumed: false,
                });
            }
            // The lane moved to another worktree (e.g. a fresh branch): the
            // old pane can't see it. Retire it — session id saved — and
            // respawn below (the saved id makes the respawn a resume, so the
            // context follows the lane into the new worktree).
            super::reaper::release_pane(deps, run.issue_number, lane_name, "worktree moved").await;
        }
    }

    deps.mux.ensure_session().await?;

    // The lane's resumable context lives on the pane row (issue lifetime),
    // not the ephemeral run: written after every completed turn and before
    // every reclamation.
    let session_id = deps
        .store
        .get_pane(&deps.project.id, run.issue_number, lane_name)?
        .and_then(|p| p.agent_session_id);
    if let Some(session_id) = session_id {
        let resumed = match spawn_agent_pane(
            deps,
            run,
            worktree,
            lane,
            initial_trigger,
            Some(&session_id),
        )
        .await
        {
            Ok(pane) => {
                if resumed_pane_survives(deps, &pane).await {
                    Some(pane)
                } else {
                    // Dead on arrival: the CLI rejected the session id.
                    let _ = deps.mux.kill_pane(&pane).await;
                    None
                }
            }
            Err(_) => None,
        };
        if let Some(pane) = resumed {
            return Ok(EnsuredPane {
                pane,
                freshly_spawned: true,
                resumed: true,
            });
        }
        // Forget the id and fall back to full re-injection.
        deps.store
            .save_pane_session(&deps.project.id, run.issue_number, lane_name, None)?;
        deps.store.update_run_agent_session(&run.id, None)?;
        deps.store.emit(
            Some(&run.id),
            "pane.resume_failed",
            json!({ "agent_session_id": session_id }),
        )?;
    }

    let pane = spawn_agent_pane(deps, run, worktree, lane, initial_trigger, None).await?;
    Ok(EnsuredPane {
        pane,
        freshly_spawned: true,
        resumed: false,
    })
}

/// Run the launch-time pre-flight prime for a pane and emit its outcome as an
/// event (issue #235). Best-effort: a failure or skip is recorded but never
/// fatal — the pane launches regardless (spec D5). `cwd` is the directory whose
/// folder trust the prime establishes (a worktree, or an advisor dir).
async fn preflight_and_emit(
    deps: &Deps,
    run: &RunRecord,
    profile: &crate::config::AgentProfile,
    profile_name: &str,
    cwd: &Path,
    config_dir: &Path,
) {
    use crate::preflight::PreflightOutcome;
    let outcome = crate::preflight::ensure_preflight(profile, cwd, config_dir).await;
    let (kind, data) = match &outcome {
        PreflightOutcome::Ran { duration_ms } => (
            "preflight.ran",
            json!({ "profile": profile_name, "command": profile.command,
                    "cwd": cwd.to_string_lossy(), "duration_ms": duration_ms }),
        ),
        PreflightOutcome::Failed {
            reason,
            duration_ms,
        } => (
            "preflight.failed",
            json!({ "profile": profile_name, "command": profile.command,
                    "reason": reason, "duration_ms": duration_ms }),
        ),
        PreflightOutcome::Skipped { reason } => (
            "preflight.skipped",
            json!({ "profile": profile_name, "command": profile.command, "reason": reason }),
        ),
        // Already primed once for this (identity, path): nothing to report.
        PreflightOutcome::AlreadyDone => return,
    };
    let _ = deps.store.emit(Some(&run.id), kind, data);
}

/// Spawn the agent pane (optionally resuming a native session) and persist
/// its handle on the run.
async fn spawn_agent_pane(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    lane: &Lane,
    initial_trigger: &str,
    resume_session: Option<&str>,
) -> Result<PaneId> {
    let lane_name = lane.lane.as_str();
    let profile_name = &lane.profile_name;
    let profile = &lane.profile;
    let worktree_str = worktree.to_string_lossy();

    let mut command = vec![profile.command.clone()];
    command.extend(profile.args.iter().cloned());
    if let Some(session_id) = resume_session {
        command.extend(profile.resume_args.iter().cloned());
        command.push(session_id.to_string());
    }
    command.push(initial_trigger.to_string());

    // Resolve the config-dir once, absolute, and hand the exact same value to
    // both the pre-flight prime and the pane (issue #235 f1): tmux/herdr spawn
    // the pane through a long-lived server whose environment can differ from
    // this daemon's, so an unset/relative `CLAUDE_CONFIG_DIR` would let the
    // prime write folder trust somewhere the pane never reads.
    let config_dir = crate::config::effective_config_dir();

    // Pre-flight: prime the fresh worktree's folder trust before the
    // interactive pane hits the first-run trust prompt (issue #235). Runs at
    // most once per (identity, path); a failure or skip never kills the pane.
    preflight_and_emit(deps, run, profile, profile_name, worktree, &config_dir).await;

    let mut env = vec![(
        "CLAUDE_CONFIG_DIR".to_string(),
        config_dir.to_string_lossy().into_owned(),
    )];
    if let Some(hint) = &profile.herdr_agent_hint {
        env.push(("HERDR_AGENT".to_string(), hint.clone()));
    }

    let title = if lane_name == LANE_AUTHOR {
        format!("meguri#{}", run.issue_number)
    } else {
        format!("meguri#{}:{lane_name}", run.issue_number)
    };
    let pane = deps
        .mux
        .spawn_pane(&PaneSpec {
            title,
            cwd: worktree.to_path_buf(),
            command,
            env,
        })
        .await?;
    deps.store.update_run_mux(
        &run.id,
        deps.mux.kind().as_str(),
        &deps.config.mux.session,
        &pane.0,
    )?;
    deps.store.upsert_pane(
        &deps.project.id,
        run.issue_number,
        lane_name,
        deps.mux.kind().as_str(),
        &deps.config.mux.session,
        &pane.0,
        &worktree_str,
    )?;
    deps.store.emit(
        Some(&run.id),
        "pane.spawned",
        json!({ "pane": pane.0, "mux": deps.mux.kind().as_str(),
                "resumed": resume_session.is_some(),
                "profile": profile_name,
                "attach": deps.mux.attach_command(&pane) }),
    )?;
    Ok(pane)
}

/// Resolve the launch profile for a run, pinning it on first spawn.
///
/// Role→profile resolution runs once, lazily, at the first pane spawn: the
/// result is persisted to `runs.agent_profile` and every later spawn/resume
/// of this run reuses it (auto detection is not re-run). If a run created
/// before migration 0004 has no pin, or a fresh run reaches here first, we
/// resolve from `runs.loop_kind` now. A pinned name that has since vanished
/// from config is a loud error — never a silent fall back to `default`.
fn resolve_run_profile(
    deps: &Deps,
    run: &RunRecord,
) -> Result<(String, crate::config::AgentProfile)> {
    // Re-read: a concurrent spawn (or an earlier turn) may have pinned the
    // profile on the store even though the caller's snapshot predates it.
    let pinned = deps
        .store
        .get_run(&run.id)?
        .and_then(|r| r.agent_profile)
        .filter(|s| !s.is_empty());
    let name = match pinned {
        Some(name) => name,
        None => {
            let role = crate::routing::routing_role_for_loop(&run.loop_kind);
            let mainline =
                crate::routing::resolve(&deps.config, role, &crate::routing::detect_command)?;
            // Explore canary (routing 3/3, issue #66): opt-in, deterministic,
            // routing-active only (a legacy config has no `[routing]`, so the
            // ratio reads 0.0 and this never diverts). A selected run is pinned
            // to the recommendation chain's next candidate instead of the
            // mainline pick, and marked `explore` so stats keep it separate.
            let ratio = deps
                .config
                .routing
                .as_ref()
                .map(|r| r.explore_ratio)
                .unwrap_or(0.0);
            let name = if ratio > 0.0
                && crate::routing::is_explore(run.task_key().number(), ratio)
                && let Some(alt) = crate::routing::explore_alternative(
                    &deps.config,
                    role,
                    &crate::routing::detect_command,
                )
                && alt != mainline
            {
                deps.store
                    .update_run_routing_arm(&run.id, Some("explore"))?;
                deps.store.emit(
                    Some(&run.id),
                    "run.explore_assigned",
                    json!({ "profile": alt, "alt_of": mainline }),
                )?;
                alt
            } else {
                mainline
            };
            deps.store.update_run_agent_profile(&run.id, &name)?;
            deps.store.emit(
                Some(&run.id),
                "run.profile_resolved",
                json!({ "profile": name, "role": run.loop_kind }),
            )?;
            name
        }
    };
    let profile = crate::routing::profile_by_name(&deps.config, &name).with_context(|| {
        format!(
            "run {} is pinned to agent profile {name:?}, which is no longer in config",
            run.id
        )
    })?;
    Ok((name, profile))
}

/// Signal-driven profile escalation (routing 3/3, issue #66). Called once the
/// current pinned profile has had a failing fix turn (`cp.pin_fix_turns >= 1`):
/// if routing is active, escalation is enabled, and the profile has a stronger
/// entry in its role's escalation chain, re-pin one step up, retire the live
/// author pane, and clear the session so the next turn spawns fresh (a model
/// change can't `--resume`). Returns whether it escalated, so the caller can
/// flag it in the fix prompt. A no-op (returns `false`) past the chain end — the
/// run then rides the existing `validate_turns` → needs-human backstop, so
/// escalation is finite.
///
/// Crash-safety: `pin_fix_turns` is reset to 0 and **persisted before** the pin
/// advances. So a crash between the reset and the pin write (or between the pin
/// write and the fix turn) resumes to `pin_fix_turns == 0`, which blocks a
/// second escalation until the new pin has actually run a fix turn — no chain
/// entry is ever skipped without a try.
async fn maybe_escalate(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    persist_step: &str,
) -> Result<bool> {
    // Common gate: escalation is a refinement of active routing and honors the
    // top-level kill switch. Both conditions fail in legacy → never escalate.
    if deps.config.routing.is_none() || !deps.config.escalation.enabled {
        return Ok(false);
    }
    let role = crate::routing::routing_role_for_loop(&run.loop_kind);
    let (current, _) = resolve_run_profile(deps, run)?;
    let Some(next) = crate::routing::next_escalation(
        &deps.config,
        role,
        &current,
        &crate::routing::detect_command,
    ) else {
        return Ok(false);
    };

    // Mark the new pin as "not yet tried" and persist it BEFORE advancing the
    // pin, so a crash in the window below can't let a resume escalate again off
    // a profile that never ran (which would skip a chain entry, issue #66).
    cp.escalation_level += 1;
    cp.pin_fix_turns = 0;
    save_step(deps, run, persist_step, cp)?;

    // Re-pin one step stronger; the next author-lane spawn reads this back.
    deps.store.update_run_agent_profile(&run.id, &next)?;

    // Retire the live author pane and forbid a resume: the model changed, so
    // `--resume <id>` under the new profile can't restore the old session.
    // `release_pane` saves the session id first (its default reversibility) —
    // clear it right after so the fresh spawn re-injects the full context
    // (validation history) instead of resuming into the old model.
    let lane = lane_for_loop(&run.loop_kind);
    super::reaper::release_pane(deps, run.issue_number, lane, "profile escalation").await;
    deps.store
        .save_pane_session(&deps.project.id, run.issue_number, lane, None)?;
    deps.store.update_run_agent_session(&run.id, None)?;

    // Arm bookkeeping: explore takes priority (explore > escalated > main), so
    // an explore run keeps its arm — the escalation still lands in the event.
    let already_explore = deps
        .store
        .get_run(&run.id)?
        .and_then(|r| r.routing_arm)
        .as_deref()
        == Some("explore");
    if !already_explore {
        deps.store
            .update_run_routing_arm(&run.id, Some("escalated"))?;
    }

    deps.store.emit(
        Some(&run.id),
        "run.escalated",
        json!({
            "from": current,
            "to": next,
            "level": cp.escalation_level,
            "reason": "validation failed",
        }),
    )?;
    Ok(true)
}

/// Watch a freshly resume-spawned pane briefly; false means it died (the
/// session id was rejected) and the caller should fall back.
async fn resumed_pane_survives(deps: &Deps, pane: &PaneId) -> bool {
    let mut waited = Duration::ZERO;
    loop {
        if !deps.mux.pane_alive(pane).await.unwrap_or(false) {
            return false;
        }
        if waited >= RESUME_PROBE {
            return true;
        }
        tokio::time::sleep(RESUME_PROBE_INTERVAL).await;
        waited += RESUME_PROBE_INTERVAL;
    }
}

/// Which lane a turn runs in, the launch profile it uses, and how it is
/// launched (issue #169). Threading this explicitly lets the worker's
/// self-review run its review turn in a separate lane under a different
/// profile — and potentially a different launch mode — than the fix turns
/// (ADR 0006). "Lane" now means "issue-scoped resumable context"; a pane is
/// optional (`mode == Direct` never has one).
pub(crate) struct Lane {
    /// The pane-registry lane key `(project, issue, lane)`. Owned (issue #214):
    /// most lanes are the historical `&'static str` constants, but a parallel
    /// round-1 reviewer uses a per-reviewer key like `self-review#0` so its
    /// pane/session never collides with a sibling reviewer's.
    lane: String,
    profile_name: String,
    profile: crate::config::AgentProfile,
    mode: LaunchMode,
}

/// The run's own lane: its loop's pane lane, under the profile pinned on the
/// run (resolved and pinned lazily on first spawn), and the launch mode its
/// routing role resolves to.
fn author_lane(deps: &Deps, run: &RunRecord) -> Result<Lane> {
    let lane = lane_for_loop(&run.loop_kind);
    let (profile_name, profile) = resolve_run_profile(deps, run)?;
    let mode = launch::resolve(&deps.config, routing::routing_role_for_loop(&run.loop_kind));
    Ok(Lane {
        lane: lane.to_string(),
        profile_name,
        profile,
        mode,
    })
}

/// The self-review lane: a separate lane keyed by the same issue, launched
/// under the `self-reviewer` routing profile (formerly `impl-reviewer` /
/// `self-review`, ADR 0003 revision) so the review turn can be a different
/// model than the author doing the fixes. Resolved without pinning the run's
/// own profile. Shared by the plan and impl self-review (the loop is
/// symmetric). Its launch mode resolves independently too — recommended
/// `direct` (ADR 0012): an internal loop no human ever attaches to.
fn self_review_lane(deps: &Deps) -> Result<Lane> {
    self_review_lane_for(deps, None, crate::store::LANE_SELF_REVIEW.to_string())
}

/// A self-review lane under a chosen profile and lane key (issue #214). A
/// parallel round-1 reviewer passes `profile_override = Some(name)` from its
/// `[[review.reviewers]].profile` and a distinct `lane` key (e.g.
/// `self-review#0`) so its pane/session never collides with a sibling's. The
/// **profile** comes from the reviewer config (falling back to the
/// `self-reviewer` routing profile when `None`), while the **launch mode** is
/// always resolved from the `self-reviewer` role — profile and mode are resolved
/// separately (spec §decision 10). `profile_override` naming an undefined
/// profile is an error the caller decides how to handle (drop-and-continue).
fn self_review_lane_for(deps: &Deps, profile_override: Option<&str>, lane: String) -> Result<Lane> {
    let profile_name = match profile_override {
        Some(name) => name.to_string(),
        None => crate::routing::resolve(
            &deps.config,
            "self-reviewer",
            &crate::routing::detect_command,
        )?,
    };
    let profile = crate::routing::profile_by_name(&deps.config, &profile_name)?;
    let mode = launch::resolve(&deps.config, "self-reviewer");
    Ok(Lane {
        lane,
        profile_name,
        profile,
        mode,
    })
}

/// Resolve this run's role preamble(s) into the standing-discipline block that
/// gets prepended to the turn prompt (issue #149, ADR 0012). Reads each
/// configured file from the worktree behind the containment gate
/// ([`crate::config::resolve_preamble_within`]): a missing path or one that
/// escapes the worktree (via a symlink) is skipped with a warning and a
/// `prompt.preamble_missing` event — never fatal, mirroring `worktree_setup`.
/// Returns an empty string when nothing is configured or everything was
/// skipped, which `write_prompt_file` renders identically to the old output.
fn resolve_preamble(deps: &Deps, run: &RunRecord, worktree: &Path, role: &str) -> Result<String> {
    let entries = deps.config.preambles_for(&deps.project, role);
    if entries.is_empty() {
        return Ok(String::new());
    }
    let mut blocks = Vec::new();
    let mut injected = Vec::new();
    for (key, rel) in entries {
        match crate::config::resolve_preamble_within(worktree, &rel) {
            crate::config::PreambleResolution::Content(text) => {
                blocks.push(text.trim_end().to_string());
                injected.push(json!({ "key": key, "path": rel }));
            }
            crate::config::PreambleResolution::Missing => {
                tracing::warn!("preamble for role {role} ({key}) not found at {rel} — skipping");
                deps.store.emit(
                    Some(&run.id),
                    "prompt.preamble_missing",
                    json!({ "role": role, "key": key, "path": rel, "reason": "missing" }),
                )?;
            }
            crate::config::PreambleResolution::Escapes => {
                tracing::warn!(
                    "preamble for role {role} ({key}) at {rel} escapes the worktree — skipping"
                );
                deps.store.emit(
                    Some(&run.id),
                    "prompt.preamble_missing",
                    json!({ "role": role, "key": key, "path": rel, "reason": "escapes_worktree" }),
                )?;
            }
        }
    }
    if blocks.is_empty() {
        return Ok(String::new());
    }
    deps.store.emit(
        Some(&run.id),
        "prompt.preamble_injected",
        json!({ "role": role, "preambles": injected }),
    )?;
    Ok(format!(
        "## プロジェクトの恒常規律(以下に従うこと。ただし meguri の完了契約・検証ルールが優先)\n\n{}",
        blocks.join("\n\n")
    ))
}

/// Run one prompt-turn in the run's own (author) lane: prepare files, deliver
/// the trigger (spawn or send_line), then wait it out.
pub(crate) async fn run_turn(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    purpose: &str,
    prompt_body: &str,
) -> Result<(TurnOutcome, String)> {
    let lane = author_lane(deps, run)?;
    let role = crate::routing::routing_role_for_loop(&run.loop_kind);
    run_turn_in(
        deps,
        run,
        worktree,
        &lane,
        role,
        purpose,
        prompt_body,
        false,
    )
    .await
}

/// Run one prompt-turn in the worker's self-review lane under the
/// `self-reviewer` profile.
pub(crate) async fn run_review_turn(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    purpose: &str,
    prompt_body: &str,
) -> Result<(TurnOutcome, String)> {
    let lane = self_review_lane(deps)?;
    run_turn_in(
        deps,
        run,
        worktree,
        &lane,
        "self-reviewer",
        purpose,
        prompt_body,
        false,
    )
    .await
}

/// Run one parallel round-1 review turn (issue #214) under a specific reviewer
/// `profile` and `lane` key, with an isolated per-turn result file so N of these
/// can run concurrently without racing on `result.json`. The profile comes from
/// the reviewer config; the launch mode is the `self-reviewer` role's (spec
/// §decision 10). An undefined profile surfaces as an `Err` for the caller to
/// drop-and-continue.
pub(crate) async fn run_parallel_review_turn(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    profile_override: Option<&str>,
    lane_key: String,
    purpose: &str,
    prompt_body: &str,
) -> Result<(TurnOutcome, String)> {
    let lane = self_review_lane_for(deps, profile_override, lane_key)?;
    run_turn_in(
        deps,
        run,
        worktree,
        &lane,
        "self-reviewer",
        purpose,
        prompt_body,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_in(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    lane: &Lane,
    role: &str,
    purpose: &str,
    prompt_body: &str,
    isolated: bool,
) -> Result<(TurnOutcome, String)> {
    let preamble = resolve_preamble(deps, run, worktree, role)?;
    let prepared = if isolated {
        prepare_turn_isolated(worktree, prompt_body, &preamble)?
    } else {
        prepare_turn(worktree, prompt_body, &preamble)?
    };
    deps.store.begin_turn(
        &run.id,
        &prepared.turn_id,
        purpose,
        &prepared.prompt_path.to_string_lossy(),
    )?;

    let control = StoreControl {
        store: deps.store.clone(),
        run_id: run.id.clone(),
        notifier: deps.notifier.clone(),
    };
    let engine = turn_engine(deps);

    let (outcome, pane, resumed) = match lane.mode {
        LaunchMode::Pane => {
            let ensured = ensure_pane(deps, run, worktree, lane, &prepared.trigger_line).await?;
            let pane = ensured.pane.clone();
            if !ensured.freshly_spawned {
                deps.mux.send_line(&pane, &prepared.trigger_line).await?;
            }
            let outcome = engine
                .await_completion(
                    &pane,
                    worktree,
                    &prepared.turn_id,
                    prepared.isolated,
                    &control,
                )
                .await?;
            (outcome, Some(pane), ensured.resumed)
        }
        LaunchMode::Direct => {
            // A lane that ran in pane mode before its role was switched to
            // `direct` may still have a live pane. ADR 0012's invariant is
            // that a direct lane has no live pane, so release it through the
            // shared reaper path (session save + kill + mark_pane_reclaimed)
            // rather than merely clearing the row's mux columns — a cleared
            // row would orphan the still-running pane process, invisible to
            // the reaper's sweeps. Releasing first also refreshes the saved
            // session id, which the resume lookup below then picks up. No-op
            // for a lane with no live pane (the steady direct-mode state).
            super::reaper::release_pane(
                deps,
                run.issue_number,
                lane.lane.as_str(),
                "lane switched to direct launch mode",
            )
            .await;
            let (child, resumed) = spawn_direct_process(
                deps,
                run,
                worktree,
                lane,
                &prepared.turn_id,
                &prepared.trigger_line,
            )
            .await?;
            let outcome = engine
                .await_completion_direct(child, worktree, &prepared.turn_id, &control)
                .await?;
            (outcome, None, resumed)
        }
    };

    record_agent_session(
        deps,
        run,
        worktree,
        lane.lane.as_str(),
        pane.as_ref(),
        resumed,
        &outcome,
    )
    .await?;

    let (outcome_str, result_json) = match &outcome {
        TurnOutcome::Completed(r) => (
            format!("{:?}", r.status).to_lowercase(),
            Some(serde_json::to_string(&json!({
                "turn_id": r.turn_id, "summary": r.summary,
            }))?),
        ),
        TurnOutcome::Stopped => ("stopped".to_string(), None),
        TurnOutcome::PaneDied => ("pane_died".to_string(), None),
    };
    deps.store
        .finish_turn(&prepared.turn_id, &outcome_str, result_json.as_deref())?;
    Ok((outcome, prepared.turn_id))
}

/// Spawn one direct-mode turn (issue #169): `{command} {args} {direct_args}
/// [{resume_args} <session-id>] <trigger>` as a plain subprocess, cwd the
/// worktree. Unlike [`ensure_pane`] there is nothing to reuse across turns —
/// direct mode has no persistent process — so this always spawns fresh; a
/// saved session id (if any) is only ever used to `--resume`, never probed
/// for survival (a bad id simply makes the CLI exit without a result, which
/// `await_completion_direct` already maps to `PaneDied`, clearing the id for
/// the next turn). Returns the child plus whether this was a resume attempt.
async fn spawn_direct_process(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    lane: &Lane,
    turn_id: &str,
    initial_trigger: &str,
) -> Result<(tokio::process::Child, bool)> {
    let lane_name = lane.lane.as_str();
    let profile = &lane.profile;

    let session_id = deps
        .store
        .get_pane(&deps.project.id, run.issue_number, lane_name)?
        .and_then(|p| p.agent_session_id);

    let mut args = profile.args.clone();
    args.extend(profile.direct_args.iter().cloned());
    if let Some(session_id) = &session_id {
        args.extend(profile.resume_args.iter().cloned());
        args.push(session_id.clone());
    }
    args.push(initial_trigger.to_string());

    // No pane scrollback in direct mode; capture stdout+stderr to a per-turn
    // log so a "died without a result" turn still leaves something to read
    // (the closest direct-mode equivalent of `meguri attach`).
    let log_path = crate::turn::prompts::meguri_dir(worktree).join(format!("direct-{turn_id}.log"));
    std::fs::create_dir_all(crate::turn::prompts::meguri_dir(worktree))?;
    let log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;

    let mut cmd = tokio::process::Command::new(&profile.command);
    cmd.args(&args)
        .current_dir(worktree)
        .kill_on_drop(true)
        .stdout(
            log.try_clone()
                .with_context(|| "cloning direct-mode log handle")?,
        )
        .stderr(log);
    if let Some(hint) = &profile.herdr_agent_hint {
        cmd.env("HERDR_AGENT", hint);
    }
    let child = cmd
        .spawn()
        .with_context(|| format!("spawning direct-mode agent `{}`", profile.command))?;

    deps.store.emit(
        Some(&run.id),
        "direct.spawned",
        json!({ "lane": lane_name, "profile": lane.profile_name,
                "resumed": session_id.is_some(), "log": log_path.to_string_lossy() }),
    )?;
    Ok((child, session_id.is_some()))
}

/// Keep the lane's resumable session id in sync with what the turn taught
/// us. The truth lives on the pane row (`panes.agent_session_id`, issue
/// lifetime) — the resume path reads only that, whether or not the lane has
/// a live pane (issue #169 broadens "lane" from "pane" to "pane optional").
/// After every completed turn the primary source is a file scan of the
/// worktree's transcripts (`agent_session::latest_session_id`, reliable and
/// independent of agent self-reporting); the result file's self-report and
/// (pane mode only) the mux (herdr carries it on `pane get`) are fallbacks.
/// `runs.agent_session_id` is still written for observability. A resumed
/// executor dying without a result means the stored id no longer restores a
/// working session, so drop it rather than resume-loop on it forever.
async fn record_agent_session(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    lane_name: &str,
    pane: Option<&PaneId>,
    resumed: bool,
    outcome: &TurnOutcome,
) -> Result<()> {
    match outcome {
        TurnOutcome::Completed(r) => {
            let session_root = agent_session::session_root(&deps.config.agent);
            let session_id = match agent_session::latest_session_id(&session_root, worktree) {
                Some(id) => Some(id),
                None => match &r.agent_session_id {
                    Some(id) => Some(id.clone()),
                    None => match pane {
                        Some(pane) => deps.mux.agent_session_id(pane).await.unwrap_or(None),
                        None => None,
                    },
                },
            };
            if let Some(id) = session_id
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
            {
                deps.store.upsert_pane_session(
                    &deps.project.id,
                    run.issue_number,
                    lane_name,
                    &worktree.to_string_lossy(),
                    Some(&id),
                )?;
                deps.store.update_run_agent_session(&run.id, Some(&id))?;
            }
        }
        TurnOutcome::PaneDied if resumed => {
            deps.store
                .save_pane_session(&deps.project.id, run.issue_number, lane_name, None)?;
            deps.store.update_run_agent_session(&run.id, None)?;
            deps.store.emit(
                Some(&run.id),
                "agent_session.cleared",
                json!({ "reason": "resumed executor died without a result" }),
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// Applies a verified execute turn's agent-authored fields to the
/// checkpoint. `sets_subject` gates whether `result.subject` may
/// (re)establish `cp.subject` (see [`Flavor::sets_subject`]); an absent or
/// blank (whitespace-only) `subject` never clears one an earlier turn
/// already set — a blank value would otherwise survive into
/// `default_pr_title()` as a literal empty title instead of falling back to
/// the issue title.
fn apply_execute_result(cp: &mut Checkpoint, result: TurnResultFile, sets_subject: bool) {
    if sets_subject {
        let subject = result.subject.as_deref().map(str::trim).unwrap_or("");
        if !subject.is_empty() {
            cp.subject = Some(subject.to_string());
        }
    }
    cp.summary = result.summary;
    cp.pr_body = result.pr_body;
}

/// execute: agent does the loop's work; the orchestrator independently
/// verifies that committed work exists (plus the flavor's own check) before
/// moving on.
async fn execute(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    worktree: &Path,
    flavor: &dyn Flavor,
) -> Result<StepFlow> {
    let mut prompt = flavor.execute_prompt(deps, run, cp, worktree);
    let mut corrective_turns = 0u32;

    loop {
        let (outcome, _) = run_turn(deps, run, worktree, "execute", &prompt).await?;
        let result = match outcome {
            TurnOutcome::Completed(r) => r,
            TurnOutcome::Stopped => return Ok(StepFlow::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(StepFlow::Interrupted("pane died during execute".into()));
            }
        };

        match result.status {
            TurnStatus::Success => {}
            TurnStatus::Failure => {
                return Err(NeedsHuman(format!(
                    "agent reported failure on issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
            TurnStatus::NeedsHuman => {
                return Err(NeedsHuman(format!(
                    "agent needs a human on issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
            TurnStatus::NeedsPlan => {
                return Ok(StepFlow::NeedsPlan(result.summary));
            }
            TurnStatus::Decompose => {
                return Ok(StepFlow::Decompose(result));
            }
        }

        // Trust but verify: success means commits exist, nothing dangles,
        // and the flavor's expected artifact is in place.
        let base = flavor.verify_base(deps, run);
        let clean = gitops::status_clean(worktree).await?;
        let ahead = gitops::commits_ahead(worktree, &base).await?;
        let problem = if !clean || ahead == 0 {
            Some(format!(
                "- working tree clean: {clean} (must be true — commit or discard everything)\n\
                 - commits ahead of {base}: {ahead} (must be > 0)",
            ))
        } else {
            flavor.verify_work(run, cp, worktree).err()
        };
        let Some(problem) = problem else {
            // Keep what the agent said for the PR title/body (persisted by
            // the caller's step save).
            apply_execute_result(cp, result, flavor.sets_subject());
            deps.store.emit(
                Some(&run.id),
                "execute.verified",
                json!({ "commits": ahead }),
            )?;
            return Ok(StepFlow::Continue);
        };

        corrective_turns += 1;
        if corrective_turns > 1 {
            return Err(NeedsHuman(format!(
                "agent claimed success but the work doesn't verify after a \
                 corrective turn:\n{problem}"
            ))
            .into());
        }
        deps.store.emit(
            Some(&run.id),
            "execute.correction",
            json!({ "clean": clean, "commits": ahead, "problem": problem }),
        )?;
        prompt = format!(
            "Your previous result claimed success, but verification failed:\n{problem}\n\n\
             Fix this and commit your completed work with clear messages. \
             Do not create a pull request; meguri handles that.",
        );
    }
}

/// validate: the orchestrator itself runs the project's check command and
/// feeds failures back to the agent, never trusting agent claims. Shared by
/// the `validate` step and the self-review phase's post-fix re-validation;
/// `persist_step` is the step written while the fix-turn counter advances, so
/// a crash resumes into the right phase.
pub(crate) async fn validate(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    worktree: &Path,
    persist_step: &str,
) -> Result<StepFlow> {
    let Some(check) = deps.project.check_command.clone() else {
        deps.store
            .emit(Some(&run.id), "validate.skipped", json!({}))?;
        return Ok(StepFlow::Continue);
    };

    loop {
        deps.store.emit(
            Some(&run.id),
            "validate.running",
            json!({ "command": check }),
        )?;
        let out = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&check)
            .current_dir(worktree)
            .output()
            .await?;
        if out.status.success() {
            deps.store
                .emit(Some(&run.id), "validate.passed", json!({}))?;
            return Ok(StepFlow::Continue);
        }

        cp.fix_turns_used += 1;
        save_step(deps, run, persist_step, cp)?;
        if cp.fix_turns_used > deps.config.limits.validate_turns {
            return Err(NeedsHuman(format!(
                "validation `{check}` still failing after {} fix turns",
                cp.fix_turns_used - 1
            ))
            .into());
        }

        // Signal-driven escalation (issue #66): once the current profile has had
        // a failing fix turn (`pin_fix_turns >= 1`), climb to a stronger profile
        // if the run's role has an escalation chain. Keying on "did the current
        // pin try?" instead of `fix_turns_used` keeps it crash-safe — a resume
        // that re-runs the check can't skip a chain entry that never ran. A
        // no-op past the chain end, so a genuinely stuck run still exhausts
        // `validate_turns` and lands on needs-human above.
        let escalated = if cp.pin_fix_turns >= 1 {
            maybe_escalate(deps, run, cp, persist_step).await?
        } else {
            false
        };
        save_step(deps, run, persist_step, cp)?;

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail = |s: &str| -> String {
            s.lines()
                .rev()
                .take(60)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        };
        deps.store.emit(
            Some(&run.id),
            "validate.failed",
            json!({ "fix_turn": cp.fix_turns_used }),
        )?;

        // When we just escalated, this pane is a fresh session under a stronger
        // model with none of the prior conversation — so the prompt carries the
        // validation history (it always does) plus a note that it is a retry
        // under a stronger model.
        let escalation_note = if escalated {
            "\n\nNote: this run was escalated to a stronger model for this \
             attempt because validation kept failing. You are a fresh session — \
             rely on the command output below, not on earlier conversation."
        } else {
            ""
        };
        let prompt = format!(
            "The project's validation command failed. Fix the code so it passes, \
             then commit your fixes.\n\nCommand: `{check}`\nExit code: {}\n\n\
             Last stdout:\n```\n{}\n```\n\nLast stderr:\n```\n{}\n```\n\n\
             Do not create a pull request; meguri handles that.{escalation_note}",
            out.status.code().unwrap_or(-1),
            tail(&stdout),
            tail(&stderr),
        );
        let (outcome, _) = run_turn(deps, run, worktree, "fix-validation", &prompt).await?;
        match outcome {
            TurnOutcome::Completed(r) => match r.status {
                TurnStatus::Success => {
                    // The current pin just spent a fix turn; record it so the
                    // next failure may escalate off it (and a resume knows this
                    // pin has been tried).
                    cp.pin_fix_turns += 1;
                    save_step(deps, run, persist_step, cp)?;
                    continue;
                }
                // needs_plan/decompose make no sense once work is committed
                // and failing validation — escalate like the other two.
                TurnStatus::Failure
                | TurnStatus::NeedsHuman
                | TurnStatus::NeedsPlan
                | TurnStatus::Decompose => {
                    return Err(NeedsHuman(format!(
                        "agent could not fix validation: {}",
                        r.summary
                    ))
                    .into());
                }
            },
            TurnOutcome::Stopped => return Ok(StepFlow::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(StepFlow::Interrupted("pane died during validate".into()));
            }
        }
    }
}

/// Produce the run's deliverable per the project's `deliver` setting and
/// return a string locating it (a PR URL, or the branch name). The shape is
/// mode-independent from here on; only this step differs.
async fn deliver(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    worktree: &Path,
    flavor: &dyn Flavor,
) -> Result<String> {
    match deps.config.deliver_for(&deps.project) {
        Deliver::Pr => open_pr(deps, run, cp, worktree, flavor).await,
        Deliver::Branch => {
            // Leave the verified commits on the local branch: no push, no PR.
            // `settle_labels` still runs (it is the coordination-layer
            // completion — `status='done'` locally, label removal in github).
            let branch = run.branch.clone().context("run has no branch")?;
            flavor.settle_labels(deps, run, cp).await?;
            deps.store.emit(
                Some(&run.id),
                "branch.delivered",
                json!({ "branch": branch }),
            )?;
            Ok(branch)
        }
        Deliver::Patch => {
            bail!("deliver = \"patch\" is not implemented yet (issue #54 Phase 2)")
        }
    }
}

/// open-pr: push, create the PR, settle labels. All side effects here are
/// idempotent enough to re-run after an interruption.
async fn open_pr(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    worktree: &Path,
    flavor: &dyn Flavor,
) -> Result<String> {
    let branch = run.branch.clone().context("run has no branch")?;
    gitops::push_branch(worktree, &branch).await?;

    // Inspection history (ADR 0008): once the reviewed head is pushed, stamp
    // the self-review verdict as a `meguri/self-review` commit status. The
    // internal loop ran pre-open in the worktree; the status makes its outcome
    // visible on the PR head without touching the conversation. Best-effort:
    // a status failure must not fail an otherwise-good PR.
    post_self_review_status(deps, run, cp, worktree).await;

    let lenses = &deps.config.review_for(&deps.project).lenses;
    let close = flavor.pr_closes_issue(deps);
    let pr_url = if let Some(url) = &cp.pr_url {
        url.clone() // resumed after PR creation
    } else {
        let title = flavor.pr_title(run, cp);
        let body = compose_pr_body(run, cp, lenses, close);
        // Auto-merge opt-in PRs open non-draft: waiting for a human to promote
        // a draft would waste the required-checks run the arm is waiting on
        // (auto-merge 1/3, #41).
        let draft = deps.config.pr_for(&deps.project).draft && !cp.automerge;
        let pr = deps
            .forge()
            .create_pr(
                &branch,
                &deps.project.default_branch,
                &title,
                &body,
                draft,
                &[],
            )
            .await?;
        cp.pr_url = Some(pr.url.clone());
        cp.pr_number = Some(pr.number);
        save_step(deps, run, STEP_OPEN_PR, cp)?;
        deps.store
            .emit(Some(&run.id), "pr.created", json!({ "url": pr.url }))?;
        // Copy the opt-in label onto the PR so the sweep can arm it without
        // re-reading the issue (the sweep keeps the issue-label fallback for
        // any copy that does not land).
        if cp.automerge {
            deps.forge()
                .add_pr_label(pr.number, forge::LABEL_AUTOMERGE)
                .await
                .ok();
        }
        pr.url
    };

    flavor.settle_presentation(deps, run, cp).await?;
    flavor.settle_labels(deps, run, cp).await?;
    Ok(pr_url)
}

/// Escalate-time fallback (issue #209, ADR 0021): when self-review cannot
/// converge and the branch is ahead of base, push it and open a draft PR
/// labeled `meguri:needs-human` so the half-finished artifact is visible on the
/// forge instead of trapped in the worktree. The label rides PR creation (a
/// single forge call), so the draft is never observable unlabeled and
/// `pr_is_touchable` excludes it from the first moment.
///
/// Best-effort throughout: it never returns an error and never changes the
/// run's terminal outcome — the caller still propagates the original
/// `NeedsHuman` and escalates the issue with a comment (the draft is the
/// evidence, the comment is the notification). No-op unless the project
/// delivers via PR, a forge is present, and the branch has commits ahead of
/// base. `pr.created` is deliberately NOT emitted (that event means "a verified
/// deliverable shipped"); an escalate-time draft emits `self_review.escalated_draft`.
///
/// Resume-safe like `open_pr`: before creating it looks up any PR already on
/// the branch (`pr_for_branch`) and adopts it, so a resume from
/// `STEP_SELF_REVIEW` after a crash — or the same escalation re-running — never
/// opens a duplicate.
pub(crate) async fn publish_needs_human_draft(
    deps: &Deps,
    run: &RunRecord,
    cp: &Checkpoint,
    worktree: &Path,
    flavor: &dyn Flavor,
) {
    if !matches!(deps.config.deliver_for(&deps.project), Deliver::Pr) || deps.forge.is_none() {
        return;
    }
    let Some(branch) = run.branch.clone() else {
        return;
    };
    let base = &deps.project.default_branch;

    // Only publish when there is actually committed work to show; an
    // unconverged run with an empty branch stays comment-only (the artifact
    // never made it to a commit).
    match gitops::commits_ahead(worktree, base).await {
        Ok(0) => return,
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(
                "issue #{}: cannot check commits ahead for needs-human draft: {e:#}",
                run.issue_number
            );
            return;
        }
    }

    // Resume-safety: self-review resumes at STEP_SELF_REVIEW, so a prior
    // escalation of this run may have already opened the draft (a crash after
    // create, or the same NeedsHuman path re-running on resume). Never open a
    // second PR for the branch — adopt whatever exists, and don't resurrect one
    // a human already closed (the "捨てる" recovery path). A lookup error is
    // treated as "cannot confirm" and skips rather than risk a duplicate.
    match deps.forge().pr_for_branch(&branch).await {
        Ok(Some(pr)) => {
            let _ = deps.store.emit(
                Some(&run.id),
                "self_review.escalated_draft_exists",
                json!({ "pr": pr.number, "url": pr.url, "state": pr.state }),
            );
            return;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                "issue #{}: cannot check for an existing PR on {branch}: {e:#} — \
                 skipping needs-human draft to avoid a duplicate",
                run.issue_number
            );
            return;
        }
    }

    if let Err(e) = gitops::push_branch(worktree, &branch).await {
        let _ = deps.store.emit(
            Some(&run.id),
            "self_review.draft_push_failed",
            json!({ "branch": branch, "error": format!("{e:#}") }),
        );
        return;
    }

    let title = flavor.pr_title(run, cp);
    let lenses = &deps.config.review_for(&deps.project).lenses;
    let body = compose_needs_human_draft_body(run, cp, lenses);
    match deps
        .forge()
        .create_pr(
            &branch,
            base,
            &title,
            &body,
            true,
            &[forge::LABEL_NEEDS_HUMAN],
        )
        .await
    {
        Ok(pr) => {
            let _ = deps.store.emit(
                Some(&run.id),
                "self_review.escalated_draft",
                json!({ "pr": pr.number, "url": pr.url,
                        "rounds": cp.self_review_rounds,
                        "pending": cp.self_review_pending.len() }),
            );
        }
        Err(e) => {
            let _ = deps.store.emit(
                Some(&run.id),
                "self_review.draft_failed",
                json!({ "branch": branch, "error": format!("{e:#}") }),
            );
        }
    }
}

/// The PR-title formula every [`Flavor::pr_title`] shares (issue #136): the
/// agent-authored `subject` from the turn that established it, or the issue
/// title when none was ever set (backward compatibility) — followed by the
/// issue number the forge conventionally carries in a squash-merged repo.
pub(crate) fn default_pr_title(run: &RunRecord, cp: &Checkpoint) -> String {
    format!(
        "{} (#{})",
        cp.subject.as_deref().unwrap_or(&cp.issue_title),
        run.issue_number
    )
}

/// The PR body meguri wraps around the agent's description: an issue-link
/// header, the agent-authored description (its `pr_body`, or the execute
/// turn's summary as a fallback), the self-review `<details>` (ADR 0008), and
/// the meguri footer. Shared by new-PR creation and the spec worker's
/// spec→implementation body transition so the two paths render an identical
/// shape (issue #98). `lenses` names the perspectives the self-review applied.
pub(crate) fn compose_pr_body(
    run: &RunRecord,
    cp: &Checkpoint,
    lenses: &[String],
    close: bool,
) -> String {
    let description = cp
        .pr_body
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cp.summary.trim());
    // A published PR either converged clean, or took the cap→final-fix path
    // (issue #212): in the latter the last fix was not re-reviewed, so the body
    // says so plainly and the human merge gate is the backstop.
    let final_fix_note = if cp.self_review_final_fix_unreviewed {
        "> ⚠️ 最終ラウンドの fix は未再レビューです(check_command と tree 検証は通過)。\
         human merge gate で確認してください。\n\n"
    } else {
        ""
    };
    format!(
        "{}.\n\n{}{}{}\n\n---\n🔁 Opened by [meguri](https://github.com/kkato1030/meguri) \
         from an interactive agent session (run `{}`).",
        issue_reference(run.issue_number, close),
        final_fix_note,
        description,
        self_review_details(cp, lenses),
        run.id
    )
}

/// The header line linking a PR to its issue. `Closes #N` (auto-closes on
/// merge) for a combined delivery / normal PR; `Refs #N` (non-closing) when
/// the caller sets `close = false` — the separate spec PR must not close the
/// issue when it merges (ADR 0008 §6).
pub(crate) fn issue_reference(issue: i64, close: bool) -> String {
    if close {
        format!("Closes #{issue}")
    } else {
        format!("Refs #{issue}")
    }
}

/// The folded self-review summary that rides the PR body (ADR 0008): the
/// lenses applied and one line per round (verdict + finding count). Empty when
/// no self-review ran (loops without it, or `review.enabled = false`), so the
/// body stays clean.
fn self_review_details(cp: &Checkpoint, lenses: &[String]) -> String {
    // A published PR either converged clean or took the cap→final-fix path
    // (issue #212); the outcome headline distinguishes the two.
    let outcome = if cp.self_review_final_fix_unreviewed {
        format!("最終 fix 未再レビュー · {} rounds", cp.self_review_rounds)
    } else {
        format!("clean after {} rounds", cp.self_review_rounds)
    };
    self_review_details_with_outcome(cp, lenses, &outcome)
}

/// Shared renderer for the folded self-review `<details>`: the `outcome`
/// headline plus one line per round. Empty when no self-review round ran. The
/// happy path passes a "clean after N rounds" outcome ([`self_review_details`]);
/// the escalate-time evidence draft (issue #209) passes an "unconverged" one.
fn self_review_details_with_outcome(cp: &Checkpoint, lenses: &[String], outcome: &str) -> String {
    if cp.self_review_log.is_empty() {
        return String::new();
    }
    let rounds = cp
        .self_review_log
        .iter()
        .map(|r| {
            let verdict = if r.findings == 0 {
                "clean".to_string()
            } else {
                format!("{} findings", r.findings)
            };
            format!("- round {}: {verdict}", r.round)
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "\n\n<details>\n<summary>🔁 self-review — {outcome}</summary>\n\n\
         lenses: {lenses}\n\n{rounds}\n</details>",
        lenses = lenses.join(" / "),
    )
}

/// The PR body for an escalate-time needs-human draft (issue #209, ADR 0021).
/// Unlike [`compose_pr_body`], this is NOT a verified deliverable: self-review
/// did not converge, so the tree carries no green guarantee. The body says so
/// plainly, links the issue without closing it (`Refs #N`), and folds in the
/// self-review round history for context.
pub(crate) fn compose_needs_human_draft_body(
    run: &RunRecord,
    cp: &Checkpoint,
    lenses: &[String],
) -> String {
    let description = cp
        .pr_body
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cp.summary.trim());
    let outcome = format!(
        "未収束 · {} rounds · {} 件未解決",
        cp.self_review_rounds,
        cp.self_review_pending.len()
    );
    format!(
        "{}.\n\n\
         > ⚠️ **未収束の証拠物件です(グリーン保証なし)。** self-review が収束しなかったため、\
         meguri が `meguri:needs-human` を付けて draft のまま公開しました。中身を見て活かすなら\
         手で直して ready + `meguri:spec-ready` に、捨てるならこの PR を閉じてください。\n\n\
         {}{}\n\n---\n🔁 Opened by [meguri](https://github.com/kkato1030/meguri) \
         as a needs-human draft from an interactive agent session (run `{}`).",
        issue_reference(run.issue_number, false),
        description,
        self_review_details_with_outcome(cp, lenses, &outcome),
        run.id
    )
}

/// Stamp the self-review verdict as a `meguri/self-review` commit status on
/// the freshly-pushed head (ADR 0008). Only when a self-review actually ran
/// and a forge is present; best-effort (the PR is already the durable truth).
async fn post_self_review_status(deps: &Deps, run: &RunRecord, cp: &Checkpoint, worktree: &Path) {
    if cp.self_review_log.is_empty() || deps.forge.is_none() {
        return;
    }
    let Ok(head) = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await else {
        return;
    };
    let head = head.trim();
    // A published PR either converged clean or took the cap→final-fix path
    // (issue #212); the status is Success either way (check_command + tree
    // passed), but the description records a non-re-reviewed final fix.
    let desc = if cp.self_review_final_fix_unreviewed {
        format!("final fix unreviewed · {} rounds", cp.self_review_rounds)
    } else {
        format!("clean · {} rounds", cp.self_review_rounds)
    };
    let (state, desc) = (crate::forge::CommitStatusState::Success, desc);
    if let Err(e) = deps
        .forge()
        .set_commit_status(head, "meguri/self-review", state, &desc)
        .await
    {
        deps.store
            .emit(
                Some(&run.id),
                "self_review.status_failed",
                json!({ "error": format!("{e:#}") }),
            )
            .ok();
    }
}

/// Where repositories keep their PR template, in priority order.
const PR_TEMPLATE_PATHS: &[&str] = &[
    ".github/pull_request_template.md",
    ".github/PULL_REQUEST_TEMPLATE.md",
    "docs/pull_request_template.md",
    "pull_request_template.md",
];

/// Fallback PR template when the repository doesn't ship one.
const DEFAULT_PR_TEMPLATE: &str = "## Summary\n<what & why>\n\n\
     ## Changes\n- <key changes>\n\n\
     ## Testing\n- <verification / tests you ran>";

/// The repository's own PR template, read from the worktree (never delegated
/// to the agent).
fn find_pr_template(worktree: &Path) -> Option<String> {
    PR_TEMPLATE_PATHS
        .iter()
        .map(|rel| worktree.join(rel))
        .find_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Prompt section pinning the language of agent-authored deliverables.
/// Returns "" when no language is configured (the agent's default, usually
/// English, wins). Prefixed with a blank line so callers can append it
/// unconditionally.
pub fn language_instruction(language: Option<&str>) -> String {
    let Some(lang) = language else {
        return String::new();
    };
    format!(
        "\n\n# Output language\n\
         Write every human-readable deliverable in {lang}: the free-text \
         fields of the result file (`summary`, `pr_body`) and anything you \
         author for humans (specs, ADRs, review comments, ...). Code \
         identifiers and commit messages follow the repository's existing \
         conventions."
    )
}

/// Prompt section asking the agent to author the PR description (`pr_body`).
pub fn pr_body_instruction(worktree: &Path) -> String {
    let template = find_pr_template(worktree).unwrap_or_else(|| DEFAULT_PR_TEMPLATE.to_string());
    format!(
        "# Pull request description\n\
         meguri opens the pull request; you write its description. In the completion \
         result file, set `pr_body` to a Markdown description that fills in every \
         section of the template below with what you actually did (do not paste the \
         issue text):\n\n{template}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::Multiplexer as _;

    fn fake_run(issue: i64) -> RunRecord {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store
            .create_run_for_loop("proj", "test-flavor", issue, "t")
            .unwrap();
        store.get_run(&run.id).unwrap().unwrap()
    }

    /// A worker run + a collab-advisor-on Deps rooted at `worktree_root`,
    /// plus the FakeMux handle for tests that script spawn behavior.
    fn advisor_env(
        worktree_root: &Path,
    ) -> (Deps, RunRecord, std::sync::Arc<crate::mux::fake::FakeMux>) {
        use crate::config::{CollabConfig, CollabMode, Config, ProjectConfig};
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store
            .create_run_for_loop("proj", super::super::worker::KIND, 7, "t")
            .unwrap();
        let run = store.get_run(&run.id).unwrap().unwrap();
        let config = Config {
            collab: Some(CollabConfig {
                mode: CollabMode::Advisor,
                advisor_role: "planner".into(),
            }),
            ..Config::default()
        };
        let project = ProjectConfig {
            id: "proj".into(),
            repo_path: Some("/tmp/unused".into()),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            worktree_root: Some(worktree_root.to_path_buf()),
            pr: None,
            clean: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup: Default::default(),
            schedules: Vec::new(),
            cadence: Vec::new(),
            prompts: Default::default(),
            notify: None,
            triage: None,
            autonomy: None,
        };
        let mux = std::sync::Arc::new(crate::mux::fake::FakeMux::new(false));
        let deps = Deps::with_label_source(
            store,
            mux.clone(),
            std::sync::Arc::new(crate::forge::fake::FakeForge::default()),
            config,
            project,
        );
        (deps, run, mux)
    }

    #[tokio::test]
    async fn advisor_spawns_before_execute_and_reaps_after() {
        let root = tempfile::tempdir().unwrap();
        let (deps, run, _mux) = advisor_env(root.path());
        let cp = Checkpoint {
            issue_title: "T".into(),
            issue_body: "B".into(),
            ..Default::default()
        };
        // No spec file in the worktree → the seed falls back to the issue body.
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();

        // Before spawn: no advisor pane, so no consult block.
        assert!(advisor_consult_section(&deps, &run).is_empty());

        ensure_advisor(&deps, &run, &worktree, &cp).await;

        // A live advisor pane exists, keyed on the advisor lane, and a bare
        // advisor cwd was created (read-only by wiring — no repo checkout).
        let pane = deps
            .store
            .get_pane("proj", 7, crate::store::LANE_ADVISOR)
            .unwrap()
            .unwrap();
        assert!(pane.mux_pane_id.is_some());
        let advisor_dir = root.path().join("proj").join("advisor-7");
        assert!(advisor_dir.exists());
        // The worker's own run pane was NOT pointed at the advisor.
        assert!(
            deps.store
                .get_run(&run.id)
                .unwrap()
                .unwrap()
                .mux_pane_id
                .is_none()
        );

        // The consult block now appears, carrying the project-scoped team.
        let block = advisor_consult_section(&deps, &run);
        assert!(block.contains("meguri-proj-7"), "{block}");
        assert!(block.contains("agmsg"));

        // Release reaps the pane and removes the bare dir; no session id saved.
        release_advisor(&deps, &run).await;
        let pane = deps
            .store
            .get_pane("proj", 7, crate::store::LANE_ADVISOR)
            .unwrap()
            .unwrap();
        assert_eq!(pane.mux_pane_id, None);
        assert_eq!(pane.agent_session_id, None);
        assert!(!advisor_dir.exists());
        assert!(advisor_consult_section(&deps, &run).is_empty());
    }

    /// A crashed advisor — or a reaper orphan sweep that kills only the pane —
    /// leaves files in the advisor cwd. Re-embodiment (ADR 0006) must not let
    /// the fresh individual see them: the directory is recreated empty.
    #[tokio::test]
    async fn advisor_respawn_recreates_cwd_without_stale_files() {
        let root = tempfile::tempdir().unwrap();
        let (deps, run, _mux) = advisor_env(root.path());
        let cp = Checkpoint {
            issue_title: "T".into(),
            issue_body: "B".into(),
            ..Default::default()
        };
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();

        // Leftovers from a previous individual that was never cleanly released.
        let advisor_dir = root.path().join("proj").join("advisor-7");
        std::fs::create_dir_all(advisor_dir.join("notes")).unwrap();
        std::fs::write(advisor_dir.join("stale.md"), "old advisor state").unwrap();
        std::fs::write(advisor_dir.join("notes").join("deep.md"), "nested").unwrap();

        ensure_advisor(&deps, &run, &worktree, &cp).await;

        // The pane spawned and the cwd exists again — but empty: no stale files.
        assert!(
            deps.store
                .get_pane("proj", 7, crate::store::LANE_ADVISOR)
                .unwrap()
                .unwrap()
                .mux_pane_id
                .is_some()
        );
        assert!(advisor_dir.exists());
        assert_eq!(
            std::fs::read_dir(&advisor_dir).unwrap().count(),
            0,
            "advisor cwd must be recreated empty on respawn"
        );
    }

    /// tmux/herdr can hand back a pane id even when the agent command died
    /// immediately. The worker prompt must not advertise an advisor nobody is
    /// running: a dead-on-arrival pane is a failed spawn — no live pane row,
    /// no consult block, and the ephemeral cwd is swept.
    #[tokio::test]
    async fn advisor_dead_on_arrival_pane_yields_no_consult_block() {
        let root = tempfile::tempdir().unwrap();
        let (deps, run, mux) = advisor_env(root.path());
        let cp = Checkpoint {
            issue_title: "T".into(),
            issue_body: "B".into(),
            ..Default::default()
        };
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        // The advisor's seed prompt carries the team name; matching on it
        // makes the spawn return a pane id whose agent is already dead.
        mux.fail_spawns_matching("meguri-proj-7");

        ensure_advisor(&deps, &run, &worktree, &cp).await;

        assert_eq!(
            deps.store
                .get_pane("proj", 7, crate::store::LANE_ADVISOR)
                .unwrap()
                .and_then(|p| p.mux_pane_id),
            None,
            "a dead-on-arrival pane must not be registered as live"
        );
        assert!(
            advisor_consult_section(&deps, &run).is_empty(),
            "the worker prompt must not advertise a dead advisor"
        );
        assert!(
            !root.path().join("proj").join("advisor-7").exists(),
            "the ephemeral advisor cwd must be swept on a failed spawn"
        );
    }

    /// `release_advisor` returns early when no `LANE_ADVISOR` row exists, so
    /// a spawn failure after the cwd was created must sweep the directory
    /// itself — the advisor stays ephemeral even when its best-effort spawn
    /// fails.
    #[tokio::test]
    async fn advisor_spawn_failure_sweeps_created_cwd() {
        let root = tempfile::tempdir().unwrap();
        let (deps, run, mux) = advisor_env(root.path());
        let cp = Checkpoint {
            issue_title: "T".into(),
            issue_body: "B".into(),
            ..Default::default()
        };
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        // spawn_pane itself errors — after the advisor cwd has been created.
        mux.error_spawns_matching("meguri-proj-7");

        ensure_advisor(&deps, &run, &worktree, &cp).await;

        assert!(
            !root.path().join("proj").join("advisor-7").exists(),
            "a spawn failure must not strand the advisor cwd"
        );
        assert!(
            deps.store
                .get_pane("proj", 7, crate::store::LANE_ADVISOR)
                .unwrap()
                .and_then(|p| p.mux_pane_id)
                .is_none()
        );
        assert!(advisor_consult_section(&deps, &run).is_empty());
    }

    /// If registering the `LANE_ADVISOR` row fails after the pane was spawned,
    /// the pane must be killed before erring: without the row neither
    /// `release_advisor` nor the reaper can see it, so surviving here would
    /// mean an untracked agent running forever.
    #[tokio::test]
    async fn advisor_pane_registration_failure_kills_the_pane() {
        let root = tempfile::tempdir().unwrap();
        let (deps, run, mux) = advisor_env(root.path());
        let cp = Checkpoint {
            issue_title: "T".into(),
            issue_body: "B".into(),
            ..Default::default()
        };
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        // Inject a store failure: any insert into the panes table aborts, so
        // upsert_pane fails after spawn_pane already handed back a live pane.
        deps.store
            .with_conn(|c| {
                c.execute_batch(
                    "CREATE TRIGGER fail_pane_insert BEFORE INSERT ON panes
                     BEGIN SELECT RAISE(ABORT, 'injected panes failure'); END;",
                )?;
                Ok(())
            })
            .unwrap();

        ensure_advisor(&deps, &run, &worktree, &cp).await;

        // Exactly one pane was spawned — and it did not survive the failure.
        assert_eq!(mux.pane_count(), 1);
        assert!(
            !mux.pane_alive(&PaneId("fake:1".into())).await.unwrap(),
            "the pane must be killed when its LANE_ADVISOR row cannot be written"
        );
        // No row, no consult block, and the ephemeral cwd was swept.
        assert!(
            deps.store
                .get_pane("proj", 7, crate::store::LANE_ADVISOR)
                .unwrap()
                .is_none()
        );
        assert!(advisor_consult_section(&deps, &run).is_empty());
        assert!(!root.path().join("proj").join("advisor-7").exists());
    }

    /// If the `collab.advisor_spawned` emit fails after the row was written,
    /// the pane must still be torn down — via `release_pane`, so the row stops
    /// advertising a live pane to `advisor_consult_section`.
    #[tokio::test]
    async fn advisor_spawned_emit_failure_releases_the_pane() {
        let root = tempfile::tempdir().unwrap();
        let (deps, run, mux) = advisor_env(root.path());
        let cp = Checkpoint {
            issue_title: "T".into(),
            issue_body: "B".into(),
            ..Default::default()
        };
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        // Inject a store failure past registration: event inserts abort, so
        // upsert_pane succeeds but the collab.advisor_spawned emit fails.
        deps.store
            .with_conn(|c| {
                c.execute_batch(
                    "CREATE TRIGGER fail_event_insert BEFORE INSERT ON events
                     BEGIN SELECT RAISE(ABORT, 'injected events failure'); END;",
                )?;
                Ok(())
            })
            .unwrap();

        ensure_advisor(&deps, &run, &worktree, &cp).await;

        // Exactly one pane was spawned — and it did not survive the failure.
        assert_eq!(mux.pane_count(), 1);
        assert!(
            !mux.pane_alive(&PaneId("fake:1".into())).await.unwrap(),
            "the pane must be killed when the spawn cannot be journalled"
        );
        // The registered row was reclaimed: it no longer claims a live pane,
        // so the worker prompt does not advertise the dead advisor.
        assert_eq!(
            deps.store
                .get_pane("proj", 7, crate::store::LANE_ADVISOR)
                .unwrap()
                .and_then(|p| p.mux_pane_id),
            None
        );
        assert!(advisor_consult_section(&deps, &run).is_empty());
        assert!(!root.path().join("proj").join("advisor-7").exists());
    }

    #[tokio::test]
    async fn advisor_is_noop_for_ineligible_loops_and_when_off() {
        let root = tempfile::tempdir().unwrap();
        let worktree = root.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let cp = Checkpoint::default();

        // collab on, but a non-advisor loop kind: no pane.
        let (deps, mut run, _mux) = advisor_env(root.path());
        run.loop_kind = "planner".into();
        ensure_advisor(&deps, &run, &worktree, &cp).await;
        assert!(
            deps.store
                .get_pane("proj", 7, crate::store::LANE_ADVISOR)
                .unwrap()
                .is_none()
        );

        // collab off: an eligible loop still spawns nothing.
        let (mut deps, run, _mux) = advisor_env(root.path());
        deps.config.collab = None;
        ensure_advisor(&deps, &run, &worktree, &cp).await;
        assert!(
            deps.store
                .get_pane("proj", 7, crate::store::LANE_ADVISOR)
                .unwrap()
                .is_none()
        );
        assert!(advisor_consult_section(&deps, &run).is_empty());
    }

    #[test]
    fn default_pr_title_prefers_subject_and_falls_back_to_issue_title() {
        let run = fake_run(7);
        let mut cp = Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        };
        assert_eq!(default_pr_title(&run, &cp), "Add caching (#7)");

        cp.subject = Some("Cache API responses in memory".into());
        assert_eq!(
            default_pr_title(&run, &cp),
            "Cache API responses in memory (#7)"
        );
    }

    /// The cap→final-fix publish (issue #212) records the non-re-review in the
    /// PR body; a clean convergence does not.
    #[test]
    fn compose_pr_body_marks_final_fix_unreviewed() {
        use super::super::self_review::RoundRecord;
        let run = fake_run(7);
        let lenses = vec!["correctness".to_string()];
        let base = Checkpoint {
            issue_title: "Add caching".into(),
            summary: "done".into(),
            self_review_rounds: 3,
            self_review_log: vec![RoundRecord {
                round: 3,
                findings: 1,
            }],
            ..Default::default()
        };

        // Clean convergence: no final-fix warning, "clean after N rounds".
        let clean = compose_pr_body(&run, &base, &lenses, true);
        assert!(!clean.contains("未再レビュー"), "{clean}");
        assert!(clean.contains("clean after 3 rounds"), "{clean}");

        // Final-fix publish: the warning line and the "最終 fix 未再レビュー"
        // outcome both appear.
        let final_fix = Checkpoint {
            self_review_final_fix_unreviewed: true,
            ..base
        };
        let body = compose_pr_body(&run, &final_fix, &lenses, true);
        assert!(body.contains("最終ラウンドの fix は未再レビュー"), "{body}");
        assert!(body.contains("最終 fix 未再レビュー · 3 rounds"), "{body}");
    }

    #[test]
    fn apply_execute_result_gates_subject_by_flavor() {
        let mut cp = Checkpoint::default();
        let result_with = |subject: Option<&str>| TurnResultFile {
            turn_id: "t".into(),
            status: TurnStatus::Success,
            summary: "done".into(),
            subject: subject.map(str::to_string),
            pr_body: None,
            agent_session_id: None,
            children: vec![],
        };

        // Implementation-shaping turn: subject is established.
        apply_execute_result(&mut cp, result_with(Some("Add caching")), true);
        assert_eq!(cp.subject.as_deref(), Some("Add caching"));

        // Fix-family turn (sets_subject = false): a new subject is ignored,
        // the established one survives (no flapping).
        apply_execute_result(&mut cp, result_with(Some("Fix flaky test")), false);
        assert_eq!(cp.subject.as_deref(), Some("Add caching"));

        // Omitting `subject` never clears an already-established one either.
        apply_execute_result(&mut cp, result_with(None), true);
        assert_eq!(cp.subject.as_deref(), Some("Add caching"));
    }

    #[test]
    fn apply_execute_result_ignores_a_blank_subject() {
        let mut cp = Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        };
        let result = TurnResultFile {
            turn_id: "t".into(),
            status: TurnStatus::Success,
            summary: "done".into(),
            subject: Some("   ".into()),
            pr_body: None,
            agent_session_id: None,
            children: vec![],
        };

        apply_execute_result(&mut cp, result, true);

        assert_eq!(
            cp.subject, None,
            "a whitespace-only subject must not become the checkpoint's subject"
        );
        // Otherwise default_pr_title() would render a broken " (#N)" title
        // instead of falling back to the issue title.
        assert_eq!(default_pr_title(&fake_run(7), &cp), "Add caching (#7)");
    }

    #[test]
    fn apply_execute_result_trims_subject_whitespace() {
        let mut cp = Checkpoint::default();
        let result = TurnResultFile {
            turn_id: "t".into(),
            status: TurnStatus::Success,
            summary: "done".into(),
            subject: Some("  Add caching  ".into()),
            pr_body: None,
            agent_session_id: None,
            children: vec![],
        };

        apply_execute_result(&mut cp, result, true);

        assert_eq!(cp.subject.as_deref(), Some("Add caching"));
    }

    #[test]
    fn pr_template_discovery_prefers_repo_locations_in_order() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_pr_template(dir.path()), None);

        std::fs::write(dir.path().join("pull_request_template.md"), "root tpl\n").unwrap();
        assert_eq!(find_pr_template(dir.path()).as_deref(), Some("root tpl"));

        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/pull_request_template.md"), "docs tpl").unwrap();
        assert_eq!(find_pr_template(dir.path()).as_deref(), Some("docs tpl"));

        std::fs::create_dir_all(dir.path().join(".github")).unwrap();
        std::fs::write(
            dir.path().join(".github/pull_request_template.md"),
            "gh tpl",
        )
        .unwrap();
        assert_eq!(find_pr_template(dir.path()).as_deref(), Some("gh tpl"));
    }

    #[test]
    fn blank_repo_template_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("pull_request_template.md"), "  \n\n").unwrap();
        assert_eq!(find_pr_template(dir.path()), None);
    }

    #[test]
    fn language_instruction_is_empty_unless_configured() {
        assert_eq!(language_instruction(None), "");
        let section = language_instruction(Some("日本語"));
        assert!(section.contains("# Output language"));
        assert!(section.contains("日本語"));
    }

    #[test]
    fn pr_body_instruction_uses_repo_template_or_default() {
        let dir = tempfile::tempdir().unwrap();
        let section = pr_body_instruction(dir.path());
        assert!(section.contains("pr_body"));
        assert!(
            section.contains("## Summary"),
            "default template: {section}"
        );

        std::fs::create_dir_all(dir.path().join(".github")).unwrap();
        std::fs::write(
            dir.path().join(".github/pull_request_template.md"),
            "## Repo Sections\n- fill me\n",
        )
        .unwrap();
        let section = pr_body_instruction(dir.path());
        assert!(section.contains("## Repo Sections"));
        assert!(!section.contains("<what & why>"));
    }

    async fn init_repo(dir: &Path) {
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
        ] {
            gitops::run_git(dir, &args).await.unwrap();
        }
    }

    /// A `Deps` wired to a real local repo (no origin needed — `create_worktree`
    /// falls back to the local branch) plus fake forge/mux, and a run of
    /// `loop_kind` "worker" for issue 7 — enough to drive `prepare_worktree`
    /// in isolation, without spawning a pane.
    fn make_deps(
        repo_path: PathBuf,
        worktree_root: PathBuf,
        worktree_setup: crate::config::WorktreeSetupConfig,
    ) -> (Deps, RunRecord) {
        let store = crate::store::Store::open_in_memory().unwrap();
        let run = store
            .create_run_for_loop("proj", "worker", 7, "Test issue")
            .unwrap();
        let forge = std::sync::Arc::new(crate::forge::fake::FakeForge::with_issue(
            7,
            "Test issue",
            "body",
            &[],
        ));
        let project = crate::config::ProjectConfig {
            id: "proj".into(),
            repo_path: Some(repo_path),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            worktree_root: Some(worktree_root),
            pr: None,
            clean: None,
            triage: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup,
            schedules: Vec::new(),
            autonomy: None,
            cadence: Vec::new(),
            prompts: Default::default(),
            notify: None,
        };
        let deps = Deps::with_label_source(
            store,
            std::sync::Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge,
            crate::config::Config::default(),
            project,
        );
        (deps, run)
    }

    #[tokio::test]
    async fn worktree_setup_runs_with_the_documented_env_vars() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let worktree_root = tempfile::tempdir().unwrap();

        let (deps, run) = make_deps(
            repo.path().to_path_buf(),
            worktree_root.path().to_path_buf(),
            crate::config::WorktreeSetupConfig {
                commands: vec![
                    "echo $MEGURI_ROLE-$MEGURI_PROFILE-$MEGURI_ISSUE > marker.txt".into(),
                ],
                ..Default::default()
            },
        );
        let cp = Checkpoint::default();

        create_branch_worktree(&deps, &run, &cp).await.unwrap();

        let run = deps.store.get_run(&run.id).unwrap().unwrap();
        let wt = PathBuf::from(run.worktree_path.unwrap());
        let marker = std::fs::read_to_string(wt.join("marker.txt")).unwrap();
        assert_eq!(marker.trim(), "worker-default-7");
    }

    #[tokio::test]
    async fn worktree_setup_env_honors_an_already_pinned_profile() {
        // A run's launch profile can be pinned (runs.agent_profile) before
        // prepare-worktree ever runs — e.g. a resumed/retried run, or a
        // concurrent spawn. MEGURI_PROFILE must reuse that pin instead of
        // re-resolving routing from scratch, or the hook and the pane it's
        // preparing for could end up generating for different profiles.
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let worktree_root = tempfile::tempdir().unwrap();

        let (deps, run) = make_deps(
            repo.path().to_path_buf(),
            worktree_root.path().to_path_buf(),
            crate::config::WorktreeSetupConfig {
                commands: vec!["echo $MEGURI_PROFILE > profile.txt".into()],
                ..Default::default()
            },
        );
        // No [routing] configured, so a fresh `routing::resolve` call would
        // return "default" — pin something else first and confirm the hook
        // picks that up instead.
        deps.store
            .update_run_agent_profile(&run.id, "codex")
            .unwrap();
        let cp = Checkpoint::default();

        create_branch_worktree(&deps, &run, &cp).await.unwrap();

        let run = deps.store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(run.agent_profile.as_deref(), Some("codex"));
        let wt = PathBuf::from(run.worktree_path.unwrap());
        let profile = std::fs::read_to_string(wt.join("profile.txt")).unwrap();
        assert_eq!(profile.trim(), "codex");
    }

    #[tokio::test]
    async fn worktree_setup_required_failure_fails_prepare_worktree() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let worktree_root = tempfile::tempdir().unwrap();

        let (deps, run) = make_deps(
            repo.path().to_path_buf(),
            worktree_root.path().to_path_buf(),
            crate::config::WorktreeSetupConfig {
                commands: vec!["exit 1".into()],
                required: true,
                ..Default::default()
            },
        );
        let cp = Checkpoint::default();

        let err = create_branch_worktree(&deps, &run, &cp).await.unwrap_err();
        assert!(err.to_string().contains("worktree_setup"), "{err}");
    }

    #[tokio::test]
    async fn worktree_setup_optional_failure_warns_and_stops_remaining_commands() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let worktree_root = tempfile::tempdir().unwrap();

        let (deps, run) = make_deps(
            repo.path().to_path_buf(),
            worktree_root.path().to_path_buf(),
            crate::config::WorktreeSetupConfig {
                commands: vec!["exit 1".into(), "echo late > marker.txt".into()],
                required: false,
                ..Default::default()
            },
        );
        let cp = Checkpoint::default();

        // Soft failure: the run continues (Ok), but the second command never
        // runs because the first one failed.
        create_branch_worktree(&deps, &run, &cp).await.unwrap();

        let run = deps.store.get_run(&run.id).unwrap().unwrap();
        let wt = PathBuf::from(run.worktree_path.unwrap());
        assert!(!wt.join("marker.txt").exists());

        let events = deps.store.events_for_run(&run.id, 20).unwrap();
        assert!(
            events.iter().any(|e| e.kind == "worktree_setup.failed"),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn worktree_setup_timeout_kills_the_child_process() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let worktree_root = tempfile::tempdir().unwrap();

        let (deps, run) = make_deps(
            repo.path().to_path_buf(),
            worktree_root.path().to_path_buf(),
            crate::config::WorktreeSetupConfig {
                commands: vec!["sleep 5 && echo late > marker.txt".into()],
                timeout_secs: 1,
                ..Default::default()
            },
        );
        let cp = Checkpoint::default();

        // The command is killed at its 1s timeout, so prepare-worktree returns
        // in ~1s (plus git-worktree overhead) rather than blocking for the
        // command's full 5s sleep. The 4s budget is deliberately loose: it
        // still catches a regression that awaits the whole 5s, but leaves ample
        // headroom for git ops under heavy parallel-test load (the tight 2s
        // budget here used to flake).
        let start = std::time::Instant::now();
        create_branch_worktree(&deps, &run, &cp).await.unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(4),
            "prepare-worktree must not block past the command's timeout"
        );

        let run = deps.store.get_run(&run.id).unwrap().unwrap();
        let wt = PathBuf::from(run.worktree_path.unwrap());

        // Wait past when the 5s sleep would have finished had it survived the
        // timeout; if `kill_on_drop` didn't actually kill it, marker.txt
        // would show up here.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        assert!(
            !wt.join("marker.txt").exists(),
            "the timed-out command must be killed, not left running in the background"
        );
    }

    #[tokio::test]
    async fn worktree_setup_reruns_on_attach_reuse() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let worktree_root = tempfile::tempdir().unwrap();

        // The setup command overwrites the marker each time it runs.
        let (deps, run) = make_deps(
            repo.path().to_path_buf(),
            worktree_root.path().to_path_buf(),
            crate::config::WorktreeSetupConfig {
                commands: vec!["echo ran > marker.txt".into()],
                ..Default::default()
            },
        );
        let mut cp = Checkpoint::default();
        create_branch_worktree(&deps, &run, &cp).await.unwrap();
        let run = deps.store.get_run(&run.id).unwrap().unwrap();
        let branch = run.branch.clone().unwrap();
        let wt = PathBuf::from(run.worktree_path.clone().unwrap());
        cp.head_branch = Some(branch.clone());

        // Simulate the marker being wiped (as `attach_worktree` would if it
        // re-pointed the checkout) and re-attach the same run's worktree:
        // the hook must run again, not just on first creation.
        std::fs::remove_file(wt.join("marker.txt")).unwrap();
        let mut run = run;
        run.branch = Some(branch);
        attach_pr_worktree(&deps, &run, &cp).await.unwrap();

        assert!(wt.join("marker.txt").exists());
    }

    /// A minimal worker flavor whose only meaningful method is `pr_title` —
    /// enough to exercise `publish_needs_human_draft` (issue #209).
    struct DraftFlavor;

    #[async_trait::async_trait]
    impl Flavor for DraftFlavor {
        fn trigger_label(&self) -> &'static str {
            ""
        }
        fn execute_prompt(&self, _: &Deps, _: &RunRecord, _: &Checkpoint, _: &Path) -> String {
            String::new()
        }
        fn verify_work(
            &self,
            _: &RunRecord,
            _: &Checkpoint,
            _: &Path,
        ) -> std::result::Result<(), String> {
            Ok(())
        }
        fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String {
            format!("draft: {} (#{})", cp.issue_title, run.issue_number)
        }
        async fn settle_labels(&self, _: &Deps, _: &RunRecord, _: &Checkpoint) -> Result<()> {
            Ok(())
        }
    }

    /// Build a real repo + worktree (optionally ahead of base, optionally wired
    /// to a bare origin) plus a Deps over a FakeForge, and a run whose
    /// branch/worktree point at it. The returned TempDirs must be kept alive by
    /// the caller for the duration of the test.
    async fn draft_env(
        ahead: bool,
        with_origin: bool,
    ) -> (
        Deps,
        std::sync::Arc<crate::forge::fake::FakeForge>,
        String,
        PathBuf,
        Vec<tempfile::TempDir>,
    ) {
        use crate::config::{Config, ProjectConfig};
        use crate::gitops;
        use crate::store::Store;

        let repo = tempfile::tempdir().unwrap();
        gitops::run_git(repo.path(), &["init", "-b", "main"])
            .await
            .unwrap();
        gitops::run_git(repo.path(), &["config", "user.email", "t@example.com"])
            .await
            .unwrap();
        gitops::run_git(repo.path(), &["config", "user.name", "t"])
            .await
            .unwrap();
        std::fs::write(repo.path().join("seed.txt"), "seed").unwrap();
        gitops::run_git(repo.path(), &["add", "."]).await.unwrap();
        gitops::run_git(repo.path(), &["commit", "-m", "init"])
            .await
            .unwrap();

        let origin = tempfile::tempdir().unwrap();
        if with_origin {
            let origin_str = origin.path().to_string_lossy().to_string();
            gitops::run_git(repo.path(), &["clone", "--bare", ".", &origin_str])
                .await
                .unwrap();
            gitops::run_git(repo.path(), &["remote", "add", "origin", &origin_str])
                .await
                .unwrap();
            gitops::run_git(repo.path(), &["fetch", "origin"])
                .await
                .unwrap();
        }

        let wt_root = tempfile::tempdir().unwrap();
        let branch = "meguri/209-draft-test".to_string();
        let wt = gitops::worktree_path(wt_root.path(), "proj", &branch);
        gitops::create_worktree(repo.path(), &wt, &branch, "main", &[])
            .await
            .unwrap();
        if ahead {
            std::fs::write(wt.join("work.txt"), "wip").unwrap();
            gitops::run_git(&wt, &["add", "."]).await.unwrap();
            gitops::run_git(&wt, &["commit", "-m", "wip"])
                .await
                .unwrap();
        }

        let store = Store::open_in_memory().unwrap();
        let run = store
            .create_run_for_loop("proj", super::super::worker::KIND, 209, "Add caching")
            .unwrap();
        let run_id = run.id.clone();
        store
            .update_run_worktree(&run_id, &branch, &wt.to_string_lossy())
            .unwrap();

        let project = ProjectConfig {
            id: "proj".into(),
            repo_path: Some(repo.path().to_path_buf()),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            worktree_root: Some(wt_root.path().to_path_buf()),
            pr: None,
            clean: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup: Default::default(),
            schedules: Vec::new(),
            cadence: Vec::new(),
            prompts: Default::default(),
            triage: None,
            autonomy: None,
            notify: None,
        };
        let forge = std::sync::Arc::new(crate::forge::fake::FakeForge::default());
        let deps = Deps::with_label_source(
            store,
            std::sync::Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge.clone(),
            Config::default(),
            project,
        );
        (deps, forge, run_id, wt, vec![repo, origin, wt_root])
    }

    /// Escalating with committed work ahead of base opens exactly one draft PR
    /// labeled `meguri:needs-human` — at creation, not via a follow-up call —
    /// and emits `self_review.escalated_draft` (never `pr.created`).
    #[tokio::test]
    async fn escalate_publishes_needs_human_draft_when_ahead() {
        let (deps, forge, run_id, wt, _tmp) = draft_env(true, true).await;
        let run = deps.store.get_run(&run_id).unwrap().unwrap();
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            summary: "書きかけの要約".into(),
            ..Default::default()
        };

        publish_needs_human_draft(&deps, &run, &cp, &wt, &DraftFlavor).await;

        let prs = forge.prs();
        assert_eq!(prs.len(), 1, "exactly one draft opened");
        let pr = &prs[0];
        assert!(pr.draft, "opened as a draft");
        assert!(
            pr.labels
                .iter()
                .any(|l| l == crate::forge::LABEL_NEEDS_HUMAN),
            "labeled needs-human at creation (not a separate add_pr_label): {:?}",
            pr.labels
        );
        assert_eq!(pr.head, "meguri/209-draft-test");
        assert_eq!(pr.base, "main");
        assert!(pr.body.contains("Refs #209"), "links without closing");
        assert!(
            pr.body.contains("証拠物件"),
            "body flags it as unconverged evidence: {}",
            pr.body
        );

        let kinds: Vec<String> = deps
            .store
            .events_for_run(&run_id, 50)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(
            kinds.iter().any(|k| k == "self_review.escalated_draft"),
            "{kinds:?}"
        );
        assert!(
            !kinds.iter().any(|k| k == "pr.created"),
            "an evidence draft must not count as a delivered PR: {kinds:?}"
        );
    }

    /// Resume-safety: a second escalation of the same run (a crash after create,
    /// or the same NeedsHuman path re-running on resume from STEP_SELF_REVIEW)
    /// must NOT open a duplicate draft — it adopts the existing one.
    #[tokio::test]
    async fn escalate_is_idempotent_across_reruns() {
        let (deps, forge, run_id, wt, _tmp) = draft_env(true, true).await;
        let run = deps.store.get_run(&run_id).unwrap().unwrap();
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        };

        publish_needs_human_draft(&deps, &run, &cp, &wt, &DraftFlavor).await;
        publish_needs_human_draft(&deps, &run, &cp, &wt, &DraftFlavor).await;

        assert_eq!(
            forge.prs().len(),
            1,
            "second run must not duplicate the draft"
        );
        let kinds: Vec<String> = deps
            .store
            .events_for_run(&run_id, 50)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            kinds
                .iter()
                .filter(|k| *k == "self_review.escalated_draft")
                .count(),
            1,
            "only the first run creates: {kinds:?}"
        );
        assert!(
            kinds
                .iter()
                .any(|k| k == "self_review.escalated_draft_exists"),
            "the re-run adopts the existing draft: {kinds:?}"
        );
    }

    /// No commits ahead → nothing to show → comment-only (no PR).
    #[tokio::test]
    async fn escalate_stays_comment_only_when_not_ahead() {
        let (deps, forge, run_id, wt, _tmp) = draft_env(false, true).await;
        let run = deps.store.get_run(&run_id).unwrap().unwrap();
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        };
        publish_needs_human_draft(&deps, &run, &cp, &wt, &DraftFlavor).await;
        assert!(forge.prs().is_empty(), "nothing committed → no draft");
    }

    /// A failed push falls back to comment-only (best-effort): no PR, and the
    /// failure is recorded rather than silently swallowed.
    #[tokio::test]
    async fn escalate_falls_back_to_comment_only_on_push_failure() {
        // No origin remote wired → `git push origin` fails.
        let (deps, forge, run_id, wt, _tmp) = draft_env(true, false).await;
        let run = deps.store.get_run(&run_id).unwrap().unwrap();
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        };
        publish_needs_human_draft(&deps, &run, &cp, &wt, &DraftFlavor).await;
        assert!(forge.prs().is_empty(), "push failed → no draft");
        let kinds: Vec<String> = deps
            .store
            .events_for_run(&run_id, 50)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert!(
            kinds.iter().any(|k| k == "self_review.draft_push_failed"),
            "{kinds:?}"
        );
    }
}
