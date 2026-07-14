//! The turn engine: drive one agent turn to completion, tolerating (and
//! expecting) human intervention at any point. Two executors share the same
//! completion authority — the result file (`.meguri/result.json` with a
//! matching turn id) — never the screen:
//!
//! - [`TurnEngine::await_completion`] (pane launch mode): the agent lives in
//!   a mux pane. Mux agent state refines behavior — Blocked pauses timers and
//!   pings a human; Working defers acceptance briefly; Idle feeds the
//!   stagnation clock that triggers nudges.
//! - [`TurnEngine::await_completion_direct`] (direct launch mode, issue
//!   #169): the agent is a plain non-interactive subprocess for exactly one
//!   turn. There is no screen to read, no nudging (nothing to type into), no
//!   Blocked state — the executor only waits for the process to exit, then
//!   reads the result file.

pub mod prompts;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use crate::mux::{AgentState, Multiplexer, PaneId};
use crate::store::{DesiredState, InteractionState};
pub use prompts::{ChildIssue, TurnResultFile, TurnStatus};

#[derive(Debug, Clone)]
pub struct TurnConfig {
    pub poll_interval: Duration,
    pub idle_grace: Duration,
    pub nudge_limit: u32,
    pub max_turn_runtime: Duration,
    pub result_grace: Duration,
}

impl TurnConfig {
    pub fn from_limits(limits: &crate::config::LimitsConfig) -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            idle_grace: Duration::from_secs(limits.idle_grace_secs),
            nudge_limit: limits.nudge_limit,
            max_turn_runtime: Duration::from_secs(limits.max_turn_runtime_secs),
            result_grace: Duration::from_secs(limits.result_grace_secs),
        }
    }
}

/// How the engine reports/receives run-level control while a turn is live.
/// Implemented over the sqlite store in production; faked in tests.
#[async_trait]
pub trait TurnControl: Send + Sync {
    /// Current control target set by CLI commands (pause/stop/takeover).
    async fn desired(&self) -> Option<DesiredState>;
    async fn set_interaction(&self, state: InteractionState);
    async fn event(&self, kind: &str, data: serde_json::Value);
}

#[derive(Debug)]
pub enum TurnOutcome {
    /// Agent wrote a matching result file.
    Completed(TurnResultFile),
    /// `meguri stop` was requested; caller cleans up.
    Stopped,
    /// The executor died before completing: a pane died (pane launch mode),
    /// or a direct subprocess exited without writing a matching result file
    /// (direct launch mode, issue #169). Both map to the same Interrupted
    /// handling upstream — callers never need to know which executor ran.
    PaneDied,
}

/// A prepared-but-not-yet-injected turn.
pub struct PreparedTurn {
    pub turn_id: String,
    pub prompt_path: PathBuf,
    pub trigger_line: String,
}

/// Write the prompt file + clear stale results. The caller then either
/// spawns the pane with the trigger as the agent's initial prompt argument
/// (turn 1) or sends the trigger line into the existing pane (later turns).
pub fn prepare_turn(worktree: &Path, prompt_body: &str, preamble: &str) -> Result<PreparedTurn> {
    let turn_id = uuid::Uuid::new_v4().to_string();
    let prompt_path = prompts::write_prompt_file(worktree, &turn_id, prompt_body, preamble)?;
    prompts::clear_result(worktree)?;
    Ok(PreparedTurn {
        trigger_line: prompts::trigger_line(&turn_id),
        turn_id,
        prompt_path,
    })
}

pub struct TurnEngine {
    pub mux: Arc<dyn Multiplexer>,
    pub cfg: TurnConfig,
}

