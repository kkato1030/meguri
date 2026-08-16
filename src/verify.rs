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

use crate::store::{Outcome, Verify};

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

/// o18: ブランチが base(worktree を切った時点の SHA)より commit が進んでいるか。
///
/// clean_tree(o17)は「ゴミが残っていない」を見るだけで、**何も作らず commit も無い**
/// 空の worktree でも通ってしまう。実際に成果 commit が積まれたかは base からの距離で見る。
pub fn commits_ahead(worktree: &Path, base_sha: &str) -> Result<Check> {
    let (ok, out, err) = git_out(worktree, &["rev-list", "--count", &format!("{base_sha}..HEAD")])?;
    if !ok {
        return Ok(Check { name: "commits_ahead", pass: false, detail: format!("git rev-list failed: {err}") });
    }
    let n: u32 = out.trim().parse().unwrap_or(0);
    if n > 0 {
        Ok(Check { name: "commits_ahead", pass: true, detail: format!("{n} commit(s) ahead of base") })
    } else {
        Ok(Check { name: "commits_ahead", pass: false, detail: "no commits ahead of base".into() })
    }
}

/// o19: Outcome の `verify=command` を worktree で実行し、exit 0 を要求する。
///
/// これが Outcome 固有の「達成の確かめ方」そのもの(§5)。clean_tree/commits_ahead が
/// 「体裁」の検証なのに対し、これは「中身が要件を満たすか」の検証。
/// human/rollup の Outcome は meguri が回すコマンドを持たないので `None`(検証対象外)。
pub fn check_command(o: &Outcome, worktree: &Path) -> Result<Option<Check>> {
    let cmd = match &o.verify {
        Verify::Command(c) => c,
        Verify::Human | Verify::Rollup => return Ok(None),
    };
    let out = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(worktree)
        .output()
        .with_context(|| format!("failed to run check_command: {cmd}"))?;
    if out.status.success() {
        return Ok(Some(Check { name: "check_command", pass: true, detail: format!("`{cmd}` exited 0") }));
    }
    // 落ちたときは診断のため末尾を少しだけ添える(耐久チャネル: exit code + 標準エラー)。
    let code = out.status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
    let err = String::from_utf8_lossy(&out.stderr);
    let tail: String = err.trim().lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
    let detail = if tail.is_empty() {
        format!("`{cmd}` exited {code}")
    } else {
        format!("`{cmd}` exited {code}:\n{tail}")
    };
    Ok(Some(Check { name: "check_command", pass: false, detail }))
}

/// o20: 適用可能な全検証子を回して結果を集める(rollup の材料)。
/// 体裁(clean_tree / commits_ahead)は常に。中身(check_command)は verify=command のときだけ。
pub fn run_all(o: &Outcome, worktree: &Path, base_sha: &str) -> Result<Vec<Check>> {
    let mut checks = vec![clean_tree(worktree)?, commits_ahead(worktree, base_sha)?];
    if let Some(c) = check_command(o, worktree)? {
        checks.push(c);
    }
    Ok(checks)
}

/// o20: 全 Check が pass なら verified の関門を通る。空でも「体裁 2 つ」は必ず含む。
pub fn all_pass(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.pass)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(verify: Verify) -> Outcome {
        Outcome {
            id: 1,
            intent_id: 1,
            statement: "s".into(),
            description: String::new(),
            verify,
            human_satisfied: false,
            requires: vec![],
        }
    }

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

    #[test]
    fn commits_ahead_pass_and_fail() {
        let repo = temp_repo();
        let (_, base, _) = git_out(&repo, &["rev-parse", "HEAD"]).unwrap();

        // base=HEAD のまま = 進んでいない。
        let c = commits_ahead(&repo, &base).unwrap();
        assert!(!c.pass, "expected not-ahead, got: {}", c.detail);

        // commit を 1 つ積むと base より進む。
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "work"]);
        let c = commits_ahead(&repo, &base).unwrap();
        assert!(c.pass);
        assert!(c.detail.contains("1 commit"));

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn check_command_pass_fail_and_na() {
        let repo = temp_repo();

        // exit 0 → pass。
        let c = check_command(&outcome(Verify::Command("true".into())), &repo).unwrap().unwrap();
        assert!(c.pass, "got: {}", c.detail);

        // exit 非0 → fail(diagnostic 込み)。
        let c = check_command(&outcome(Verify::Command("echo boom >&2; exit 3".into())), &repo).unwrap().unwrap();
        assert!(!c.pass);
        assert!(c.detail.contains("exited 3"));
        assert!(c.detail.contains("boom"));

        // command は worktree で走る(cwd 検証)。
        std::fs::write(repo.join("marker"), "x").unwrap();
        let c = check_command(&outcome(Verify::Command("test -f marker".into())), &repo).unwrap().unwrap();
        assert!(c.pass);

        // human/rollup は検証対象外 = None。
        assert!(check_command(&outcome(Verify::Human), &repo).unwrap().is_none());
        assert!(check_command(&outcome(Verify::Rollup), &repo).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn run_all_rollup() {
        let repo = temp_repo();
        let (_, base, _) = git_out(&repo, &["rev-parse", "HEAD"]).unwrap();

        // clean だが commit なし → commits_ahead で落ちる。
        let checks = run_all(&outcome(Verify::Command("true".into())), &repo, &base).unwrap();
        assert_eq!(checks.len(), 3);
        assert!(!all_pass(&checks));

        // f.txt を commit すると 3 つとも通る(check は test -f f.txt)。
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "work"]);
        let checks = run_all(&outcome(Verify::Command("test -f f.txt".into())), &repo, &base).unwrap();
        assert!(all_pass(&checks), "{:?}", checks);

        // human の Outcome は check_command 抜きの体裁 2 つだけ。
        let checks = run_all(&outcome(Verify::Human), &repo, &base).unwrap();
        assert_eq!(checks.len(), 2);
        assert!(all_pass(&checks));

        let _ = std::fs::remove_dir_all(&repo);
    }
}
