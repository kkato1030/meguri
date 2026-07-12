//! The impl-reviewer loop: an open meguri implementation PR (green CI, no
//! spec labels, no thread already awaiting the fixer) → detached worktree at
//! the PR head → an agent turn reads the diff and writes findings. Findings
//! are posted as inline review threads (plus a marked summary comment), which
//! feeds the existing fixer ping-pong; a clean review posts only the marked
//! comment, so nothing reacts and the loop rests. Label-less by design: what
//! was reviewed is recorded on the PR ("Authority"), never in local state.
//! Convergence is triple-stopped (ADR 0004): one review per head (the
//! marker), a rounds cap (`review.impl_max_rounds`), and clean-creates-no-
//! threads. The AI never approves or requests changes — event=COMMENT only;
//! the human merge gate stays human.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use super::WorkerOutcome;
use super::fixer::{MEGURI_BRANCH_PREFIX, thread_awaits_fixer};
use super::flow::{self, NeedsHuman, STEP_EXECUTE, STEP_PREPARE_WORK, STEP_PREPARE_WORKTREE};
pub use super::reviewer::{DIFF_FILE, REVIEW_FILE, ReviewVerdict};
use super::{Deps, Target};
use crate::forge::{self, CheckState, PullRequest, ReviewCommentDraft};
use crate::gitops;
use crate::store::{RunRecord, RunStatus};
use crate::turn::{TurnOutcome, TurnStatus};

/// `runs.loop_kind` value for impl-reviewer runs.
pub const KIND: &str = "impl-reviewer";

/// Terminal impl-reviewer step: post the review threads and comment.
pub const STEP_SETTLE: &str = "settle";

/// Hidden marker embedded in the summary comment of every impl review. Its
/// presence for a head sha means that head was already reviewed; the count
/// of markers (any head) is the rounds spent on this PR.
pub fn impl_review_marker(head_sha: &str) -> String {
    format!("<!-- meguri:impl-review head={head_sha} -->")
}

/// Prefix shared by every impl-review marker, for counting rounds.
const MARKER_PREFIX: &str = "<!-- meguri:impl-review head=";

pub fn head_already_reviewed(comments: &[String], head_sha: &str) -> bool {
    let marker = impl_review_marker(head_sha);
    comments.iter().any(|c| c.contains(&marker))
}

/// Impl-review rounds already spent on this PR: one marker comment per
/// reviewed head, whatever the head was.
pub fn review_rounds(comments: &[String]) -> u32 {
    comments
        .iter()
        .filter(|c| c.contains(MARKER_PREFIX))
        .count() as u32
}

/// One finding, anchored to a line on the NEW side of the diff — it becomes
/// an inline review thread the fixer picks up. Anchor-less remarks belong in
/// the summary `review`, not here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub path: String,
    pub line: u64,
    pub body: String,
}

