//! In-memory `Multiplexer` used by unit tests: the test controls the
//! reported agent state and inspects what the orchestrator sent.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::{
    AgentState, Multiplexer, MuxCapabilities, MuxError, MuxKind, MuxResult, PaneId, PaneSpec,
};

#[derive(Debug)]
pub struct FakePane {
    pub spec_title: String,
    pub sent_lines: Vec<String>,
    pub state: AgentState,
    pub alive: bool,
    pub tail: Vec<String>,
}

pub struct FakeMux {
    pub native_agent_state: bool,
    next_id: AtomicUsize,
    panes: Mutex<HashMap<PaneId, FakePane>>,
}

impl FakeMux {
    pub fn new(native_agent_state: bool) -> Self {
        Self {
            native_agent_state,
            next_id: AtomicUsize::new(1),
            panes: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_state(&self, pane: &PaneId, state: AgentState) {
        let mut panes = self.panes.lock().unwrap();
        if let Some(p) = panes.get_mut(pane) {
            p.state = state;
        }
    }

    pub fn set_tail(&self, pane: &PaneId, tail: Vec<String>) {
        let mut panes = self.panes.lock().unwrap();
        if let Some(p) = panes.get_mut(pane) {
            p.tail = tail;
        }
    }

    pub fn kill(&self, pane: &PaneId) {
        let mut panes = self.panes.lock().unwrap();
        if let Some(p) = panes.get_mut(pane) {
            p.alive = false;
        }
    }

    pub fn sent_lines(&self, pane: &PaneId) -> Vec<String> {
        self.panes
            .lock()
            .unwrap()
            .get(pane)
            .map(|p| p.sent_lines.clone())
            .unwrap_or_default()
    }

    pub fn pane_count(&self) -> usize {
        self.panes.lock().unwrap().len()
    }
}

#[async_trait]
impl Multiplexer for FakeMux {
    fn kind(&self) -> MuxKind {
        MuxKind::Tmux
    }

    fn capabilities(&self) -> MuxCapabilities {
        MuxCapabilities {
            native_agent_state: self.native_agent_state,
        }
    }

    async fn ensure_session(&self) -> MuxResult<()> {
        Ok(())
    }

    async fn spawn_pane(&self, spec: &PaneSpec) -> MuxResult<PaneId> {
        let id = PaneId(format!(
            "fake:{}",
            self.next_id.fetch_add(1, Ordering::SeqCst)
        ));
        self.panes.lock().unwrap().insert(
            id.clone(),
            FakePane {
                spec_title: spec.title.clone(),
                sent_lines: Vec::new(),
                state: AgentState::Working,
                alive: true,
                tail: Vec::new(),
            },
        );
        Ok(id)
    }

    async fn pane_alive(&self, pane: &PaneId) -> MuxResult<bool> {
        Ok(self
            .panes
            .lock()
            .unwrap()
            .get(pane)
            .map(|p| p.alive)
            .unwrap_or(false))
    }

    async fn send_line(&self, pane: &PaneId, text: &str) -> MuxResult<()> {
        let mut panes = self.panes.lock().unwrap();
        let p = panes
            .get_mut(pane)
            .ok_or_else(|| MuxError::PaneNotFound(pane.clone()))?;
        if !p.alive {
            return Err(MuxError::PaneNotFound(pane.clone()));
        }
        p.sent_lines.push(text.to_string());
        Ok(())
    }

    async fn read_tail(&self, pane: &PaneId, lines: usize) -> MuxResult<Vec<String>> {
        let panes = self.panes.lock().unwrap();
        let p = panes
            .get(pane)
            .ok_or_else(|| MuxError::PaneNotFound(pane.clone()))?;
        let skip = p.tail.len().saturating_sub(lines);
        Ok(p.tail[skip..].to_vec())
    }

    async fn agent_state(&self, pane: &PaneId) -> MuxResult<AgentState> {
        Ok(self
            .panes
            .lock()
            .unwrap()
            .get(pane)
            .map(|p| {
                if p.alive {
                    p.state
                } else {
                    AgentState::Unknown
                }
            })
            .unwrap_or(AgentState::Unknown))
    }

    async fn kill_pane(&self, pane: &PaneId) -> MuxResult<()> {
        self.kill(pane);
        Ok(())
    }

    fn attach_command(&self, pane: &PaneId) -> String {
        format!("echo fake pane {pane}")
    }
}
