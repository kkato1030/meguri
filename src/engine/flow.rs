//! The shared issue→PR flow every loop drives: claim the labeled issue →
//! worktree → interactive agent turns in a mux pane → verified commits → PR.
//! Steps are checkpointed in `runs.step` so an interrupted run resumes where
//! it left off. Loop-specific behavior (trigger label, prompts, extra
//! verification, PR shape, label settling) plugs in via [`Flavor`].

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{Deps, StoreControl, Target, WorkerOutcome};
use crate::forge;
use crate::gitops;
use crate::mux::{PaneId, PaneSpec};
use crate::store::{RunRecord, RunStatus};
use crate::turn::{TurnConfig, TurnEngine, TurnOutcome, TurnStatus, prepare_turn};

pub const STEP_PREPARE_WORK: &str = "prepare-work";
pub const STEP_PREPARE_WORKTREE: &str = "prepare-worktree";
pub const STEP_EXECUTE: &str = "execute";
pub const STEP_VALIDATE: &str = "validate";
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

    /// Claim the run's target and fill the checkpoint. Default: re-verify
    /// the trigger label on the issue, then claim it with `meguri:working`.
    async fn prepare_work(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &mut Checkpoint,
    ) -> Result<PreparedWork> {
        claim_issue(deps, run, self.trigger_label(), cp).await
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
    fn verify_work(&self, run: &RunRecord, worktree: &Path) -> std::result::Result<(), String>;

    /// Base ref the execute step counts commits against. Default: the
    /// project's default branch (new-branch loops); the fixer counts against
    /// the PR branch's pushed tip instead.
    fn verify_base(&self, deps: &Deps, run: &RunRecord) -> String {
        let _ = run;
        deps.project.default_branch.clone()
    }

    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String;

    /// Settle forge labels once the PR exists. Re-run on resume, so keep it
    /// idempotent.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()>;

    /// Release the claim marker on `meguri stop`. Default: the issue's
    /// `meguri:working` label.
    async fn release_claim(&self, deps: &Deps, run: &RunRecord) {
        deps.forge
            .remove_label(run.issue_number, forge::LABEL_WORKING)
            .await
            .ok();
    }

    /// Failure escalation ("Authority": the durable record of why the run
    /// stopped lives on the forge). Default: label + comment on the issue.
    async fn escalate(&self, deps: &Deps, run: &RunRecord, reason: &str) {
        escalate_on_forge(deps, run.issue_number, reason).await;
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
    /// Agent-authored PR description (Markdown) from the verified execute turn.
    #[serde(default)]
    pub pr_body: Option<String>,
    /// Existing PR head branch to attach to (fixer runs).
    #[serde(default)]
    pub head_branch: Option<String>,
    /// Review threads the run set out to address (fixer runs); replied to
    /// after the push.
    #[serde(default)]
    pub thread_ids: Vec<String>,
}

/// Error kind signalling "a human needs to look"; the run is failed on the
/// forge with the needs-human label and an explanatory comment.
#[derive(Debug, thiserror::Error)]
#[error("needs human: {0}")]
pub struct NeedsHuman(pub String);

/// Shared discovery: open issues carrying `label` that are actionable — not
/// held, not claimed by another host, not already shipped by a succeeded run
/// of this loop (avoids duplicate PRs when the trigger label lingers or
/// reappears; humans can force a rerun with `meguri run --issue N`), and not
/// gated by an unresolved `blocked_by` dependency.
pub async fn discover_by_label(deps: &Deps, loop_kind: &str, label: &str) -> Result<Vec<Target>> {
    let issues = deps.forge.list_issues_with_label(label).await?;
    let mut targets = Vec::new();
    for issue in issues {
        if issue.has_label(forge::LABEL_HOLD) || issue.has_label(forge::LABEL_WORKING) {
            continue;
        }
        if deps
            .store
            .issue_has_succeeded_run(&deps.project.id, loop_kind, issue.number)?
        {
            continue;
        }
        if has_unresolved_blockers(deps, issue.number).await {
            continue;
        }
        targets.push(Target {
            issue_number: issue.number,
            title: issue.title,
        });
    }
    Ok(targets)
}

/// Dependency gate (looper ADR-0004): GitHub-native `blocked_by` is the
/// authority. Only a blocker closed as completed resolves; open blockers,
/// not_planned/duplicate closes, and blockers we cannot read all keep the
/// issue out of discovery. The skip is silent — no label, no comment: the
/// dependency graph on the forge already tells a human why nothing starts.
async fn has_unresolved_blockers(deps: &Deps, issue: i64) -> bool {
    match deps.forge.blocked_by(issue).await {
        Ok(blockers) => blockers.iter().any(|b| !b.resolved()),
        Err(_) => true,
    }
}

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

    match drive(deps, &run, flavor).await {
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

    if step == STEP_EXECUTE {
        match execute(deps, &run, &mut checkpoint, &worktree, flavor).await? {
            StepFlow::Continue => {}
            StepFlow::Stopped => return Ok(WorkerOutcome::Stopped),
            StepFlow::Interrupted(r) => return Ok(WorkerOutcome::Interrupted(r)),
        }
        step = save_step(deps, &run, STEP_VALIDATE, &checkpoint)?;
    }

    if step == STEP_VALIDATE {
        match validate(deps, &run, &mut checkpoint, &worktree).await? {
            StepFlow::Continue => {}
            StepFlow::Stopped => return Ok(WorkerOutcome::Stopped),
            StepFlow::Interrupted(r) => return Ok(WorkerOutcome::Interrupted(r)),
        }
        step = save_step(deps, &run, STEP_OPEN_PR, &checkpoint)?;
    }

    if step == STEP_OPEN_PR {
        let pr_url = open_pr(deps, &run, &mut checkpoint, &worktree, flavor).await?;
        cleanup_pane(deps, &run, true).await;
        return Ok(WorkerOutcome::Succeeded { pr_url });
    }

    bail!("unknown step {step:?}");
}

pub(crate) enum StepFlow {
    Continue,
    Stopped,
    Interrupted(String),
}

/// Apply the keep_pane policy after a run reaches a terminal state.
pub(crate) async fn cleanup_pane(deps: &Deps, run: &RunRecord, success: bool) {
    // The caller's record may predate the pane spawn (execute updates the
    // store, not the in-memory run) — read the current pane id.
    let pane_id = deps
        .store
        .get_run(&run.id)
        .ok()
        .flatten()
        .and_then(|r| r.mux_pane_id)
        .or_else(|| run.mux_pane_id.clone());
    let Some(pane_id) = &pane_id else {
        return;
    };
    let keep = match deps.config.mux.keep_pane.as_str() {
        "always" => true,
        "never" => false,
        _ => !success, // "on-failure": keep only when something went wrong
    };
    if !keep {
        let _ = deps.mux.kill_pane(&PaneId(pane_id.clone())).await;
    }
}

/// `meguri stop`: cancel the run, release the claim, kill the pane.
async fn finalize_cancelled(deps: &Deps, run: &RunRecord, flavor: &dyn Flavor) -> Result<()> {
    deps.store
        .update_run_status(&run.id, RunStatus::Cancelled, None)?;
    flavor.release_claim(deps, run).await;
    if let Some(pane_id) = &run.mux_pane_id {
        let _ = deps.mux.kill_pane(&PaneId(pane_id.clone())).await;
    }
    deps.store.emit(Some(&run.id), "run.cancelled", json!({}))?;
    Ok(())
}

/// Failure escalation on the forge ("Authority": the durable record of why
/// the run stopped lives on the issue, not in meguri's local state).
pub(crate) async fn escalate_on_forge(deps: &Deps, issue: i64, reason: &str) {
    let _ = deps.forge.add_label(issue, forge::LABEL_NEEDS_HUMAN).await;
    let _ = deps.forge.remove_label(issue, forge::LABEL_WORKING).await;
    let _ = deps
        .forge
        .comment(
            issue,
            &format!(
                "🔁 **meguri** could not finish this issue and needs a human.\n\n> {reason}\n\n\
                 The agent's pane (if still open) has the full context — \
                 see `meguri ps` / `meguri attach` on the host running meguri."
            ),
        )
        .await;
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

/// Default prepare-work: re-verify labels on the forge, then claim with
/// `meguri:working` (the durable claim marker). A hold or missing trigger
/// label here is a benign race (the issue changed between discovery and
/// claim, e.g. another run just shipped it) — skip, don't escalate.
async fn claim_issue(
    deps: &Deps,
    run: &RunRecord,
    trigger_label: &str,
    cp: &mut Checkpoint,
) -> Result<PreparedWork> {
    let issue = deps.forge.get_issue(run.issue_number).await?;
    if issue.has_label(forge::LABEL_HOLD) {
        return Ok(PreparedWork::Skip(format!(
            "issue #{} is on hold ({})",
            issue.number,
            forge::LABEL_HOLD
        )));
    }
    if !issue.has_label(trigger_label) {
        return Ok(PreparedWork::Skip(format!(
            "issue #{} is not labeled {trigger_label} (removed since discovery?)",
            issue.number,
        )));
    }
    deps.forge
        .add_label(issue.number, forge::LABEL_WORKING)
        .await?;
    // A fresh claim supersedes a previous run's escalation: the human is no
    // longer needed while this run is in flight (and a new failure re-adds
    // the label). No-op if absent; best-effort like the escalation side.
    let _ = deps
        .forge
        .remove_label(issue.number, forge::LABEL_NEEDS_HUMAN)
        .await;
    deps.store.emit(
        Some(&run.id),
        "issue.claimed",
        json!({ "issue": issue.number }),
    )?;
    cp.issue_title = issue.title;
    cp.issue_body = issue.body;
    Ok(PreparedWork::Claimed)
}

/// Default prepare-worktree: a new run-scoped branch off the default branch.
async fn create_branch_worktree(deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
    let branch = run
        .branch
        .clone()
        .unwrap_or_else(|| gitops::branch_name(run.issue_number, &cp.issue_title, &run.id));
    let root = deps
        .project
        .worktree_root
        .clone()
        .unwrap_or_else(crate::config::worktrees_root);
    let wt = gitops::worktree_path(&root, &deps.project.id, &branch);
    gitops::create_worktree(
        &deps.project.repo_path,
        &wt,
        &branch,
        &deps.project.default_branch,
    )
    .await?;
    deps.store
        .update_run_worktree(&run.id, &branch, &wt.to_string_lossy())?;
    deps.store.emit(
        Some(&run.id),
        "worktree.created",
        json!({ "branch": branch, "path": wt.to_string_lossy() }),
    )?;
    Ok(())
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
    gitops::attach_worktree(&deps.project.repo_path, &wt, &branch).await?;
    deps.store
        .update_run_worktree(&run.id, &branch, &wt.to_string_lossy())?;
    deps.store.emit(
        Some(&run.id),
        "worktree.attached",
        json!({ "branch": branch, "path": wt.to_string_lossy() }),
    )?;
    Ok(())
}

fn turn_engine(deps: &Deps) -> TurnEngine {
    TurnEngine {
        mux: deps.mux.clone(),
        cfg: TurnConfig::from_limits(&deps.config.limits),
    }
}

/// Get the run's pane, spawning it (with the trigger as the agent's initial
/// prompt argument) if it doesn't exist or died.
async fn ensure_pane(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    initial_trigger: &str,
) -> Result<(PaneId, bool)> {
    if let Some(id) = &run.mux_pane_id {
        let pane = PaneId(id.clone());
        if run.mux_kind.as_deref() == Some(deps.mux.kind().as_str())
            && deps.mux.pane_alive(&pane).await.unwrap_or(false)
        {
            return Ok((pane, false));
        }
    }

    deps.mux.ensure_session().await?;
    let mut command = vec![deps.config.agent.command.clone()];
    command.extend(deps.config.agent.args.iter().cloned());
    command.push(initial_trigger.to_string());

    let mut env = Vec::new();
    if let Some(hint) = &deps.config.agent.herdr_agent_hint {
        env.push(("HERDR_AGENT".to_string(), hint.clone()));
    }

    let pane = deps
        .mux
        .spawn_pane(&PaneSpec {
            title: format!("meguri#{}", run.issue_number),
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
    deps.store.emit(
        Some(&run.id),
        "pane.spawned",
        json!({ "pane": pane.0, "mux": deps.mux.kind().as_str(),
                "attach": deps.mux.attach_command(&pane) }),
    )?;
    Ok((pane, true))
}

/// Run one prompt-turn: prepare files, deliver the trigger (spawn or
/// send_line), then wait it out.
pub(crate) async fn run_turn(
    deps: &Deps,
    run: &RunRecord,
    worktree: &Path,
    purpose: &str,
    prompt_body: &str,
) -> Result<(TurnOutcome, String)> {
    let prepared = prepare_turn(worktree, prompt_body)?;
    let (pane, freshly_spawned) = ensure_pane(deps, run, worktree, &prepared.trigger_line).await?;
    deps.store.begin_turn(
        &run.id,
        &prepared.turn_id,
        purpose,
        &prepared.prompt_path.to_string_lossy(),
    )?;
    if !freshly_spawned {
        deps.mux.send_line(&pane, &prepared.trigger_line).await?;
    }

    let control = StoreControl {
        store: deps.store.clone(),
        run_id: run.id.clone(),
    };
    let engine = turn_engine(deps);
    let outcome = engine
        .await_completion(&pane, worktree, &prepared.turn_id, &control)
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
            flavor.verify_work(run, worktree).err()
        };
        let Some(problem) = problem else {
            // Keep what the agent said for the PR body (persisted by the
            // caller's step save).
            cp.summary = result.summary;
            cp.pr_body = result.pr_body;
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
/// feeds failures back to the agent, never trusting agent claims.
async fn validate(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    worktree: &Path,
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
        save_step(deps, run, STEP_VALIDATE, cp)?;
        if cp.fix_turns_used > deps.config.limits.validate_turns {
            return Err(NeedsHuman(format!(
                "validation `{check}` still failing after {} fix turns",
                cp.fix_turns_used - 1
            ))
            .into());
        }

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
                TurnStatus::Success => continue,
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

    let pr_url = if let Some(url) = &cp.pr_url {
        url.clone() // resumed after PR creation
    } else {
        let title = flavor.pr_title(run, cp);
        let description = cp
            .pr_body
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| cp.summary.trim());
        let body = format!(
            "Closes #{}.\n\n{}\n\n---\n🔁 Opened by [meguri](https://github.com/kkato1030/meguri) \
             from an interactive agent session (run `{}`).",
            run.issue_number, description, run.id
        );
        let draft = deps.config.pr_for(&deps.project).draft;
        let pr = deps
            .forge
            .create_pr(&branch, &deps.project.default_branch, &title, &body, draft)
            .await?;
        cp.pr_url = Some(pr.url.clone());
        cp.pr_number = Some(pr.number);
        save_step(deps, run, STEP_OPEN_PR, cp)?;
        deps.store
            .emit(Some(&run.id), "pr.created", json!({ "url": pr.url }))?;
        pr.url
    };

    flavor.settle_labels(deps, run, cp).await?;
    Ok(pr_url)
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
}