/// What the agent writes to [`REVIEW_FILE`] on an impl-review turn.
#[derive(Debug, Deserialize)]
pub struct ImplReviewFile {
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub review: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ImplReviewCheckpoint {
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
    #[serde(default)]
    pub verdict: Option<ReviewVerdict>,
    #[serde(default)]
    pub review: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
}

/// The impl-reviewer as a schedulable loop: quiet meguri implementation PRs
/// in, review threads (fixer input) and marked summary comments out.
pub struct ImplReviewerLoop;

/// Why a PR is not impl-reviewable right now, or None if it is. Shared by
/// discovery and the prepare-work re-check; the forge lookups (comments,
/// threads, CI) stay outside so each caller fetches them once.
fn pr_not_reviewable(pr: &PullRequest) -> Option<String> {
    if pr.state != "open" {
        return Some(format!("PR #{} is {} (not open)", pr.number, pr.state));
    }
    if !pr.head_branch.starts_with(MEGURI_BRANCH_PREFIX) {
        return Some(format!(
            "PR #{} head `{}` was not opened by meguri",
            pr.number, pr.head_branch
        ));
    }
    for label in [
        forge::LABEL_SPEC_REVIEWING, // spec phase: the (spec) reviewer's territory
        forge::LABEL_SPEC_READY,     // implementation underway: the worker owns the branch
        forge::LABEL_WORKING,        // claimed by some run
        forge::LABEL_HOLD,
    ] {
        if pr.has_label(label) {
            return Some(format!("PR #{} is labeled {label}", pr.number));
        }
    }
    None
}

#[async_trait]
impl super::Loop for ImplReviewerLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    /// Open meguri implementation PRs whose current head is green, quiet
    /// (no thread already awaiting the fixer) and unreviewed, below the
    /// rounds cap. Like the reviewer, deliberately not succeeded-run-guarded:
    /// every pushed head is a fresh review candidate — the marker is the
    /// dedup, the rounds cap the brake.
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        if !deps.config.review.impl_enabled {
            return Ok(Vec::new());
        }
        let max_rounds = deps.config.review.impl_max_rounds;
        let mut targets = Vec::new();
        for pr in deps.forge.list_open_prs().await? {
            if pr_not_reviewable(&pr).is_some() {
                continue;
            }
            let comments = deps.forge.pr_comments(pr.number).await?;
            if head_already_reviewed(&comments, &pr.head_sha)
                || review_rounds(&comments) >= max_rounds
            {
                continue;
            }
            // Fixes first: while a thread awaits the fixer the head is about
            // to move; review the pushed result instead.
            let threads = deps.forge.list_review_threads(pr.number).await?;
            if threads.iter().any(thread_awaits_fixer) {
                continue;
            }
            // Only green heads: Failure is the ci-fixer's territory, Pending
            // may still change under the review — next poll retries.
            if deps.forge.pr_check_rollup(pr.number).await?.state() != CheckState::Success {
                continue;
            }
            targets.push(Target {
                issue_number: pr.number,
                title: pr.title,
            });
        }
        Ok(targets)
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
        run_impl_reviewer(deps, run_id).await
    }
}

pub async fn run_impl_reviewer(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
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
                WorkerOutcome::NeedsPlan(reason) => {
                    // Unreachable: review turns escalate needs_plan instead.
                    deps.store
                        .update_run_status(run_id, RunStatus::NeedsPlan, Some(reason))?;
                    deps.store
                        .emit(Some(run_id), "run.needs_plan", json!({ "reason": reason }))?;
                }
                WorkerOutcome::Decomposed(reason) => {
                    // Unreachable: review turns escalate decompose instead.
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
            escalate_on_pr(deps, run.issue_number, &msg).await;
            Err(e)
        }
    }
}

