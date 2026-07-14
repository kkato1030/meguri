//! The pr-reviewer loop: the optional external GitHub review, symmetric
//! across plan and impl (ADR 0008). One kind-parameterized component
//! supersedes the old `spec_reviewer` (review for the plan/spec PR) and gives
//! the impl PR the same optional review. Its output stays **off the
//! review-thread timeline**:
//!
//! - a `meguri/pr-review` **commit status** on the reviewed head (success =
//!   clean, failure = findings) — the dedup key, the human-visible advisory
//!   check, and the auto-merger's arm gate (ADR 0008 §5);
//! - a folded `<details>` block appended to the PR **body** (idempotent by a
//!   marker) — the round summary.
//!
//! It never posts inline review threads (`create_pr_review`): the fixer only
//! reacts to threads, so a pr-reviewer that opened one would re-ignite the
//! AI↔AI ping-pong ADR 0006 removed. The pr-reviewer is summary-only.
//!
//! Kind-specific behavior lives only in discovery and settle:
//! - **Plan** (spec/ADR PR, `meguri:spec-reviewing`): additionally drives the
//!   label state machine — a clean review flips `spec-reviewing → spec-ready`
//!   (which the combined-mode spec worker keys off), findings keep
//!   `spec-reviewing` so the next push is re-reviewed. The plan review is on
//!   by default (it is the old mandatory spec review).
//! - **Impl** (implementation PR): no label transition; off by default
//!   (opt-in; external-bot compatible).
//!
//! Lifetime (issue #92): runs are keyed by the PR's canonical *issue*; the
//! pane lives in the issue's independent `pr-review` lane; the worktree is a
//! read-only detached checkout fixed at `pr-reviewer-<issue>`, re-pointed to
//! each new head — all reclaimed when the issue closes.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use super::WorkerOutcome;
use super::flow::{self, Kind, NeedsHuman, STEP_EXECUTE, STEP_PREPARE_WORK, STEP_PREPARE_WORKTREE};
use super::{Deps, Target, canonical_issue, canonical_key};
use crate::forge::{self, CheckState, CommitStatusState, PullRequest};
use crate::gitops;
use crate::store::{LANE_PR_REVIEW, RunRecord, RunStatus};
use crate::tasks::TaskKey;
use crate::turn::{TurnOutcome, TurnStatus};

/// `runs.loop_kind` value for pr-reviewer runs.
pub const KIND: &str = "pr-reviewer";

/// The commit-status context the pr-reviewer writes on the reviewed head.
pub const PR_REVIEW_STATUS: &str = "meguri/pr-review";

/// Terminal pr-reviewer step: post the status/body, settle the PR labels.
pub const STEP_SETTLE: &str = "settle";

/// Where the orchestrator drops the PR diff for the agent to read.
pub const DIFF_FILE: &str = ".meguri/pr-diff.patch";
/// Where the agent writes its verdict + review body.
pub const REVIEW_FILE: &str = ".meguri/review.json";

/// Marker beginning the pr-reviewer's folded `<details>` in the PR body.
/// Everything from this marker to the end of the body is the pr-review
/// block, so re-reviewing truncates at it and re-appends (idempotent).
const PR_REVIEW_BODY_MARKER: &str = "<!-- meguri:pr-review -->";

/// Head-branch prefix identifying meguri's own PRs — the impl pr-reviewer
/// only reviews work meguri opened (same guard as the fixer).
const MEGURI_BRANCH_PREFIX: &str = "meguri/";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewVerdict {
    Clean,
    Findings,
}

/// What the agent writes to [`REVIEW_FILE`].
#[derive(Debug, Deserialize)]
pub struct ReviewFile {
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub review: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PrReviewCheckpoint {
    #[serde(default)]
    pub pr_number: Option<i64>,
    #[serde(default)]
    pub pr_title: String,
    #[serde(default)]
    pub pr_body: String,
    #[serde(default)]
    pub head_branch: String,
    #[serde(default)]
    pub head_sha: String,
    #[serde(default)]
    pub pr_url: String,
    /// Which side of the loop this PR is (plan/impl), decided at claim time
    /// from the PR's labels.
    #[serde(default = "default_kind")]
    pub kind: Kind,
    #[serde(default)]
    pub verdict: Option<ReviewVerdict>,
    #[serde(default)]
    pub review: String,
}

fn default_kind() -> Kind {
    Kind::Impl
}

/// Whether `pr` is a plan (spec/ADR) target: it carries `meguri:spec-reviewing`.
fn is_plan_pr(pr: &PullRequest) -> bool {
    pr.has_label(forge::LABEL_SPEC_REVIEWING)
}

/// The pr-reviewer's kind for a PR (plan iff it is a spec-reviewing PR).
fn kind_of(pr: &PullRequest) -> Kind {
    if is_plan_pr(pr) {
        Kind::Plan
    } else {
        Kind::Impl
    }
}

/// The pr-reviewer as a schedulable loop: reviewable meguri PRs (spec or
/// impl) in, a `meguri/pr-review` status + folded body summary out.
pub struct PrReviewerLoop;

#[async_trait]
impl super::Loop for PrReviewerLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    /// Candidate PRs whose *current head* has no pr-review status yet, keyed
    /// by their canonical issue. Plan candidates are `spec-reviewing` PRs
    /// (when the plan review is on); impl candidates are green,
    /// unlabeled-by-spec meguri PRs (when the impl review is on).
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        if deps.forge.is_none() {
            return Ok(Vec::new()); // PR loops are inert in local mode
        }
        let mut targets = Vec::new();
        for pr in deps.forge().list_open_prs().await? {
            if self.candidate_kind(deps, &pr).await?.is_none() {
                continue;
            }
            // Degraded mode: unresolved canonical issue is observable, not fatal.
            if canonical_issue(&pr).is_none() {
                deps.store.emit(
                    None,
                    "canonical_issue.unresolved",
                    json!({ "pr": pr.number, "head_branch": pr.head_branch }),
                )?;
            }
            targets.push(Target {
                key: TaskKey::Issue(canonical_key(&pr)),
                title: pr.title,
                cadence_label: None,
            });
        }
        Ok(targets)
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
        run_pr_reviewer(deps, run_id).await
    }
}

