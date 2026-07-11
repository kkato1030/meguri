pub mod conflict_resolver;
pub mod fixer;
pub mod flow;
pub mod planner;
pub mod reaper;
pub mod reviewer;
pub mod scheduler;
pub mod spec_worker;
pub mod worker;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::{Config, ProjectConfig};
use crate::forge::Forge;
use crate::mux::Multiplexer;
use crate::store::{DesiredState, InteractionState, Store};
use crate::turn::TurnControl;

/// Everything a loop needs to drive runs for one project.
#[derive(Clone)]
pub struct Deps {
    pub store: Store,
    pub mux: Arc<dyn Multiplexer>,
    pub forge: Arc<dyn Forge>,
    pub config: Config,
    pub project: ProjectConfig,
}

/// A unit of work a loop wants a run for: the issue to drive.
#[derive(Debug, Clone)]
pub struct Target {
    pub issue_number: i64,
    pub title: String,
}

/// Terminal outcomes of driving one run (shared by every loop).
#[derive(Debug)]
pub enum WorkerOutcome {
    Succeeded {
        pr_url: String,
    },
    Stopped,
    Interrupted(String),
    /// Benign race: the issue was held or de-labeled between discovery and
    /// claim (e.g. another run already shipped it). No escalation.
    Skipped(String),
    /// The agent found a design decision must precede implementation; the
    /// issue was handed to the planner (issue #22). A normal ending, not a
    /// failure — the reason (agent's summary) is left as an issue comment.
    NeedsPlan(String),
    /// The planner split the issue into sub-issues instead of writing a spec
    /// (issue #24). The second normal planner ending — the rationale
    /// (agent's summary) is left as an issue comment.
    Decomposed(String),
}

/// A schedulable loop: discovers actionable targets for a project and drives
/// runs to a terminal outcome. `kind()` is persisted in `runs.loop_kind` so
/// the scheduler can route a run back to its loop after a restart.
#[async_trait]
pub trait Loop: Send + Sync {
    /// Stable identifier stored in `runs.loop_kind`.
    fn kind(&self) -> &'static str;

    /// Find targets that need a run for this project. Discovery must be
    /// idempotent: targets already covered by an active run are filtered by
    /// the scheduler via the unique (project, loop, issue) run index.
    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>>;

    /// Drive one run to a terminal outcome, resuming from its checkpoint.
    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome>;
}

/// The loops meguri ships today, in dispatch-priority order (the pipeline
/// reversed = closest to merge first). The scheduler hands out slots from
/// the head of this list, so ordering alone is the priority mechanism.
pub fn default_loops() -> Vec<Arc<dyn Loop>> {
    vec![
        Arc::new(conflict_resolver::ConflictResolverLoop),
        Arc::new(fixer::FixerLoop),
        Arc::new(spec_worker::SpecWorkerLoop),
        Arc::new(reviewer::ReviewerLoop),
        Arc::new(worker::WorkerLoop),
        Arc::new(planner::PlannerLoop),
    ]
}

/// TurnControl over the sqlite store: the CLI writes `desired_state`,
/// live turns converge to it and report state/events back.
pub struct StoreControl {
    pub store: Store,
    pub run_id: String,
}

#[async_trait]
impl TurnControl for StoreControl {
    async fn desired(&self) -> Option<DesiredState> {
        self.store.read_desired_state(&self.run_id).ok().flatten()
    }

    async fn set_interaction(&self, state: InteractionState) {
        let _ = self
            .store
            .update_interaction_state(&self.run_id, Some(state));
    }

    async fn event(&self, kind: &str, data: serde_json::Value) {
        let _ = self.store.emit(Some(&self.run_id), kind, data);
    }
}
