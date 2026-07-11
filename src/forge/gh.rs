//! GitHub gateway backed by the `gh` CLI (reuses the user's existing auth,
//! same approach as looper).

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;

use super::{CreatedPr, Forge, Issue};

pub struct GhForge {
    /// "owner/repo"
    repo: String,
}

impl GhForge {
    pub fn new(repo_slug: &str) -> Self {
        Self {
            repo: repo_slug.to_string(),
        }
    }

    async fn gh(&self, args: &[&str]) -> Result<String> {
        let out = tokio::process::Command::new("gh")
            .args(args)
            .output()
            .await
            .context("spawning gh (is the GitHub CLI installed?)")?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
        } else {
            bail!(
                "gh {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }

    fn issue_from_json(v: &Value) -> Option<Issue> {
        Some(Issue {
            number: v.get("number")?.as_i64()?,
            title: v.get("title")?.as_str()?.to_string(),
            body: v
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            labels: v
                .get("labels")
                .and_then(Value::as_array)
                .map(|labels| {
                    labels
                        .iter()
                        .filter_map(|l| l.get("name").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// --edit doesn't create missing labels — ensure it exists first
    /// (idempotent; ignore "already exists" failures).
    async fn ensure_label(&self, label: &str) {
        let _ = self
            .gh(&[
                "label",
                "create",
                label,
                "--repo",
                &self.repo,
                "--color",
                "1D76DB",
                "--description",
                "managed by meguri",
            ])
            .await;
    }
}

#[async_trait]
impl Forge for GhForge {
    async fn get_issue(&self, number: i64) -> Result<Issue> {
        let raw = self
            .gh(&[
                "issue",
                "view",
                &number.to_string(),
                "--repo",
                &self.repo,
                "--json",
                "number,title,body,labels",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh issue view output")?;
        Self::issue_from_json(&v).with_context(|| format!("unexpected issue shape: {raw}"))
    }

    async fn list_issues_with_label(&self, label: &str) -> Result<Vec<Issue>> {
        let raw = self
            .gh(&[
                "issue",
                "list",
                "--repo",
                &self.repo,
                "--state",
                "open",
                "--label",
                label,
                "--limit",
                "50",
                "--json",
                "number,title,body,labels",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh issue list output")?;
        Ok(v.as_array()
            .map(|items| items.iter().filter_map(Self::issue_from_json).collect())
            .unwrap_or_default())
    }

    async fn add_label(&self, issue: i64, label: &str) -> Result<()> {
        self.ensure_label(label).await;
        self.gh(&[
            "issue",
            "edit",
            &issue.to_string(),
            "--repo",
            &self.repo,
            "--add-label",
            label,
        ])
        .await?;
        Ok(())
    }

    async fn remove_label(&self, issue: i64, label: &str) -> Result<()> {
        self.gh(&[
            "issue",
            "edit",
            &issue.to_string(),
            "--repo",
            &self.repo,
            "--remove-label",
            label,
        ])
        .await?;
        Ok(())
    }

    async fn add_pr_label(&self, pr: i64, label: &str) -> Result<()> {
        self.ensure_label(label).await;
        self.gh(&[
            "pr",
            "edit",
            &pr.to_string(),
            "--repo",
            &self.repo,
            "--add-label",
            label,
        ])
        .await?;
        Ok(())
    }

    async fn comment(&self, issue: i64, body: &str) -> Result<()> {
        self.gh(&[
            "issue",
            "comment",
            &issue.to_string(),
            "--repo",
            &self.repo,
            "--body",
            body,
        ])
        .await?;
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
        let mut args = vec![
            "pr", "create", "--repo", &self.repo, "--head", head, "--base", base, "--title", title,
            "--body", body,
        ];
        if draft {
            args.push("--draft");
        }
        let url = self.gh(&args).await?;
        let url = url
            .lines()
            .rev()
            .find(|l| l.starts_with("https://"))
            .unwrap_or(&url)
            .trim()
            .to_string();
        let number = url
            .rsplit('/')
            .next()
            .and_then(|n| n.parse::<i64>().ok())
            .unwrap_or(0);
        Ok(CreatedPr { number, url })
    }
}
