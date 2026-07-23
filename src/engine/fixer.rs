//! The fixer loop: an open meguri PR with unresolved review comments →
//! worktree attached to the PR's existing branch → agent addresses the
//! comments → verified fix commits pushed to the same PR → reply on each
//! thread and wait for the reviewer's re-review.
//!
//! Convergence of the reviewer↔fixer ping-pong lives on the forge, not in
//! local state: a thread is actionable only while its *last* comment is the
//! reviewer's. After pushing, the fixer replies on every thread it
//! addressed, which parks the thread until the reviewer either resolves it
//! (done) or answers again (next fixer round). Spec-ready and merged PRs are
//! the spec worker's and the humans' territory — the fixer never touches them.
//!
//! Since ADR 0006 the AI's implementation review is an internal loop that
//! never posts threads, so the fixer's discovery naturally narrows to
//! **human and external-bot** review threads — GitHub stays the transport
//! only where a human actually sits.
//!
//! Lifetime (issue #92): runs are keyed by the PR's canonical *issue*
//! (recovered from the `meguri/<issue>-…` head branch), so the fixer joins
//! the issue's author lane — same pane, same live session as the worker or
//! planner that wrote the branch — and the review-fix context continues
//! where the implementation left off. The worktree attaches to the PR head;
//! the pane is kept and reclaimed when the issue closes.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor, PreparedWork};
use super::{Deps, is_combined, open_pr_for_issue, pr_is_touchable};
use crate::forge::{self, ReviewThread};
use crate::store::RunRecord;
use serde_json::json;

/// `runs.loop_kind` value for fixer runs.
pub const KIND: &str = "fixer";

/// Reply prefix that marks a thread as "addressed, awaiting re-review".
/// Discovery treats a thread whose last comment starts with this as parked.
pub const FIXER_REPLY_MARKER: &str = "🔁 meguri";

/// A thread the fixer still owes a fix: unresolved, and the ball is in
/// meguri's court (the last comment is not meguri's reply).
pub fn thread_awaits_fixer(thread: &ReviewThread) -> bool {
    !thread.resolved
        && thread
            .comments
            .last()
            .is_some_and(|c| !c.body.starts_with(FIXER_REPLY_MARKER))
}

pub async fn run_fixer(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
    flow::run_flow(deps, run_id, &FixerFlavor).await
}

