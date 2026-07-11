//! The planner loop: `meguri:plan` issue → investigate the repository →
//! lightweight spec (`docs/specs/issue-<N>.md`) → spec PR labeled
//! `meguri:spec-reviewing`. Spec-first is opt-in; `meguri:ready` issues keep
//! going straight to the worker.
//!
//! The spec PR and the implementation PR are the same PR: after review (the
//! reviewer loop, or a human) flips the PR to `meguri:spec-ready`, the
//! spec-worker loop takes over the branch and stacks implementation commits
//! on it (issue #21). Branch naming, run
//! bookkeeping, and escalation therefore follow the worker conventions
//! exactly — only the trigger label, prompt, spec-file verification, and PR
//! shape differ.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor};
use super::{Deps, Target};
use crate::forge;
use crate::store::RunRecord;

/// `runs.loop_kind` value for planner runs.
pub const KIND: &str = "planner";

/// Where an issue's spec lives, relative to the repository root.
pub fn spec_rel_path(issue: i64) -> String {
    format!("docs/specs/issue-{issue}.md")
}

/// The planner as a schedulable loop: `meguri:plan` issues in, spec PRs out.
pub struct PlannerLoop;

#[async_trait]
impl super::Loop for PlannerLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        flow::discover_by_label(deps, KIND, forge::LABEL_PLAN).await
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
        run_planner(deps, run_id).await
    }
}

pub async fn run_planner(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
    flow::run_flow(deps, run_id, &PlannerFlavor).await
}

struct PlannerFlavor;

#[async_trait]
impl Flavor for PlannerFlavor {
    fn trigger_label(&self) -> &'static str {
        forge::LABEL_PLAN
    }

    fn execute_prompt(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
        worktree: &Path,
    ) -> String {
        format!(
            "You are planning GitHub issue #{number} in this repository \
             (branch `{branch}`, a dedicated worktree). Write a spec for the \
             issue; do NOT implement it.\n\n\
             # Issue: {title}\n\n{body}\n\n\
             # Instructions\n\
             - Investigate the repository first: what the issue needs, which \
               files/modules it touches, and which decisions have to be made.\n\
             - Write the spec to `{spec}` (create parent directories as needed). \
               Keep it lightweight — it exists to converge review on the approach \
               before implementation: acceptance criteria, files to touch, and \
               key decisions are enough.\n\
             - A decision worth keeping after the PR merges belongs in an ADR \
               (`docs/adr/NNNN-<slug>.md`, next free number), not the spec.\n\
             - Do NOT implement the issue; the spec (plus any ADR) is the only \
               deliverable. The implementation continues later on this same branch.\n\
             - COMMIT your work to the current branch with clear messages. \
               Leave the working tree clean.\n\
             - Do NOT push and do NOT create a pull request; meguri handles both.\n\
             - Do NOT switch branches or touch other worktrees.\n\n\
             {pr_section}{lang_section}",
            number = run.issue_number,
            branch = run.branch.as_deref().unwrap_or("?"),
            title = cp.issue_title,
            body = cp.issue_body,
            spec = spec_rel_path(run.issue_number),
            pr_section = flow::pr_body_instruction(worktree),
            lang_section = flow::language_instruction(deps.config.language_for(&deps.project)),
        )
        // The completion contract is appended by prepare_turn.
    }

    /// The planner's deliverable is the spec file; committed-but-specless
    /// work gets a corrective turn.
    fn verify_work(&self, run: &RunRecord, worktree: &Path) -> std::result::Result<(), String> {
        let spec = spec_rel_path(run.issue_number);
        if worktree.join(&spec).is_file() {
            Ok(())
        } else {
            Err(format!(
                "- spec file `{spec}` does not exist (write it and commit it)"
            ))
        }
    }

    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String {
        format!("Spec: {} (#{})", cp.issue_title, run.issue_number)
    }

    /// Label transition: the issue's `meguri:plan` becomes the PR's
    /// `meguri:spec-reviewing` — the PR is the reviewable artifact from here
    /// on. The PR label is load-bearing (review discovery keys off it), so
    /// failing to apply it fails the run instead of passing silently.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        if let Some(pr) = cp.pr_number {
            deps.forge
                .add_pr_label(pr, forge::LABEL_SPEC_REVIEWING)
                .await?;
        }
        deps.forge
            .remove_label(run.issue_number, forge::LABEL_WORKING)
            .await
            .ok();
        deps.forge
            .remove_label(run.issue_number, forge::LABEL_PLAN)
            .await
            .ok();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_path_is_per_issue() {
        assert_eq!(spec_rel_path(42), "docs/specs/issue-42.md");
    }

    #[test]
    fn verify_work_requires_the_spec_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut run = fake_run(7);
        run.worktree_path = Some(dir.path().to_string_lossy().into_owned());

        let err = PlannerFlavor.verify_work(&run, dir.path()).unwrap_err();
        assert!(err.contains("docs/specs/issue-7.md"), "{err}");

        std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
        std::fs::write(dir.path().join("docs/specs/issue-7.md"), "# Spec\n").unwrap();
        assert!(PlannerFlavor.verify_work(&run, dir.path()).is_ok());
    }

    #[test]
    fn prompt_demands_spec_not_implementation() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            issue_body: "Cache the thing.".into(),
            ..Default::default()
        };
        let prompt = PlannerFlavor.execute_prompt(&fake_deps(), &run, &cp, dir.path());
        assert!(prompt.contains("docs/specs/issue-7.md"));
        assert!(prompt.contains("do NOT implement"));
        assert!(prompt.contains("# Issue: Add caching"));
        assert!(prompt.contains("# Pull request description"));
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn prompt_pins_output_language_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let cp = Checkpoint::default();
        let mut deps = fake_deps();
        deps.config.language = Some("日本語".into());
        let prompt = PlannerFlavor.execute_prompt(&deps, &run, &cp, dir.path());
        assert!(prompt.contains("# Output language"));
        assert!(prompt.contains("日本語"));
    }

    #[test]
    fn pr_title_carries_spec_prefix() {
        let run = fake_run(7);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        };
        assert_eq!(PlannerFlavor.pr_title(&run, &cp), "Spec: Add caching (#7)");
    }

    fn fake_run(issue: i64) -> RunRecord {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run_for_loop("proj", KIND, issue, "t").unwrap();
        let mut run = store.get_run(&run.id).unwrap().unwrap();
        run.branch = Some("meguri/test".into());
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
