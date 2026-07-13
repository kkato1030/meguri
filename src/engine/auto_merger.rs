//! Auto-merge sweep (auto-merge 1/3, issue #41): arm GitHub-native
//! auto-merge on eligible meguri PRs. Following ADR 0003, meguri never
//! decides "safe to merge" — it arms `gh pr merge --auto` (pinned to the
//! confirmed head) and GitHub (branch protection + required checks) decides.
//! The one exception is a PR GitHub already judged mergeable (clean status),
//! which meguri finalizes with `merge_pr` on GitHub's own verdict.
//!
//! No new loop: like the reaper, this rides the watch poll as a sweep. arm is
//! a cheap API call, not an agent turn, so it needs no run record or pane.

use anyhow::Result;
use serde_json::json;

use super::Deps;
use super::guard::GUARD_STATUS;
use crate::config::{AutoMergeConfig, AutoMergeOptIn};
use crate::forge::{
    self, ArmOutcome, CommitStatusState, MergePolicy, MergeStrategy, PullRequest,
};

/// Head-branch prefix identifying meguri's own PRs — auto-merge only ever
/// touches branches meguri opened (same guard as the fixer / conflict
/// resolver).
const MEGURI_BRANCH_PREFIX: &str = "meguri/";

/// Labels that forbid arming: hold / needs-human are human stop signals,
/// working means a run owns the PR, and the two spec-phase labels mean the
/// PR is still a spec under review — auto-merge must never fire mid-spec.
const BLOCKING_LABELS: &[&str] = &[
    forge::LABEL_HOLD,
    forge::LABEL_NEEDS_HUMAN,
    forge::LABEL_WORKING,
    forge::LABEL_SPEC_REVIEWING,
    forge::LABEL_SPEC_READY,
];

/// Hidden marker embedded in every arm comment. Its presence for a head sha
/// is the idempotency key *and* the human-override key in one: an armed head
/// is never re-armed, and a head whose marker exists but whose auto-merge a
/// human disabled is left alone (a new push moves the head past the marker
/// and the conditions are re-evaluated). Same style as the reviewer's
/// head-sha marker (`src/engine/reviewer.rs`).
pub fn armed_marker(head_sha: &str) -> String {
    format!("{ARMED_MARKER_PREFIX} head={head_sha} -->")
}

/// The head-independent prefix of [`armed_marker`]. merge-watch (#42) uses it
/// to recognize an armed PR regardless of which head was armed.
pub const ARMED_MARKER_PREFIX: &str = "<!-- meguri:automerge armed";

pub fn head_already_armed(comments: &[String], head_sha: &str) -> bool {
    let marker = armed_marker(head_sha);
    comments.iter().any(|c| c.contains(&marker))
}

/// The tracked issue a PR closes, parsed strictly from the first body line
/// meguri always writes (`flow.rs`: `"Closes #{n}.\n\n..."`, trailing period
/// included). Anything else returns None — a PR without both the `meguri/`
/// branch convention *and* this link is out of scope (looper: one signal is
/// not enough).
pub fn linked_issue(body: &str) -> Option<i64> {
    body.lines()
        .next()?
        .trim()
        .strip_prefix("Closes #")?
        .strip_suffix('.')?
        .parse::<i64>()
        .ok()
}

