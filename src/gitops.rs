//! git worktree の作成(v0.2 execution の土台、i3 の o13)。
//!
//! 隔離 worktree = Work を実装させる作業場。base の先端から切り、専用ブランチを持つ。
//! v1/v2 の学び(§9.3)を織り込む:
//!  - worktree 個別の `info/exclude` は git が読まない(common dir 共有)→
//!    `.meguri/` は**共有 `.git/info/exclude`** に書く(実行系の scratch を tree から隠す)。
//!  - 切った時点の **base SHA を保持**(検証②「base から commit が進んでいる」の基準点)。
//!
//! git 依存は crate でなくコマンド実行(v2 と同じ。gitops 以外から git を直接叩かない)。

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

#[derive(Debug, Clone)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    /// worktree を切った時点の base の SHA(後の検証の基準点)。
    pub base_sha: String,
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("failed to run git (is it installed?)")?;
    if !out.status.success() {
        bail!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `-C` を付けない git(clone のように repo の外で走らせるもの)。
fn git_top(args: &[&str]) -> Result<String> {
    let out = Command::new("git").args(args).output().context("failed to run git")?;
    if !out.status.success() {
        bail!("git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `origin`(url または path)から `dest` に **bare clone** を作る。
/// `--mirror` は使わない(refspec が暴れる、v1 の学び)。fetch refspec を
/// remote-tracking(`refs/remotes/origin/*`)に張り、以降 `fetch` で更新できるようにする。
pub fn bare_clone(origin: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        bail!("bare clone already exists: {}", dest.display());
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dest_str = dest.to_str().context("non-utf8 dest")?;
    git_top(&["clone", "--bare", origin, dest_str])?;
    // bare clone は refs/heads を直接持つので、remote-tracking を張り直して fetch を通す。
    git(dest, &["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"])?;
    git(dest, &["fetch", "--quiet", "origin"])?;
    Ok(())
}

/// worktree の現在の HEAD SHA(o21: verified な commit を Artifact として記録する用)。
pub fn head_sha(worktree: &Path) -> Result<String> {
    git(worktree, &["rev-parse", "HEAD"])
}

/// bare clone を origin から更新する。
pub fn fetch(bare: &Path) -> Result<()> {
    git(bare, &["fetch", "--quiet", "origin"])?;
    Ok(())
}

/// `repo` の `base` 先端から、`worktrees_dir/<key>` に隔離 worktree を作り、
/// 専用ブランチ `meguri/<key>` を切る。base SHA を記録して返す。
pub fn create_worktree(repo: &Path, base: &str, worktrees_dir: &Path, key: &str) -> Result<Worktree> {
    let base_sha = git(repo, &["rev-parse", base]).with_context(|| format!("base {base:?} not found"))?;

    std::fs::create_dir_all(worktrees_dir)
        .with_context(|| format!("cannot create {}", worktrees_dir.display()))?;
    let path = worktrees_dir.join(key);
    if path.exists() {
        bail!("worktree path already exists: {}", path.display());
    }
    let branch = format!("meguri/{key}");
    let path_str = path.to_str().context("non-utf8 worktree path")?;
    git(repo, &["worktree", "add", "-b", &branch, path_str, &base_sha])?;

    exclude_meguri_scratch(repo)?;
    Ok(Worktree { path, branch, base_sha })
}

/// 共有 `.git/info/exclude` に `.meguri/` を(無ければ)足す。worktree 個別の
/// info/exclude は git が読まないため、common dir 側へ書く(v2 で実測)。
fn exclude_meguri_scratch(repo: &Path) -> Result<()> {
    let common = git(repo, &["rev-parse", "--git-common-dir"])?;
    // --git-common-dir は repo 相対のことがあるので絶対化する。
    let common = {
        let p = PathBuf::from(&common);
        if p.is_absolute() { p } else { repo.join(p) }
    };
    let excl = common.join("info").join("exclude");
    if let Some(parent) = excl.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let cur = std::fs::read_to_string(&excl).unwrap_or_default();
    if !cur.lines().any(|l| l.trim() == ".meguri/") {
        let mut s = cur;
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str(".meguri/\n");
        std::fs::write(&excl, s).with_context(|| format!("cannot write {}", excl.display()))?;
    }
    Ok(())
}

/// worktree を撤去する(掃除用)。ブランチも消す(未マージでも force)。
pub fn remove_worktree(repo: &Path, wt: &Worktree) -> Result<()> {
    let path_str = wt.path.to_str().context("non-utf8 worktree path")?;
    git(repo, &["worktree", "remove", "--force", path_str])?;
    let _ = git(repo, &["branch", "-D", &wt.branch]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr));
    }

    /// 初期 commit を持つ一時 git repo を作る。
    fn temp_repo() -> PathBuf {
        let repo = std::env::temp_dir().join(format!("meguri-git-{}-{}", std::process::id(), rand_key()));
        std::fs::create_dir_all(&repo).unwrap();
        sh(&repo, &["init", "-q"]);
        std::fs::write(repo.join("README.md"), "hi").unwrap();
        sh(&repo, &["add", "."]);
        sh(&repo, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
        repo
    }

    fn rand_key() -> String {
        // テスト内の一意化(乱数は使えないので nanos ベース)。
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        format!("{n}")
    }

    #[test]
    fn creates_isolated_worktree_off_base() {
        let repo = temp_repo();
        let base_sha_expected = git(&repo, &["rev-parse", "HEAD"]).unwrap();
        let wtdir = std::env::temp_dir().join(format!("meguri-wt-{}-{}", std::process::id(), rand_key()));
        let key = format!("k{}", rand_key());

        let wt = create_worktree(&repo, "HEAD", &wtdir, &key).unwrap();

        // worktree の実体があり、ブランチが base SHA を指す。
        assert!(wt.path.exists());
        assert!(wt.path.join("README.md").exists());
        assert_eq!(wt.base_sha, base_sha_expected);
        assert_eq!(git(&repo, &["rev-parse", &wt.branch]).unwrap(), base_sha_expected);

        // 共有 exclude に .meguri/ が入っている。
        let excl = std::fs::read_to_string(repo.join(".git").join("info").join("exclude")).unwrap();
        assert!(excl.lines().any(|l| l.trim() == ".meguri/"));

        // 掃除。
        remove_worktree(&repo, &wt).unwrap();
        assert!(!wt.path.exists());
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&wtdir);
    }

    #[test]
    fn worktree_off_bare_clone() {
        // main ブランチを持つ元 repo を作る。
        let src = std::env::temp_dir().join(format!("meguri-src-{}-{}", std::process::id(), rand_key()));
        std::fs::create_dir_all(&src).unwrap();
        sh(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(src.join("README.md"), "hi").unwrap();
        sh(&src, &["add", "."]);
        sh(&src, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);

        // bare clone を作り、そこから worktree を切る(= o14 の経路)。
        let bare = std::env::temp_dir().join(format!("meguri-bare-{}-{}.git", std::process::id(), rand_key()));
        bare_clone(src.to_str().unwrap(), &bare).unwrap();
        fetch(&bare).unwrap();

        let wtdir = std::env::temp_dir().join(format!("meguri-wtb-{}-{}", std::process::id(), rand_key()));
        let key = format!("w{}", rand_key());
        let wt = create_worktree(&bare, "main", &wtdir, &key).unwrap();
        assert!(wt.path.join("README.md").exists());

        remove_worktree(&bare, &wt).unwrap();
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&wtdir);
    }

    #[test]
    fn worktree_off_fetched_origin_ref_tracks_updates() {
        // origin(src)を作って bare clone。その後 origin に新 commit を積み、fetch する。
        let src = std::env::temp_dir().join(format!("meguri-src2-{}-{}", std::process::id(), rand_key()));
        std::fs::create_dir_all(&src).unwrap();
        sh(&src, &["init", "-q", "-b", "main"]);
        std::fs::write(src.join("README.md"), "hi").unwrap();
        sh(&src, &["add", "."]);
        sh(&src, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);

        let bare = std::env::temp_dir().join(format!("meguri-bare2-{}-{}.git", std::process::id(), rand_key()));
        bare_clone(src.to_str().unwrap(), &bare).unwrap();

        // clone 後に origin へ新しい commit(= bare の local main は古いまま、origin/main が進む)。
        std::fs::write(src.join("NEW.md"), "new").unwrap();
        sh(&src, &["add", "."]);
        sh(&src, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "advance"]);
        let new_tip = git(&src, &["rev-parse", "HEAD"]).unwrap();

        fetch(&bare).unwrap();
        // local main は古い / origin/main は新しい、ことを確認(= バグの前提)。
        assert_ne!(git(&bare, &["rev-parse", "main"]).unwrap(), new_tip);
        assert_eq!(git(&bare, &["rev-parse", "origin/main"]).unwrap(), new_tip);

        // origin/main から切れば最新に追随する(run_cmd がこの ref を使う)。
        let wtdir = std::env::temp_dir().join(format!("meguri-wtf-{}-{}", std::process::id(), rand_key()));
        let key = format!("w{}", rand_key());
        let wt = create_worktree(&bare, "origin/main", &wtdir, &key).unwrap();
        assert_eq!(wt.base_sha, new_tip, "worktree should branch off the fetched origin tip");
        assert!(wt.path.join("NEW.md").exists());

        remove_worktree(&bare, &wt).unwrap();
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&wtdir);
    }
}
