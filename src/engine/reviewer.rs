//! The reviewer loop: open PR labeled `meguri:spec-reviewing` → detached
//! worktree at the PR head → an agent turn reads the diff and writes a
//! summary review. A clean review flips the PR to `meguri:spec-ready`; a
//! review with findings is posted as a PR comment and the loop waits for the
//! next push. Every review comment embeds a head-sha marker, so the same
//! head is never reviewed twice ("Authority": what was reviewed is recorded
//! on the PR, not in local state). Inline review threads are future work —
//! a summary comment is the deliverable for now.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use super::WorkerOutcome;
use super::flow::{self, NeedsHuman, STEP_EXECUTE, STEP_PREPARE_WORK, STEP_PREPARE_WORKTREE};
use super::{Deps, Target};
use crate::forge::{self, PullRequest};
use crate::gitops;
use crate::mux::PaneId;
use crate::store::{RunRecord, RunStatus};
use crate::tasks::TaskKey;
use crate::turn::{TurnOutcome, TurnStatus};

/// `runs.loop_kind` value for reviewer runs.
pub const KIND: &str = "reviewer";

/// Terminal reviewer step: post the review, settle the PR labels.
pub const STEP_SETTLE: &str = "settle";

/// Where the orchestrator drops the PR diff for the agent to read
/// (worktree-relative; `.meguri/` is git-excluded, so it never dirties the
/// tree).
pub const DIFF_FILE: &str = ".meguri/pr-diff.patch";
/// Where the agent writes its verdict + review body (worktree-relative).
pub const REVIEW_FILE: &str = ".meguri/review.json";

/// Hidden marker embedded in every review comment. Its presence for a head
/// sha means that head was already reviewed — the idempotency key across
/// restarts, re-discoveries, and hosts.
pub fn review_marker(head_sha: &str) -> String {
    format!("<!-- meguri:review head={head_sha} -->")
}

pub fn head_already_reviewed(comments: &[String], head_sha: &str) -> bool {
    let marker = review_marker(head_sha);
    comments.iter().any(|c| c.contains(&marker))
}

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
pub struct ReviewCheckpoint {
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
}

/// The reviewer as a schedulable loop: `meguri:spec-reviewing` PRs in,
/// review comments (and `meguri:spec-ready` transitions) out.
pub struct ReviewerLoop;

#[async_trait]
impl super::Loop for ReviewerLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    /// Open PRs carrying the review label whose *current head* has no review
    /// comment yet. Deliberately not `issue_has_succeeded_run`-guarded: a
    /// succeeded findings-review must not block re-reviewing the next push —
    /// the head-sha marker is the dedup.
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        if deps.forge.is_none() {
            return Ok(Vec::new()); // PR loops are inert in local mode
        }
        let prs = deps
            .forge()
            .list_prs_with_label(forge::LABEL_SPEC_REVIEWING)
            .await?;
        let mut targets = Vec::new();
        for pr in prs {
            if pr.has_label(forge::LABEL_HOLD) || pr.has_label(forge::LABEL_WORKING) {
                continue;
            }
            let comments = deps.forge().pr_comments(pr.number).await?;
            if head_already_reviewed(&comments, &pr.head_sha) {
                continue;
            }
            targets.push(Target {
                key: TaskKey::Issue(pr.number),
                title: pr.title,
            });
        }
        Ok(targets)
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
        run_reviewer(deps, run_id).await
    }
}

pub async fn run_reviewer(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
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
    let mut cp: ReviewCheckpoint = serde_json::from_str(&run.checkpoint_json).unwrap_or_default();
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
        flow::cleanup_pane(deps, &run, true).await;
        return Ok(WorkerOutcome::Succeeded { pr_url });
    }

    bail!("unknown step {step:?}");
}

fn save_step(deps: &Deps, run: &RunRecord, step: &str, cp: &ReviewCheckpoint) -> Result<String> {
    deps.store
        .update_run_step(&run.id, step, &serde_json::to_string(cp)?)?;
    Ok(step.to_string())
}

/// `meguri stop`: cancel the run, release the PR claim, kill the pane.
async fn finalize_cancelled(deps: &Deps, run: &RunRecord) -> Result<()> {
    deps.store
        .update_run_status(&run.id, RunStatus::Cancelled, None)?;
    deps.forge()
        .remove_pr_label(run.issue_number, forge::LABEL_WORKING)
        .await
        .ok();
    if let Some(pane_id) = &run.mux_pane_id {
        let _ = deps.mux.kill_pane(&PaneId(pane_id.clone())).await;
    }
    deps.store.emit(Some(&run.id), "run.cancelled", json!({}))?;
    Ok(())
}