/// The fail-fast / arm gate (ADR 0003): every reason the repository's merge
/// settings forbid arming with `cfg`. Empty result = armable. Shared by the
/// sweep's per-PR gate (condition 7), `meguri watch` startup, and
/// `meguri doctor`.
pub fn validate_policy(cfg: &AutoMergeConfig, policy: &MergePolicy) -> Result<(), Vec<String>> {
    let mut problems = Vec::new();
    if !policy.auto_merge_allowed {
        problems.push(
            "repository does not allow auto-merge (enable \"Allow auto-merge\" in \
             the repo's settings)"
                .to_string(),
        );
    }
    if !policy.allows(cfg.strategy) {
        problems.push(format!(
            "merge strategy `{}` is not allowed by the repository (ADR 0003 forbids \
             falling back to another strategy)",
            cfg.strategy.as_str()
        ));
    }
    if cfg.require_branch_protection && !policy.protected_with_required_checks {
        problems.push(
            "base branch has no classic branch protection with required status checks \
             (set `require_branch_protection = false` to arm without it, e.g. on \
             rulesets or without an admin token)"
                .to_string(),
        );
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

/// Any of the labels that keep a PR out of arming.
fn has_blocking_label(pr: &PullRequest) -> bool {
    BLOCKING_LABELS.iter().any(|l| pr.has_label(l))
}

/// First 12 chars of a sha for human-facing text (like the reviewer).
fn short_sha(head_sha: &str) -> &str {
    head_sha.get(..12).unwrap_or(head_sha)
}

/// Watch-poll sweep: arm auto-merge on every eligible open meguri PR. A
/// per-PR failure warns and is retried next poll; it never aborts the sweep.
pub async fn sweep(deps: &Deps) -> Result<()> {
    if deps.forge.is_none() {
        return Ok(()); // no forge, no PRs to arm (local mode)
    }
    let am = deps.config.pr_for(&deps.project).auto_merge.clone();
    if !am.enabled {
        return Ok(());
    }
    // Fetched at most once per sweep (only when a candidate reaches
    // condition 7), then reused for every candidate in the same project.
    let mut policy: Option<MergePolicy> = None;
    for pr in deps.forge().list_open_prs().await? {
        if let Err(e) = process_pr(deps, &am, &pr, &mut policy).await {
            tracing::warn!("auto-merge sweep failed for PR #{}: {e:#}", pr.number);
        }
    }
    Ok(())
}

/// One PR through the arm conditions (cheapest first, ADR 0003 / spec §3).
/// A condition that isn't met returns `Ok(())` (silent skip); only genuine
/// API/arm failures return `Err` (warned by the caller and retried).
async fn process_pr(
    deps: &Deps,
    am: &AutoMergeConfig,
    pr: &PullRequest,
    policy: &mut Option<MergePolicy>,
) -> Result<()> {
    // 1: meguri's own PR only.
    if !pr.head_branch.starts_with(MEGURI_BRANCH_PREFIX) {
        return Ok(());
    }
    // 2: no hold / spec-phase / claimed labels.
    if has_blocking_label(pr) {
        return Ok(());
    }
    // 3: linked to a tracked issue (branch convention + link, both required).
    let Some(issue_number) = linked_issue(&pr.body) else {
        return Ok(());
    };
    // 4: opted in (config `all`, PR label, or issue label).
    if !opted_in(deps, am, pr, issue_number).await? {
        return Ok(());
    }
    // 5: idempotency / human-override marker for the current head. Placed
    // before the GraphQL thread fetch so steady-state armed PRs cost one
    // comments read, not a review-threads query, each poll.
    let comments = deps.forge().pr_comments(pr.number).await?;
    if head_already_armed(&comments, &pr.head_sha) {
        return Ok(());
    }
    // 6: zero unresolved review threads. Stricter than `thread_awaits_fixer`:
    // a fixer reply parks a thread but does not mean the reviewer accepted, so
    // we require actual resolution before arming.
    let threads = deps.forge().list_review_threads(pr.number).await?;
    if threads.iter().any(|t| !t.resolved) {
        return Ok(());
    }
    // 6b: the guard gate (ADR 0008 §5). When the impl guard is enabled, the
    // auto-merger only arms a head whose `meguri/guard-review` status is
    // success — a failure is escalated to a human, and an absent/pending
    // status simply waits (no-op, retried next sweep). When the impl guard is
    // disabled there is no status to require, so this condition is skipped
    // (never demand a status nothing produces — the ADR 0007 deadlock trap).
    match guard_gate(deps, pr).await? {
        GuardGate::Proceed => {}
        GuardGate::Wait => return Ok(()),
        GuardGate::Failed => {
            escalate_guard_failed(deps, pr).await;
            return Ok(());
        }
    }
    // 7: repository merge settings (fetched once per sweep). A mismatch here
    // means the config passed startup fail-fast but the repo changed since —
    // warn and skip rather than error.
    if policy.is_none() {
        *policy = Some(
            deps.forge()
                .merge_policy(&deps.project.default_branch, am.require_branch_protection)
                .await?,
        );
    }
    let policy = policy.as_ref().expect("merge policy fetched just above");
    if let Err(problems) = validate_policy(am, policy) {
        tracing::warn!(
            "PR #{}: repository auto-merge preconditions unmet, skipping: {}",
            pr.number,
            problems.join("; ")
        );
        return Ok(());
    }

    arm(deps, am, pr).await
}

/// Whether this PR is opted into auto-merge: `opt_in = "all"`, the PR carries
/// `meguri:automerge` directly, or its tracked issue does.
async fn opted_in(
    deps: &Deps,
    am: &AutoMergeConfig,
    pr: &PullRequest,
    issue_number: i64,
) -> Result<bool> {
    if am.opt_in == AutoMergeOptIn::All {
        return Ok(true);
    }
    if pr.has_label(forge::LABEL_AUTOMERGE) {
        return Ok(true);
    }
    // The worker copies the label onto the PR, but fall back to the issue for
    // PRs opened before that or where the copy did not land.
    let issue = deps.forge().get_issue(issue_number).await?;
    Ok(issue.has_label(forge::LABEL_AUTOMERGE))
}

/// The guard gate's verdict for one PR (ADR 0008 §5).
enum GuardGate {
    /// Guard disabled, or a success status on the head — arming may proceed.
    Proceed,
    /// Guard enabled but the status is absent/pending — wait (retry next sweep).
    Wait,
    /// Guard enabled and the head's status is a failure — escalate, don't arm.
    Failed,
}

/// Read the impl guard status on `pr`'s head. Auto-merge only ever touches impl
/// PRs (spec-phase labels are blocking), so the relevant toggle is the impl
/// guard.
async fn guard_gate(deps: &Deps, pr: &PullRequest) -> Result<GuardGate> {
    if !deps.config.review_for(&deps.project).guard.impl_enabled {
        return Ok(GuardGate::Proceed);
    }
    match deps
        .forge()
        .commit_status(&pr.head_sha, GUARD_STATUS)
        .await?
    {
        Some(CommitStatusState::Success) => Ok(GuardGate::Proceed),
        Some(CommitStatusState::Failure) => Ok(GuardGate::Failed),
        Some(CommitStatusState::Pending) | None => Ok(GuardGate::Wait),
    }
}

/// A guard-failed head with auto-merge opted in: park the PR on
/// `meguri:needs-human` (a human resolves the guard's findings before it can
/// merge). Reached only when the label is absent (condition 2 blocks it
/// otherwise), so the escalation and its comment fire once.
async fn escalate_guard_failed(deps: &Deps, pr: &PullRequest) {
    let _ = deps
        .forge()
        .add_pr_label(pr.number, forge::LABEL_NEEDS_HUMAN)
        .await;
    let _ = deps
        .forge()
        .comment_pr(
            pr.number,
            &format!(
                "🔁 **meguri** — auto-merge は `{}` の guard review が失敗しているため arm しません。\n\
                 指摘(PR 本文の折り畳み参照)を解消して新しい head を push すると再評価します。",
                short_sha(&pr.head_sha)
            ),
        )
        .await;
    let _ = deps.store.emit(
        None,
        "automerge.guard_failed",
        json!({ "pr": pr.number, "head": pr.head_sha }),
    );
}

/// Ready → arm → marker (spec §3). If arm fails the marker is never written,
/// so the next sweep retries; the arm is idempotent, so a marker-only failure
/// also converges.
async fn arm(deps: &Deps, am: &AutoMergeConfig, pr: &PullRequest) -> Result<()> {
    if pr.is_draft {
        deps.forge().mark_pr_ready(pr.number).await?;
        deps.store
            .emit(None, "pr.readied", json!({ "pr": pr.number }))?;
    }

    let (body, kind) = match deps
        .forge()
        .enable_auto_merge(pr.number, am.strategy, &pr.head_sha)
        .await?
    {
        ArmOutcome::Armed => (
            armed_comment(am.strategy, &pr.head_sha),
            "pr.automerge_armed",
        ),
        ArmOutcome::AlreadyClean => {
            // GitHub already judged the PR mergeable (all required checks
            // green): no block to reserve against, so we finalize on GitHub's
            // verdict. `--match-head-commit` guarantees only the confirmed
            // head merges (ADR 0003).
            deps.forge()
                .merge_pr(pr.number, am.strategy, &pr.head_sha)
                .await?;
            (
                merged_comment(am.strategy, &pr.head_sha),
                "pr.automerge_merged",
            )
        }
    };

    // The marker is head-keyed, so the same comment is the idempotency key for
    // both the arm and the merge path.
    deps.forge().comment_pr(pr.number, &body).await?;
    deps.store.emit(
        None,
        kind,
        json!({ "pr": pr.number, "head": pr.head_sha, "strategy": am.strategy.as_str() }),
    )?;
    tracing::info!(
        "PR #{}: {kind} ({} at {})",
        pr.number,
        am.strategy.as_str(),
        short_sha(&pr.head_sha)
    );
    Ok(())
}

/// The comment posted when auto-merge was armed (marker + human line).
fn armed_comment(strategy: MergeStrategy, head_sha: &str) -> String {
    format!(
        "{marker}\n🔁 **meguri** — auto-merge ({strat}) を `{short}` で arm しました。\n\
         required checks が通れば GitHub がマージします。解除したい場合は PR の \
         auto-merge を無効化してください(この head には再 arm しません)。",
        marker = armed_marker(head_sha),
        strat = strategy.as_str(),
        short = short_sha(head_sha),
    )
}

/// The comment posted when GitHub already judged the PR clean and meguri
/// finalized the merge (same marker line, different prose).
fn merged_comment(strategy: MergeStrategy, head_sha: &str) -> String {
    format!(
        "{marker}\n🔁 **meguri** — GitHub が既にマージ可能と判定していたため \
         `{short}` で auto-merge ({strat}) を確定しました。",
        marker = armed_marker(head_sha),
        strat = strategy.as_str(),
        short = short_sha(head_sha),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linked_issue_parses_the_closes_line_strictly() {
        assert_eq!(linked_issue("Closes #41.\n\nbody"), Some(41));
        assert_eq!(linked_issue("Closes #7.\n"), Some(7));
        // Missing trailing period, wrong verb, or not on the first line: none.
        assert_eq!(linked_issue("Closes #41\n"), None);
        assert_eq!(linked_issue("Fixes #41.\n"), None);
        assert_eq!(linked_issue("intro\nCloses #41.\n"), None);
        assert_eq!(linked_issue("Closes #.\n"), None);
        assert_eq!(linked_issue(""), None);
    }

    #[test]
    fn marker_matches_only_its_own_head() {
        let comments = vec![
            "unrelated chatter".to_string(),
            format!("{}\narmed body", armed_marker("abc123")),
        ];
        assert!(head_already_armed(&comments, "abc123"));
        assert!(!head_already_armed(&comments, "def456"));
        assert!(!head_already_armed(&[], "abc123"));
    }

    fn policy(auto: bool, strategies: Vec<MergeStrategy>, protected: bool) -> MergePolicy {
        MergePolicy {
            auto_merge_allowed: auto,
            allowed_strategies: strategies,
            protected_with_required_checks: protected,
        }
    }

    #[test]
    fn validate_policy_accepts_a_fully_configured_repo() {
        let cfg = AutoMergeConfig::default(); // squash, require protection
        let p = policy(true, vec![MergeStrategy::Squash], true);
        assert!(validate_policy(&cfg, &p).is_ok());
    }

    #[test]
    fn validate_policy_reports_every_missing_precondition() {
        let cfg = AutoMergeConfig::default();
        let p = policy(false, vec![MergeStrategy::Merge], false);
        let problems = validate_policy(&cfg, &p).unwrap_err();
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("Allow auto-merge")));
        assert!(problems.iter().any(|p| p.contains("squash")));
        assert!(problems.iter().any(|p| p.contains("branch protection")));
    }

    #[test]
    fn validate_policy_skips_protection_when_not_required() {
        let cfg = AutoMergeConfig {
            require_branch_protection: false,
            ..AutoMergeConfig::default()
        };
        let p = policy(true, vec![MergeStrategy::Squash], false);
        assert!(validate_policy(&cfg, &p).is_ok());
    }

    #[test]
    fn comments_carry_the_head_marker() {
        let armed = armed_comment(MergeStrategy::Squash, "0123456789abcdef");
        assert!(armed.contains(&armed_marker("0123456789abcdef")));
        assert!(armed.contains("`0123456789ab`"), "{armed}");
        assert!(armed.contains("squash"));

        let merged = merged_comment(MergeStrategy::Rebase, "abc");
        assert!(merged.contains(&armed_marker("abc")));
        assert!(merged.contains("rebase"));
        assert!(merged.contains("確定"));
    }

    #[test]
    fn blocking_labels_cover_hold_and_spec_phase() {
        let base = PullRequest {
            number: 1,
            title: String::new(),
            body: String::new(),
            url: String::new(),
            head_branch: "meguri/1-x".into(),
            head_sha: "sha".into(),
            state: "open".into(),
            is_draft: false,
            labels: vec![],
        };
        assert!(!has_blocking_label(&base));
        for label in BLOCKING_LABELS {
            let pr = PullRequest {
                labels: vec![label.to_string()],
                ..base.clone()
            };
            assert!(has_blocking_label(&pr), "{label} should block");
        }
    }
}
