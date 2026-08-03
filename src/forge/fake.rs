//! In-memory Forge for tests: records every mutation for assertions.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::{Result, bail};
use async_trait::async_trait;

use super::{Blocker, CreatedPr, Forge, Issue, IssueState, PullRequest};

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
    /// This fake's own repo slug, if it stands in for a specific repo (issue
    /// #154 cross-repo tests). `None` = the single-repo default: every
    /// `add_blocked_by_in` is treated as same-repo.
    pub slug: Option<String>,
    pub issues: Mutex<Vec<Issue>>,
    /// Closed issues: number → state_reason ("completed", "not_planned", ...).
    pub closed: Mutex<HashMap<i64, String>>,
    /// Dependency graph: issue → numbers of the issues blocking it.
    pub blocked_by: Mutex<HashMap<i64, Vec<i64>>>,
    /// Issues whose blocked_by lookup fails (unreadable-blocker scenarios).
    pub blocked_by_errors: Mutex<HashSet<i64>>,
    pub comments: Mutex<Vec<(i64, String)>>,
    pub prs: Mutex<Vec<RecordedPr>>,
    /// Branches whose pr_for_branch lookup fails (forge-outage scenarios).
    pub pr_for_branch_errors: Mutex<HashSet<String>>,
    /// Issues whose `comment` fails (forge-hiccup scenarios, e.g. triage
    /// auto-promote rolling a label back when the reason comment can't post).
    pub comment_errors: Mutex<HashSet<i64>>,
    /// GitHub's cross-reference ("Development") linkage: issue → PR numbers
    /// mentioning it, real or rail-external (issue #249). Seeded via
    /// [`FakeForge::link_pr_to_issue`]; `linked_open_prs` looks the numbers
    /// up in `prs` and reports only the still-open ones, like the real
    /// forge's timeline query would.
    pub linked_prs: Mutex<HashMap<i64, Vec<i64>>>,
}

impl FakeForge {
    pub fn with_issue(number: i64, title: &str, body: &str, labels: &[&str]) -> Self {
        let forge = Self::default();
        forge.add_issue(number, title, body, labels);
        forge
    }

    /// Seed an additional issue on the fake forge (multi-issue discovery /
    /// cadence tests).
    pub fn add_issue(&self, number: i64, title: &str, body: &str, labels: &[&str]) {
        self.issues.lock().unwrap().push(Issue {
            number,
            title: title.into(),
            body: body.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        });
    }

    /// A fake standing in for a specific repo slug (issue #154 cross-repo
    /// decomposition tests): `add_blocked_by_in` then distinguishes
    /// same-repo blockers (existence-checked) from cross-repo ones (recorded
    /// as-is, since the blocker lives in another fake's store).
    pub fn with_slug(slug: &str) -> Self {
        Self {
            slug: Some(slug.to_string()),
            ..Self::default()
        }
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
    /// dependency); the blocker's state comes from the closed map. Idempotent:
    /// an edge already present is not duplicated (mirrors the real forge's
    /// idempotent add, issue #134).
    pub fn block_issue(&self, issue: i64, blocker: i64) {
        let mut graph = self.blocked_by.lock().unwrap();
        let edges = graph.entry(issue).or_default();
        if !edges.contains(&blocker) {
            edges.push(blocker);
        }
    }

    /// Make blocked_by lookups for `issue` fail (unreadable blockers).
    pub fn fail_blocked_by(&self, issue: i64) {
        self.blocked_by_errors.lock().unwrap().insert(issue);
    }

    /// Make `comment` on `issue` fail (forge hiccup mid-write).
    pub fn fail_comment(&self, issue: i64) {
        self.comment_errors.lock().unwrap().insert(issue);
    }

    /// Make pr_for_branch lookups for `branch` fail (forge outage).
    pub fn fail_pr_for_branch(&self, branch: &str) {
        self.pr_for_branch_errors
            .lock()
            .unwrap()
            .insert(branch.to_string());
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

    /// Seed a GitHub-style cross-reference from `pr` to `issue` (issue #249
    /// tests): a rail-external PR mentioning the issue, or one meguri itself
    /// opened. `linked_open_prs(issue)` reports `pr` back while it stays
    /// open in `self.prs`.
    pub fn link_pr_to_issue(&self, issue: i64, pr: i64) {
        self.linked_prs
            .lock()
            .unwrap()
            .entry(issue)
            .or_default()
            .push(pr);
    }

    /// Simulate a new push to the PR branch (head moves, review marker for
    /// the old head no longer matches).
    pub fn set_pr_head(&self, number: i64, head_sha: &str) {
        let mut prs = self.prs.lock().unwrap();
        if let Some(pr) = prs.iter_mut().find(|p| p.number == number) {
            pr.head_sha = head_sha.into();
        }
    }

    /// Toggle a seeded PR's draft flag (auto-merge draft-readying tests).
    pub fn set_pr_draft(&self, number: i64, draft: bool) {
        let mut prs = self.prs.lock().unwrap();
        if let Some(pr) = prs.iter_mut().find(|p| p.number == number) {
            pr.draft = draft;
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

    /// Whether a PR is currently a draft.
    pub fn is_draft(&self, pr: i64) -> bool {
        self.prs
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.number == pr)
            .map(|p| p.draft)
            .unwrap_or(false)
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
            is_draft: pr.draft,
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

    async fn blocked_by(&self, issue: i64) -> Result<Vec<Blocker>> {
        if self.blocked_by_errors.lock().unwrap().contains(&issue) {
            bail!("blocked_by of issue #{issue} is unreadable");
        }
        let closed = self.closed.lock().unwrap();
        let issues = self.issues.lock().unwrap();
        let own_repo = self.slug.clone().unwrap_or_default();
        Ok(self
            .blocked_by
            .lock()
            .unwrap()
            .get(&issue)
            .map(|blockers| {
                blockers
                    .iter()
                    .map(|n| {
                        let (repo, body) = match issues.iter().find(|i| i.number == *n) {
                            Some(i) => (own_repo.clone(), i.body.clone()),
                            None => Default::default(),
                        };
                        Blocker {
                            number: *n,
                            state: if closed.contains_key(n) {
                                "closed"
                            } else {
                                "open"
                            }
                            .into(),
                            state_reason: closed.get(n).cloned(),
                            body,
                            repo,
                        }
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

    async fn linked_open_prs(&self, issue: i64) -> Result<Vec<PullRequest>> {
        let numbers = self
            .linked_prs
            .lock()
            .unwrap()
            .get(&issue)
            .cloned()
            .unwrap_or_default();
        let prs = self.prs.lock().unwrap();
        Ok(prs
            .iter()
            .filter(|p| p.state == "open" && numbers.contains(&p.number))
            .map(Self::pr_to_public)
            .collect())
    }

    async fn comment(&self, issue: i64, body: &str) -> Result<()> {
        if self.comment_errors.lock().unwrap().contains(&issue) {
            bail!("simulated comment failure on issue #{issue}");
        }
        self.comments.lock().unwrap().push((issue, body.into()));
        Ok(())
    }

    async fn create_pr(
        &self,
        head: &str,
        base: &str,
        title: &str,
        body: &str,
        draft: bool,
        labels: &[&str],
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
            labels: labels.iter().map(|s| s.to_string()).collect(),
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
}

#[cfg(test)]
mod tests {}