impl PrReviewerLoop {
    /// The kind this PR is a pr-review candidate for, or `None` if it is not
    /// actionable (review disabled for its kind, held/claimed, already
    /// reviewed at this head, or — for impl — CI not green).
    async fn candidate_kind(&self, deps: &Deps, pr: &PullRequest) -> Result<Option<Kind>> {
        let review = deps.config.review_for(&deps.project);
        // needs-human is a human stop signal on both sides: once the
        // pr-reviewer (or anything else) escalated a PR, do not re-review it
        // until a human clears the label (issue #176 — plan was previously
        // reviewed unconditionally, so a findings escalation would re-fire
        // forever; now symmetric with impl).
        if pr.state != "open"
            || pr.has_label(forge::LABEL_HOLD)
            || pr.has_label(forge::LABEL_WORKING)
            || pr.has_label(forge::LABEL_NEEDS_HUMAN)
        {
            return Ok(None);
        }
        let kind = kind_of(pr);
        if !kind.guard_enabled(review) {
            return Ok(None);
        }
        match kind {
            Kind::Plan => {} // spec-reviewing PRs are always reviewable
            Kind::Impl => {
                // Same ownership guard as the fixer: meguri branch only, and no
                // spec-phase label (spec-ready is the combined spec worker's
                // territory). needs-human is handled in common above.
                if !pr.head_branch.starts_with(MEGURI_BRANCH_PREFIX)
                    || pr.has_label(forge::LABEL_SPEC_READY)
                {
                    return Ok(None);
                }
                // Only review a settled-green head: Failure is the ci-fixer's,
                // Pending may still change under us.
                if deps.forge().pr_check_rollup(pr.number).await?.state() != CheckState::Success {
                    return Ok(None);
                }
            }
        }
        // Head already reviewed (the status is the dedup key).
        if deps
            .forge()
            .commit_status(&pr.head_sha, PR_REVIEW_STATUS)
            .await?
            .is_some()
        {
            return Ok(None);
        }
        Ok(Some(kind))
    }
}

