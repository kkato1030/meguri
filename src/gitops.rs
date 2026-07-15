//! Git plumbing: worktree lifecycle, branch naming, and the independent
//! verification the orchestrator runs instead of trusting agent claims.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub async fn run_git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .context("spawning git")?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        bail!(
            "git {} (in {}) failed: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
}

/// Blocking sibling of [`run_git`] for the synchronous verification hooks
/// (`Flavor::verify_work` runs outside an executor-friendly context).
pub fn run_git_sync(dir: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("spawning git")?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        bail!(
            "git {} (in {}) failed: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
}

/// Resolve the toplevel of the Git work tree containing `dir` (blocking, via
/// `git rev-parse --show-toplevel`). Errors when `dir` is not inside a Git
/// work tree — callers that want a fallback must handle it explicitly rather
/// than silently getting `dir` back.
pub fn repo_toplevel_sync(dir: &Path) -> Result<PathBuf> {
    let top = run_git_sync(dir, &["rev-parse", "--show-toplevel"])
        .with_context(|| format!("{} is not inside a Git repository", dir.display()))?;
    Ok(PathBuf::from(top))
}

/// The fetch refspec meguri sets on a managed bare clone. `git clone --bare`
/// leaves `remote.origin.fetch` unset, so `refs/remotes/origin/*` would never
/// update and every `origin/<default>` lookup would silently fall back to a
/// stale local ref. Setting this (and doing an initial fetch) makes the managed
/// clone behave like a normal `origin`-tracking repo. NOT `--mirror`'s
/// `+refs/*:refs/*`, which would prune the in-flight `meguri/*` branches.
const MANAGED_FETCH_REFSPEC: &str = "+refs/heads/*:refs/remotes/origin/*";

/// Health of a candidate managed-clone directory (see [`ensure_bare_clone`]).
/// `doctor` reads it too, to tell "declared but not cloned yet" (normal) from
/// "clone failed / broken remnant".
#[derive(Debug)]
pub enum CloneHealth {
    /// A well-formed managed bare clone — [`ensure_bare_clone`] is a no-op.
    Healthy,
    /// Nothing there yet (missing or empty dir) — clone into it.
    Absent,
    /// Something is there but it is not a healthy managed clone (a stray file,
    /// a non-bare repo, a clone interrupted before the initial fetch). Carries a
    /// human-readable reason; [`ensure_bare_clone`] refuses to touch it.
    Broken(String),
}

/// Classify a managed-clone directory. Deliberately stricter than "does `HEAD`
/// exist": a clone that died after `git clone --bare` wrote `HEAD` but before
/// the refspec/fetch completed must be caught as `Broken`, not silently treated
/// as healthy (which would make later `origin/<default>` lookups fail obscurely).
pub async fn clone_health(dest: &Path, repo_slug: &str) -> CloneHealth {
    match std::fs::read_dir(dest) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CloneHealth::Absent,
        Err(e) => return CloneHealth::Broken(format!("cannot read {}: {e}", dest.display())),
        Ok(mut entries) => {
            if entries.next().is_none() {
                return CloneHealth::Absent; // exists but empty
            }
        }
    }
    match run_git(dest, &["rev-parse", "--is-bare-repository"]).await {
        Ok(out) if out.trim() == "true" => {}
        Ok(out) => return CloneHealth::Broken(format!("not a bare repository (is-bare={out:?})")),
        Err(e) => return CloneHealth::Broken(format!("not a git repository ({e})")),
    }
    // The origin URL must name the same repo the config declares. Otherwise
    // changing `repo_slug` while keeping the same project `id` would leave the
    // old bare clone in place: the forge would see the new slug while
    // worktree/fetch/push kept using the stale repo — silent cross-repo work.
    match run_git(dest, &["config", "--get", "remote.origin.url"]).await {
        Err(_) => return CloneHealth::Broken("remote.origin.url is not set".into()),
        Ok(url) => match slug_from_remote_url(&url) {
            Some(got) if got.eq_ignore_ascii_case(repo_slug) => {}
            Some(got) => {
                return CloneHealth::Broken(format!(
                    "remote.origin.url points at {got}, not {repo_slug}"
                ));
            }
            None => {
                return CloneHealth::Broken(format!("cannot parse remote.origin.url {url:?}"));
            }
        },
    }
    match run_git(dest, &["config", "--get-all", "remote.origin.fetch"]).await {
        Ok(out) if out.lines().any(|l| l == MANAGED_FETCH_REFSPEC) => {}
        _ => {
            return CloneHealth::Broken(format!(
                "remote.origin.fetch is not {MANAGED_FETCH_REFSPEC}"
            ));
        }
    }
    match run_git(
        dest,
        &[
            "for-each-ref",
            "--count=1",
            "--format=%(refname)",
            "refs/remotes/origin",
        ],
    )
    .await
    {
        Ok(out) if !out.trim().is_empty() => {}
        _ => {
            return CloneHealth::Broken(
                "no refs/remotes/origin/* (initial fetch never completed)".into(),
            );
        }
    }
    CloneHealth::Healthy
}

/// Extract `owner/repo` from a git remote URL (or path), so a clone's
/// `remote.origin.url` can be matched against the declared `repo_slug`
/// regardless of the transport gh happened to use. Handles the common shapes —
/// `https://host/owner/repo(.git)`, `git@host:owner/repo(.git)`,
/// `ssh://…/owner/repo(.git)`, and a plain filesystem path ending in
/// `owner/repo(.git)` — by taking the last two `/`- or `:`-separated segments.
fn slug_from_remote_url(url: &str) -> Option<String> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);
    let parts: Vec<&str> = url.split(['/', ':']).filter(|s| !s.is_empty()).collect();
    match parts.as_slice() {
        [.., owner, repo] if !owner.is_empty() && !repo.is_empty() => {
            Some(format!("{owner}/{repo}"))
        }
        _ => None,
    }
}

/// Set the managed fetch refspec and run the initial fetch on a freshly-cloned
/// bare repo, so `refs/remotes/origin/*` is populated. Split out from
/// [`ensure_bare_clone`] so it can be exercised in tests against a plain
/// `git clone --bare` of a local origin, without `gh` or the network.
async fn configure_managed_remote(dest: &Path) -> Result<()> {
    run_git(
        dest,
        &["config", "remote.origin.fetch", MANAGED_FETCH_REFSPEC],
    )
    .await
    .context("setting remote.origin.fetch on the managed clone")?;
    run_git(dest, &["fetch", "origin"])
        .await
        .context("initial fetch of the managed clone")?;
    Ok(())
}

