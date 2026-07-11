//! The spec-worker loop: open PR labeled `meguri:spec-ready` (a reviewed
//! spec PR from the planner) → worktree attached to the PR's existing branch
//! → worker-style implementation turns with the spec as context → verified
//! commits pushed onto the same PR. The second entrance to implementation,
//! next to the worker's `meguri:ready` issues (issue #21).
//!
//! The spec PR and the implementation PR are the same PR: no new PR is ever
//! created here. The run is tied to the *issue* the branch encodes
//! (`meguri/<issue>-…`, the planner follows the worker's branch convention),
//! so run bookkeeping, dedup, and escalation match the worker's exactly.
//! Success consumes the PR's `meguri:spec-ready` label — from then on the
//! PR is a normal in-flight meguri implementation PR (fixer territory).

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor, PreparedWork};
use super::{Deps, Target};
use crate::forge::{self, PullRequest};
use crate::gitops;
use crate::store::RunRecord;

/// `runs.loop_kind` value for spec-worker runs.
pub const KIND: &str = "spec-worker";

/// The spec worker as a schedulable loop: `meguri:spec-ready` PRs in,
/// implementation commits on the same PR out.
pub struct SpecWorkerLoop;

#[async_trait]
impl super::Loop for SpecWorkerLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    /// Open spec-ready PRs that are actionable — not held, not claimed, on a
    /// worker-convention branch (a takeover needs the branch to encode its
    /// issue), and not already shipped by a succeeded run of this loop
    /// (avoids a second takeover when the label lingers; humans can force a
    /// rerun with `meguri run --issue N`).
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        let prs = deps
            .forge
            .list_prs_with_label(forge::LABEL_SPEC_READY)
            .await?;
        let mut targets = Vec::new();
        for pr in prs {
            if pr.state != "open"
                || pr.has_label(forge::LABEL_HOLD)
                || pr.has_label(forge::LABEL_WORKING)
            {
                continue;
            }
            let Some(issue) = gitops::branch_issue(&pr.head_branch) else {
                continue; // human-made head: not meguri's to take over
            };
            if deps
                .store
                .issue_has_succeeded_run(&deps.project.id, KIND, issue)?
            {
                continue;
            }
            targets.push(Target {
                issue_number: issue,
                title: pr.title,
            });
        }
        Ok(targets)
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
        run_spec_worker(deps, run_id).await
    }
}

pub async fn run_spec_worker(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
    flow::run_flow(deps, run_id, &SpecWorkerFlavor).await
}

/// The open spec-ready PR whose head branch encodes `issue`, if any.
async fn spec_ready_pr(deps: &Deps, issue: i64) -> Result<Option<PullRequest>> {
    Ok(deps
        .forge
        .list_prs_with_label(forge::LABEL_SPEC_READY)
        .await?
        .into_iter()
        .find(|pr| pr.state == "open" && gitops::branch_issue(&pr.head_branch) == Some(issue)))
}

/// The PR this run claimed, from its persisted checkpoint (release/escalate
/// hooks get the run record as of drive start, so re-read the store).
fn claimed_pr(deps: &Deps, run_id: &str) -> Option<i64> {
    let run = deps.store.get_run(run_id).ok().flatten()?;
    serde_json::from_str::<Checkpoint>(&run.checkpoint_json)
        .ok()
        .and_then(|cp| cp.pr_number)
}

struct SpecWorkerFlavor;

