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
//!
//! The spec is transient review scaffolding: this loop prunes it as part of
//! the implementation (the prompt asks for the deletion, [`Flavor::verify_work`]
//! enforces it), so `docs/specs/` never accumulates on the default branch
//! (issue #48). Durable knowledge lives in ADRs / domain docs instead — the
//! planner's prompt routes it there.
//!
//! Lifetime (issue #92): keyed by the issue (branch-encoded), worktree
//! attached to the spec PR's existing branch, pane in the issue's author
//! lane — normally the planner's own pane, adopted live or resumed via the
//! saved session id, so the plan's context carries into implementation;
//! kept after success, reclaimed when the issue closes.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::json;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor, PreparedWork, claimed_pr};
use super::{Deps, Target};
use crate::forge::{self, PullRequest};
use crate::gitops;
use crate::store::RunRecord;
use crate::tasks::TaskKey;

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
        if deps.forge.is_none() {
            return Ok(Vec::new()); // PR loops are inert in local mode
        }
        // The branch-takeover morph is the *combined* delivery (ADR 0008). In
        // separate delivery the spec PR is standalone (the handoff sweep flips
        // the issue to `ready` and the worker implements in a fresh PR), so the
        // spec worker stays out.
        if deps.project.plan_delivery != crate::config::PlanDelivery::Combined {
            return Ok(Vec::new());
        }
        let prs = deps
            .forge()
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
            let Some(issue) = gitops::issue_from_branch(&pr.head_branch) else {
                continue; // human-made head: not meguri's to take over
            };
            if deps
                .store
                .issue_has_succeeded_run(&deps.project.id, KIND, issue)?
            {
                continue;
            }
            targets.push(Target {
                key: TaskKey::Issue(issue),
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

/// The "# Reviewed spec" prompt section for a worktree that carries a landed
/// spec (separate delivery, ADR 0008 finding 1), including the deletion
/// instruction; `None` when no spec file is present (a normal issue with no
/// plan, which degrades to the ordinary flow). Shared with the normal worker so
/// both entrances read and prune the spec identically.
pub fn reviewed_spec_section(worktree: &Path, issue: i64) -> Option<String> {
    let spec_path = super::planner::spec_rel_path(issue);
    let spec = std::fs::read_to_string(worktree.join(&spec_path)).ok()?;
    Some(format!(
        "# Reviewed spec (`{spec_path}`)\n\n{spec}\n\n\
         The approach above was already reviewed and merged — follow it. The \
         spec is disposable review scaffolding: once the implementation is \
         complete, delete `{spec_path}` and commit the deletion (it must not \
         survive onto the default branch).\n\n"
    ))
}

/// The disposable-spec check: a spec that survived implementation gets a
/// corrective turn. Shared with the normal worker under separate delivery.
pub fn verify_spec_pruned(worktree: &Path, issue: i64) -> std::result::Result<(), String> {
    let spec = super::planner::spec_rel_path(issue);
    if worktree.join(&spec).is_file() {
        Err(format!(
            "- spec file `{spec}` still exists (it is disposable review \
             scaffolding: delete it and commit the deletion)"
        ))
    } else {
        Ok(())
    }
}

/// The open spec-ready PR whose head branch encodes `issue`, if any.
async fn spec_ready_pr(deps: &Deps, issue: i64) -> Result<Option<PullRequest>> {
    Ok(deps
        .forge()
        .list_prs_with_label(forge::LABEL_SPEC_READY)
        .await?
        .into_iter()
        .find(|pr| pr.state == "open" && gitops::issue_from_branch(&pr.head_branch) == Some(issue)))
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
        deps.forge()
            .add_pr_label(pr.number, forge::LABEL_WORKING)
            .await?;
        deps.store.emit(
            Some(&run.id),
            "pr.claimed",
            json!({ "pr": pr.number, "issue": run.issue_number }),
        )?;

        // Phase flip on the issue (ADR 0005): implementation has begun, so the
        // issue moves from `meguri:speccing` to `meguri:implementing`. Flipping
        // at claim time (not at settle) means an in-implementation escalation
        // leaves `implementing` + `needs-human` — "stuck in implementation" —
        // instead of `speccing`, which would read as "stuck in spec". add/
        // remove are idempotent, so a resumed run re-running this is safe. The
        // add is load-bearing (the unlabeled = untriaged invariant); the remove
        // is best-effort.
        deps.forge()
            .add_label(run.issue_number, forge::LABEL_IMPLEMENTING)
            .await?;
        deps.forge()
            .remove_label(run.issue_number, forge::LABEL_SPECCING)
            .await
            .ok();

        // The prompt carries the issue (what to build) plus the spec (how).
        let issue = deps.forge().get_issue(run.issue_number).await?;
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
             - The spec above is disposable review scaffolding. Once the \
               implementation is complete, delete `{spec_path}` and commit \
               the deletion — the spec must not survive onto the default \
               branch.\n\
             - COMMIT all your work to the current branch with clear messages. \
               Leave the working tree clean.\n\
             - Do NOT push and do NOT create a pull request; the PR already \
               exists and meguri pushes to it.\n\
             - Do NOT switch branches, do NOT rebase, and do NOT touch other \
               worktrees.\n\n\
             {pr_section}{lang_section}",
            number = run.issue_number,
            branch = run.branch.as_deref().unwrap_or("?"),
            title = cp.issue_title,
            body = cp.issue_body,
            pr_section = flow::pr_body_instruction(worktree),
            lang_section = flow::language_instruction(deps.config.language_for(&deps.project)),
        )
        // The completion contract is appended by prepare_turn. The PR already
        // exists, but the takeover still authors `pr_body`: settle rewrites
        // the PR's body from the planner's spec description to this one
        // (issue #98).
    }

    /// The planner's "spec must exist" check, inverted: the spec is
    /// disposable scaffolding, so a spec that survived implementation gets a
    /// corrective turn asking for its deletion.
    fn verify_work(
        &self,
        run: &RunRecord,
        _cp: &Checkpoint,
        worktree: &Path,
    ) -> std::result::Result<(), String> {
        let spec = super::planner::spec_rel_path(run.issue_number);
        if worktree.join(&spec).is_file() {
            Err(format!(
                "- spec file `{spec}` still exists (it is disposable review \
                 scaffolding: delete it and commit the deletion)"
            ))
        } else {
            Ok(())
        }
    }

    /// New commits are counted against the spec PR branch's pushed tip, not
    /// the default branch (the spec commit is already ahead of that).
    fn verify_base(&self, deps: &Deps, run: &RunRecord) -> String {
        run.branch
            .clone()
            .unwrap_or_else(|| deps.project.default_branch.clone())
    }

    /// open-pr never *creates* the PR (it already exists), but
    /// [`Flavor::settle_presentation`] retitles it to this: the planner opened
    /// the PR as `Spec: X (#N)`, and once implementation lands the `Spec:`
    /// prefix is dropped so the title reads as an implementation PR (issue #98).
    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String {
        format!("{} (#{})", cp.issue_title, run.issue_number)
    }

    /// Transition the takeover PR's presentation from spec to implementation.
    /// The planner authored it as `Spec: X (#N)` with a spec-premised body;
    /// now that the implementation is committed, retitle it `X (#N)` and
    /// replace the body with the implementation description the agent wrote
    /// (issue #98). Idempotent: re-running sets the same values. This is the
    /// spec-worker-only half of the presentation the normal worker sets at PR
    /// creation, so the two paths converge instead of diverging.
    async fn settle_presentation(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
    ) -> Result<()> {
        let pr = cp
            .pr_number
            .context("spec-worker checkpoint has no PR number")?;
        deps.forge()
            .update_pr_title(pr, &self.pr_title(run, cp))
            .await?;
        let lenses = &deps.config.review_for(&deps.project).lenses;
        deps.forge()
            .update_pr_body(pr, &flow::compose_pr_body(run, cp, lenses, true))
            .await?;
        deps.store.emit(
            Some(&run.id),
            "pr.presentation_settled",
            json!({ "pr": pr }),
        )?;
        Ok(())
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
        deps.forge()
            .remove_pr_label(pr, forge::LABEL_SPEC_READY)
            .await?;
        deps.forge()
            .remove_pr_label(pr, forge::LABEL_WORKING)
            .await
            .ok();
        Ok(())
    }

    /// The claim marker lives on the PR, not the issue.
    async fn release_claim(&self, deps: &Deps, run: &RunRecord) {
        if let Some(pr) = claimed_pr(deps, &run.id) {
            deps.forge()
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
            prompt.contains("# Pull request description"),
            "settle rewrites the takeover PR's body, so pr_body is consumed"
        );
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn prompt_tells_the_agent_to_prune_the_spec() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);
        let prompt =
            SpecWorkerFlavor.execute_prompt(&fake_deps(), &run, &Checkpoint::default(), dir.path());
        assert!(prompt.contains("disposable review scaffolding"));
        assert!(prompt.contains("delete `docs/specs/issue-7.md` and commit"));
        assert!(prompt.contains("must not survive onto the default branch"));
    }

    #[test]
    fn verify_work_rejects_a_surviving_spec_and_accepts_its_absence() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(7);

        assert!(
            SpecWorkerFlavor
                .verify_work(&run, &Checkpoint::default(), dir.path())
                .is_ok()
        );

        std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
        std::fs::write(dir.path().join("docs/specs/issue-7.md"), "# Spec\n").unwrap();
        let err = SpecWorkerFlavor
            .verify_work(&run, &Checkpoint::default(), dir.path())
            .unwrap_err();
        assert!(err.contains("docs/specs/issue-7.md"), "{err}");
        assert!(err.contains("delete it"), "{err}");
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
        let project = crate::config::ProjectConfig {
            id: "proj".into(),
            repo_path: "/tmp/unused".into(),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            worktree_root: None,
            pr: None,
            clean: None,
            plan_delivery: Default::default(),
            review: None,
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
