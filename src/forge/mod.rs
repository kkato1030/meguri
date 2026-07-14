//! Forge abstraction (GitHub for MVP). Follows looper's "Authority"
//! principle: labels and comments on the forge are the durable source of
//! truth for workflow state, never in-memory agent output.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

pub mod fake;
pub mod gh;

// Issue labels form two axes (ADR 0005). Axis 1 — the phase: a meguri-engaged
// open issue always carries exactly one of `plan` / `speccing` / `ready` /
// `implementing`, so an unlabeled issue means "untriaged". Axis 2 — the ball
// (who holds it): `working` / `needs-human` / `hold` layer on top of the phase
// without removing it.

/// Phase (axis 1): issue is queued for the worker loop (applied by a human).
pub const LABEL_READY: &str = "meguri:ready";
/// Phase (axis 1): issue is queued for the planner loop (applied by a human;
/// opt-in spec-first flow — the default stays `meguri:ready` straight to a PR).
pub const LABEL_PLAN: &str = "meguri:plan";
/// Phase (axis 1): the issue's spec PR is open. The planner swaps `plan` for
/// this at spec-PR creation; the spec-worker swaps it for `implementing` when
/// it claims the takeover. Detail (reviewing / ready) lives on the PR.
pub const LABEL_SPECCING: &str = "meguri:speccing";
/// Phase (axis 1): the issue's implementation PR is open (CI fixing, review,
/// awaiting merge all included). The worker/spec-worker apply it at PR
/// creation/takeover and it stays until the issue closes. Load-bearing: it
/// backs the "unlabeled = untriaged" invariant.
pub const LABEL_IMPLEMENTING: &str = "meguri:implementing";
/// The planner's spec PR awaits review; the reviewer loop picks it up,
/// posts a summary review, and flips it to `meguri:spec-ready` when clean.
pub const LABEL_SPEC_REVIEWING: &str = "meguri:spec-reviewing";
/// Spec review approved the approach; the worker continues implementation on
/// the same branch (issue #21) and owns it from here on — the fixer must keep
/// its hands off the PR. A human can also apply this label directly.
pub const LABEL_SPEC_READY: &str = "meguri:spec-ready";
/// meguri claimed the issue (dedup across restarts and hosts).
pub const LABEL_WORKING: &str = "meguri:working";
/// Discovery must skip this issue.
pub const LABEL_HOLD: &str = "meguri:hold";
/// meguri gave up and a human needs to look (a comment explains why).
pub const LABEL_NEEDS_HUMAN: &str = "meguri:needs-human";
/// Opt-in to GitHub-native auto-merge (auto-merge 1/3, issue #41). A human
/// applies it to an issue (the worker copies it onto the PR) or straight to a
/// PR; the auto-merger sweep arms auto-merge on PRs carrying it.
pub const LABEL_AUTOMERGE: &str = "meguri:automerge";
/// The cleaner loop's per-project report issue (one per project; its body is
/// a snapshot of the current divergence, rewritten on every sweep).
pub const LABEL_CLEAN_REPORT: &str = "meguri:clean-report";
/// The triage loop's per-project report issue (issue #85). Read-only, like
/// the cleaner's: its body is a snapshot of the current triage
/// recommendations for untriaged open issues, rewritten on every sweep.
pub const LABEL_TRIAGE_REPORT: &str = "meguri:triage-report";

/// GitHub's three merge strategies. This is the forge's vocabulary and config
/// deserializes straight into it (`serde(lowercase)`); ADR 0003 forbids
/// falling back between them, so an unavailable strategy is an error, never a
/// silent substitution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeStrategy {
    Squash,
    Merge,
    Rebase,
}

impl MergeStrategy {
    /// The `gh pr merge` flag that selects this strategy.
    pub fn flag(self) -> &'static str {
        match self {
            Self::Squash => "--squash",
            Self::Merge => "--merge",
            Self::Rebase => "--rebase",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Squash => "squash",
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        }
    }
}