/// Materialize (idempotently) a meguri-managed **bare** clone of `repo_slug` at
/// `dest` (`~/.meguri/repos/<id>`). Inherits `gh`'s credential helper via
/// `gh repo clone`, consistent with the forge's gh dependence.
///
/// - Already a healthy managed clone → no-op (a cheap local health probe; no
///   network, no `gh`).
/// - Absent (missing/empty dir) → `gh repo clone … -- --bare`, then set the
///   fetch refspec and do the initial fetch.
/// - A broken remnant (stray file, non-bare repo, interrupted clone) → `bail!`
///   loudly with a "remove it and re-run" hint. meguri never `rm -rf`s a
///   directory it did not create.
pub async fn ensure_bare_clone(dest: &Path, repo_slug: &str) -> Result<()> {
    match clone_health(dest, repo_slug).await {
        CloneHealth::Healthy => return Ok(()),
        CloneHealth::Absent => {}
        CloneHealth::Broken(why) => bail!(
            "managed clone at {} is broken ({why}); remove it and re-run",
            dest.display()
        ),
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating managed-clone parent {}", parent.display()))?;
    }
    gh_clone_bare(repo_slug, dest).await?;
    configure_managed_remote(dest).await?;
    Ok(())
}

/// `gh repo clone <slug> <dest> -- --bare` — the one place meguri shells out to
/// clone. The `-- --bare` passes `--bare` through to the underlying `git clone`.
async fn gh_clone_bare(repo_slug: &str, dest: &Path) -> Result<()> {
    let dest = dest.to_string_lossy().to_string();
    let out = tokio::process::Command::new("gh")
        .args(["repo", "clone", repo_slug, &dest, "--", "--bare"])
        .output()
        .await
        .context("spawning gh (is the GitHub CLI installed?)")?;
    if out.status.success() {
        Ok(())
    } else {
        bail!(
            "gh repo clone {repo_slug} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
}

pub fn slugify(title: &str) -> String {
    let mut slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }
    let slug = slug.trim_matches('-');
    let mut truncated: String = slug.chars().take(30).collect();
    while truncated.ends_with('-') {
        truncated.pop();
    }
    if truncated.is_empty() {
        "work".to_string()
    } else {
        truncated
    }
}

/// `meguri/<issue>-<slug>-<runhash>`: the run-scoped hash keeps concurrent
/// or retried runs on the same issue from colliding.
pub fn branch_name(issue: i64, title: &str, run_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = hex::encode(&Sha256::digest(run_id.as_bytes())[..3]);
    format!("meguri/{issue}-{slug}-{hash}", slug = slugify(title))
}

/// `meguri/t<task_id>-<slug>-<runhash>`: the local-task counterpart of
/// [`branch_name`]. The `t` prefix keeps local branches out of
/// [`issue_from_branch`]'s number space, and Phase 4 detects a re-claim by
/// matching the `meguri/t<id>-` prefix on the remote.
pub fn task_branch_name(task_id: i64, title: &str, run_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let hash = hex::encode(&Sha256::digest(run_id.as_bytes())[..3]);
    format!("meguri/t{task_id}-{slug}-{hash}", slug = slugify(title))
}

/// The task id a [`task_branch_name`]-style branch encodes
/// (`meguri/t<id>-<slug>-<runhash>`); `None` for anything else, including
/// issue branches (`meguri/<issue>-…`).
pub fn task_from_branch(branch: &str) -> Option<i64> {
    branch
        .strip_prefix("meguri/t")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// The issue number a [`branch_name`]-style branch encodes
/// (`meguri/<issue>-<slug>-<runhash>`); None for branches that don't follow
/// the convention (human-made heads).
pub fn issue_from_branch(branch: &str) -> Option<i64> {
    branch
        .strip_prefix("meguri/")?
        .split('-')
        .next()?
        .parse()
        .ok()
}

pub fn worktree_path(worktrees_root: &Path, project_id: &str, branch: &str) -> PathBuf {
    worktrees_root
        .join(project_id)
        .join(branch.replace('/', "-"))
}

/// Create (or reuse) a worktree for `branch` off the project's default
/// branch. Prefers `origin/<default>` when a remote exists. `extra_excludes`
/// (a project's `worktree_setup.exclude`) is appended to `info/exclude`
/// alongside the always-on `.meguri/`.
pub async fn create_worktree(
    repo_path: &Path,
    worktree: &Path,
    branch: &str,
    default_branch: &str,
    extra_excludes: &[String],
) -> Result<()> {
    if worktree.join(".git").exists() {
        // Resuming an interrupted run. `worktree_setup.exclude` may have
        // changed since this worktree was first created, so re-apply it
        // rather than assuming the original creation covered it.
        return exclude_paths(worktree, extra_excludes).await;
    }
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Best-effort freshness; offline or remote-less repos still work.
    let _ = run_git(repo_path, &["fetch", "origin", default_branch]).await;
    let start_point = if run_git(
        repo_path,
        &["rev-parse", "--verify", &format!("origin/{default_branch}")],
    )
    .await
    .is_ok()
    {
        format!("origin/{default_branch}")
    } else {
        default_branch.to_string()
    };

    let wt = worktree.to_string_lossy().to_string();
    run_git(
        repo_path,
        &[
            "worktree",
            "add",
            "--force",
            "-b",
            branch,
            &wt,
            &start_point,
        ],
    )
    .await
    .context("git worktree add")?;

    exclude_paths(worktree, extra_excludes).await
}

/// Attach a worktree to an *existing* branch (a PR's head): detach the
/// branch from whichever worktree still holds it (git refuses two checkouts
/// of one branch), reset it to the pushed tip, and check it out here.
/// `extra_excludes` — see [`create_worktree`].
pub async fn attach_worktree(
    repo_path: &Path,
    worktree: &Path,
    branch: &str,
    extra_excludes: &[String],
) -> Result<()> {
    if worktree.join(".git").exists() {
        // Resuming, or reusing the worktree that already owns the branch
        // (attach and create share the same path scheme). Best-effort sync
        // to the pushed tip; a diverged or offline worktree keeps working.
        let _ = run_git(worktree, &["fetch", "origin", branch]).await;
        let _ = run_git(
            worktree,
            &["merge", "--ff-only", &format!("origin/{branch}")],
        )
        .await;
        // `worktree_setup.exclude` may have changed since this worktree was
        // first attached, so re-apply it rather than assuming it's covered.
        return exclude_paths(worktree, extra_excludes).await;
    }
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Best-effort freshness; offline or remote-less repos still work.
    let _ = run_git(repo_path, &["fetch", "origin", branch]).await;
    detach_branch(repo_path, branch).await?;

    let origin_ref = format!("origin/{branch}");
    if run_git(repo_path, &["rev-parse", "--verify", &origin_ref])
        .await
        .is_ok()
    {
        // Create or reset the local branch to the pushed tip (safe: nothing
        // has it checked out after the detach).
        run_git(repo_path, &["branch", "--force", branch, &origin_ref]).await?;
    } else {
        run_git(
            repo_path,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
        )
        .await
        .with_context(|| format!("branch {branch} exists neither on origin nor locally"))?;
    }

    let wt = worktree.to_string_lossy().to_string();
    run_git(repo_path, &["worktree", "add", "--force", &wt, branch])
        .await
        .context("git worktree add (attach)")?;

    exclude_paths(worktree, extra_excludes).await
}

/// Create (or re-point) a review worktree detached at `head_sha` (a PR
/// head). Detached HEAD avoids colliding with whichever worktree still has
/// the PR branch checked out (e.g. the planner's on the same host). The
/// worktree is issue-scoped and survives review rounds (issue #92): when it
/// already exists — resuming an interrupted run, or reviewing the next push
/// — it is reset hard onto the new head instead of being recreated, so the
/// pane standing in it stays valid. `extra_excludes` — see
/// [`create_worktree`].
pub async fn create_review_worktree(
    repo_path: &Path,
    worktree: &Path,
    head_branch: &str,
    head_sha: &str,
    extra_excludes: &[String],
) -> Result<()> {
    // Best-effort: the head may already be local (pushed from this host).
    let _ = run_git(repo_path, &["fetch", "origin", head_branch]).await;

    if worktree.join(".git").exists() {
        run_git(worktree, &["reset", "--hard", head_sha])
            .await
            .context("git reset --hard (review re-point)")?;
        // Stray untracked files from the previous round would taint the
        // read-only checkout; `.meguri/` is excluded, so it survives.
        run_git(worktree, &["clean", "-fd"])
            .await
            .context("git clean (review re-point)")?;
        // `worktree_setup.exclude` may have changed since this worktree was
        // first created, so re-apply it rather than assuming it's covered.
        return exclude_paths(worktree, extra_excludes).await;
    }
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let wt = worktree.to_string_lossy().to_string();
    run_git(
        repo_path,
        &["worktree", "add", "--force", "--detach", &wt, head_sha],
    )
    .await
    .context("git worktree add (review)")?;

    exclude_paths(worktree, extra_excludes).await
}

/// Detach `branch` from every worktree that has it checked out so another
/// worktree can take it over.
pub async fn detach_branch(repo_path: &Path, branch: &str) -> Result<()> {
    let _ = run_git(repo_path, &["worktree", "prune"]).await;
    let list = run_git(repo_path, &["worktree", "list", "--porcelain"]).await?;
    let want = format!("branch refs/heads/{branch}");
    let mut current: Option<PathBuf> = None;
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(PathBuf::from(path));
        } else if line == want
            && let Some(path) = current.take()
        {
            run_git(&path, &["checkout", "--detach"])
                .await
                .with_context(|| format!("detaching {branch} from worktree {}", path.display()))?;
        }
    }
    Ok(())
}

