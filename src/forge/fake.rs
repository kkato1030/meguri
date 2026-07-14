//! In-memory Forge for tests: records every mutation for assertions.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::{Result, bail};
use async_trait::async_trait;

use super::{
    Blocker, CheckRollup, CheckRun, CheckState, CreatedPr, Forge, Issue, IssueState,
    MergeableState, PullRequest, ReviewComment, ReviewCommentDraft, ReviewThread,
};

#[derive(Debug, Clone)]
pub struct RecordedPr {
    pub number: i64,
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub labels: Vec<String>,
    pub head_sha: String,
    /// "open", "merged" or "closed".
    pub state: String,
}

#[derive(Default)]
pub struct FakeForge {
    pub issues: Mutex<Vec<Issue>>,
    /// Closed issues: number → state_reason ("completed", "not_planned", ...).
    pub closed: Mutex<HashMap<i64, String>>,
    /// Dependency graph: issue → numbers of the issues blocking it.
    pub blocked_by: Mutex<HashMap<i64, Vec<i64>>>,
    /// Issues whose blocked_by lookup fails (unreadable-blocker scenarios).
    pub blocked_by_errors: Mutex<HashSet<i64>>,
    pub comments: Mutex<Vec<(i64, String)>>,
    pub prs: Mutex<Vec<RecordedPr>>,
    /// Review threads per PR number.
    pub threads: Mutex<Vec<(i64, ReviewThread)>>,
    pub pr_comments: Mutex<Vec<(i64, String)>>,
    pub pr_diffs: Mutex<HashMap<i64, String>>,
    /// Mergeability per PR number; unset PRs report `Unknown` (like GitHub
    /// before it finished computing).
    pub mergeable: Mutex<HashMap<i64, MergeableState>>,
    /// Branches whose pr_for_branch lookup fails (forge-outage scenarios).
    pub pr_for_branch_errors: Mutex<HashSet<String>>,
    /// CI checks per PR number (ci-fixer tests); unset PRs report an empty
    /// rollup (Success — no CI configured).
    pub checks: Mutex<HashMap<i64, Vec<CheckRun>>>,
    /// What pr_failed_check_logs returns, per PR number.
    pub failed_check_logs: Mutex<HashMap<i64, String>>,
    /// Bodies of PR reviews posted via create_pr_review, per PR number
    /// (the inline comments land in `threads`).
    pub pr_reviews: Mutex<Vec<(i64, String)>>,
    /// PRs whose create_pr_review call fails (inline-anchor-rejected
    /// scenarios; the impl-reviewer falls back to a summary comment).
    pub create_pr_review_errors: Mutex<HashSet<i64>>,
}

impl FakeForge {
    pub fn with_issue(number: i64, title: &str, body: &str, labels: &[&str]) -> Self {
        let forge = Self::default();
        forge.issues.lock().unwrap().push(Issue {
            number,
            title: title.into(),
            body: body.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        });
        forge
    }

    pub fn close_issue(&self, number: i64) {
        self.close_issue_as(number, "completed");
    }

    /// Close with an explicit state_reason ("not_planned", "duplicate", ...).
    pub fn close_issue_as(&self, number: i64, state_reason: &str) {
        self.closed
            .lock()
            .unwrap()
            .insert(number, state_reason.to_string());
    }

    /// Record that `issue` is blocked by `blocker` (GitHub-native
    /// dependency); the blocker's state comes from the closed map.
    pub fn block_issue(&self, issue: i64, blocker: i64) {
        self.blocked_by
            .lock()
            .unwrap()
            .entry(issue)
            .or_default()
            .push(blocker);
    }

    /// Make blocked_by lookups for `issue` fail (unreadable blockers).
    pub fn fail_blocked_by(&self, issue: i64) {
        self.blocked_by_errors.lock().unwrap().insert(issue);
    }

    /// Make pr_for_branch lookups for `branch` fail (forge outage).
    pub fn fail_pr_for_branch(&self, branch: &str) {
        self.pr_for_branch_errors
            .lock()
            .unwrap()
            .insert(branch.to_string());
    }

    /// Make create_pr_review fail for `pr` (e.g. GitHub rejecting an inline
    /// anchor that is not part of the diff).
    pub fn fail_create_pr_review(&self, pr: i64) {
        self.create_pr_review_errors.lock().unwrap().insert(pr);
    }