/// A snapshot of a repository's merge configuration for one base branch — the
/// input to the auto-merge fail-fast (ADR 0003) and the sweep's arm gate.
#[derive(Debug, Clone)]
pub struct MergePolicy {
    /// The repo's "Allow auto-merge" toggle (`allow_auto_merge`).
    pub auto_merge_allowed: bool,
    /// Strategies the repo permits (`allow_squash_merge` / `allow_merge_commit`
    /// / `allow_rebase_merge`).
    pub allowed_strategies: Vec<MergeStrategy>,
    /// Whether the base branch carries classic branch protection with required
    /// status checks. Rulesets are not detected (ADR 0003) — a rulesets-only
    /// repo reads as `false`, and `require_branch_protection = false` is the
    /// escape hatch.
    pub protected_with_required_checks: bool,
}

impl MergePolicy {
    pub fn allows(&self, strategy: MergeStrategy) -> bool {
        self.allowed_strategies.contains(&strategy)
    }
}

/// The result of trying to arm GitHub-native auto-merge on a PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArmOutcome {
    /// auto-merge was reserved; GitHub merges the PR once required checks pass.
    Armed,
    /// GitHub already considers the PR mergeable (clean status), so there was
    /// no block to reserve against — the caller finalizes with `merge_pr`.
    AlreadyClean,
}

/// Open/closed lifecycle of an issue on the forge — the authority that
/// decides when local resources tied to the issue (worktrees, panes) may be
/// reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    Open,
    Closed,
}

/// Whether a PR can merge into its base, as computed by the forge — the
/// trigger for the conflict-resolver loop. `Unknown` is GitHub's transient
/// "still computing" state; discovery treats it as not actionable and simply
/// retries on the next poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeableState {
    Mergeable,
    Conflicting,
    Unknown,
}

/// GitHub's `mergeStateStatus` — the platform's own verdict on why (or
/// whether) a PR can merge right now. merge-watch (auto-merge 2/3, #42) leans
/// on this instead of re-deriving required-vs-optional checks itself: the
/// required-check authority stays with GitHub (ADR 0003 / 0007). Notably
/// `Unstable` (a non-required check failing) still merges under auto-merge,
/// while `Blocked` (a required check failing or a required review missing)
/// does not — that split is exactly the "required checks only" rule the issue
/// asks for, computed by GitHub rather than by meguri.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStateStatus {
    /// Mergeable and every required check green — auto-merge fires immediately.
    Clean,
    /// A required check failed, a required review is missing, or other required
    /// protection blocks the merge.
    Blocked,
    /// The base moved ahead; the branch needs an update before merging.
    Behind,
    /// Conflicts with the base (the `mergeStateStatus` face of CONFLICTING).
    Dirty,
    /// Mergeable, but a non-required check is failing or pending — GitHub still
    /// merges once the required checks pass.
    Unstable,
    /// The PR is a draft.
    Draft,
    /// A pre-receive hook blocks the merge.
    HasHooks,
    /// GitHub is still computing the state.
    Unknown,
}

impl MergeStateStatus {
    /// Map GitHub's uppercase `mergeStateStatus` string; anything unrecognized
    /// (including the empty string) degrades to [`Self::Unknown`], never to a
    /// state that would make merge-watch act.
    pub fn from_gh(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "CLEAN" => Self::Clean,
            "BLOCKED" => Self::Blocked,
            "BEHIND" => Self::Behind,
            "DIRTY" => Self::Dirty,
            "UNSTABLE" => Self::Unstable,
            "DRAFT" => Self::Draft,
            "HAS_HOOKS" => Self::HasHooks,
            _ => Self::Unknown,
        }
    }
}

/// A snapshot of one PR's merge readiness for merge-watch (auto-merge 2/3,
/// #42): GitHub's mergeability, its `mergeStateStatus` verdict, and whether
/// auto-merge is currently armed (`autoMergeRequest` non-null).
#[derive(Debug, Clone)]
pub struct MergeState {
    pub mergeable: MergeableState,
    pub status: MergeStateStatus,
    /// Whether GitHub-native auto-merge is armed on the PR right now. A human
    /// disabling it (arm marker present but this false) is the HumanDisabled
    /// signal merge-watch backs off from.
    pub auto_merge_enabled: bool,
}

/// Verdict of one CI check on a PR head, reduced to the axis the ci-fixer
/// cares about: done-and-green, done-and-red, or still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Success,
    Failure,
    Pending,
}

