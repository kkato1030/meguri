use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::{
    AgentState, Multiplexer, MuxCapabilities, MuxError, MuxKind, MuxResult, PaneId, PaneSpec,
};

/// Delay between typing text and pressing Enter (paste-detection quirks).
const ENTER_DELAY: Duration = Duration::from_millis(300);

/// herdr-backed multiplexer, driven through the `herdr` CLI (a thin wrapper
/// over its local socket API).
///
/// Layout: one workspace labeled with the configured session name, one tab
/// per run. The agent is launched *inside the tab's shell* (`pane run`), so
/// the pane and its final screen survive agent exit.
pub struct HerdrMux {
    /// Workspace label that groups all meguri panes.
    session: String,
}

impl HerdrMux {
    pub fn new(session: &str) -> Self {
        Self {
            session: session.to_string(),
        }
    }

    pub fn socket_path() -> PathBuf {
        std::env::var("HERDR_SOCKET_PATH")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs::home_dir()
                    .unwrap_or_default()
                    .join(".config/herdr/herdr.sock")
            })
    }

    pub fn socket_live() -> bool {
        Self::socket_path().exists()
    }

    /// Run a herdr CLI command, returning raw stdout.
    async fn herdr_raw(&self, args: &[&str]) -> MuxResult<String> {
        let out = tokio::process::Command::new("herdr")
            .args(args)
            .output()
            .await?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
        if out.status.success() {
            Ok(stdout)
        } else {
            // Errors also arrive as ndjson on stdout: {"error":{"code","message"},...}
            let detail = extract_error(&stdout).unwrap_or_else(|| {
                format!(
                    "herdr {} -> exit {}: {}",
                    args.join(" "),
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr).trim()
                )
            });
            Err(MuxError::CommandFailed {
                kind: "herdr",
                detail,
            })
        }
    }

    /// Run a herdr CLI command where we only care about success
    /// (some commands, e.g. `pane run`/`send-text`, print nothing).
    async fn herdr_ok(&self, args: &[&str]) -> MuxResult<()> {
        self.herdr_raw(args).await.map(|_| ())
    }

    /// Run a herdr CLI command and parse its ndjson `result` object.
    async fn herdr_json(&self, args: &[&str]) -> MuxResult<Value> {
        let raw = self.herdr_raw(args).await?;
        let parsed: Value = serde_json::from_str(&raw).map_err(|e| MuxError::CommandFailed {
            kind: "herdr",
            detail: format!(
                "unparseable response for herdr {}: {e}: {raw}",
                args.join(" ")
            ),
        })?;
        if let Some(msg) = parsed
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
        {
            return Err(MuxError::CommandFailed {
                kind: "herdr",
                detail: msg.to_string(),
            });
        }
        Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn find_workspace(&self) -> MuxResult<Option<String>> {
        let result = self.herdr_json(&["workspace", "list"]).await?;
        let workspaces = result
            .get("workspaces")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for ws in workspaces {
            if ws.get("label").and_then(Value::as_str) == Some(self.session.as_str())
                && let Some(id) = ws.get("workspace_id").and_then(Value::as_str)
            {
                return Ok(Some(id.to_string()));
            }
        }
        Ok(None)
    }

    async fn workspace_id(&self) -> MuxResult<String> {
        if let Some(id) = self.find_workspace().await? {
            return Ok(id);
        }
        let result = self
            .herdr_json(&[
                "workspace",
                "create",
                "--label",
                &self.session,
                "--no-focus",
            ])
            .await?;
        result
            .pointer("/workspace/workspace_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| MuxError::CommandFailed {
                kind: "herdr",
                detail: format!("workspace create returned no id: {result}"),
            })
    }
}

fn extract_error(stdout: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(stdout).ok()?;
    parsed
        .get("error")?
        .get("message")?
        .as_str()
        .map(str::to_string)
}

/// POSIX single-quote escaping for embedding argv in a `pane run` command line.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+".contains(c))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

#[async_trait]
impl Multiplexer for HerdrMux {
    fn kind(&self) -> MuxKind {
        MuxKind::Herdr
    }

    fn capabilities(&self) -> MuxCapabilities {
        MuxCapabilities {
            native_agent_state: true,
        }
    }

