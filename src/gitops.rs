//! git 操作の集約。他のモジュールから `git` プロセスを直接起動しない。
//!
//! v0 で git に求めることは 2 つ:
//! - **worktree を切る** — run ごとに隔離された作業場所を base ブランチから作る
//! - **検証する** — trust-but-verify の 3 点セット(clean / ahead / check_command)

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// base ブランチの先端から `branch` を切って worktree を作り、切った時点の
/// base の SHA を返す(検証の「進んでいるか」の基準点)。`.meguri/` は
/// `.git/info/exclude` に足してコミット対象から外す。
pub fn create_worktree(
    repo: &Path,
    worktree: &Path,
    branch: &str,
    default_branch: &str,
) -> Result<String> {
    if let Some(parent) = worktree.parent() {
        std::fs::create_dir_all(parent).context("worktree 置き場の作成")?;
    }
    let base_sha = git_capture(
        repo,
        &["rev-parse", &format!("{default_branch}^{{commit}}")],
    )
    .with_context(|| format!("base ブランチ {default_branch:?} の解決"))?;
    git(
        repo,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            worktree
                .to_str()
                .context("worktree パスが UTF-8 ではありません")?,
            &base_sha,
        ],
    )?;
    // exclude は共有側 `.git/info/exclude` に書く。worktree ごとの
    // `worktrees/<name>/info/exclude` は git が読まない(gitignore(5):
    // `info/` は common dir 共有)— これはテストで実際に踏んだ落とし穴。
    let git_dir = git_capture(worktree, &["rev-parse", "--git-common-dir"])?;
    let info = Path::new(&git_dir).join("info");
    std::fs::create_dir_all(&info).context("`.git/info` の作成")?;
    let exclude = info.join("exclude");
    let mut body = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !body.lines().any(|l| l == ".meguri/") {
        body.push_str(".meguri/\n");
        std::fs::write(&exclude, body).context("`.git/info/exclude` への追記")?;
    }
    Ok(base_sha)
}

/// trust-but-verify: エージェントの success 申告をこの 3 点で独立に検証する。
/// どれか欠けたら成功として扱わない。
pub fn verify(worktree: &Path, base_sha: &str, check_command: Option<&str>) -> Result<()> {
    // 1. tree が clean(未 commit の変更が残っていない)。
    let dirty = git_capture(worktree, &["status", "--porcelain"])?;
    if !dirty.is_empty() {
        bail!("worktree に未 commit の変更があります:\n{dirty}");
    }
    // 2. base より commit が進んでいる(「何もせず success」を弾く)。
    let ahead: u64 = git_capture(
        worktree,
        &["rev-list", "--count", &format!("{base_sha}..HEAD")],
    )?
    .parse()
    .context("rev-list --count のパース")?;
    if ahead == 0 {
        bail!("base から commit が 1 つも進んでいません");
    }
    // 3. プロジェクトの check_command が通る(orchestrator 自身が実行する)。
    if let Some(check) = check_command {
        let out = Command::new("sh")
            .args(["-c", check])
            .current_dir(worktree)
            .output()
            .with_context(|| format!("check_command {check:?} の起動"))?;
        if !out.status.success() {
            bail!(
                "check_command {check:?} が失敗しました:\n{}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> Result<()> {
    git_capture(dir, args).map(|_| ())
}

fn git_capture(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .context("git の起動")?;
    if !out.status.success() {
        bail!(
            "git {} (in {}) failed: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo(dir: &Path) {
        for args in [
            vec!["init", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-m", "init"],
        ] {
            git(dir, &args).unwrap();
        }
    }

    #[test]
    fn worktree_verify_rejects_dirty_empty_and_accepts_committed_work() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);

        let wt = root.path().join("wt");
        let base = create_worktree(&repo, &wt, "meguri/test-1", "main").unwrap();

        // 何もしていない → ahead=0 で弾く。
        assert!(
            verify(&wt, &base, None)
                .unwrap_err()
                .to_string()
                .contains("進んでいません")
        );

        // 未 commit の変更 → clean 検証で弾く。
        std::fs::write(wt.join("a.txt"), "hi").unwrap();
        assert!(
            verify(&wt, &base, None)
                .unwrap_err()
                .to_string()
                .contains("未 commit")
        );

        // `.meguri/` は exclude 済みなので clean 判定を汚さない。
        std::fs::create_dir_all(wt.join(".meguri")).unwrap();
        std::fs::write(wt.join(".meguri/result.json"), "{}").unwrap();

        // commit すれば通る。check_command も orchestrator が実行する。
        git(&wt, &["add", "a.txt"]).unwrap();
        git(&wt, &["commit", "-m", "work"]).unwrap();
        verify(&wt, &base, Some("test -f a.txt")).unwrap();
        assert!(
            verify(&wt, &base, Some("false")).is_err(),
            "check_command の失敗は成功にしない"
        );
    }
}