impl TurnEngine {
    /// Wait for the turn identified by `turn_id` to complete in `pane`.
    ///
    /// Never fails the turn because of silence or human activity: quiet
    /// agents get nudged then escalated to a human; the only exits are a
    /// matching result file, an explicit stop, or the pane dying.
    pub async fn await_completion(
        &self,
        pane: &PaneId,
        worktree: &Path,
        turn_id: &str,
        control: &dyn TurnControl,
    ) -> Result<TurnOutcome> {
        let mut activity_clock = Duration::ZERO; // stagnation: time since last observed activity
        let mut runtime_clock = Duration::ZERO; // budget: time spent in agent_working
        let mut result_wait = Duration::ZERO; // time since result seen while still Working
        let mut last_tail_hash: Option<u64> = None;
        let mut nudges_sent: u32 = 0;
        let mut interaction = InteractionState::AgentWorking;
        let mut escalated = false;
        let mut blocked_notified = false;

        control
            .set_interaction(InteractionState::AgentWorking)
            .await;

        loop {
            // 1. Control channel wins over everything.
            match control.desired().await {
                Some(DesiredState::Stopped) => {
                    control
                        .event("turn.stopped", json!({ "turn_id": turn_id }))
                        .await;
                    return Ok(TurnOutcome::Stopped);
                }
                Some(DesiredState::Paused) => {
                    if interaction != InteractionState::Paused {
                        interaction = InteractionState::Paused;
                        control.set_interaction(interaction).await;
                        control
                            .event("turn.paused", json!({ "turn_id": turn_id }))
                            .await;
                    }
                    tokio::time::sleep(self.cfg.poll_interval).await;
                    continue;
                }
                Some(DesiredState::Takeover) => {
                    if interaction != InteractionState::HumanDriving {
                        interaction = InteractionState::HumanDriving;
                        control.set_interaction(interaction).await;
                        control
                            .event("turn.takeover", json!({ "turn_id": turn_id }))
                            .await;
                    }
                    // Hands off — but still honor a result the human-driven
                    // agent produces.
                    if let Some(result) = prompts::read_result(worktree, turn_id) {
                        return self.complete(control, turn_id, result).await;
                    }
                    tokio::time::sleep(self.cfg.poll_interval).await;
                    continue;
                }
                None => {
                    if matches!(
                        interaction,
                        InteractionState::Paused | InteractionState::HumanDriving
                    ) {
                        // Just resumed/handed back: restart the stagnation clock.
                        interaction = InteractionState::AgentWorking;
                        control.set_interaction(interaction).await;
                        control
                            .event("turn.resumed", json!({ "turn_id": turn_id }))
                            .await;
                        activity_clock = Duration::ZERO;
                    }
                }
            }

            // 2. Pane must exist (agent process may still be booting).
            if !self.mux.pane_alive(pane).await? {
                control
                    .event("turn.pane_died", json!({ "turn_id": turn_id }))
                    .await;
                return Ok(TurnOutcome::PaneDied);
            }

            let state = self.mux.agent_state(pane).await?;

            // 3. Result file is the completion authority.
            if let Some(result) = prompts::read_result(worktree, turn_id) {
                // If the agent is still visibly working, give it a moment to
                // finish trailing actions (it may amend commits etc.).
                if state == AgentState::Working && result_wait < self.cfg.result_grace {
                    result_wait += self.cfg.poll_interval;
                    tokio::time::sleep(self.cfg.poll_interval).await;
                    continue;
                }
                return self.complete(control, turn_id, result).await;
            }
            result_wait = Duration::ZERO;

            // 4. Observe activity: state or screen movement.
            let tail_hash = match self.mux.read_tail(pane, 30).await {
                Ok(tail) => Some(hash_lines(&tail)),
                Err(_) => None,
            };
            let screen_moved = match (tail_hash, last_tail_hash) {
                (Some(now), Some(before)) => now != before,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if tail_hash.is_some() {
                last_tail_hash = tail_hash;
            }

            match state {
                AgentState::Blocked => {
                    // A human must answer the agent's question. Suspend all
                    // timers; notify once per blockage.
                    if !blocked_notified {
                        blocked_notified = true;
                        interaction = InteractionState::AwaitingHuman;
                        control.set_interaction(interaction).await;
                        control
                            .event(
                                "turn.awaiting_human",
                                json!({
                                    "turn_id": turn_id,
                                    "reason": "agent_blocked",
                                    "attach": self.mux.attach_command(pane),
                                }),
                            )
                            .await;
                    }
                }
                AgentState::Working => {
                    blocked_notified = false;
                    activity_clock = Duration::ZERO;
                    runtime_clock += self.cfg.poll_interval;
                    if interaction != InteractionState::AgentWorking && !escalated {
                        interaction = InteractionState::AgentWorking;
                        control.set_interaction(interaction).await;
                    }
                }
                AgentState::Idle | AgentState::Done | AgentState::Unknown => {
                    blocked_notified = false;
                    if screen_moved {
                        activity_clock = Duration::ZERO;
                    } else {
                        activity_clock += self.cfg.poll_interval;
                    }
                    runtime_clock += self.cfg.poll_interval;
                }
            }

            // 5. Stagnation: nudge, then hand to a human. Never auto-fail.
            if state != AgentState::Blocked && !escalated && activity_clock >= self.cfg.idle_grace {
                if nudges_sent < self.cfg.nudge_limit {
                    // Only type into the pane when the agent is not mid-output
                    // and no human question is pending (checked above).
                    self.mux
                        .send_line(pane, &prompts::nudge_line(turn_id))
                        .await?;
                    nudges_sent += 1;
                    activity_clock = Duration::ZERO;
                    control
                        .event(
                            "turn.nudged",
                            json!({ "turn_id": turn_id, "nudge": nudges_sent }),
                        )
                        .await;
                } else {
                    escalated = true;
                    interaction = InteractionState::AwaitingHuman;
                    control.set_interaction(interaction).await;
                    control
                        .event(
                            "turn.awaiting_human",
                            json!({
                                "turn_id": turn_id,
                                "reason": "agent_quiet",
                                "attach": self.mux.attach_command(pane),
                            }),
                        )
                        .await;
                }
            }

            // 6. Runtime budget: escalate (don't kill).
            if !escalated && runtime_clock >= self.cfg.max_turn_runtime {
                escalated = true;
                interaction = InteractionState::AwaitingHuman;
                control.set_interaction(interaction).await;
                control
                    .event(
                        "turn.awaiting_human",
                        json!({
                            "turn_id": turn_id,
                            "reason": "runtime_budget_exceeded",
                            "attach": self.mux.attach_command(pane),
                        }),
                    )
                    .await;
            }

            tokio::time::sleep(self.cfg.poll_interval).await;
        }
    }

    /// Wait for a direct-mode turn (issue #169) to finish: watch `child`
    /// until it exits, then read the result file. There is no pane, so no
    /// agent-state polling and no nudging (nothing interactive to nudge);
    /// `Blocked` cannot happen (the CLI runs non-interactively, e.g.
    /// `claude -p`, and never shows a permission prompt). `meguri stop` kills
    /// the subprocess; `Paused`/`Takeover` have no pane to hand a human, so
    /// both just keep waiting.
    pub async fn await_completion_direct(
        &self,
        mut child: tokio::process::Child,
        worktree: &Path,
        turn_id: &str,
        control: &dyn TurnControl,
    ) -> Result<TurnOutcome> {
        control
            .set_interaction(InteractionState::AgentWorking)
            .await;

        let mut runtime_clock = Duration::ZERO;
        let mut escalated = false;

        loop {
            if let Some(DesiredState::Stopped) = control.desired().await {
                let _ = child.start_kill();
                control
                    .event("turn.stopped", json!({ "turn_id": turn_id }))
                    .await;
                return Ok(TurnOutcome::Stopped);
            }

            match child.try_wait() {
                Ok(Some(_status)) => {
                    if let Some(result) = prompts::read_result(worktree, turn_id) {
                        return self.complete(control, turn_id, result).await;
                    }
                    control
                        .event("turn.pane_died", json!({ "turn_id": turn_id }))
                        .await;
                    return Ok(TurnOutcome::PaneDied);
                }
                Ok(None) => {}
                Err(e) => return Err(e.into()),
            }

            runtime_clock += self.cfg.poll_interval;
            if !escalated && runtime_clock >= self.cfg.max_turn_runtime {
                escalated = true;
                control
                    .set_interaction(InteractionState::AwaitingHuman)
                    .await;
                control
                    .event(
                        "turn.awaiting_human",
                        json!({
                            "turn_id": turn_id,
                            "reason": "runtime_budget_exceeded",
                            "attach": "no pane (direct launch mode) — see `meguri ps`",
                        }),
                    )
                    .await;
            }

            tokio::time::sleep(self.cfg.poll_interval).await;
        }
    }

    async fn complete(
        &self,
        control: &dyn TurnControl,
        turn_id: &str,
        result: TurnResultFile,
    ) -> Result<TurnOutcome> {
        control
            .event(
                "turn.completed",
                json!({
                    "turn_id": turn_id,
                    "status": format!("{:?}", result.status),
                    "summary": result.summary,
                }),
            )
            .await;
        Ok(TurnOutcome::Completed(result))
    }
}

fn hash_lines(lines: &[String]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for line in lines {
        line.hash(&mut h);
    }
    h.finish()
}
