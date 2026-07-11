//! In-memory Forge for tests: records every mutation for assertions.

use std::sync::Mutex;

use anyhow::{Result, bail};
use async_trait::async_trait;

use super::{CreatedPr, Forge, Issue};

#[derive(Debug, Clone)]
pub struct RecordedPr {
    pub head: String,
    pub base: String,
    pub title: String,
    pub body: String,
    pub draft: bool,
}

#[derive(Default)]
pub struct FakeForge {
    pub issues: Mutex<Vec<Issue>>,
    pub comments: Mutex<Vec<(i64, String)>>,
    pub prs: Mutex<Vec<RecordedPr>>,
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

    async fn comment(&self, issue: i64, body: &str) -> Result<()> {
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
    ) -> Result<CreatedPr> {
        let mut prs = self.prs.lock().unwrap();
        prs.push(RecordedPr {
            head: head.into(),
            base: base.into(),
            title: title.into(),
            body: body.into(),
            draft,
        });
        Ok(CreatedPr {
            number: prs.len() as i64,
            url: format!("https://fake.example/pr/{}", prs.len()),
        })
    }
}
