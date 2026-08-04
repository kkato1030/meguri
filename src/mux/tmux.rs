//! tmux backend。session「meguri-<project>」に run ごとの window を作る。

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use super::{Mux, Pane};

pub struct Tmux {
    session: String,
}

impl Tmux {
    pub fn new(project: &str) -> Self {
        Self {
            session: format!("meguri-{project}"),
        }
    }
}

impl Mux for Tmux {
    fn spawn_agent(&self, title: &str, cwd: &Path, command: &[String]) -> Result<Pane> {
        let cwd = cwd
            .to_str()
            .context("worktree パスが UTF-8 ではありません")?;
        // `new-session` は初期 window ごとエージェントを起動し、既存 session
        // には `new-window` で足す — どちらも作った pane の id を返させる。
        let exists = tmux(&["has-session", "-t", &self.session]).is_ok();
        let head: Vec<&str> = if exists {
            vec![
                "new-window",
                "-t",
                &self.session,
                "-n",
                title,
                "-P",
                "-F",
                "#{pane_id}",
                "-c",
                cwd,
            ]
        } else {
            vec![
                "new-session",
                "-d",
                "-s",
                &self.session,
                "-n",
                title,
                "-P",
                "-F",
                "#{pane_id}",
                "-c",
                cwd,
            ]
        };
        let mut args = head;
        args.extend(command.iter().map(String::as_str));
        let id = tmux_capture(&args)?;
        Ok(Pane {
            id,
            attach_hint: format!("tmux attach -t {}", self.session),
        })
    }

    fn send_line(&self, pane: &Pane, line: &str) -> Result<()> {
        // `-l` でリテラル送信(key 名解釈をさせない)。
        tmux(&["send-keys", "-t", &pane.id, "-l", line])?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        tmux(&["send-keys", "-t", &pane.id, "Enter"])
    }

    fn pane_alive(&self, pane: &Pane) -> Result<bool> {
        Ok(tmux(&["display-message", "-p", "-t", &pane.id, "#{pane_id}"]).is_ok())
    }
}

fn tmux(args: &[&str]) -> Result<()> {
    tmux_capture(args).map(|_| ())
}

fn tmux_capture(args: &[&str]) -> Result<String> {
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
