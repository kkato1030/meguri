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
use crate::tasks::TaskKey;

/// A fresh view of everything the watch derives from the config, produced by
/// the `reload` hook when `config.toml` changed on disk.
pub struct Reload {
    pub projects: Vec<Deps>,
    pub poll_interval: Duration,
    pub max_concurrent: usize,
}

pub struct Scheduler {
    /// One Deps per configured project (mux/store shared via clones).
    pub projects: Vec<Deps>,
    /// The loops to run; dispatch resolves a run's loop via `runs.loop_kind`.
    pub loops: Vec<Arc<dyn Loop>>,
    pub poll_interval: Duration,
    pub max_concurrent: usize,
    /// Config hot reload (issue #73), polled once per tick before discovery:
    /// `Some(_)` swaps the per-project Deps and the scheduler knobs, so every
    /// run spawned from that tick on sees the new config. Runs already
    /// dispatched keep the Deps they were spawned with — no retroactive
    /// application.
    pub reload: Option<Box<dyn FnMut() -> Option<Reload> + Send + Sync>>,
}

impl Scheduler {
    pub async fn watch(mut self) -> Result<()> {
        let mut store = self.projects[0].store.clone();
        self.recover(&store).await?;

        let mut running: JoinSet<String> = JoinSet::new();
        let mut active_run_ids: HashSet<String> = HashSet::new();

        // Re-dispatch interrupted runs before discovering new work.
        for run in store.list_runs(true)? {
            if run.status == RunStatus::Interrupted || run.status == RunStatus::Queued {
                self.dispatch(&run, &mut running, &mut active_run_ids);
            }
        }

        loop {
            // Pick up config edits before this tick's discovery, so a change
            // applies to every run spawned from here on.
            if let Some(reload) = self.reload.as_mut()
                && let Some(next) = reload()
            {
                self.projects = next.projects;
                self.poll_interval = next.poll_interval;
                self.max_concurrent = next.max_concurrent;
                store = self.projects[0].store.clone();
                tracing::info!(
                    projects = self.projects.len(),
                    poll_secs = self.poll_interval.as_secs(),
                    slots = self.max_concurrent,
                    "scheduler picked up reloaded config"
                );
            }

            // Liveness beacon for external readers (future `meguri top`).
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

            // Ride the poll: reclaim panes and worktrees whose issue closed
            // (the issue is the unit of lifetime — one author pane plus one
            // review pane per issue, kept until it closes; #13, #92).
            // Runs on the first tick too, i.e. as startup recovery.
            for deps in &self.projects {
                if let Err(e) = super::reaper::sweep(deps).await {
                    tracing::warn!("worktree sweep failed for {}: {e:#}", deps.project.id);
                }
                // Ride the poll: arm GitHub-native auto-merge on eligible PRs
                // (auto-merge 1/3, #41). Like the reaper, a light API sweep —
                // no run record, no pane.
                if let Err(e) = super::auto_merger::sweep(deps).await {
                    tracing::warn!("auto-merge sweep failed for {}: {e:#}", deps.project.id);
                }
                // Then watch the PRs it armed for drift GitHub silently stalled
                // (auto-merge 2/3, #42). After the arm sweep so a freshly armed
                // PR is seen once in the same tick.
                if let Err(e) = super::merge_watch::sweep(deps).await {
                    tracing::warn!("merge-watch sweep failed for {}: {e:#}", deps.project.id);
                }
                // Separate-mode plan→impl handoff (ADR 0008): a merged spec PR
                // flips its issue speccing → ready so the worker implements it.
                if let Err(e) = super::handoff::sweep(deps).await {
                    tracing::warn!("handoff sweep failed for {}: {e:#}", deps.project.id);
                }
                // Ride the poll: recompute routing outcome drift from run
                // history and record any threshold crossing (routing 2/3,
                // #65). Pure sqlite, no pane, no API.
                if let Err(e) = super::routing_drift::sweep(deps) {
                    tracing::warn!("routing drift sweep failed for {}: {e:#}", deps.project.id);
                }
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
    /// them, respecting the slot budget. Loops are visited in priority order
    /// (loop before project, so priority beats project order); within a loop,
    /// targets go oldest-first (FIFO by issue/PR number).
    async fn discover(
        &self,
        running: &mut JoinSet<String>,
        active: &mut HashSet<String>,
    ) -> Result<()> {
        for lp in &self.loops {
            for deps in &self.projects {
                if active.len() >= self.max_concurrent {
                    return Ok(());
                }
                let mut targets = lp.discover(deps).await?;
                // Sort by the coordination key: issue_number is no longer the
                // only identity (local tasks have none), so the key gives a
                // stable order across Issue/Local targets.
                targets.sort_by_key(|t| t.key);
                for target in targets {
                    if active.len() >= self.max_concurrent {
                        return Ok(());
                    }
                    // Unique active run per (project, loop, target) — enforced
                    // by the partial DB indexes; a violation just means
                    // someone raced us. Run creation branches on the key so
                    // the target travels from discovery through claim.
                    let created = match target.key {
                        TaskKey::Issue(n) => deps.store.create_run_for_loop(
                            &deps.project.id,
                            lp.kind(),
                            n,
                            &target.title,
                        ),
                        TaskKey::Local(id) => deps.store.create_run_for_task(
                            &deps.project.id,
                            lp.kind(),
                            id,
                            &target.title,
                        ),
                    };
                    let run = match created {
                        Ok(run) => run,
                        Err(_) => continue,
                    };
                    deps.store.emit(
                        Some(&run.id),
                        "run.discovered",
                        json!({ "key": format!("{:?}", target.key), "title": target.title,
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
                    // Only checks liveness by pane id — session-independent, so
                    // the base label (project = None) is sufficient.
                    match crate::mux::from_kind(kind, &self.projects[0].config.mux.session, None) {
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