/// Keep `.meguri/` (prompts, result contract) — and any project-configured
/// `worktree_setup.exclude` entries — out of the agent's diffs and out of
/// the clean-tree verification.
async fn exclude_paths(worktree: &Path, extra: &[String]) -> Result<()> {
    let exclude = run_git(worktree, &["rev-parse", "--git-path", "info/exclude"]).await?;
    let exclude_path = if Path::new(&exclude).is_absolute() {
        PathBuf::from(exclude)
    } else {
        worktree.join(exclude)
    };
    if let Some(dir) = exclude_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let current = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    let mut to_append = String::new();
    for entry in std::iter::once(".meguri/").chain(extra.iter().map(String::as_str)) {
        if !current.lines().any(|line| line == entry) {
            to_append.push_str(entry);
            to_append.push('\n');
        }
    }
    if !to_append.is_empty() {
        std::fs::write(&exclude_path, current + &to_append)?;
    }
    Ok(())
}

pub async fn remove_worktree(repo_path: &Path, worktree: &Path) -> Result<()> {
    let wt = worktree.to_string_lossy().to_string();
    run_git(repo_path, &["worktree", "remove", "--force", &wt]).await?;
    Ok(())
}

/// A worktree registered on the repo, as reported by
/// `git worktree list --porcelain` (includes the primary checkout).
#[derive(Debug, Clone)]
pub struct ListedWorktree {
    pub path: PathBuf,
    /// `None` for a detached HEAD.
    pub branch: Option<String>,
}

pub async fn list_worktrees(repo_path: &Path) -> Result<Vec<ListedWorktree>> {
    let out = run_git(repo_path, &["worktree", "list", "--porcelain"]).await?;
    let mut worktrees = Vec::new();
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            worktrees.push(ListedWorktree {
                path: PathBuf::from(path),
                branch: None,
            });
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
            && let Some(last) = worktrees.last_mut()
        {
            last.branch = Some(branch.to_string());
        }
    }
    Ok(worktrees)
}

/// Delete a local branch. Without `force`, the branch must be merged into
/// the default branch (`origin/<default>` when a remote exists) — checked
/// explicitly with merge-base, because `git branch -d`'s own heuristic
/// depends on whatever upstream the branch happens to track.
pub async fn delete_branch(
    repo_path: &Path,
    branch: &str,
    default_branch: &str,
    force: bool,
) -> Result<()> {
    if !force {
        let base = if run_git(
            repo_path,
            &["rev-parse", "--verify", &format!("origin/{default_branch}")],
        )
        .await
        .is_ok()
        {
            format!("origin/{default_branch}")
        } else {
            default_branch.to_string()
        };
        run_git(repo_path, &["merge-base", "--is-ancestor", branch, &base])
            .await
            .with_context(|| format!("branch {branch} is not merged into {base}"))?;
    }
    run_git(repo_path, &["branch", "-D", branch]).await?;
    Ok(())
}

pub async fn prune_worktrees(repo_path: &Path) -> Result<()> {
    run_git(repo_path, &["worktree", "prune"]).await?;
    Ok(())
}

/// Current head sha of the default branch, preferring the remote's view.
/// Fetch is best-effort; offline or remote-less repos fall back to the local
/// branch.
pub async fn default_branch_head(repo_path: &Path, default_branch: &str) -> Result<String> {
    let _ = run_git(repo_path, &["fetch", "origin", default_branch]).await;
    if let Ok(sha) = run_git(
        repo_path,
        &["rev-parse", &format!("origin/{default_branch}")],
    )
    .await
    {
        return Ok(sha);
    }
    run_git(repo_path, &["rev-parse", default_branch]).await
}

/// Like [`run_git`] but returns raw stdout bytes with no trimming, so a blob's
/// trailing newline/whitespace survives. Callers needing text convert with
/// strict UTF-8 (unlike `run_git`'s lossy `trim_end`).
async fn run_git_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .context("spawning git")?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        bail!(
            "git {} (in {}) failed: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
}

/// A repo-relative path as it exists on the default branch, read via git
/// plumbing (blob-direct, not the working tree). See ADR 0015.
#[derive(Debug)]
pub enum DefaultBranchFile {
    /// A regular file (mode 100644 / 100755): its exact contents.
    Content(String),
    /// No entry at `rel` in the default branch tree.
    Absent,
    /// `rel` exists but is not a regular file (symlink 120000, tree 040000, or
    /// submodule gitlink 160000) — its contents can't be returned as a blob.
    NotRegularFile,
}

