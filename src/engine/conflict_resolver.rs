//! The conflict-resolver loop: an open meguri PR the forge reports as
//! CONFLICTING → worktree attached to the PR's existing branch → agent merges
//! the base branch and resolves the conflicts semantically → verified merge
//! commit pushed to the same PR. A fixer-family loop (README roadmap): the
//! push moves the PR head, so the reviewer's head-sha marker automatically
//! queues the resolution for re-review.
//!
//! Resolution needs judgment (which side's intent survives), so the merge is
//! delegated to the agent rather than a mechanical rebase; the orchestrator
//! only pins the base tip at claim time and verifies afterwards that the tip
//! was merged and no conflict markers were committed.
//!
//! Convergence: unlike the label-triggered loops, the trigger condition
//! (CONFLICTING) survives a failed run, so discovery must not re-fire
//! blindly. Two brakes bound the loop: escalated PRs (`meguri:needs-human`)
//! are skipped until a human clears the label, and a PR that was already
//! successfully resolved [`MAX_RESOLVE_RUNS`] times stops being rediscovered
//! (a base that keeps re-conflicting that often deserves a human;
//! `meguri run --issue N` can force another round).
//!
//! Touchability (open / meguri branch / `spec-ready` / hold / working /
//! needs-human) is [`super::pr_is_touchable`], shared with fixer and
//! ci_fixer: until issue #170 this loop carried its own copy that never
//! gained the `spec-ready` gate the other two got under ADR 0008, so a
//! resolver could merge the base into a branch the spec worker still owned
//! under combined delivery. Lifting it here closed that gap.
//!
//! Lifetime (issue #92): runs are keyed by the PR's canonical *issue*
//! (recovered from the `meguri/<issue>-…` head branch), so conflicts are
//! resolved in the issue's author lane — same pane, same live session as
//! the run that wrote the branch. The worktree attaches to the PR head; the
//! pane is kept and reclaimed when the issue closes.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor, PreparedWork};
use super::{
    Deps, MEGURI_BRANCH_PREFIX, Target, canonical_key, is_combined, open_pr_for_issue,
    pr_is_touchable,
};
use crate::forge::{self, MergeableState};
use crate::gitops;
use crate::store::RunRecord;
use crate::tasks::TaskKey;
use serde_json::json;

/// `runs.loop_kind` value for conflict-resolver runs.
pub const KIND: &str = "conflict-resolver";

/// Successful resolves budgeted per PR; beyond this, discovery stays quiet
/// (see the module docs on convergence).
pub const MAX_RESOLVE_RUNS: i64 = 3;

/// The conflict resolver as a schedulable loop: CONFLICTING meguri PRs in,
/// pushed merge commits out.
pub struct ConflictResolverLoop;

#[async_trait]
impl super::Loop for ConflictResolverLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    /// Open meguri PRs the forge reports as CONFLICTING. Escalated PRs wait
    /// for a human (a failed resolve would otherwise re-trigger forever) and
    /// the per-PR resolve budget stops a resolve→re-conflict ping-pong; the
    /// active-run unique index dedups concurrent rounds as usual.
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        if deps.forge.is_none() {
            return Ok(Vec::new()); // PR loops are inert in local mode
        }
        let combined = is_combined(deps);
        let mut targets = Vec::new();
        for pr in deps.open_prs.get(deps).await? {
            if pr_is_touchable(&pr, combined).is_some() {
                continue;
            }
            let issue = canonical_key(&pr);
            let exhausted = deps
                .store
                .succeeded_run_count(&deps.project.id, KIND, issue)?
                >= MAX_RESOLVE_RUNS;
            // Only conflicting PRs are actionable. Check this BEFORE acting on
            // the budget: a PR that already stopped conflicting (resolved by a
            // human, or a base that moved) must not be escalated just because it
            // spent its resolve budget (issue #176).
            if deps.forge().pr_mergeable(pr.number).await? != MergeableState::Conflicting {
                continue;
            }
            if exhausted {
                // Budget spent AND still conflicting: a base that re-conflicts
                // this often needs a human (ADR 0012, P4 — was a silent discover
                // skip before #176). The needs-human filter above makes this
                // fire exactly once; a human clears the label / `meguri run`
                // forces another round.
                let comment = super::escalation::pr_needs_human_comment(
                    &format!(
                        "resolved this PR's conflicts {MAX_RESOLVE_RUNS} times but the base keeps \
                         re-conflicting, and needs a human."
                    ),
                    "Clear the needs-human label (and `meguri run --issue N` if wanted) once the \
                     repeated conflict is understood.",
                );
                super::escalation::escalate_pr(deps, pr.number, &comment).await;
                deps.store.emit(
                    None,
                    "conflict_resolver.exhausted",
                    json!({ "pr": pr.number, "issue": issue }),
                )?;
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
        run_conflict_resolver(deps, run_id).await
    }
}

