//! Consecutive-failure escalation for the watch loop's ride-the-poll sweeps
//! (issue #251, design doc P6.5).
//!
//! #227's unbalanced-brace GraphQL bug killed the merge-tail sweep every poll
//! for hours across every project; the only trace was a `tracing::warn!` —
//! invisible to both the issue tracker and the notify sink, so a human only
//! found it by getting suspicious that a green PR wasn't merging. This module
//! is the fix: every sweep failure is also a `sweep.failed` event (so `meguri
//! doctor` can read a failure rate straight from `events`, no new state
//! table), and a streak that persists past the configured threshold escalates
//! *once* — edge-triggered, like `schedule::DiagMemory` — to a
//! `sweep.degraded` event plus a notification. A later success emits
//! `sweep.recovered` and resets the streak.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::json;

use super::Deps;
use crate::notify::Notification;

/// Per-project, per-sweep consecutive failure streak. Lives in the watch
/// loop, not sqlite — a restart resets it, the same tradeoff
/// `schedule::DiagMemory` already makes for edge-triggered diagnostics.
#[derive(Default)]
pub struct SweepHealth(HashMap<(String, &'static str), u32>);

impl SweepHealth {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one sweep tick's outcome through the tracker. `name` identifies
    /// the sweep in events/logs (e.g. `"merge-tail"`); `outcome` is the
    /// `Result` the caller already got back from calling the sweep function.
    /// Never fails itself — a `sweep.failed`/`degraded`/`recovered` emit
    /// error is logged and swallowed, matching every other sweep's
    /// best-effort event emission.
    pub async fn record(&mut self, deps: &Deps, name: &'static str, outcome: Result<()>) {
        let key = (deps.project.id.clone(), name);
        let threshold = deps.config.scheduler.sweep_degraded_threshold;
        match outcome {
            Ok(()) => {
                // Edge-triggered recovery: only worth an event if this sweep
                // was actually degraded (streak had reached the threshold),
                // not on every quiet success.
                if let Some(streak) = self.0.remove(&key)
                    && streak >= threshold
                {
                    let _ = deps.store.emit(
                        None,
                        "sweep.recovered",
                        json!({ "project": deps.project.id, "sweep": name }),
                    );
                    tracing::info!(
                        project = deps.project.id,
                        sweep = name,
                        "sweep recovered after {streak} consecutive failures"
                    );
                }
            }
            Err(e) => {
                let detail = format!("{e:#}");
                tracing::warn!("{name} sweep failed for {}: {detail}", deps.project.id);
                let _ = deps.store.emit(
                    None,
                    "sweep.failed",
                    json!({ "project": deps.project.id, "sweep": name, "error": detail }),
                );
                let streak = self.0.entry(key).or_insert(0);
                *streak += 1;
                // `==` rather than `>=`: fires exactly once per outage (issue
                // #251 acceptance criterion), not on every failure once past
                // the threshold.
                if *streak == threshold {
                    let _ = deps.store.emit(
                        None,
                        "sweep.degraded",
                        json!({
                            "project": deps.project.id,
                            "sweep": name,
                            "consecutive_failures": *streak,
                            "error": detail,
                        }),
                    );
                    deps.notifier
                        .notify(&Notification::sweep_degraded(
                            &deps.project.id,
                            name,
                            *streak,
                            &detail,
                        ))
                        .await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::anyhow;

    use super::*;
    use crate::config::{Config, ProjectConfig};
    use crate::forge::fake::FakeForge;
    use crate::notify::fake::recording_notifier_with_events;
    use crate::store::Store;

    fn deps_with_threshold(threshold: u32, notifier: Arc<crate::notify::Notifier>) -> Deps {
        let mut config = Config::default();
        config.scheduler.sweep_degraded_threshold = threshold;
        let project = ProjectConfig {
            id: "proj".into(),
            repo_path: Some("/tmp/unused".into()),
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
            notify: None,
            triage: None,
            autonomy: None,
        };
        let mut deps = Deps::with_label_source(
            Store::open_in_memory().unwrap(),
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            Arc::new(FakeForge::default()),
            config,
            project,
        );
        deps.notifier = notifier;
        deps
    }

    #[tokio::test]
    async fn escalates_exactly_once_after_k_consecutive_failures() {
        let (notifier, gw) = recording_notifier_with_events(&["sweep.degraded"]);
        let deps = deps_with_threshold(3, notifier);
        let mut health = SweepHealth::new();

        for _ in 0..2 {
            health
                .record(&deps, "merge-tail", Err(anyhow!("boom")))
                .await;
        }
        assert_eq!(
            gw.delivered().len(),
            0,
            "no escalation before the threshold"
        );

        health
            .record(&deps, "merge-tail", Err(anyhow!("boom")))
            .await;
        assert_eq!(gw.delivered().len(), 1, "escalates on the K-th failure");

        // Idempotent: further consecutive failures must not re-notify.
        for _ in 0..5 {
            health
                .record(&deps, "merge-tail", Err(anyhow!("boom")))
                .await;
        }
        assert_eq!(gw.delivered().len(), 1, "escalation fires exactly once");

        assert_eq!(
            deps.store.count_events("sweep.degraded").unwrap(),
            1,
            "sweep.degraded event also fires exactly once"
        );
        assert_eq!(
            deps.store.count_events("sweep.failed").unwrap(),
            8,
            "every failure still gets its own sweep.failed event"
        );
    }

    #[tokio::test]
    async fn a_success_resets_the_streak() {
        let (notifier, gw) = recording_notifier_with_events(&["sweep.degraded"]);
        let deps = deps_with_threshold(3, notifier);
        let mut health = SweepHealth::new();

        health
            .record(&deps, "merge-tail", Err(anyhow!("boom")))
            .await;
        health.record(&deps, "merge-tail", Ok(())).await;
        health
            .record(&deps, "merge-tail", Err(anyhow!("boom")))
            .await;
        health
            .record(&deps, "merge-tail", Err(anyhow!("boom")))
            .await;
        assert_eq!(
            gw.delivered().len(),
            0,
            "the intervening success should reset the streak, not just delay escalation"
        );
    }

    #[tokio::test]
    async fn recovery_after_degraded_emits_sweep_recovered() {
        let (notifier, gw) = recording_notifier_with_events(&["sweep.degraded"]);
        let deps = deps_with_threshold(2, notifier);
        let mut health = SweepHealth::new();

        health
            .record(&deps, "merge-tail", Err(anyhow!("boom")))
            .await;
        health
            .record(&deps, "merge-tail", Err(anyhow!("boom")))
            .await;
        assert_eq!(gw.delivered().len(), 1);

        health.record(&deps, "merge-tail", Ok(())).await;
        assert_eq!(deps.store.count_events("sweep.recovered").unwrap(), 1);
    }

    #[tokio::test]
    async fn distinct_sweeps_and_projects_track_independent_streaks() {
        let (notifier, gw) = recording_notifier_with_events(&["sweep.degraded"]);
        let deps = deps_with_threshold(2, notifier);
        let mut health = SweepHealth::new();

        health
            .record(&deps, "merge-tail", Err(anyhow!("boom")))
            .await;
        health.record(&deps, "reaper", Err(anyhow!("boom"))).await;
        // Neither sweep alone has reached the threshold of 2 yet.
        assert_eq!(gw.delivered().len(), 0);
    }
}
