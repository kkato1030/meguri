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

mod hex {
    pub fn encode(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
}

pub fn worktree_path(worktrees_root: &Path, project_id: &str, branch: &str) -> PathBuf {
    worktrees_root
        .join(project_id)
        .join(branch.replace('/', "-"))
}

/// Create (or reuse) a worktree for `branch` off the project's default
/// branch. Prefers `origin/<default>` when a remote exists.
pub async fn create_worktree(
    repo_path: &Path,
    worktree: &Path,
    branch: &str,
    default_branch: &str,
) -> Result<()> {
    if worktree.join(".git").exists() {
        return Ok(()); // resuming an interrupted run
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

    exclude_meguri(worktree).await
}

/// Attach a worktree to an *existing* branch (a PR's head): detach the
/// branch from whichever worktree still holds it (git refuses two checkouts
/// of one branch), reset it to the pushed tip, and check it out here.
pub async fn attach_worktree(repo_path: &Path, worktree: &Path, branch: &str) -> Result<()> {
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
        return Ok(());
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

    exclude_meguri(worktree).await
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

/// Keep .meguri/ (prompts, result contract) out of the agent's diffs.
async fn exclude_meguri(worktree: &Path) -> Result<()> {
    let exclude = run_git(worktree, &["rev-parse", "--git-path", "info/exclude"]).await?;
    let exclude_path = if Path::new(&exclude).is_absolute() {
        PathBuf::from(exclude)
    } else {
        worktree.join(exclude)
    };
    if let Some(dir) = exclude_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut current = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if !current.contains(".meguri/") {
        current.push_str("\n.meguri/\n");
        std::fs::write(&exclude_path, current)?;
    }
    Ok(())
}

pub async fn remove_worktree(repo_path: &Path, worktree: &Path) -> Result<()> {
    let wt = worktree.to_string_lossy().to_string();
    run_git(repo_path, &["worktree", "remove", "--force", &wt]).await?;
    Ok(())
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

pub async fn push_branch(worktree: &Path, branch: &str) -> Result<()> {
    run_git(worktree, &["push", "-u", "origin", branch]).await?;
    Ok(())
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

    #[tokio::test]
    async fn worktree_lifecycle_and_verification() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        let wt_root = tempfile::tempdir().unwrap();
        let branch = branch_name(1, "Test issue", "run-x");
        let wt = worktree_path(wt_root.path(), "proj", &branch);

        create_worktree(repo.path(), &wt, &branch, "main")
            .await
            .unwrap();
        assert!(wt.join(".git").exists());
        // Idempotent for resume.
        create_worktree(repo.path(), &wt, &branch, "main")
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
    async fn attach_takes_over_a_branch_checked_out_elsewhere() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path()).await;

        // The "worker's" worktree owns the branch and committed on it.
        let wt_root = tempfile::tempdir().unwrap();
        let branch = "meguri/1-feature-abc";
        let old_wt = worktree_path(wt_root.path(), "proj", branch);
        create_worktree(repo.path(), &old_wt, branch, "main")
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
        attach_worktree(repo.path(), &new_wt, branch).await.unwrap();

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
        attach_worktree(repo.path(), &new_wt, branch).await.unwrap();

        // A branch that exists nowhere fails loudly.
        let missing = worktree_path(new_root.path(), "proj", "meguri/none");
        assert!(
            attach_worktree(repo.path(), &missing, "meguri/none")
                .await
                .is_err()
        );
    }
}