    async fn ensure_session(&self) -> MuxResult<()> {
        self.workspace_id().await.map(|_| ())
    }

    async fn spawn_pane(&self, spec: &PaneSpec) -> MuxResult<PaneId> {
        let ws = self.workspace_id().await?;
        let cwd = spec.cwd.to_string_lossy().to_string();
        let mut args: Vec<String> = vec![
            "tab".into(),
            "create".into(),
            "--workspace".into(),
            ws,
            "--cwd".into(),
            cwd,
            "--label".into(),
            spec.title.clone(),
            "--no-focus".into(),
        ];
        for (k, v) in &spec.env {
            args.push("--env".into());
            args.push(format!("{k}={v}"));
        }
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let result = self.herdr_json(&argv).await?;
        let pane_id = result
            .pointer("/root_pane/pane_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| MuxError::CommandFailed {
                kind: "herdr",
                detail: format!("tab create returned no root pane: {result}"),
            })?;

        // Launch the agent inside the tab's shell so the pane survives exit.
        let command_line = shell_join(&spec.command);
        self.herdr_ok(&["pane", "run", &pane_id, &command_line])
            .await?;
        Ok(PaneId(pane_id))
    }

    async fn pane_alive(&self, pane: &PaneId) -> MuxResult<bool> {
        match self.herdr_json(&["pane", "get", &pane.0]).await {
            Ok(_) => Ok(true),
            Err(MuxError::CommandFailed { detail, .. }) if detail.contains("not found") => {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    async fn send_line(&self, pane: &PaneId, text: &str) -> MuxResult<()> {
        self.herdr_ok(&["pane", "send-text", &pane.0, text]).await?;
        tokio::time::sleep(ENTER_DELAY).await;
        self.herdr_ok(&["pane", "send-keys", &pane.0, "enter"])
            .await?;
        Ok(())
    }

    async fn read_tail(&self, pane: &PaneId, lines: usize) -> MuxResult<Vec<String>> {
        let lines_arg = lines.to_string();
        // `pane read` emits plain text (not ndjson).
        let raw = self
            .herdr_raw(&[
                "pane", "read", &pane.0, "--source", "visible", "--lines", &lines_arg,
            ])
            .await?;
        let all: Vec<String> = raw.lines().map(str::to_string).collect();
        let skip = all.len().saturating_sub(lines);
        Ok(all[skip..].to_vec())
    }

    async fn agent_state(&self, pane: &PaneId) -> MuxResult<AgentState> {
        let result = match self.herdr_json(&["pane", "get", &pane.0]).await {
            Ok(r) => r,
            Err(MuxError::CommandFailed { detail, .. }) if detail.contains("not found") => {
                return Ok(AgentState::Unknown);
            }
            Err(e) => return Err(e),
        };
        let status = result
            .pointer("/pane/agent_status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        Ok(match status {
            "working" => AgentState::Working,
            "idle" => AgentState::Idle,
            "blocked" => AgentState::Blocked,
            "done" => AgentState::Done,
            _ => AgentState::Unknown,
        })
    }

    async fn kill_pane(&self, pane: &PaneId) -> MuxResult<()> {
        self.herdr_ok(&["pane", "close", &pane.0]).await?;
        Ok(())
    }

    fn attach_command(&self, pane: &PaneId) -> String {
        let ws = pane.0.split(':').next().unwrap_or("").to_string();
        format!("herdr workspace focus {ws} >/dev/null 2>&1; herdr")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_join_quotes_safely() {
        let argv = vec![
            "claude".to_string(),
            "--permission-mode".to_string(),
            "acceptEdits".to_string(),
            "Read the file '.meguri/prompt.md' and go".to_string(),
        ];
        assert_eq!(
            shell_join(&argv),
            "claude --permission-mode acceptEdits 'Read the file '\\''.meguri/prompt.md'\\'' and go'"
        );
    }

    #[test]
    fn extract_error_reads_message() {
        let raw = r#"{"error":{"code":"pane_not_found","message":"pane w4:p9 not found"},"id":"cli:pane:get"}"#;
        assert_eq!(extract_error(raw).as_deref(), Some("pane w4:p9 not found"));
    }
}