/// Read a repo-relative path's contents as they exist on the default branch,
/// without touching the working tree (ADR 0015: advisory/discovery reads see
/// the merged declaration, not a working-tree approximation).
///
/// Prefers `origin/<default_branch>`, falling back to the local branch when no
/// remote exists — the same base resolution as [`default_branch_head`]. Unlike
/// that helper this does *not* fetch: callers are hot (the doctor loops, the
/// scheduler tick) and the run flow already keeps the ref fetched.
///
/// The entry type is settled from a single `ls-tree` record so a directory or
/// symlink can never be mistaken for file contents; the blob is then read by
/// its object id, not by re-resolving the pathspec.
pub async fn read_file_at_default_branch(
    repo_path: &Path,
    default_branch: &str,
    rel: &str,
) -> Result<DefaultBranchFile> {
    let base = if run_git(
        repo_path,
        &["rev-parse", "--verify", &format!("origin/{default_branch}")],
    )
    .await
    .is_ok()
    {
        format!("origin/{default_branch}")
    } else {
        default_branch.to_string()
    };

    // One entry, exact-path match. `--full-tree` makes the pathspec
    // root-relative; `:(literal)` disables pathspec magic/globbing; the absence
    // of `-r` means a directory returns its own tree entry, not its children.
    let literal = format!(":(literal){rel}");
    let out = run_git(
        repo_path,
        &[
            "ls-tree",
            "--full-tree",
            "-z",
            "--format=%(objectmode) %(objecttype) %(objectname) %(path)",
            &base,
            "--",
            &literal,
        ],
    )
    .await
    .with_context(|| format!("ls-tree {base} for {rel}"))?;

    let mut entries = out.split('\0').filter(|s| !s.is_empty());
    let Some(entry) = entries.next() else {
        return Ok(DefaultBranchFile::Absent);
    };
    if entries.next().is_some() {
        // More than one entry means `rel` matched several paths (e.g. a
        // trailing slash listing a directory's children) — never content.
        return Ok(DefaultBranchFile::Absent);
    }

    // `mode type oid path`: mode/type/oid are space-free tokens, so split on
    // the first three spaces and take the remainder as the (possibly
    // space-bearing) path.
    let mut parts = entry.splitn(4, ' ');
    let (Some(mode), Some(_type), Some(oid), Some(path)) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        bail!("unparseable ls-tree entry: {entry:?}");
    };
    if path != rel {
        return Ok(DefaultBranchFile::Absent);
    }

    match mode {
        "100644" | "100755" => {
            let bytes = run_git_bytes(repo_path, &["cat-file", "blob", oid]).await?;
            let text = String::from_utf8(bytes)
                .with_context(|| format!("{rel} on {base} is not valid UTF-8"))?;
            Ok(DefaultBranchFile::Content(text))
        }
        _ => Ok(DefaultBranchFile::NotRegularFile),
    }
}

/// A branch on origin as the cleaner's stale-branch check sees it.
#[derive(Debug, Clone)]
pub struct RemoteBranch {
    /// Branch name without the `origin/` prefix.
    pub name: String,
    /// Committer time (unix epoch seconds) of the branch tip.
    pub committer_unix: i64,
}

/// Branches on origin with their tip committer times. Fetches with `--prune`
/// first (best-effort) so deleted branches drop out of the listing.
pub async fn list_remote_branches(repo_path: &Path) -> Result<Vec<RemoteBranch>> {
    let _ = run_git(repo_path, &["fetch", "--prune", "origin"]).await;
    let out = run_git(
        repo_path,
        &[
            "for-each-ref",
            "--format=%(refname) %(committerdate:unix)",
            "refs/remotes/origin",
        ],
    )
    .await?;
    let mut branches = Vec::new();
    for line in out.lines() {
        let Some((refname, date)) = line.rsplit_once(' ') else {
            continue;
        };
        let Some(name) = refname.strip_prefix("refs/remotes/origin/") else {
            continue;
        };
        if name == "HEAD" {
            continue; // symbolic ref, not a branch
        }
        branches.push(RemoteBranch {
            name: name.to_string(),
            committer_unix: date.parse().unwrap_or(0),
        });
    }
    Ok(branches)
}

/// True when nothing is uncommitted (untracked counts as dirty).
pub async fn status_clean(worktree: &Path) -> Result<bool> {
    Ok(run_git(worktree, &["status", "--porcelain"])
        .await?
        .is_empty())
}

/// Commits on HEAD that are not on the base ref.
pub async fn commits_ahead(worktree: &Path, default_branch: &str) -> Result<u64> {
    let base = if run_git(
        worktree,
        &["rev-parse", "--verify", &format!("origin/{default_branch}")],
    )
    .await
    .is_ok()
    {
        format!("origin/{default_branch}")
    } else {
        default_branch.to_string()
    };
    let count = run_git(worktree, &["rev-list", "--count", &format!("{base}..HEAD")]).await?;
    Ok(count.parse().unwrap_or(0))
}

/// The unified diff of HEAD against the base ref — three-dot, i.e. the
/// changes introduced on HEAD since it diverged from base (the same shape a
/// PR shows). Mirrors [`commits_ahead`]'s `origin/<base>` vs `<base>`
/// resolution so the self-review reads exactly the PR's own diff, locally,
/// without any forge call (ADR 0006).
pub async fn diff_against_base(worktree: &Path, default_branch: &str) -> Result<String> {
    let base = if run_git(
        worktree,
        &["rev-parse", "--verify", &format!("origin/{default_branch}")],
    )
    .await
    .is_ok()
    {
        format!("origin/{default_branch}")
    } else {
        default_branch.to_string()
    };
    run_git(worktree, &["diff", &format!("{base}...HEAD")]).await
}

pub async fn push_branch(worktree: &Path, branch: &str) -> Result<()> {
    run_git(worktree, &["push", "-u", "origin", branch]).await?;
    Ok(())
}

/// Fetch the base branch and return its tip commit (`origin/<base>` when a
/// remote exists, the local branch otherwise). The conflict resolver pins
/// this sha at claim time so the merge target stays fixed even if the base
/// moves mid-run.
pub async fn fetch_base_tip(repo_path: &Path, base_branch: &str) -> Result<String> {
    // Best-effort freshness; offline or remote-less repos still work.
    let _ = run_git(repo_path, &["fetch", "origin", base_branch]).await;
    if let Ok(sha) = run_git(
        repo_path,
        &["rev-parse", "--verify", &format!("origin/{base_branch}")],
    )
    .await
    {
        return Ok(sha);
    }
    run_git(
        repo_path,
        &[
            "rev-parse",
            "--verify",
            &format!("refs/heads/{base_branch}"),
        ],
    )
    .await
    .with_context(|| format!("base branch {base_branch} exists neither on origin nor locally"))
}

/// Fetch a PR head branch from origin and return its freshly-fetched tip sha
/// (`origin/<branch>`). The decompose materializer pins this against the PR's
/// approved head_sha to notice a head that moved mid-sweep (issue #134).
///
/// The fetch is **not** best-effort here (unlike [`fetch_base_tip`]): this gates
/// an irreversible issue-creation, so a failed fetch — network down, or the
/// branch deleted on the remote (`git fetch origin <gone>` errors) — must surface
/// as an error, never fall through to a stale local remote-tracking ref that
/// could still match the approved sha. The sweep then skips and retries next tick.
pub async fn fetch_branch_tip(repo_path: &Path, branch: &str) -> Result<String> {
    run_git(repo_path, &["fetch", "origin", branch])
        .await
        .with_context(|| format!("fetching branch {branch} from origin"))?;
    run_git(
        repo_path,
        &["rev-parse", "--verify", &format!("origin/{branch}")],
    )
    .await
    .with_context(|| format!("branch {branch} not found on origin"))
}