    /// Review bodies posted on `pr` via create_pr_review.
    pub fn pr_reviews_of(&self, pr: i64) -> Vec<String> {
        self.pr_reviews
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| *n == pr)
            .map(|(_, b)| b.clone())
            .collect()
    }

    /// Seed a pull request as if it already existed on the forge (reviewer
    /// tests; `create_pr` records worker/planner-created ones).
    pub fn add_pr(
        &self,
        number: i64,
        title: &str,
        body: &str,
        labels: &[&str],
        head_branch: &str,
        head_sha: &str,
    ) {
        self.prs.lock().unwrap().push(RecordedPr {
            number,
            head: head_branch.into(),
            base: "main".into(),
            title: title.into(),
            body: body.into(),
            draft: false,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            head_sha: head_sha.into(),
            state: "open".into(),
        });
    }

    pub fn set_pr_diff(&self, number: i64, diff: &str) {
        self.pr_diffs.lock().unwrap().insert(number, diff.into());
    }

    /// Simulate the forge's mergeability verdict (conflict-resolver tests).
    pub fn set_pr_mergeable(&self, number: i64, state: MergeableState) {
        self.mergeable.lock().unwrap().insert(number, state);
    }

    /// Record one CI check's verdict on a PR head (ci-fixer tests). Calls
    /// accumulate, like checks on a real head.
    pub fn set_pr_check(&self, number: i64, name: &str, state: CheckState) {
        self.checks
            .lock()
            .unwrap()
            .entry(number)
            .or_default()
            .push(CheckRun {
                name: name.into(),
                state,
                url: format!("https://fake.example/actions/runs/{number}/job/1"),
            });
    }

    /// Simulate CI resetting on a new head (a fresh push clears the old
    /// head's checks).
    pub fn clear_pr_checks(&self, number: i64) {
        self.checks.lock().unwrap().remove(&number);
    }

    /// What the fake returns as the PR's failed-job logs.
    pub fn set_pr_failed_check_logs(&self, number: i64, logs: &str) {
        self.failed_check_logs
            .lock()
            .unwrap()
            .insert(number, logs.into());
    }

    /// Simulate a new push to the PR branch (head moves, review marker for
    /// the old head no longer matches).
    pub fn set_pr_head(&self, number: i64, head_sha: &str) {
        let mut prs = self.prs.lock().unwrap();
        if let Some(pr) = prs.iter_mut().find(|p| p.number == number) {
            pr.head_sha = head_sha.into();
        }
    }

    /// Numbers of the issues recorded as blocking `number`.
    pub fn blockers_of(&self, number: i64) -> Vec<i64> {
        self.blocked_by
            .lock()
            .unwrap()
            .get(&number)
            .cloned()
            .unwrap_or_default()
    }

    /// Snapshot of every issue on the fake forge (creation-order).
    pub fn all_issues(&self) -> Vec<Issue> {
        self.issues.lock().unwrap().clone()
    }

    pub fn labels_of(&self, number: i64) -> Vec<String> {
        self.issues
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.number == number)
            .map(|i| i.labels.clone())
            .unwrap_or_default()
    }

    pub fn pr_labels_of(&self, number: i64) -> Vec<String> {
        self.prs
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.number == number)
            .map(|p| p.labels.clone())
            .unwrap_or_default()
    }

    pub fn prs(&self) -> Vec<RecordedPr> {
        self.prs.lock().unwrap().clone()
    }

    pub fn comments_of(&self, number: i64) -> Vec<String> {
        self.comments
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| *n == number)
            .map(|(_, c)| c.clone())
            .collect()
    }

    pub fn pr_comments_of(&self, number: i64) -> Vec<String> {
        self.pr_comments
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| *n == number)
            .map(|(_, c)| c.clone())
            .collect()
    }

    /// Seed an already-open PR (as if a worker run shipped it earlier);
    /// returns its number.
    pub fn push_pr(&self, head: &str, title: &str, labels: &[&str]) -> i64 {
        let mut prs = self.prs.lock().unwrap();
        let number = prs.len() as i64 + 1;
        prs.push(RecordedPr {
            number,
            head: head.into(),
            base: "main".into(),
            title: title.into(),
            body: String::new(),
            draft: true,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            head_sha: String::new(),
            state: "open".into(),
        });
        number
    }

    pub fn set_pr_state(&self, pr: i64, state: &str) {
        let mut prs = self.prs.lock().unwrap();
        if let Some(rec) = prs.iter_mut().find(|p| p.number == pr) {
            rec.state = state.to_string();
        }
    }

    pub fn pr_labels(&self, pr: i64) -> Vec<String> {
        self.pr_labels_of(pr)
    }

    /// The reviewer side of the ping-pong: open an unresolved thread.
    pub fn add_review_thread(&self, pr: i64, id: &str, path: &str, author: &str, body: &str) {
        self.threads.lock().unwrap().push((
            pr,
            ReviewThread {
                id: id.into(),
                resolved: false,
                path: Some(path.into()),
                line: None,
                comments: vec![ReviewComment {
                    author: author.into(),
                    body: body.into(),
                }],
            },
        ));
    }

    /// The reviewer follows up inside an existing thread.
    pub fn add_thread_comment(&self, pr: i64, thread_id: &str, author: &str, body: &str) {
        let mut threads = self.threads.lock().unwrap();
        if let Some((_, t)) = threads
            .iter_mut()
            .find(|(n, t)| *n == pr && t.id == thread_id)
        {
            t.comments.push(ReviewComment {
                author: author.into(),
                body: body.into(),
            });
        }
    }

    /// The reviewer accepts the fix.
    pub fn resolve_thread(&self, pr: i64, thread_id: &str) {
        let mut threads = self.threads.lock().unwrap();
        if let Some((_, t)) = threads
            .iter_mut()
            .find(|(n, t)| *n == pr && t.id == thread_id)
        {
            t.resolved = true;
        }
    }

    pub fn threads_of(&self, pr: i64) -> Vec<ReviewThread> {
        self.threads
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| *n == pr)
            .map(|(_, t)| t.clone())
            .collect()
    }

    fn pr_to_public(pr: &RecordedPr) -> PullRequest {
        PullRequest {
            number: pr.number,
            title: pr.title.clone(),
            body: pr.body.clone(),
            url: format!("https://fake.example/pr/{}", pr.number),
            head_branch: pr.head.clone(),
            head_sha: pr.head_sha.clone(),
            state: pr.state.clone(),
            labels: pr.labels.clone(),
        }
    }
}

