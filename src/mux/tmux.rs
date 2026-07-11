use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::{
    AgentState, Multiplexer, MuxCapabilities, MuxError, MuxKind, MuxResult, PaneId, PaneSpec,
    tail_looks_blocked,
};

/// How long the screen must stay unchanged before we call the agent Idle/Blocked.
const SETTLE_AFTER: Duration = Duration::from_secs(5);
/// Delay between typing text and pressing Enter (paste-detection quirks).
const ENTER_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, Clone)]
struct ScreenObservation {
    hash: u64,
    changed_at: Instant,
}

/// tmux-backed multiplexer. One meguri session; one window per run.
///
/// Agent state is a screen-stability heuristic — callers must treat it as a
/// hint (capabilities.native_agent_state == false) and rely on the result
/// file for completion.
pub struct TmuxMux {
    session: String,
    screens: Mutex<HashMap<PaneId, ScreenObservation>>,
}

impl TmuxMux {
    pub fn new(session: &str) -> Self {
        Self {
            session: session.to_string(),
            screens: Mutex::new(HashMap::new()),
        }
    }

    async fn tmux(&self, args: &[&str]) -> MuxResult<String> {
        let out = tokio::process::Command::new("tmux")
            .args(args)
            .output()
            .await?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
        } else {
            Err(MuxError::CommandFailed {
                kind: "tmux",
                detail: format!(
                    "tmux {} -> exit {}: {}",
                    args.join(" "),
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            })
        }
    }

    async fn capture_screen(&self, pane: &PaneId) -> MuxResult<String> {
        self.tmux(&["capture-pane", "-p", "-t", &pane.0]).await
    }
}

fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

#[async_trait]
impl Multiplexer for TmuxMux {
    fn kind(&self) -> MuxKind {
        MuxKind::Tmux
    }

    fn capabilities(&self) -> MuxCapabilities {
        MuxCapabilities {
            native_agent_state: false,
        }
    }

    async fn ensure_session(&self) -> MuxResult<()> {
        if self
            .tmux(&["has-session", "-t", &format!("={}", self.session)])
            .await
            .is_ok()
        {
            return Ok(());
        }
        // -x/-y: sane size for detached agents (default 80x24 truncates TUIs).
        self.tmux(&[
            "new-session",
            "-d",
            "-s",
            &self.session,
            "-x",
            "220",
            "-y",
            "50",
        ])
        .await?;
        Ok(())
    }

    async fn spawn_pane(&self, spec: &PaneSpec) -> MuxResult<PaneId> {
        self.ensure_session().await?;
        let cwd = spec.cwd.to_string_lossy().to_string();
        let mut args: Vec<String> = vec![
            "new-window".into(),
            "-t".into(),
            self.session.clone(),
            "-n".into(),
            spec.title.clone(),
            "-c".into(),
            cwd,
            "-P".into(),
            "-F".into(),
            "#{pane_id}".into(),
        ];
        for (k, v) in &spec.env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push("--".into());
        args.extend(spec.command.iter().cloned());
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let pane_id = self.tmux(&argv).await?;
        // remain-on-exit keeps the pane (with final screen) visible if the
        // agent process dies, instead of silently vanishing.
        let _ = self
            .tmux(&["set-option", "-p", "-t", &pane_id, "remain-on-exit", "on"])
            .await;
        Ok(PaneId(pane_id))
    }

    async fn pane_alive(&self, pane: &PaneId) -> MuxResult<bool> {
        match self
            .tmux(&["display-message", "-p", "-t", &pane.0, "#{pane_dead}"])
            .await
        {
            // "0" = alive; "1" = dead but kept by remain-on-exit;
            // "" = tmux expanded a missing pane to nothing (pane gone).
            Ok(dead) => Ok(dead.trim() == "0"),
            Err(_) => Ok(false),
        }
    }

    async fn send_line(&self, pane: &PaneId, text: &str) -> MuxResult<()> {
        self.tmux(&["send-keys", "-t", &pane.0, "-l", "--", text])
            .await?;
        tokio::time::sleep(ENTER_DELAY).await;
        self.tmux(&["send-keys", "-t", &pane.0, "Enter"]).await?;
        Ok(())
    }

    async fn read_tail(&self, pane: &PaneId, lines: usize) -> MuxResult<Vec<String>> {
        let start = format!("-{lines}");
        let out = self
            .tmux(&["capture-pane", "-p", "-t", &pane.0, "-S", &start])
            .await?;
        // Last N non-empty lines: the screen pads blank rows below the cursor
        // (and above status banners like "Pane is dead").
        let all: Vec<String> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_string)
            .collect();
        let skip = all.len().saturating_sub(lines);
        Ok(all[skip..].to_vec())
    }

    async fn agent_state(&self, pane: &PaneId) -> MuxResult<AgentState> {
        if !self.pane_alive(pane).await? {
            return Ok(AgentState::Unknown);
        }
        let screen = self.capture_screen(pane).await?;
        let now = Instant::now();
        let hash = hash_str(&screen);

        let settled = {
            let mut screens = self.screens.lock().unwrap();
            match screens.get_mut(pane) {
                Some(obs) if obs.hash == hash => now.duration_since(obs.changed_at) >= SETTLE_AFTER,
                Some(obs) => {
                    obs.hash = hash;
                    obs.changed_at = now;
                    false
                }
                None => {
                    screens.insert(
                        pane.clone(),
                        ScreenObservation {
                            hash,
                            changed_at: now,
                        },
                    );
                    false
                }
            }
        };

        if !settled {
            return Ok(AgentState::Working);
        }
        // Only lines near the cursor count: interactive TUIs render approval
        // dialogs at the bottom and redraw them away once answered.
        let tail: Vec<String> = {
            let mut lines: Vec<&str> = screen.lines().collect();
            while lines.last().is_some_and(|l| l.trim().is_empty()) {
                lines.pop();
            }
            lines.iter().rev().take(15).map(|s| s.to_string()).collect()
        };
        if tail_looks_blocked(&tail) {
            Ok(AgentState::Blocked)
        } else {
            Ok(AgentState::Idle)
        }
    }

    async fn kill_pane(&self, pane: &PaneId) -> MuxResult<()> {
        self.screens.lock().unwrap().remove(pane);
        self.tmux(&["kill-pane", "-t", &pane.0]).await?;
        Ok(())
    }

    fn attach_command(&self, pane: &PaneId) -> String {
        format!(
            "tmux select-window -t {pane} \\; attach -t {session}",
            pane = pane.0,
            session = self.session
        )
    }
}
