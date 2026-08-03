//! GitHub gateway backed by the `gh` CLI (reuses the user's existing auth,
//! same approach as looper).

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;

use super::{Blocker, CreatedPr, Forge, Issue, IssueState, PullRequest};

/// The `gh` binary itself could not be started (missing, not executable, a
/// bad PATH, ...) — as opposed to `gh` running and exiting non-zero. Kept
/// distinct from a bare `std::io::Error` so `run_flow` can classify only
/// this specific boundary as a retryable infra fault (issue #250 f1):
/// every other `io::Error` in the codebase (git, direct-mode agent spawn,
/// prompt/log file writes) must keep escalating to needs-human as before.
#[derive(Debug, thiserror::Error)]
#[error("spawning gh (is the GitHub CLI installed?): {0}")]
pub struct GhSpawnFailed(#[from] std::io::Error);

/// The generic color for a meguri label with no scheme entry.
const DEFAULT_LABEL_COLOR: &str = "1D76DB";

/// The linked-PR cross-reference query (issue #249, [`Forge::linked_open_prs`]):
/// GitHub's issue timeline, filtered to `CrossReferencedEvent`s whose source
/// is a PR. Kept as a const for the same reason as
/// [`MERGE_TAIL_OBSERVE_QUERY`]: FakeForge tests never execute this string,
/// so a parse-level brace-balance check is the only thing that would catch a
/// syntax slip before production.
const LINKED_OPEN_PRS_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){\
     repository(owner:$owner,name:$name){issue(number:$number){\
     timelineItems(first:100,itemTypes:[CROSS_REFERENCED_EVENT]){\
     nodes{... on CrossReferencedEvent{source{... on PullRequest{\
     number title body url headRefName headRefOid state isDraft \
     labels(first:20){nodes{name}}}}}}}}}}";

/// Scheme color (hex, no `#`) and description for a known meguri label — the
/// color encodes the two-axis model (ADR 0005): phase labels by stage
/// (plan/ready = blue, speccing = purple, implementing = green) and ball
/// labels by who holds it (working = yellow, needs-human = red, hold = grey).
/// Unknown labels fall back to [`DEFAULT_LABEL_COLOR`].
fn label_scheme(label: &str) -> (&'static str, &'static str) {
    use super::*;
    match label {
        // Axis 1 — phase.
        LABEL_READY => ("1D76DB", "meguri phase: awaiting implementation"),
        LABEL_IMPLEMENTING => ("0E8A16", "meguri phase: implementation PR open"),
        // Axis 2 — ball / who holds it.
        LABEL_WORKING => ("FBCA04", "meguri: an agent is working on it"),
        LABEL_NEEDS_HUMAN => ("B60205", "meguri: a human needs to look (see comment)"),
        LABEL_HOLD => ("CFD3D7", "meguri: intentionally paused by a human"),
        _ => (DEFAULT_LABEL_COLOR, "managed by meguri"),
    }
}

