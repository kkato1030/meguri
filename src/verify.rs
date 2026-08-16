//! meguri 側の独立検証(§9.3、i3 の o17-o20)。
//!
//! エージェントの自己申告(result.json の `status`)を**信じきらない**(§3.5 trust-but-verify)。
//! meguri 自身が worktree の耐久状態(git / コマンド)を見て、Work が verified の関門を
//! 通るかを判定する。各検証子は小さな bool + 理由(`Check`)を返し、o20 が rollup する。
//!
//! git は gitops と同じくコマンド実行。ただしここでは **exit 非0 も「データ」**として扱う
//! (o19 の check_command 失敗は Err ではなく `pass=false`)。

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// 検証子ひとつの結果。o17-o19 が返し、o20 がまとめる。
#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub pass: bool,
    pub detail: String,
}

/// worktree で git を走らせ `(成功?, stdout, stderr)` を返す。exit 非0 も拾う。
fn git_out(worktree: &Path, args: &[&str]) -> Result<(bool, String, String)> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()
        .context("failed to run git (is it installed?)")?;
    Ok((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    ))
}

/// o17: working tree が clean か(未コミットの変更・追跡外ファイルが残っていないか)。
///
/// エージェントが「実装した」と言っても、commit し忘れ・ゴミファイルが残っていれば
/// Artifact にできない。`.meguri/` は共有 exclude 済み(gitops)なので現れない。
pub fn clean_tree(worktree: &Path) -> Result<Check> {
    let (ok, out, err) = git_out(worktree, &["status", "--porcelain"])?;
    if !ok {
        return Ok(Check { name: "clean_tree", pass: false, detail: format!("git status failed: {err}") });
    }
    if out.is_empty() {
        Ok(Check { name: "clean_tree", pass: true, detail: "working tree is clean".into() })
    } else {
        let n = out.lines().count();
        Ok(Check { name: "clean_tree", pass: false, detail: format!("{n} uncommitted path(s):\n{out}") })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }

    fn temp_repo() -> std::path::PathBuf {
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let repo = std::env::temp_dir().join(format!("meguri-verify-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q"]);
        std::fs::write(repo.join("README.md"), "hi").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
        repo
    }

    #[test]
    fn clean_tree_pass_and_fail() {
        let repo = temp_repo();

        // commit 直後は clean。
        let c = clean_tree(&repo).unwrap();
        assert!(c.pass, "expected clean, got: {}", c.detail);

        // 追跡外ファイルを置くと clean でない。
        std::fs::write(repo.join("scratch.txt"), "x").unwrap();
        let c = clean_tree(&repo).unwrap();
        assert!(!c.pass);
        assert!(c.detail.contains("scratch.txt"));

        // .meguri/ は共有 exclude に入れれば無視される(実行系の scratch を検証が拾わない)。
        std::fs::remove_file(repo.join("scratch.txt")).unwrap();
        let excl = repo.join(".git").join("info").join("exclude");
        std::fs::create_dir_all(excl.parent().unwrap()).unwrap();
        std::fs::write(&excl, ".meguri/\n").unwrap();
        std::fs::create_dir_all(repo.join(".meguri")).unwrap();
        std::fs::write(repo.join(".meguri").join("result.json"), "{}").unwrap();
        let c = clean_tree(&repo).unwrap();
        assert!(c.pass, "expected .meguri/ ignored, got: {}", c.detail);

        let _ = std::fs::remove_dir_all(&repo);
    }
}
