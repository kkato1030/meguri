//! Forge abstraction (GitHub for MVP). Follows looper's "Authority"
//! principle: labels and comments on the forge are the durable source of
//! truth for workflow state, never in-memory agent output.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

pub mod fake;
pub mod gh;

// Issue labels form two axes (ADR 0005). Axis 1 — the phase: a meguri-engaged
// open issue always carries exactly one of `plan` / `speccing` / `ready` /
// `implementing`, so an unlabeled issue means "untriaged". Axis 2 — the ball
// (who holds it): `working` / `needs-human` / `hold` layer on top of the phase
// without removing it.

/// Phase (axis 1): issue is queued for the worker loop (applied by a human).
pub const LABEL_READY: &str = "meguri:ready";
/// Phase (axis 1): the issue's implementation PR is open (CI fixing, review,
/// awaiting merge all included). The worker/spec-worker apply it at PR
/// creation/takeover and it stays until the issue closes. Load-bearing: it
/// backs the "unlabeled = untriaged" invariant.
pub const LABEL_IMPLEMENTING: &str = "meguri:implementing";
/// meguri claimed the issue (dedup across restarts and hosts).
pub const LABEL_WORKING: &str = "meguri:working";
/// Discovery must skip this issue.
pub const LABEL_HOLD: &str = "meguri:hold";
/// meguri gave up and a human needs to look (a comment explains why).
pub const LABEL_NEEDS_HUMAN: &str = "meguri:needs-human";
/// The cleaner loop's per-project report issue (one per project; its body is
/// a snapshot of the current divergence, rewritten on every sweep).
pub const LABEL_CLEAN_REPORT: &str = "meguri:clean-report";
/// The triage loop's per-project report issue (issue #85). Read-only, like
/// the cleaner's: its body is a snapshot of the current triage
/// recommendations for untriaged open issues, rewritten on every sweep.
pub const LABEL_TRIAGE_REPORT: &str = "meguri:triage-report";
/// Triage v1 advise (issue #87): proposes `meguri:ready` on the issue itself.
/// A human promotes it verbatim; meguri never applies the real label.
pub const LABEL_TRIAGE_READY: &str = "meguri:triage-ready";
/// Triage v1 advise (issue #87): proposes `meguri:plan`, same rules as
/// [`LABEL_TRIAGE_READY`].
pub const LABEL_TRIAGE_PLAN: &str = "meguri:triage-plan";
/// Triage v1 advise (issue #87): proposes `meguri:needs-human`, same rules as
/// [`LABEL_TRIAGE_READY`].
pub const LABEL_TRIAGE_NEEDS_HUMAN: &str = "meguri:triage-needs-human";
/// All three triage-advise proposal labels. They carry the `meguri:` prefix
/// (so worker/planner discovery — keyed on the exact real labels, never a
/// prefix scan — cannot mistake one for a go-ahead) but are deliberately
/// excluded from the two-axis phase/ball vocabulary: a proposal is not yet a
/// decision, so it must not read as "engaged" to triage's own re-triage gate.
pub const TRIAGE_PROPOSAL_LABELS: [&str; 3] = [
    LABEL_TRIAGE_READY,
    LABEL_TRIAGE_PLAN,
    LABEL_TRIAGE_NEEDS_HUMAN,
];

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
    /// The blocker issue's body, as GitHub's dependency endpoint returns it
    /// (the whole issue object). The decompose materializer matches its
    /// per-child marker here to recognize an already-created child as the
    /// strongly-consistent authority (issue #134); empty when the forge did
    /// not supply one. The dependency gate ignores it.
    pub body: String,
    /// The blocker issue's home repo slug (`owner/repo`) — a cross-repo
    /// decomposition child lives in a workspace sibling, so identifying it
    /// needs the repo, not just the number (issue #134 / #154). Empty when the
    /// forge did not supply one.
    pub repo: String,
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
    /// issue #24; the cleaner's report issue, issue #44).
    async fn create_issue(&self, title: &str, body: &str, labels: &[&str]) -> Result<i64>;
    async fn add_label(&self, issue: i64, label: &str) -> Result<()>;
    async fn remove_label(&self, issue: i64, label: &str) -> Result<()>;
    /// Add a label to a pull request (issues and PRs share GitHub's number
    /// space but need different edit commands).
    async fn add_pr_label(&self, pr: i64, label: &str) -> Result<()>;
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
    /// worker/planner check this immediately before opening a new PR (issue
    /// #249, docs/design/needs-human-friction-and-delivery-speed.md §3-D/§P5)
    /// so a rail-external PR that already covers the issue is never
    /// duplicated.
    async fn linked_open_prs(&self, issue: i64) -> Result<Vec<PullRequest>>;
    /// Open PRs (candidates for fixer discovery).
    async fn list_open_prs(&self) -> Result<Vec<PullRequest>>;
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
            body: String::new(),
            repo: String::new(),
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