/// Create a GitHub repository from scratch, initial commit included
/// (`--add-readme`), so it has a default branch the moment it exists — a
/// commit-0 repo has no default branch and breaks `worktree add` / the PR base
/// (issue #196, ADR 0019). The one place meguri shells out to `gh repo create`.
///
/// A free function, not a [`Forge`] method: `GhForge` is built per existing slug
/// and all its methods operate on a repo that already exists, whereas creation
/// runs before any such repo — the same shape as [`crate::gitops::ensure_bare_clone`]
/// being a free function. **Irreversible**: meguri never deletes a repo it
/// created, so the caller (not this function) owns recovery on later failure.
pub async fn create_repo(slug: &str, public: bool) -> Result<()> {
    let visibility = if public { "--public" } else { "--private" };
    let out = tokio::process::Command::new("gh")
        .args(["repo", "create", slug, visibility, "--add-readme"])
        .output()
        .await
        .map_err(GhSpawnFailed)?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "gh repo create {slug} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
}

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
            .map_err(GhSpawnFailed)?;
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

    /// Like [`Self::pr_from_json`], but for a raw GraphQL PR node (as
    /// opposed to `gh`'s REST-shaped `--json` output): `state` is
    /// GraphQL's uppercase enum and `labels` is a `{nodes:[...]}`
    /// connection rather than a flat array. An empty `source` object (a
    /// cross-reference from something other than a PR, or a PR meguri's
    /// token cannot read) yields `None`, silently dropped by the caller.
    fn pr_from_cross_reference_json(v: &Value) -> Option<PullRequest> {
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
                .unwrap_or("OPEN")
                .to_lowercase(),
            labels: v
                .pointer("/labels/nodes")
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

    /// Open PRs the forge's timeline cross-references to `issue` (GitHub's
    /// "Development" linkage: any PR whose body/comment mentions `#issue`,
    /// closing-keyword or not). One page of 100 is generous for this —
    /// the worker calls it once right before opening a PR, never in a
    /// hot loop, so the bounded-window idioms `observe_open_prs` needs
    /// (incomplete-tracking, pagination) would be overkill here.
    async fn linked_open_prs(&self, issue: i64) -> Result<Vec<PullRequest>> {
        let (owner, name) = self
            .repo
            .split_once('/')
            .with_context(|| format!("repo slug `{}` is not owner/name", self.repo))?;
        let raw = self
            .gh(&[
                "api",
                "graphql",
                "-f",
                &format!("query={LINKED_OPEN_PRS_QUERY}"),
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={name}"),
                "-F",
                &format!("number={issue}"),
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing linked-PRs GraphQL")?;
        let nodes = v
            .pointer("/data/repository/issue/timelineItems/nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(nodes
            .iter()
            .filter_map(|n| n.pointer("/source"))
            .filter_map(Self::pr_from_cross_reference_json)
            .filter(|pr| pr.state == "open")
            .collect())
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
        labels: &[&str],
    ) -> Result<CreatedPr> {
        // `gh pr create --label` fails on labels that don't exist yet — create
        // them first, same as `create_issue`. Applying labels here (not in a
        // follow-up `add_pr_label`) keeps the PR from ever being observed
        // unlabeled (issue #209).
        for label in labels {
            self.ensure_label(label).await;
        }
        let mut args = vec![
            "pr", "create", "--repo", &self.repo, "--head", head, "--base", base, "--title", title,
            "--body", body,
        ];
        if draft {
            args.push("--draft");
        }
        for label in labels {
            args.push("--label");
            args.push(label);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    // FakeForge tests never execute these hand-written GraphQL strings, so a
    // syntax slip in one of them (an unbalanced brace killed every merge-tail
    // sweep in production on 2026-07-21, #227) only surfaces via this
    // parse-level check — hence every literal query *and mutation* is a
    // module-level const covered here, not just the one #242 fixed (issue
    // #251, design doc P6.5 item 2).
    fn assert_braces_balance(name: &str, query: &str) {
        let mut depth = 0i64;
        for (i, c) in query.chars().enumerate() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    assert!(depth >= 0, "{name}: extra closing brace at index {i}");
                }
                _ => {}
            }
        }
        assert_eq!(depth, 0, "{name}: {depth} unclosed brace(s)");
    }

    #[test]
    fn linked_open_prs_query_braces_balance() {
        assert_braces_balance("LINKED_OPEN_PRS_QUERY", LINKED_OPEN_PRS_QUERY);
    }

    #[test]
    fn phase_labels_carry_their_scheme_colors() {
        // The color encodes the two-axis meaning (ADR 0005), so lock it here.
        assert_eq!(label_scheme(super::super::LABEL_IMPLEMENTING).0, "0E8A16");
        assert_eq!(label_scheme(super::super::LABEL_READY).0, "1D76DB");
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
}
