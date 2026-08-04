//! 起動前の folder-trust prime(v1 の preflight を最小移植)。
//!
//! `claude` CLI は fresh worktree での初回起動時に「このフォルダのファイルを
//! 信頼するか?」ダイアログで止まる。meguri は画面を読まないので誰も答えられず、
//! run が永久に始まらない — v2 は run ごとに worktree を新規に切るので毎回
//! 発生する(v1 で解決済みの失敗の再観測。docs/design/v2-roadmap.md の規律
//! どおり、再観測を根拠にこの機構を戻した)。
//!
//! 対策: 対話 pane を spawn する直前に、その worktree で headless の claude を
//! 1 回だけ走らせて folder trust を記録させる。この 1 回は **ツールを一切実行
//! できない**条件で走る — yolo なし・meguri 所有の deny-all `--settings`・
//! `--strict-mcp-config`。worktree に悪意ある CLAUDE.md があっても、pane が
//! 立つ前に Bash/Edit/MCP を駆動することはできない。
//!
//! best-effort: prime の失敗(claude が古い・タイムアウト等)は警告して pane
//! 起動に進む。その場合は従来どおりダイアログで止まるので、人間が attach して
//! 答えれば run は続く。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config::Agent;

const PRIME_TIMEOUT: Duration = Duration::from_secs(30);
const NOOP_PROMPT: &str = "reply ok and make no changes";

/// deny-all settings(v1 ADR 0027 の prime 仕様そのまま)。Claude Code の
/// permission スキーマに「全部 deny」のワイルドカードは無いので、組み込み
/// ツールを名前で列挙する — CLI が新ツールを足したらここも追随が要る。
const DENY_SETTINGS_JSON: &str = r#"{
  "permissions": {
    "deny": ["Bash", "BashOutput", "KillShell", "Read", "Edit", "Write", "Glob", "Grep", "WebFetch", "WebSearch", "Task", "NotebookEdit", "TodoWrite", "SlashCommand", "ExitPlanMode", "mcp__*"],
    "defaultMode": "plan"
  },
  "enableAllProjectMcpServers": false,
  "enabledMcpjsonServers": []
}
"#;

/// worktree の folder trust を prime する。claude 以外の CLI は対象外(他の
/// CLI に folder-trust ゲートは無い)。失敗しても run は止めない。
pub fn prime(worktree: &Path, agent: &Agent) {
    let base = Path::new(&agent.command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&agent.command);
    if base != "claude" {
        return;
    }
    match try_prime(worktree, agent) {
        Ok(elapsed) => println!("preflight: folder trust を記録しました ({elapsed:?})"),
        Err(e) => eprintln!(
            "preflight: 失敗しました({e:#})。pane の trust ダイアログには attach して答えてください"
        ),
    }
}

fn try_prime(worktree: &Path, agent: &Agent) -> Result<Duration> {
    let settings = ensure_deny_settings()?;
    let mut argv = model_flag(&agent.args);
    argv.extend([
        "--strict-mcp-config".into(),
        "--settings".into(),
        settings.to_string_lossy().into_owned(),
        "-p".into(),
        NOOP_PROMPT.into(),
    ]);

    let start = Instant::now();
    let mut child = Command::new(&agent.command)
        .args(&argv)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("prime の起動")?;
    // std に wait_timeout が無いので try_wait をポーリングする。
    loop {
        if let Some(status) = child.try_wait().context("prime の監視")? {
            if status.success() {
                return Ok(start.elapsed());
            }
            anyhow::bail!("prime が exit {} で終了", status.code().unwrap_or(-1));
        }
        if start.elapsed() > PRIME_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("prime が {PRIME_TIMEOUT:?} でタイムアウト");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// deny-all settings ファイルを `~/.meguri/preflight/` に用意する(所有者のみ
/// 読める 0600。エージェントに書き換えさせない)。
fn ensure_deny_settings() -> Result<PathBuf> {
    let dir = crate::config::meguri_home().join("preflight");
    std::fs::create_dir_all(&dir).context("preflight ディレクトリの作成")?;
    let path = dir.join("deny-settings.json");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(DENY_SETTINGS_JSON) {
        std::fs::write(&path, DENY_SETTINGS_JSON).context("deny settings の書き出し")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .context("deny settings の権限設定")?;
        }
    }
    Ok(path)
}

/// pane の args から `--model <名前>` だけを prime に持ち越す(モデル固有の
/// 初回状態も一緒に prime されるように)。yolo は絶対に持ち越さない。
fn model_flag(args: &[String]) -> Vec<String> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--model"
            && let Some(m) = iter.next()
        {
            return vec!["--model".into(), m.clone()];
        }
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_settings_is_valid_json_denying_every_surface() {
        let v: serde_json::Value = serde_json::from_str(DENY_SETTINGS_JSON).unwrap();
        let deny = v.pointer("/permissions/deny").unwrap().as_array().unwrap();
        for must in ["Bash", "Edit", "Write", "mcp__*"] {
            assert!(deny.iter().any(|d| d == must), "missing {must}");
        }
        assert_eq!(v.pointer("/permissions/defaultMode").unwrap(), "plan");
    }

    #[test]
    fn model_flag_carries_only_the_model() {
        let args: Vec<String> = ["--dangerously-skip-permissions", "--model", "opus"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(model_flag(&args), vec!["--model", "opus"]);
        assert!(model_flag(&["--dangerously-skip-permissions".into()]).is_empty());
    }
}
