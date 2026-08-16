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
use serde_json::Value;

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

/// backend の自動選択: herdr サーバが生きていれば herdr(§8 の優先)、いなければ tmux。
pub fn select() -> Box<dyn Mux> {
    if Herdr::is_available() {
        Box::new(Herdr)
    } else {
        Box::new(Tmux)
    }
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

// ---- herdr backend ----

/// herdr backend(socket API 型)。§8 の優先。tmux と違い**サーバが動いている前提**
/// (auto 選択では `is_available()` で socket を確かめてから使う)。
///
/// mux の対応づけ(0.7.1 で実測):
///   - open_pane = workspace(label で再利用/新規)に tab を作り、その root pane を使う
///   - send_line = `herdr pane run <pane_id> <text>`(text + Enter)
///   - is_alive  = `herdr pane get <pane_id>` が成功する
pub struct Herdr;

impl Herdr {
    /// サーバが生きているか(auto 選択用)。
    pub fn is_available() -> bool {
        Command::new("herdr")
            .args(["workspace", "list"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// herdr を叩き、`{"result":...}` の result を返す。
    fn json(args: &[&str]) -> Result<Value> {
        let out = Command::new("herdr")
            .args(args)
            .output()
            .context("failed to run herdr (is it installed / server running?)")?;
        if !out.status.success() {
            bail!("herdr {:?} failed: {}", args, String::from_utf8_lossy(&out.stderr).trim());
        }
        let v: Value = serde_json::from_slice(&out.stdout)
            .with_context(|| format!("herdr {:?} returned non-JSON", args))?;
        if let Some(err) = v.get("error") {
            bail!("herdr {:?} error: {err}", args);
        }
        v.get("result")
            .cloned()
            .with_context(|| format!("herdr {:?} has no result", args))
    }

    /// label に一致する workspace の id を返す(無ければ None)。
    fn find_workspace(label: &str) -> Result<Option<String>> {
        let r = Self::json(&["workspace", "list"])?;
        let found = r["workspaces"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| w["label"].as_str() == Some(label))
            .and_then(|w| w["workspace_id"].as_str().map(String::from));
        Ok(found)
    }
}

impl Mux for Herdr {
    fn open_pane(&self, session: &str, title: &str) -> Result<PaneId> {
        // workspace は label で再利用、無ければ作る。どちらも root_pane が今回の pane。
        let result = match Self::find_workspace(session)? {
            Some(ws) => Self::json(&["tab", "create", "--workspace", &ws, "--label", title, "--no-focus"])?,
            None => Self::json(&["workspace", "create", "--label", session, "--no-focus"])?,
        };
        let pane_id = result["root_pane"]["pane_id"]
            .as_str()
            .context("herdr create result has no root_pane.pane_id")?
            .to_string();
        Ok(PaneId(pane_id))
    }

    fn send_line(&self, pane: &PaneId, text: &str) -> Result<()> {
        // pane run = コマンド文字列 + Enter。成功時は空出力なので JSON でなく exit status を見る。
        let out = Command::new("herdr")
            .args(["pane", "run", &pane.0, text])
            .output()
            .context("failed to run herdr pane run")?;
        if !out.status.success() {
            bail!("herdr pane run failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(())
    }

    fn is_alive(&self, pane: &PaneId) -> Result<bool> {
        let ok = Command::new("herdr")
            .args(["pane", "get", &pane.0])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
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

    #[test]
    fn herdr_open_send_alive() {
        if !Herdr::is_available() {
            eprintln!("skip: herdr server not running");
            return;
        }
        let m = Herdr;
        let session = format!("meguri-test-{}", std::process::id());
        let pane = m.open_pane(&session, "smoke").unwrap();
        assert!(m.is_alive(&pane).unwrap());

        let dir = std::env::temp_dir().join(format!("meguri-herdr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("marker");
        let cmd = format!("printf ok > {}", marker.display());

        // 生成直後の pane はシェル準備前で最初の send を落とすことがあるので、
        // marker が出るまで送信を繰り返す(printf は冪等)。
        let mut seen = false;
        for _ in 0..40 {
            m.send_line(&pane, &cmd).unwrap();
            for _ in 0..5 {
                if marker.exists() {
                    seen = true;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            if seen {
                break;
            }
        }
        assert!(seen, "marker file was not written by the pane");
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ok");

        // 後始末: pane_id "wN:pM" の "wN" が workspace id。
        if let Some(ws) = pane.0.split(':').next() {
            let _ = Command::new("herdr").args(["workspace", "close", ws]).output();
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