/// The subset of GitHub's commit-status states meguri writes for its inspection
/// history (`meguri/self-review`, `meguri/pr-review`, ADR 0008). Advisory by
/// default: a `Failure` status is a red check that does not block a human merge
/// (GitHub reports the PR `UNSTABLE`) unless the user makes the context a
/// required check; the auto-merger reads it as its arm gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitStatusState {
    Success,
    Failure,
    Pending,
}

impl CommitStatusState {
    /// The GitHub `state` string for the statuses API.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Pending => "pending",
        }
    }

    /// Parse GitHub's lowercase status state; `error` folds into `Failure`.
    pub fn from_gh(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "success" => Some(Self::Success),
            "failure" | "error" => Some(Self::Failure),
            "pending" => Some(Self::Pending),
            _ => None,
        }
    }
}

/// One CI check on the PR's head commit (a GitHub Actions check run or a
/// classic commit status).
#[derive(Debug, Clone)]
pub struct CheckRun {
    pub name: String,
    pub state: CheckState,
    /// Detail page of the check; on GitHub Actions this carries the workflow
    /// run id the failed-log fetch needs. Empty when the forge has none.
    pub url: String,
}

/// The check/status rollup of a PR's head commit — the trigger for the
/// ci-fixer loop.
#[derive(Debug, Clone, Default)]
pub struct CheckRollup {
    pub checks: Vec<CheckRun>,
}

impl CheckRollup {
    /// Aggregate verdict. Pending wins over Failure: while anything is still
    /// running the picture is incomplete — the ci-fixer must not start on a
    /// head whose CI could still change under it (and whose failed logs may
    /// not exist yet). No checks at all is Success: a project without CI has
    /// nothing to fix.
    pub fn state(&self) -> CheckState {
        if self.checks.iter().any(|c| c.state == CheckState::Pending) {
            CheckState::Pending
        } else if self.checks.iter().any(|c| c.state == CheckState::Failure) {
            CheckState::Failure
        } else {
            CheckState::Success
        }
    }

