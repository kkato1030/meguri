//! GitHub gateway backed by the `gh` CLI (reuses the user's existing auth,
//! same approach as looper).

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde_json::Value;

use super::{
    ArmOutcome, Blocker, CheckRollup, CheckRun, CheckState, CommitStatusState, CreatedPr, Forge,
    Issue, IssueState, MergePolicy, MergeState, MergeStateStatus, MergeStrategy,
    MergeTailObservation, MergeableState, ObserveCost, PrComment, PrObservation, PullRequest,
    ReviewComment, ReviewCommentDraft, ReviewThread, UpdateBranchOutcome,
};

/// How much of each failed job log survives into the fix prompt (logs can be
/// megabytes; the failure is almost always at the tail).
const FAILED_LOG_TAIL_LINES: usize = 200;

/// The generic color for a meguri label with no scheme entry.
const DEFAULT_LABEL_COLOR: &str = "1D76DB";

/// Page budget for the per-PR comment overflow pagination (100 comments per
/// page, on top of the bulk window's last-100). Anyone who can comment on a
/// public PR controls the conversation's length, so an unbounded walk would
/// let one chatty PR spend arbitrary API cost on every resync; past this
/// budget the observation is marked incomplete and the engine parks the PR
/// (safe side) instead of re-paginating forever.
const MAX_COMMENT_PAGES: u32 = 10;

/// How long a truncated comment pagination stays parked before it may be
/// retried with an unchanged `totalCount` (a changed count retries at once).
/// Bounds the worst case to one page budget per PR per window instead of one
/// per resync.
const COMMENT_PAGINATION_COOLDOWN: Duration = Duration::from_secs(30 * 60);

/// The merge-tail informer-cache observe (issue #221, ADR 0012 decision 3):
/// one GraphQL query folding every per-PR signal the old sweeps read (merge
/// state, arm-marker comments, review threads, check rollup) into a single
/// round-trip. `comments(last:100)` covers all but the chattiest PRs in one
/// shot; `totalCount` detects when the window clipped older comments so the
/// arm marker (the durable idempotency / human-override key) is never missed —
/// a clipped marker would let a human-disarmed head look unarmed and get
/// wrongly re-armed (f1). Kept as a const so a unit test can parse-check it:
/// FakeForge tests never execute this string, so an unbalanced brace here
/// reaches production silently and kills every merge-tail sweep.
const MERGE_TAIL_OBSERVE_QUERY: &str = "query($owner:String!,$name:String!){\
     rateLimit{cost}\
     repository(owner:$owner,name:$name){pullRequests(first:50,states:OPEN){nodes{\
     number title body url headRefName headRefOid isDraft state \
     labels(first:100){totalCount nodes{name}} mergeable mergeStateStatus \
     autoMergeRequest{enabledAt} \
     comments(last:100){totalCount pageInfo{startCursor} nodes{id body createdAt viewerDidAuthor}} \
     reviewThreads(first:100){totalCount nodes{isResolved comments(last:1){nodes{author{login} body}}}} \
     commits(last:1){nodes{commit{statusCheckRollup{contexts(first:100){nodes{__typename \
     ... on CheckRun{name status conclusion detailsUrl} \
     ... on StatusContext{context state targetUrl}}}}}}}}}}}";

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

/// Extract an `owner/repo` slug from a GitHub API `repository_url`
/// (`https://api.github.com/repos/owner/repo`). Used to tag each blocker with
/// its home repo for cross-repo decomposition (issue #134).
fn slug_from_repository_url(url: &str) -> Option<String> {
    url.split("/repos/")
        .nth(1)
        .map(|s| s.trim_end_matches('/').to_string())
}

/// Whether a `gh api` dependency POST failed only because the edge already
/// exists — the idempotent no-op case for [`GhForge::add_blocked_by_in`].
fn is_dependency_exists(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("already") && (lower.contains("depend") || lower.contains("block"))
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
        .context("spawning gh (is the GitHub CLI installed?)")?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "gh repo create {slug} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
}