#[async_trait]
impl Flavor for SpecWorkerFlavor {
    /// Discovery-time label; the [`Flavor::prepare_work`] override re-checks
    /// it on the PR (not an issue).
    fn trigger_label(&self) -> &'static str {
        forge::LABEL_SPEC_READY
    }

    /// Claim the spec PR (labels live on the PR, the run is keyed by the
    /// issue). Any change that makes the PR untakeable between discovery and
    /// claim is a benign race — skip, don't escalate.
    async fn prepare_work(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &mut Checkpoint,
    ) -> Result<PreparedWork> {
        let Some(pr) = spec_ready_pr(deps, run.issue_number).await? else {
            return Ok(PreparedWork::Skip(format!(
                "no open {} PR for issue #{} (label removed since discovery?)",
                forge::LABEL_SPEC_READY,
                run.issue_number
            )));
        };
        if pr.has_label(forge::LABEL_HOLD) {
            return Ok(PreparedWork::Skip(format!(
                "PR #{} is on hold ({})",
                pr.number,
                forge::LABEL_HOLD
            )));
        }
        if pr.has_label(forge::LABEL_WORKING) {
            return Ok(PreparedWork::Skip(format!(
                "PR #{} is already claimed ({})",
                pr.number,
                forge::LABEL_WORKING
            )));
        }
        deps.forge
            .add_pr_label(pr.number, forge::LABEL_WORKING)
            .await?;
        deps.store.emit(
            Some(&run.id),
            "pr.claimed",
            json!({ "pr": pr.number, "issue": run.issue_number }),
        )?;

        // The prompt carries the issue (what to build) plus the spec (how).
        let issue = deps.forge.get_issue(run.issue_number).await?;
        cp.issue_title = issue.title;
        cp.issue_body = issue.body;
        cp.head_branch = Some(pr.head_branch);
        // The PR already exists: open-pr must only push and settle.
        cp.pr_number = Some(pr.number);
        cp.pr_url = Some(pr.url);
        Ok(PreparedWork::Claimed)
    }

    /// Take over the spec PR's branch instead of cutting a new one.
    async fn prepare_worktree(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        flow::attach_pr_worktree(deps, run, cp).await
    }

    fn execute_prompt(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
        worktree: &Path,
    ) -> String {
        let spec_path = super::planner::spec_rel_path(run.issue_number);
        let spec = std::fs::read_to_string(worktree.join(&spec_path)).unwrap_or_else(|_| {
            "(spec file missing — follow the issue and the branch's existing commits)".to_string()
        });
        format!(
            "You are implementing GitHub issue #{number} in this repository \
             (branch `{branch}`, a dedicated worktree attached to the spec \
             PR's branch). The approach was already agreed in the reviewed \
             spec below; continue on this same branch — the spec PR becomes \
             the implementation PR.\n\n\
             # Issue: {title}\n\n{body}\n\n\
             # Reviewed spec (`{spec_path}`)\n\n{spec}\n\n\
             # Instructions\n\
             - Follow the reviewed spec; it is the agreed approach. If \
               reality forces a deviation, explain it in your result summary.\n\
             - Explore the repository first and follow its existing conventions.\n\
             - Implement the issue completely, including tests where the \
               project has them.\n\
             - Run the relevant tests/checks yourself before declaring success.\n\
             - COMMIT all your work to the current branch with clear messages. \
               Leave the working tree clean.\n\
             - Do NOT push and do NOT create a pull request; the PR already \
               exists and meguri pushes to it.\n\
             - Do NOT switch branches, do NOT rebase, and do NOT touch other \
               worktrees.{lang_section}",
            number = run.issue_number,
            branch = run.branch.as_deref().unwrap_or("?"),
            title = cp.issue_title,
            body = cp.issue_body,
            lang_section = flow::language_instruction(deps.config.language_for(&deps.project)),
        )
        // The completion contract is appended by prepare_turn. No PR-body
        // section: the PR already exists, nothing consumes `pr_body` here.
    }

    fn verify_work(&self, _run: &RunRecord, _worktree: &Path) -> std::result::Result<(), String> {
        Ok(()) // committed implementation work is all the takeover requires
    }

    /// New commits are counted against the spec PR branch's pushed tip, not
    /// the default branch (the spec commit is already ahead of that).
    fn verify_base(&self, deps: &Deps, run: &RunRecord) -> String {
        run.branch
            .clone()
            .unwrap_or_else(|| deps.project.default_branch.clone())
    }

    /// Unused: the PR already exists, so open-pr never creates one.
    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String {
        format!("{} (#{})", cp.issue_title, run.issue_number)
    }

    /// Label transition on the PR: the takeover consumed `meguri:spec-ready`;
    /// without it the PR is a normal in-flight implementation PR that the
    /// fixer may amend. The removal is load-bearing (a lingering spec-ready
    /// label keeps the fixer off the PR forever), so failing to remove it
    /// fails the run instead of passing silently.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        let _ = run;
        let pr = cp
            .pr_number
            .context("spec-worker checkpoint has no PR number")?;
        deps.forge
            .remove_pr_label(pr, forge::LABEL_SPEC_READY)
            .await?;
        deps.forge
            .remove_pr_label(pr, forge::LABEL_WORKING)
            .await
            .ok();
        Ok(())
    }

    /// The claim marker lives on the PR, not the issue.
    async fn release_claim(&self, deps: &Deps, run: &RunRecord) {
        if let Some(pr) = claimed_pr(deps, &run.id) {
            deps.forge
                .remove_pr_label(pr, forge::LABEL_WORKING)
                .await
                .ok();
        }
    }

    /// Same escalation as the worker — needs-human label + comment on the
    /// issue — plus releasing the PR claim so a human retrigger can reclaim.
    async fn escalate(&self, deps: &Deps, run: &RunRecord, reason: &str) {
        self.release_claim(deps, run).await;
        flow::escalate_on_forge(deps, run.issue_number, reason).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_carries_issue_spec_and_takeover_rules() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
        std::fs::write(
            dir.path().join("docs/specs/issue-7.md"),
            "# Spec\n\n- cache in-memory first\n",
        )
        .unwrap();

        let run = fake_run(7);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            issue_body: "Cache the thing.".into(),
            ..Default::default()
        };
        let prompt = SpecWorkerFlavor.execute_prompt(&fake_deps(), &run, &cp, dir.path());
        assert!(prompt.contains("# Issue: Add caching"));
        assert!(prompt.contains("Cache the thing."));
        assert!(prompt.contains("# Reviewed spec (`docs/specs/issue-7.md`)"));
        assert!(prompt.contains("- cache in-memory first"));
        assert!(prompt.contains("the PR already exists"));
        assert!(prompt.contains("Do NOT push"));
        assert!(
            !prompt.contains("# Pull request description"),
            "the PR exists; nothing consumes pr_body"
        );
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn prompt_survives_a_missing_spec_file() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let prompt =
            SpecWorkerFlavor.execute_prompt(&fake_deps(), &run, &Checkpoint::default(), dir.path());
        assert!(prompt.contains("spec file missing"));
    }

    #[test]
    fn prompt_pins_output_language_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let mut deps = fake_deps();
        deps.config.language = Some("日本語".into());
        let prompt =
            SpecWorkerFlavor.execute_prompt(&deps, &run, &Checkpoint::default(), dir.path());
        assert!(prompt.contains("# Output language"));
        assert!(prompt.contains("日本語"));
    }

    #[test]
    fn verify_base_is_the_pr_branch_tip() {
        let deps = fake_deps();
        let run = fake_run(7);
        assert_eq!(
            SpecWorkerFlavor.verify_base(&deps, &run),
            "meguri/7-add-caching-abc123"
        );
        let mut branchless = run;
        branchless.branch = None;
        assert_eq!(SpecWorkerFlavor.verify_base(&deps, &branchless), "main");
    }

    fn fake_run(issue: i64) -> RunRecord {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run_for_loop("proj", KIND, issue, "t").unwrap();
        let mut run = store.get_run(&run.id).unwrap().unwrap();
        run.branch = Some("meguri/7-add-caching-abc123".into());
        run
    }

    fn fake_deps() -> Deps {
        use std::sync::Arc;
        Deps {
            store: crate::store::Store::open_in_memory().unwrap(),
            mux: Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge: Arc::new(crate::forge::fake::FakeForge::default()),
            config: crate::config::Config::default(),
            project: crate::config::ProjectConfig {
                id: "proj".into(),
                repo_path: "/tmp/unused".into(),
                repo_slug: "me/proj".into(),
                default_branch: "main".into(),
                language: None,
                check_command: None,
                worktree_root: None,
                pr: None,
            },
        }
    }
}