pub async fn run_pr_reviewer(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
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

    match drive(deps, &run).await {
        Ok(outcome) => {
            match &outcome {
                WorkerOutcome::Succeeded { pr_url } => {
                    deps.store
                        .update_run_status(run_id, RunStatus::Succeeded, None)?;
                    deps.store
                        .emit(Some(run_id), "run.succeeded", json!({ "pr": pr_url }))?;
                }
                WorkerOutcome::Stopped => {
                    finalize_cancelled(deps, &run).await?;
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
                WorkerOutcome::NeedsPlan(reason) | WorkerOutcome::Decomposed(reason) => {
                    // Unreachable: review turns escalate these instead.
                    deps.store
                        .update_run_status(run_id, RunStatus::Failed, Some(reason))?;
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
            match claimed_pr(deps, run_id) {
                Some(pr) => escalate_on_pr(deps, &run, pr, &msg).await,
                None => flow::escalate_on_forge(deps, run.issue_number, &msg).await,
            }
            Err(e)
        }
    }
}

fn claimed_pr(deps: &Deps, run_id: &str) -> Option<i64> {
    let run = deps.store.get_run(run_id).ok().flatten()?;
    serde_json::from_str::<PrReviewCheckpoint>(&run.checkpoint_json)
        .ok()
        .and_then(|cp| cp.pr_number)
}

async fn drive(deps: &Deps, run: &RunRecord) -> Result<WorkerOutcome> {
    let mut cp: PrReviewCheckpoint = serde_json::from_str(&run.checkpoint_json).unwrap_or_default();
    let mut step = run.step.clone();

    if step == STEP_PREPARE_WORK {
        let pr = match prepare_work(deps, run).await? {
            Prepared::Claimed(pr) => pr,
            Prepared::Skip(reason) => return Ok(WorkerOutcome::Skipped(reason)),
        };
        cp.kind = kind_of(&pr);
        cp.pr_number = Some(pr.number);
        cp.pr_title = pr.title;
        cp.pr_body = pr.body;
        cp.head_branch = pr.head_branch;
        cp.head_sha = pr.head_sha;
        cp.pr_url = pr.url;
        step = save_step(deps, run, STEP_PREPARE_WORKTREE, &cp)?;
    }

    if step == STEP_PREPARE_WORKTREE {
        prepare_worktree(deps, run, &cp).await?;
        step = save_step(deps, run, STEP_EXECUTE, &cp)?;
    }

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
        match execute(deps, &run, &mut cp, &worktree).await? {
            flow::StepFlow::Continue => {}
            flow::StepFlow::Stopped => return Ok(WorkerOutcome::Stopped),
            flow::StepFlow::Interrupted(r) => return Ok(WorkerOutcome::Interrupted(r)),
            flow::StepFlow::NeedsPlan(reason) => {
                return Err(NeedsHuman(format!(
                    "agent asked for a plan reviewing issue #{}: {reason}",
                    run.issue_number
                ))
                .into());
            }
            flow::StepFlow::Decompose(result) => {
                return Err(NeedsHuman(format!(
                    "agent asked to decompose reviewing issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
        }
        step = save_step(deps, &run, STEP_SETTLE, &cp)?;
    }

    if step == STEP_SETTLE {
        let pr_url = settle(deps, &run, &cp).await?;
        flow::finish_pane(deps, &run).await;
        return Ok(WorkerOutcome::Succeeded { pr_url });
    }

    bail!("unknown step {step:?}");
}

fn save_step(deps: &Deps, run: &RunRecord, step: &str, cp: &PrReviewCheckpoint) -> Result<String> {
    deps.store
        .update_run_step(&run.id, step, &serde_json::to_string(cp)?)?;
    Ok(step.to_string())
}

async fn finalize_cancelled(deps: &Deps, run: &RunRecord) -> Result<()> {
    deps.store
        .update_run_status(&run.id, RunStatus::Cancelled, None)?;
    if let Some(pr) = claimed_pr(deps, &run.id) {
        deps.forge()
            .remove_pr_label(pr, forge::LABEL_WORKING)
            .await
            .ok();
    }
    super::reaper::release_pane(deps, run.issue_number, LANE_PR_REVIEW, "stopped by user").await;
    deps.store.emit(Some(&run.id), "run.cancelled", json!({}))?;
    Ok(())
}

async fn escalate_on_pr(deps: &Deps, run: &RunRecord, pr: i64, reason: &str) {
    // The central helper posts the label/comment/event; the closing hint is
    // launch-mode-aware (issue #169) — a direct-mode pr-reviewer has no pane.
    let comment = super::escalation::pr_needs_human_comment(
        "could not finish reviewing this PR and needs a human.",
        reason,
        &flow::attach_hint(deps, run),
    );
    super::escalation::escalate_pr(deps, pr, &comment).await;
}

enum Prepared {
    Claimed(PullRequest),
    Skip(String),
}

/// prepare-work: re-resolve the PR for the run's canonical issue and claim it
/// with `meguri:working`. Any change that makes it un-reviewable is a benign
/// race — skip, don't escalate.
async fn prepare_work(deps: &Deps, run: &RunRecord) -> Result<Prepared> {
    let mut matches: Vec<PullRequest> = deps
        .forge()
        .list_open_prs()
        .await?
        .into_iter()
        .filter(|pr| canonical_key(pr) == run.issue_number)
        .collect();
    let pr = match matches.len() {
        1 => matches.remove(0),
        0 => {
            return Ok(Prepared::Skip(format!(
                "no open reviewable PR for issue #{} (label removed since discovery?)",
                run.issue_number
            )));
        }
        n => {
            return Ok(Prepared::Skip(format!(
                "{n} open PRs resolve to issue #{} — not picking one",
                run.issue_number
            )));
        }
    };
    if PrReviewerLoop.candidate_kind(deps, &pr).await?.is_none() {
        return Ok(Prepared::Skip(format!(
            "PR #{} is no longer a pr-review candidate (claimed, held, reviewed, or CI moved)",
            pr.number
        )));
    }
    deps.forge()
        .add_pr_label(pr.number, forge::LABEL_WORKING)
        .await?;
    deps.store.emit(
        Some(&run.id),
        "pr.claimed",
        json!({ "pr": pr.number, "head": pr.head_sha, "kind": kind_of(&pr).as_str() }),
    )?;
    Ok(Prepared::Claimed(pr))
}

/// prepare-worktree: detached checkout of the PR head, fixed at
/// `pr-reviewer-<issue>` so pane and session survive review rounds.
async fn prepare_worktree(deps: &Deps, run: &RunRecord, cp: &PrReviewCheckpoint) -> Result<()> {
    let root = deps
        .project
        .worktree_root
        .clone()
        .unwrap_or_else(crate::config::worktrees_root);
    let dir = format!("pr-reviewer-{}", run.issue_number);
    let wt = gitops::worktree_path(&root, &deps.project.id, &dir);
    gitops::create_review_worktree(
        &deps.project.repo_path,
        &wt,
        &cp.head_branch,
        &cp.head_sha,
        &deps.project.worktree_setup.exclude,
    )
    .await?;
    deps.store
        .update_run_worktree(&run.id, &cp.head_branch, &wt.to_string_lossy())?;
    deps.store.emit(
        Some(&run.id),
        "worktree.created",
        json!({ "branch": cp.head_branch, "head": cp.head_sha,
                "path": wt.to_string_lossy() }),
    )?;
    flow::run_worktree_setup(deps, run, &wt).await
}

fn execute_prompt(cp: &PrReviewCheckpoint, language: Option<&str>) -> String {
    let (subject, artifact) = match cp.kind {
        Kind::Plan => ("spec/ADR pull request", "spec"),
        Kind::Impl => ("implementation pull request", "change"),
    };
    format!(
        "You are the independent pr-reviewer for {subject} #{number} in this \
         repository. The worktree is checked out read-only at the PR head (commit \
         `{sha}`, branch `{branch}`).\n\n\
         # PR: {title}\n\n{body}\n\n\
         # Instructions\n\
         - Read the PR's full diff at `{diff}`; browse the checked-out code for \
           context as needed.\n\
         - Review the {artifact} for correctness, completeness, and fit with the \
           repository's conventions. A summary-style review is enough — no inline \
           threads.\n\
         - Do NOT modify, commit, or push anything; the review file below is your \
           only deliverable.\n\
         - Write your review to `{review}` as JSON:\n\
           `{{\"verdict\": \"clean\" | \"findings\", \"review\": \"<Markdown review>\"}}`\n\
           - \"clean\": nothing must change before this PR can proceed (pure nitpicks \
             do not block; mention them in `review`).\n\
           - \"findings\": something must change; list every finding in `review`.\n\
         - A completed review is a success regardless of verdict; report \
           \"failure\"/\"needs_human\" only when you cannot review at all.\
         {lang_section}",
        number = cp.pr_number.unwrap_or_default(),
        sha = cp.head_sha,
        branch = cp.head_branch,
        title = cp.pr_title,
        body = cp.pr_body,
        diff = DIFF_FILE,
        review = REVIEW_FILE,
        lang_section = flow::language_instruction(language),
    )
    // The completion contract is appended by prepare_turn.
}

/// The pr-reviewer's deliverable, verified after each turn: a parseable
/// review file and an untouched checkout.
fn read_review(worktree: &Path) -> std::result::Result<ReviewFile, String> {
    let raw = std::fs::read_to_string(worktree.join(REVIEW_FILE)).map_err(|_| {
        format!("- review file `{REVIEW_FILE}` does not exist (write it as instructed)")
    })?;
    let review: ReviewFile = serde_json::from_str(raw.trim()).map_err(|e| {
        format!(
            "- review file `{REVIEW_FILE}` is not valid JSON ({e}); expected \
             {{\"verdict\": \"clean\" | \"findings\", \"review\": \"<Markdown>\"}}"
        )
    })?;
    if review.verdict == ReviewVerdict::Findings && review.review.trim().is_empty() {
        return Err(format!(
            "- verdict is \"findings\" but `review` in `{REVIEW_FILE}` is empty; \
             describe every finding"
        ));
    }
    Ok(review)
}

/// execute: one review turn (plus at most one corrective turn), then the
/// verdict lands in the checkpoint.
async fn execute(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut PrReviewCheckpoint,
    worktree: &Path,
) -> Result<flow::StepFlow> {
    let pr = cp.pr_number.context("checkpoint has no PR number")?;
    let diff = deps.forge().pr_diff(pr).await?;
    std::fs::create_dir_all(worktree.join(crate::turn::prompts::MEGURI_DIR))?;
    std::fs::write(worktree.join(DIFF_FILE), &diff)?;

    let mut prompt = execute_prompt(cp, deps.config.language_for(&deps.project));
    let mut corrective_turns = 0u32;

    loop {
        // The pr-reviewer runs in its own `pr-review` lane (lane_for_loop maps
        // the `pr-reviewer` loop_kind → LANE_PR_REVIEW), under the
        // `pr-reviewer` routing role.
        let (outcome, _) = flow::run_turn(deps, run, worktree, "pr-reviewer", &prompt).await?;
        let result = match outcome {
            TurnOutcome::Completed(r) => r,
            TurnOutcome::Stopped => return Ok(flow::StepFlow::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(flow::StepFlow::Interrupted(
                    "pane died during pr review".into(),
                ));
            }
        };

        match result.status {
            TurnStatus::Success => {}
            TurnStatus::Failure => {
                return Err(NeedsHuman(format!(
                    "agent reported failure reviewing PR #{pr}: {}",
                    result.summary
                ))
                .into());
            }
            TurnStatus::NeedsHuman | TurnStatus::NeedsPlan | TurnStatus::Decompose => {
                return Err(NeedsHuman(format!(
                    "agent needs a human reviewing PR #{pr}: {}",
                    result.summary
                ))
                .into());
            }
        }

        let clean = gitops::status_clean(worktree).await?;
        let head_now = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
        let problem = if !clean || head_now != cp.head_sha {
            Some(format!(
                "- the review checkout must stay untouched: working tree clean: \
                 {clean} (must be true), HEAD: {head_now} (must be {expected}) — \
                 discard all changes, the review file under .meguri/ is exempt",
                expected = cp.head_sha,
            ))
        } else {
            read_review(worktree).err()
        };
        let Some(problem) = problem else {
            let review = read_review(worktree).expect("verified above");
            cp.verdict = Some(review.verdict);
            cp.review = review.review;
            deps.store.emit(
                Some(&run.id),
                "pr_review.verified",
                json!({ "verdict": review.verdict, "head": cp.head_sha }),
            )?;
            return Ok(flow::StepFlow::Continue);
        };

        corrective_turns += 1;
        if corrective_turns > 1 {
            return Err(NeedsHuman(format!(
                "agent claimed success but the pr review doesn't verify after a \
                 corrective turn:\n{problem}"
            ))
            .into());
        }
        deps.store.emit(
            Some(&run.id),
            "execute.correction",
            json!({ "problem": problem }),
        )?;
        prompt = format!(
            "Your previous result claimed success, but verification failed:\n{problem}\n\n\
             Fix this. Remember: do not modify the checkout; write your review \
             to `{REVIEW_FILE}` as instructed.",
        );
    }
}

/// The folded `<details>` the pr-reviewer appends to the PR body. Idempotent
/// by [`PR_REVIEW_BODY_MARKER`]: [`upsert_pr_review_details`] truncates any
/// prior block before appending this one.
fn pr_review_details(cp: &PrReviewCheckpoint, verdict: ReviewVerdict) -> String {
    let short = cp.head_sha.get(..12).unwrap_or(&cp.head_sha);
    let outcome = match verdict {
        ReviewVerdict::Clean => "clean",
        ReviewVerdict::Findings => "findings",
    };
    let review = cp.review.trim();
    let review = if review.is_empty() {
        "(no notes)"
    } else {
        review
    };
    format!(
        "{PR_REVIEW_BODY_MARKER}\n<details>\n<summary>🛡️ pr review ({kind}) — {outcome} at `{short}`</summary>\n\n{review}\n</details>",
        kind = cp.kind.as_str(),
    )
}

/// The pr-reviewer's folded `<details>` block within a PR body, if present:
/// everything from [`PR_REVIEW_BODY_MARKER`] to the end (the block is always
/// last — see [`upsert_pr_review_details`]). `spec_fixer` reads it to feed the
/// plan findings back to the fixing agent (issue #188); returns `None` when the
/// body carries no review block.
pub fn extract_pr_review_details(body: &str) -> Option<String> {
    let idx = body.find(PR_REVIEW_BODY_MARKER)?;
    let block = body[idx..].trim();
    (!block.is_empty()).then(|| block.to_string())
}

/// Replace (or append) the pr-review `<details>` in a PR body. The pr-review
/// block is always last: everything from the marker to the end is the block,
/// so a re-review truncates there and re-appends.
fn upsert_pr_review_details(body: &str, block: &str) -> String {
    let base = match body.find(PR_REVIEW_BODY_MARKER) {
        Some(idx) => body[..idx].trim_end(),
        None => body.trim_end(),
    };
    if base.is_empty() {
        block.to_string()
    } else {
        format!("{base}\n\n{block}")
    }
}

/// settle: write the `meguri/pr-review` commit status, fold the summary into
/// the PR body, and (plan only) settle the spec labels. Idempotent on resume.
async fn settle(deps: &Deps, run: &RunRecord, cp: &PrReviewCheckpoint) -> Result<String> {
    let pr = cp.pr_number.context("checkpoint has no PR number")?;
    let verdict = cp.verdict.context("checkpoint has no review verdict")?;

    let (state, desc) = match verdict {
        ReviewVerdict::Clean => (CommitStatusState::Success, "clean".to_string()),
        ReviewVerdict::Findings => (
            CommitStatusState::Failure,
            "findings — see the PR body".to_string(),
        ),
    };
    // The status is the dedup key + advisory check + auto-merge gate (ADR 0008).
    deps.forge()
        .set_commit_status(&cp.head_sha, PR_REVIEW_STATUS, state, &desc)
        .await?;

    // Fold the round summary into the PR body (no conversation comment, no
    // inline thread — the fixer never reacts).
    let new_body = upsert_pr_review_details(&cp.pr_body, &pr_review_details(cp, verdict));
    deps.forge().update_pr_body(pr, &new_body).await?;
    deps.store.emit(
        Some(&run.id),
        "pr_review.posted",
        json!({ "pr": pr, "verdict": verdict, "head": cp.head_sha, "kind": cp.kind.as_str() }),
    )?;

    // Plan review drives the label state machine (ADR 0008 §3): a clean spec
    // review flips spec-reviewing → spec-ready (the combined spec worker keys
    // off it). The impl review never touches spec labels.
    if cp.kind == Kind::Plan && verdict == ReviewVerdict::Clean {
        deps.forge()
            .add_pr_label(pr, forge::LABEL_SPEC_READY)
            .await?;
        deps.forge()
            .remove_pr_label(pr, forge::LABEL_SPEC_REVIEWING)
            .await
            .ok();
    }

    match (cp.kind, verdict) {
        (_, ReviewVerdict::Clean) => {
            deps.forge()
                .remove_pr_label(pr, forge::LABEL_WORKING)
                .await
                .ok();
        }
        // Plan findings: `spec_fixer` (ADR 0013) is the plan-side human gate —
        // it discovers `spec-reviewing` PRs whose head `meguri/pr-review` is
        // `Failure` and drives the fix itself, escalating on its own if its
        // round budget runs out. Adding `needs-human` here would starve that
        // discover query (it skips escalated PRs) before spec_fixer ever
        // runs — the same lockout ADR 0007 avoids by having merge_watch defer
        // to fixer loops instead of escalating first. Drop the working claim
        // (this settle's turn is done) but leave the label/status as-is.
        (Kind::Plan, ReviewVerdict::Findings) => {
            deps.forge()
                .remove_pr_label(pr, forge::LABEL_WORKING)
                .await
                .ok();
            deps.store.emit(
                Some(&run.id),
                "pr_review.deferred_to_spec_fixer",
                json!({ "pr": pr, "kind": cp.kind.as_str(), "head": cp.head_sha }),
            )?;
        }
        // Impl findings: no auto-fix loop drives impl PRs off pr-reviewer
        // findings, so the pr-reviewer stays the human gate here (ADR 0012
        // P1/P3). `escalate_pr` drops the working claim and adds
        // needs-human (which also stops discover from re-reviewing until a
        // human clears it).
        (Kind::Impl, ReviewVerdict::Findings) => {
            let lead = format!(
                "PR review ({}) found issues that need a human before this PR can proceed.",
                cp.kind.as_str()
            );
            let comment = super::escalation::pr_needs_human_comment(
                &lead,
                "See the folded 🛡️ PR review in the PR body for the findings.",
                &flow::attach_hint(deps, run),
            );
            super::escalation::escalate_pr(deps, pr, &comment).await;
            deps.store.emit(
                Some(&run.id),
                "pr_review.escalated",
                json!({ "pr": pr, "kind": cp.kind.as_str(), "head": cp.head_sha }),
            )?;
        }
    }
    Ok(cp.pr_url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(kind: Kind, head: &str) -> PrReviewCheckpoint {
        PrReviewCheckpoint {
            pr_number: Some(12),
            pr_title: "Spec: Add caching (#5)".into(),
            pr_body: "Refs #5.".into(),
            head_branch: "meguri/5-add-caching-abc".into(),
            head_sha: head.into(),
            kind,
            review: "- missing acceptance criteria".into(),
            ..Default::default()
        }
    }

    #[test]
    fn kind_of_keys_off_spec_reviewing() {
        let base = PullRequest {
            number: 1,
            title: String::new(),
            body: String::new(),
            url: String::new(),
            head_branch: "meguri/1-x".into(),
            head_sha: "sha".into(),
            state: "open".into(),
            is_draft: false,
            labels: vec![forge::LABEL_SPEC_REVIEWING.to_string()],
        };
        assert_eq!(kind_of(&base), Kind::Plan);
        let impl_pr = PullRequest {
            labels: vec![forge::LABEL_IMPLEMENTING.to_string()],
            ..base.clone()
        };
        assert_eq!(kind_of(&impl_pr), Kind::Impl);
    }

    #[test]
    fn prompt_demands_review_not_changes() {
        let prompt = execute_prompt(&cp(Kind::Plan, "deadbeef"), None);
        assert!(prompt.contains("pull request #12"));
        assert!(prompt.contains("# PR: Spec: Add caching (#5)"));
        assert!(prompt.contains(DIFF_FILE));
        assert!(prompt.contains(REVIEW_FILE));
        assert!(prompt.contains("Do NOT modify"));
        assert!(prompt.contains("deadbeef"));
        assert!(prompt.contains("no inline threads"));
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn impl_prompt_frames_a_change_not_a_spec() {
        let prompt = execute_prompt(&cp(Kind::Impl, "abc"), Some("日本語"));
        assert!(prompt.contains("implementation pull request"));
        assert!(prompt.contains("# Output language"));
    }

    #[test]
    fn pr_review_details_folds_verdict_and_head() {
        let block = pr_review_details(&cp(Kind::Impl, "0123456789abcdef"), ReviewVerdict::Findings);
        assert!(block.contains(PR_REVIEW_BODY_MARKER));
        assert!(block.contains("<details>"));
        assert!(block.contains("pr review (impl) — findings at `0123456789ab`"));
        assert!(block.contains("- missing acceptance criteria"));
    }

    #[test]
    fn upsert_replaces_a_prior_pr_review_block() {
        let body = "Refs #5.\n\nBody text.";
        let first = upsert_pr_review_details(
            body,
            &pr_review_details(&cp(Kind::Plan, "aaa"), ReviewVerdict::Findings),
        );
        assert!(first.starts_with("Refs #5."));
        assert!(first.contains("Body text."));
        assert_eq!(first.matches(PR_REVIEW_BODY_MARKER).count(), 1);

        // Re-reviewing the new head replaces the block, never stacks it.
        let second = upsert_pr_review_details(
            &first,
            &pr_review_details(&cp(Kind::Plan, "bbb"), ReviewVerdict::Clean),
        );
        assert_eq!(second.matches(PR_REVIEW_BODY_MARKER).count(), 1);
        assert!(second.contains("clean at `bbb`"));
        assert!(!second.contains("findings at `aaa`"));
        assert!(second.starts_with("Refs #5."));
    }

    #[test]
    fn extract_pr_review_details_pulls_the_folded_block() {
        // No block: nothing to feed a fixer.
        assert_eq!(extract_pr_review_details("Refs #5.\n\nBody text."), None);

        // With a block: everything from the marker to the end (issue #188).
        let body = upsert_pr_review_details(
            "Refs #5.",
            &pr_review_details(&cp(Kind::Plan, "aaa"), ReviewVerdict::Findings),
        );
        let block = extract_pr_review_details(&body).unwrap();
        assert!(block.starts_with(PR_REVIEW_BODY_MARKER));
        assert!(block.contains("- missing acceptance criteria"));
        assert!(
            !block.contains("Refs #5."),
            "only the review block: {block}"
        );
    }

    /// The plan review's settle drives the spec label state machine (ADR 0008
    /// §3): clean flips `spec-reviewing → spec-ready`, findings keep
    /// `spec-reviewing` so the next push (spec_fixer, issue #188) triggers a
    /// re-review.
    #[tokio::test]
    async fn plan_settle_drives_the_spec_label_state_machine() {
        async fn settle_verdict(
            verdict: ReviewVerdict,
        ) -> std::sync::Arc<crate::forge::fake::FakeForge> {
            let forge = std::sync::Arc::new(crate::forge::fake::FakeForge::default());
            forge.add_pr(
                7,
                "Spec: caching (#5)",
                "Refs #5.",
                &[forge::LABEL_SPEC_REVIEWING],
                "meguri/5-add-caching-abc",
                "deadbeef",
            );
            let project = crate::config::ProjectConfig {
                id: "proj".into(),
                repo_path: "/tmp/unused".into(),
                repo_slug: Some("me/proj".into()),
                mode: Default::default(),
                deliver: None,
                default_branch: "main".into(),
                check_command: None,
                worktree_root: None,
                language: None,
                pr: None,
                clean: None,
                plan_delivery: Default::default(),
                review: None,
                worktree_setup: Default::default(),
                schedules: Vec::new(),
                autonomy: None,
                cadence: Vec::new(),
                prompts: Default::default(),
            };
            let deps = Deps::with_label_source(
                crate::store::Store::open_in_memory().unwrap(),
                std::sync::Arc::new(crate::mux::fake::FakeMux::new(false)),
                forge.clone(),
                crate::config::Config::default(),
                project,
            );
            let run = deps
                .store
                .create_run_for_loop("proj", KIND, 5, "Spec: caching (#5)")
                .unwrap();
            let mut c = cp(Kind::Plan, "deadbeef");
            c.pr_number = Some(7);
            c.pr_url = "https://fake.example/pr/7".into();
            c.verdict = Some(verdict);
            settle(&deps, &run, &c).await.unwrap();
            forge
        }

        // Clean: spec-reviewing removed, spec-ready added.
        let forge = settle_verdict(ReviewVerdict::Clean).await;
        let labels = forge.pr_labels(7);
        assert!(labels.contains(&forge::LABEL_SPEC_READY.to_string()));
        assert!(!labels.contains(&forge::LABEL_SPEC_REVIEWING.to_string()));
        assert_eq!(
            forge.commit_status_of("deadbeef", PR_REVIEW_STATUS),
            Some(CommitStatusState::Success)
        );

        // Findings: spec-reviewing kept (spec_fixer's re-drive target), no
        // spec-ready, status failure, and — issue #192 — no needs-human, or
        // spec_fixer's discover (which skips escalated PRs) would never fire.
        let forge = settle_verdict(ReviewVerdict::Findings).await;
        let labels = forge.pr_labels(7);
        assert!(labels.contains(&forge::LABEL_SPEC_REVIEWING.to_string()));
        assert!(!labels.contains(&forge::LABEL_SPEC_READY.to_string()));
        assert!(
            !labels.contains(&forge::LABEL_NEEDS_HUMAN.to_string()),
            "plan findings must not escalate — spec_fixer (ADR 0013) owns the fix loop: {labels:?}"
        );
        assert_eq!(
            forge.commit_status_of("deadbeef", PR_REVIEW_STATUS),
            Some(CommitStatusState::Failure)
        );
    }

    /// Impl findings are unchanged by issue #192: no auto-fix loop drives
    /// impl PRs, so the pr-reviewer stays the human gate (ADR 0012).
    #[tokio::test]
    async fn impl_findings_still_escalate_to_needs_human() {
        let forge = std::sync::Arc::new(crate::forge::fake::FakeForge::default());
        forge.add_pr(
            9,
            "Add caching (#5)",
            "Refs #5.",
            &[forge::LABEL_IMPLEMENTING, forge::LABEL_WORKING],
            "meguri/5-add-caching-def",
            "cafef00d",
        );
        let project = crate::config::ProjectConfig {
            id: "proj".into(),
            repo_path: "/tmp/unused".into(),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            check_command: None,
            worktree_root: None,
            language: None,
            pr: None,
            clean: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup: Default::default(),
            schedules: Vec::new(),
            autonomy: None,
            cadence: Vec::new(),
            prompts: Default::default(),
        };
        let deps = Deps::with_label_source(
            crate::store::Store::open_in_memory().unwrap(),
            std::sync::Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge.clone(),
            crate::config::Config::default(),
            project,
        );
        let run = deps
            .store
            .create_run_for_loop("proj", KIND, 5, "Add caching (#5)")
            .unwrap();
        let mut c = cp(Kind::Impl, "cafef00d");
        c.pr_number = Some(9);
        c.pr_url = "https://fake.example/pr/9".into();
        c.verdict = Some(ReviewVerdict::Findings);
        settle(&deps, &run, &c).await.unwrap();

        let labels = forge.pr_labels(9);
        assert!(labels.contains(&forge::LABEL_NEEDS_HUMAN.to_string()));
        assert!(!labels.contains(&forge::LABEL_WORKING.to_string()));
        assert_eq!(
            forge.commit_status_of("cafef00d", PR_REVIEW_STATUS),
            Some(CommitStatusState::Failure)
        );
    }

    #[test]
    fn review_file_parses_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".meguri")).unwrap();
        let path = dir.path().join(REVIEW_FILE);

        assert!(
            read_review(dir.path())
                .unwrap_err()
                .contains("does not exist")
        );
        std::fs::write(&path, "not json").unwrap();
        assert!(
            read_review(dir.path())
                .unwrap_err()
                .contains("not valid JSON")
        );
        std::fs::write(&path, r#"{"verdict":"findings","review":"  "}"#).unwrap();
        assert!(read_review(dir.path()).unwrap_err().contains("empty"));
        std::fs::write(&path, r#"{"verdict":"clean"}"#).unwrap();
        assert_eq!(
            read_review(dir.path()).unwrap().verdict,
            ReviewVerdict::Clean
        );
    }

    /// `prepare_worktree` re-points the same detached checkout onto each new
    /// review round's head via `reset --hard` + `clean -fd` (see
    /// `gitops::create_review_worktree`), which wipes untracked files. The
    /// `worktree_setup` hook (issue #138) must run again on that re-point — not
    /// just the first time the checkout is created — since its output is
    /// exactly the kind of untracked artifact `clean -fd` removes.
    #[tokio::test]
    async fn worktree_setup_reruns_when_the_pr_reviewer_worktree_is_repointed() {
        let repo = tempfile::tempdir().unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
            vec!["checkout", "-b", "pr-branch"],
            vec!["commit", "--allow-empty", "-m", "round1"],
        ] {
            gitops::run_git(repo.path(), &args).await.unwrap();
        }

        let worktree_root = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open_in_memory().unwrap();
        let run = store
            .create_run_for_loop("proj", KIND, 5, "Spec: caching (#5)")
            .unwrap();
        let forge = std::sync::Arc::new(crate::forge::fake::FakeForge::with_issue(
            5,
            "Spec: caching (#5)",
            "body",
            &[],
        ));
        let project = crate::config::ProjectConfig {
            id: "proj".into(),
            repo_path: repo.path().to_path_buf(),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            worktree_root: Some(worktree_root.path().to_path_buf()),
            pr: None,
            clean: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup: crate::config::WorktreeSetupConfig {
                commands: vec!["echo ran > marker.txt".into()],
                ..Default::default()
            },
            schedules: Vec::new(),
            autonomy: None,
            cadence: Vec::new(),
            prompts: Default::default(),
        };
        let deps = Deps::with_label_source(
            store,
            std::sync::Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge,
            crate::config::Config::default(),
            project,
        );

        let head1 = gitops::run_git(repo.path(), &["rev-parse", "HEAD"])
            .await
            .unwrap();
        let mut cp = PrReviewCheckpoint {
            head_branch: "pr-branch".into(),
            head_sha: head1,
            ..Default::default()
        };
        prepare_worktree(&deps, &run, &cp).await.unwrap();
        let wt = PathBuf::from(
            deps.store
                .get_run(&run.id)
                .unwrap()
                .unwrap()
                .worktree_path
                .unwrap(),
        );
        assert!(wt.join("marker.txt").exists());

        // A second review round: a new commit lands on the PR branch, the
        // checkout re-points onto it (reset --hard + clean -fd wipes
        // marker.txt), and the hook must regenerate it.
        gitops::run_git(repo.path(), &["commit", "--allow-empty", "-m", "round2"])
            .await
            .unwrap();
        cp.head_sha = gitops::run_git(repo.path(), &["rev-parse", "HEAD"])
            .await
            .unwrap();
        prepare_worktree(&deps, &run, &cp).await.unwrap();
        assert!(
            wt.join("marker.txt").exists(),
            "worktree_setup must rerun after the pr-reviewer worktree re-points"
        );
    }
}
