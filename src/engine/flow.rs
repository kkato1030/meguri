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

use super::{Deps, MEGURI_BRANCH_PREFIX, StoreControl, WorkerOutcome, lane_for_loop};
use crate::agent_session;
use crate::config::{Deliver, LaunchMode};
use crate::forge;
use crate::gitops;
use crate::launch;
use crate::mux::{PaneId, PaneSpec};
use crate::routing;
use crate::store::{LANE_AUTHOR, RunRecord, RunStatus};
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
            }
            Ok(outcome)
        }
        Err(e) => {
            let msg = format!("{e:#}");
            deps.store
                .update_run_status(run_id, RunStatus::Failed, Some(&msg))?;
            deps.store
                .emit(Some(run_id), "run.failed", json!({ "error": msg }))?;
            // A forge/mux command fault (stopped mux, dropped connection) says
            // nothing about the issue — release the claim so the next sweep
            // just retries, instead of parking it on needs-human (issue #250).
            match super::escalation::infra_reason(&e) {
                Some(reason) => {
                    flavor.release_claim(deps, &run).await;
                    super::escalation::escalate_infra(deps, &run, reason, &msg).await;
                }
                None => flavor.escalate(deps, &run, &msg).await,
            }
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
        match execute(deps, &run, &mut checkpoint, &worktree, flavor).await? {
            StepFlow::Continue => {}
            StepFlow::Stopped => return Ok(WorkerOutcome::Stopped),
            StepFlow::Interrupted(r) => return Ok(WorkerOutcome::Interrupted(r)),
        }
        step = save_step(deps, &run, STEP_VALIDATE, &checkpoint)?;
    }

    if step == STEP_VALIDATE {
        match validate(deps, &run, &mut checkpoint, &worktree, STEP_VALIDATE).await? {
            StepFlow::Continue => {}
            StepFlow::Stopped => return Ok(WorkerOutcome::Stopped),
            StepFlow::Interrupted(r) => return Ok(WorkerOutcome::Interrupted(r)),
        }
        step = save_step(deps, &run, STEP_SELF_REVIEW, &checkpoint)?;
    }

    if step == STEP_SELF_REVIEW {
        // The internal self-review phase is dormant (docs/adr/STATUS.md); the
        // step id is kept so an in-flight checkpoint resumes cleanly.
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

/// Loop kinds whose `Flavor` claims a PR by adding `meguri:working` to it
/// (mirrors the `Flavor::release_claim` overrides in each of these modules).
/// `worker`/`planner` also stamp `Checkpoint::pr_number` once their PR opens,
/// but that PR is claimed through the *issue's* label, never the PR's — so
/// they must NOT be treated as PR claimers here (issue #252 review finding
/// f1): an interrupted worker run stopped after some other loop later
/// claimed its PR would otherwise strip that unrelated run's live claim.
fn claims_pr_by_label(_loop_kind: &str) -> bool {
    false
}

/// `meguri stop` on a run with no live driver (queued/interrupted — issue
/// #252): `cmd_stop` finalizes right there in the CLI process, without ever
/// reaching the loop's `Flavor`, so it cannot call `Flavor::release_claim`
/// the way [`finalize_cancelled`] (a live driver noticing `desired_state`)
/// does. A PR-claiming loop's [`Checkpoint::pr_number`] is loop-agnostic
/// (every `Flavor` that claims a PR stores it there), so this reads straight
/// from the checkpoint and drops `meguri:working` from that PR itself —
/// gated to loops that actually claim by PR label ([`claims_pr_by_label`]).
/// Best-effort and a no-op when the run never claimed a PR, isn't a
/// PR-claiming loop, or this project has no forge (local mode — review
/// finding f2: `Deps::forge` panics on `None`, and a local-mode project can
/// still have a leftover github-mode checkpoint after a mode switch).
/// Issue-claim loops release through the task source instead; a failed
/// removal is still swept up by #223's run-liveness check on the next
/// discovery.
pub(crate) async fn release_stray_pr_claim(deps: &Deps, run: &RunRecord) {
    if !claims_pr_by_label(&run.loop_kind) {
        return;
    }
    let Some(forge) = deps.forge.as_ref() else {
        return;
    };
    if let Some(pr) = claimed_pr(deps, &run.id) {
        let _ = forge.remove_pr_label(pr, forge::LABEL_WORKING).await;
    }
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
    match deps.task_source.claim(&key, tasks::LOCAL_HOST).await? {
        // Arm-tagged re-verification (ADR 0012 S4 決定1): the claim must still
        // match the arm this run was enqueued as. A phase label that moved
        Some(task) => {
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
            // Adopt gate (issue #245): "pane alive" is not "agent alive". A
            // pane whose agent exited to a bare shell would swallow the
            // trigger line as a shell command, so a definite absent retires
            // the pane (kill + reclaim) and falls through to resume/fresh.
            // None (mux can't tell) adopts as before — fail open.
            if matches!(deps.mux.agent_present(&pane).await, Ok(Some(false))) {
                deps.store.emit(
                    Some(&run.id),
                    "pane.agent_absent",
                    json!({ "pane": pane.0, "lane": lane_name }),
                )?;
                super::reaper::release_pane(deps, run.issue_number, lane_name, "agent absent")
                    .await;
            } else if record.worktree_path.as_deref() == Some(worktree_str.as_ref()) {
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
            } else {
                // The lane moved to another worktree (e.g. a fresh branch):
                // the old pane can't see it. Retire it — session id saved —
                // and respawn below (the saved id makes the respawn a resume,
                // so the context follows the lane into the new worktree).
                super::reaper::release_pane(deps, run.issue_number, lane_name, "worktree moved")
                    .await;
            }
        }
    }

    deps.mux.ensure_session().await?;

    // The lane's resumable context lives on the pane row (issue lifetime),
    // not the ephemeral run: written after every completed turn and before
    // every reclamation. Resuming it is conditional on the session still
    // being conversable (issue #245) — the transcript-size gate clears a
    // session that would only produce context-window 400s.
    let session_id = gated_resume_session(deps, run, worktree, lane).await?;
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
    // Disabled in the test harness so a FakeMux run never fires a real prime
    // subprocess against the default `claude` command (issue #235).
    if !deps.preflight_enabled {
        return;
    }
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

/// The lane's saved session id, after the resume-health gate (issue #245):
/// resume must mean "the conversation is still alive", not just "an id is on
/// record". A transcript past the profile's size limit is the signature of a
/// session at its context limit — resuming it yields nothing but API 400s —
/// so the gate retires the pane, clears the id, and lets the caller fall back
/// to a fresh spawn with full re-injection (prompts are self-contained).
///
/// The gate is the single size authority and resolves the transcript root
/// from the lane's pinned profile (spec f4) — id *acquisition* elsewhere
/// (reaper rescans with the default root) stays best-effort precisely because
/// any stale/mis-rooted id saved there is re-measured here before a resume.
/// An unlocatable transcript fails open (`pane.resume_gate_skipped`): the
/// quiet-strike ladder is the backstop for dead sessions it lets through.
async fn gated_resume_session(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    lane: &Lane,
) -> Result<Option<String>> {
    let lane_name = lane.lane.as_str();
    let Some(record) = deps
        .store
        .get_pane(&deps.project.id, run.issue_number, lane_name)?
    else {
        return Ok(None);
    };
    let Some(session_id) = record.agent_session_id else {
        return Ok(None);
    };
    let limit = lane.profile.resume_transcript_limit_bytes;
    if limit == 0 {
        return Ok(Some(session_id));
    }
    // The transcript lives under the cwd the session actually ran in: the
    // pane row's worktree when recorded (the lane may have moved since),
    // else the current worktree.
    let session_worktree = record
        .worktree_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| worktree.to_path_buf());
    let root = agent_session::session_root(&lane.profile);
    match agent_session::transcript_len(&root, &session_worktree, &session_id) {
        None => {
            deps.store.emit(
                Some(&run.id),
                "pane.resume_gate_skipped",
                json!({ "reason": "transcript_not_found",
                        "lane": lane_name, "agent_session_id": session_id }),
            )?;
            Ok(Some(session_id))
        }
        Some(bytes) if bytes > limit => {
            // Kill + reclaim BEFORE clearing the id: `release_pane` re-saves
            // the freshest transcript id as its reversibility net, so the
            // clear must come after to win. A pane-less lane makes the
            // release a no-op and the clear still lands.
            super::reaper::release_pane(deps, run.issue_number, lane_name, "transcript oversize")
                .await;
            deps.store
                .save_pane_session(&deps.project.id, run.issue_number, lane_name, None)?;
            deps.store.update_run_agent_session(&run.id, None)?;
            deps.store.emit(
                Some(&run.id),
                "agent_session.cleared",
                json!({ "reason": "transcript_oversize", "lane": lane_name,
                        "bytes": bytes, "limit": limit,
                        "agent_session_id": session_id }),
            )?;
            Ok(None)
        }
        Some(_) => Ok(Some(session_id)),
    }
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
            let name =
                crate::routing::resolve(&deps.config, role, &crate::routing::detect_command)?;
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

    record_agent_session(deps, run, worktree, lane, pane.as_ref(), resumed, &outcome).await?;

    let (outcome_str, result_json) = match &outcome {
        TurnOutcome::Completed(r) => (
            format!("{:?}", r.status).to_lowercase(),
            Some(serde_json::to_string(&json!({
                "turn_id": r.turn_id, "summary": r.summary,
            }))?),
        ),
        TurnOutcome::Stopped => ("stopped".to_string(), None),
        TurnOutcome::PaneDied => ("pane_died".to_string(), None),
        TurnOutcome::AgentQuiet { .. } => ("agent_quiet".to_string(), None),
    };
    deps.store
        .finish_turn(&prepared.turn_id, &outcome_str, result_json.as_deref())?;

    // Normalize AgentQuiet here, at the single flow choke point (issue #245):
    // callers keep seeing only Completed/Stopped/PaneDied. The strike ladder
    // decides between "retry" (PaneDied → the usual Interrupted redispatch)
    // and "hand to a human" (Err(NeedsHuman) → the flavor's escalate path).
    if let TurnOutcome::AgentQuiet { tail } = outcome {
        let outcome = handle_agent_quiet(deps, run, lane, &prepared.turn_id, tail).await?;
        return Ok((outcome, prepared.turn_id));
    }
    Ok((outcome, prepared.turn_id))
}

/// Strikes of consecutive agent_quiet before the session is rotated: the
/// first quiet gets one more resume (a one-off hiccup deserves grace), the
/// second proves the session dead.
const QUIET_STRIKE_CLEAR: i64 = 2;
/// Strikes before a human takes over: quiet even after a fresh session means
/// the environment (profile, CLI, credentials), not the session, is broken —
/// more rotations would loop forever.
const QUIET_STRIKE_HUMAN: i64 = 3;

/// The quiet-strike ladder (issue #245): count consecutive quiet turns per
/// lane, rotate the session at [`QUIET_STRIKE_CLEAR`], escalate at
/// [`QUIET_STRIKE_HUMAN`]. Owns all session/pane state for the quiet path —
/// `record_agent_session` never sees an `AgentQuiet` (it no-ops on it), so
/// the two can't double-clear. The counter is reset only by a completed turn;
/// deliberately NOT by the rotation itself, which would reset the ladder and
/// make the human rung unreachable.
async fn handle_agent_quiet(
    deps: &Deps,
    run: &RunRecord,
    lane: &Lane,
    turn_id: &str,
    tail: Vec<String>,
) -> Result<TurnOutcome> {
    let lane_name = lane.lane.as_str();
    let strikes =
        deps.store
            .bump_pane_quiet_strikes(&deps.project.id, run.issue_number, lane_name)?;
    // Raw tail stays inside the local trust boundary (the events table);
    // anything leaving for the forge below goes through sanitize_pane_tail.
    deps.store.emit(
        Some(&run.id),
        "turn.agent_quiet",
        json!({ "turn_id": turn_id, "lane": lane_name, "strikes": strikes, "tail": tail }),
    )?;

    if strikes >= QUIET_STRIKE_HUMAN {
        let diagnosis = if deps.config.limits.escalation_pane_tail {
            format!(
                "\n\npane tail (diagnosis only — success is still judged by the \
                 result-file contract):\n{}",
                super::escalation::sanitize_pane_tail(&tail)
            )
        } else {
            String::new()
        };
        return Err(NeedsHuman(format!(
            "agent went quiet {strikes} turns in a row on issue #{} ({lane_name} lane) — \
             nudges and a fresh session did not revive it, so the profile/environment \
             looks broken{diagnosis}",
            run.issue_number
        ))
        .into());
    }
    if strikes >= QUIET_STRIKE_CLEAR {
        // Kill + reclaim first, clear the id second (`release_pane` re-saves
        // the freshest transcript id, so the clear must come after): the next
        // dispatch then has nothing to adopt and nothing to resume — a
        // guaranteed fresh spawn with full re-injection.
        super::reaper::release_pane(deps, run.issue_number, lane_name, "quiet loop").await;
        deps.store
            .save_pane_session(&deps.project.id, run.issue_number, lane_name, None)?;
        deps.store.update_run_agent_session(&run.id, None)?;
        deps.store.emit(
            Some(&run.id),
            "agent_session.cleared",
            json!({ "reason": "quiet_loop", "lane": lane_name, "strikes": strikes }),
        )?;
    }
    // Strike 1 keeps pane and session: the next dispatch adopts/resumes and
    // may simply recover. Either way the run takes the usual Interrupted
    // redispatch path.
    Ok(TurnOutcome::PaneDied)
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

    // Same resume-health gate as the pane path (issue #245): an oversized
    // transcript is cleared instead of resumed.
    let session_id = gated_resume_session(deps, run, worktree, lane).await?;

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
///
/// The transcript root comes from the lane's pinned profile (issue #245,
/// spec f4) — a named profile with its own `session_dir` would otherwise
/// have its sessions recorded from (and misjudged against) the default
/// agent's directory. An `AgentQuiet` outcome is deliberately a no-op here:
/// [`handle_agent_quiet`] owns that path's session state.
async fn record_agent_session(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    lane: &Lane,
    pane: Option<&PaneId>,
    resumed: bool,
    outcome: &TurnOutcome,
) -> Result<()> {
    let lane_name = lane.lane.as_str();
    match outcome {
        TurnOutcome::Completed(r) => {
            // The session answered — the lane's quiet-strike ladder resets.
            deps.store
                .reset_pane_quiet_strikes(&deps.project.id, run.issue_number, lane_name)?;
            let session_root = agent_session::session_root(&lane.profile);
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
            // Normalized inside run_turn_in (issue #245); kept for exhaustiveness.
            TurnOutcome::AgentQuiet { .. } => {
                return Ok(StepFlow::Interrupted(
                    "agent went quiet during execute".into(),
                ));
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

        let prompt = format!(
            "The project's validation command failed. Fix the code so it passes, \
             then commit your fixes.\n\nCommand: `{check}`\nExit code: {}\n\n\
             Last stdout:\n```\n{}\n```\n\nLast stderr:\n```\n{}\n```\n\n\
             Do not create a pull request; meguri handles that.",
            out.status.code().unwrap_or(-1),
            tail(&stdout),
            tail(&stderr),
        );
        let (outcome, _) = run_turn(deps, run, worktree, "fix-validation", &prompt).await?;
        match outcome {
            TurnOutcome::Completed(r) => match r.status {
                TurnStatus::Success => {
                    save_step(deps, run, persist_step, cp)?;
                    continue;
                }
                TurnStatus::Failure | TurnStatus::NeedsHuman => {
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
            // Normalized inside run_turn_in (issue #245); kept for exhaustiveness.
            TurnOutcome::AgentQuiet { .. } => {
                return Ok(StepFlow::Interrupted(
                    "agent went quiet during validate".into(),
                ));
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

/// Duplicate-delivery guard (issue #249, design doc §3-D/§P5): right before
/// the worker/planner open a *new* PR for an issue, check whether the forge
/// already cross-references an open PR on a non-meguri branch. If one
/// exists, a human already has (or is running) a rail-external delivery for
/// this issue — opening a second one risks shipping the same change twice
/// under different file names, as #236/#237/#238 did. Only meguri's own
/// branches (`meguri/...`) are exempt: a resumed run's own PR, or a sibling
/// meguri loop's, must never trip this. Local tasks (no github issue) are
/// exempt — there is nothing on a forge to cross-reference.
async fn reject_rail_external_duplicate(deps: &Deps, run: &RunRecord) -> Result<()> {
    let TaskKey::Issue(issue) = run.task_key() else {
        return Ok(());
    };
    let linked = deps.forge().linked_open_prs(issue).await?;
    let mut rail_external = linked
        .iter()
        .filter(|pr| !pr.head_branch.starts_with(MEGURI_BRANCH_PREFIX));
    if let Some(pr) = rail_external.next() {
        return Err(NeedsHuman(format!(
            "issue #{issue} already has an open PR outside meguri's rails \
             (#{} on branch `{}`, {}). adopt it or close it — a human must \
             decide before meguri opens another PR for this issue.",
            pr.number, pr.head_branch, pr.url
        ))
        .into());
    }
    Ok(())
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
    let close = flavor.pr_closes_issue(deps);
    let pr_url = if let Some(url) = &cp.pr_url {
        url.clone() // resumed after PR creation
    } else {
        reject_rail_external_duplicate(deps, run).await?;
        let title = flavor.pr_title(run, cp);
        let body = compose_pr_body(run, cp, close);
        let draft = deps.config.pr_for(&deps.project).draft;
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
        pr.url
    };

    flavor.settle_presentation(deps, run, cp).await?;
    flavor.settle_labels(deps, run, cp).await?;
    Ok(pr_url)
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
pub(crate) fn compose_pr_body(run: &RunRecord, cp: &Checkpoint, close: bool) -> String {
    let description = cp
        .pr_body
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cp.summary.trim());
    format!(
        "{}.\n\n{}\n\n---\n🔁 Opened by [meguri](https://github.com/kkato1030/meguri) \
         from an interactive agent session (run `{}`).",
        issue_reference(run.issue_number, close),
        description,
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

    fn fake_run(issue: i64) -> RunRecord {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store
            .create_run_for_loop("proj", "test-flavor", issue, "t")
            .unwrap();
        store.get_run(&run.id).unwrap().unwrap()
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
            worktree_setup,
            prompts: Default::default(),
        };
        let deps = Deps::with_github_source(
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
}
