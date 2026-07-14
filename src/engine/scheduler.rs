//! The watch loop: startup recovery, per-loop discovery, slot-limited
//! dispatch. Loops discover targets (e.g. labeled GitHub issues); sqlite
//! only tracks runs, and `runs.loop_kind` routes each run to its loop.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tokio::task::JoinSet;

use super::{Deps, Loop};
use crate::mux::PaneId;
use crate::store::{RunRecord, RunStatus, Store};
use crate::tasks::TaskKey;

/// The slot budget is spent by *weight*, not run count (issue #111). A collab
/// advisor is a real agent on the subscription quota, so a run that actually
/// spawns one weighs 2; every other run weighs 1. This must use the same
/// `run_gets_advisor` predicate flow's `ensure_advisor` does — a run that gets
/// no advisor (e.g. a local task) must not book the extra slot.
fn run_weight(deps: &Deps, run: &RunRecord) -> usize {
    if crate::collab::run_gets_advisor(&deps.config, run) {
        2
    } else {
        1
    }
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
        // run_id → slot weight (issue #111): most runs weigh 1, a collab-advisor
        // run weighs 2. The budget is the sum, not the count.
        let mut active_run_ids: HashMap<String, usize> = HashMap::new();

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

            // Re-dispatch interrupted/queued runs before discovering new
            // work, every tick rather than only at watch startup (#183): a
            // pane that died mid-execute resumes from its checkpoint within
            // one poll_interval instead of staying stuck until the next
            // `meguri daemon restart`.
            if let Err(e) = self.redispatch_interrupted(&store, &mut running, &mut active_run_ids) {
                tracing::warn!("redispatch failed: {e:#}");
            }

            if active_weight(&active_run_ids) < self.max_concurrent
                && let Err(e) = self.discover(&mut running, &mut active_run_ids).await
            {
                tracing::warn!("discovery failed: {e:#}");
            }

            // Ride the poll: fire due cron schedules (issue #146). An
            // out-of-band enqueue like the sweeps below — it creates an
            // issue/task that the loops above discover next tick. `now` is
            // sampled once so every project's schedules see the same instant.
            let now = super::scheduler_fire::epoch_now();

            // Ride the poll: reclaim panes and worktrees whose issue closed
            // (the issue is the unit of lifetime — one author pane plus one
            // review pane per issue, kept until it closes; #13, #92).
            // Runs on the first tick too, i.e. as startup recovery.
            for deps in &self.projects {
                if let Err(e) = super::scheduler_fire::sweep(deps, now).await {
                    tracing::warn!("schedule sweep failed for {}: {e:#}", deps.project.id);
                }
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
                if let Err(e) = super::plan_handoff::sweep(deps).await {
                    tracing::warn!("handoff sweep failed for {}: {e:#}", deps.project.id);
                }
                // Ride the poll: recompute routing outcome drift from run
                // history and record any threshold crossing (routing 2/3,
                // #65). Pure sqlite, no pane, no API.
                if let Err(e) = super::routing_drift::sweep(deps) {
                    tracing::warn!("routing drift sweep failed for {}: {e:#}", deps.project.id);
                }
                // Notice body edits on already-shipped issues the label-filtered
                // discovery can no longer see (issue #142, half B) and leave a
                // re-attention signal. Light API sweep, no run record.
                if let Err(e) = super::reconcile::sweep(deps).await {
                    tracing::warn!("reconcile sweep failed for {}: {e:#}", deps.project.id);
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
        active: &mut HashMap<String, usize>,
    ) -> Result<()> {
        // Fresh per-tick cache: the fixer-family loops (fixer / ci_fixer /
        // conflict_resolver) below share one `list_open_prs` call per
        // project this tick instead of one each (issue #170).
        for deps in &self.projects {
            deps.open_prs.clear().await;
        }
        for lp in &self.loops {
            for deps in &self.projects {
                if active_weight(active) >= self.max_concurrent {
                    return Ok(());
                }
                let mut targets = lp.discover(deps).await?;
                // Sort by the coordination key: issue_number is no longer the
                // only identity (local tasks have none), so the key gives a
                // stable order across Issue/Local targets.
                targets.sort_by_key(|t| t.key);
                for target in targets {
                    if active_weight(active) >= self.max_concurrent {
                        return Ok(());
                    }
                    // Unique active run per (project, loop, target) — enforced
                    // by the partial DB indexes; a violation just means
                    // someone raced us. Run creation branches on the key so
                    // the target travels from discovery through claim.
                    let created = match target.key {
                        TaskKey::Issue(n) => deps.store.create_run_for_loop_cadence(
                            &deps.project.id,
                            lp.kind(),
                            n,
                            &target.title,
                            target.cadence_label.as_deref(),
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
                    // Admit by weight (issue #111): a collab-advisor run books 2
                    // slots, so start it only if it fits the budget. A run that
                    // doesn't fit stays `queued` for a later tick — head-of-line,
                    // so a heavy run isn't starved by lighter ones behind it.
                    if !self.admits(active, self.run_weight_for(&run)) {
                        return Ok(());
                    }
                    self.dispatch(&run, running, active);
                }
            }
        }
        Ok(())
    }

    /// Redispatch runs left `interrupted` (pane died mid-execute) or
    /// `queued` (never got a slot), respecting the slot budget. `active`
    /// also guards against double-dispatching a run this loop already
    /// spawned earlier in the same tick, or in a still-running previous
    /// tick, whose store status hasn't caught up to `running` yet.
    fn redispatch_interrupted(
        &self,
        store: &Store,
        running: &mut JoinSet<String>,
        active: &mut HashMap<String, usize>,
    ) -> Result<()> {
        for run in store.list_runs(true)? {
            if active_weight(active) >= self.max_concurrent {
                break;
            }
            if active.contains_key(&run.id) {
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
        active.insert(run_id.clone(), weight);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CollabConfig, CollabMode, Config, ProjectConfig};
    use crate::forge::fake::FakeForge;
    use crate::store::Store;

    fn deps_with_collab(mode: Option<CollabMode>) -> Deps {
        let mut config = Config::default();
        if let Some(mode) = mode {
            config.collab = Some(CollabConfig {
                mode,
                advisor_role: "planner".into(),
            });
        }
        let project = ProjectConfig {
            id: "proj".into(),
            repo_path: "/tmp/unused".into(),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            worktree_root: None,
            pr: None,
            clean: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup: Default::default(),
            schedules: Vec::new(),
            cadence: Vec::new(),
            prompts: Default::default(),
            triage: None,
            autonomy: None,
        };
        Deps::with_label_source(
            Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            Arc::new(FakeForge::default()),
            config,
            project,
        )
    }

    fn run_of_kind(deps: &Deps, loop_kind: &str) -> RunRecord {
        let run = deps
            .store
            .create_run_for_loop("proj", loop_kind, 7, "t")
            .unwrap();
        deps.store.get_run(&run.id).unwrap().unwrap()
    }

    fn local_run_of_kind(deps: &Deps, loop_kind: &str) -> RunRecord {
        let run = deps
            .store
            .create_run_for_task("proj", loop_kind, 42, "t")
            .unwrap();
        deps.store.get_run(&run.id).unwrap().unwrap()
    }

    #[test]
    fn active_weight_sums_weights() {
        let mut m = HashMap::new();
        m.insert("a".to_string(), 1usize);
        m.insert("b".to_string(), 2usize);
        assert_eq!(active_weight(&m), 3);
        assert_eq!(active_weight(&HashMap::new()), 0);
    }

    #[test]
    fn collab_advisor_run_weighs_two() {
        // With `[collab] mode = "advisor"`, an advisor-eligible run books two
        // slots (issue #111): the worker plus its advisor.
        let deps = deps_with_collab(Some(CollabMode::Advisor));
        assert_eq!(
            run_weight(&deps, &run_of_kind(&deps, crate::engine::worker::KIND)),
            2
        );
        assert_eq!(
            run_weight(&deps, &run_of_kind(&deps, crate::engine::spec_worker::KIND)),
            2
        );
        // A non-advisor loop still weighs 1 even with collab on.
        assert_eq!(run_weight(&deps, &run_of_kind(&deps, "planner")), 1);
        // A *local* worker run gets no advisor (no issue lane), so it must not
        // book the extra slot even with collab on.
        assert_eq!(
            run_weight(
                &deps,
                &local_run_of_kind(&deps, crate::engine::worker::KIND)
            ),
            1
        );
    }

    #[test]
    fn collab_off_every_run_weighs_one() {
        for mode in [None, Some(CollabMode::Off)] {
            let deps = deps_with_collab(mode);
            assert_eq!(
                run_weight(&deps, &run_of_kind(&deps, crate::engine::worker::KIND)),
                1
            );
        }
    }

    fn empty_scheduler(max: usize) -> Scheduler {
        Scheduler {
            projects: vec![],
            loops: vec![],
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

    #[test]
    fn admits_lets_a_lone_advisor_run_start_at_max_one() {
        // Criterion 8: a single collab-advisor run (weight 2) must start even
        // at max_concurrent = 1 (the idle-scheduler escape) …
        let s = empty_scheduler(1);
        assert!(s.admits(&active_map(&[]), 2));
        // … but nothing starts alongside it.
        assert!(!s.admits(&active_map(&[("a", 2)]), 1));
        assert!(!s.admits(&active_map(&[("a", 1)]), 1));
    }
}