/// One GraphQL comment node → [`PrComment`], preserving the node `id` and
/// `viewerDidAuthor` (the claim marker's authenticity + tombstone-edit key, §7).
fn comment_from_node(c: &Value) -> Option<PrComment> {
    Some(PrComment {
        body: c.get("body").and_then(Value::as_str)?.to_string(),
        created_at: c
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        id: c
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        viewer_did_author: c
            .get("viewerDidAuthor")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Fold GraphQL comment *pages* (each a `comments` connection object,
/// `{ nodes: [...], pageInfo, totalCount }`) into a flat [`PrComment`] list,
/// carrying `id` + `viewerDidAuthor` on every page. This is the overflow path
/// for a >100-comment PR (f6): the REST fallback dropped `viewerDidAuthor`, so a
/// self-authored claim on a chatty PR was seen as a third party's and no-steal
/// was lost. Pure and page-shaped so it is unit-testable without a live `gh`
/// (f8).
pub fn fold_comment_pages(pages: &[Value]) -> Vec<PrComment> {
    pages
        .iter()
        .filter_map(|p| p.pointer("/nodes").and_then(Value::as_array))
        .flatten()
        .filter_map(comment_from_node)
        .collect()
}

pub struct GhForge {
    /// "owner/repo"
    repo: String,
    /// Cooldown for the comment overflow pagination, keyed by PR number. A PR
    /// whose conversation was truncated (page budget / stalled cursor) is not
    /// re-paginated until the cooldown lapses — without this, a 10k-comment PR
    /// would re-spend its whole page budget on every 30s resync, forever. The
    /// cooldown deliberately ignores `totalCount` movement: keying the retry
    /// on it would let anyone who can comment reset the cooldown every poll.
    comment_pagination_cooldown:
        std::sync::Mutex<std::collections::HashMap<i64, std::time::Instant>>,
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
            comment_pagination_cooldown: std::sync::Mutex::new(std::collections::HashMap::new()),
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

    /// Parses `gh api --paginate --slurp` output for the REST issues
    /// endpoint: an outer array whose elements are the per-page arrays.
    /// The endpoint returns PRs too (a PR is an issue there), so drop
    /// anything carrying a `pull_request` object; the remaining JSON shape
    /// (number/title/body, `labels[].name`) is exactly what
    /// `issue_from_json` already reads.
    fn open_issues_from_slurped_pages(raw: &str) -> Result<Vec<Issue>> {
        let pages: Value = serde_json::from_str(raw).context("parsing gh api issues output")?;
        Ok(pages
            .as_array()
            .map(|pages| {
                pages
                    .iter()
                    .filter_map(Value::as_array)
                    .flatten()
                    .filter(|it| it.get("pull_request").is_none())
                    .filter_map(Self::issue_from_json)
                    .collect()
            })
            .unwrap_or_default())
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
            is_draft: v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
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

    /// One PR node from the merge-tail bulk GraphQL into a [`PrObservation`]
    /// (issue #221). Reduces the same signals the two old sweeps read per PR;
    /// the pr-review status is pulled out of the rollup contexts by the caller's
    /// `pr_review_context`, and the raw comments travel on so the engine can
    /// extract the arm marker (an engine concept the forge stays free of).
    fn pr_observation_from_node(node: &Value, pr_review_context: &str) -> Option<PrObservation> {
        let str_of = |key: &str| {
            node.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let pr = PullRequest {
            number: node.get("number")?.as_i64()?,
            title: str_of("title"),
            body: str_of("body"),
            url: str_of("url"),
            head_branch: str_of("headRefName"),
            head_sha: str_of("headRefOid"),
            state: str_of("state").to_lowercase(),
            is_draft: node
                .get("isDraft")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            labels: node
                .pointer("/labels/nodes")
                .and_then(Value::as_array)
                .map(|ls| {
                    ls.iter()
                        .filter_map(|l| l.get("name").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        };
        let mergeable = match str_of("mergeable").to_ascii_uppercase().as_str() {
            "MERGEABLE" => MergeableState::Mergeable,
            "CONFLICTING" => MergeableState::Conflicting,
            _ => MergeableState::Unknown,
        };
        let merge = Some(MergeState {
            mergeable,
            status: MergeStateStatus::from_gh(&str_of("mergeStateStatus")),
            auto_merge_enabled: node.get("autoMergeRequest").is_some_and(|a| !a.is_null()),
        });
        let comments = node
            .pointer("/comments")
            .map(|conn| fold_comment_pages(std::slice::from_ref(conn)))
            .unwrap_or_default();
        let review_threads: Vec<ReviewThread> = node
            .pointer("/reviewThreads/nodes")
            .and_then(Value::as_array)
            .map(|ts| {
                ts.iter()
                    .map(|t| ReviewThread {
                        id: String::new(),
                        resolved: t
                            .get("isResolved")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        path: None,
                        line: None,
                        // Only the last comment is fetched (`comments(last:1)`) —
                        // enough for `thread_awaits_fixer`'s "ball in meguri's
                        // court" test (the Fixer arm's trigger, §1.5).
                        comments: t
                            .pointer("/comments/nodes")
                            .and_then(Value::as_array)
                            .map(|cs| {
                                cs.iter()
                                    .filter_map(|c| {
                                        Some(ReviewComment {
                                            author: c
                                                .pointer("/author/login")
                                                .and_then(Value::as_str)
                                                .unwrap_or_default()
                                                .to_string(),
                                            body: c
                                                .get("body")
                                                .and_then(Value::as_str)?
                                                .to_string(),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        // A clipped window makes the safety gates unreliable — a `hold` /
        // `needs-human` label or an unresolved thread hidden past it would be
        // missed. `totalCount` vs the returned count flags that so the engine
        // falls back conservatively (f1 sibling: labels / review threads).
        let complete = |field: &str, got: usize| {
            node.pointer(&format!("/{field}/totalCount"))
                .and_then(Value::as_u64)
                .is_none_or(|total| total as usize <= got)
        };
        let labels_complete = complete("labels", pr.labels.len());
        let review_threads_complete = complete("reviewThreads", review_threads.len());
        let rollup_nodes = node
            .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let checks = Self::checks_from_rollup_nodes(&rollup_nodes);
        // The pr-review status rides in the rollup as a StatusContext named by
        // the caller's context; map its CheckState back to a CommitStatusState.
        let pr_review = checks
            .iter()
            .find(|c| c.name == pr_review_context)
            .map(|c| match c.state {
                CheckState::Success => CommitStatusState::Success,
                CheckState::Failure => CommitStatusState::Failure,
                CheckState::Pending => CommitStatusState::Pending,
            });
        Some(PrObservation {
            pr,
            merge,
            comments,
            review_threads,
            rollup: CheckRollup { checks },
            pr_review,
            labels_complete,
            review_threads_complete,
            // The bulk window is complete unless the overflow pagination (the
            // caller) says otherwise.
            comments_complete: true,
        })
    }

    /// Whether the comment overflow pagination for `pr` is parked: it already
    /// truncated recently and the cooldown has not lapsed. Deliberately blind
    /// to conversation growth — a count-sensitive retry would hand anyone who
    /// can comment a lever to reset the cooldown on every poll.
    fn comment_pagination_parked(&self, pr: i64) -> bool {
        let map = self.comment_pagination_cooldown.lock().unwrap();
        map.get(&pr).is_some_and(|until| Instant::now() < *until)
    }

    /// Remember a truncated pagination (start the cooldown) or clear a
    /// completed one.
    fn record_pagination_outcome(&self, pr: i64, complete: bool) {
        let mut map = self.comment_pagination_cooldown.lock().unwrap();
        if complete {
            map.remove(&pr);
        } else {
            map.insert(pr, Instant::now() + COMMENT_PAGINATION_COOLDOWN);
        }
    }

    /// Every conversation comment on a PR, paginated over **GraphQL** so
    /// `id` + `viewerDidAuthor` survive on every page (f6). Used only when the
    /// bulk observe's comment window clipped older comments, so the arm marker
    /// and any self-authored claim marker are never missed. Returns the folded
    /// comments, the number of HTTP round-trips it took (for the cost), and
    /// whether the conversation was read to the end: the walk stops at
    /// [`MAX_COMMENT_PAGES`] or on a non-advancing cursor, because a
    /// pathologically chatty PR (anyone can comment on a public PR) must not
    /// be able to spend unbounded API cost on every resync — the caller marks
    /// the observation incomplete and the engine parks the PR instead.
    async fn paginate_pr_comments(&self, number: i64) -> Result<(Vec<PrComment>, u32, bool)> {
        let (owner, name) = self
            .repo
            .split_once('/')
            .with_context(|| format!("repo slug `{}` is not owner/name", self.repo))?;
        let query = "query($owner:String!,$name:String!,$number:Int!,$cursor:String){\
             repository(owner:$owner,name:$name){pullRequest(number:$number){\
             comments(first:100,after:$cursor){pageInfo{hasNextPage endCursor} \
             nodes{id body createdAt viewerDidAuthor}}}}}";
        let mut pages: Vec<Value> = Vec::new();
        let mut cursor: Option<String> = None;
        let mut requests: u32 = 0;
        let complete = loop {
            let mut args: Vec<String> = vec![
                "api".into(),
                "graphql".into(),
                "-f".into(),
                format!("query={query}"),
                "-f".into(),
                format!("owner={owner}"),
                "-f".into(),
                format!("name={name}"),
                "-F".into(),
                format!("number={number}"),
            ];
            if let Some(c) = &cursor {
                args.push("-f".into());
                args.push(format!("cursor={c}"));
            }
            let argv: Vec<&str> = args.iter().map(String::as_str).collect();
            let raw = self.gh(&argv).await?;
            requests += 1;
            let v: Value = serde_json::from_str(&raw).context("parsing paginated PR comments")?;
            let conn = v
                .pointer("/data/repository/pullRequest/comments")
                .cloned()
                .unwrap_or_default();
            let has_next = conn
                .pointer("/pageInfo/hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let next_cursor = conn
                .pointer("/pageInfo/endCursor")
                .and_then(Value::as_str)
                .map(str::to_string);
            let advanced = next_cursor.is_some() && next_cursor != cursor;
            cursor = next_cursor;
            pages.push(conn);
            if !has_next {
                break true;
            }
            // hasNextPage with a missing or stalled cursor: a malformed
            // response would otherwise re-read the same page forever.
            if !advanced || requests >= MAX_COMMENT_PAGES {
                break false;
            }
        };
        Ok((fold_comment_pages(&pages), requests, complete))
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

    async fn list_open_issues(&self) -> Result<Vec<Issue>> {
        // Triage's contract is to see *every* untriaged open issue, so this
        // must not truncate the way `gh issue list --limit N` does — an old,
        // low-numbered unlabeled issue past the cap would never be triaged, and
        // `max_open_issue` would read a subset. `gh api --paginate` follows the
        // Link headers, but it prints each page's JSON back to back — past 100
        // open issues that concatenation is no longer one valid JSON document.
        // `--slurp` wraps the pages into a single outer array instead, which
        // `open_issues_from_slurped_pages` flattens.
        let raw = self
            .gh(&[
                "api",
                "--paginate",
                "--slurp",
                &format!("repos/{}/issues?state=open&per_page=100", self.repo),
            ])
            .await?;
        Self::open_issues_from_slurped_pages(&raw)
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
                        // The dependency endpoint returns the whole blocker
                        // issue object, so its body and home repo come for free
                        // — no extra get_issue per blocker (issue #134). Missing
                        // fields degrade to empty (never matches a marker).
                        body: b
                            .get("body")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        repo: b
                            .get("repository_url")
                            .and_then(Value::as_str)
                            .and_then(slug_from_repository_url)
                            .unwrap_or_default(),
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

    async fn find_issue_by_marker(&self, marker: &str) -> Result<Option<i64>> {
        // All states: an already-created child may have been closed by a human
        // or a worker before we recorded it (issue #134). `--search "… in:body"`
        // scopes the term to issue bodies where the marker lives.
        let raw = self
            .gh(&[
                "issue",
                "list",
                "--repo",
                &self.repo,
                "--state",
                "all",
                "--search",
                &format!("{marker} in:body"),
                "--json",
                "number,body",
                "--limit",
                "50",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh issue list output")?;
        // GitHub search is token-based, so confirm the exact marker substring is
        // present in the body before trusting a hit.
        Ok(v.as_array().and_then(|items| {
            items
                .iter()
                .find(|i| {
                    i.get("body")
                        .and_then(Value::as_str)
                        .is_some_and(|b| b.contains(marker))
                })
                .and_then(|i| i.get("number").and_then(Value::as_i64))
        }))
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
        // Idempotent: re-adding an existing edge must succeed as a no-op (the
        // decompose materializer re-wires every sweep, issue #134). GitHub
        // returns a 4xx whose body says the dependency already exists; swallow
        // exactly that and surface anything else.
        match self
            .gh_try(&[
                "api",
                "-X",
                "POST",
                &format!("repos/{}/issues/{issue}/dependencies/blocked_by", self.repo),
                "-F",
                &format!("issue_id={id}"),
            ])
            .await?
        {
            Ok(_) => Ok(()),
            Err(stderr) if is_dependency_exists(&stderr) => Ok(()),
            Err(stderr) => {
                bail!("adding blocked_by {blocker_repo}#{blocker} to #{issue}: {stderr}")
            }
        }
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

    async fn update_issue_title(&self, number: i64, title: &str) -> Result<()> {
        self.gh(&[
            "issue",
            "edit",
            &number.to_string(),
            "--repo",
            &self.repo,
            "--title",
            title,
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

    /// Open PRs the forge's timeline cross-references to `issue` (GitHub's
    /// "Development" linkage: any PR whose body/comment mentions `#issue`,
    /// closing-keyword or not). One page of 100 is generous for this —
    /// worker/planner call it once right before opening a PR, never in a
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
                        Some(PrComment {
                            body,
                            created_at,
                            ..Default::default()
                        })
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

    async fn update_comment(&self, comment_id: &str, body: &str) -> Result<()> {
        // GraphQL `updateIssueComment` edits a PR conversation comment by its
        // node id (the id the bulk observe folded in, §1.5).
        let query = "mutation($id:ID!,$body:String!){\
             updateIssueComment(input:{id:$id,body:$body}){clientMutationId}}";
        self.gh(&[
            "api",
            "graphql",
            "-f",
            &format!("query={query}"),
            "-f",
            &format!("id={comment_id}"),
            "-f",
            &format!("body={body}"),
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

    async fn issue_comments(&self, issue: i64) -> Result<Vec<String>> {
        let raw = self
            .gh(&[
                "issue",
                "view",
                &issue.to_string(),
                "--repo",
                &self.repo,
                "--json",
                "comments",
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing gh issue view comments")?;
        Ok(v.get("comments")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|c| c.get("body").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
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

    async fn update_branch(&self, pr: i64, expected_head_sha: &str) -> Result<UpdateBranchOutcome> {
        // `expected_head_sha` pins the update to the head we observed: GitHub
        // rejects it with a 422 if the head moved (TOCTOU-safe, issue #221).
        match self
            .gh_try(&[
                "api",
                "--method",
                "PUT",
                &format!("repos/{}/pulls/{pr}/update-branch", self.repo),
                "-f",
                &format!("expected_head_sha={expected_head_sha}"),
            ])
            .await?
        {
            Ok(_) => Ok(UpdateBranchOutcome::Updated),
            Err(stderr) => {
                let lower = stderr.to_ascii_lowercase();
                if lower.contains("expected head sha") || lower.contains("head branch was modified")
                {
                    Ok(UpdateBranchOutcome::HeadMoved)
                } else if lower.contains("not behind")
                    || lower.contains("up to date")
                    || lower.contains("up-to-date")
                {
                    Ok(UpdateBranchOutcome::AlreadyUpToDate)
                } else {
                    bail!("gh update-branch failed for #{pr}: {stderr}");
                }
            }
        }
    }

    async fn observe_open_prs(&self, pr_review_context: &str) -> Result<MergeTailObservation> {
        // Informer-cache observe (issue #221, ADR 0012 decision 3): one GraphQL
        // query folds every signal the two old sweeps read per PR (merge state,
        // arm-marker comments, review threads, the check rollup, the pr-review
        // status) into a single round-trip. `rateLimit { cost }` rides along so
        // the API cost is measured, not estimated.
        let (owner, name) = self
            .repo
            .split_once('/')
            .with_context(|| format!("repo slug `{}` is not owner/name", self.repo))?;
        // The query is the module-level `MERGE_TAIL_OBSERVE_QUERY` const so a
        // unit test can parse-check its braces (issue #242); this branch (#223)
        // extended it with each comment's `id`/`viewerDidAuthor` (claim
        // authenticity) and each thread's last comment (the Fixer arm's
        // trigger), plus `pageInfo` for the >100-comment pagination fallback.
        let raw = self
            .gh(&[
                "api",
                "graphql",
                "-f",
                &format!("query={MERGE_TAIL_OBSERVE_QUERY}"),
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={name}"),
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing merge-tail GraphQL")?;
        let graphql_cost = v
            .pointer("/data/rateLimit/cost")
            .and_then(Value::as_u64)
            .map(|c| c as u32);
        let nodes = v
            .pointer("/data/repository/pullRequests/nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut requests: u32 = 1;
        let mut prs = Vec::with_capacity(nodes.len());
        for node in &nodes {
            let Some(mut obs) = Self::pr_observation_from_node(node, pr_review_context) else {
                continue;
            };
            // Window clipped older comments → paginate the full set via GraphQL
            // (not REST — REST drops `viewerDidAuthor`, breaking the claim's
            // authenticity on a chatty PR, f6). Rare (a PR with >100 comments);
            // the extra reads are counted so the cost stays honest.
            let total = node
                .pointer("/comments/totalCount")
                .and_then(Value::as_u64)
                .unwrap_or(0) as usize;
            if total > obs.comments.len() {
                if self.comment_pagination_parked(obs.pr.number) {
                    // Cooldown: this conversation already truncated at this
                    // size — stay parked without re-spending the page budget.
                    obs.comments_complete = false;
                } else {
                    let (all, reqs, complete) = self.paginate_pr_comments(obs.pr.number).await?;
                    obs.comments = all;
                    obs.comments_complete = complete;
                    requests += reqs;
                    self.record_pagination_outcome(obs.pr.number, complete);
                }
            }
            prs.push(obs);
        }
        Ok(MergeTailObservation {
            prs,
            cost: ObserveCost {
                requests,
                graphql_cost,
            },
        })
    }

    async fn mark_pr_ready(&self, pr: i64) -> Result<()> {
        self.gh(&["pr", "ready", &pr.to_string(), "--repo", &self.repo])
            .await?;
        Ok(())
    }

    async fn close_pr(&self, pr: i64) -> Result<()> {
        // Idempotent: closing an already-closed PR just reports it closed.
        match self
            .gh_try(&["pr", "close", &pr.to_string(), "--repo", &self.repo])
            .await?
        {
            Ok(_) => Ok(()),
            Err(stderr) if stderr.to_ascii_lowercase().contains("already") => Ok(()),
            Err(stderr) => bail!("closing PR #{pr}: {stderr}"),
        }
    }

    async fn set_commit_status(
        &self,
        head_sha: &str,
        context: &str,
        state: CommitStatusState,
        description: &str,
    ) -> Result<()> {
        // GitHub truncates the description at 140 chars; keep it short.
        let description: String = description.chars().take(140).collect();
        self.gh(&[
            "api",
            "-X",
            "POST",
            &format!("repos/{}/statuses/{head_sha}", self.repo),
            "-f",
            &format!("state={}", state.as_str()),
            "-f",
            &format!("context={context}"),
            "-f",
            &format!("description={description}"),
        ])
        .await?;
        Ok(())
    }

    async fn commit_status(
        &self,
        head_sha: &str,
        context: &str,
    ) -> Result<Option<CommitStatusState>> {
        // `.../commits/{sha}/statuses` lists statuses newest-first; take the
        // most recent entry for the requested context.
        let raw = self
            .gh(&[
                "api",
                &format!("repos/{}/commits/{head_sha}/statuses", self.repo),
            ])
            .await?;
        let v: Value = serde_json::from_str(&raw).context("parsing commit statuses output")?;
        let state = v.as_array().and_then(|items| {
            items
                .iter()
                .find(|s| s.get("context").and_then(Value::as_str) == Some(context))
                .and_then(|s| s.get("state").and_then(Value::as_str))
                .and_then(CommitStatusState::from_gh)
        });
        Ok(state)
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

    // FakeForge tests never execute these real GraphQL queries, so a syntax
    // slip here (an unbalanced brace killed every merge-tail sweep in
    // production on 2026-07-21) only surfaces via this parse-level check.
    fn assert_braces_balance(query: &str) {
        let mut depth = 0i64;
        for (i, c) in query.chars().enumerate() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    assert!(depth >= 0, "extra closing brace at index {i}");
                }
                _ => {}
            }
        }
        assert_eq!(depth, 0, "{depth} unclosed brace(s) in the query");
    }

    #[test]
    fn merge_tail_observe_query_braces_balance() {
        assert_braces_balance(MERGE_TAIL_OBSERVE_QUERY);
    }

    #[test]
    fn linked_open_prs_query_braces_balance() {
        assert_braces_balance(LINKED_OPEN_PRS_QUERY);
    }

    #[test]
    fn fold_comment_pages_preserves_author_and_id_across_pages() {
        // Two scripted `comments` connection pages (the second reached only by
        // following `endCursor`): a >100-comment PR. `viewerDidAuthor` and the
        // node `id` must survive on every page (f6/f8), so a self-authored claim
        // marker on page 2 is still recognised (and editable by its id).
        let page1 = serde_json::json!({
            "pageInfo": {"hasNextPage": true, "endCursor": "c1"},
            "nodes": [
                {"id": "n1", "body": "hi", "createdAt": "2026-01-01T00:00:00Z", "viewerDidAuthor": false},
            ]
        });
        let page2 = serde_json::json!({
            "pageInfo": {"hasNextPage": false, "endCursor": serde_json::Value::Null},
            "nodes": [
                {"id": "n2", "body": "<!-- meguri:claim instance=me run=r-1 -->",
                 "createdAt": "2026-01-02T00:00:00Z", "viewerDidAuthor": true},
            ]
        });
        let folded = fold_comment_pages(&[page1, page2]);
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].id, "n1");
        assert!(!folded[0].viewer_did_author);
        // The page-2 claim marker keeps its authorship and node id.
        assert_eq!(folded[1].id, "n2");
        assert!(folded[1].viewer_did_author);
        assert!(folded[1].body.contains("meguri:claim"));
    }

    #[test]
    fn comment_pagination_cooldown_parks_regardless_of_count_movement() {
        let forge = GhForge::new("o/r");
        // Truncated: parked for the TTL — conversation growth (an attacker
        // adding a comment per poll) must NOT reset or bypass the cooldown.
        forge.record_pagination_outcome(7, false);
        assert!(forge.comment_pagination_parked(7));
        // A completed pagination clears the memo.
        forge.record_pagination_outcome(7, true);
        assert!(!forge.comment_pagination_parked(7));
        // Other PRs are unaffected.
        assert!(!forge.comment_pagination_parked(8));
    }

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
    fn open_issues_flatten_slurped_pages_and_drop_prs() {
        // `gh api --paginate --slurp` yields one outer array with one inner
        // array per page; issues past page one must survive the flatten, and
        // PR entries (marked by `pull_request`) must be dropped on any page.
        let raw = r#"[
          [{"number":1,"title":"first","body":"b1","labels":[{"name":"bug"}]},
           {"number":2,"title":"a PR","body":"", "pull_request":{"url":"x"}}],
          [{"number":150,"title":"second page","body":null,"labels":[]}]
        ]"#;
        let issues = GhForge::open_issues_from_slurped_pages(raw).unwrap();
        assert_eq!(
            issues.iter().map(|i| i.number).collect::<Vec<_>>(),
            vec![1, 150]
        );
        assert_eq!(issues[0].labels, vec!["bug".to_string()]);
        // Null body degrades to empty, same as issue_from_json elsewhere.
        assert_eq!(issues[1].body, "");
    }

    #[test]
    fn open_issues_reject_unslurped_page_concatenation() {
        // Without --slurp, two pages arrive as two JSON arrays back to back —
        // the very input that used to blow up mid-triage. It must be a parse
        // error (so a regression is loud), never a silent partial read.
        let raw = r#"[{"number":1,"title":"a","body":""}]
[{"number":2,"title":"b","body":""}]"#;
        assert!(GhForge::open_issues_from_slurped_pages(raw).is_err());
    }

    #[test]
    fn tail_lines_keeps_the_tail_and_marks_the_cut() {
        assert_eq!(GhForge::tail_lines("a\nb", 5), "a\nb");
        let tailed = GhForge::tail_lines("a\nb\nc\nd", 2);
        assert!(tailed.starts_with("[... 2 earlier lines omitted ...]"));
        assert!(tailed.ends_with("c\nd"));
    }
}
