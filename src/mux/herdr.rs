use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::{
    AgentState, DashboardId, Multiplexer, MuxCapabilities, MuxError, MuxKind, MuxResult, PaneId,
    PaneSpec, Split,
    herdr_socket::{self, EventStream},
};

/// Delay between typing text and pressing Enter (paste-detection quirks).
const ENTER_DELAY: Duration = Duration::from_millis(300);

/// A watcher this quiet re-reads `pane.get`, bounding cache staleness when
/// events are dropped or missed.
const REVALIDATE_EVERY: Duration = Duration::from_secs(15);

/// Poll cadence for `wait_state`'s last resort, when both the event socket
/// and `herdr wait` are unusable (herdr down or restarting).
const FALLBACK_POLL: Duration = Duration::from_secs(2);

/// What the status cache knows about a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneStatus {
    Alive(AgentState),
    /// The pane no longer exists.
    Dead,
}

/// A live per-pane status subscription: the receiver serves cached reads,
/// the task feeds it from `events.subscribe`.
struct PaneWatch {
    rx: watch::Receiver<PaneStatus>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for PaneWatch {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// herdr-backed multiplexer, driven through the local socket API where
/// possible (no subprocess per call) with the `herdr` CLI as fallback.
///
/// Layout: one workspace per project, labeled `<session>:<project>` (the bare
/// `<session>` — no project — is the cross-project `meguri top` view). Issue
/// tabs land in their project's workspace; the label is chosen at construction
/// (`mux::detect`, per project). One tab per run. The agent is launched
/// *inside the tab's shell* (`pane run`), so the pane and its final screen
/// survive agent exit. Operations on an existing pane address it by its id
/// (`wN:pM`, which carries the workspace), so they never need the label.
///
/// Agent state is served from a per-pane cache fed by a
/// `pane.agent_status_changed` subscription, so the turn engine's poll loop
/// costs no subprocess spawns and `wait_state` reacts within milliseconds of
/// a transition instead of a poll interval.
pub struct HerdrMux {
    /// Workspace label this mux creates panes in — `<session>:<project>` for a
    /// project, or the bare `<session>` for the cross-project `meguri top` view.
    session: String,
    /// Unix socket for direct requests and event subscriptions.
    socket: PathBuf,
    /// Event-fed status watchers, keyed by pane id.
    watchers: tokio::sync::Mutex<HashMap<String, PaneWatch>>,
}

impl HerdrMux {
    pub fn new(session: &str) -> Self {
        Self::with_socket(session, Self::socket_path())
    }

    /// Like [`new`](Self::new) but against an explicit socket path
    /// (tests point this at a dead path to exercise fallbacks).
    pub fn with_socket(session: &str, socket: PathBuf) -> Self {
        Self {
            session: session.to_string(),
            socket,
            watchers: tokio::sync::Mutex::new(HashMap::new()),
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

    /// Get or create the event-fed status watcher for `pane`.
    ///
    /// An error means the socket is unusable for subscriptions (herdr down,
    /// or too old for `events.subscribe`) — callers fall back to the CLI.
    async fn watch(&self, pane: &PaneId) -> MuxResult<watch::Receiver<PaneStatus>> {
        let mut watchers = self.watchers.lock().await;
        if let Some(w) = watchers.get(&pane.0) {
            if !w.task.is_finished() {
                return Ok(w.rx.clone());
            }
            watchers.remove(&pane.0);
        }
        // Subscribe before seeding so no transition slips between the two.
        let stream = herdr_socket::subscribe_pane_events(&self.socket, &pane.0).await?;
        let seed = socket_pane_status(&self.socket, &pane.0).await?;
        let (tx, rx) = watch::channel(seed);
        if seed == PaneStatus::Dead {
            // Don't cache dead panes: ids may be reused after a herdr restart.
            return Ok(rx);
        }
        let task = tokio::spawn(watch_pane(stream, tx, self.socket.clone(), pane.0.clone()));
        watchers.insert(
            pane.0.clone(),
            PaneWatch {
                rx: rx.clone(),
                task,
            },
        );
        Ok(rx)
    }

    async fn drop_watcher(&self, pane: &PaneId) {
        self.watchers.lock().await.remove(&pane.0);
    }

    /// Race one `herdr wait agent-status` process per target; first match
    /// wins. `Ok(None)` is a clean herdr-reported timeout; `Err` means herdr
    /// was unusable and the caller should degrade to polling.
    async fn cli_wait(
        &self,
        pane: &PaneId,
        targets: &[AgentState],
        remaining: Duration,
    ) -> MuxResult<Option<AgentState>> {
        let timeout_ms = remaining.as_millis().to_string();
        let mut children = tokio::task::JoinSet::new();
        for &target in targets {
            let pane = pane.0.clone();
            let status = agent_status_arg(target).to_string();
            let timeout_ms = timeout_ms.clone();
            children.spawn(async move {
                let out = tokio::process::Command::new("herdr")
                    .args([
                        "wait",
                        "agent-status",
                        &pane,
                        "--status",
                        &status,
                        "--timeout",
                        &timeout_ms,
                    ])
                    .kill_on_drop(true)
                    .output()
                    .await;
                (target, out)
            });
        }
        let mut timed_out = false;
        let mut first_err: Option<MuxError> = None;
        while let Some(joined) = children.join_next().await {
            let Ok((target, out)) = joined else { continue };
            match out {
                // Dropping the set kills the still-waiting siblings.
                Ok(out) if out.status.success() => return Ok(Some(target)),
                Ok(out) => {
                    let text = format!(
                        "{}{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    if text.contains("timed out") {
                        timed_out = true;
                    } else {
                        first_err.get_or_insert(MuxError::CommandFailed {
                            kind: "herdr",
                            detail: extract_error(text.trim())
                                .unwrap_or_else(|| format!("herdr wait: {}", text.trim())),
                        });
                    }
                }
                Err(e) => {
                    first_err.get_or_insert(MuxError::Io(e));
                }
            }
        }
        match (first_err, timed_out) {
            (Some(e), _) => Err(e),
            (None, true) => Ok(None),
            (None, false) => Err(MuxError::Other("herdr wait returned no verdict".into())),
        }
    }
}

/// Current pane status via a one-shot socket `pane.get`.
async fn socket_pane_status(socket: &Path, pane_id: &str) -> MuxResult<PaneStatus> {
    match herdr_socket::request(socket, "pane.get", json!({ "pane_id": pane_id })).await {
        Ok(result) => {
            let status = result
                .pointer("/pane/agent_status")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            Ok(PaneStatus::Alive(map_agent_status(status)))
        }
        Err(MuxError::CommandFailed { detail, .. }) if detail.contains("not found") => {
            Ok(PaneStatus::Dead)
        }
        Err(e) => Err(e),
    }
}

/// Feed `tx` from the pane's event stream. Exits — dropping `tx`, which
/// invalidates the cache entry — when the stream dies, the socket stops
/// answering, or the pane is confirmed gone.
async fn watch_pane(
    mut stream: EventStream,
    tx: watch::Sender<PaneStatus>,
    socket: PathBuf,
    pane_id: String,
) {
    let publish = |status: PaneStatus| {
        tx.send_if_modified(|current| {
            let changed = *current != status;
            *current = status;
            changed
        });
    };
    loop {
        match tokio::time::timeout(REVALIDATE_EVERY, stream.next_event()).await {
            // Quiet for a while: revalidate against pane.get.
            Err(_) => match socket_pane_status(&socket, &pane_id).await {
                Ok(PaneStatus::Dead) => {
                    publish(PaneStatus::Dead);
                    return;
                }
                Ok(status) => publish(status),
                Err(_) => return,
            },
            Ok(Ok(Some((event, data)))) => {
                // herdr does not reliably filter lifecycle events by pane.
                if data.get("pane_id").and_then(Value::as_str) != Some(pane_id.as_str()) {
                    continue;
                }
                match event.as_str() {
                    "pane.agent_status_changed" => {
                        let status = data
                            .get("agent_status")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        publish(PaneStatus::Alive(map_agent_status(status)));
                    }
                    // Close/exit events can be stale replays from subscribe
                    // time — verify before declaring the pane dead.
                    "pane.closed" | "pane_closed" | "pane.exited" | "pane_exited" => {
                        match socket_pane_status(&socket, &pane_id).await {
                            Ok(PaneStatus::Dead) => {
                                publish(PaneStatus::Dead);
                                return;
                            }
                            Ok(status) => publish(status),
                            Err(_) => return,
                        }
                    }
                    _ => {}
                }
            }
            Ok(Ok(None)) | Ok(Err(_)) => return,
        }
    }
}

fn map_agent_status(status: &str) -> AgentState {
    match status {
        "working" => AgentState::Working,
        "idle" => AgentState::Idle,
        "blocked" => AgentState::Blocked,
        "done" => AgentState::Done,
        _ => AgentState::Unknown,
    }
}

/// The `--status` argument `herdr wait agent-status` expects.
fn agent_status_arg(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::Blocked => "blocked",
        AgentState::Done => "done",
        AgentState::Unknown => "unknown",
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
        let pane = PaneId(pane_id);
        // Defensive: a reused pane id must not inherit a stale watcher.
        self.drop_watcher(&pane).await;
        Ok(pane)
    }

    async fn pane_alive(&self, pane: &PaneId) -> MuxResult<bool> {
        if let Ok(rx) = self.watch(pane).await {
            return Ok(!matches!(*rx.borrow(), PaneStatus::Dead));
        }
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
        let raw = match herdr_socket::request(
            &self.socket,
            "pane.read",
            json!({ "pane_id": pane.0, "source": "visible", "lines": lines }),
        )
        .await
        {
            Ok(result) => result
                .pointer("/read/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            // Socket transport unusable → CLI (`pane read` emits plain text).
            Err(MuxError::Io(_)) => {
                let lines_arg = lines.to_string();
                self.herdr_raw(&[
                    "pane", "read", &pane.0, "--source", "visible", "--lines", &lines_arg,
                ])
                .await?
            }
            Err(e) => return Err(e),
        };
        let all: Vec<String> = raw.lines().map(str::to_string).collect();
        let skip = all.len().saturating_sub(lines);
        Ok(all[skip..].to_vec())
    }

    async fn agent_state(&self, pane: &PaneId) -> MuxResult<AgentState> {
        // Served from the event-fed cache: no subprocess, no round trip.
        if let Ok(rx) = self.watch(pane).await {
            return Ok(match *rx.borrow() {
                PaneStatus::Dead => AgentState::Unknown,
                PaneStatus::Alive(state) => state,
            });
        }
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
        Ok(map_agent_status(status))
    }

    /// Native session id as last reported to herdr (`pane
    /// report-agent-session`), carried on the pane object of `pane get`.
    /// A missing pane or an empty report reads as None, never an error.
    async fn agent_session_id(&self, pane: &PaneId) -> MuxResult<Option<String>> {
        let result =
            match herdr_socket::request(&self.socket, "pane.get", json!({ "pane_id": pane.0 }))
                .await
            {
                Ok(result) => result,
                // Socket transport unusable → CLI.
                Err(MuxError::Io(_)) => match self.herdr_json(&["pane", "get", &pane.0]).await {
                    Ok(result) => result,
                    Err(MuxError::CommandFailed { detail, .. }) if detail.contains("not found") => {
                        return Ok(None);
                    }
                    Err(e) => return Err(e),
                },
                Err(MuxError::CommandFailed { detail, .. }) if detail.contains("not found") => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            };
        Ok(result
            .pointer("/pane/agent_session_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string))
    }

    /// Native wait: the event subscription reacts within milliseconds of a
    /// transition; `herdr wait agent-status` (one racing process per target)
    /// covers a missing subscription; tolerant polling covers a dead herdr —
    /// which degrades to `WaitTimeout`, never an instant error.
    async fn wait_state(
        &self,
        pane: &PaneId,
        targets: &[AgentState],
        timeout: Duration,
    ) -> MuxResult<AgentState> {
        let deadline = tokio::time::Instant::now() + timeout;

        if let Ok(mut rx) = self.watch(pane).await {
            loop {
                let state = match *rx.borrow_and_update() {
                    PaneStatus::Dead => AgentState::Unknown,
                    PaneStatus::Alive(state) => state,
                };
                if targets.contains(&state) {
                    return Ok(state);
                }
                match tokio::time::timeout_at(deadline, rx.changed()).await {
                    Err(_) => return Err(MuxError::WaitTimeout(timeout)),
                    Ok(Ok(())) => {}
                    // Watcher died mid-wait: degrade to the CLI ladder.
                    Ok(Err(_)) => break,
                }
            }
        }

        loop {
            let now = tokio::time::Instant::now();
            let remaining = match deadline.checked_duration_since(now) {
                Some(d) if !d.is_zero() => d,
                _ => return Err(MuxError::WaitTimeout(timeout)),
            };
            match self.cli_wait(pane, targets, remaining).await {
                Ok(Some(state)) => return Ok(state),
                Ok(None) => return Err(MuxError::WaitTimeout(timeout)),
                Err(_) => {
                    // herdr unreachable: keep checking gently until the
                    // deadline in case it comes back.
                    tokio::time::sleep(FALLBACK_POLL.min(remaining)).await;
                    if let Ok(state) = self.agent_state(pane).await
                        && targets.contains(&state)
                    {
                        return Ok(state);
                    }
                }
            }
        }
    }

    async fn kill_pane(&self, pane: &PaneId) -> MuxResult<()> {
        self.drop_watcher(pane).await;
        self.herdr_ok(&["pane", "close", &pane.0]).await?;
        Ok(())
    }

    fn attach_command(&self, pane: &PaneId) -> String {
        let ws = pane.0.split(':').next().unwrap_or("").to_string();
        format!("herdr workspace focus {ws} >/dev/null 2>&1; herdr")
    }

    /// Reuse the dashboard tab with this label if one already exists in the
    /// meguri workspace, else create it. The tab's root pane doubles as the
    /// status header row (`meguri top` writes into the terminal it runs in).
    async fn ensure_dashboard(&self, label: &str) -> MuxResult<DashboardId> {
        let ws = self.workspace_id().await?;
        let tabs = self
            .herdr_json(&["tab", "list", "--workspace", &ws])
            .await?;
        if let Some(arr) = tabs.get("tabs").and_then(Value::as_array) {
            for tab in arr {
                if tab.get("label").and_then(Value::as_str) == Some(label)
                    && let Some(id) = tab.get("tab_id").and_then(Value::as_str)
                {
                    return Ok(DashboardId(id.to_string()));
                }
            }
        }
        let result = self
            .herdr_json(&[
                "tab",
                "create",
                "--workspace",
                &ws,
                "--label",
                label,
                "--no-focus",
            ])
            .await?;
        result
            .pointer("/tab/tab_id")
            .and_then(Value::as_str)
            .map(|s| DashboardId(s.to_string()))
            .ok_or_else(|| MuxError::CommandFailed {
                kind: "herdr",
                detail: format!("tab create returned no tab id: {result}"),
            })
    }

    async fn tile_pane(&self, pane: &PaneId, into: &DashboardId, dir: Split) -> MuxResult<()> {
        let split = match dir {
            Split::Right => "right",
            Split::Down => "down",
        };
        self.herdr_ok(&[
            "pane",
            "move",
            &pane.0,
            "--tab",
            &into.0,
            "--split",
            split,
            "--no-focus",
        ])
        .await
    }

    fn dashboard_attach_command(&self, dashboard: &DashboardId) -> String {
        let ws = dashboard.0.split(':').next().unwrap_or("");
        format!(
            "herdr workspace focus {ws} >/dev/null 2>&1; \
             herdr tab focus {tab} >/dev/null 2>&1; herdr",
            tab = dashboard.0
        )
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
    fn agent_status_round_trips_through_wait_args() {
        for state in [
            AgentState::Working,
            AgentState::Idle,
            AgentState::Blocked,
            AgentState::Done,
            AgentState::Unknown,
        ] {
            assert_eq!(map_agent_status(agent_status_arg(state)), state);
        }
        assert_eq!(map_agent_status("something-new"), AgentState::Unknown);
    }

    #[test]
    fn extract_error_reads_message() {
        let raw = r#"{"error":{"code":"pane_not_found","message":"pane w4:p9 not found"},"id":"cli:pane:get"}"#;
        assert_eq!(extract_error(raw).as_deref(), Some("pane w4:p9 not found"));
    }
}