    /// The failing checks (prompt rendering, failed-log fetching).
    pub fn failed(&self) -> Vec<&CheckRun> {
        self.checks
            .iter()
            .filter(|c| c.state == CheckState::Failure)
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

/// One blocking issue from the forge-native dependency graph (GitHub's
/// `blocked_by`) — the dependency gate's single source of truth (looper
/// ADR-0004). No label conventions, no issue-body parsing.
#[derive(Debug, Clone)]
pub struct Blocker {
    pub number: i64,
    /// Lowercase state: "open" or "closed".
    pub state: String,
    /// Why it closed ("completed", "not_planned", "duplicate"), if closed.
    pub state_reason: Option<String>,
}

impl Blocker {
    /// Only closed-as-completed resolves a dependency. A blocker closed as
    /// not_planned/duplicate keeps blocking: the dependent issue was planned
    /// against work that never happened, so a human must re-triage it.
    pub fn resolved(&self) -> bool {
        self.state == "closed" && self.state_reason.as_deref() == Some("completed")
    }
}

impl Issue {
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
}

#[derive(Debug, Clone)]
pub struct CreatedPr {
    pub number: i64,
    pub url: String,
}

/// A pull request as discovery sees it: state and labels drive whether the
/// fixer may touch it, and the head sha lets the reviewer loop check what was
/// already reviewed and check out its head.
#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub url: String,
    /// Head branch name (meguri's own PRs follow `meguri/...`).
    pub head_branch: String,
    pub head_sha: String,
    /// Lowercase state: "open", "merged" or "closed".
    pub state: String,
    /// Whether the PR is still a draft (`isDraft`). The auto-merger readies a
    /// draft before arming; the worker opens automerge PRs non-draft.
    pub is_draft: bool,
    pub labels: Vec<String>,
}

impl PullRequest {
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
}

/// One PR conversation comment with its creation time (RFC3339 UTC, as GitHub
/// returns `createdAt`). merge-watch reads the #41 arm marker's `createdAt` to
/// know how long a PR has been armed (arm-since) without any local state.
#[derive(Debug, Clone)]
pub struct PrComment {
    pub body: String,
    /// GitHub's `createdAt`, e.g. `2026-07-13T09:00:00Z`; empty when the forge
    /// did not supply one (`store::parse_ts` then yields None → never stale).
    pub created_at: String,
}

/// One comment inside a review thread.
#[derive(Debug, Clone)]
pub struct ReviewComment {
    pub author: String,
    pub body: String,
}

/// A review-comment thread on a PR; resolution state is the reviewer's
/// durable verdict, replies are how the fixer signals "addressed".
#[derive(Debug, Clone)]
pub struct ReviewThread {
    /// Forge-native thread id (GraphQL node id on GitHub).
    pub id: String,
    pub resolved: bool,
    /// File the thread is anchored to, if any.
    pub path: Option<String>,
    pub line: Option<i64>,
    pub comments: Vec<ReviewComment>,
}

/// Draft of one inline review comment (a thread anchor). The line is
/// mandatory: GitHub's review REST API only anchors comments to a line of
/// the diff — anchor-less remarks belong in the review body instead.
#[derive(Debug, Clone)]
pub struct ReviewCommentDraft {
    pub path: String,
    /// Line on the NEW side of the diff (side=RIGHT).
    pub line: u64,
    pub body: String,
}

#[async_trait]
pub trait Forge: Send + Sync {
    async fn get_issue(&self, number: i64) -> Result<Issue>;
    /// Open/closed state of a single issue (see [`IssueState`]).
    async fn issue_state(&self, number: i64) -> Result<IssueState>;
    /// Open issues carrying `label` (candidates for discovery).
    async fn list_issues_with_label(&self, label: &str) -> Result<Vec<Issue>>;
    /// Every open issue, label-agnostic (triage discovery, issue #85). The
    /// caller filters by label/hold/blocker — no forge-side search is used, so
    /// "untriaged = no workflow label" stays a single client-side rule.
    async fn list_open_issues(&self) -> Result<Vec<Issue>>;
    /// Issues blocking `issue` via the forge-native dependency graph
    /// (GitHub's `blocked_by`); discovery gates on them (see [`Blocker`]).
    async fn blocked_by(&self, issue: i64) -> Result<Vec<Blocker>>;
    /// File a new issue; returns its number (planner decomposition,
    /// issue #24; the cleaner's report issue, issue #44).
    async fn create_issue(&self, title: &str, body: &str, labels: &[&str]) -> Result<i64>;
    /// Record `issue` (in this forge's repo) as blocked by `blocker` in the
    /// forge-native dependency graph (the same graph [`Forge::blocked_by`]
    /// reads).
    async fn add_blocked_by(&self, issue: i64, blocker: i64) -> Result<()>;
    /// Like [`Forge::add_blocked_by`] but the blocker lives in `blocker_repo`
    /// (`owner/repo`), which may differ from this forge's own repo — the
    /// cross-repo decomposition case (issue #154). The dependent `issue` is
    /// still in this forge's repo; only the blocker's home repo changes. When
    /// `blocker_repo` equals this forge's repo the two are equivalent.
    async fn add_blocked_by_in(&self, issue: i64, blocker_repo: &str, blocker: i64) -> Result<()>;
    /// Overwrite an issue's body wholesale (snapshot-style report updates).
    async fn update_issue_body(&self, number: i64, body: &str) -> Result<()>;
    /// Overwrite an issue's title (the `meguri add` refine step retitles a
    /// raw one-liner into a summarized title, issue #120).
    async fn update_issue_title(&self, number: i64, title: &str) -> Result<()>;
    async fn add_label(&self, issue: i64, label: &str) -> Result<()>;
    async fn remove_label(&self, issue: i64, label: &str) -> Result<()>;
    /// Add a label to a pull request (issues and PRs share GitHub's number
    /// space but need different edit commands).
    async fn add_pr_label(&self, pr: i64, label: &str) -> Result<()>;
    async fn remove_pr_label(&self, pr: i64, label: &str) -> Result<()>;
    /// Overwrite a pull request's title (the spec worker retitles a takeover
    /// PR from `Spec: X` to `X` once implementation lands, issue #98).
    async fn update_pr_title(&self, pr: i64, title: &str) -> Result<()>;
    /// Overwrite a pull request's body wholesale (the spec worker replaces the
    /// planner's spec description with the implementation one, issue #98).
    async fn update_pr_body(&self, pr: i64, body: &str) -> Result<()>;
    /// Open pull requests carrying `label` (candidates for review discovery).
    async fn list_prs_with_label(&self, label: &str) -> Result<Vec<PullRequest>>;
    /// The PR's full unified diff against its base.
    async fn pr_diff(&self, number: i64) -> Result<String>;
    /// Bodies of the PR's conversation comments (review-marker lookups).
    async fn pr_comments(&self, number: i64) -> Result<Vec<String>>;
    /// The PR's conversation comments with creation timestamps — merge-watch
    /// reads the arm marker's `createdAt` for arm-since (auto-merge 2/3, #42).
    async fn pr_comments_meta(&self, number: i64) -> Result<Vec<PrComment>>;
    /// Post a conversation comment on a pull request.
    async fn comment_pr(&self, pr: i64, body: &str) -> Result<()>;
    async fn comment(&self, issue: i64, body: &str) -> Result<()>;
    /// Comment on a pull request (same number space, different command).
    async fn pr_comment(&self, pr: i64, body: &str) -> Result<()>;
    async fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<CreatedPr>;
    async fn get_pr(&self, number: i64) -> Result<PullRequest>;
    /// The PR whose head is `branch`, if any — open PRs win over closed or
    /// merged ones. The reaper uses the merged state to recognize squash and
    /// rebase merges, whose branch tips never become ancestors of the base.
    async fn pr_for_branch(&self, branch: &str) -> Result<Option<PullRequest>>;
    /// Whether the PR can merge into its base (conflict-resolver discovery).
    async fn pr_mergeable(&self, number: i64) -> Result<MergeableState>;
    /// The PR's merge-readiness snapshot for merge-watch (auto-merge 2/3, #42):
    /// mergeability + `mergeStateStatus` + whether auto-merge is armed, in one
    /// `gh pr view`. A forge error here is the TransientError signal — the
    /// caller must not escalate on it (ADR 0007).
    async fn pr_merge_state(&self, number: i64) -> Result<MergeState>;
    /// The check/status rollup of the PR's head commit (ci-fixer discovery).
    async fn pr_check_rollup(&self, number: i64) -> Result<CheckRollup>;
    /// Failed-job logs of the PR's failing checks, pre-trimmed for a prompt.
    /// Best-effort per check: a check whose logs cannot be fetched
    /// contributes a note instead of failing the whole call.
    async fn pr_failed_check_logs(&self, number: i64) -> Result<String>;
    /// Open PRs (candidates for fixer discovery).
    async fn list_open_prs(&self) -> Result<Vec<PullRequest>>;
    /// All review threads on a PR, resolved or not.
    async fn list_review_threads(&self, pr: i64) -> Result<Vec<ReviewThread>>;
    /// Reply inside an existing review thread.
    async fn reply_review_thread(&self, pr: i64, thread_id: &str, body: &str) -> Result<()>;
    /// Post a PR review with inline comments — each draft becomes a review
    /// thread the fixer can pick up. Always event=COMMENT: meguri never
    /// approves or requests changes; the human merge gate stays human
    /// (ADR 0004).
    async fn create_pr_review(
        &self,
        pr: i64,
        body: &str,
        comments: &[ReviewCommentDraft],
    ) -> Result<()>;

