//! The watch loop: startup recovery, GitHub discovery, slot-limited
//! dispatch. GitHub issues (labels) are the queue; sqlite only tracks runs.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::task::JoinSet;

use super::{Deps, worker};
use crate::forge;
use crate::mux::PaneId;
use crate::store::{RunStatus, Store};

pub struct Scheduler {
    /// One Deps per configured project (mux/store shared via clones).
    pub projects: Vec<Deps>,
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
                self.dispatch(&run.id, &run.project_id, &mut running, &mut active_run_ids);
            }
        }

        loop {
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

    /// Find `meguri:ready` issues (not held, not claimed) with no active run
    /// and enqueue them, respecting the slot budget.
    async fn discover(
        &self,
        running: &mut JoinSet<String>,
        active: &mut HashSet<String>,
    ) -> Result<()> {
        for deps in &self.projects {
            if active.len() >= self.max_concurrent {
                return Ok(());
            }
            let issues = deps
                .forge
                .list_issues_with_label(forge::LABEL_READY)
                .await?;
            for issue in issues {
                if active.len() >= self.max_concurrent {
                    return Ok(());
                }
                if issue.has_label(forge::LABEL_HOLD) || issue.has_label(forge::LABEL_WORKING) {
                    continue;
                }
                // Unique active run per (project, worker, issue) — enforced
                // by the DB index; a violation just means someone raced us.
                let run = match deps
                    .store
                    .create_run(&deps.project.id, issue.number, &issue.title)
                {
                    Ok(run) => run,
                    Err(_) => continue,
                };
                deps.store.emit(
                    Some(&run.id),
                    "run.discovered",
                    json!({ "issue": issue.number, "title": issue.title }),
                )?;
                self.dispatch(&run.id, &deps.project.id, running, active);
            }
        }
        Ok(())
    }

    fn dispatch(
        &self,
        run_id: &str,
        project_id: &str,
        running: &mut JoinSet<String>,
        active: &mut HashSet<String>,
    ) {
        let Some(deps) = self
            .projects
            .iter()
            .find(|d| d.project.id == project_id)
            .cloned()
        else {
            tracing::warn!("run {run_id} references unknown project {project_id}");
            return;
        };
        let run_id = run_id.to_string();
        active.insert(run_id.clone());
        running.spawn(async move {
            if let Err(e) = worker::run_worker(&deps, &run_id).await {
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