/// Read a file's contents at a git ref (`git show <ref>:<path>`); the ref may be
/// a branch or a sha. The decompose materializer reads the proposal spec from
/// the exact approved head sha, not the working tree (issue #134).
pub async fn show_file_at_ref(repo_path: &Path, git_ref: &str, path: &str) -> Result<String> {
    run_git(repo_path, &["show", &format!("{git_ref}:{path}")])
        .await
        .with_context(|| format!("reading {path} at {git_ref}"))
}

/// Whether `ancestor` is reachable from `descendant` — how the conflict
/// resolver proves the base tip was actually merged, not cherry-picked
/// around. Synchronous: called from `Flavor::verify_work`.
pub fn is_ancestor(worktree: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .context("spawning git")?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => bail!(
            "git merge-base --is-ancestor {ancestor} {descendant} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// A line git would have written as a conflict marker. `=======` alone is
/// deliberately not matched (legitimate as e.g. a setext heading underline);
/// a leftover separator without its bracketing markers cannot occur.
fn is_conflict_marker_line(line: &str) -> bool {
    ["<<<<<<<", ">>>>>>>", "|||||||"].iter().any(|marker| {
        line.strip_prefix(marker)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })
}

/// Files changed between `from_ref` and HEAD that still contain conflict
/// markers — the conflict resolver's proof that a "resolved" merge did not
/// commit the markers themselves. Synchronous: called from
/// `Flavor::verify_work`. Unreadable (deleted, binary) files are skipped.
pub fn conflict_marker_files(worktree: &Path, from_ref: &str) -> Result<Vec<String>> {
    let changed = run_git_sync(
        worktree,
        &["diff", "--name-only", &format!("{from_ref}..HEAD")],
    )?;
    let mut hits = Vec::new();
    for file in changed.lines().filter(|l| !l.is_empty()) {
        let Ok(content) = std::fs::read_to_string(worktree.join(file)) else {
            continue;
        };
        if content.lines().any(is_conflict_marker_line) {
            hits.push(file.to_string());
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("Fix the bug!"), "fix-the-bug");
        assert_eq!(slugify("日本語タイトル"), "work");
        assert_eq!(
            slugify("A very long title that should be truncated somewhere"),
            "a-very-long-title-that-should"
        );
    }

    #[test]
    fn issue_from_branch_parses_meguri_branches_only() {
        assert_eq!(
            issue_from_branch("meguri/28-clean-worktrees-ab12cd"),
            Some(28)
        );
        assert_eq!(issue_from_branch("meguri/7-fix-bug-000000"), Some(7));
        assert_eq!(
            issue_from_branch(&branch_name(21, "Take over", "r")),
            Some(21)
        );
        assert_eq!(issue_from_branch("meguri/7"), Some(7));
        assert_eq!(issue_from_branch("meguri/not-a-number-x"), None);
        assert_eq!(issue_from_branch("meguri/-no-number"), None);
        assert_eq!(issue_from_branch("feature/28-something"), None);
        assert_eq!(issue_from_branch("main"), None);
    }

    #[test]
    fn task_branches_carry_the_t_prefix_and_stay_out_of_issue_space() {
        let b = task_branch_name(5, "Local task", "run-1");
        assert!(b.starts_with("meguri/t5-local-task-"));
        assert_eq!(task_from_branch(&b), Some(5));
        // Task branches never parse as issue branches, and vice versa.
        assert_eq!(issue_from_branch(&b), None);
        assert_eq!(task_from_branch("meguri/7-fix-bug-abc"), None);
        assert_eq!(task_from_branch("main"), None);
    }

    #[test]
    fn branch_name_is_stable_and_scoped() {
        let a = branch_name(7, "Fix bug", "run-1");
        let b = branch_name(7, "Fix bug", "run-2");
        assert!(a.starts_with("meguri/7-fix-bug-"));
        assert_ne!(a, b);
        assert_eq!(a, branch_name(7, "Fix bug", "run-1"));
    }

    async fn init_repo(dir: &Path) {
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
        ] {
            run_git(dir, &args).await.unwrap();
        }
    }

    /// A bare origin at `<dir>/owner/repo.git` — its path tail is `owner/repo`,
    /// so a managed clone of it has a `remote.origin.url` that
    /// [`slug_from_remote_url`] resolves to the slug `owner/repo`. Returns the
    /// bare path.
    async fn bare_origin(dir: &Path) -> PathBuf {
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        init_repo(&work).await;
        run_git(&work, &["commit", "--allow-empty", "-m", "seed"])
            .await
            .unwrap();
        let bare = dir.join("owner").join("repo.git");
        std::fs::create_dir_all(bare.parent().unwrap()).unwrap();
        run_git(
            dir,
            &[
                "clone",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        )
        .await
        .unwrap();
        bare
    }

    #[test]
    fn slug_from_remote_url_handles_common_shapes() {
        for (url, want) in [
            ("https://github.com/owner/repo.git", "owner/repo"),
            ("https://github.com/owner/repo", "owner/repo"),
            ("git@github.com:owner/repo.git", "owner/repo"),
            ("ssh://git@github.com/owner/repo.git", "owner/repo"),
            ("/tmp/x/owner/repo.git", "owner/repo"),
        ] {
            assert_eq!(slug_from_remote_url(url).as_deref(), Some(want), "{url}");
        }
        assert_eq!(slug_from_remote_url("bogus"), None);
    }

    #[tokio::test]
    async fn configure_managed_remote_sets_refspec_and_populates_origin_refs() {
        // A plain `git clone --bare` (no gh, no network) reproduces what
        // `ensure_bare_clone`'s clone step leaves behind, so we can test the
        // refspec + fetch finalize against a local origin.
        let root = tempfile::tempdir().unwrap();
        let origin = bare_origin(root.path()).await;
        let dest = root.path().join("managed.git");
        run_git(
            root.path(),
            &[
                "clone",
                "--bare",
                origin.to_str().unwrap(),
                dest.to_str().unwrap(),
            ],
        )
        .await
        .unwrap();

        // `git clone --bare` sets no fetch refspec and no remote-tracking refs.
        assert!(matches!(
            clone_health(&dest, "owner/repo").await,
            CloneHealth::Broken(_)
        ));

        configure_managed_remote(&dest).await.unwrap();

        let refspec = run_git(&dest, &["config", "--get-all", "remote.origin.fetch"])
            .await
            .unwrap();
        assert!(
            refspec.lines().any(|l| l == MANAGED_FETCH_REFSPEC),
            "refspec: {refspec:?}"
        );
        let origin_refs = run_git(
            &dest,
            &["for-each-ref", "--format=%(refname)", "refs/remotes/origin"],
        )
        .await
        .unwrap();
        assert!(
            origin_refs.contains("refs/remotes/origin/main"),
            "origin refs: {origin_refs:?}"
        );

        // Now healthy for the matching slug — and `ensure_bare_clone` is a no-op
        // (never touches gh).
        assert!(matches!(
            clone_health(&dest, "owner/repo").await,
            CloneHealth::Healthy
        ));
        ensure_bare_clone(&dest, "owner/repo").await.unwrap();

        // …but a DIFFERENT slug on the same path is broken: changing `repo_slug`
        // while keeping the project `id` must not silently reuse the old clone.
        match clone_health(&dest, "owner/other").await {
            CloneHealth::Broken(why) => assert!(why.contains("owner/repo"), "{why}"),
            other => panic!("expected Broken on slug mismatch, got {other:?}"),
        }
        // `ensure_bare_clone` bails on the mismatch rather than reusing it.
        assert!(ensure_bare_clone(&dest, "owner/other").await.is_err());
    }

    #[tokio::test]
    async fn clone_health_treats_missing_and_empty_as_absent() {
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            clone_health(&root.path().join("nope"), "owner/repo").await,
            CloneHealth::Absent
        ));
        let empty = root.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(matches!(
            clone_health(&empty, "owner/repo").await,
            CloneHealth::Absent
        ));
    }

    #[tokio::test]
    async fn ensure_bare_clone_bails_on_broken_remnants() {
        let root = tempfile::tempdir().unwrap();

        // A stray `HEAD` file (a clone that died immediately) is not a repo.
        let stray = root.path().join("stray");
        std::fs::create_dir_all(&stray).unwrap();
        std::fs::write(stray.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let err = ensure_bare_clone(&stray, "owner/repo").await.unwrap_err();
        assert!(format!("{err:#}").contains("broken"), "{err:#}");

        // A non-bare repo at the managed path is also refused (never cloned over).
        let nonbare = root.path().join("nonbare");
        std::fs::create_dir_all(&nonbare).unwrap();
        init_repo(&nonbare).await;
        let err = ensure_bare_clone(&nonbare, "owner/repo").await.unwrap_err();
        assert!(format!("{err:#}").contains("broken"), "{err:#}");
    }

    #[test]
    fn repo_toplevel_sync_walks_up_from_subdir_and_rejects_non_repos() {
        let repo = tempfile::tempdir().unwrap();
        run_git_sync(repo.path(), &["init", "-b", "main"]).unwrap();
        let sub = repo.path().join("docs").join("adr");
        std::fs::create_dir_all(&sub).unwrap();
        // Canonicalize both sides: on macOS the tempdir sits behind the
        // /var -> /private/var symlink and git reports the resolved path.
        assert_eq!(
            repo_toplevel_sync(&sub).unwrap().canonicalize().unwrap(),
            repo.path().canonicalize().unwrap()
        );

        // Outside any work tree: a hard error, never a silent fallback.
        let plain = tempfile::tempdir().unwrap();
        let err = repo_toplevel_sync(plain.path()).unwrap_err();
        assert!(
            format!("{err:#}").contains("not inside a Git repository"),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn worktree_lifecycle_and_verification() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        let wt_root = tempfile::tempdir().unwrap();
        let branch = branch_name(1, "Test issue", "run-x");
        let wt = worktree_path(wt_root.path(), "proj", &branch);

        create_worktree(repo.path(), &wt, &branch, "main", &[])
            .await
            .unwrap();
        assert!(wt.join(".git").exists());
        // Idempotent for resume.
        create_worktree(repo.path(), &wt, &branch, "main", &[])
            .await
            .unwrap();

        // .meguri is excluded from status.
        std::fs::create_dir_all(wt.join(".meguri")).unwrap();
        std::fs::write(wt.join(".meguri/result.json"), "{}").unwrap();
        assert!(status_clean(&wt).await.unwrap());
        assert_eq!(commits_ahead(&wt, "main").await.unwrap(), 0);

        // Uncommitted work is detected...
        std::fs::write(wt.join("new.txt"), "hello").unwrap();
        assert!(!status_clean(&wt).await.unwrap());

        // ...and committed work counts as ahead.
        run_git(&wt, &["add", "new.txt"]).await.unwrap();
        run_git(&wt, &["commit", "-m", "add file"]).await.unwrap();
        assert!(status_clean(&wt).await.unwrap());
        assert_eq!(commits_ahead(&wt, "main").await.unwrap(), 1);

        remove_worktree(repo.path(), &wt).await.unwrap();
        assert!(!wt.exists());
    }

    #[tokio::test]
    async fn extra_excludes_are_appended_alongside_meguri() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        let wt_root = tempfile::tempdir().unwrap();
        let branch = branch_name(2, "Excludes", "run-e");
        let wt = worktree_path(wt_root.path(), "proj", &branch);
        let extra = vec!["generated/".to_string(), "AGENTS.md".to_string()];

        create_worktree(repo.path(), &wt, &branch, "main", &extra)
            .await
            .unwrap();

        let exclude = run_git(&wt, &["rev-parse", "--git-path", "info/exclude"])
            .await
            .unwrap();
        let exclude_path = wt.join(exclude);
        let contents = std::fs::read_to_string(&exclude_path).unwrap();
        assert!(contents.contains(".meguri/"));
        assert!(contents.contains("generated/"));
        assert!(contents.contains("AGENTS.md"));

        // Re-running does not duplicate entries.
        create_worktree(repo.path(), &wt, &branch, "main", &extra)
            .await
            .unwrap();
        let contents_again = std::fs::read_to_string(&exclude_path).unwrap();
        assert_eq!(contents_again.matches("generated/").count(), 1);
    }

    #[tokio::test]
    async fn extra_excludes_apply_on_the_resume_fast_path_too() {
        // `create_worktree` short-circuits when the worktree already exists
        // (resuming); excludes configured *after* that first creation must
        // still land on a later call, not just on the initial `worktree add`.
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        let wt_root = tempfile::tempdir().unwrap();
        let branch = branch_name(6, "Resume excludes", "run-r");
        let wt = worktree_path(wt_root.path(), "proj", &branch);

        create_worktree(repo.path(), &wt, &branch, "main", &[])
            .await
            .unwrap();
        assert!(wt.join(".git").exists());

        let extra = vec!["late-generated/".to_string()];
        create_worktree(repo.path(), &wt, &branch, "main", &extra)
            .await
            .unwrap();

        let exclude = run_git(&wt, &["rev-parse", "--git-path", "info/exclude"])
            .await
            .unwrap();
        let contents = std::fs::read_to_string(wt.join(exclude)).unwrap();
        assert!(
            contents.contains("late-generated/"),
            "resume path must still apply new excludes: {contents:?}"
        );
    }

    #[tokio::test]
    async fn list_worktrees_reports_paths_and_branches() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        let wt_root = tempfile::tempdir().unwrap();
        let branch = branch_name(3, "List me", "run-l");
        let wt = worktree_path(wt_root.path(), "proj", &branch);
        create_worktree(repo.path(), &wt, &branch, "main", &[])
            .await
            .unwrap();

        let listed = list_worktrees(repo.path()).await.unwrap();
        assert_eq!(listed.len(), 2, "primary checkout + worktree: {listed:?}");
        assert_eq!(listed[0].branch.as_deref(), Some("main"));
        let entry = listed
            .iter()
            .find(|w| w.branch.as_deref() == Some(branch.as_str()))
            .expect("worktree listed");
        assert_eq!(
            std::fs::canonicalize(&entry.path).unwrap(),
            std::fs::canonicalize(&wt).unwrap()
        );
    }

    #[tokio::test]
    async fn attach_takes_over_a_branch_checked_out_elsewhere() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        // The "worker's" worktree owns the branch and committed on it.
        let wt_root = tempfile::tempdir().unwrap();
        let branch = "meguri/1-feature-abc";
        let old_wt = worktree_path(wt_root.path(), "proj", branch);
        create_worktree(repo.path(), &old_wt, branch, "main", &[])
            .await
            .unwrap();
        std::fs::write(old_wt.join("f.txt"), "v1").unwrap();
        run_git(&old_wt, &["add", "."]).await.unwrap();
        run_git(&old_wt, &["commit", "-m", "feature"])
            .await
            .unwrap();
        let tip = run_git(&old_wt, &["rev-parse", "HEAD"]).await.unwrap();

        // The fixer attaches the same branch at a different path: the old
        // worktree gets detached, the new one sits on the branch tip.
        let new_root = tempfile::tempdir().unwrap();
        let new_wt = worktree_path(new_root.path(), "proj", branch);
        attach_worktree(repo.path(), &new_wt, branch, &[])
            .await
            .unwrap();

        assert_eq!(run_git(&new_wt, &["rev-parse", "HEAD"]).await.unwrap(), tip);
        assert_eq!(
            run_git(&new_wt, &["rev-parse", "--abbrev-ref", "HEAD"])
                .await
                .unwrap(),
            branch
        );
        assert_eq!(
            run_git(&old_wt, &["rev-parse", "--abbrev-ref", "HEAD"])
                .await
                .unwrap(),
            "HEAD",
            "old worktree must be detached"
        );

        // New commits land on the branch; .meguri/ stays excluded.
        std::fs::create_dir_all(new_wt.join(".meguri")).unwrap();
        std::fs::write(new_wt.join(".meguri/result.json"), "{}").unwrap();
        assert!(status_clean(&new_wt).await.unwrap());
        assert_eq!(commits_ahead(&new_wt, "main").await.unwrap(), 1);

        // Attaching again (resume) is idempotent.
        attach_worktree(repo.path(), &new_wt, branch, &[])
            .await
            .unwrap();

        // A branch that exists nowhere fails loudly.
        let missing = worktree_path(new_root.path(), "proj", "meguri/none");
        assert!(
            attach_worktree(repo.path(), &missing, "meguri/none", &[])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn attach_worktree_applies_new_excludes_on_the_reuse_path() {
        // `attach_worktree`'s reuse branch (fetch + ff-only merge) must still
        // re-apply `extra_excludes`, not just the first `worktree add`.
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        let branch = "meguri/9-reuse";
        run_git(repo.path(), &["branch", branch]).await.unwrap();

        let wt_root = tempfile::tempdir().unwrap();
        let wt = worktree_path(wt_root.path(), "proj", branch);
        attach_worktree(repo.path(), &wt, branch, &[])
            .await
            .unwrap();

        let extra = vec!["late-generated/".to_string()];
        attach_worktree(repo.path(), &wt, branch, &extra)
            .await
            .unwrap();

        let exclude = run_git(&wt, &["rev-parse", "--git-path", "info/exclude"])
            .await
            .unwrap();
        let contents = std::fs::read_to_string(wt.join(exclude)).unwrap();
        assert!(
            contents.contains("late-generated/"),
            "reuse path must still apply new excludes: {contents:?}"
        );
    }

    #[tokio::test]
    async fn delete_branch_requires_merge_unless_forced() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        let wt_root = tempfile::tempdir().unwrap();
        let branch = branch_name(4, "Unmerged", "run-u");
        let wt = worktree_path(wt_root.path(), "proj", &branch);
        create_worktree(repo.path(), &wt, &branch, "main", &[])
            .await
            .unwrap();
        run_git(&wt, &["commit", "--allow-empty", "-m", "unmerged work"])
            .await
            .unwrap();
        remove_worktree(repo.path(), &wt).await.unwrap();

        // Unmerged: refused without force, removed with it.
        assert!(
            delete_branch(repo.path(), &branch, "main", false)
                .await
                .is_err()
        );
        delete_branch(repo.path(), &branch, "main", true)
            .await
            .unwrap();
        assert!(
            run_git(repo.path(), &["rev-parse", "--verify", &branch])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn fetch_base_tip_prefers_origin_and_pins_a_sha() {
        // Remote-less repo: falls back to the local branch tip.
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        let local_tip = run_git(repo.path(), &["rev-parse", "main"]).await.unwrap();
        assert_eq!(
            fetch_base_tip(repo.path(), "main").await.unwrap(),
            local_tip
        );
        assert!(fetch_base_tip(repo.path(), "nope").await.is_err());

        // With a remote: the origin tip wins even when the local branch lags.
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "--bare", "-b", "main"])
            .await
            .unwrap();
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        )
        .await
        .unwrap();
        run_git(repo.path(), &["push", "origin", "main"])
            .await
            .unwrap();
        run_git(repo.path(), &["commit", "--allow-empty", "-m", "ahead"])
            .await
            .unwrap();
        let pushed = run_git(repo.path(), &["rev-parse", "origin/main"])
            .await
            .unwrap();
        assert_eq!(fetch_base_tip(repo.path(), "main").await.unwrap(), pushed);
    }

    #[tokio::test]
    async fn ancestry_and_marker_scan_verify_a_merge() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        std::fs::write(repo.path().join("f.txt"), "base\n").unwrap();
        run_git(repo.path(), &["add", "."]).await.unwrap();
        run_git(repo.path(), &["commit", "-m", "base"])
            .await
            .unwrap();
        let base = run_git(repo.path(), &["rev-parse", "HEAD"]).await.unwrap();

        run_git(repo.path(), &["checkout", "-b", "topic"])
            .await
            .unwrap();
        std::fs::write(repo.path().join("f.txt"), "topic\n").unwrap();
        run_git(repo.path(), &["commit", "-am", "topic"])
            .await
            .unwrap();

        assert!(is_ancestor(repo.path(), &base, "HEAD").unwrap());
        assert!(!is_ancestor(repo.path(), "HEAD", &base).unwrap());
        assert!(is_ancestor(repo.path(), "no-such-ref", "HEAD").is_err());

        // Committed conflict markers are found; mid-line lookalikes are not.
        std::fs::write(
            repo.path().join("f.txt"),
            "<<<<<<< HEAD\ntopic\n=======\nbase\n>>>>>>> main\n",
        )
        .unwrap();
        std::fs::write(
            repo.path().join("clean.md"),
            "Heading\n=======\na <<<<<<< b\n",
        )
        .unwrap();
        run_git(repo.path(), &["add", "."]).await.unwrap();
        run_git(repo.path(), &["commit", "-m", "markers"])
            .await
            .unwrap();
        assert_eq!(
            conflict_marker_files(repo.path(), &base).unwrap(),
            vec!["f.txt".to_string()]
        );

        std::fs::write(repo.path().join("f.txt"), "resolved\n").unwrap();
        run_git(repo.path(), &["commit", "-am", "resolve"])
            .await
            .unwrap();
        assert!(
            conflict_marker_files(repo.path(), &base)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn review_worktree_is_detached_at_the_head_sha() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        // A "PR branch" with one commit; main stays behind.
        run_git(repo.path(), &["checkout", "-b", "pr-branch"])
            .await
            .unwrap();
        std::fs::write(repo.path().join("spec.md"), "spec\n").unwrap();
        run_git(repo.path(), &["add", "spec.md"]).await.unwrap();
        run_git(repo.path(), &["commit", "-m", "spec"])
            .await
            .unwrap();
        let sha = run_git(repo.path(), &["rev-parse", "HEAD"]).await.unwrap();
        run_git(repo.path(), &["checkout", "main"]).await.unwrap();

        let wt_root = tempfile::tempdir().unwrap();
        let wt = worktree_path(wt_root.path(), "proj", "review-1-run-x");
        create_review_worktree(repo.path(), &wt, "pr-branch", &sha, &[])
            .await
            .unwrap();
        assert!(wt.join("spec.md").exists());
        assert_eq!(run_git(&wt, &["rev-parse", "HEAD"]).await.unwrap(), sha);
        // Detached: rev-parse a symbolic ref fails.
        assert!(
            run_git(&wt, &["symbolic-ref", "HEAD"]).await.is_err(),
            "review worktree must be detached"
        );
        // Idempotent for resume.
        create_review_worktree(repo.path(), &wt, "pr-branch", &sha, &[])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn review_worktree_applies_new_excludes_on_the_repoint_path() {
        // The re-point branch (`reset --hard` + `clean -fd`) must still
        // re-apply `extra_excludes`, not just the first `worktree add`.
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        run_git(repo.path(), &["checkout", "-b", "pr-branch"])
            .await
            .unwrap();
        run_git(repo.path(), &["commit", "--allow-empty", "-m", "r1"])
            .await
            .unwrap();
        let sha1 = run_git(repo.path(), &["rev-parse", "HEAD"]).await.unwrap();

        let wt_root = tempfile::tempdir().unwrap();
        let wt = worktree_path(wt_root.path(), "proj", "review-2-run-x");
        create_review_worktree(repo.path(), &wt, "pr-branch", &sha1, &[])
            .await
            .unwrap();

        run_git(repo.path(), &["commit", "--allow-empty", "-m", "r2"])
            .await
            .unwrap();
        let sha2 = run_git(repo.path(), &["rev-parse", "HEAD"]).await.unwrap();
        let extra = vec!["late-generated/".to_string()];
        create_review_worktree(repo.path(), &wt, "pr-branch", &sha2, &extra)
            .await
            .unwrap();

        let exclude = run_git(&wt, &["rev-parse", "--git-path", "info/exclude"])
            .await
            .unwrap();
        let contents = std::fs::read_to_string(wt.join(exclude)).unwrap();
        assert!(
            contents.contains("late-generated/"),
            "re-point path must still apply new excludes: {contents:?}"
        );
    }

    /// Commit `content` at `rel` (creating parents) on the current branch.
    async fn commit_file(repo: &Path, rel: &str, content: &str) {
        let full = repo.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&full, content).unwrap();
        run_git(repo, &["add", rel]).await.unwrap();
        run_git(repo, &["commit", "-m", &format!("add {rel}")])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn read_file_at_default_branch_returns_committed_content() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        // Trailing newline + trailing spaces must survive verbatim.
        commit_file(repo.path(), "ops/plan.md", "line1\nline2  \n").await;

        // Working-tree-only edit must NOT be reflected: default branch wins.
        std::fs::write(repo.path().join("ops/plan.md"), "tampered\n").unwrap();

        match read_file_at_default_branch(repo.path(), "main", "ops/plan.md")
            .await
            .unwrap()
        {
            DefaultBranchFile::Content(c) => assert_eq!(c, "line1\nline2  \n"),
            other => panic!("expected Content, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_file_at_default_branch_prefers_origin() {
        // Remote-less: reads the local branch.
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        commit_file(repo.path(), "meguri.toml", "language = \"local\"\n").await;
        assert!(matches!(
            read_file_at_default_branch(repo.path(), "main", "meguri.toml")
                .await
                .unwrap(),
            DefaultBranchFile::Content(c) if c == "language = \"local\"\n"
        ));

        // With a remote, `origin/main` wins over a diverged local tip.
        let origin = tempfile::tempdir().unwrap();
        run_git(origin.path(), &["init", "--bare", "-b", "main"])
            .await
            .unwrap();
        run_git(
            repo.path(),
            &["remote", "add", "origin", origin.path().to_str().unwrap()],
        )
        .await
        .unwrap();
        run_git(repo.path(), &["push", "origin", "main"])
            .await
            .unwrap();
        // Advance local main past origin with a different value.
        commit_file(repo.path(), "meguri.toml", "language = \"ahead\"\n").await;
        assert!(matches!(
            read_file_at_default_branch(repo.path(), "main", "meguri.toml")
                .await
                .unwrap(),
            DefaultBranchFile::Content(c) if c == "language = \"local\"\n"
        ));
    }

    #[tokio::test]
    async fn read_file_at_default_branch_classifies_non_files() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        commit_file(repo.path(), "dir/inside.md", "hi\n").await;

        // Absent path.
        assert!(matches!(
            read_file_at_default_branch(repo.path(), "main", "nope.md")
                .await
                .unwrap(),
            DefaultBranchFile::Absent
        ));

        // A directory is not a regular file...
        assert!(matches!(
            read_file_at_default_branch(repo.path(), "main", "dir")
                .await
                .unwrap(),
            DefaultBranchFile::NotRegularFile
        ));
        // ...and a trailing slash (which would make ls-tree list children)
        // must not surface the first child as content.
        assert!(matches!(
            read_file_at_default_branch(repo.path(), "main", "dir/")
                .await
                .unwrap(),
            DefaultBranchFile::Absent
        ));

        // A symlink's target string must not be read as blob content.
        std::os::unix::fs::symlink("dir/inside.md", repo.path().join("link.md")).unwrap();
        run_git(repo.path(), &["add", "link.md"]).await.unwrap();
        run_git(repo.path(), &["commit", "-m", "add symlink"])
            .await
            .unwrap();
        assert!(matches!(
            read_file_at_default_branch(repo.path(), "main", "link.md")
                .await
                .unwrap(),
            DefaultBranchFile::NotRegularFile
        ));

        // Missing base ref is an error, not a silent Absent.
        assert!(
            read_file_at_default_branch(repo.path(), "no-such-branch", "dir/inside.md")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn read_file_at_default_branch_rejects_invalid_utf8() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;
        std::fs::write(repo.path().join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        run_git(repo.path(), &["add", "bin.dat"]).await.unwrap();
        run_git(repo.path(), &["commit", "-m", "binary"])
            .await
            .unwrap();
        // Strict UTF-8: lossy replacement would silently corrupt the body.
        assert!(
            read_file_at_default_branch(repo.path(), "main", "bin.dat")
                .await
                .is_err()
        );
    }
}