    /// Arm GitHub-native auto-merge, pinned to `head_sha`
    /// (`--match-head-commit`). Already-armed is treated as success
    /// (idempotent). The [`ArmOutcome`] distinguishes a reservation
    /// ([`ArmOutcome::Armed`]) from GitHub already judging the PR mergeable
    /// ([`ArmOutcome::AlreadyClean`]) — the caller `merge_pr`s the latter.
    async fn enable_auto_merge(
        &self,
        pr: i64,
        strategy: MergeStrategy,
        head_sha: &str,
    ) -> Result<ArmOutcome>;
    /// Finalize a PR GitHub already judged clean, pinned to `head_sha`
    /// (`gh pr merge --match-head-commit`, no `--auto`). A moved head is
    /// rejected by GitHub, so no head other than the confirmed one merges.
    async fn merge_pr(&self, pr: i64, strategy: MergeStrategy, head_sha: &str) -> Result<()>;
    /// Ready a draft PR (`gh pr ready`).
    async fn mark_pr_ready(&self, pr: i64) -> Result<()>;

    /// Write a commit status on `head_sha` (`POST /repos/{repo}/statuses/{sha}`)
    /// — meguri's inspection history for a review (ADR 0008). `context` is the
    /// status name (`meguri/self-review` / `meguri/pr-review`), `description`
    /// the one-line verdict. Idempotent from the caller's view: re-posting the
    /// same context replaces the visible status.
    async fn set_commit_status(
        &self,
        head_sha: &str,
        context: &str,
        state: CommitStatusState,
        description: &str,
    ) -> Result<()>;

