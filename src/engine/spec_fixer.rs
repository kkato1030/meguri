//! The spec-fixer loop: the plan-side mirror of the ci-fixer (issue #188).
//! An open spec/ADR PR whose plan review came back with findings
//! (`meguri:spec-reviewing`, `meguri/pr-review = failure` on its head) →
//! worktree attached to the PR's existing branch → agent reads the
//! pr-reviewer's folded `<details>` findings and revises the spec/ADR →
//! verified commits pushed to the same PR. A fixer-family loop: the push moves
//! the head, the pr-reviewer re-reviews the new head, and the verdict comes
//! back through the next discovery poll — clean flips the PR to `spec-ready`,
//! findings start another round.
//!
//! Why this loop exists: ADR 0008 defined plan-review findings as "keep
//! `spec-reviewing`, re-review on the next push", but never assigned the push.
//! The impl side has a symmetric driver (a red rollup → the ci-fixer; human /
//! external-bot threads → the fixer); the plan side had none, so every spec PR
//! parked on its first findings (ADR 0013). `spec_fixer` is that missing
//! driver.
//!
//! Convergence dedups on the head sha, more cleanly than the ci-fixer: after a
//! fix push the new head carries no `meguri/pr-review` status yet, so
//! discovery's "head status is failure" condition is false until the
//! pr-reviewer re-runs — the loop cannot re-fire on a head it already fixed. Three brakes
//! still bound it: a `Pending`/absent status is never picked up, escalated PRs
//! (`meguri:needs-human`) wait for a human, and a PR already fixed
//! [`MAX_SPEC_FIX_RUNS`] times escalates to `meguri:needs-human` — a reviewer
//! that keeps emitting *fresh* findings after that many rounds is diverging and
//! needs a person (`meguri run --issue N` can force another round).
//!
//! Lifetime (issue #92): runs are keyed by the PR's canonical *issue*, so the
//! fix happens in the planner's author lane — same pane, same live session as
//! the run that wrote the spec — and the revision keeps the planning context.
//! The worktree attaches to the PR head; the pane is reclaimed when the issue
//! closes.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor, PreparedWork};
use super::{Deps, canonical_key, is_combined, open_pr_for_issue, pr_is_touchable, pr_reviewer};
use crate::forge::{self, CommitStatusState, PullRequest};
use crate::store::RunRecord;
use serde_json::json;

/// `runs.loop_kind` value for spec-fixer runs.
pub const KIND: &str = "spec-fixer";

/// Fix rounds budgeted per spec PR; a PR whose plan review is still red after
/// this many pushed revisions escalates to `meguri:needs-human` (see the
/// module docs on convergence). Same budget as the ci-fixer.
pub const MAX_SPEC_FIX_RUNS: i64 = 3;

/// Whether `pr` is a plan-review findings target: an open, meguri-owned,
/// unclaimed spec PR (`meguri:spec-reviewing`) that is not held/escalated.
/// `spec-reviewing` precedes the `spec-ready` divergence, so the touchability
/// gate is delivery-mode independent — `skip_spec_ready` never matters here
/// (a spec-reviewing PR carries no `spec-ready` label), but passing the real
/// `is_combined` value keeps this identical to the other fixer-family loops.
fn is_findings_target(pr: &PullRequest, combined: bool) -> Option<String> {
    if !pr.has_label(forge::LABEL_SPEC_REVIEWING) {
        return Some(format!("PR #{} is not spec-reviewing", pr.number));
    }
    pr_is_touchable(pr, combined)
}

pub async fn run_spec_fixer(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
    flow::run_flow(deps, run_id, &SpecFixerFlavor).await
}

