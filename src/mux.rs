//! tmux の薄いラッパ。v0 は tmux のみ(herdr は後の増分)。
//!
//! meguri が multiplexer に求めることは 3 つだけ:
//! pane を作る・1 行打ち込む・生死を確かめる。エージェントの状態推定
//! (Working/Idle/Blocked のヒューリスティック)は v0 では持たない —
//! 完了は result.json、破綻は pane の死で判定する。

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::Agent;

pub struct Pane {
    /// tmux の pane id(`%3` 形式)。session/window の rename に耐える唯一の名指し。
    pub id: String,
    /// 人間向けの attach 先(`meguri-<project>`)。
    pub session: String,
}

/// プロジェクトごとの session `meguri-<project>` に window を 1 枚足して
/// エージェント CLI を起動する。session が無ければ作る。
pub fn spawn_agent(project: &str, window: &str, cwd: &Path, agent: &Agent) -> Result<Pane> {
    let session = format!("meguri-{project}");
    let mut cmd = vec![agent.command.clone()];
    cmd.extend(agent.args.iter().cloned());

    // `new-session -A` は「あれば流用、無ければ作る」。最初の window はすぐ
    // 消す使い捨てにせず、run ごとの window を new-window で足す方が単純 —
    // ただし新規 session の初期 window が余るのを避けるため、新規時はその
    // 初期 window でそのままエージェントを起動する。
    let exists = tmux(&["has-session", "-t", &session]).is_ok();
    let pane_id = if exists {
        run_tmux_capture(&{
            let mut a = vec![
                "new-window",
                "-t",
                &session,
                "-n",
                window,
                "-P",
                "-F",
                "#{pane_id}",
                "-c",
            ];
            a.push(
                cwd.to_str()
                    .context("worktree パスが UTF-8 ではありません")?,
            );
            a.extend(cmd.iter().map(String::as_str));
            a
        })?
    } else {
        run_tmux_capture(&{
            let mut a = vec![
                "new-session",
                "-d",
                "-s",
                &session,
                "-n",
                window,
                "-P",
                "-F",
                "#{pane_id}",
                "-c",
            ];
            a.push(
                cwd.to_str()
                    .context("worktree パスが UTF-8 ではありません")?,
            );
            a.extend(cmd.iter().map(String::as_str));
            a
        })?
    };
    Ok(Pane {
        id: pane_id,
        session,
    })
}

/// pane に 1 行打ち込んで Enter。`-l` でリテラル送信(key 名解釈をさせない)。
/// テキストと Enter を分けるのは、貼り付け検知を持つ CLI が改行込みの一括
/// 送信を「入力途中」と誤認することがあるため。
pub fn send_line(pane: &Pane, line: &str) -> Result<()> {
    tmux(&["send-keys", "-t", &pane.id, "-l", line])?;
    std::thread::sleep(std::time::Duration::from_millis(300));
    tmux(&["send-keys", "-t", &pane.id, "Enter"])
}

pub fn pane_alive(pane: &Pane) -> Result<bool> {
    Ok(tmux(&["display-message", "-p", "-t", &pane.id, "#{pane_id}"]).is_ok())
}

fn tmux(args: &[&str]) -> Result<()> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .context("tmux の起動")?;
    if !out.status.success() {
        bail!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn run_tmux_capture(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .context("tmux の起動")?;
    if !out.status.success() {
        bail!(
            "tmux {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