pub async fn run_conflict_resolver(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
    flow::run_flow(deps, run_id, &ConflictResolverFlavor).await
}

struct ConflictResolverFlavor;

#[async_trait]
impl Flavor for ConflictResolverFlavor {
    /// Unused: the resolver's [`Flavor::prepare_work`] override claims by PR
    /// state and mergeability, not by an issue label.
    fn trigger_label(&self) -> &'static str {
        ""
    }

    /// Re-resolve the PR from the run's canonical issue, claim it (labels
    /// live on the PR, not the issue) and pin the base tip the agent must
    /// merge. Any change that makes the PR untouchable — or mergeable again
    /// — between discovery and claim is a benign race: skip, don't
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
        if deps.forge().pr_mergeable(pr.number).await? != MergeableState::Conflicting {
            return Ok(PreparedWork::Skip(format!(
                "PR #{} is no longer conflicting",
                pr.number
            )));
        }

        // Pin the merge target before claiming: the exact commit the agent
        // must bring in, immune to the base moving mid-run.
        let base_sha =
            gitops::fetch_base_tip(&deps.project.repo_path, &deps.project.default_branch).await?;

        deps.forge()
            .add_pr_label(pr.number, forge::LABEL_WORKING)
            .await?;
        deps.store.emit(
            Some(&run.id),
            "pr.claimed",
            json!({ "pr": pr.number, "base": base_sha }),
        )?;

        cp.issue_title = pr.title.clone();
        cp.head_branch = Some(pr.head_branch.clone());
        cp.base_sha = Some(base_sha);
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
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
        _worktree: &Path,
    ) -> String {
        format!(
            "You are resolving merge conflicts on pull request #{number} \
             \"{title}\" in this repository (branch `{branch}`, a dedicated \
             worktree attached to the PR's branch). The PR conflicts with its \
             base branch `{base_branch}` and cannot be merged.\n\n\
             # Instructions\n\
             - Merge commit `{base_sha}` (the pinned tip of `{base_branch}`) \
               into the current branch: `git merge {base_sha}`.\n\
             - Resolve every conflict semantically: preserve the intent of \
               BOTH this branch's changes and the base branch's changes. Read \
               the surrounding code and history instead of picking a side \
               blindly.\n\
             - Remove all conflict markers, run the relevant tests/checks \
               yourself, then conclude the merge with a commit. Leave the \
               working tree clean.\n\
             - If resolving would require a product or design decision you \
               cannot make from the code, report \"needs_human\" instead of \
               guessing.\n\
             - Do NOT push; meguri handles that.\n\
             - Do NOT rebase, do NOT force anything, do NOT switch branches, \
               and do NOT touch other worktrees.{lang_section}",
            number = cp.pr_number.unwrap_or(run.issue_number),
            title = cp.issue_title,
            branch = run.branch.as_deref().unwrap_or("?"),
            base_branch = deps.project.default_branch,
            base_sha = cp.base_sha.as_deref().unwrap_or("?"),
            lang_section = flow::language_instruction(deps.config.language_for(&deps.project)),
        )
        // The completion contract is appended by prepare_turn. No PR-body
        // section: the PR already exists, nothing consumes `pr_body` here.
    }

    /// The resolver's deliverable, independently verified: the pinned base
    /// tip is an ancestor of HEAD (the merge really happened) and no file
    /// the run changed still carries conflict markers. The Err text feeds a
    /// corrective prompt.
    fn verify_work(
        &self,
        run: &RunRecord,
        cp: &Checkpoint,
        worktree: &Path,
    ) -> std::result::Result<(), String> {
        let base_sha = cp
            .base_sha
            .as_deref()
            .ok_or_else(|| "- checkpoint has no pinned base commit".to_string())?;
        let merged = gitops::is_ancestor(worktree, base_sha, "HEAD")
            .map_err(|e| format!("- checking the merge failed: {e:#}"))?;
        if !merged {
            return Err(format!(
                "- base commit {base_sha} is not an ancestor of HEAD: merge it \
                 (`git merge {base_sha}`), do not cherry-pick or rebase around it"
            ));
        }
        // Scan what this run changed: everything past the PR's pushed tip.
        let pushed_tip = format!(
            "origin/{}",
            run.branch.as_deref().unwrap_or(MEGURI_BRANCH_PREFIX)
        );
        let marked = gitops::conflict_marker_files(worktree, &pushed_tip)
            .map_err(|e| format!("- scanning for conflict markers failed: {e:#}"))?;
        if !marked.is_empty() {
            return Err(format!(
                "- conflict markers are still committed in: {} — resolve them \
                 and amend/commit",
                marked.join(", ")
            ));
        }
        Ok(())
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

    /// Resolving a conflict doesn't change the nature of the change (issue
    /// #136): keep the subject the establishing turn set instead of letting
    /// the resolution's wording flap the PR title.
    fn sets_subject(&self) -> bool {
        false
    }

    /// After the push: leave a durable trace on the PR (the resolution is
    /// meguri-authored history a human may want to double-check), then
    /// release the claim. The re-review itself needs no signal — the pushed
    /// head moves past the reviewer's head-sha marker on its own — so the
    /// comment is best-effort.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        let pr = cp
            .pr_number
            .context("conflict-resolver checkpoint has no PR number")?;
        let base = cp.base_sha.as_deref().unwrap_or("?");
        let _ = deps
            .forge()
            .pr_comment(
                pr,
                &format!(
                    "🔁 **meguri** merged the base branch (`{base}`) and resolved \
                     the conflicts (run `{}`). The new head will be re-reviewed \
                     automatically.",
                    run.id
                ),
            )
            .await;
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

    /// Escalation lands on the claimed PR (the resolver's target); before
    /// the checkpoint knows the PR (prepare-work failed), the canonical
    /// issue gets the notice via the issue API instead.
    async fn escalate(&self, deps: &Deps, run: &RunRecord, reason: &str) {
        let Some(pr) = flow::claimed_pr(deps, &run.id) else {
            super::escalation::escalate_issue(deps, run.issue_number, reason).await;
            return;
        };
        let comment = super::escalation::pr_needs_human_comment(
            "could not resolve the merge conflicts on this PR and needs a human.",
            reason,
        );
        super::escalation::escalate_pr(deps, pr, &comment).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gitops::run_git_sync;

    #[test]
    fn resolve_turns_never_establish_a_new_subject() {
        assert!(!ConflictResolverFlavor.sets_subject());
    }

    // Touchability (open / meguri branch / spec-ready / hold / working /
    // needs-human) is the shared `pr_is_touchable` guard's job — see its
    // tests in `engine::mod`.

    #[test]
    fn prompt_pins_the_base_and_forbids_push_and_rebase() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(3);
        let cp = Checkpoint {
            issue_title: "Add feature (#9)".into(),
            base_sha: Some("cafebabe".into()),
            ..Default::default()
        };
        let prompt = ConflictResolverFlavor.execute_prompt(&fake_deps(), &run, &cp, dir.path());
        assert!(prompt.contains("merge conflicts on pull request #3"));
        assert!(prompt.contains("git merge cafebabe"));
        assert!(prompt.contains("base branch `main`"));
        assert!(prompt.contains("Do NOT push"));
        assert!(prompt.contains("Do NOT rebase"));
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn prompt_pins_output_language_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(3);
        let mut deps = fake_deps();
        deps.config.language = Some("日本語".into());
        let prompt =
            ConflictResolverFlavor.execute_prompt(&deps, &run, &Checkpoint::default(), dir.path());
        assert!(prompt.contains("# Output language"));
        assert!(prompt.contains("日本語"));
    }

    /// verify_work against a real repo: an unmerged base fails, a true merge
    /// passes, committed conflict markers fail.
    #[test]
    fn verify_work_demands_the_merge_and_clean_markers() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
        ] {
            run_git_sync(repo, &args).unwrap();
        }
        std::fs::write(repo.join("f.txt"), "base\n").unwrap();
        run_git_sync(repo, &["add", "."]).unwrap();
        run_git_sync(repo, &["commit", "-m", "init"]).unwrap();

        // Diverge: base advances on main, the PR branch changes the same file.
        run_git_sync(repo, &["checkout", "-b", "meguri/9-feature-abc"]).unwrap();
        std::fs::write(repo.join("f.txt"), "pr\n").unwrap();
        run_git_sync(repo, &["commit", "-am", "pr change"]).unwrap();
        run_git_sync(repo, &["checkout", "main"]).unwrap();
        std::fs::write(repo.join("f.txt"), "main\n").unwrap();
        run_git_sync(repo, &["commit", "-am", "main change"]).unwrap();
        let base_sha = run_git_sync(repo, &["rev-parse", "main"]).unwrap();
        run_git_sync(repo, &["checkout", "meguri/9-feature-abc"]).unwrap();
        // The pushed tip the marker scan diffs against (no real origin here).
        let pr_tip = run_git_sync(repo, &["rev-parse", "HEAD"]).unwrap();
        run_git_sync(
            repo,
            &[
                "update-ref",
                "refs/remotes/origin/meguri/9-feature-abc",
                &pr_tip,
            ],
        )
        .unwrap();

        let mut run = fake_run(9);
        run.branch = Some("meguri/9-feature-abc".into());
        let cp = Checkpoint {
            base_sha: Some(base_sha.clone()),
            ..Default::default()
        };

        // No merge yet: the base tip is not an ancestor.
        let err = ConflictResolverFlavor
            .verify_work(&run, &cp, repo)
            .unwrap_err();
        assert!(err.contains("not an ancestor"), "{err}");

        // A committed "resolution" that keeps the markers is rejected.
        std::fs::write(
            repo.join("f.txt"),
            "<<<<<<< HEAD\npr\n=======\nmain\n>>>>>>> main\n",
        )
        .unwrap();
        run_git_sync(repo, &["commit", "-am", "fake merge"]).unwrap();
        run_git_sync(repo, &["merge", "-s", "ours", "--no-edit", &base_sha]).unwrap();
        let err = ConflictResolverFlavor
            .verify_work(&run, &cp, repo)
            .unwrap_err();
        assert!(err.contains("conflict markers"), "{err}");
        assert!(err.contains("f.txt"), "{err}");

        // A real resolution passes.
        std::fs::write(repo.join("f.txt"), "merged\n").unwrap();
        run_git_sync(repo, &["commit", "-am", "resolve"]).unwrap();
        assert!(ConflictResolverFlavor.verify_work(&run, &cp, repo).is_ok());

        // A missing pinned base is an orchestrator bug surfaced loudly.
        let err = ConflictResolverFlavor
            .verify_work(&run, &Checkpoint::default(), repo)
            .unwrap_err();
        assert!(err.contains("no pinned base"), "{err}");
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
            prompts: Default::default(),
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
