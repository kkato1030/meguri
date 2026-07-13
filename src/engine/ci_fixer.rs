//! The ci-fixer loop: an open meguri PR whose CI rollup on the forge is
//! FAILURE → worktree attached to the PR's existing branch → agent diagnoses
//! the failed job logs and fixes the cause → verified fix commits pushed to
//! the same PR. A fixer-family loop: the push moves the head, CI re-runs,
//! and the verdict comes back through the next discovery poll — green means
//! done, red means another round.
//!
//! Convergence mirrors the conflict resolver: the trigger condition (a red
//! rollup) survives a failed run, so discovery must not re-fire blindly.
//! Three brakes bound the loop: a PENDING rollup is never picked up (CI is
//! still running — the verdict may change and failed logs may not exist
//! yet), escalated PRs (`meguri:needs-human`) wait for a human to clear the
//! label, and a PR already fixed [`MAX_CI_FIX_RUNS`] times escalates to
//! `meguri:needs-human` instead of being rediscovered — CI that keeps coming
//! back red after that many fix rounds needs a human (`meguri run --issue N`
//! can force another round). Rate limits ride the same brakes: the rollup
//! poll (one forge call per PR per sweep, like the conflict resolver's
//! mergeability poll) is only spent on unclaimed, unescalated meguri PRs.
//!
//! Lifetime (issue #92): runs are keyed by the PR's canonical *issue*
//! (recovered from the `meguri/<issue>-…` head branch), so CI fixes happen
//! in the issue's author lane — same pane, same live session as the run
//! that wrote the failing code. The worktree attaches to the PR head; the
//! pane is kept and reclaimed when the issue closes.

use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor, PreparedWork};
use super::{Deps, Target, canonical_key, open_pr_for_issue};
use crate::forge::{self, CheckRollup, CheckState, PullRequest};
use crate::store::RunRecord;
use crate::tasks::TaskKey;
use serde_json::json;

/// `runs.loop_kind` value for ci-fixer runs.
pub const KIND: &str = "ci-fixer";

/// Successful fix rounds budgeted per PR; a PR whose CI is still red after
/// this many pushed fixes escalates to `meguri:needs-human` (see the module
/// docs on convergence).
pub const MAX_CI_FIX_RUNS: i64 = 3;

/// Head-branch prefix identifying meguri's own PRs (the ci-fixer only amends
/// work meguri opened).
const MEGURI_BRANCH_PREFIX: &str = "meguri/";

/// Prefix of meguri's own commit-status contexts (`meguri/self-review`,
/// `meguri/guard-review`). The ci-fixer must not treat these as fixable CI:
/// they carry no failed-job log to diagnose, and an advisory-red guard status
/// (ADR 0008) is deliberately not a merge blocker — picking it up would spin
/// the ci-fixer on nothing and could wrongly escalate it (criterion 6).
const MEGURI_STATUS_PREFIX: &str = "meguri/";

/// The rollup with meguri's own status contexts stripped, so the ci-fixer's
/// fixable verdict and prompt only ever consider real CI.
fn without_meguri_statuses(rollup: CheckRollup) -> CheckRollup {
    CheckRollup {
        checks: rollup
            .checks
            .into_iter()
            .filter(|c| !c.name.starts_with(MEGURI_STATUS_PREFIX))
            .collect(),
    }
}

/// Whether the project uses combined plan delivery (ADR 0008).
fn is_combined(deps: &Deps) -> bool {
    deps.project.plan_delivery == crate::config::PlanDelivery::Combined
}

/// Whether the ci-fixer may touch this PR at all (independent of its CI
/// state). The `spec-ready` skip only applies under combined delivery (ADR
/// 0008): under separate delivery a `spec-ready` spec/ADR PR is standalone and
/// its red CI is the ci-fixer's to fix (finding 3).
fn pr_is_ci_fixable(pr: &PullRequest, combined_delivery: bool) -> Option<String> {
    if pr.state != "open" {
        return Some(format!("PR #{} is {} (not open)", pr.number, pr.state));
    }
    if !pr.head_branch.starts_with(MEGURI_BRANCH_PREFIX) {
        return Some(format!(
            "PR #{} head `{}` was not opened by meguri",
            pr.number, pr.head_branch
        ));
    }
    if combined_delivery && pr.has_label(forge::LABEL_SPEC_READY) {
        return Some(format!(
            "PR #{} is {} (the spec worker owns the branch)",
            pr.number,
            forge::LABEL_SPEC_READY
        ));
    }
    if pr.has_label(forge::LABEL_HOLD) {
        return Some(format!("PR #{} is on hold", pr.number));
    }
    None
}

