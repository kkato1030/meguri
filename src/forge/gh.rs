//! GitHub gateway backed by the `gh` CLI (reuses the user's existing auth,
//! same approach as looper).

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;

use super::{CreatedPr, Forge, Issue, IssueState, PullRequest, ReviewComment, ReviewThread};

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

    fn labels_from_json(v: &Value) -> Vec<String> {
        v.get("labels")
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|l| l.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn pr_from_json(v: &Value) -> Option<PullRequest> {
        Some(PullRequest {
            number: v.get("number")?.as_i64()?,
            title: v.get("title")?.as_str()?.to_string(),
            body: v
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            url: v
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            head_branch: v
                .get("headRefName")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            head_sha: v
                .get("headRefOid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            state: v
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("open")
                .to_lowercase(),
            labels: Self::labels_from_json(v),
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

    async fn issue_state(&self, number: i64) -> Result<IssueState> {
        let raw = self
            .gh(&[
                "issue",
                "view",
                &number.to_string(),
                "--repo",
                &self.repo,
                "--json",
                "state",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh issue view output")?;
        let state = v
            .get("state")
            .and_then(Value::as_str)
            .with_context(|| format!("unexpected issue state shape: {raw}"))?;
        Ok(if state.eq_ignore_ascii_case("closed") {
            IssueState::Closed
        } else {
            IssueState::Open
        })
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

    async fn create_issue(&self, title: &str, body: &str, labels: &[&str]) -> Result<i64> {
        // `gh issue create --label` fails on labels that don't exist yet.
        for label in labels {
            self.ensure_label(label).await;
        }
        let mut args = vec![
            "issue", "create", "--repo", &self.repo, "--title", title, "--body", body,
        ];
        for label in labels {
            args.push("--label");
            args.push(label);
        }
        let url = self.gh(&args).await?;
        // gh prints the issue URL (possibly after progress lines).
        let url = url
            .lines()
            .rev()
            .find(|l| l.starts_with("https://"))
            .unwrap_or(&url)
            .trim();
        url.rsplit('/')
            .next()
            .and_then(|n| n.parse::<i64>().ok())
            .with_context(|| format!("cannot parse issue number from gh output: {url}"))
    }

    async fn update_issue_body(&self, number: i64, body: &str) -> Result<()> {
        self.gh(&[
            "issue",
            "edit",
            &number.to_string(),
            "--repo",
            &self.repo,
            "--body",
            body,
        ])
        .await?;
        Ok(())
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

    async fn remove_pr_label(&self, pr: i64, label: &str) -> Result<()> {
        self.gh(&[
            "pr",
            "edit",
            &pr.to_string(),
            "--repo",
            &self.repo,
            "--remove-label",
            label,
        ])
        .await?;
        Ok(())
    }

    async fn get_pr(&self, number: i64) -> Result<PullRequest> {
        let raw = self
            .gh(&[
                "pr",
                "view",
                &number.to_string(),
                "--repo",
                &self.repo,
                "--json",
                "number,title,body,labels,headRefName,headRefOid,state,url",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh pr view output")?;
        Self::pr_from_json(&v).with_context(|| format!("unexpected PR shape: {raw}"))
    }

    async fn list_prs_with_label(&self, label: &str) -> Result<Vec<PullRequest>> {
        let raw = self
            .gh(&[
                "pr",
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
                "number,title,body,labels,headRefName,headRefOid,state,url",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh pr list output")?;
        Ok(v.as_array()
            .map(|items| items.iter().filter_map(Self::pr_from_json).collect())
            .unwrap_or_default())
    }

    async fn pr_diff(&self, number: i64) -> Result<String> {
        self.gh(&["pr", "diff", &number.to_string(), "--repo", &self.repo])
            .await
    }

    async fn pr_comments(&self, number: i64) -> Result<Vec<String>> {
        let raw = self
            .gh(&[
                "pr",
                "view",
                &number.to_string(),
                "--repo",
                &self.repo,
                "--json",
                "comments",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh pr view comments")?;
        Ok(v.get("comments")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|c| c.get("body").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn comment_pr(&self, pr: i64, body: &str) -> Result<()> {
        self.gh(&[
            "pr",
            "comment",
            &pr.to_string(),
            "--repo",
            &self.repo,
            "--body",
            body,
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

    async fn pr_comment(&self, pr: i64, body: &str) -> Result<()> {
        self.gh(&[
            "pr",
            "comment",
            &pr.to_string(),
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

    async fn list_open_prs(&self) -> Result<Vec<PullRequest>> {
        let raw = self
            .gh(&[
                "pr",
                "list",
                "--repo",
                &self.repo,
                "--state",
                "open",
                "--limit",
                "50",
                "--json",
                "number,title,body,url,headRefName,headRefOid,state,labels",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh pr list output")?;
        Ok(v.as_array()
            .map(|items| items.iter().filter_map(Self::pr_from_json).collect())
            .unwrap_or_default())
    }

    /// Thread resolution state only exists in GitHub's GraphQL API; the REST
    /// review-comment endpoints don't expose it.
    async fn list_review_threads(&self, pr: i64) -> Result<Vec<ReviewThread>> {
        let (owner, name) = self
            .repo
            .split_once('/')
            .with_context(|| format!("repo slug `{}` is not owner/name", self.repo))?;
        let query = "query($owner:String!,$name:String!,$number:Int!){\
             repository(owner:$owner,name:$name){pullRequest(number:$number){\
             reviewThreads(first:100){nodes{id isResolved path line \
             comments(first:100){nodes{author{login} body}}}}}}}";
        let raw = self
            .gh(&[
                "api",
                "graphql",
                "-f",
                &format!("query={query}"),
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={name}"),
                "-F",
                &format!("number={pr}"),
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing review-threads GraphQL")?;
        let nodes = v
            .pointer("/data/repository/pullRequest/reviewThreads/nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(nodes
            .iter()
            .filter_map(|t| {
                Some(ReviewThread {
                    id: t.get("id")?.as_str()?.to_string(),
                    resolved: t
                        .get("isResolved")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    path: t.get("path").and_then(Value::as_str).map(str::to_string),
                    line: t.get("line").and_then(Value::as_i64),
                    comments: t
                        .pointer("/comments/nodes")
                        .and_then(Value::as_array)
                        .map(|cs| {
                            cs.iter()
                                .map(|c| ReviewComment {
                                    author: c
                                        .pointer("/author/login")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    body: c
                                        .get("body")
                                        .and_then(Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
            })
            .collect())
    }

    async fn reply_review_thread(&self, _pr: i64, thread_id: &str, body: &str) -> Result<()> {
        let mutation = "mutation($threadId:ID!,$body:String!){\
             addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$threadId,body:$body})\
             {comment{id}}}";
        self.gh(&[
            "api",
            "graphql",
            "-f",
            &format!("query={mutation}"),
            "-f",
            &format!("threadId={thread_id}"),
            "-f",
            &format!("body={body}"),
        ])
        .await?;
        Ok(())
    }
}
