//! Outcome-based routing drift detection (routing 2/3, issue #65).
//!
//! Rides the watch poll like the reaper / auto-merger sweeps. For each
//! `(role, profile)` with enough history, it compares the most recent window
//! of scored runs against the preceding one and, when a regression crosses the
//! `[drift]` thresholds, records it in the `routing_drift` state table. The
//! read side (doctor / top / `meguri stats routing`) reads that table scoped
//! by project; here we only detect and journal transitions.

use anyhow::Result;
use serde_json::json;

use super::Deps;
use crate::config::DriftConfig;
use crate::store::{DriftSample, DriftTransition, WindowAgg};

/// Whether a group regressed past the thresholds: success rate fell by at
/// least `success_rate_drop_pt` points, OR mean turns rose by at least
/// `turns_increase_pct` percent (a zero previous baseline can't yield a
/// percentage and is ignored for the turns criterion).
pub fn is_drifting(sample: &DriftSample, cfg: &DriftConfig) -> bool {
    let success_dropped =
        sample.previous.success_rate - sample.recent.success_rate >= cfg.success_rate_drop_pt;
    let turns_increased = sample.previous.avg_turns > 0.0
        && (sample.recent.avg_turns - sample.previous.avg_turns) / sample.previous.avg_turns
            * 100.0
            >= cfg.turns_increase_pct;
    success_dropped || turns_increased
}

fn metrics_payload(recent: &WindowAgg, previous: &WindowAgg) -> serde_json::Value {
    json!({
        "recent": recent,
        "previous": previous,
    })
}

/// One drift sweep for a project: detect, persist current state, and emit an
/// event only when a `(role, profile)` crosses into or out of drift.
pub fn sweep(deps: &Deps) -> Result<()> {
    let cfg = &deps.config.drift;
    let samples = deps
        .store
        .routing_drift_samples(Some(&deps.project.id), cfg.window)?;

    for sample in &samples {
        let active = is_drifting(sample, cfg);
        let metrics = metrics_payload(&sample.recent, &sample.previous);
        let transition = deps.store.record_drift(
            &sample.project_id,
            &sample.loop_kind,
            &sample.agent_profile,
            active,
            &metrics.to_string(),
        )?;

        match transition {
            DriftTransition::BecameActive => {
                deps.store.emit(
                    None,
                    "routing.drift",
                    json!({
                        "project_id": sample.project_id,
                        "role": sample.loop_kind,
                        "profile": sample.agent_profile,
                        "recent": sample.recent,
                        "previous": sample.previous,
                    }),
                )?;
                tracing::warn!(
                    project = sample.project_id,
                    role = sample.loop_kind,
                    profile = sample.agent_profile,
                    "routing drift detected"
                );
            }
            DriftTransition::Cleared => {
                deps.store.emit(
                    None,
                    "routing.drift_cleared",
                    json!({
                        "project_id": sample.project_id,
                        "role": sample.loop_kind,
                        "profile": sample.agent_profile,
                        "recent": sample.recent,
                    }),
                )?;
            }
            DriftTransition::Unchanged => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::WindowAgg;

    fn sample(recent: (f64, f64), previous: (f64, f64)) -> DriftSample {
        DriftSample {
            project_id: "demo".into(),
            loop_kind: "worker".into(),
            agent_profile: "claude-sonnet".into(),
            recent: WindowAgg {
                runs: 20,
                success_rate: recent.0,
                avg_turns: recent.1,
            },
            previous: WindowAgg {
                runs: 20,
                success_rate: previous.0,
                avg_turns: previous.1,
            },
        }
    }

    #[test]
    fn success_rate_drop_trips_at_the_threshold() {
        let cfg = DriftConfig::default(); // -20pt / +50%
        // 90% → 69% is a 21pt drop → drift.
        assert!(is_drifting(&sample((69.0, 5.0), (90.0, 5.0)), &cfg));
        // 90% → 75% is 15pt → within tolerance.
        assert!(!is_drifting(&sample((75.0, 5.0), (90.0, 5.0)), &cfg));
    }

    #[test]
    fn turns_increase_trips_at_the_threshold() {
        let cfg = DriftConfig::default();
        // 4 → 6 turns is +50% → drift (equality trips).
        assert!(is_drifting(&sample((90.0, 6.0), (90.0, 4.0)), &cfg));
        // 4 → 5 is +25% → fine.
        assert!(!is_drifting(&sample((90.0, 5.0), (90.0, 4.0)), &cfg));
    }

    #[test]
    fn steady_group_does_not_drift() {
        let cfg = DriftConfig::default();
        assert!(!is_drifting(&sample((90.0, 5.0), (90.0, 5.0)), &cfg));
        // Improvement never drifts.
        assert!(!is_drifting(&sample((95.0, 3.0), (80.0, 5.0)), &cfg));
    }
}