/// The ci-fixer as a schedulable loop: red meguri PRs in, fix pushes out.
pub struct CiFixerLoop;

#[async_trait]
impl super::Loop for CiFixerLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    /// Open meguri PRs whose CI rollup is FAILURE. Pending rollups wait for
    /// CI to settle (retried next poll), escalated PRs wait for a human, and
    /// a PR whose fix budget is spent while CI is still red escalates right
    /// here — its rounds all *succeeded* (fixes pushed), so the flow's
    /// failure escalation never fired, yet a human must look. The
    /// needs-human guard runs before the rollup poll, so the escalation
    /// fires once and later sweeps skip the PR cheaply.
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        if deps.forge.is_none() {
            return Ok(Vec::new()); // PR loops are inert in local mode
        }
        let combined = is_combined(deps);
        let mut targets = Vec::new();
        for pr in deps.forge().list_open_prs().await? {
            if pr_is_ci_fixable(&pr, combined).is_some()
                || pr.has_label(forge::LABEL_WORKING)
                || pr.has_label(forge::LABEL_NEEDS_HUMAN)
            {
                continue;
            }
            let rollup = without_meguri_statuses(deps.forge().pr_check_rollup(pr.number).await?);
            if rollup.state() != CheckState::Failure {
                continue;
            }
            let issue = canonical_key(&pr);
            if deps
                .store
                .succeeded_run_count(&deps.project.id, KIND, issue)?
                >= MAX_CI_FIX_RUNS
            {
                escalate_budget_exhausted(deps, &pr).await;
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
        run_ci_fixer(deps, run_id).await
    }
}

pub async fn run_ci_fixer(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
    flow::run_flow(deps, run_id, &CiFixerFlavor).await
}

/// The retry-limit escalation: every budgeted round pushed a fix, CI is
/// still red — park the PR on `meguri:needs-human`. Best-effort like the
/// other escalations; the label is what stops rediscovery.
async fn escalate_budget_exhausted(deps: &Deps, pr: &PullRequest) {
    let _ = deps
        .forge()
        .add_pr_label(pr.number, forge::LABEL_NEEDS_HUMAN)
        .await;
    let _ = deps
        .forge()
        .pr_comment(
            pr.number,
            &format!(
                "🔁 **meguri** pushed {MAX_CI_FIX_RUNS} CI fixes to this PR but its \
                 checks are still failing, and needs a human.\n\n\
                 Clear the `{}` label (and re-run with `meguri run --issue {}` \
                 if wanted) once the cause is understood.",
                forge::LABEL_NEEDS_HUMAN,
                pr.number
            ),
        )
        .await;
    let _ = deps.store.emit(
        None,
        "ci_fixer.budget_exhausted",
        json!({ "pr": pr.number, "budget": MAX_CI_FIX_RUNS }),
    );
}