/// Markdown listing of the threads for the execute prompt.
fn render_threads(threads: &[ReviewThread]) -> String {
    threads
        .iter()
        .map(|t| {
            let location = match (&t.path, t.line) {
                (Some(path), Some(line)) => format!("`{path}` line {line}"),
                (Some(path), None) => format!("`{path}`"),
                _ => "(no file anchor)".to_string(),
            };
            let comments = t
                .comments
                .iter()
                .map(|c| format!("  - **{}**: {}", c.author, c.body))
                .collect::<Vec<_>>()
                .join("\n");
            format!("- {location} (thread `{id}`):\n{comments}", id = t.id)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct FixerFlavor;

#[async_trait]
impl Flavor for FixerFlavor {
    /// Unused: the fixer's [`Flavor::prepare_work`] override claims by PR
    /// state and review threads, not by an issue label.
    fn trigger_label(&self) -> &'static str {
        ""
    }

    /// Re-resolve the PR from the run's canonical issue, then claim it
    /// (labels live on the PR, not the issue). Any change that makes the PR
    /// unfixable between discovery and claim is a benign race — skip, don't
    /// escalate.
    async fn prepare_work(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &mut Checkpoint,
    ) -> Result<PreparedWork> {
        let Some(pr) = open_pr_for_issue(deps, run.issue_number).await? else {
            return Ok(PreparedWork::Skip(format!(
                "no single open PR resolves to issue #{} (changed since discovery?)",
                run.issue_number
            )));
        };
        if let Some(reason) = pr_is_touchable(&pr, is_combined(deps)) {
            return Ok(PreparedWork::Skip(reason));
        }
        let threads: Vec<ReviewThread> = deps
            .forge()
            .list_review_threads(pr.number)
            .await?
            .into_iter()
            .filter(thread_awaits_fixer)
            .collect();
        if threads.is_empty() {
            return Ok(PreparedWork::Skip(format!(
                "PR #{} has no review comments awaiting a fix",
                pr.number
            )));
        }

        deps.forge()
            .add_pr_label(pr.number, forge::LABEL_WORKING)
            .await?;
        deps.store
            .emit(Some(&run.id), "pr.claimed", json!({ "pr": pr.number }))?;

        cp.issue_title = pr.title.clone();
        cp.issue_body = render_threads(&threads);
        cp.head_branch = Some(pr.head_branch.clone());
        cp.thread_ids = threads.iter().map(|t| t.id.clone()).collect();
        // The PR already exists: open-pr must only push and settle.
        cp.pr_number = Some(pr.number);
        cp.pr_url = Some(pr.url.clone());
        Ok(PreparedWork::Claimed)
    }

    /// Attach to the PR's existing branch instead of cutting a new one.
    async fn prepare_worktree(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        flow::attach_pr_worktree(deps, run, cp).await
    }

    fn execute_prompt(
        &self,
        _deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
        _worktree: &Path,
    ) -> String {
        format!(
            "You are addressing review comments on pull request #{number} \
             \"{title}\" in this repository (branch `{branch}`, a dedicated \
             worktree attached to the PR's branch).\n\n\
             # Unresolved review comments\n\n{threads}\n\n\
             # Instructions\n\
             - Address every comment above. If a comment is wrong or you \
               deliberately deviate, explain why in your result summary.\n\
             - Follow the repository's existing conventions; update tests \
               affected by your changes.\n\
             - Run the relevant tests/checks yourself before declaring success.\n\
             - COMMIT all your work to the current branch with clear messages. \
               Leave the working tree clean.\n\
             - Do NOT push and do NOT reply to the review comments; meguri \
               handles both.\n\
             - Do NOT switch branches, do NOT rebase, and do NOT touch other \
               worktrees.",
            number = cp.pr_number.unwrap_or(run.issue_number),
            title = cp.issue_title,
            branch = run.branch.as_deref().unwrap_or("?"),
            threads = cp.issue_body,
        )
        // The completion contract is appended by prepare_turn.
    }

    fn verify_work(
        &self,
        _run: &RunRecord,
        _cp: &Checkpoint,
        _worktree: &Path,
    ) -> std::result::Result<(), String> {
        Ok(()) // committed fixes are all the fixer requires
    }

    /// New commits are counted against the PR branch's pushed tip, not the
    /// default branch (the PR is already ahead of that).
    fn verify_base(&self, deps: &Deps, run: &RunRecord) -> String {
        run.branch
            .clone()
            .unwrap_or_else(|| deps.project.default_branch.clone())
    }

    /// Unused: the PR already exists, so open-pr never creates one.
    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String {
        flow::default_pr_title(run, cp)
    }

    /// Fixing review comments doesn't change the nature of the change
    /// (issue #136): keep the subject the establishing turn set instead of
    /// letting a fix's wording flap the PR title.
    fn sets_subject(&self) -> bool {
        false
    }

    /// After the push: park every addressed thread with a marker reply (this
    /// is what keeps discovery quiet until the reviewer answers), then
    /// release the claim. The replies are load-bearing — failing to post
    /// them would re-trigger the fixer forever — so errors fail the run.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        let pr = cp.pr_number.context("fixer checkpoint has no PR number")?;
        for thread_id in &cp.thread_ids {
            deps.forge()
                .reply_review_thread(
                    pr,
                    thread_id,
                    &format!(
                        "{FIXER_REPLY_MARKER} pushed a fix for this (run `{}`); \
                         please re-review.",
                        run.id
                    ),
                )
                .await?;
        }
        deps.store.emit(
            Some(&run.id),
            "threads.replied",
            json!({ "pr": pr, "threads": cp.thread_ids }),
        )?;
        deps.forge()
            .remove_pr_label(pr, forge::LABEL_WORKING)
            .await
            .ok();
        Ok(())
    }

    async fn release_claim(&self, deps: &Deps, run: &RunRecord) {
        if let Some(pr) = flow::claimed_pr(deps, &run.id) {
            deps.forge()
                .remove_pr_label(pr, forge::LABEL_WORKING)
                .await
                .ok();
        }
    }

    /// Escalation lands on the claimed PR (the fixer's target); before the
    /// checkpoint knows the PR (prepare-work failed), the canonical issue
    /// gets the notice via the issue API instead.
    async fn escalate(&self, deps: &Deps, run: &RunRecord, reason: &str) {
        let Some(pr) = flow::claimed_pr(deps, &run.id) else {
            super::escalation::escalate_issue(deps, run.issue_number, reason).await;
            return;
        };
        // The central helper posts the label/comment/event; the closing hint is
        // launch-mode-aware (issue #169) — a direct-mode fixer has no pane.
        let comment = super::escalation::pr_needs_human_comment(
            "could not address the review comments on this PR and needs a human.",
            reason,
            &flow::attach_hint(deps, run),
        );
        super::escalation::escalate_pr(deps, pr, &comment).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::ReviewComment;

    #[test]
    fn fix_turns_never_establish_a_new_subject() {
        assert!(!FixerFlavor.sets_subject());
    }

    fn thread(resolved: bool, last_body: &str) -> ReviewThread {
        ReviewThread {
            id: "t1".into(),
            resolved,
            path: Some("src/lib.rs".into()),
            line: Some(10),
            comments: vec![
                ReviewComment {
                    author: "reviewer".into(),
                    body: "please rename".into(),
                },
                ReviewComment {
                    author: "someone".into(),
                    body: last_body.into(),
                },
            ],
        }
    }

    #[test]
    fn thread_actionability_follows_resolution_and_last_comment() {
        assert!(thread_awaits_fixer(&thread(false, "still wrong")));
        // Resolved: the reviewer accepted, nothing to do.
        assert!(!thread_awaits_fixer(&thread(true, "still wrong")));
        // Parked: meguri replied last, awaiting re-review.
        assert!(!thread_awaits_fixer(&thread(
            false,
            "🔁 meguri pushed a fix for this"
        )));
        // Degenerate thread without comments: nothing to address.
        assert!(!thread_awaits_fixer(&ReviewThread {
            comments: vec![],
            ..thread(false, "")
        }));
    }

    // Touchability (open / meguri branch / spec-ready / hold / working /
    // needs-human) is the shared `pr_is_touchable` guard's job — see its
    // tests in `engine::mod`.

    #[test]
    fn prompt_lists_threads_and_forbids_push() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(3);
        let cp = Checkpoint {
            issue_title: "Add feature (#9)".into(),
            issue_body: render_threads(&[thread(false, "still wrong")]),
            ..Default::default()
        };
        let prompt = FixerFlavor.execute_prompt(&fake_deps(), &run, &cp, dir.path());
        assert!(prompt.contains("review comments on pull request #3"));
        assert!(prompt.contains("`src/lib.rs` line 10"));
        assert!(prompt.contains("please rename"));
        assert!(prompt.contains("Do NOT push"));
    }

    fn fake_run(pr: i64) -> RunRecord {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run_for_loop("proj", KIND, pr, "t").unwrap();
        let mut run = store.get_run(&run.id).unwrap().unwrap();
        run.branch = Some("meguri/9-add-feature-abc123".into());
        run
    }

    fn fake_deps() -> Deps {
        use std::sync::Arc;
        let project = crate::config::ProjectConfig {
            id: "proj".into(),
            repo_path: Some("/tmp/unused".into()),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            check_command: None,
            worktree_root: None,
            language: None,
            pr: None,
            clean: None,
            triage: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup: Default::default(),
            schedules: Vec::new(),
            autonomy: None,
            cadence: Vec::new(),
            prompts: Default::default(),
            notify: None,
        };
        Deps::with_label_source(
            crate::store::Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            Arc::new(crate::forge::fake::FakeForge::default()),
            crate::config::Config::default(),
            project,
        )
    }
}