/// The retry-limit escalation: every budgeted round pushed a revision, the
/// plan review is still red — park the PR on `meguri:needs-human`. Best-effort
/// like the ci-fixer's; the label is what stops rediscovery. This is where the
/// plan-side parked-review page lives (ADR 0009 / issue #153): the base
/// pr-reviewer defers findings to spec_fixer, so the page moved here — the
/// budget is spent, the review is still red, so a real human wait exists.
pub(crate) async fn escalate_budget_exhausted(deps: &Deps, pr: &PullRequest) {
    // Runs are keyed by the PR's canonical *issue* (issue #92), which the spec
    // PR's own number usually differs from — so the re-run hint must name the
    // issue, not the PR, or `meguri run --issue N` would target the wrong one.
    let issue = canonical_key(pr);
    let _ = deps
        .forge()
        .add_pr_label(pr.number, forge::LABEL_NEEDS_HUMAN)
        .await;
    let _ = deps
        .forge()
        .pr_comment(
            pr.number,
            &format!(
                "🔁 **meguri** pushed {MAX_SPEC_FIX_RUNS} revisions to this spec PR but \
                 the plan review is still finding issues, and needs a human.\n\n\
                 Clear the `{}` label (and re-run with `meguri run --issue {}` \
                 if wanted) once the findings are understood.",
                forge::LABEL_NEEDS_HUMAN,
                issue
            ),
        )
        .await;
    let _ = deps.store.emit(
        None,
        "spec_fixer.budget_exhausted",
        json!({ "pr": pr.number, "budget": MAX_SPEC_FIX_RUNS }),
    );
    // Page a human at the round limit (ADR 0009 / issue #153). Run-less: no turn
    // is running at discovery time, so this points at the PR (not a pane) and
    // carries no interaction_state (the `needs-human` label above is the
    // dashboard marker). The synthetic run_id keys the notifier's throttle so a
    // re-fire before a human clears the label does not re-page. Best-effort like
    // the rest of this escalation — a delivery failure never blocks the sweep.
    deps.notifier
        .notify(&crate::notify::Notification::awaiting_human(
            format!("spec-fixer-budget-{}", pr.number),
            issue,
            Some(pr.title.clone()),
            flow::REASON_REVIEW_PARKED,
            None,
            Some(pr.url.clone()),
        ))
        .await;
}

struct SpecFixerFlavor;