/// Markdown listing of the failing checks plus the failed job logs for the
/// execute prompt.
fn render_failures(rollup: &CheckRollup, logs: &str) -> String {
    let list = rollup
        .failed()
        .iter()
        .map(|c| {
            if c.url.is_empty() {
                format!("- {}", c.name)
            } else {
                format!("- {} ({})", c.name, c.url)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let logs = logs.trim();
    if logs.is_empty() {
        list
    } else {
        format!("{list}\n\n## Failed job logs\n\n{logs}")
    }
}

struct CiFixerFlavor;

#[async_trait]
impl Flavor for CiFixerFlavor {
    /// Unused: the ci-fixer's [`Flavor::prepare_work`] override claims by PR
    /// state and CI rollup, not by an issue label.
    fn trigger_label(&self) -> &'static str {
        ""
    }

    /// Re-resolve the PR from the run's canonical issue, claim it (labels
    /// live on the PR, not the issue) and snapshot the failing checks and
    /// their logs into the checkpoint. Any change that makes the PR
    /// untouchable — or its CI green or pending again — between discovery
    /// and claim is a benign race: skip, don't escalate.
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
        if let Some(reason) = pr_is_ci_fixable(&pr, is_combined(deps)) {
            return Ok(PreparedWork::Skip(reason));
        }
        if pr.has_label(forge::LABEL_WORKING) {
            return Ok(PreparedWork::Skip(format!(
                "PR #{} is already claimed ({})",
                pr.number,
                forge::LABEL_WORKING
            )));
        }
        if pr.has_label(forge::LABEL_NEEDS_HUMAN) {
            return Ok(PreparedWork::Skip(format!(
                "PR #{} is escalated ({})",
                pr.number,
                forge::LABEL_NEEDS_HUMAN
            )));
        }
        let rollup = without_meguri_statuses(deps.forge().pr_check_rollup(pr.number).await?);
        if rollup.state() != CheckState::Failure {
            return Ok(PreparedWork::Skip(format!(
                "PR #{}'s CI is no longer failing",
                pr.number
            )));
        }
        // Logs are context, not the trigger: a fetch failure must not stall
        // the fix (the agent can query CI itself from the prompt's hints).
        let logs = deps
            .forge()
            .pr_failed_check_logs(pr.number)
            .await
            .unwrap_or_else(|e| format!("(fetching the failed job logs failed: {e:#})"));

        deps.forge()
            .add_pr_label(pr.number, forge::LABEL_WORKING)
            .await?;
        deps.store.emit(
            Some(&run.id),
            "pr.claimed",
            json!({
                "pr": pr.number,
                "failing_checks": rollup.failed().iter().map(|c| c.name.clone())
                    .collect::<Vec<_>>(),
            }),
        )?;

        cp.issue_title = pr.title.clone();
        cp.issue_body = render_failures(&rollup, &logs);
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
            "You are fixing failing CI checks on pull request #{number} \
             \"{title}\" in this repository (branch `{branch}`, a dedicated \
             worktree attached to the PR's branch). The PR's checks on the \
             forge came back red.\n\n\
             # Failing checks\n\n{failures}\n\n\
             # Instructions\n\
             - Diagnose each failing check from the logs above and fix the \
               underlying cause in the code. Do not paper over failures \
               (no skipped tests, no loosened assertions); if the check \
               itself is wrong, fixing the check is fine — explain any \
               deliberate deviation in your result summary.\n\
             - If you need more context, `gh pr checks {number}` and \
               `gh run view <run-id> --log-failed` show the CI results.\n\
             - Follow the repository's existing conventions; run the \
               relevant tests/checks yourself before declaring success.\n\
             - COMMIT all your work to the current branch with clear \
               messages. Leave the working tree clean.\n\
             - Do NOT push; meguri handles that (the push re-runs CI).\n\
             - Do NOT switch branches, do NOT rebase, and do NOT touch \
               other worktrees.{lang_section}",
            number = cp.pr_number.unwrap_or(run.issue_number),
            title = cp.issue_title,
            branch = run.branch.as_deref().unwrap_or("?"),
            failures = cp.issue_body,
            lang_section = flow::language_instruction(deps.config.language_for(&deps.project)),
        )
        // The completion contract is appended by prepare_turn. No PR-body
        // section: the PR already exists, nothing consumes `pr_body` here.
    }

    /// Committed fixes are all the ci-fixer requires locally: the real
    /// verdict is the next CI run on the pushed head, which discovery reads
    /// back from the forge.
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
        format!("{} (#{})", cp.issue_title, run.issue_number)
    }

    /// After the push: leave a durable trace on the PR, then release the
    /// claim. No further signal is needed — the push re-runs CI, a PENDING
    /// rollup keeps discovery quiet, and a red one re-triggers the loop
    /// (bounded by the fix budget) — so the comment is best-effort.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
        let pr = cp
            .pr_number
            .context("ci-fixer checkpoint has no PR number")?;
        let _ = deps
            .forge()
            .pr_comment(
                pr,
                &format!(
                    "🔁 **meguri** pushed a fix for the failing CI checks \
                     (run `{}`); CI re-runs on the new head.",
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

    /// Escalation lands on the claimed PR (the ci-fixer's target); before
    /// the checkpoint knows the PR (prepare-work failed), the canonical
    /// issue gets the notice via the issue API instead.
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
                    "🔁 **meguri** could not fix the failing CI checks on this \
                     PR and needs a human.\n\n> {reason}\n\n\
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
    use crate::forge::CheckRun;

    #[test]
    fn fixable_guards_state_ownership_and_labels() {
        let pr = PullRequest {
            number: 3,
            title: "Add feature (#9)".into(),
            body: String::new(),
            url: "https://fake.example/pr/3".into(),
            head_branch: "meguri/9-add-feature-abc123".into(),
            head_sha: String::new(),
            state: "open".into(),
            is_draft: false,
            labels: vec![],
        };
        assert!(pr_is_ci_fixable(&pr, true).is_none());

        let merged = PullRequest {
            state: "merged".into(),
            ..pr.clone()
        };
        assert!(pr_is_ci_fixable(&merged, true).unwrap().contains("merged"));

        let human = PullRequest {
            head_branch: "feature/manual".into(),
            ..pr.clone()
        };
        assert!(
            pr_is_ci_fixable(&human, true)
                .unwrap()
                .contains("not opened by meguri")
        );

        // spec-ready: skipped under combined, ci-fixable under separate (finding 3).
        let spec_ready = PullRequest {
            labels: vec![forge::LABEL_SPEC_READY.to_string()],
            ..pr.clone()
        };
        assert!(
            pr_is_ci_fixable(&spec_ready, true)
                .unwrap()
                .contains(forge::LABEL_SPEC_READY)
        );
        assert!(pr_is_ci_fixable(&spec_ready, false).is_none());

        let held = PullRequest {
            labels: vec![forge::LABEL_HOLD.to_string()],
            ..pr
        };
        assert!(pr_is_ci_fixable(&held, true).unwrap().contains("hold"));
    }

    fn red_rollup() -> CheckRollup {
        CheckRollup {
            checks: vec![
                CheckRun {
                    name: "test".into(),
                    state: CheckState::Failure,
                    url: "https://github.com/me/proj/actions/runs/42/job/7".into(),
                },
                CheckRun {
                    name: "lint".into(),
                    state: CheckState::Success,
                    url: String::new(),
                },
            ],
        }
    }

    #[test]
    fn meguri_status_contexts_are_stripped_from_the_rollup() {
        // A red `meguri/guard-review` advisory status (ADR 0008) must not make
        // the ci-fixer think there is CI to fix (criterion 6).
        let rollup = CheckRollup {
            checks: vec![
                CheckRun {
                    name: "meguri/guard-review".into(),
                    state: CheckState::Failure,
                    url: String::new(),
                },
                CheckRun {
                    name: "test".into(),
                    state: CheckState::Success,
                    url: String::new(),
                },
            ],
        };
        let stripped = without_meguri_statuses(rollup);
        assert_eq!(stripped.checks.len(), 1);
        assert_eq!(stripped.checks[0].name, "test");
        // Only real CI remains, so the verdict is green (nothing to fix).
        assert_eq!(stripped.state(), CheckState::Success);

        // A real red check still drives the ci-fixer even next to a meguri one.
        let mixed = CheckRollup {
            checks: vec![
                CheckRun {
                    name: "meguri/guard-review".into(),
                    state: CheckState::Failure,
                    url: String::new(),
                },
                CheckRun {
                    name: "test".into(),
                    state: CheckState::Failure,
                    url: String::new(),
                },
            ],
        };
        assert_eq!(without_meguri_statuses(mixed).state(), CheckState::Failure);
    }

    #[test]
    fn failures_render_lists_failing_checks_and_logs() {
        let body = render_failures(&red_rollup(), "### test\n```\nassert failed\n```");
        assert!(body.contains("- test (https://github.com/me/proj/actions/runs/42/job/7)"));
        assert!(!body.contains("- lint"), "green checks stay out: {body}");
        assert!(body.contains("## Failed job logs"));
        assert!(body.contains("assert failed"));

        // Logs are best-effort; their absence must not leave a dangling
        // heading.
        let body = render_failures(&red_rollup(), "  \n");
        assert!(!body.contains("## Failed job logs"));
    }

    #[test]
    fn prompt_lists_failures_and_forbids_push_and_rebase() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(3);
        let cp = Checkpoint {
            issue_title: "Add feature (#9)".into(),
            issue_body: render_failures(&red_rollup(), "### test\n```\nassert failed\n```"),
            ..Default::default()
        };
        let prompt = CiFixerFlavor.execute_prompt(&fake_deps(), &run, &cp, dir.path());
        assert!(prompt.contains("failing CI checks on pull request #3"));
        assert!(prompt.contains("- test (https://github.com/me/proj/actions/runs/42/job/7)"));
        assert!(prompt.contains("assert failed"));
        assert!(prompt.contains("gh pr checks 3"));
        assert!(prompt.contains("Do NOT push"));
        assert!(prompt.contains("do NOT rebase"));
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn prompt_pins_output_language_when_configured() {
        let dir = tempfile::tempdir().unwrap();
        let run = fake_run(3);
        let mut deps = fake_deps();
        deps.config.language = Some("日本語".into());
        let prompt = CiFixerFlavor.execute_prompt(&deps, &run, &Checkpoint::default(), dir.path());
        assert!(prompt.contains("# Output language"));
        assert!(prompt.contains("日本語"));
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
