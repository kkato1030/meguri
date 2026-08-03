//! Forge abstraction (GitHub for MVP). Since the 権威反転 (kernel-pruning
//! Phase 5), the sqlite `tasks` table is the workflow authority; the forge is
//! read as a low-frequency edge signal and written as a best-effort
//! projection.

use anyhow::Result;
use async_trait::async_trait;

pub mod fake;
pub mod gh;

// Issue labels since the 権威反転: `ready` / `hold` are the human's edge
// inputs (read by the intake pass), `working` / `implementing` / `needs-human`
// are meguri's best-effort projections of the sqlite task state.

/// Input: queue this issue for the worker loop (applied by a human).
pub const LABEL_READY: &str = "meguri:ready";
/// Projection: the issue's implementation PR is open.
pub const LABEL_IMPLEMENTING: &str = "meguri:implementing";
/// Projection: meguri claimed the issue (an agent is working on it).
pub const LABEL_WORKING: &str = "meguri:working";
/// Input: dispatch must skip this issue's task (the phone-operable emergency
/// stop; in-flight runs are not interrupted).
pub const LABEL_HOLD: &str = "meguri:hold";
/// Projection: meguri gave up and a human needs to look (a comment explains
/// why). Clearing it while `ready` is still on re-queues the task.
pub const LABEL_NEEDS_HUMAN: &str = "meguri:needs-human";

/// Open/closed lifecycle of an issue on the forge — the authority that
/// decides when local resources tied to the issue (worktrees, panes) may be
/// reclaimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueState {
    Open,
    Closed,
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
    /// File a new issue; returns its number (`meguri add` capture).
    async fn create_issue(&self, title: &str, body: &str, labels: &[&str]) -> Result<i64>;
    async fn add_label(&self, issue: i64, label: &str) -> Result<()>;
    async fn remove_label(&self, issue: i64, label: &str) -> Result<()>;
    async fn remove_pr_label(&self, pr: i64, label: &str) -> Result<()>;
    async fn comment(&self, issue: i64, body: &str) -> Result<()>;
    /// Open a pull request. `labels` are applied as part of creation (a single
    /// forge operation), so the PR is never observable unlabeled — the
    /// escalate-time needs-human draft (issue #209) relies on this to be
    /// excluded by `pr_is_touchable` from its first moment. Pass `&[]` when the
    /// PR needs no label at birth.
    async fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
        labels: &[&str],
    ) -> Result<CreatedPr>;
    /// The PR whose head is `branch`, if any — open PRs win over closed or
    /// merged ones. The reaper uses the merged state to recognize squash and
    /// rebase merges, whose branch tips never become ancestors of the base.
    async fn pr_for_branch(&self, branch: &str) -> Result<Option<PullRequest>>;
    /// Open PRs the forge already cross-references to `issue` (GitHub's own
    /// "Development"/timeline linkage — not a meguri label or comment). The
    /// worker checks this immediately before opening a new PR (issue #249)
    /// so a rail-external PR that already covers the issue is never
    /// duplicated.
    async fn linked_open_prs(&self, issue: i64) -> Result<Vec<PullRequest>>;
    /// Open PRs (candidates for fixer discovery).
    async fn list_open_prs(&self) -> Result<Vec<PullRequest>>;
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