    /// The latest state of `context` on `head_sha`, or `None` if meguri never
    /// wrote that context on that commit — the auto-merger's guard gate reads
    /// it (ADR 0008 §5). `None` means "not decided yet": the caller waits
    /// rather than escalating.
    async fn commit_status(
        &self,
        head_sha: &str,
        context: &str,
    ) -> Result<Option<CommitStatusState>>;
    /// The repository's merge configuration for `base_branch` (ADR 0003
    /// fail-fast + arm gate). When `require_branch_protection` is false the
    /// branch-protection probe is skipped and `protected_with_required_checks`
    /// comes back false — the caller opted out, so the (admin-only, 403-prone)
    /// probe must not run and must not be able to fail startup. When true, the
    /// probe runs and a 403 (non-admin token) surfaces as an error.
    async fn merge_policy(
        &self,
        base_branch: &str,
        require_branch_protection: bool,
    ) -> Result<MergePolicy>;
}

/// Builds a [`Forge`] for a given repo slug (`owner/repo`). Cross-repo
/// decomposition needs a forge for a workspace sibling's repository, which the
/// per-project `Deps::forge` cannot provide (issue #154). Production returns a
/// `GhForge`; tests inject fakes so the sibling-repo path is exercised without
/// hitting GitHub. See ADR 0009.
pub trait ForgeFactory: Send + Sync {
    fn for_slug(&self, slug: &str) -> Arc<dyn Forge>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocker(state: &str, state_reason: Option<&str>) -> Blocker {
        Blocker {
            number: 1,
            state: state.into(),
            state_reason: state_reason.map(str::to_string),
        }
    }

    fn check(state: CheckState) -> CheckRun {
        CheckRun {
            name: "ci".into(),
            state,
            url: String::new(),
        }
    }

    #[test]
    fn rollup_state_is_pending_over_failure_over_success() {
        // No checks: nothing to fix, never a trigger.
        assert_eq!(CheckRollup::default().state(), CheckState::Success);

        let green = CheckRollup {
            checks: vec![check(CheckState::Success), check(CheckState::Success)],
        };
        assert_eq!(green.state(), CheckState::Success);

        let red = CheckRollup {
            checks: vec![check(CheckState::Success), check(CheckState::Failure)],
        };
        assert_eq!(red.state(), CheckState::Failure);

        // A failure with anything still running stays Pending: the picture
        // is incomplete until CI settles.
        let mixed = CheckRollup {
            checks: vec![check(CheckState::Failure), check(CheckState::Pending)],
        };
        assert_eq!(mixed.state(), CheckState::Pending);
    }

    #[test]
    fn rollup_failed_lists_only_failing_checks() {
        let rollup = CheckRollup {
            checks: vec![
                check(CheckState::Success),
                check(CheckState::Failure),
                check(CheckState::Pending),
            ],
        };
        assert_eq!(rollup.failed().len(), 1);
        assert_eq!(rollup.failed()[0].state, CheckState::Failure);
    }

    #[test]
    fn only_closed_as_completed_resolves_a_blocker() {
        assert!(blocker("closed", Some("completed")).resolved());
        assert!(!blocker("open", None).resolved());
        assert!(!blocker("closed", Some("not_planned")).resolved());
        assert!(!blocker("closed", Some("duplicate")).resolved());
        assert!(!blocker("closed", None).resolved());
        // Unreadable state degrades to unresolved, never to resolved.
        assert!(!blocker("", None).resolved());
    }
}
