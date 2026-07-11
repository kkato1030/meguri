pub mod scheduler;
pub mod worker;

use std::sync::Arc;

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
