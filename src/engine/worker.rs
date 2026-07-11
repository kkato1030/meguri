//! The worker loop: `meguri:ready` issue → worktree → interactive agent
//! turns in a mux pane → verified commits → implementation PR. The heavy
//! lifting lives in [`super::flow`]; this module only plugs in the
//! worker-specific label, prompt, and PR shape.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor};
use super::{Deps, Target};
use crate::forge;
use crate::store::RunRecord;

/// `runs.loop_kind` value for worker runs (the schema default).
pub const KIND: &str = "worker";

/// The worker as a schedulable loop: `meguri:ready` issues in, PRs out.
pub struct WorkerLoop;

#[async_trait]
impl super::Loop for WorkerLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        flow::discover_by_label(deps, KIND, forge::LABEL_READY).await
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
        run_worker(deps, run_id).await
    }
}

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
        format!(
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
            number = run.issue_number,
            branch = run.branch.as_deref().unwrap_or("?"),
            title = cp.issue_title,
            body = cp.issue_body,
            pr_section = flow::pr_body_instruction(worktree),
            lang_section = flow::language_instruction(deps.config.language_for(&deps.project)),
        )
        // The completion contract is appended by prepare_turn.
    }

    fn verify_work(&self, _run: &RunRecord, _worktree: &Path) -> std::result::Result<(), String> {
        Ok(()) // committed work is all the worker requires
    }

    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String {
        format!("{} (#{})", cp.issue_title, run.issue_number)
    }

    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, _cp: &Checkpoint) -> Result<()> {
        deps.forge
            .remove_label(run.issue_number, forge::LABEL_WORKING)
            .await
            .ok();
        deps.forge
            .remove_label(run.issue_number, forge::LABEL_READY)
            .await
            .ok();
        Ok(())
    }
}
