//! GitHub gateway backed by the `gh` CLI (reuses the user's existing auth,
//! same approach as looper).

use std::collections::HashSet;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;

use super::{
    ArmOutcome, Blocker, CheckRollup, CheckRun, CheckState, CreatedPr, Forge, Issue, IssueState,
    MergePolicy, MergeState, MergeStateStatus, MergeStrategy, MergeableState, PrComment,
    PullRequest, ReviewComment, ReviewCommentDraft, ReviewThread,
};

/// How much of each failed job log survives into the fix prompt (logs can be
/// megabytes; the failure is almost always at the tail).
const FAILED_LOG_TAIL_LINES: usize = 200;

/// The generic color for a meguri label with no scheme entry.
const DEFAULT_LABEL_COLOR: &str = "1D76DB";

/// Scheme color (hex, no `#`) and description for a known meguri label — the
/// color encodes the two-axis model (ADR 0005): phase labels by stage
/// (plan/ready = blue, speccing = purple, implementing = green) and ball
/// labels by who holds it (working = yellow, needs-human = red, hold = grey).
/// Unknown labels fall back to [`DEFAULT_LABEL_COLOR`].
fn label_scheme(label: &str) -> (&'static str, &'static str) {
    use super::*;
    match label {
        // Axis 1 — phase.
        LABEL_PLAN => ("1D76DB", "meguri phase: awaiting spec planning"),
        LABEL_READY => ("1D76DB", "meguri phase: awaiting implementation"),
        LABEL_SPECCING => ("6F42C1", "meguri phase: spec PR open"),
        LABEL_IMPLEMENTING => ("0E8A16", "meguri phase: implementation PR open"),
        // Axis 2 — ball / who holds it.
        LABEL_WORKING => ("FBCA04", "meguri: an agent is working on it"),
        LABEL_NEEDS_HUMAN => ("B60205", "meguri: a human needs to look (see comment)"),
        LABEL_HOLD => ("CFD3D7", "meguri: intentionally paused by a human"),
        // PR-side spec review labels.
        LABEL_SPEC_REVIEWING => ("6F42C1", "meguri: spec PR awaiting review"),
        LABEL_SPEC_READY => ("0E8A16", "meguri: spec approved; implementation continues"),
        // Bookkeeping.
        LABEL_CLEAN_REPORT => (DEFAULT_LABEL_COLOR, "meguri: cleaner report issue"),
        _ => (DEFAULT_LABEL_COLOR, "managed by meguri"),
    }
}

pub struct GhForge {
    /// "owner/repo"
    repo: String,
}

/// Production [`ForgeFactory`](super::ForgeFactory): builds a [`GhForge`] per
/// repo slug. Used by cross-repo decomposition to reach workspace siblings
/// (issue #154).
pub struct GhForgeFactory;

impl super::ForgeFactory for GhForgeFactory {
    fn for_slug(&self, slug: &str) -> std::sync::Arc<dyn Forge> {
        std::sync::Arc::new(GhForge::new(slug))
    }
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

    /// Run `gh`, returning `Ok(stdout)` on success and `Err(stderr)` on a
    /// non-zero exit (spawn failures still bubble up as `Err(anyhow)`). Lets
    /// callers branch on the error text / HTTP status instead of a flat bail.
    async fn gh_try(&self, args: &[&str]) -> Result<std::result::Result<String, String>> {
        let out = tokio::process::Command::new("gh")
            .args(args)
            .output()
            .await
            .context("spawning gh (is the GitHub CLI installed?)")?;
        if out.status.success() {
            Ok(Ok(String::from_utf8_lossy(&out.stdout)
                .trim_end()
                .to_string()))
        } else {
            Ok(Err(String::from_utf8_lossy(&out.stderr).trim().to_string()))
        }
    }

