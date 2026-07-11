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
/// The planner's spec PR awaits (human) spec review; flipping it to
/// `meguri:spec-ready` is manual until the reviewer loop exists.
pub const LABEL_SPEC_REVIEWING: &str = "meguri:spec-reviewing";
/// Spec review approved the approach; the worker owns the branch from here
/// on and the fixer must keep its hands off the PR.
pub const LABEL_SPEC_READY: &str = "meguri:spec-ready";
/// meguri claimed the issue (dedup across restarts and hosts).
pub const LABEL_WORKING: &str = "meguri:working";
/// Discovery must skip this issue.
pub const LABEL_HOLD: &str = "meguri:hold";
/// meguri gave up and a human needs to look (a comment explains why).
pub const LABEL_NEEDS_HUMAN: &str = "meguri:needs-human";

#[derive(Debug, Clone)]
pub struct Issue {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
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

/// A pull request as discovery sees it (state and labels drive whether the
/// fixer may touch it).
#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: i64,
    pub title: String,
    pub url: String,
    /// Head branch name (meguri's own PRs follow `meguri/...`).
    pub head_branch: String,
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
    /// Open issues carrying `label` (candidates for discovery).
    async fn list_issues_with_label(&self, label: &str) -> Result<Vec<Issue>>;
    async fn add_label(&self, issue: i64, label: &str) -> Result<()>;
    async fn remove_label(&self, issue: i64, label: &str) -> Result<()>;
    /// Add a label to a pull request (issues and PRs share GitHub's number
    /// space but need different edit commands).
    async fn add_pr_label(&self, pr: i64, label: &str) -> Result<()>;
    async fn remove_pr_label(&self, pr: i64, label: &str) -> Result<()>;
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
    /// Open PRs (candidates for fixer discovery).
    async fn list_open_prs(&self) -> Result<Vec<PullRequest>>;
    /// All review threads on a PR, resolved or not.
    async fn list_review_threads(&self, pr: i64) -> Result<Vec<ReviewThread>>;
    /// Reply inside an existing review thread.
    async fn reply_review_thread(&self, pr: i64, thread_id: &str, body: &str) -> Result<()>;
}
