//! The worker loop: `meguri:ready` issue → worktree → interactive agent
//! turns in a mux pane → verified commits → implementation PR. The heavy
//! lifting lives in [`super::flow`]; this module only plugs in the
//! worker-specific label, prompt, and PR shape.
//!
//! Lifetime (issue #92): keyed by the issue, new branch and worktree, pane
//! in the issue's author lane — kept after success so the implementation
//! context survives for a human follow-up or a re-run on the same branch;
//! the reaper reclaims it when the issue closes.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;

use super::Deps;
pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor};
use crate::config::Deliver;
use crate::forge;
use crate::store::RunRecord;
use crate::tasks::TaskKey;

/// `runs.loop_kind` value for worker runs (the schema default).
pub const KIND: &str = "worker";

pub async fn run_worker(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
    flow::run_flow(deps, run_id, &WorkerFlavor).await
}

struct WorkerFlavor;

#[async_trait]
impl Flavor for WorkerFlavor {
    fn trigger_label(&self) -> &'static str {
        forge::LABEL_READY
    }

    fn execute_prompt(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
        worktree: &Path,
    ) -> String {
        let branch = run.branch.as_deref().unwrap_or("?");
        let lang_section = flow::language_instruction(deps.config.language_for(&deps.project));
        // The PR-body section only matters when the deliverable is a PR.
        let pr_section = if deps.config.deliver_for(&deps.project) == Deliver::Pr {
            flow::pr_body_instruction(worktree)
        } else {
            String::new()
        };
        match run.task_key() {
            TaskKey::Issue(number) => format!(
                "You are implementing GitHub issue #{number} in this repository \
                 (branch `{branch}`, a dedicated worktree).\n\n\
                 # Issue: {title}\n\n{body}\n\n\
                 # Instructions\n\
                 - Explore the repository first and follow its existing conventions.\n\
                 - Implement the issue completely, including tests where the project has them.\n\
                 - Run the relevant tests/checks yourself before declaring success.\n\
                 - COMMIT all your work to the current branch with clear messages. \
                   Leave the working tree clean.\n\
                 - Do NOT push and do NOT create a pull request; meguri handles both.\n\
                 - Do NOT switch branches or touch other worktrees.\n\n\
                 {pr_section}{lang_section}",
                title = cp.issue_title,
                body = cp.issue_body,
            ),
            // local task: no issue number; the deliverable is the verified
            // branch.
            TaskKey::Local(_) => format!(
                "You are implementing a local task in this repository \
                 (branch `{branch}`, a dedicated worktree).\n\n\
                 # Task: {title}\n\n{body}\n\n\
                 # Instructions\n\
                 - Explore the repository first and follow its existing conventions.\n\
                 - Implement the task completely, including tests where the project has them.\n\
                 - Run the relevant tests/checks yourself before declaring success.\n\
                 - COMMIT all your work to the current branch with clear messages. \
                   Leave the working tree clean.\n\
                 - Do NOT push and do NOT create a pull request; meguri leaves the \
                   verified branch in place for you to review.\n\
                 - Do NOT switch branches or touch other worktrees.\n\n\
                 {pr_section}{lang_section}",
                title = cp.issue_title,
                body = cp.issue_body,
            ),
        }
        // The completion contract is appended by prepare_turn.
    }

    fn verify_work(
        &self,
        _run: &RunRecord,
        _cp: &Checkpoint,
        _worktree: &Path,
    ) -> std::result::Result<(), String> {
        Ok(()) // committed work is all the worker requires
    }

    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String {
        flow::default_pr_title(run, cp)
    }

    /// Phase transition (ADR 0005) + claim release. In github mode the issue's
    /// `meguri:ready` becomes `meguri:implementing` — the implementation PR is
    /// now open. The `implementing` add runs *before* the claim is released
    /// (which drops `working`+`ready`), keeping the issue labeled throughout;
    /// failing the add fails the run. The coordination layer's `complete`
    /// then releases the claim: github drops `working`+`ready` best-effort,
    /// local flips the task to `done`. No-op forge in local mode.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, _cp: &Checkpoint) -> Result<()> {
        // Only an issue-keyed run has an issue to label: a local task row in a
        // github project (TaskKey::Local) must not label "issue #0".
        if let Some(f) = &deps.forge
            && let crate::tasks::TaskKey::Issue(issue) = run.task_key()
        {
            f.add_label(issue, forge::LABEL_IMPLEMENTING).await?;
        }
        deps.task_source.complete(&run.task_key()).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, ProjectConfig};
    use crate::forge::fake::FakeForge;
    use crate::store::Store;

    #[test]
    fn pr_title_prefers_subject_and_falls_back_to_issue_title() {
        let (_deps, run, _forge) = fake_env(&[forge::LABEL_READY]);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        };
        assert_eq!(WorkerFlavor.pr_title(&run, &cp), "Add caching (#7)");

        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            subject: Some("Cache API responses in memory".into()),
            ..Default::default()
        };
        assert_eq!(
            WorkerFlavor.pr_title(&run, &cp),
            "Cache API responses in memory (#7)"
        );
    }

    #[test]
    fn prompt_names_the_issue_and_forbids_push() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, run, _forge) = fake_env(&[forge::LABEL_READY]);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            issue_body: "Cache the thing.".into(),
            ..Default::default()
        };
        let prompt = WorkerFlavor.execute_prompt(&deps, &run, &cp, dir.path());
        assert!(prompt.contains("# Issue: Add caching"));
        assert!(prompt.contains("Do NOT push"));
    }

    fn fake_env(labels: &[&str]) -> (Deps, RunRecord, Arc<FakeForge>) {
        let forge = Arc::new(FakeForge::with_issue(
            7,
            "Add caching",
            "Cache the thing.",
            labels,
        ));
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run_for_loop("proj", KIND, 7, "t").unwrap();
        let mut run = store.get_run(&run.id).unwrap().unwrap();
        run.branch = Some("meguri/test".into());
        let project = ProjectConfig {
            id: "proj".into(),
            repo_path: "/tmp/unused".into(),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            profile: None,
            worktree_root: None,
            pr: None,
            worktree_setup: Default::default(),
            prompts: Default::default(),
        };
        let deps = Deps::with_github_source(
            store,
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge.clone(),
            Config::default(),
            project,
        );
        (deps, run, forge)
    }
}