/// Failure escalation on the PR (mirrors the worker's issue escalation).
async fn escalate_on_pr(deps: &Deps, pr: i64, reason: &str) {
    let _ = deps
        .forge()
        .add_pr_label(pr, forge::LABEL_NEEDS_HUMAN)
        .await;
    let _ = deps.forge().remove_pr_label(pr, forge::LABEL_WORKING).await;
    let _ = deps
        .forge()
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
/// `meguri:working`. A hold, a missing review label, or a head that got its
/// review while we queued is a benign race — skip, don't escalate.
async fn prepare_work(deps: &Deps, run: &RunRecord) -> Result<Prepared> {
    let pr = deps.forge().get_pr(run.issue_number).await?;
    if pr.has_label(forge::LABEL_HOLD) {
        return Ok(Prepared::Skip(format!(
            "PR #{} is on hold ({})",
            pr.number,
            forge::LABEL_HOLD
        )));
    }
    if !pr.has_label(forge::LABEL_SPEC_REVIEWING) {
        return Ok(Prepared::Skip(format!(
            "PR #{} is not labeled {} (removed since discovery?)",
            pr.number,
            forge::LABEL_SPEC_REVIEWING
        )));
    }
    let comments = deps.forge().pr_comments(pr.number).await?;
    if head_already_reviewed(&comments, &pr.head_sha) {
        return Ok(Prepared::Skip(format!(
            "PR #{} head {} already carries a review",
            pr.number, pr.head_sha
        )));
    }
    deps.forge()
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
async fn prepare_worktree(deps: &Deps, run: &RunRecord, cp: &ReviewCheckpoint) -> Result<()> {
    let root = deps
        .project
        .worktree_root
        .clone()
        .unwrap_or_else(crate::config::worktrees_root);
    let dir = format!("review-{}-{}", run.issue_number, run.id);
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

fn execute_prompt(run: &RunRecord, cp: &ReviewCheckpoint, language: Option<&str>) -> String {
    format!(
        "You are reviewing pull request #{number} in this repository. The \
         worktree is checked out read-only at the PR head (commit \
         `{sha}`, branch `{branch}`).\n\n\
         # PR: {title}\n\n{body}\n\n\
         # Instructions\n\
         - Read the PR's full diff at `{diff}`; browse the checked-out code \
           for context as needed.\n\
         - Review the change for correctness, completeness, and fit with the \
           repository's conventions. A summary-style review is enough — no \
           inline threads.\n\
         - Do NOT modify, commit, or push anything; the review file below is \
           your only deliverable.\n\
         - Write your review to `{review}` as JSON:\n\
           `{{\"verdict\": \"clean\" | \"findings\", \"review\": \"<Markdown review comment>\"}}`\n\
           - \"clean\": nothing must change before this PR can proceed \
             (pure nitpicks do not block; mention them in `review`).\n\
           - \"findings\": something must change; list every finding in \
             `review` so the author can fix and push.\n\
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

/// The reviewer's deliverable, verified after each turn: a parseable review
/// file and an untouched checkout. The Err text feeds a corrective prompt.
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
/// verdict lands in the checkpoint. Verification is the orchestrator's, not
/// the agent's: the review file must parse and the checkout must be exactly
/// the head we claimed.
async fn execute(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut ReviewCheckpoint,
    worktree: &Path,
) -> Result<flow::StepFlow> {
    // Drop the diff where the prompt says it is (idempotent on resume).
    let diff = deps.forge().pr_diff(run.issue_number).await?;
    std::fs::create_dir_all(worktree.join(crate::turn::prompts::MEGURI_DIR))?;
    std::fs::write(worktree.join(DIFF_FILE), &diff)?;

    let mut prompt = execute_prompt(run, cp, deps.config.language_for(&deps.project));
    let mut corrective_turns = 0u32;

    loop {
        let (outcome, _) = flow::run_turn(deps, run, worktree, "review", &prompt).await?;
        let result = match outcome {
            TurnOutcome::Completed(r) => r,
            TurnOutcome::Stopped => return Ok(flow::StepFlow::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(flow::StepFlow::Interrupted(
                    "pane died during review".into(),
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
            deps.store.emit(
                Some(&run.id),
                "review.verified",
                json!({ "verdict": review.verdict, "head": cp.head_sha }),
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

/// The PR comment carrying the review (and the idempotency marker).
fn review_comment(cp: &ReviewCheckpoint, verdict: ReviewVerdict) -> String {
    let marker = review_marker(&cp.head_sha);
    let short = cp.head_sha.get(..12).unwrap_or(&cp.head_sha);
    let review = cp.review.trim();
    match verdict {
        ReviewVerdict::Clean => {
            let mut body = format!(
                "{marker}\n🔁 **meguri review** — clean at `{short}`; moving to `{}`.",
                forge::LABEL_SPEC_READY
            );
            if !review.is_empty() {
                body.push_str(&format!("\n\n{review}"));
            }
            body
        }
        ReviewVerdict::Findings => format!(
            "{marker}\n🔁 **meguri review** — findings at `{short}`:\n\n{review}\n\n\
             Push fixes to this branch and meguri will re-review the new head."
        ),
    }
}

/// settle: post the review comment (once per head — the marker makes re-runs
/// after an interruption idempotent) and settle the labels. The spec-ready
/// label is load-bearing (the worker's continuation keys off it), so failing
/// to apply it fails the run instead of passing silently.
async fn settle(deps: &Deps, run: &RunRecord, cp: &ReviewCheckpoint) -> Result<String> {
    let pr = run.issue_number;
    let verdict = cp.verdict.context("checkpoint has no review verdict")?;

    let comments = deps.forge().pr_comments(pr).await?;
    if !head_already_reviewed(&comments, &cp.head_sha) {
        deps.forge()
            .comment_pr(pr, &review_comment(cp, verdict))
            .await?;
        deps.store.emit(
            Some(&run.id),
            "review.posted",
            json!({ "verdict": verdict, "head": cp.head_sha }),
        )?;
    }

    if verdict == ReviewVerdict::Clean {
        deps.forge()
            .add_pr_label(pr, forge::LABEL_SPEC_READY)
            .await?;
        deps.forge()
            .remove_pr_label(pr, forge::LABEL_SPEC_REVIEWING)
            .await
            .ok();
    }
    // Findings: keep `meguri:spec-reviewing` — the next push moves the head
    // past the marker and discovery re-reviews it.
    deps.forge()
        .remove_pr_label(pr, forge::LABEL_WORKING)
        .await
        .ok();
    Ok(cp.pr_url.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_matches_only_its_own_head() {
        let comments = vec![
            "unrelated chatter".to_string(),
            format!("{}\nreview body", review_marker("abc123")),
        ];
        assert!(head_already_reviewed(&comments, "abc123"));
        assert!(!head_already_reviewed(&comments, "def456"));
        assert!(!head_already_reviewed(&[], "abc123"));
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

        std::fs::write(&path, r#"{"verdict":"findings","review":"- bug"}"#).unwrap();
        let review = read_review(dir.path()).unwrap();
        assert_eq!(review.verdict, ReviewVerdict::Findings);
        assert_eq!(review.review, "- bug");

        std::fs::write(&path, r#"{"verdict":"clean"}"#).unwrap();
        assert_eq!(
            read_review(dir.path()).unwrap().verdict,
            ReviewVerdict::Clean
        );
    }

    #[test]
    fn prompt_demands_review_not_changes() {
        let run = fake_run(12);
        let cp = ReviewCheckpoint {
            pr_title: "Spec: Add caching (#5)".into(),
            pr_body: "Closes #5.".into(),
            head_branch: "meguri/5-add-caching-abc".into(),
            head_sha: "deadbeef".into(),
            ..Default::default()
        };
        let prompt = execute_prompt(&run, &cp, None);
        assert!(prompt.contains("# PR: Spec: Add caching (#5)"));
        assert!(prompt.contains(DIFF_FILE));
        assert!(prompt.contains(REVIEW_FILE));
        assert!(prompt.contains("Do NOT modify"));
        assert!(prompt.contains("deadbeef"));
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn prompt_pins_output_language_when_configured() {
        let run = fake_run(12);
        let cp = ReviewCheckpoint::default();
        let prompt = execute_prompt(&run, &cp, Some("日本語"));
        assert!(prompt.contains("# Output language"));
        assert!(prompt.contains("日本語"));
    }

    #[test]
    fn review_comment_carries_marker_and_verdict() {
        let cp = ReviewCheckpoint {
            head_sha: "0123456789abcdef".into(),
            review: "- missing acceptance criteria".into(),
            ..Default::default()
        };
        let dirty = review_comment(&cp, ReviewVerdict::Findings);
        assert!(dirty.contains(&review_marker("0123456789abcdef")));
        assert!(dirty.contains("`0123456789ab`"), "{dirty}");
        assert!(dirty.contains("- missing acceptance criteria"));
        assert!(dirty.contains("re-review"));

        let clean = review_comment(
            &ReviewCheckpoint {
                head_sha: "abc".into(), // shorter than the display width
                ..Default::default()
            },
            ReviewVerdict::Clean,
        );
        assert!(clean.contains(&review_marker("abc")));
        assert!(clean.contains(forge::LABEL_SPEC_READY));
    }

    fn fake_run(pr: i64) -> RunRecord {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run_for_loop("proj", KIND, pr, "t").unwrap();
        store.get_run(&run.id).unwrap().unwrap()
    }
}