async fn drive(deps: &Deps, run: &RunRecord) -> Result<WorkerOutcome> {
    let mut cp: ImplReviewCheckpoint =
        serde_json::from_str(&run.checkpoint_json).unwrap_or_default();
    let mut step = run.step.clone();

    if step == STEP_PREPARE_WORK {
        let pr = match prepare_work(deps, run).await? {
            Prepared::Claimed(pr) => pr,
            Prepared::Skip(reason) => return Ok(WorkerOutcome::Skipped(reason)),
        };
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
        match execute(deps, &run, &mut cp, &worktree).await? {
            flow::StepFlow::Continue => {}
            flow::StepFlow::Stopped => return Ok(WorkerOutcome::Stopped),
            flow::StepFlow::Interrupted(r) => return Ok(WorkerOutcome::Interrupted(r)),
            flow::StepFlow::NeedsPlan(reason) => {
                // Unreachable: the review turn escalates needs_plan below.
                return Err(NeedsHuman(format!(
                    "agent asked for a plan reviewing PR #{}: {reason}",
                    run.issue_number
                ))
                .into());
            }
            flow::StepFlow::Decompose(result) => {
                // Unreachable: the review turn escalates decompose below.
                return Err(NeedsHuman(format!(
                    "agent asked to decompose reviewing PR #{}: {}",
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

fn save_step(
    deps: &Deps,
    run: &RunRecord,
    step: &str,
    cp: &ImplReviewCheckpoint,
) -> Result<String> {
    deps.store
        .update_run_step(&run.id, step, &serde_json::to_string(cp)?)?;
    Ok(step.to_string())
}

/// `meguri stop`: cancel the run, release the PR claim, release the pane
/// (its session id is saved first, so the context stays resumable).
async fn finalize_cancelled(deps: &Deps, run: &RunRecord) -> Result<()> {
    deps.store
        .update_run_status(&run.id, RunStatus::Cancelled, None)?;
    deps.forge
        .remove_pr_label(run.issue_number, forge::LABEL_WORKING)
        .await
        .ok();
    super::reaper::release_pane(
        deps,
        run.issue_number,
        super::role_for_loop(&run.loop_kind),
        "stopped by user",
    )
    .await;
    deps.store.emit(Some(&run.id), "run.cancelled", json!({}))?;
    Ok(())
}

/// Failure escalation on the PR (mirrors the reviewer's).
async fn escalate_on_pr(deps: &Deps, pr: i64, reason: &str) {
    let _ = deps.forge.add_pr_label(pr, forge::LABEL_NEEDS_HUMAN).await;
    let _ = deps.forge.remove_pr_label(pr, forge::LABEL_WORKING).await;
    let _ = deps
        .forge
        .comment_pr(
            pr,
            &format!(
                "🔁 **meguri** could not finish reviewing this PR and needs a human.\n\n> {reason}\n\n\
                 The agent's pane (if still open) has the full context — \
                 see `meguri ps` / `meguri attach` on the host running meguri."
            ),
        )
        .await;
}

enum Prepared {
    Claimed(PullRequest),
    Skip(String),
}

/// prepare-work: re-verify the PR on the forge, then claim it with
/// `meguri:working`. Anything that changed since discovery (a label, a new
/// thread, a moved or reviewed head, CI going red) is a benign race — skip,
/// don't escalate.
async fn prepare_work(deps: &Deps, run: &RunRecord) -> Result<Prepared> {
    let pr = deps.forge.get_pr(run.issue_number).await?;
    if let Some(reason) = pr_not_reviewable(&pr) {
        return Ok(Prepared::Skip(reason));
    }
    let comments = deps.forge.pr_comments(pr.number).await?;
    if head_already_reviewed(&comments, &pr.head_sha) {
        return Ok(Prepared::Skip(format!(
            "PR #{} head {} already carries an impl review",
            pr.number, pr.head_sha
        )));
    }
    if review_rounds(&comments) >= deps.config.review.impl_max_rounds {
        return Ok(Prepared::Skip(format!(
            "PR #{} exhausted its impl-review rounds",
            pr.number
        )));
    }
    let threads = deps.forge.list_review_threads(pr.number).await?;
    if threads.iter().any(thread_awaits_fixer) {
        return Ok(Prepared::Skip(format!(
            "PR #{} has threads awaiting the fixer",
            pr.number
        )));
    }
    if deps.forge.pr_check_rollup(pr.number).await?.state() != CheckState::Success {
        return Ok(Prepared::Skip(format!(
            "PR #{} head {} is not green",
            pr.number, pr.head_sha
        )));
    }
    deps.forge
        .add_pr_label(pr.number, forge::LABEL_WORKING)
        .await?;
    deps.store.emit(
        Some(&run.id),
        "pr.claimed",
        json!({ "pr": pr.number, "head": pr.head_sha }),
    )?;
    Ok(Prepared::Claimed(pr))
}

/// prepare-worktree: detached checkout of the PR head. The run id keeps
/// concurrent or retried reviews of the same PR in separate directories.
async fn prepare_worktree(deps: &Deps, run: &RunRecord, cp: &ImplReviewCheckpoint) -> Result<()> {
    let root = deps
        .project
        .worktree_root
        .clone()
        .unwrap_or_else(crate::config::worktrees_root);
    let dir = format!("impl-review-{}-{}", run.issue_number, run.id);
    let wt = gitops::worktree_path(&root, &deps.project.id, &dir);
    gitops::create_review_worktree(&deps.project.repo_path, &wt, &cp.head_branch, &cp.head_sha)
        .await?;
    deps.store
        .update_run_worktree(&run.id, &cp.head_branch, &wt.to_string_lossy())?;
    deps.store.emit(
        Some(&run.id),
        "worktree.created",
        json!({ "branch": cp.head_branch, "head": cp.head_sha,
                "path": wt.to_string_lossy() }),
    )?;
    Ok(())
}

fn execute_prompt(run: &RunRecord, cp: &ImplReviewCheckpoint, language: Option<&str>) -> String {
    format!(
        "You are reviewing the implementation in pull request #{number} of \
         this repository. The worktree is checked out read-only at the PR \
         head (commit `{sha}`, branch `{branch}`).\n\n\
         # PR: {title}\n\n{body}\n\n\
         # Instructions\n\
         - Read the PR's full diff at `{diff}`; browse the checked-out code \
           for context as needed.\n\
         - Review the implementation for correctness, completeness (tests \
           included), and fit with the repository's conventions.\n\
         - Do NOT modify, commit, or push anything; the review file below is \
           your only deliverable.\n\
         - Write your review to `{review}` as JSON:\n\
           `{{\"verdict\": \"clean\" | \"findings\", \"review\": \"<Markdown summary>\", \
           \"findings\": [{{\"path\": \"src/x.rs\", \"line\": 42, \"body\": \"<what must change>\"}}]}}`\n\
           - \"clean\": nothing must change before this PR can merge \
             (pure nitpicks do not block; mention them in `review` and leave \
             `findings` empty).\n\
           - \"findings\": something must change. Each `findings` entry \
             becomes an inline review thread, so it must anchor to a line \
             that appears on the NEW side of the diff in `{diff}`; put \
             cross-cutting remarks that fit no single line in `review` only.\n\
         - A completed review is a success regardless of verdict; report \
           \"failure\"/\"needs_human\" only when you cannot review at all.\
         {lang_section}",
        number = run.issue_number,
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

/// The impl-reviewer's deliverable, verified after each turn: a parseable
/// review file with anchored findings and an untouched checkout. The Err
/// text feeds a corrective prompt.
fn read_review(worktree: &Path) -> std::result::Result<ImplReviewFile, String> {
    let raw = std::fs::read_to_string(worktree.join(REVIEW_FILE)).map_err(|_| {
        format!("- review file `{REVIEW_FILE}` does not exist (write it as instructed)")
    })?;
    let review: ImplReviewFile = serde_json::from_str(raw.trim()).map_err(|e| {
        format!(
            "- review file `{REVIEW_FILE}` is not valid JSON ({e}); expected \
             {{\"verdict\": \"clean\" | \"findings\", \"review\": \"<Markdown>\", \
             \"findings\": [{{\"path\": ..., \"line\": ..., \"body\": ...}}]}}"
        )
    })?;
    if review.verdict == ReviewVerdict::Findings && review.review.trim().is_empty() {
        return Err(format!(
            "- verdict is \"findings\" but `review` in `{REVIEW_FILE}` is empty; \
             summarize every finding"
        ));
    }
    if review.verdict == ReviewVerdict::Clean && !review.findings.is_empty() {
        return Err(format!(
            "- verdict is \"clean\" but `findings` in `{REVIEW_FILE}` is not \
             empty; a clean review must not open threads — move the remarks \
             into `review` or change the verdict"
        ));
    }
    for f in &review.findings {
        if f.path.trim().is_empty() || f.line == 0 || f.body.trim().is_empty() {
            return Err(format!(
                "- every `findings` entry in `{REVIEW_FILE}` needs a non-empty \
                 `path`, a `line` >= 1 on the NEW side of the diff, and a \
                 non-empty `body`"
            ));
        }
    }
    Ok(review)
}

/// execute: one review turn (plus at most one corrective turn), then the
/// verdict lands in the checkpoint. Verification is the orchestrator's, not
/// the agent's: the review file must parse and the checkout must be exactly
/// the head we claimed.
async fn execute(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut ImplReviewCheckpoint,
    worktree: &Path,
) -> Result<flow::StepFlow> {
    // Drop the diff where the prompt says it is (idempotent on resume).
    let diff = deps.forge.pr_diff(run.issue_number).await?;
    std::fs::create_dir_all(worktree.join(crate::turn::prompts::MEGURI_DIR))?;
    std::fs::write(worktree.join(DIFF_FILE), &diff)?;

    let mut prompt = execute_prompt(run, cp, deps.config.language_for(&deps.project));
    let mut corrective_turns = 0u32;

    loop {
        let (outcome, _) = flow::run_turn(deps, run, worktree, "impl-review", &prompt).await?;
        let result = match outcome {
            TurnOutcome::Completed(r) => r,
            TurnOutcome::Stopped => return Ok(flow::StepFlow::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(flow::StepFlow::Interrupted(
                    "pane died during impl review".into(),
                ));
            }
        };

        match result.status {
            TurnStatus::Success => {}
            TurnStatus::Failure => {
                return Err(NeedsHuman(format!(
                    "agent reported failure reviewing PR #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
            // needs_plan is a worker signal and decompose a planner one; on
            // a review turn a human looks.
            TurnStatus::NeedsHuman | TurnStatus::NeedsPlan | TurnStatus::Decompose => {
                return Err(NeedsHuman(format!(
                    "agent needs a human reviewing PR #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
        }

        // Trust but verify: the checkout must be pristine and still at the
        // claimed head, and the review file must parse.
        let clean = gitops::status_clean(worktree).await?;
        let head_now = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
        let problem = if !clean || head_now != cp.head_sha {
            Some(format!(
                "- the review checkout must stay untouched: working tree clean: \
                 {clean} (must be true), HEAD: {head_now} (must be {expected}) — \
                 discard all changes (`git checkout -- . && git clean -fd && \
                 git reset --hard {expected}`), the review file under .meguri/ \
                 is exempt",
                expected = cp.head_sha,
            ))
        } else {
            read_review(worktree).err()
        };
        let Some(problem) = problem else {
            let review = read_review(worktree).expect("verified above");
            cp.verdict = Some(review.verdict);
            cp.review = review.review;
            cp.findings = review.findings;
            deps.store.emit(
                Some(&run.id),
                "impl_review.verified",
                json!({ "verdict": review.verdict, "head": cp.head_sha,
                        "findings": cp.findings.len() }),
            )?;
            return Ok(flow::StepFlow::Continue);
        };

        corrective_turns += 1;
        if corrective_turns > 1 {
            return Err(NeedsHuman(format!(
                "agent claimed success but the review doesn't verify after a \
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

fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The review body posted via create_pr_review alongside the inline threads.
fn review_body(cp: &ImplReviewCheckpoint) -> String {
    format!(
        "🔁 **meguri impl review** — findings at `{}`:\n\n{}\n\n\
         The inline threads below carry the individual findings; push fixes \
         to this branch and meguri will re-review the new head.",
        short_sha(&cp.head_sha),
        cp.review.trim()
    )
}

/// The marked summary comment (the idempotency and rounds record — PR review
/// bodies don't show up in `pr_comments`, conversation comments do).
fn marker_comment(cp: &ImplReviewCheckpoint, verdict: ReviewVerdict) -> String {
    let marker = impl_review_marker(&cp.head_sha);
    let short = short_sha(&cp.head_sha);
    match verdict {
        ReviewVerdict::Clean => {
            let mut body = format!(
                "{marker}\n🔁 **meguri impl review** — clean at `{short}`; \
                 nothing blocks the human merge gate."
            );
            let review = cp.review.trim();
            if !review.is_empty() {
                body.push_str(&format!("\n\n{review}"));
            }
            body
        }
        ReviewVerdict::Findings => format!(
            "{marker}\n🔁 **meguri impl review** — {n} finding(s) at `{short}`, \
             posted as inline review threads.",
            n = cp.findings.len()
        ),
    }
}

/// Fallback when the forge rejects the inline review (e.g. an anchor not on
/// the diff): fold everything into the marked comment so the review isn't
/// lost. It won't feed the fixer — that's why it is logged as an event.
fn fallback_comment(cp: &ImplReviewCheckpoint) -> String {
    let findings = cp
        .findings
        .iter()
        .map(|f| format!("- `{}:{}` — {}", f.path, f.line, f.body))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{marker}\n🔁 **meguri impl review** — findings at `{short}` (inline \
         threads could not be created; findings follow):\n\n{review}\n\n{findings}",
        marker = impl_review_marker(&cp.head_sha),
        short = short_sha(&cp.head_sha),
        review = cp.review.trim(),
    )
}

/// settle: post the findings as inline review threads plus the marked
/// summary comment (once per head — the marker makes re-runs after an
/// interruption idempotent), then release the claim. No label transitions:
/// the threads themselves are the handoff to the fixer.
async fn settle(deps: &Deps, run: &RunRecord, cp: &ImplReviewCheckpoint) -> Result<String> {
    let pr = run.issue_number;
    let verdict = cp.verdict.context("checkpoint has no review verdict")?;

    let comments = deps.forge.pr_comments(pr).await?;
    if !head_already_reviewed(&comments, &cp.head_sha) {
        let comment = if verdict == ReviewVerdict::Findings && !cp.findings.is_empty() {
            let drafts: Vec<ReviewCommentDraft> = cp
                .findings
                .iter()
                .map(|f| ReviewCommentDraft {
                    path: f.path.clone(),
                    line: f.line,
                    body: f.body.clone(),
                })
                .collect();
            match deps
                .forge
                .create_pr_review(pr, &review_body(cp), &drafts)
                .await
            {
                Ok(()) => marker_comment(cp, verdict),
                Err(e) => {
                    deps.store.emit(
                        Some(&run.id),
                        "impl_review.fallback",
                        json!({ "error": format!("{e:#}"), "findings": cp.findings.len() }),
                    )?;
                    fallback_comment(cp)
                }
            }
        } else {
            marker_comment(cp, verdict)
        };
        deps.forge.comment_pr(pr, &comment).await?;
        deps.store.emit(
            Some(&run.id),
            "impl_review.posted",
            json!({ "verdict": verdict, "head": cp.head_sha,
                    "findings": cp.findings.len() }),
        )?;
    }

    deps.forge
        .remove_pr_label(pr, forge::LABEL_WORKING)
        .await
        .ok();
    Ok(cp.pr_url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_matches_only_its_own_head_and_kind() {
        let comments = vec![
            "unrelated chatter".to_string(),
            format!("{}\nreview body", impl_review_marker("abc123")),
            // The spec reviewer's marker must not count as an impl review.
            format!(
                "{}\nspec review",
                super::super::reviewer::review_marker("abc123")
            ),
        ];
        assert!(head_already_reviewed(&comments, "abc123"));
        assert!(!head_already_reviewed(&comments, "def456"));
        assert!(!head_already_reviewed(&[], "abc123"));
    }

    #[test]
    fn rounds_count_markers_across_heads() {
        let comments = vec![
            format!("{}\nround 1", impl_review_marker("aaa")),
            "chatter".to_string(),
            format!("{}\nround 2", impl_review_marker("bbb")),
            // Spec-review markers don't consume impl-review rounds.
            format!("{}\nspec", super::super::reviewer::review_marker("ccc")),
        ];
        assert_eq!(review_rounds(&comments), 2);
        assert_eq!(review_rounds(&[]), 0);
    }

    #[test]
    fn review_file_parses_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".meguri")).unwrap();
        let path = dir.path().join(REVIEW_FILE);

        let err = read_review(dir.path()).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");

        std::fs::write(&path, "not json").unwrap();
        let err = read_review(dir.path()).unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");

        std::fs::write(&path, r#"{"verdict":"findings","review":"  "}"#).unwrap();
        let err = read_review(dir.path()).unwrap_err();
        assert!(err.contains("empty"), "{err}");

        // Clean must not open threads.
        std::fs::write(
            &path,
            r#"{"verdict":"clean","review":"ok","findings":[{"path":"a.rs","line":1,"body":"x"}]}"#,
        )
        .unwrap();
        let err = read_review(dir.path()).unwrap_err();
        assert!(err.contains("clean"), "{err}");

        // Findings entries must be fully anchored.
        std::fs::write(
            &path,
            r#"{"verdict":"findings","review":"r","findings":[{"path":"","line":1,"body":"x"}]}"#,
        )
        .unwrap();
        let err = read_review(dir.path()).unwrap_err();
        assert!(err.contains("non-empty"), "{err}");
        std::fs::write(
            &path,
            r#"{"verdict":"findings","review":"r","findings":[{"path":"a.rs","line":0,"body":"x"}]}"#,
        )
        .unwrap();
        assert!(read_review(dir.path()).is_err());

        std::fs::write(
            &path,
            r#"{"verdict":"findings","review":"- bug","findings":[{"path":"src/a.rs","line":42,"body":"off by one"}]}"#,
        )
        .unwrap();
        let review = read_review(dir.path()).unwrap();
        assert_eq!(review.verdict, ReviewVerdict::Findings);
        assert_eq!(review.findings.len(), 1);
        assert_eq!(review.findings[0].line, 42);

        // Summary-only findings (no anchorable line) are allowed.
        std::fs::write(&path, r#"{"verdict":"findings","review":"- global"}"#).unwrap();
        assert!(read_review(dir.path()).unwrap().findings.is_empty());

        std::fs::write(&path, r#"{"verdict":"clean"}"#).unwrap();
        assert_eq!(
            read_review(dir.path()).unwrap().verdict,
            ReviewVerdict::Clean
        );
    }

    #[test]
    fn prompt_demands_anchored_findings_not_changes() {
        let run = fake_run(12);
        let cp = ImplReviewCheckpoint {
            pr_title: "Add caching (#5)".into(),
            pr_body: "Closes #5.".into(),
            head_branch: "meguri/5-add-caching-abc".into(),
            head_sha: "deadbeef".into(),
            ..Default::default()
        };
        let prompt = execute_prompt(&run, &cp, None);
        assert!(prompt.contains("# PR: Add caching (#5)"));
        assert!(prompt.contains(DIFF_FILE));
        assert!(prompt.contains(REVIEW_FILE));
        assert!(prompt.contains("Do NOT modify"));
        assert!(prompt.contains("findings"));
        assert!(prompt.contains("NEW side of the diff"));
        assert!(prompt.contains("deadbeef"));
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn comments_carry_marker_and_verdict() {
        let cp = ImplReviewCheckpoint {
            head_sha: "0123456789abcdef".into(),
            review: "- looks solid".into(),
            findings: vec![Finding {
                path: "src/a.rs".into(),
                line: 7,
                body: "handle the None case".into(),
            }],
            ..Default::default()
        };
        let marker = impl_review_marker("0123456789abcdef");

        let findings = marker_comment(&cp, ReviewVerdict::Findings);
        assert!(findings.contains(&marker));
        assert!(findings.contains("1 finding(s)"));
        assert!(findings.contains("`0123456789ab`"), "{findings}");

        let clean = marker_comment(&cp, ReviewVerdict::Clean);
        assert!(clean.contains(&marker));
        assert!(clean.contains("- looks solid"));

        let body = review_body(&cp);
        assert!(body.contains("- looks solid"));
        assert!(body.contains("re-review"));

        let fallback = fallback_comment(&cp);
        assert!(fallback.contains(&marker));
        assert!(fallback.contains("`src/a.rs:7`"));
        assert!(fallback.contains("handle the None case"));
    }

    #[test]
    fn unreviewable_prs_are_named() {
        let pr = |labels: &[&str], head: &str, state: &str| PullRequest {
            number: 9,
            title: "t".into(),
            body: String::new(),
            url: String::new(),
            head_branch: head.into(),
            head_sha: "sha".into(),
            state: state.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        };
        assert!(pr_not_reviewable(&pr(&[], "meguri/x", "open")).is_none());
        assert!(pr_not_reviewable(&pr(&[], "feature/x", "open")).is_some());
        assert!(pr_not_reviewable(&pr(&[], "meguri/x", "merged")).is_some());
        for label in [
            forge::LABEL_SPEC_REVIEWING,
            forge::LABEL_SPEC_READY,
            forge::LABEL_WORKING,
            forge::LABEL_HOLD,
        ] {
            assert!(
                pr_not_reviewable(&pr(&[label], "meguri/x", "open")).is_some(),
                "{label} must exclude the PR"
            );
        }
    }

    fn fake_run(pr: i64) -> RunRecord {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run_for_loop("proj", KIND, pr, "t").unwrap();
        store.get_run(&run.id).unwrap().unwrap()
    }
}
