//! mux(pane を供給する層、§8)。meguri がエージェントを走らせる場所。
//!
//! 求めるのは 3 操作だけ(v1/v2 で確立): pane を作る / 1 行打ち込む / 生死を見る。
//! pane は**シェルの中**で開くので、エージェント CLI が exit しても pane と最終画面は
//! 残る(「ブロック ≠ 失敗」— 人間がいつでも attach して引き取れる、§3.5)。
//!
//! 抽象(trait)は §8 が herdr / tmux / remote を想定しているため導入する。最初の
//! backend は **tmux**(自動検証しやすい)。herdr backend は p2.2b で足す(§8 の優先)。

use std::process::Command;

use anyhow::{bail, Context, Result};

/// backend 固有の pane ハンドル(tmux なら window id "@3" 等)。
#[derive(Debug, Clone)]
pub struct PaneId(pub String);

pub trait Mux {
    /// 名前付き session に pane(シェル)を開き、ハンドルを返す。
    fn open_pane(&self, session: &str, title: &str) -> Result<PaneId>;
    /// pane に 1 行打ち込む(末尾で Enter)。
    fn send_line(&self, pane: &PaneId, text: &str) -> Result<()>;
    /// pane がまだ生きているか。
    fn is_alive(&self, pane: &PaneId) -> Result<bool>;
}

// ---- tmux backend ----

pub struct Tmux;

impl Tmux {
    fn run(args: &[&str]) -> Result<std::process::Output> {
        Command::new("tmux")
            .args(args)
            .output()
            .context("failed to run tmux (is it installed?)")
    }

    fn ok(args: &[&str]) -> Result<String> {
        let out = Self::run(args)?;
        if !out.status.success() {
            bail!("tmux {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

impl Mux for Tmux {
    fn open_pane(&self, session: &str, title: &str) -> Result<PaneId> {
        // session が無ければ作る(detached)。window にはコマンドを渡さない =
        // 既定シェルが動き続けるので、あとで送るエージェントが exit しても pane は残る。
        let exists = Self::run(&["has-session", "-t", session])?.status.success();
        let id = if exists {
            Self::ok(&["new-window", "-t", session, "-n", title, "-P", "-F", "#{window_id}"])?
        } else {
            Self::ok(&["new-session", "-d", "-s", session, "-n", title, "-P", "-F", "#{window_id}"])?
        };
        Ok(PaneId(id))
    }

    fn send_line(&self, pane: &PaneId, text: &str) -> Result<()> {
        // テキストと Enter を分ける(-l = literal で、テキストがキー名に解釈されるのを防ぐ)。
        Self::ok(&["send-keys", "-t", &pane.0, "-l", "--", text])?;
        Self::ok(&["send-keys", "-t", &pane.0, "Enter"])?;
        Ok(())
    }

    fn is_alive(&self, pane: &PaneId) -> Result<bool> {
        // window が一覧に居るか。
        let ok = Self::run(&["display-message", "-p", "-t", &pane.0, "#{window_id}"])?.status.success();
        Ok(ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmux_available() -> bool {
        Command::new("tmux").arg("-V").output().map(|o| o.status.success()).unwrap_or(false)
    }

    #[test]
    fn tmux_open_send_alive() {
        if !tmux_available() {
            eprintln!("skip: tmux not available");
            return;
        }
        let m = Tmux;
        // 衝突しない session 名(pid ベース)。
        let session = format!("meguri-test-{}", std::process::id());
        let pane = m.open_pane(&session, "smoke").unwrap();
        assert!(m.is_alive(&pane).unwrap());

        // シェルにファイル書き込みを打ち込み、結果を確認する。
        let dir = std::env::temp_dir().join(format!("meguri-mux-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("marker");
        m.send_line(&pane, &format!("printf ok > {}", marker.display())).unwrap();

        // シェルが処理するのを、ファイル出現をポーリングして待つ(sleep でなく条件待ち)。
        let mut seen = false;
        for _ in 0..200 {
            if marker.exists() {
                seen = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(seen, "marker file was not written by the pane shell");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ok");

        // 後始末。
        let _ = Command::new("tmux").args(["kill-session", "-t", &session]).output();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
