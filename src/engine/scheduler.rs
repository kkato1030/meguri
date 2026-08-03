//! The watch loop: startup recovery, level-triggered reconcile passes, and
//! slot-limited dispatch (ADR 0012). Enqueue is owned entirely by the
//! reconcilers (issue / repo / schedule Kind); sqlite tracks runs, and
//! `runs.loop_kind` routes each queued run to its recipe (決定8).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::task::JoinSet;

use super::Deps;
use crate::mux::PaneId;
use crate::store::{RunRecord, RunStatus, Store};

/// The slot budget is spent by *weight*, not run count (issue #111, #214). Two
/// phases spawn extra concurrent agents:
///
/// Every run books one slot.
fn run_weight(_deps: &Deps, _run: &RunRecord) -> usize {
    1
}

fn active_weight(active: &HashMap<String, usize>) -> usize {
    active.values().sum()
}

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
    /// The recipe dispatcher (ADR 0012 §決定8): routes each run to its
    /// `run_*` entry by `loop_kind`. Production is `default_recipe()`; tests
    /// inject recording recipes.
    pub recipe: super::RecipeFn,
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
        // run_id → slot weight (every run books one slot).
        let mut active_run_ids: HashMap<String, usize> = HashMap::new();
        // Last GitHub intake per project (権威反転): the label read runs on
        // its own, slower cadence. Absent = run it on the first tick.
        let mut last_intake: HashMap<String, std::time::Instant> = HashMap::new();

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

            // Materialize any declared-but-missing managed clones BEFORE anything
            // touches `repo_path` (ADR 0018). Must precede redispatch, discover,
            // AND the sweeps: redispatch runs before discover, discover is
            // skipped when slots are full, and the sweeps touch `repo_path`
            // outside discover — so a hook placed in any one of them would leave
            // a window where an un-cloned project is processed. A project whose
            // clone can't be materialized is excluded from this whole tick and
            // retried next tick.
            let ready = self.ensure_projects_ready().await;

            // Re-dispatch interrupted/queued runs before discovering new
            // work, every tick rather than only at watch startup (#183): a
            // pane that died mid-execute resumes from its checkpoint within
            // one poll_interval instead of staying stuck until the next
            // `meguri daemon restart`.
            if let Err(e) =
                self.redispatch_interrupted(&store, &ready, &mut running, &mut active_run_ids)
            {
                tracing::warn!("redispatch failed: {e:#}");
            }

            // Ride the poll: reclaim panes and worktrees whose issue closed
            // (the issue is the unit of lifetime — one author pane plus one
            // review pane per issue, kept until it closes; #13, #92).
            // Runs on the first tick too, i.e. as startup recovery.
            for deps in &self.projects {
                // Skip a project whose managed clone isn't ready this tick (the
                // sweeps below touch `repo_path`); it retries next tick.
                if !ready.contains(&deps.project.id) {
                    continue;
                }
                // The Issue Kind reconcile pass (ADR 0012 S4): the merge tail
                // plus every folded act/arm — Op(Finalize) and the
                // issue-/local-side deciders' enqueue — one level-triggered
                // pass per project.
                // GitHub intake on its own cadence (権威反転): labels → task
                // rows, the only recurring GitHub reads.
                let intake_due = last_intake.get(&deps.project.id).is_none_or(|t| {
                    t.elapsed().as_secs() >= deps.config.scheduler.intake_interval_secs
                });
                if intake_due {
                    if let Err(e) = super::issue_reconciler::intake(deps).await {
                        tracing::warn!("intake failed for {}: {e:#}", deps.project.id);
                    }
                    last_intake.insert(deps.project.id.clone(), std::time::Instant::now());
                }
                if let Err(e) = super::issue_reconciler::sweep(deps).await {
                    tracing::warn!("task reconcile failed for {}: {e:#}", deps.project.id);
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

    /// Redispatch runs left `interrupted` (pane died mid-execute) or
    /// `queued` (never got a slot), respecting the slot budget. `active`
    /// also guards against double-dispatching a run this loop already
    /// spawned earlier in the same tick, or in a still-running previous
    /// tick, whose store status hasn't caught up to `running` yet.
    /// Materialize declared-but-missing managed clones and return the set of
    /// project ids ready to process this tick, via the Repo Kind reconcile's
    /// first Op (ADR 0012 §決定6): `repo_reconciler::reconcile_ready` observes
    /// the clone health, runs `Op(EnsureClone)` when needed, and reports
    /// readiness. A project whose clone can't be materialized is excluded (the
    /// `repo.clone.failed` event / warn are emitted inside `reconcile_ready`)
    /// and retried next tick. This replaces the old scheduler-specific bootstrap
    /// gate with the same readiness contract every Kind consumes.
    async fn ensure_projects_ready(&self) -> HashSet<String> {
        let mut ready = HashSet::with_capacity(self.projects.len());
        for deps in &self.projects {
            if super::repo_reconciler::reconcile_ready(deps).await {
                ready.insert(deps.project.id.clone());
            }
        }
        ready
    }

    fn redispatch_interrupted(
        &self,
        store: &Store,
        ready: &HashSet<String>,
        running: &mut JoinSet<String>,
        active: &mut HashMap<String, usize>,
    ) -> Result<()> {
        // The workqueue's activeQ order (ADR 0012 §5): dispatch `queued` runs by
        // merge-proximity `dispatch_rank` (then issue number, FIFO) rather than
        // by creation order, so the reconciler's fixer-family runs — created in
        // the sweep, outside discovery — get their priority. Head-of-line
        // admission (the `break` below) then applies to the highest-priority run.
        let mut runs = store.list_runs(true)?;
        runs.sort_by_key(|r| (super::dispatch_rank(&r.loop_kind), r.issue_number));
        for run in runs {
            if active_weight(active) >= self.max_concurrent {
                break;
            }
            if active.contains_key(&run.id) {
                continue;
            }
            // Don't resume a run whose managed clone isn't ready this tick.
            if !ready.contains(&run.project_id) {
                continue;
            }
            if run.status == RunStatus::Interrupted || run.status == RunStatus::Queued {
                // Same weighted admission as discovery (issue #111): don't
                // resume a heavy run until it fits. Stop at the first that
                // doesn't, so it isn't skipped over by lighter runs behind it.
                if !self.admits(active, self.run_weight_for(&run)) {
                    break;
                }
                self.dispatch(&run, running, active);
            }
        }
        Ok(())
    }

    /// The run's slot weight (issue #111), or 1 when its project is unknown
    /// (that run can't be dispatched anyway — `dispatch` warns and skips it).
    fn run_weight_for(&self, run: &RunRecord) -> usize {
        self.projects
            .iter()
            .find(|d| d.project.id == run.project_id)
            .map(|d| run_weight(d, run))
            .unwrap_or(1)
    }

    /// Whether a run of `weight` can start now without over-spending the slot
    /// budget (issue #111). One escape: a run always starts on an idle
    /// scheduler, so a weight-2 collab-advisor run is not deadlocked at
    /// `max_concurrent = 1` (criterion 8). Otherwise the budget is hard
    /// (`active + weight <= max`) — never the "+1 slack" that would let an
    /// advisor run over-subscribe a busy scheduler.
    fn admits(&self, active: &HashMap<String, usize>, weight: usize) -> bool {
        let current = active_weight(active);
        current == 0 || current + weight <= self.max_concurrent
    }

    fn dispatch(
        &self,
        run: &RunRecord,
        running: &mut JoinSet<String>,
        active: &mut HashMap<String, usize>,
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
        let weight = run_weight(&deps, run);
        let run_id = run.id.clone();
        let loop_kind = run.loop_kind.clone();

        // ADR 0012 §決定8: dispatch is a pure kind→recipe map.
        let recipe = self.recipe.clone();
        active.insert(run_id.clone(), weight);
        running.spawn(async move {
            if let Err(e) = recipe(deps, run_id.clone(), loop_kind).await {
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

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn active_weight_sums_weights() {
        let mut m = HashMap::new();
        m.insert("a".to_string(), 1usize);
        m.insert("b".to_string(), 2usize);
        assert_eq!(active_weight(&m), 3);
        assert_eq!(active_weight(&HashMap::new()), 0);
    }

    fn empty_scheduler(max: usize) -> Scheduler {
        Scheduler {
            projects: vec![],
            recipe: super::super::default_recipe(),
            poll_interval: Duration::from_secs(1),
            max_concurrent: max,
            reload: None,
        }
    }

    fn active_map(weights: &[(&str, usize)]) -> HashMap<String, usize> {
        weights.iter().map(|(k, w)| (k.to_string(), *w)).collect()
    }

    #[test]
    fn admits_enforces_the_weighted_budget() {
        // This is the gate discover/redispatch use before every dispatch, so a
        // weight-2 collab-advisor run cannot over-subscribe the slot budget.
        let s = empty_scheduler(2);
        // Idle scheduler: a weight-2 advisor run fits exactly.
        assert!(s.admits(&active_map(&[]), 2));
        // One normal run active (weight 1): a weight-2 advisor run would push
        // the total to 3 — rejected (the over-subscription the review caught).
        assert!(!s.admits(&active_map(&[("a", 1)]), 2));
        // …but a weight-1 run still fits (1 + 1 = 2).
        assert!(s.admits(&active_map(&[("a", 1)]), 1));
        // Full: nothing more admits.
        assert!(!s.admits(&active_map(&[("a", 2)]), 1));
    }
}
