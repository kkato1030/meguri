//! The watch loop: startup recovery, per-loop discovery, slot-limited
//! dispatch. Loops discover targets (e.g. labeled GitHub issues); sqlite
//! only tracks runs, and `runs.loop_kind` routes each run to its loop.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::task::JoinSet;

use super::{Deps, Loop};
use crate::mux::PaneId;
use crate::store::{RunRecord, RunStatus, Store};

pub struct Scheduler {
    /// One Deps per configured project (mux/store shared via clones).
    pub projects: Vec<Deps>,
    /// The loops to run; dispatch resolves a run's loop via `runs.loop_kind`.
    pub loops: Vec<Arc<dyn Loop>>,
    pub poll_interval: Duration,
    pub max_concurrent: usize,
}

impl Scheduler {
    pub async fn watch(&self) -> Result<()> {
        let store = &self.projects[0].store;
        self.recover(store).await?;

        let mut running: JoinSet<String> = JoinSet::new();
        let mut active_run_ids: HashSet<String> = HashSet::new();

        // Re-dispatch interrupted runs before discovering new work.
        for run in store.list_runs(true)? {
            if run.status == RunStatus::Interrupted || run.status == RunStatus::Queued {
                self.dispatch(&run, &mut running, &mut active_run_ids);
            }
        }

        loop {
            // Liveness beacon for external readers (`meguri serve`).
            if let Err(e) = store.heartbeat("watch") {
                tracing::warn!("heartbeat failed: {e:#}");
            }

            // Reap finished drivers.
            while let Some(res) = running.try_join_next() {
                if let Ok(run_id) = res {
                    active_run_ids.remove(&run_id);
                }
            }

            if active_run_ids.len() < self.max_concurrent
                && let Err(e) = self.discover(&mut running, &mut active_run_ids).await
            {
                tracing::warn!("discovery failed: {e:#}");
            }

            tokio::select! {
                _ = tokio::time::sleep(self.poll_interval) => {}
                Some(res) = running.join_next(), if !running.is_empty() => {
                    if let Ok(run_id) = res {
                        active_run_ids.remove(&run_id);
                    }
                }
            }
        }
    }

    /// Ask every loop for actionable targets in every project and enqueue
    /// them, respecting the slot budget.
    async fn discover(
        &self,
        running: &mut JoinSet<String>,
        active: &mut HashSet<String>,
    ) -> Result<()> {
        for deps in &self.projects {
            for lp in &self.loops {
                if active.len() >= self.max_concurrent {
                    return Ok(());
                }
                for target in lp.discover(deps).await? {
                    if active.len() >= self.max_concurrent {
                        return Ok(());
                    }
                    // Unique active run per (project, loop, issue) — enforced
                    // by the DB index; a violation just means someone raced us.
                    let run = match deps.store.create_run_for_loop(
                        &deps.project.id,
                        lp.kind(),
                        target.issue_number,
                        &target.title,
                    ) {
                        Ok(run) => run,
                        Err(_) => continue,
                    };
                    deps.store.emit(
                        Some(&run.id),
                        "run.discovered",
                        json!({ "issue": target.issue_number, "title": target.title,
                                "loop": lp.kind() }),
                    )?;
                    self.dispatch(&run, running, active);
                }
            }
        }
        Ok(())
    }

    fn dispatch(
        &self,
        run: &RunRecord,
        running: &mut JoinSet<String>,
        active: &mut HashSet<String>,
    ) {
        let Some(deps) = self
            .projects
            .iter()
            .find(|d| d.project.id == run.project_id)
            .cloned()
        else {
            tracing::warn!(
                "run {} references unknown project {}",
                run.id,
                run.project_id
            );
            return;
        };
        let Some(lp) = self
            .loops
            .iter()
            .find(|l| l.kind() == run.loop_kind)
            .cloned()
        else {
            tracing::warn!("run {} references unknown loop {}", run.id, run.loop_kind);
            return;
        };
        let run_id = run.id.clone();
        active.insert(run_id.clone());
        running.spawn(async move {
            if let Err(e) = lp.drive(&deps, &run_id).await {
                tracing::warn!("run {run_id} failed: {e:#}");
            }
            run_id
        });
    }

    /// Startup recovery: every run left `running` by a dead orchestrator is
    /// re-adopted when its pane is alive, or parked as interrupted so the
    /// dispatch pass above resumes it from its checkpoint.
    async fn recover(&self, store: &Store) -> Result<()> {
        for run in store.list_runs(true)? {
            if run.status != RunStatus::Running {
                continue;
            }
            let pane_alive = match (&run.mux_kind, &run.mux_pane_id) {
                (Some(kind), Some(pane)) => {
                    match crate::mux::from_kind(kind, &self.projects[0].config.mux.session) {
                        Ok(mux) => mux.pane_alive(&PaneId(pane.clone())).await.unwrap_or(false),
                        Err(_) => false,
                    }
                }
                _ => false,
            };
            store.update_run_status(
                &run.id,
                RunStatus::Interrupted,
                Some("orchestrator restarted"),
            )?;
            store.emit(
                Some(&run.id),
                "run.recovered",
                json!({ "pane_alive": pane_alive, "step": run.step }),
            )?;
            tracing::info!(
                run = run.id,
                pane_alive,
                step = run.step,
                "recovered interrupted run"
            );
        }
        Ok(())
    }
}
