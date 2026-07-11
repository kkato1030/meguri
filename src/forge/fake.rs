//! In-memory Forge for tests: records every mutation for assertions.

use std::sync::Mutex;

use anyhow::{Result, bail};
use async_trait::async_trait;

use super::{CreatedPr, Forge, Issue, PullRequest, ReviewComment, ReviewThread};

#[derive(Debug, Clone)]
pub struct RecordedPr {
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
    pub labels: Vec<String>,
    /// "open", "merged" or "closed".
    pub state: String,
}

#[derive(Default)]
pub struct FakeForge {
    pub issues: Mutex<Vec<Issue>>,
    pub comments: Mutex<Vec<(i64, String)>>,
    pub prs: Mutex<Vec<RecordedPr>>,
    /// Review threads per PR number.
    pub threads: Mutex<Vec<(i64, ReviewThread)>>,
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

    pub fn labels_of(&self, number: i64) -> Vec<String> {
        self.issues
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.number == number)
            .map(|i| i.labels.clone())
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
        prs.push(RecordedPr {
            head: head.into(),
            base: "main".into(),
            title: title.into(),
            body: String::new(),
            draft: true,
            labels: labels.iter().map(|s| s.to_string()).collect(),
            state: "open".into(),
        });
        prs.len() as i64
    }

    pub fn set_pr_state(&self, pr: i64, state: &str) {
        let mut prs = self.prs.lock().unwrap();
        if let Some(rec) = pr_index(pr).and_then(|i| prs.get_mut(i)) {
            rec.state = state.to_string();
        }
    }

    pub fn pr_labels(&self, pr: i64) -> Vec<String> {
        let prs = self.prs.lock().unwrap();
        pr_index(pr)
            .and_then(|i| prs.get(i))
            .map(|rec| rec.labels.clone())
            .unwrap_or_default()
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
}

/// PR numbers are 1-based indices into the recorded list.
fn pr_index(pr: i64) -> Option<usize> {
    usize::try_from(pr).ok().and_then(|n| n.checked_sub(1))
}

fn pull_request(number: i64, rec: &RecordedPr) -> PullRequest {
    PullRequest {
        number,
        title: rec.title.clone(),
        url: format!("https://fake.example/pr/{number}"),
        head_branch: rec.head.clone(),
        state: rec.state.clone(),
        labels: rec.labels.clone(),
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

    async fn list_issues_with_label(&self, label: &str) -> Result<Vec<Issue>> {
        Ok(self
            .issues
            .lock()
            .unwrap()
            .iter()
            .filter(|i| i.has_label(label))
            .cloned()
            .collect())
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
        let Some(rec) = pr_index(pr).and_then(|i| prs.get_mut(i)) else {
            bail!("PR #{pr} not found");
        };
        if !rec.labels.iter().any(|l| l == label) {
            rec.labels.push(label.to_string());
        }
        Ok(())
    }

    async fn remove_pr_label(&self, pr: i64, label: &str) -> Result<()> {
        let mut prs = self.prs.lock().unwrap();
        let Some(rec) = pr_index(pr).and_then(|i| prs.get_mut(i)) else {
            bail!("PR #{pr} not found");
        };
        rec.labels.retain(|l| l != label);
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
        prs.push(RecordedPr {
            head: head.into(),
            base: base.into(),
            title: title.into(),
            body: body.into(),
            draft,
            labels: Vec::new(),
            state: "open".into(),
        });
        Ok(CreatedPr {
            number: prs.len() as i64,
            url: format!("https://fake.example/pr/{}", prs.len()),
        })
    }

    async fn get_pr(&self, number: i64) -> Result<PullRequest> {
        let prs = self.prs.lock().unwrap();
        pr_index(number)
            .and_then(|i| prs.get(i))
            .map(|rec| pull_request(number, rec))
            .ok_or_else(|| anyhow::anyhow!("PR #{number} not found"))
    }

    async fn list_open_prs(&self) -> Result<Vec<PullRequest>> {
        Ok(self
            .prs
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .filter(|(_, rec)| rec.state == "open")
            .map(|(i, rec)| pull_request(i as i64 + 1, rec))
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
}
