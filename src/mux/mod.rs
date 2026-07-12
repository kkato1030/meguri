use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

pub mod herdr;
mod herdr_socket;
pub mod tmux;

pub mod fake;

/// Semantic agent state as seen through the multiplexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Agent is actively producing output / running tools.
    Working,
    /// Agent is at its input prompt, waiting.
    Idle,
    /// Agent is showing a permission/question UI and needs a human.
    Blocked,
    /// Agent reported completion (herdr-native only).
    Done,
    /// Cannot determine (pane gone, detection unavailable, ...).
    Unknown,
}

impl AgentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Blocked => "blocked",
            Self::Done => "done",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MuxCapabilities {
    /// True when the mux itself detects agent state (herdr manifests).
    /// When false, callers must treat state as a weak hint and rely on
    /// out-of-band signals (the result-file contract).
    pub native_agent_state: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MuxKind {
    Herdr,
    Tmux,
}

impl MuxKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Herdr => "herdr",
            Self::Tmux => "tmux",
        }
    }
}

/// Opaque pane handle (herdr terminal id, tmux pane id like "%3").
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneId(pub String);

impl std::fmt::Display for PaneId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque handle to a dashboard container that tiles several live agent panes
/// (herdr tab id like "wD:t4", tmux window id like "@5"). Used by `meguri top`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DashboardId(pub String);

impl std::fmt::Display for DashboardId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Direction to grow a tile when placing a pane into a dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Split {
    Right,
    Down,
}

#[derive(Debug, Clone)]
pub struct PaneSpec {
    /// Human-facing title/label for the pane.
    pub title: String,
    pub cwd: PathBuf,
    /// argv of the interactive agent process.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    #[error("pane not found: {0}")]
    PaneNotFound(PaneId),
    #[error("timed out waiting for agent state after {0:?}")]
    WaitTimeout(Duration),
    #[error("{kind} command failed: {detail}")]
    CommandFailed { kind: &'static str, detail: String },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type MuxResult<T> = Result<T, MuxError>;

#[async_trait]
pub trait Multiplexer: Send + Sync {
    fn kind(&self) -> MuxKind;
    fn capabilities(&self) -> MuxCapabilities;

    /// Make sure the meguri session/workspace exists.
    async fn ensure_session(&self) -> MuxResult<()>;

    /// Spawn an interactive agent pane; returns a handle that stays valid
    /// across meguri restarts (persisted in sqlite).
    async fn spawn_pane(&self, spec: &PaneSpec) -> MuxResult<PaneId>;

    async fn pane_alive(&self, pane: &PaneId) -> MuxResult<bool>;

    /// Type `text` into the pane and submit it (Enter).
    async fn send_line(&self, pane: &PaneId, text: &str) -> MuxResult<()>;

    /// Last `lines` lines of pane output (screen + recent history).
    async fn read_tail(&self, pane: &PaneId, lines: usize) -> MuxResult<Vec<String>>;

    async fn agent_state(&self, pane: &PaneId) -> MuxResult<AgentState>;

    /// Native session id of the agent CLI in the pane, when the mux can
    /// supply it (herdr carries it on `pane get` after the agent integration
    /// calls `pane report-agent-session`). Used to `--resume` the agent after
    /// its pane dies; muxes without the capability return None.
    async fn agent_session_id(&self, pane: &PaneId) -> MuxResult<Option<String>> {
        let _ = pane;
        Ok(None)
    }

    /// Wait until state ∈ `targets`, polling or via native events.
    /// Returns the matched state, or `WaitTimeout`.
    async fn wait_state(
        &self,
        pane: &PaneId,
        targets: &[AgentState],
        timeout: Duration,
    ) -> MuxResult<AgentState> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let state = self.agent_state(pane).await?;
            if targets.contains(&state) {
                return Ok(state);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(MuxError::WaitTimeout(timeout));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    async fn kill_pane(&self, pane: &PaneId) -> MuxResult<()>;

    /// Shell command a human runs to attach to this pane.
    fn attach_command(&self, pane: &PaneId) -> String;

    // --- Dashboard layout (`meguri top`, issue #96) -----------------------
    //
    // These only move panes between containers; they never touch the agent
    // process, so meguri keeps driving each pane by its `PaneId` regardless of
    // which tab/window it lives in.

    /// Ensure a dashboard container labeled `label` exists in the meguri
    /// session, returning its handle. Idempotent: an existing container with
    /// the same label is reused rather than duplicated.
    async fn ensure_dashboard(&self, label: &str) -> MuxResult<DashboardId>;

    /// Move a live agent pane into the dashboard, tiling it in `dir`. The
    /// pane's process is preserved (herdr `pane move`, tmux `join-pane`), so
    /// the orchestrator keeps driving it by id.
    async fn tile_pane(&self, pane: &PaneId, into: &DashboardId, dir: Split) -> MuxResult<()>;

    /// Shell command a human runs to view the dashboard container.
    fn dashboard_attach_command(&self, dashboard: &DashboardId) -> String;
}

/// Pick a multiplexer: explicit kind, else herdr if its socket is live, else tmux.
pub fn detect(kind_hint: &str, session: &str) -> anyhow::Result<std::sync::Arc<dyn Multiplexer>> {
    match kind_hint {
        "herdr" => Ok(std::sync::Arc::new(herdr::HerdrMux::new(session))),
        "tmux" => Ok(std::sync::Arc::new(tmux::TmuxMux::new(session))),
        "auto" => {
            if herdr::HerdrMux::socket_live() {
                Ok(std::sync::Arc::new(herdr::HerdrMux::new(session)))
            } else if which_ok("tmux") {
                Ok(std::sync::Arc::new(tmux::TmuxMux::new(session)))
            } else {
                anyhow::bail!("no usable multiplexer: start `herdr` or install tmux")
            }
        }
        other => anyhow::bail!("unknown mux kind {other:?} (use auto|herdr|tmux)"),
    }
}

/// Resolve a mux by the kind string persisted on a run record.
pub fn from_kind(kind: &str, session: &str) -> anyhow::Result<std::sync::Arc<dyn Multiplexer>> {
    match kind {
        "herdr" | "tmux" => detect(kind, session),
        other => anyhow::bail!("run has unknown mux kind {other:?}"),
    }
}

fn which_ok(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Blocked-UI patterns shared by tmux heuristics (herdr detects natively).
/// Deliberately strict: match only known approval/question UIs so that a
/// thinking-but-quiet agent reads as Idle, not Blocked.
pub(crate) const BLOCKED_PATTERNS: &[&str] = &[
    "Do you want to",
    "Do you trust the files",
    "❯ 1. Yes",
    "│ ❯ 1.",
    "(y/n)",
    "[y/N]",
    "[Y/n]",
    "Grant permission",
    "needs your permission",
    "Waiting for your approval",
];

pub(crate) fn tail_looks_blocked(tail: &[String]) -> bool {
    tail.iter()
        .any(|line| BLOCKED_PATTERNS.iter().any(|p| line.contains(p)))
}
