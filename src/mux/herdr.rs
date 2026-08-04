//! herdr backend。workspace「meguri:<project>」に run ごとの tab を作る。
//!
//! v1 で実証済みのコマンド面をそのまま使う:
//! - `workspace list` / `workspace create --label <l> --no-focus` — プロジェクト
//!   ごとの workspace を「あれば流用、無ければ作る」
//! - `tab create --workspace <id> --cwd <dir> --label <t> --no-focus` — tab を
//!   作ると root pane の id(`wN:pM` 形式)が返る
//! - `pane run <id> <command line>` — エージェントは **tab のシェルの中で**
//!   起動する。CLI が exit しても pane と最終画面が残る(tmux と違い、
//!   コマンド直起動だと exit と同時に pane が消えるため)
//! - `pane send-text` + `pane send-keys enter` — 1 行入力
//! - `pane get <id>` — 生死確認(not found エラー = 死)
//!
//! herdr の応答は JSON(エラーも stdout に `{"error":{...}}`)。socket API は
//! まだ使わない — 状態検出の増分で event 購読とともに導入する。

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::{Mux, Pane};

pub struct Herdr {
    /// workspace ラベル(`meguri:<project>`)。
    workspace: String,
}

impl Herdr {
    pub fn new(project: &str) -> Self {
        Self {
            workspace: format!("meguri:{project}"),
        }
    }
}

/// `mux.kind = "auto"` の検出材料: herdr の Unix socket が存在するか。
pub fn socket_live() -> bool {
    socket_path().exists()
}

fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("HERDR_SOCKET_PATH") {
        return PathBuf::from(p);
    }
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".config/herdr/herdr.sock")
}

impl Mux for Herdr {
    fn spawn_agent(&self, title: &str, cwd: &Path, command: &[String]) -> Result<Pane> {
        let ws = self.workspace_id()?;
        let cwd = cwd
            .to_str()
            .context("worktree パスが UTF-8 ではありません")?;
        let created = herdr_json(&[
            "tab",
            "create",
            "--workspace",
            &ws,
            "--cwd",
            cwd,
            "--label",
            title,
            "--no-focus",
        ])?;
        let pane_id = created
            .pointer("/root_pane/pane_id")
            .and_then(Value::as_str)
            .with_context(|| format!("tab create が root pane を返しませんでした: {created}"))?
            .to_string();
        // エージェントは tab のシェルの中で起動する(pane run)。
        herdr_ok(&["pane", "run", &pane_id, &shell_join(command)])?;
        Ok(Pane {
            attach_hint: format!("herdr workspace「{}」の pane {pane_id}", self.workspace),
            id: pane_id,
        })
    }

    fn send_line(&self, pane: &Pane, line: &str) -> Result<()> {
        herdr_ok(&["pane", "send-text", &pane.id, line])?;
        std::thread::sleep(std::time::Duration::from_millis(300));
        herdr_ok(&["pane", "send-keys", &pane.id, "enter"])
    }

    fn pane_alive(&self, pane: &Pane) -> Result<bool> {
        match herdr_json(&["pane", "get", &pane.id]) {
            Ok(_) => Ok(true),
            Err(e) if e.to_string().contains("not found") => Ok(false),
            Err(e) => Err(e),
        }
    }
}

impl Herdr {
    /// workspace を「あれば流用、無ければ作る」。id を返す。
    fn workspace_id(&self) -> Result<String> {
        let list = herdr_json(&["workspace", "list"])?;
        if let Some(id) = list
            .get("workspaces")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|w| w.get("label").and_then(Value::as_str) == Some(&self.workspace))
            .and_then(|w| w.get("workspace_id").and_then(Value::as_str))
        {
            return Ok(id.to_string());
        }
        let created = herdr_json(&[
            "workspace",
            "create",
            "--label",
            &self.workspace,
            "--no-focus",
        ])?;
        Ok(created
            .pointer("/workspace/workspace_id")
            .and_then(Value::as_str)
            .with_context(|| format!("workspace create が id を返しませんでした: {created}"))?
            .to_string())
    }
}

fn herdr_ok(args: &[&str]) -> Result<()> {
    herdr_raw(args).map(|_| ())
}

/// 応答は `{"id": ..., "result": {...}}` のエンベロープ付き(エラー時は
/// `{"error": {...}}`)。`result` の中身だけを返す。
fn herdr_json(args: &[&str]) -> Result<Value> {
    let raw = herdr_raw(args)?;
    let parsed: Value = serde_json::from_str(&raw)
        .with_context(|| format!("herdr {} の応答のパース: {raw}", args.join(" ")))?;
    if let Some(detail) = extract_error(&raw) {
        bail!("herdr {} failed: {detail}", args.join(" "));
    }
    Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
}

/// herdr CLI を 1 回叩く。エラーは stdout の `{"error":{...}}` を優先して
/// 文言化する(stderr は空のことが多い)。
fn herdr_raw(args: &[&str]) -> Result<String> {
    let out = Command::new("herdr")
        .args(args)
        .output()
        .context("herdr の起動")?;
    let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    if out.status.success() {
        return Ok(stdout);
    }
    let detail = extract_error(&stdout).unwrap_or_else(|| {
        format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    });
    bail!("herdr {} failed: {detail}", args.join(" "))
}

/// `{"error":{"code":"...","message":"..."}}` から人間向けの 1 行を取り出す。
fn extract_error(stdout: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdout.lines().last()?).ok()?;
    let e = v.get("error")?;
    let code = e.get("code").and_then(Value::as_str).unwrap_or("error");
    let message = e.get("message").and_then(Value::as_str).unwrap_or("");
    Some(format!("{code}: {message}"))
}

/// `pane run` に渡すコマンドラインを組む(POSIX single-quote エスケープ)。
fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_join_quotes_safely() {
        let argv = vec![
            "claude".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "it's here".to_string(),
        ];
        assert_eq!(
            shell_join(&argv),
            "claude --dangerously-skip-permissions 'it'\\''s here'"
        );
    }

    #[test]
    fn extract_error_reads_the_last_ndjson_line() {
        let raw = r#"{"error":{"code":"pane_not_found","message":"pane w4:p9 not found"},"id":"cli:pane:get"}"#;
        assert_eq!(
            extract_error(raw).unwrap(),
            "pane_not_found: pane w4:p9 not found"
        );
        assert_eq!(extract_error("not json"), None);
    }
}