#[async_trait]
impl Flavor for SpecFixerFlavor {
    /// Unused: the spec-fixer's [`Flavor::prepare_work`] override claims by PR
    /// state and pr-review status, not by an issue label.
    fn trigger_label(&self) -> &'static str {
        ""
    }

    /// Re-resolve the PR from the run's canonical issue, claim it (labels live
    /// on the PR, not the issue) and snapshot the pr-reviewer's folded findings
    /// into the checkpoint. Any change that makes the PR untouchable — or its
    /// pr-review status green/absent again — between discovery and claim is a
    /// benign race: skip, don't escalate.
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
        if let Some(reason) = is_findings_target(&pr, is_combined(deps)) {
            return Ok(PreparedWork::Skip(reason));
        }
        if deps
            .forge()
            .commit_status(&pr.head_sha, pr_reviewer::PR_REVIEW_STATUS)
            .await?
            != Some(CommitStatusState::Failure)
        {
            return Ok(PreparedWork::Skip(format!(
                "PR #{}'s plan review is no longer failing",
                pr.number
            )));
        }
        // Findings live in the pr-reviewer's folded `<details>` in the PR body
        // (ADR 0006: the pr-reviewer posts no inline threads). Fall back to the
        // whole body if the block is missing — the agent can still read the PR.
        let findings = pr_reviewer::extract_pr_review_details(&pr.body).unwrap_or_else(|| {
            format!(
                "(the plan review's findings block was not found in the PR body; \
                 read the PR #{} description and its plan review directly.)\n\n{}",
                pr.number, pr.body
            )
        });

        deps.forge()
            .add_pr_label(pr.number, forge::LABEL_WORKING)
            .await?;
        deps.store
            .emit(Some(&run.id), "pr.claimed", json!({ "pr": pr.number }))?;

        cp.issue_title = pr.title.clone();
        cp.issue_body = findings;
        cp.head_branch = Some(pr.head_branch.clone());
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
            "You are revising the spec/ADR on pull request #{number} \"{title}\" \
             in this repository (branch `{branch}`, a dedicated worktree attached \
             to the PR's branch). The independent plan reviewer (pr-reviewer) \
             reviewed this spec and returned findings.\n\n\
             # Plan review findings\n\n{findings}\n\n\
             # Instructions\n\
             - Address every finding above by revising the spec (`docs/specs/…`) \
               and any ADR/domain document it touches. If a finding is wrong or \
               you deliberately deviate, explain why in your result summary.\n\
             - Keep the spec disposable scaffolding: anything with durable value \
               belongs in an ADR (`docs/adr/NNNN-<slug>.md`) or the permanent \
               domain document, not the spec — do not smuggle durable decisions \
               into a spec that gets deleted at implementation.\n\
             - Do NOT implement the issue here; this PR is still at the spec \
               stage. Revise the plan only.\n\
             - Follow the repository's existing conventions.\n\
             - COMMIT all your work to the current branch with clear messages. \
               Leave the working tree clean.\n\
             - Do NOT push; meguri handles that (the push re-runs the plan \
               review).\n\
             - Do NOT switch branches, do NOT rebase, and do NOT touch other \
               worktrees.{lang_section}",
            number = cp.pr_number.unwrap_or(run.issue_number),
            title = cp.issue_title,
            branch = run.branch.as_deref().unwrap_or("?"),
            findings = cp.issue_body,
            lang_section = flow::language_instruction(deps.config.language_for(&deps.project)),
        )
        // The completion contract is appended by prepare_turn. No PR-body
        // section: the PR already exists, nothing consumes `pr_body` here.
    }

    /// Committed revisions are all the spec-fixer requires locally: the real
    /// verdict is the pr-reviewer's re-review of the pushed head, which
    /// discovery reads back from the forge.
    fn verify_work(
        &self,
        _run: &RunRecord,
        _cp: &Checkpoint,
        _worktree: &Path,
    ) -> std::result::Result<(), String> {
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

    /// Revising a spec doesn't change the nature of the change (issue #136):
    /// keep the subject the planner's establishing turn set instead of letting
    /// a revision's wording flap the PR title.
    fn sets_subject(&self) -> bool {
        false
    }

    /// After the push: leave a durable trace on the PR, then release the claim.
    /// The label stays `spec-reviewing` — the pushed head has no pr-review
    /// status, so the pr-reviewer re-reviews it (clean → spec-ready, findings →
    /// another round). Best-effort comment.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        let pr = cp
            .pr_number
            .context("spec-fixer checkpoint has no PR number")?;
        let _ = deps
            .forge()
            .pr_comment(
                pr,
                &format!(
                    "🔁 **meguri** pushed a spec revision addressing the plan \
                     review's findings (run `{}`); the pr-reviewer re-reviews \
                     the new head.",
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

    /// Escalation lands on the claimed PR (the spec-fixer's target); before the
    /// checkpoint knows the PR (prepare-work failed), the canonical issue gets
    /// the notice via the issue API instead.
    async fn escalate(&self, deps: &Deps, run: &RunRecord, reason: &str) {
        let Some(pr) = flow::claimed_pr(deps, &run.id) else {
            flow::escalate_on_forge(deps, run.issue_number, reason).await;
            return;
        };
        let _ = deps
            .forge()
            .add_pr_label(pr, forge::LABEL_NEEDS_HUMAN)
            .await;
        let _ = deps.forge().remove_pr_label(pr, forge::LABEL_WORKING).await;
        let _ = deps
            .forge()
            .pr_comment(
                pr,
                &format!(
                    "🔁 **meguri** could not revise this spec to satisfy the plan \
                     review and needs a human.\n\n> {reason}\n\n\
                     The agent's pane (if still open) has the full context — \
                     see `meguri ps` / `meguri attach` on the host running meguri."
                ),
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::Forge;
    use crate::store::Store;
    use std::sync::Arc;

    #[test]
    fn spec_fix_turns_never_establish_a_new_subject() {
        assert!(!SpecFixerFlavor.sets_subject());
    }

    fn spec_pr(number: i64, labels: &[&str]) -> PullRequest {
        PullRequest {
            number,
            title: format!("Write a spec for caching (#{number})"),
            body: String::new(),
            url: format!("https://fake.example/pr/{number}"),
            head_branch: format!("meguri/{number}-add-caching-abc"),
            head_sha: format!("sha{number}"),
            state: "open".into(),
            is_draft: false,
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn findings_target_requires_spec_reviewing_and_touchability() {
        // A plain spec-reviewing meguri PR is a target.
        assert!(is_findings_target(&spec_pr(1, &[forge::LABEL_SPEC_REVIEWING]), false).is_none());

        // Not spec-reviewing: the plan review's label state machine hasn't put
        // it here.
        assert!(
            is_findings_target(&spec_pr(2, &[]), false)
                .unwrap()
                .contains("not spec-reviewing")
        );

        // The shared touchability gates still apply on top of spec-reviewing.
        for (label, needle) in [
            (forge::LABEL_HOLD, "hold"),
            (forge::LABEL_WORKING, forge::LABEL_WORKING),
            (forge::LABEL_NEEDS_HUMAN, forge::LABEL_NEEDS_HUMAN),
        ] {
            let pr = spec_pr(3, &[forge::LABEL_SPEC_REVIEWING, label]);
            assert!(
                is_findings_target(&pr, false).unwrap().contains(needle),
                "label {label} should block"
            );
        }
    }

    fn fake_deps(forge: Arc<crate::forge::fake::FakeForge>) -> Deps {
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
            Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge,
            crate::config::Config::default(),
            project,
        )
    }

    /// A spec-reviewing PR at head `sha` with the given review verdict recorded
    /// on that head, plus the pr-reviewer's folded findings in its body.
    fn seed_spec_pr(forge: &crate::forge::fake::FakeForge, number: i64, head: &str) {
        let body = format!(
            "Refs #{number}.\n\n<!-- meguri:pr-review -->\n<details>\n\
             <summary>🛡️ pr review (plan) — findings at `{head}`</summary>\n\n\
             - the acceptance criteria are missing\n</details>"
        );
        forge.add_pr(
            number,
            &format!("Write a spec for caching (#{number})"),
            &body,
            &[forge::LABEL_SPEC_REVIEWING],
            &format!("meguri/{number}-add-caching-abc"),
            head,
        );
    }

    #[tokio::test]
    async fn discover_picks_only_failing_review_heads() {
        let forge = Arc::new(crate::forge::fake::FakeForge::default());
        // #1: spec-reviewing, plan review failure at its head → a target.
        seed_spec_pr(&forge, 1, "h1");
        forge.set_commit_status_direct(
            "h1",
            pr_reviewer::PR_REVIEW_STATUS,
            CommitStatusState::Failure,
        );
        // #2: spec-reviewing but the review is green → not a target.
        seed_spec_pr(&forge, 2, "h2");
        forge.set_commit_status_direct(
            "h2",
            pr_reviewer::PR_REVIEW_STATUS,
            CommitStatusState::Success,
        );
        // #3: spec-reviewing, review pending → not a target (still settling).
        seed_spec_pr(&forge, 3, "h3");
        forge.set_commit_status_direct(
            "h3",
            pr_reviewer::PR_REVIEW_STATUS,
            CommitStatusState::Pending,
        );
        // #4: spec-reviewing but no review status yet (freshly pushed) → skip.
        seed_spec_pr(&forge, 4, "h4");
        // #5: review failure but not spec-reviewing → skip.
        forge.add_pr(5, "impl (#5)", "body", &[], "meguri/5-impl-abc", "h5");
        forge.set_commit_status_direct(
            "h5",
            pr_reviewer::PR_REVIEW_STATUS,
            CommitStatusState::Failure,
        );

        let deps = fake_deps(forge);
        // The spec-fixer arm is a branch of the PR-side reconciler now
        // (ADR 0012 S4 決定2): one sweep enqueues its run for #1 only.
        crate::engine::issue_reconciler::sweep(&deps).await.unwrap();
        let runs: Vec<_> = deps
            .store
            .list_runs(true)
            .unwrap()
            .into_iter()
            .filter(|r| r.loop_kind == KIND)
            .collect();
        assert_eq!(runs.len(), 1, "only #1 is a target: {runs:?}");
        assert_eq!(runs[0].issue_number, 1);
    }

    #[tokio::test]
    async fn discover_escalates_when_the_fix_budget_is_spent() {
        let forge = Arc::new(crate::forge::fake::FakeForge::default());
        // The PR number (42) deliberately differs from the canonical issue (7,
        // encoded in the branch), so the recovery hint's `--issue N` is
        // actually exercised — a PR-number hint would send a human to the
        // wrong issue.
        forge.add_pr(
            42,
            "Write a spec for caching (#7)",
            "Refs #7.",
            &[forge::LABEL_SPEC_REVIEWING],
            "meguri/7-add-caching-abc",
            "h1",
        );
        forge.set_commit_status_direct(
            "h1",
            pr_reviewer::PR_REVIEW_STATUS,
            CommitStatusState::Failure,
        );
        let mut deps = fake_deps(forge.clone());
        let (notifier, gw) = crate::notify::fake::recording_notifier();
        deps.notifier = notifier;

        // Record MAX succeeded spec-fixer runs for the canonical issue #7.
        for _ in 0..MAX_SPEC_FIX_RUNS {
            let run = deps
                .store
                .create_run_for_loop("proj", KIND, 7, "t")
                .unwrap();
            deps.store
                .update_run_status(&run.id, crate::store::RunStatus::Succeeded, None)
                .unwrap();
        }

        crate::engine::issue_reconciler::sweep(&deps).await.unwrap();
        let queued: Vec<_> = deps
            .store
            .list_runs(true)
            .unwrap()
            .into_iter()
            .filter(|r| r.loop_kind == KIND && r.status == crate::store::RunStatus::Queued)
            .collect();
        assert!(queued.is_empty(), "budget spent → no run enqueued");

        // The full escalation contract: needs-human label on the PR…
        assert!(
            forge
                .pr_labels(42)
                .contains(&forge::LABEL_NEEDS_HUMAN.to_string()),
            "the exhausted PR is parked on needs-human"
        );
        // …a recovery comment that names the canonical *issue*, not the PR…
        let comments = forge.comments_of(42);
        assert_eq!(
            comments.len(),
            1,
            "exactly one escalation comment: {comments:?}"
        );
        assert!(
            comments[0].contains("meguri run --issue 7"),
            "recovery hint must target the issue: {}",
            comments[0]
        );
        assert!(
            !comments[0].contains("--issue 42"),
            "recovery hint must not name the PR number: {}",
            comments[0]
        );
        // …and the audit event, emitted once.
        assert_eq!(
            deps.store
                .count_events("spec_fixer.budget_exhausted")
                .unwrap(),
            1
        );
        // …and a human is paged at the round limit (ADR 0009 / issue #153),
        // pointing at the PR (not a pane — no turn runs at discovery time).
        let delivered = gw.delivered();
        assert_eq!(delivered.len(), 1, "the round-limit park pages once");
        assert_eq!(delivered[0].event, "awaiting_human");
        assert_eq!(
            delivered[0].dedup_key, "spec-fixer-budget-42",
            "keyed by the PR so a re-fire before a human clears it does not re-page"
        );
        assert!(
            delivered[0].title.contains("#7"),
            "keyed by the canonical issue"
        );
        assert!(delivered[0].url.is_some(), "the page points at the PR");
        assert!(
            delivered[0].body.contains("spec レビュー"),
            "reason surfaces in the body: {}",
            delivered[0].body
        );
    }

    #[tokio::test]
    async fn prepare_work_claims_and_loads_findings() {
        let forge = Arc::new(crate::forge::fake::FakeForge::default());
        seed_spec_pr(&forge, 1, "h1");
        forge.set_commit_status_direct(
            "h1",
            pr_reviewer::PR_REVIEW_STATUS,
            CommitStatusState::Failure,
        );
        let deps = fake_deps(forge.clone());
        let run = deps
            .store
            .create_run_for_loop("proj", KIND, 1, "t")
            .unwrap();

        let mut cp = Checkpoint::default();
        let prepared = SpecFixerFlavor
            .prepare_work(&deps, &run, &mut cp)
            .await
            .unwrap();
        assert!(matches!(prepared, PreparedWork::Claimed));
        assert_eq!(cp.pr_number, Some(1));
        assert!(cp.issue_body.contains("acceptance criteria are missing"));
        assert!(
            forge
                .pr_labels(1)
                .contains(&forge::LABEL_WORKING.to_string()),
            "claimed with the working label"
        );
    }

    #[test]
    fn prompt_lists_findings_and_forbids_push_and_implementation() {
        let forge = Arc::new(crate::forge::fake::FakeForge::default());
        let deps = fake_deps(forge);
        let mut run = deps
            .store
            .create_run_for_loop("proj", KIND, 1, "t")
            .unwrap();
        run.branch = Some("meguri/1-add-caching-abc".into());
        let cp = Checkpoint {
            pr_number: Some(1),
            issue_title: "Write a spec for caching (#1)".into(),
            issue_body: "- the acceptance criteria are missing".into(),
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let prompt = SpecFixerFlavor.execute_prompt(&deps, &run, &cp, dir.path());
        assert!(prompt.contains("revising the spec/ADR on pull request #1"));
        assert!(prompt.contains("the acceptance criteria are missing"));
        assert!(prompt.contains("Do NOT push"));
        assert!(prompt.contains("Do NOT implement the issue here"));
        assert!(prompt.contains("disposable scaffolding"));
        assert!(!prompt.contains("# Output language"));
    }

    /// The crux of issue #188: the plan-side handoff between the pr-reviewer
    /// and the spec-fixer converges without a human. Exercised at the
    /// discovery/settle seam (no real tmux) so any scheduling order closes the
    /// chain.
    #[tokio::test]
    async fn cross_loop_handoff_pr_reviewer_to_spec_fixer_and_back() {
        let forge = Arc::new(crate::forge::fake::FakeForge::default());
        // A spec PR the pr-reviewer already reviewed with findings at head h1.
        seed_spec_pr(&forge, 1, "h1");
        forge.set_commit_status_direct(
            "h1",
            pr_reviewer::PR_REVIEW_STATUS,
            CommitStatusState::Failure,
        );
        let deps = fake_deps(forge.clone());

        // 1. spec-fixer picks it up; the pr-reviewer does not (h1 is already
        //    reviewed). Both are branches of the PR-side reconciler now.
        let runs_of = |deps: &Deps, kind: &str| {
            deps.store
                .list_runs(true)
                .unwrap()
                .into_iter()
                .filter(|r| r.loop_kind == kind && r.status == crate::store::RunStatus::Queued)
                .collect::<Vec<_>>()
        };
        crate::engine::issue_reconciler::sweep(&deps).await.unwrap();
        let sf = runs_of(&deps, KIND);
        assert_eq!(sf.len(), 1, "spec-fixer targets the parked PR");
        assert!(
            runs_of(&deps, pr_reviewer::KIND).is_empty(),
            "pr-reviewer skips the already-reviewed head h1"
        );
        // Retire the queued run so the reservation frees for the later steps.
        deps.store
            .update_run_status(&sf[0].id, crate::store::RunStatus::Skipped, None)
            .unwrap();

        // 2. After the spec-fixer settles, working is gone and spec-reviewing
        //    stays (nothing flips the label until the pr-reviewer re-reviews).
        forge.add_pr_label(1, forge::LABEL_WORKING).await.unwrap();
        let run = deps
            .store
            .create_run_for_loop("proj", KIND, 1, "t")
            .unwrap();
        let cp = Checkpoint {
            pr_number: Some(1),
            ..Default::default()
        };
        SpecFixerFlavor
            .settle_labels(&deps, &run, &cp)
            .await
            .unwrap();
        let labels = forge.pr_labels(1);
        assert!(!labels.contains(&forge::LABEL_WORKING.to_string()));
        assert!(labels.contains(&forge::LABEL_SPEC_REVIEWING.to_string()));
        // The settle run is done; retire it so step 3's assertions see only
        // what the next sweep enqueues.
        deps.store
            .update_run_status(&run.id, crate::store::RunStatus::Succeeded, None)
            .unwrap();

        // 3. The fix push moves the head to h2 (no review status yet): the
        //    spec-fixer no longer fires (head-sha dedup), the pr-reviewer now
        //    does.
        forge.set_pr_head(1, "h2");
        crate::engine::issue_reconciler::sweep(&deps).await.unwrap();
        assert!(
            runs_of(&deps, KIND).is_empty(),
            "spec-fixer waits — h2 has no failing status"
        );
        assert_eq!(
            runs_of(&deps, pr_reviewer::KIND).len(),
            1,
            "pr-reviewer re-reviews the new head h2"
        );

        // 4. The pr-reviewer's clean settle on h2 flips spec-reviewing →
        //    spec-ready, covered by pr_reviewer.rs's
        //    `plan_settle_drives_the_spec_label_state_machine`.
    }
}