#[async_trait]
impl Forge for FakeForge {
    async fn get_issue(&self, number: i64) -> Result<Issue> {
        self.issues
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.number == number)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("issue #{number} not found"))
    }

    async fn issue_state(&self, number: i64) -> Result<IssueState> {
        if self.closed.lock().unwrap().contains_key(&number) {
            return Ok(IssueState::Closed);
        }
        if self
            .issues
            .lock()
            .unwrap()
            .iter()
            .any(|i| i.number == number)
        {
            return Ok(IssueState::Open);
        }
        // Issues and PRs share the number space (as on GitHub, where
        // `gh issue view <PR#>` resolves the PR): merged counts as closed,
        // anything unrecognized is an error, never a silent Open.
        let pr_state = self
            .prs
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.number == number)
            .map(|p| p.state.clone());
        match pr_state.as_deref() {
            Some("merged") | Some("closed") => Ok(IssueState::Closed),
            Some("open") => Ok(IssueState::Open),
            Some(other) => bail!("unrecognized state `{other}` of PR #{number}"),
            None => bail!("issue #{number} not found"),
        }
    }

    async fn list_issues_with_label(&self, label: &str) -> Result<Vec<Issue>> {
        let closed = self.closed.lock().unwrap();
        Ok(self
            .issues
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.has_label(label) && !closed.contains_key(&i.number))
            .cloned()
            .collect())
    }

    async fn list_open_issues(&self) -> Result<Vec<Issue>> {
        let closed = self.closed.lock().unwrap();
        Ok(self
            .issues
            .lock()
            .unwrap()
            .iter()
            .filter(|i| !closed.contains_key(&i.number))
            .cloned()
            .collect())
    }

    async fn blocked_by(&self, issue: i64) -> Result<Vec<Blocker>> {
        if self.blocked_by_errors.lock().unwrap().contains(&issue) {
            bail!("blocked_by of issue #{issue} is unreadable");
        }
        let closed = self.closed.lock().unwrap();
        Ok(self
            .blocked_by
            .lock()
            .unwrap()
            .get(&issue)
            .map(|blockers| {
                blockers
                    .iter()
                    .map(|n| Blocker {
                        number: *n,
                        state: if closed.contains_key(n) {
                            "closed"
                        } else {
                            "open"
                        }
                        .into(),
                        state_reason: closed.get(n).cloned(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn create_issue(&self, title: &str, body: &str, labels: &[&str]) -> Result<i64> {
        let mut issues = self.issues.lock().unwrap();
        let number = issues.iter().map(|i| i.number).max().unwrap_or(0) + 1;
        issues.push(Issue {
            number,
            title: title.into(),
            body: body.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        });
        Ok(number)
    }

    async fn add_blocked_by(&self, issue: i64, blocker: i64) -> Result<()> {
        {
            let issues = self.issues.lock().unwrap();
            for number in [issue, blocker] {
                if !issues.iter().any(|i| i.number == number) {
                    bail!("issue #{number} not found");
                }
            }
        }
        self.block_issue(issue, blocker);
        Ok(())
    }

    async fn update_issue_body(&self, number: i64, body: &str) -> Result<()> {
        let mut issues = self.issues.lock().unwrap();
        let Some(i) = issues.iter_mut().find(|i| i.number == number) else {
            bail!("issue #{number} not found");
        };
        i.body = body.to_string();
        Ok(())
    }

    async fn add_label(&self, issue: i64, label: &str) -> Result<()> {
        let mut issues = self.issues.lock().unwrap();
        let Some(i) = issues.iter_mut().find(|i| i.number == issue) else {
            bail!("issue #{issue} not found");
        };
        if !i.labels.iter().any(|l| l == label) {
            i.labels.push(label.to_string());
        }
        Ok(())
    }

    async fn remove_label(&self, issue: i64, label: &str) -> Result<()> {
        let mut issues = self.issues.lock().unwrap();
        let Some(i) = issues.iter_mut().find(|i| i.number == issue) else {
            bail!("issue #{issue} not found");
        };
        i.labels.retain(|l| l != label);
        Ok(())
    }

    async fn add_pr_label(&self, pr: i64, label: &str) -> Result<()> {
        let mut prs = self.prs.lock().unwrap();
        let Some(rec) = prs.iter_mut().find(|p| p.number == pr) else {
            bail!("PR #{pr} not found");
        };
        if !rec.labels.iter().any(|l| l == label) {
            rec.labels.push(label.to_string());
        }
        Ok(())
    }

    async fn remove_pr_label(&self, pr: i64, label: &str) -> Result<()> {
        let mut prs = self.prs.lock().unwrap();
        let Some(rec) = prs.iter_mut().find(|p| p.number == pr) else {
            bail!("PR #{pr} not found");
        };
        rec.labels.retain(|l| l != label);
        Ok(())
    }

    async fn update_pr_title(&self, pr: i64, title: &str) -> Result<()> {
        let mut prs = self.prs.lock().unwrap();
        let Some(rec) = prs.iter_mut().find(|p| p.number == pr) else {
            bail!("PR #{pr} not found");
        };
        rec.title = title.to_string();
        Ok(())
    }

    async fn update_pr_body(&self, pr: i64, body: &str) -> Result<()> {
        let mut prs = self.prs.lock().unwrap();
        let Some(rec) = prs.iter_mut().find(|p| p.number == pr) else {
            bail!("PR #{pr} not found");
        };
        rec.body = body.to_string();
        Ok(())
    }

    async fn get_pr(&self, number: i64) -> Result<PullRequest> {
        self.prs
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.number == number)
            .map(Self::pr_to_public)
            .ok_or_else(|| anyhow::anyhow!("PR #{number} not found"))
    }

    async fn pr_for_branch(&self, branch: &str) -> Result<Option<PullRequest>> {
        if self.pr_for_branch_errors.lock().unwrap().contains(branch) {
            bail!("forge lookup of branch {branch} is unavailable");
        }
        let prs = self.prs.lock().unwrap();
        let matching: Vec<&RecordedPr> = prs.iter().filter(|p| p.head == branch).collect();
        // Like `gh pr view <branch>`: an open PR wins over closed/merged ones.
        Ok(matching
            .iter()
            .find(|p| p.state == "open")
            .or(matching.last())
            .map(|p| Self::pr_to_public(p)))
    }

    async fn pr_mergeable(&self, number: i64) -> Result<MergeableState> {
        Ok(self
            .mergeable
            .lock()
            .unwrap()
            .get(&number)
            .copied()
            .unwrap_or(MergeableState::Unknown))
    }

    async fn pr_check_rollup(&self, number: i64) -> Result<CheckRollup> {
        Ok(CheckRollup {
            checks: self
                .checks
                .lock()
                .unwrap()
                .get(&number)
                .cloned()
                .unwrap_or_default(),
        })
    }

    async fn pr_failed_check_logs(&self, number: i64) -> Result<String> {
        Ok(self
            .failed_check_logs
            .lock()
            .unwrap()
            .get(&number)
            .cloned()
            .unwrap_or_default())
    }

    async fn list_prs_with_label(&self, label: &str) -> Result<Vec<PullRequest>> {
        Ok(self
            .prs
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.labels.iter().any(|l| l == label))
            .map(Self::pr_to_public)
            .collect())
    }

    async fn pr_diff(&self, number: i64) -> Result<String> {
        Ok(self
            .pr_diffs
            .lock()
            .unwrap()
            .get(&number)
            .cloned()
            .unwrap_or_default())
    }

    async fn pr_comments(&self, number: i64) -> Result<Vec<String>> {
        Ok(self.pr_comments_of(number))
    }

    async fn comment_pr(&self, pr: i64, body: &str) -> Result<()> {
        self.pr_comments.lock().unwrap().push((pr, body.into()));
        Ok(())
    }

    async fn comment(&self, issue: i64, body: &str) -> Result<()> {
        self.comments.lock().unwrap().push((issue, body.into()));
        Ok(())
    }

    async fn pr_comment(&self, pr: i64, body: &str) -> Result<()> {
        self.comments.lock().unwrap().push((pr, body.into()));
        Ok(())
    }

    async fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
    ) -> Result<CreatedPr> {
        let mut prs = self.prs.lock().unwrap();
        let number = prs.len() as i64 + 1;
        prs.push(RecordedPr {
            number,
            head: head.into(),
            base: base.into(),
            title: title.into(),
            body: body.into(),
            draft,
            labels: Vec::new(),
            head_sha: String::new(),
            state: "open".into(),
        });
        Ok(CreatedPr {
            number,
            url: format!("https://fake.example/pr/{number}"),
        })
    }

    async fn list_open_prs(&self) -> Result<Vec<PullRequest>> {
        Ok(self
            .prs
            .lock()
            .unwrap()
            .iter()
            .filter(|rec| rec.state == "open")
            .map(Self::pr_to_public)
            .collect())
    }

    async fn list_review_threads(&self, pr: i64) -> Result<Vec<ReviewThread>> {
        Ok(self.threads_of(pr))
    }

    async fn reply_review_thread(&self, pr: i64, thread_id: &str, body: &str) -> Result<()> {
        let mut threads = self.threads.lock().unwrap();
        let Some((_, t)) = threads
            .iter_mut()
            .find(|(n, t)| *n == pr && t.id == thread_id)
        else {
            bail!("thread {thread_id} on PR #{pr} not found");
        };
        t.comments.push(ReviewComment {
            author: "meguri".into(),
            body: body.into(),
        });
        Ok(())
    }

    async fn create_pr_review(
        &self,
        pr: i64,
        body: &str,
        comments: &[ReviewCommentDraft],
    ) -> Result<()> {
        if self.create_pr_review_errors.lock().unwrap().contains(&pr) {
            bail!("create_pr_review on PR #{pr} rejected (fake)");
        }
        self.pr_reviews.lock().unwrap().push((pr, body.into()));
        let mut threads = self.threads.lock().unwrap();
        for draft in comments {
            let id = format!("fake-thread-{}", threads.len() + 1);
            threads.push((
                pr,
                ReviewThread {
                    id,
                    resolved: false,
                    path: Some(draft.path.clone()),
                    line: Some(draft.line as i64),
                    comments: vec![ReviewComment {
                        author: "meguri".into(),
                        body: draft.body.clone(),
                    }],
                },
            ));
        }
        Ok(())
    }
}
