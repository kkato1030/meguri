//! Forge abstraction (GitHub for MVP). Follows looper's "Authority"
//! principle: labels and comments on the forge are the durable source of
//! truth for workflow state, never in-memory agent output.

use anyhow::Result;
use async_trait::async_trait;

pub mod fake;
pub mod gh;

/// Issue is queued for the worker loop (applied by a human).
pub const LABEL_READY: &str = "meguri:ready";
/// Issue is queued for the planner loop (applied by a human; opt-in
/// spec-first flow — the default stays `meguri:ready` straight to a PR).
pub const LABEL_PLAN: &str = "meguri:plan";
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
    pub labels: Vec<String>,
}

impl PullRequest {
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
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

#[async_trait]
pub trait Forge: Send + Sync {
    async fn get_issue(&self, number: i64) -> Result<Issue>;
    /// Open/closed state of a single issue (see [`IssueState`]).
    async fn issue_state(&self, number: i64) -> Result<IssueState>;
    /// Open issues carrying `label` (candidates for discovery).
    async fn list_issues_with_label(&self, label: &str) -> Result<Vec<Issue>>;
    /// Issues blocking `issue` via the forge-native dependency graph
    /// (GitHub's `blocked_by`); discovery gates on them (see [`Blocker`]).
    async fn blocked_by(&self, issue: i64) -> Result<Vec<Blocker>>;
    /// File a new issue; returns its number (planner decomposition,
    /// issue #24).
    async fn create_issue(&self, title: &str, body: &str, labels: &[&str]) -> Result<i64>;
    /// Record `issue` as blocked by `blocker` in the forge-native dependency
    /// graph (the same graph [`Forge::blocked_by`] reads).
    async fn add_blocked_by(&self, issue: i64, blocker: i64) -> Result<()>;
    async fn add_label(&self, issue: i64, label: &str) -> Result<()>;
    async fn remove_label(&self, issue: i64, label: &str) -> Result<()>;
    /// Add a label to a pull request (issues and PRs share GitHub's number
    /// space but need different edit commands).
    async fn add_pr_label(&self, pr: i64, label: &str) -> Result<()>;
    async fn remove_pr_label(&self, pr: i64, label: &str) -> Result<()>;
    /// Open pull requests carrying `label` (candidates for review discovery).
    async fn list_prs_with_label(&self, label: &str) -> Result<Vec<PullRequest>>;
    /// The PR's full unified diff against its base.
    async fn pr_diff(&self, number: i64) -> Result<String>;
    /// Bodies of the PR's conversation comments (review-marker lookups).
    async fn pr_comments(&self, number: i64) -> Result<Vec<String>>;
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
    /// Whether the PR can merge into its base (conflict-resolver discovery).
    async fn pr_mergeable(&self, number: i64) -> Result<MergeableState>;
    /// Open PRs (candidates for fixer discovery).
    async fn list_open_prs(&self) -> Result<Vec<PullRequest>>;
    /// All review threads on a PR, resolved or not.
    async fn list_review_threads(&self, pr: i64) -> Result<Vec<ReviewThread>>;
    /// Reply inside an existing review thread.
    async fn reply_review_thread(&self, pr: i64, thread_id: &str, body: &str) -> Result<()>;
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
