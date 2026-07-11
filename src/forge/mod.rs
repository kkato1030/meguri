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
/// The spec PR passed review; the worker continues implementation on the
/// same branch (issue #21). A human can also apply this label directly.
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

/// A pull request as the reviewer loop sees it: enough to claim it, check
/// what was already reviewed (head sha), and check out its head.
#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: i64,
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub head_branch: String,
    pub head_sha: String,
    pub url: String,
}

impl PullRequest {
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }
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
    async fn get_pr(&self, number: i64) -> Result<PullRequest>;
    /// Open pull requests carrying `label` (candidates for review discovery).
    async fn list_prs_with_label(&self, label: &str) -> Result<Vec<PullRequest>>;
    /// The PR's full unified diff against its base.
    async fn pr_diff(&self, number: i64) -> Result<String>;
    /// Bodies of the PR's conversation comments (review-marker lookups).
    async fn pr_comments(&self, number: i64) -> Result<Vec<String>>;
    /// Post a conversation comment on a pull request.
    async fn comment_pr(&self, pr: i64, body: &str) -> Result<()>;
    async fn comment(&self, issue: i64, body: &str) -> Result<()>;
    async fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<CreatedPr>;
}
