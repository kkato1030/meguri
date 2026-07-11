//! Forge abstraction (GitHub for MVP). Follows looper's "Authority"
//! principle: labels and comments on the forge are the durable source of
//! truth for workflow state, never in-memory agent output.

use anyhow::Result;
use async_trait::async_trait;

pub mod fake;
pub mod gh;

/// Issue is queued for the worker loop (applied by a human).
pub const LABEL_READY: &str = "meguri:ready";
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

#[async_trait]
pub trait Forge: Send + Sync {
    async fn get_issue(&self, number: i64) -> Result<Issue>;
    /// Open issues carrying `label` (candidates for discovery).
    async fn list_issues_with_label(&self, label: &str) -> Result<Vec<Issue>>;
    async fn add_label(&self, issue: i64, label: &str) -> Result<()>;
    async fn remove_label(&self, issue: i64, label: &str) -> Result<()>;
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