    /// The `gh pr merge` argument vector, shared by arm (`--auto`) and the
    /// clean-status finalize (no `--auto`). Pinned to the confirmed head via
    /// `--match-head-commit` in both cases (ADR 0003).
    fn merge_args<'a>(
        pr: &'a str,
        repo: &'a str,
        strategy: MergeStrategy,
        head_sha: &'a str,
        auto: bool,
    ) -> Vec<&'a str> {
        let mut args = vec!["pr", "merge", pr, "--repo", repo];
        if auto {
            args.push("--auto");
        }
        args.push(strategy.flag());
        args.push("--match-head-commit");
        args.push(head_sha);
        args
    }

    /// Read an arm attempt's stderr: `Some(Armed)` when auto-merge was already
    /// enabled (idempotent success), `Some(AlreadyClean)` when GitHub reports
    /// the PR already in clean status (no block to reserve → caller finalizes),
    /// `None` for a genuine failure (e.g. the head moved) the caller returns.
    fn classify_arm_stderr(stderr: &str) -> Option<ArmOutcome> {
        let lower = stderr.to_ascii_lowercase();
        if lower.contains("clean status") {
            Some(ArmOutcome::AlreadyClean)
        } else if lower.contains("already enabled")
            || lower.contains("auto-merge is already")
            || lower.contains("pull request is already")
        {
            Some(ArmOutcome::Armed)
        } else {
            None
        }
    }

    /// Interpret the required-checks protection endpoint's failure: 404 means
    /// no classic protection (`Ok(false)`); 403 means the token lacks admin
    /// rights to read protection and we must not degrade to "unprotected"
    /// (ADR 0003) — surface it with the admin-token remedy.
    fn protection_from_stderr(&self, base: &str, stderr: &str) -> Result<bool> {
        if stderr.contains("HTTP 404") {
            Ok(false)
        } else if stderr.contains("HTTP 403") {
            bail!(
                "cannot read branch protection on {}/{base}: the token lacks \
                 admin rights (HTTP 403). Use an admin-scoped token, or set \
                 `require_branch_protection = false` if you are not an admin: {stderr}",
                self.repo
            )
        } else {
            bail!(
                "cannot read branch protection on {}/{base}: {stderr}",
                self.repo
            )
        }
    }

    /// Like [`Self::gh`] but with a JSON payload on stdin (`--input -`), for
    /// endpoints whose body nests arrays that `-f` flags cannot express.
    async fn gh_stdin(&self, args: &[&str], input: &str) -> Result<String> {
        use tokio::io::AsyncWriteExt;
        let mut child = tokio::process::Command::new("gh")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("spawning gh (is the GitHub CLI installed?)")?;
        child
            .stdin
            .take()
            .context("gh stdin unavailable")?
            .write_all(input.as_bytes())
            .await
            .context("writing gh stdin")?;
        let out = child.wait_with_output().await.context("waiting for gh")?;
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
            is_draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
            labels: Self::labels_from_json(v),
        })
    }

    /// Issues and PRs share GitHub's number space and `gh issue view`
    /// resolves both, reporting `MERGED` for a merged PR. Merged means the
    /// lifecycle is over, so it maps to Closed like `closed` does. Anything
    /// unrecognized is an error, never a silent Open — the reaper must land
    /// on StateUnknown (skip), not keep a dead worktree alive forever.
    fn parse_issue_state(state: &str) -> Result<IssueState> {
        match state.to_ascii_lowercase().as_str() {
            "closed" | "merged" => Ok(IssueState::Closed),
            "open" => Ok(IssueState::Open),
            other => bail!("unrecognized issue state `{other}`"),
        }
    }

    /// A GitHub Actions check run: `status` says whether it finished,
    /// `conclusion` how. Anything not completed is Pending; a completed run
    /// passes only on SUCCESS/NEUTRAL/SKIPPED — CANCELLED, TIMED_OUT,
    /// ACTION_REQUIRED, STALE and friends block the merge just like FAILURE,
    /// so they count as Failure.
    fn check_state_from_check_run(status: &str, conclusion: &str) -> CheckState {
        if !status.eq_ignore_ascii_case("completed") {
            return CheckState::Pending;
        }
        match conclusion.to_ascii_uppercase().as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" => CheckState::Success,
            _ => CheckState::Failure,
        }
    }

    /// A classic commit status: SUCCESS/PENDING/EXPECTED/ERROR/FAILURE.
    fn check_state_from_status_context(state: &str) -> CheckState {
        match state.to_ascii_uppercase().as_str() {
            "SUCCESS" => CheckState::Success,
            "PENDING" | "EXPECTED" => CheckState::Pending,
            _ => CheckState::Failure,
        }
    }

    /// The rollup's context nodes (CheckRun | StatusContext) as [`CheckRun`]s.
    fn checks_from_rollup_nodes(nodes: &[Value]) -> Vec<CheckRun> {
        nodes
            .iter()
            .filter_map(|n| {
                let str_of = |key: &str| {
                    n.get(key)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                match n.get("__typename").and_then(Value::as_str)? {
                    "CheckRun" => Some(CheckRun {
                        name: str_of("name"),
                        state: Self::check_state_from_check_run(
                            &str_of("status"),
                            &str_of("conclusion"),
                        ),
                        url: str_of("detailsUrl"),
                    }),
                    "StatusContext" => Some(CheckRun {
                        name: str_of("context"),
                        state: Self::check_state_from_status_context(&str_of("state")),
                        url: str_of("targetUrl"),
                    }),
                    _ => None,
                }
            })
            .collect()
    }

    /// The workflow run id inside a check's details URL
    /// (`.../actions/runs/<id>/job/<job>`), if it points at GitHub Actions.
    fn actions_run_id(url: &str) -> Option<String> {
        let (_, rest) = url.split_once("/actions/runs/")?;
        let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
        (!id.is_empty()).then_some(id)
    }

    fn tail_lines(s: &str, n: usize) -> String {
        let lines: Vec<&str> = s.lines().collect();
        let skipped = lines.len().saturating_sub(n);
        let tail = lines[skipped..].join("\n");
        if skipped > 0 {
            format!("[... {skipped} earlier lines omitted ...]\n{tail}")
        } else {
            tail
        }
    }

    /// --edit doesn't create missing labels — ensure it exists first
    /// (idempotent; ignore "already exists" failures). Known meguri labels are
    /// created with their scheme color (ADR 0005: the label color carries the
    /// two-axis meaning), so a fresh repository gets the right palette without
    /// any manual step; unknown labels fall back to the generic blue. Existing
    /// labels are never recolored here — that is a one-time ops step
    /// (`gh label edit <name> --color <hex>`), documented in the README, so
    /// meguri does not keep overwriting a color a human deliberately set.
    async fn ensure_label(&self, label: &str) {
        let (color, description) = label_scheme(label);
        let _ = self
            .gh(&[
                "label",
                "create",
                label,
                "--repo",
                &self.repo,
                "--color",
                color,
                "--description",
                description,
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
        Self::parse_issue_state(state)
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

    /// GitHub-native issue dependencies. Missing fields degrade to an
    /// unresolved blocker (never to resolved), matching the gate's
    /// "unreadable means unresolved" rule.
    async fn blocked_by(&self, issue: i64) -> Result<Vec<Blocker>> {
        let raw = self
            .gh(&[
                "api",
                &format!(
                    "repos/{}/issues/{issue}/dependencies/blocked_by?per_page=100",
                    self.repo
                ),
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing blocked_by output")?;
        Ok(v.as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|b| Blocker {
                        number: b.get("number").and_then(Value::as_i64).unwrap_or(0),
                        state: b
                            .get("state")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_lowercase(),
                        state_reason: b
                            .get("state_reason")
                            .and_then(Value::as_str)
                            .map(str::to_lowercase),
                    })
                    .collect()
            })
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
        let out = self.gh(&args).await?;
        // gh prints the created issue's URL (possibly after other lines).
        let url = out
            .lines()
            .rev()
            .find(|l| l.starts_with("https://"))
            .unwrap_or(&out)
            .trim();
        url.rsplit('/')
            .next()
            .and_then(|n| n.parse::<i64>().ok())
            .with_context(|| format!("no issue number in gh issue create output: {out}"))
    }

    async fn add_blocked_by(&self, issue: i64, blocker: i64) -> Result<()> {
        let repo = self.repo.clone();
        self.add_blocked_by_in(issue, &repo, blocker).await
    }

    /// The dependencies endpoint wants the blocking issue's database id, not
    /// its number — resolve it from the blocker's own repo (which may be a
    /// workspace sibling, issue #154). The `issue_id` is unique across GitHub,
    /// so once resolved the POST targets this forge's repo unchanged.
    async fn add_blocked_by_in(&self, issue: i64, blocker_repo: &str, blocker: i64) -> Result<()> {
        let raw = self
            .gh(&["api", &format!("repos/{blocker_repo}/issues/{blocker}")])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh issue output")?;
        let id = v
            .get("id")
            .and_then(Value::as_i64)
            .with_context(|| format!("issue {blocker_repo}#{blocker} has no id: {raw}"))?;
        self.gh(&[
            "api",
            "-X",
            "POST",
            &format!("repos/{}/issues/{issue}/dependencies/blocked_by", self.repo),
            "-F",
            &format!("issue_id={id}"),
        ])
        .await?;
        Ok(())
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

    async fn update_pr_title(&self, pr: i64, title: &str) -> Result<()> {
        self.gh(&[
            "pr",
            "edit",
            &pr.to_string(),
            "--repo",
            &self.repo,
            "--title",
            title,
        ])
        .await?;
        Ok(())
    }

    async fn update_pr_body(&self, pr: i64, body: &str) -> Result<()> {
        self.gh(&[
            "pr",
            "edit",
            &pr.to_string(),
            "--repo",
            &self.repo,
            "--body",
            body,
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
                "number,title,body,labels,headRefName,headRefOid,state,url,isDraft",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh pr view output")?;
        Self::pr_from_json(&v).with_context(|| format!("unexpected PR shape: {raw}"))
    }

    /// `gh pr view` resolves a branch name to its PR (preferring an open one
    /// when several exist, which is the safe direction: open means keep).
    /// "No PR" is a normal answer, not an error — only real lookup failures
    /// (network, auth) propagate so the caller can fall back to keeping.
    async fn pr_for_branch(&self, branch: &str) -> Result<Option<PullRequest>> {
        let raw = match self
            .gh(&[
                "pr",
                "view",
                branch,
                "--repo",
                &self.repo,
                "--json",
                "number,title,body,labels,headRefName,headRefOid,state,url",
            ])
            .await
        {
            Ok(raw) => raw,
            Err(e) if e.to_string().contains("no pull requests found") => return Ok(None),
            Err(e) => return Err(e),
        };
        let v: Value = serde_json::from_str(&raw).context("parsing gh pr view output")?;
        Ok(Some(
            Self::pr_from_json(&v).with_context(|| format!("unexpected PR shape: {raw}"))?,
        ))
    }

    /// GitHub computes mergeability lazily; `mergeable` is "MERGEABLE",
    /// "CONFLICTING" or "UNKNOWN" (still computing). `mergeStateStatus` is
    /// requested too so a future caller can distinguish e.g. blocked-but-
    /// mergeable, but only the conflict axis matters here.
    async fn pr_mergeable(&self, number: i64) -> Result<MergeableState> {
        let raw = self
            .gh(&[
                "pr",
                "view",
                &number.to_string(),
                "--repo",
                &self.repo,
                "--json",
                "mergeable,mergeStateStatus",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh pr view mergeable")?;
        Ok(
            match v.get("mergeable").and_then(Value::as_str).unwrap_or("") {
                s if s.eq_ignore_ascii_case("mergeable") => MergeableState::Mergeable,
                s if s.eq_ignore_ascii_case("conflicting") => MergeableState::Conflicting,
                _ => MergeableState::Unknown,
            },
        )
    }

    /// One `gh pr view` folding the three signals merge-watch classifies on:
    /// mergeability, the `mergeStateStatus` verdict, and whether auto-merge is
    /// armed (`autoMergeRequest` is null when it is not). A `gh` failure
    /// propagates as `Err`, which merge-watch reads as TransientError (no
    /// escalation — ADR 0007).
    async fn pr_merge_state(&self, number: i64) -> Result<MergeState> {
        let raw = self
            .gh(&[
                "pr",
                "view",
                &number.to_string(),
                "--repo",
                &self.repo,
                "--json",
                "mergeable,mergeStateStatus,autoMergeRequest",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh pr view merge-state")?;
        let mergeable = match v.get("mergeable").and_then(Value::as_str).unwrap_or("") {
            s if s.eq_ignore_ascii_case("mergeable") => MergeableState::Mergeable,
            s if s.eq_ignore_ascii_case("conflicting") => MergeableState::Conflicting,
            _ => MergeableState::Unknown,
        };
        let status = MergeStateStatus::from_gh(
            v.get("mergeStateStatus")
                .and_then(Value::as_str)
                .unwrap_or(""),
        );
        // `autoMergeRequest` is a non-null object while armed, null once a
        // human (or a merge) clears it.
        let auto_merge_enabled = v.get("autoMergeRequest").is_some_and(|a| !a.is_null());
        Ok(MergeState {
            mergeable,
            status,
            auto_merge_enabled,
        })
    }

    /// Checks and classic commit statuses both live in GraphQL's
    /// `statusCheckRollup` contexts; `gh pr checks` is avoided because it
    /// exits non-zero on pending/failing checks (indistinguishable from a
    /// real gh failure). The aggregate verdict is computed locally by
    /// [`CheckRollup::state`], not taken from GitHub's rollup `state`, so
    /// "one check failed, others still running" stays Pending.
    async fn pr_check_rollup(&self, number: i64) -> Result<CheckRollup> {
        let (owner, name) = self
            .repo
            .split_once('/')
            .with_context(|| format!("repo slug `{}` is not owner/name", self.repo))?;
        let query = "query($owner:String!,$name:String!,$number:Int!){\
             repository(owner:$owner,name:$name){pullRequest(number:$number){\
             commits(last:1){nodes{commit{statusCheckRollup{\
             contexts(first:100){nodes{__typename \
             ... on CheckRun{name status conclusion detailsUrl} \
             ... on StatusContext{context state targetUrl}}}}}}}}}}";
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
                &format!("number={number}"),
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing check-rollup GraphQL")?;
        // A null rollup means no CI ever ran on this head: an empty rollup.
        let nodes = v
            .pointer(
                "/data/repository/pullRequest/commits/nodes/0/commit\
                 /statusCheckRollup/contexts/nodes",
            )
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(CheckRollup {
            checks: Self::checks_from_rollup_nodes(&nodes),
        })
    }

    /// One section per failing workflow run (`gh run view --log-failed`),
    /// deduped when several failed checks belong to the same run. Failures
    /// from external CI (plain commit statuses) have no fetchable log — they
    /// contribute a pointer to their details URL instead.
    async fn pr_failed_check_logs(&self, number: i64) -> Result<String> {
        let rollup = self.pr_check_rollup(number).await?;
        let mut sections = Vec::new();
        let mut seen_runs = HashSet::new();
        for check in rollup.failed() {
            match Self::actions_run_id(&check.url) {
                Some(run_id) => {
                    if !seen_runs.insert(run_id.clone()) {
                        continue;
                    }
                    let section = match self
                        .gh(&["run", "view", &run_id, "--repo", &self.repo, "--log-failed"])
                        .await
                    {
                        Ok(log) => format!(
                            "### {} (workflow run {run_id})\n```\n{}\n```",
                            check.name,
                            Self::tail_lines(&log, FAILED_LOG_TAIL_LINES),
                        ),
                        Err(e) => format!(
                            "### {} (workflow run {run_id})\n(log fetch failed: {e:#})",
                            check.name
                        ),
                    };
                    sections.push(section);
                }
                None => sections.push(format!(
                    "### {}\n(no workflow-run log on GitHub; details: {})",
                    check.name,
                    if check.url.is_empty() {
                        "none"
                    } else {
                        &check.url
                    }
                )),
            }
        }
        Ok(sections.join("\n\n"))
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
                "number,title,body,labels,headRefName,headRefOid,state,url,isDraft",
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
        Ok(self
            .pr_comments_meta(number)
            .await?
            .into_iter()
            .map(|c| c.body)
            .collect())
    }

    async fn pr_comments_meta(&self, number: i64) -> Result<Vec<PrComment>> {
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
                    .filter_map(|c| {
                        let body = c.get("body").and_then(Value::as_str)?.to_string();
                        let created_at = c
                            .get("createdAt")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        Some(PrComment { body, created_at })
                    })
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
                "number,title,body,url,headRefName,headRefOid,state,labels,isDraft",
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

    async fn create_pr_review(
        &self,
        pr: i64,
        body: &str,
        comments: &[ReviewCommentDraft],
    ) -> Result<()> {
        let payload = serde_json::json!({
            "event": "COMMENT",
            "body": body,
            "comments": comments
                .iter()
                .map(|c| serde_json::json!({
                    "path": c.path,
                    "line": c.line,
                    "side": "RIGHT",
                    "body": c.body,
                }))
                .collect::<Vec<_>>(),
        });
        self.gh_stdin(
            &[
                "api",
                &format!("repos/{}/pulls/{pr}/reviews", self.repo),
                "--method",
                "POST",
                "--input",
                "-",
            ],
            &payload.to_string(),
        )
        .await?;
        Ok(())
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

    async fn enable_auto_merge(
        &self,
        pr: i64,
        strategy: MergeStrategy,
        head_sha: &str,
    ) -> Result<ArmOutcome> {
        let pr = pr.to_string();
        let args = Self::merge_args(&pr, &self.repo, strategy, head_sha, true);
        match self.gh_try(&args).await? {
            Ok(_) => Ok(ArmOutcome::Armed),
            Err(stderr) => match Self::classify_arm_stderr(&stderr) {
                Some(outcome) => Ok(outcome),
                // A moved head (or any other failure) is returned as-is; the
                // sweep warns and re-evaluates the new head next poll.
                None => bail!("gh pr merge --auto failed for #{pr}: {stderr}"),
            },
        }
    }

    async fn merge_pr(&self, pr: i64, strategy: MergeStrategy, head_sha: &str) -> Result<()> {
        let pr = pr.to_string();
        let args = Self::merge_args(&pr, &self.repo, strategy, head_sha, false);
        self.gh(&args).await?;
        Ok(())
    }

    async fn mark_pr_ready(&self, pr: i64) -> Result<()> {
        self.gh(&["pr", "ready", &pr.to_string(), "--repo", &self.repo])
            .await?;
        Ok(())
    }

    async fn merge_policy(
        &self,
        base_branch: &str,
        require_branch_protection: bool,
    ) -> Result<MergePolicy> {
        let raw = self.gh(&["api", &format!("repos/{}", self.repo)]).await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh api repos output")?;
        let flag = |key: &str| v.get(key).and_then(Value::as_bool).unwrap_or(false);
        let mut allowed_strategies = Vec::new();
        if flag("allow_squash_merge") {
            allowed_strategies.push(MergeStrategy::Squash);
        }
        if flag("allow_merge_commit") {
            allowed_strategies.push(MergeStrategy::Merge);
        }
        if flag("allow_rebase_merge") {
            allowed_strategies.push(MergeStrategy::Rebase);
        }

        // The protection probe needs an admin-scoped token and 403s without
        // one. It is the escape hatch's whole point that `require_branch_
        // protection = false` skips it — otherwise the 403 would bail here and
        // fail `meguri watch` / `doctor` before `validate_policy` (which
        // ignores protection when not required) ever runs. So only probe when
        // protection is actually required. Classic branch protection only:
        // 200 = required checks present, 404 = no protection, 403 = admin
        // required (ADR 0003 — never silently "unprotected").
        let protected_with_required_checks = if require_branch_protection {
            match self
                .gh_try(&[
                    "api",
                    &format!(
                        "repos/{}/branches/{base_branch}/protection/required_status_checks",
                        self.repo
                    ),
                ])
                .await?
            {
                Ok(_) => true,
                Err(stderr) => self.protection_from_stderr(base_branch, &stderr)?,
            }
        } else {
            false
        };

        Ok(MergePolicy {
            auto_merge_allowed: flag("allow_auto_merge"),
            allowed_strategies,
            protected_with_required_checks,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_args_pin_head_and_toggle_auto() {
        let armed = GhForge::merge_args("7", "me/repo", MergeStrategy::Squash, "abc", true);
        assert_eq!(
            armed,
            vec![
                "pr",
                "merge",
                "7",
                "--repo",
                "me/repo",
                "--auto",
                "--squash",
                "--match-head-commit",
                "abc",
            ]
        );
        // The clean-status finalize drops --auto but keeps the head pin.
        let finalize = GhForge::merge_args("7", "me/repo", MergeStrategy::Rebase, "abc", false);
        assert_eq!(
            finalize,
            vec![
                "pr",
                "merge",
                "7",
                "--repo",
                "me/repo",
                "--rebase",
                "--match-head-commit",
                "abc",
            ]
        );
    }

    #[test]
    fn arm_stderr_maps_idempotent_and_clean() {
        assert_eq!(
            GhForge::classify_arm_stderr("Pull request is in clean status"),
            Some(ArmOutcome::AlreadyClean)
        );
        assert_eq!(
            GhForge::classify_arm_stderr("auto-merge is already enabled for this PR"),
            Some(ArmOutcome::Armed)
        );
        // A moved head is a genuine failure the caller must surface.
        assert_eq!(
            GhForge::classify_arm_stderr(
                "Head branch was modified. Review and try the merge again."
            ),
            None
        );
    }

    #[test]
    fn protection_stderr_maps_status_codes() {
        let forge = GhForge::new("me/repo");
        assert!(
            !forge
                .protection_from_stderr("main", "gh: Not Found (HTTP 404)")
                .unwrap()
        );
        let admin = forge
            .protection_from_stderr("main", "gh: Must have admin rights (HTTP 403)")
            .unwrap_err()
            .to_string();
        assert!(admin.contains("admin"), "{admin}");
        assert!(
            admin.contains("require_branch_protection = false"),
            "{admin}"
        );
        // An unexpected error is neither "unprotected" nor swallowed.
        assert!(
            forge
                .protection_from_stderr("main", "gh: boom (HTTP 500)")
                .is_err()
        );
    }

    #[test]
    fn phase_labels_carry_their_scheme_colors() {
        // The color encodes the two-axis meaning (ADR 0005), so lock it here.
        assert_eq!(label_scheme(super::super::LABEL_SPECCING).0, "6F42C1");
        assert_eq!(label_scheme(super::super::LABEL_IMPLEMENTING).0, "0E8A16");
        assert_eq!(label_scheme(super::super::LABEL_READY).0, "1D76DB");
        assert_eq!(label_scheme(super::super::LABEL_PLAN).0, "1D76DB");
        assert_eq!(label_scheme(super::super::LABEL_WORKING).0, "FBCA04");
        assert_eq!(label_scheme(super::super::LABEL_NEEDS_HUMAN).0, "B60205");
        assert_eq!(label_scheme(super::super::LABEL_HOLD).0, "CFD3D7");
        // An unknown label falls back to the generic blue.
        assert_eq!(
            label_scheme("random:label"),
            (DEFAULT_LABEL_COLOR, "managed by meguri")
        );
    }

    #[test]
    fn merged_pr_state_is_closed() {
        // gh reports a merged PR's state as MERGED through the issue view.
        assert_eq!(
            GhForge::parse_issue_state("MERGED").unwrap(),
            IssueState::Closed
        );
        assert_eq!(
            GhForge::parse_issue_state("merged").unwrap(),
            IssueState::Closed
        );
    }

    #[test]
    fn open_and_closed_states_parse_case_insensitively() {
        assert_eq!(
            GhForge::parse_issue_state("OPEN").unwrap(),
            IssueState::Open
        );
        assert_eq!(
            GhForge::parse_issue_state("open").unwrap(),
            IssueState::Open
        );
        assert_eq!(
            GhForge::parse_issue_state("CLOSED").unwrap(),
            IssueState::Closed
        );
        assert_eq!(
            GhForge::parse_issue_state("closed").unwrap(),
            IssueState::Closed
        );
    }

    #[test]
    fn unknown_state_is_an_error_not_open() {
        // Unknown must surface as Err (reaper: StateUnknown), never as a
        // silent Open that pins the worktree forever.
        assert!(GhForge::parse_issue_state("DRAFT").is_err());
        assert!(GhForge::parse_issue_state("").is_err());
    }

    #[test]
    fn check_run_states_map_to_the_three_way_verdict() {
        use CheckState::*;
        // Not completed yet: pending regardless of any (stale) conclusion.
        assert_eq!(
            GhForge::check_state_from_check_run("IN_PROGRESS", ""),
            Pending
        );
        assert_eq!(GhForge::check_state_from_check_run("QUEUED", ""), Pending);
        // Completed: only a pass-shaped conclusion is green.
        assert_eq!(
            GhForge::check_state_from_check_run("COMPLETED", "SUCCESS"),
            Success
        );
        assert_eq!(
            GhForge::check_state_from_check_run("COMPLETED", "NEUTRAL"),
            Success
        );
        assert_eq!(
            GhForge::check_state_from_check_run("COMPLETED", "SKIPPED"),
            Success
        );
        for red in [
            "FAILURE",
            "CANCELLED",
            "TIMED_OUT",
            "ACTION_REQUIRED",
            "STALE",
        ] {
            assert_eq!(
                GhForge::check_state_from_check_run("COMPLETED", red),
                Failure,
                "{red} must count as a failure"
            );
        }
    }

    #[test]
    fn status_context_states_map_to_the_three_way_verdict() {
        use CheckState::*;
        assert_eq!(GhForge::check_state_from_status_context("SUCCESS"), Success);
        assert_eq!(GhForge::check_state_from_status_context("PENDING"), Pending);
        assert_eq!(
            GhForge::check_state_from_status_context("EXPECTED"),
            Pending
        );
        assert_eq!(GhForge::check_state_from_status_context("FAILURE"), Failure);
        assert_eq!(GhForge::check_state_from_status_context("ERROR"), Failure);
    }

    #[test]
    fn rollup_nodes_parse_check_runs_and_status_contexts() {
        let nodes: Vec<Value> = serde_json::from_str(
            r#"[
              {"__typename":"CheckRun","name":"test","status":"COMPLETED",
               "conclusion":"FAILURE",
               "detailsUrl":"https://github.com/me/proj/actions/runs/42/job/7"},
              {"__typename":"StatusContext","context":"external-ci",
               "state":"SUCCESS","targetUrl":"https://ci.example/x"},
              {"__typename":"SomethingElse","name":"ignored"}
            ]"#,
        )
        .unwrap();
        let checks = GhForge::checks_from_rollup_nodes(&nodes);
        assert_eq!(checks.len(), 2);
        assert_eq!(checks[0].name, "test");
        assert_eq!(checks[0].state, CheckState::Failure);
        assert!(checks[0].url.contains("/actions/runs/42/"));
        assert_eq!(checks[1].name, "external-ci");
        assert_eq!(checks[1].state, CheckState::Success);
    }

    #[test]
    fn actions_run_id_only_parses_actions_urls() {
        assert_eq!(
            GhForge::actions_run_id("https://github.com/me/proj/actions/runs/42/job/7").as_deref(),
            Some("42")
        );
        assert_eq!(
            GhForge::actions_run_id("https://github.com/me/proj/actions/runs/42").as_deref(),
            Some("42")
        );
        assert_eq!(GhForge::actions_run_id("https://ci.example/build/42"), None);
        assert_eq!(GhForge::actions_run_id(""), None);
    }

    #[test]
    fn tail_lines_keeps_the_tail_and_marks_the_cut() {
        assert_eq!(GhForge::tail_lines("a\nb", 5), "a\nb");
        let tailed = GhForge::tail_lines("a\nb\nc\nd", 2);
        assert!(tailed.starts_with("[... 2 earlier lines omitted ...]"));
        assert!(tailed.ends_with("c\nd"));
    }
}
