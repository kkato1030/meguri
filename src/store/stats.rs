//! Routing outcome stats and drift state (routing 2/3, issue #65).
//!
//! Everything here is pure sqlite direct-read/write so `meguri stats routing`
//! and doctor work with the watch stopped (mirrors the retired serve read
//! path, ADR 0002). `routing_stats` keys on `(project_id, runs.loop_kind =
//! role, runs.agent_profile, runs.routing_arm)` — the arm (main / explore /
//! escalated, routing 3/3 #66) keeps a canary or escalated run on its own row
//! instead of collapsing into the mainline profile it shares. Drift keys on the
//! narrower `(project, role, profile)`. The empty string stands in for an
//! unrouted run (`agent_profile IS NULL`) so it can sit in a composite key.

use anyhow::Result;
use rusqlite::params;
use serde::Serialize;

use super::{Store, now, parse_ts};

/// Run statuses that count toward a routing "score": a success and the two
/// genuine failures. `skipped` / `needs_plan` / `decomposed` are benign
/// terminal endings and are excluded from the denominator (ADR 0007).
const SCORED_STATUSES: &str = "('succeeded', 'failed', 'cancelled')";

/// The empty-string sentinel for an unrouted run (NULL `agent_profile`).
pub const UNROUTED: &str = "";

/// The mainline-arm sentinel for a run with no `routing_arm` set (routing 3/3,
/// issue #66) — the ordinary pick, as opposed to `explore` / `escalated`.
pub const ARM_MAIN: &str = "main";

/// The read-side default for a run that was never stamped with a collab plane
/// (feature off / ineligible loop / pre-migration run). Only `"advisor"` is
/// ever written; NULL collapses to this (issue #121).
pub const COLLAB_OFF: &str = "off";

/// One terminal, scored run reduced to just the columns aggregation needs.
struct RunOutcome {
    project_id: String,
    loop_kind: String,
    agent_profile: String,
    /// Routing arm: [`ARM_MAIN`], `"explore"`, or `"escalated"` (issue #66).
    routing_arm: String,
    /// Collab plane: [`COLLAB_OFF`] or `"advisor"` (issue #121). NULL rows read
    /// as `off`.
    collab_mode: String,
    /// The github issue number, or `None` for a local-task run. `collab_stats`
    /// keeps only issue-backed runs (local tasks never get an advisor).
    issue_number: Option<i64>,
    succeeded: bool,
    turn_no: i64,
    duration_secs: Option<i64>,
}

/// Aggregate metrics for one `(role, profile)` group over a set of runs.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct WindowAgg {
    pub runs: usize,
    /// Success rate in percentage points, 0–100.
    pub success_rate: f64,
    pub avg_turns: f64,
}

impl WindowAgg {
    fn of(runs: &[&RunOutcome]) -> Self {
        let n = runs.len();
        if n == 0 {
            return Self {
                runs: 0,
                success_rate: 0.0,
                avg_turns: 0.0,
            };
        }
        let succ = runs.iter().filter(|r| r.succeeded).count();
        let turns: i64 = runs.iter().map(|r| r.turn_no).sum();
        Self {
            runs: n,
            success_rate: succ as f64 / n as f64 * 100.0,
            avg_turns: turns as f64 / n as f64,
        }
    }
}

/// One row of `meguri stats routing`: a `(role, profile, arm)` group over the
/// most recent N scored runs.
#[derive(Debug, Clone, Serialize)]
pub struct RoutingStatRow {
    pub project_id: String,
    pub loop_kind: String,
    /// Profile name, or [`UNROUTED`] (empty) for runs with no pinned profile.
    pub agent_profile: String,
    /// Routing arm: [`ARM_MAIN`], `"explore"`, or `"escalated"` (issue #66).
    pub routing_arm: String,
    pub runs: usize,
    pub success_rate: f64,
    pub avg_turns: f64,
    pub avg_duration_secs: Option<f64>,
}

/// One row of `meguri stats collab` (issue #121): a `(role, profile, arm,
/// collab_mode)` group over the most recent N scored runs. Same shape as
/// [`RoutingStatRow`] with the collab plane added, so the CLI can put an `off`
/// and an `advisor` row side by side for the same routing.
#[derive(Debug, Clone, Serialize)]
pub struct CollabStatRow {
    pub project_id: String,
    pub loop_kind: String,
    /// Profile name, or [`UNROUTED`] (empty) for runs with no pinned profile.
    pub agent_profile: String,
    /// Routing arm: [`ARM_MAIN`], `"explore"`, or `"escalated"` (issue #66).
    pub routing_arm: String,
    /// Collab plane: [`COLLAB_OFF`] or `"advisor"` (issue #121).
    pub collab_mode: String,
    pub runs: usize,
    pub success_rate: f64,
    pub avg_turns: f64,
    pub avg_duration_secs: Option<f64>,
}

/// A group that has enough history to compare a recent window against the one
/// before it — the input to drift detection.
#[derive(Debug, Clone, Serialize)]
pub struct DriftSample {
    pub project_id: String,
    pub loop_kind: String,
    pub agent_profile: String,
    pub recent: WindowAgg,
    pub previous: WindowAgg,
}

/// Current drift state for one `(project, role, profile)` — the read-side
/// source of truth (the `events` log has no project and can't be scoped).
#[derive(Debug, Clone, Serialize)]
pub struct DriftRow {
    pub project_id: String,
    pub loop_kind: String,
    pub agent_profile: String,
    pub active: bool,
    pub metric_json: String,
    pub detected_at: String,
    pub updated_at: String,
}

/// What a [`Store::record_drift`] write did, so the caller emits an event only
/// on a threshold crossing (dedup: identical repeat sweeps are `Unchanged`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftTransition {
    /// Crossed into drift (0→1) — emit `routing.drift`.
    BecameActive,
    /// Recovered (1→0) — emit `routing.drift_cleared`.
    Cleared,
    /// No state change — emit nothing.
    Unchanged,
}

impl Store {
    /// Terminal, scored runs newest-first, optionally scoped to one project.
    fn scored_outcomes(&self, project: Option<&str>) -> Result<Vec<RunOutcome>> {
        self.with_conn(|c| {
            let sql = format!(
                "SELECT project_id, loop_kind, COALESCE(agent_profile, '') AS profile,
                        COALESCE(routing_arm, '{ARM_MAIN}') AS arm,
                        COALESCE(collab_mode, '{COLLAB_OFF}') AS collab, issue_number,
                        status, turn_no, started_at, finished_at
                 FROM runs
                 WHERE status IN {SCORED_STATUSES}
                   AND (?1 IS NULL OR project_id = ?1)
                 -- rowid tiebreaks runs sharing a created_at second, giving a
                 -- deterministic newest-first order (insertion order).
                 ORDER BY created_at DESC, rowid DESC"
            );
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map(params![project], |row| {
                    let status: String = row.get("status")?;
                    let started: Option<String> = row.get("started_at")?;
                    let finished: Option<String> = row.get("finished_at")?;
                    let duration_secs = match (
                        started.as_deref().and_then(parse_ts),
                        finished.as_deref().and_then(parse_ts),
                    ) {
                        (Some(s), Some(f)) if f >= s => Some((f - s) as i64),
                        _ => None,
                    };
                    Ok(RunOutcome {
                        project_id: row.get("project_id")?,
                        loop_kind: row.get("loop_kind")?,
                        agent_profile: row.get("profile")?,
                        routing_arm: row.get("arm")?,
                        collab_mode: row.get("collab")?,
                        issue_number: row.get("issue_number")?,
                        succeeded: status == "succeeded",
                        turn_no: row.get("turn_no")?,
                        duration_secs,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// `(role, profile, arm)` metrics over the most recent `window` scored
    /// runs. `project = None` spans every project (each row keeps its
    /// `project_id`); `Some(id)` restricts to one. Groups are returned sorted
    /// for stable display (project, role, profile, arm).
    pub fn routing_stats(
        &self,
        project: Option<&str>,
        window: usize,
    ) -> Result<Vec<RoutingStatRow>> {
        let outcomes = self.scored_outcomes(project)?;
        // Preserve newest-first order within each group as we bucket. The arm
        // (main / explore / escalated) joins the key so a canary or an
        // escalated run shows as its own row rather than collapsing into the
        // mainline profile it shares an `agent_profile` with (issue #66).
        type Key = (String, String, String, String);
        let mut order: Vec<Key> = Vec::new();
        let mut groups: std::collections::HashMap<Key, Vec<&RunOutcome>> =
            std::collections::HashMap::new();
        for o in &outcomes {
            let key = (
                o.project_id.clone(),
                o.loop_kind.clone(),
                o.agent_profile.clone(),
                o.routing_arm.clone(),
            );
            let bucket = groups.entry(key.clone()).or_insert_with(|| {
                order.push(key);
                Vec::new()
            });
            if bucket.len() < window {
                bucket.push(o);
            }
        }
        order.sort();
        let mut rows = Vec::with_capacity(order.len());
        for key in order {
            let bucket = &groups[&key];
            let agg = WindowAgg::of(bucket);
            let durations: Vec<i64> = bucket.iter().filter_map(|r| r.duration_secs).collect();
            let avg_duration_secs = if durations.is_empty() {
                None
            } else {
                Some(durations.iter().sum::<i64>() as f64 / durations.len() as f64)
            };
            rows.push(RoutingStatRow {
                project_id: key.0,
                loop_kind: key.1,
                agent_profile: key.2,
                routing_arm: key.3,
                runs: agg.runs,
                success_rate: agg.success_rate,
                avg_turns: agg.avg_turns,
                avg_duration_secs,
            });
        }
        Ok(rows)
    }

    /// `(role, profile, arm, collab_mode)` metrics over the most recent
    /// `window` scored runs — `meguri stats collab` (issue #121). This is
    /// `routing_stats` with the collab plane added to the key, so an `off` and
    /// an `advisor` row for the same `(role, profile, arm)` sit next to each
    /// other and the routing is held constant while only the collab plane
    /// varies (ADR 0017).
    ///
    /// Only runs that could have received an advisor are counted: an
    /// advisor-eligible loop kind (worker / spec-worker) AND a github issue
    /// backing (local tasks never get an advisor — the agmsg team is
    /// issue-scoped, ADR 0006). Everything else is dropped so it can't pollute
    /// the `off` baseline. This narrowing lives here, not in the shared
    /// `scored_outcomes` (routing stats count every run).
    pub fn collab_stats(&self, project: Option<&str>, window: usize) -> Result<Vec<CollabStatRow>> {
        let outcomes = self.scored_outcomes(project)?;
        type Key = (String, String, String, String, String);
        let mut order: Vec<Key> = Vec::new();
        let mut groups: std::collections::HashMap<Key, Vec<&RunOutcome>> =
            std::collections::HashMap::new();
        for o in &outcomes {
            // Keep only runs an advisor could have joined; the rest would skew
            // the off/advisor comparison.
            if !crate::collab::supports_advisor_loop_kind(&o.loop_kind) || o.issue_number.is_none()
            {
                continue;
            }
            let key = (
                o.project_id.clone(),
                o.loop_kind.clone(),
                o.agent_profile.clone(),
                o.routing_arm.clone(),
                o.collab_mode.clone(),
            );
            let bucket = groups.entry(key.clone()).or_insert_with(|| {
                order.push(key);
                Vec::new()
            });
            if bucket.len() < window {
                bucket.push(o);
            }
        }
        order.sort();
        let mut rows = Vec::with_capacity(order.len());
        for key in order {
            let bucket = &groups[&key];
            let agg = WindowAgg::of(bucket);
            let durations: Vec<i64> = bucket.iter().filter_map(|r| r.duration_secs).collect();
            let avg_duration_secs = if durations.is_empty() {
                None
            } else {
                Some(durations.iter().sum::<i64>() as f64 / durations.len() as f64)
            };
            rows.push(CollabStatRow {
                project_id: key.0,
                loop_kind: key.1,
                agent_profile: key.2,
                routing_arm: key.3,
                collab_mode: key.4,
                runs: agg.runs,
                success_rate: agg.success_rate,
                avg_turns: agg.avg_turns,
                avg_duration_secs,
            });
        }
        Ok(rows)
    }

    /// Groups with a full recent window AND a full preceding window (≥ 2·window
    /// scored runs), split newest-first: the first `window` are `recent`, the
    /// next `window` are `previous`. Groups with too little history are
    /// omitted — drift is not judged until both windows are full (ADR 0007).
    pub fn routing_drift_samples(
        &self,
        project: Option<&str>,
        window: usize,
    ) -> Result<Vec<DriftSample>> {
        if window == 0 {
            return Ok(Vec::new());
        }
        let outcomes = self.scored_outcomes(project)?;
        let mut order: Vec<(String, String, String)> = Vec::new();
        let mut groups: std::collections::HashMap<(String, String, String), Vec<&RunOutcome>> =
            std::collections::HashMap::new();
        for o in &outcomes {
            let key = (
                o.project_id.clone(),
                o.loop_kind.clone(),
                o.agent_profile.clone(),
            );
            groups
                .entry(key.clone())
                .or_insert_with(|| {
                    order.push(key);
                    Vec::new()
                })
                .push(o);
        }
        order.sort();
        let mut samples = Vec::new();
        for key in order {
            let bucket = &groups[&key];
            if bucket.len() < window * 2 {
                continue;
            }
            let recent = WindowAgg::of(&bucket[..window]);
            let previous = WindowAgg::of(&bucket[window..window * 2]);
            samples.push(DriftSample {
                project_id: key.0,
                loop_kind: key.1,
                agent_profile: key.2,
                recent,
                previous,
            });
        }
        Ok(samples)
    }

    // --- CLI version drift (layer 1) ---------------------------------------

    /// The last recorded version row for a CLI, if any.
    pub fn get_cli_version(&self, command: &str) -> Result<Option<(String, Option<i64>)>> {
        self.with_conn(|c| {
            let mut stmt =
                c.prepare("SELECT version, major FROM cli_versions WHERE command = ?1")?;
            let mut rows = stmt.query([command])?;
            match rows.next()? {
                Some(row) => Ok(Some((row.get(0)?, row.get(1)?))),
                None => Ok(None),
            }
        })
    }

    /// UPSERT the current version of a CLI (called by doctor each run).
    pub fn record_cli_version(
        &self,
        command: &str,
        version: &str,
        major: Option<i64>,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO cli_versions (command, version, major, checked_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(command) DO UPDATE SET
                   version = excluded.version,
                   major = excluded.major,
                   checked_at = excluded.checked_at",
                params![command, version, major, now()],
            )?;
            Ok(())
        })
    }

    // --- drift state table (layer 2) ---------------------------------------

    /// Record the current drift verdict for a `(project, role, profile)` and
    /// report whether it crossed a threshold. Nothing is written for a group
    /// that is not drifting and has no prior row (avoids clutter). `detected_at`
    /// is stamped only when the state flips into drift.
    pub fn record_drift(
        &self,
        project_id: &str,
        loop_kind: &str,
        agent_profile: &str,
        active: bool,
        metric_json: &str,
    ) -> Result<DriftTransition> {
        self.with_conn(|c| {
            let existing: Option<(bool, String)> = {
                let mut stmt = c.prepare(
                    "SELECT active, detected_at FROM routing_drift
                     WHERE project_id = ?1 AND loop_kind = ?2 AND agent_profile = ?3",
                )?;
                let mut rows = stmt.query(params![project_id, loop_kind, agent_profile])?;
                match rows.next()? {
                    Some(row) => {
                        let a: i64 = row.get(0)?;
                        Some((a != 0, row.get(1)?))
                    }
                    None => None,
                }
            };
            let prev_active = existing.as_ref().map(|(a, _)| *a).unwrap_or(false);

            // Never-drifted group staying calm: leave no row behind.
            if !active && existing.is_none() {
                return Ok(DriftTransition::Unchanged);
            }

            let ts = now();
            let detected_at = if active && !prev_active {
                ts.clone()
            } else {
                existing.map(|(_, d)| d).unwrap_or_else(|| ts.clone())
            };
            c.execute(
                "INSERT INTO routing_drift
                   (project_id, loop_kind, agent_profile, active, metric_json, detected_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(project_id, loop_kind, agent_profile) DO UPDATE SET
                   active = excluded.active,
                   metric_json = excluded.metric_json,
                   detected_at = excluded.detected_at,
                   updated_at = excluded.updated_at",
                params![
                    project_id,
                    loop_kind,
                    agent_profile,
                    active as i64,
                    metric_json,
                    detected_at,
                    ts,
                ],
            )?;
            Ok(match (prev_active, active) {
                (false, true) => DriftTransition::BecameActive,
                (true, false) => DriftTransition::Cleared,
                _ => DriftTransition::Unchanged,
            })
        })
    }

    /// Unresolved (`active=1`) drift rows, optionally scoped to one project,
    /// ordered for stable display.
    pub fn active_drift(&self, project: Option<&str>) -> Result<Vec<DriftRow>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT project_id, loop_kind, agent_profile, active, metric_json,
                        detected_at, updated_at
                 FROM routing_drift
                 WHERE active = 1 AND (?1 IS NULL OR project_id = ?1)
                 ORDER BY project_id, loop_kind, agent_profile",
            )?;
            let rows = stmt
                .query_map(params![project], |row| {
                    let active: i64 = row.get(3)?;
                    Ok(DriftRow {
                        project_id: row.get(0)?,
                        loop_kind: row.get(1)?,
                        agent_profile: row.get(2)?,
                        active: active != 0,
                        metric_json: row.get(4)?,
                        detected_at: row.get(5)?,
                        updated_at: row.get(6)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::RunStatus;

    /// Insert a scored run with the given group and turn count, then close it.
    fn seed_run(
        store: &Store,
        project: &str,
        issue: i64,
        loop_kind: &str,
        profile: Option<&str>,
        turns: i64,
        status: RunStatus,
    ) {
        let run = store
            .create_run_for_loop(project, loop_kind, issue, "t")
            .unwrap();
        if let Some(p) = profile {
            store.update_run_agent_profile(&run.id, p).unwrap();
        }
        store
            .with_conn(|c| {
                c.execute(
                    "UPDATE runs SET turn_no = ?2 WHERE id = ?1",
                    params![run.id, turns],
                )?;
                Ok(())
            })
            .unwrap();
        // Terminal status stamps finished_at; started_at is stamped on Running.
        store
            .update_run_status(&run.id, RunStatus::Running, None)
            .unwrap();
        store.update_run_status(&run.id, status, None).unwrap();
    }

    #[test]
    fn routing_stats_splits_by_arm() {
        // Two runs share (worker, claude-opus) but differ by arm: one mainline,
        // one escalated. They must land in separate rows so `stats routing` can
        // tell "started on opus" from "climbed to opus" (issue #66).
        let store = Store::open_in_memory().unwrap();
        seed_run(
            &store,
            "demo",
            1,
            "worker",
            Some("claude-opus"),
            3,
            RunStatus::Succeeded,
        );
        let escalated = store.create_run_for_loop("demo", "worker", 2, "t").unwrap();
        store
            .update_run_agent_profile(&escalated.id, "claude-opus")
            .unwrap();
        store
            .update_run_routing_arm(&escalated.id, Some("escalated"))
            .unwrap();
        store
            .update_run_status(&escalated.id, RunStatus::Running, None)
            .unwrap();
        store
            .update_run_status(&escalated.id, RunStatus::Succeeded, None)
            .unwrap();

        let rows = store.routing_stats(Some("demo"), 20).unwrap();
        assert_eq!(rows.len(), 2, "mainline and escalated are separate rows");
        assert!(
            rows.iter()
                .any(|r| r.agent_profile == "claude-opus" && r.routing_arm == ARM_MAIN)
        );
        assert!(
            rows.iter()
                .any(|r| r.agent_profile == "claude-opus" && r.routing_arm == "escalated")
        );
    }

    #[test]
    fn routing_stats_groups_and_scores() {
        let store = Store::open_in_memory().unwrap();
        // 3 worker/claude-sonnet runs: 2 succeeded, 1 failed → 66.6% success.
        seed_run(
            &store,
            "demo",
            1,
            "worker",
            Some("claude-sonnet"),
            4,
            RunStatus::Succeeded,
        );
        seed_run(
            &store,
            "demo",
            2,
            "worker",
            Some("claude-sonnet"),
            6,
            RunStatus::Succeeded,
        );
        seed_run(
            &store,
            "demo",
            3,
            "worker",
            Some("claude-sonnet"),
            8,
            RunStatus::Failed,
        );
        // A benign skip must NOT count toward the denominator.
        seed_run(
            &store,
            "demo",
            4,
            "worker",
            Some("claude-sonnet"),
            99,
            RunStatus::Skipped,
        );

        let rows = store.routing_stats(None, 20).unwrap();
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.loop_kind, "worker");
        assert_eq!(r.agent_profile, "claude-sonnet");
        assert_eq!(r.runs, 3);
        assert!((r.success_rate - 200.0 / 3.0).abs() < 1e-9);
        assert!((r.avg_turns - 6.0).abs() < 1e-9);
    }

    #[test]
    fn routing_stats_scopes_by_project_and_marks_unrouted() {
        let store = Store::open_in_memory().unwrap();
        seed_run(
            &store,
            "demo",
            1,
            "worker",
            Some("claude-sonnet"),
            3,
            RunStatus::Succeeded,
        );
        seed_run(
            &store,
            "other",
            2,
            "worker",
            Some("claude-opus"),
            5,
            RunStatus::Succeeded,
        );
        // A run with no pinned profile → grouped under the unrouted sentinel.
        seed_run(&store, "demo", 3, "planner", None, 2, RunStatus::Succeeded);

        let all = store.routing_stats(None, 20).unwrap();
        assert_eq!(all.len(), 3, "one row per (project, role, profile)");

        let demo = store.routing_stats(Some("demo"), 20).unwrap();
        assert!(demo.iter().all(|r| r.project_id == "demo"));
        assert_eq!(demo.len(), 2);
        assert!(
            demo.iter()
                .any(|r| r.agent_profile == UNROUTED && r.loop_kind == "planner")
        );
    }

    #[test]
    fn routing_stats_window_keeps_only_recent() {
        let store = Store::open_in_memory().unwrap();
        // 3 fails (older) then would-be recent successes; window=2 keeps the 2
        // newest (both succeeded) → 100%.
        for i in 0..3 {
            seed_run(
                &store,
                "demo",
                i,
                "fixer",
                Some("claude-sonnet"),
                1,
                RunStatus::Failed,
            );
        }
        for i in 3..5 {
            seed_run(
                &store,
                "demo",
                i,
                "fixer",
                Some("claude-sonnet"),
                1,
                RunStatus::Succeeded,
            );
        }
        let rows = store.routing_stats(Some("demo"), 2).unwrap();
        assert_eq!(rows[0].runs, 2);
        assert!((rows[0].success_rate - 100.0).abs() < 1e-9);
    }

    #[test]
    fn drift_samples_need_two_full_windows() {
        let store = Store::open_in_memory().unwrap();
        // window=2 → need ≥4 runs. Give 3 → no sample.
        for i in 0..3 {
            seed_run(
                &store,
                "demo",
                i,
                "worker",
                Some("claude-sonnet"),
                2,
                RunStatus::Succeeded,
            );
        }
        assert!(
            store
                .routing_drift_samples(Some("demo"), 2)
                .unwrap()
                .is_empty()
        );

        // Add a 4th → recent=[#3,#2], previous=[#1,#0].
        seed_run(
            &store,
            "demo",
            3,
            "worker",
            Some("claude-sonnet"),
            2,
            RunStatus::Succeeded,
        );
        let s = store.routing_drift_samples(Some("demo"), 2).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].recent.runs, 2);
        assert_eq!(s[0].previous.runs, 2);
    }

    #[test]
    fn record_drift_transitions_and_dedups() {
        let store = Store::open_in_memory().unwrap();
        // No row, calm → nothing written, Unchanged.
        assert_eq!(
            store
                .record_drift("demo", "worker", "claude-sonnet", false, "{}")
                .unwrap(),
            DriftTransition::Unchanged
        );
        assert!(store.active_drift(None).unwrap().is_empty());

        // Calm → drift.
        assert_eq!(
            store
                .record_drift("demo", "worker", "claude-sonnet", true, "{\"x\":1}")
                .unwrap(),
            DriftTransition::BecameActive
        );
        // Repeat same state → dedup.
        assert_eq!(
            store
                .record_drift("demo", "worker", "claude-sonnet", true, "{\"x\":1}")
                .unwrap(),
            DriftTransition::Unchanged
        );
        let active = store.active_drift(None).unwrap();
        assert_eq!(active.len(), 1);
        assert!(active[0].active);

        // Recover → Cleared, and it drops out of active_drift.
        assert_eq!(
            store
                .record_drift("demo", "worker", "claude-sonnet", false, "{}")
                .unwrap(),
            DriftTransition::Cleared
        );
        assert!(store.active_drift(None).unwrap().is_empty());

        // Re-drift is a genuine 0→1 flip again (a fresh BecameActive), not a
        // suppressed repeat.
        assert_eq!(
            store
                .record_drift("demo", "worker", "claude-sonnet", true, "{}")
                .unwrap(),
            DriftTransition::BecameActive
        );
        assert_eq!(store.active_drift(None).unwrap().len(), 1);
    }

    #[test]
    fn active_drift_scopes_by_project() {
        let store = Store::open_in_memory().unwrap();
        store
            .record_drift("demo", "worker", "claude-sonnet", true, "{}")
            .unwrap();
        store
            .record_drift("other", "worker", "claude-sonnet", true, "{}")
            .unwrap();
        assert_eq!(store.active_drift(Some("demo")).unwrap().len(), 1);
        assert_eq!(
            store.active_drift(Some("demo")).unwrap()[0].project_id,
            "demo"
        );
        assert_eq!(store.active_drift(None).unwrap().len(), 2);
    }

    #[test]
    fn cli_version_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_cli_version("claude").unwrap().is_none());
        store
            .record_cli_version("claude", "1.2.3", Some(1))
            .unwrap();
        assert_eq!(
            store.get_cli_version("claude").unwrap(),
            Some(("1.2.3".to_string(), Some(1)))
        );
        // UPSERT overwrites in place.
        store
            .record_cli_version("claude", "2.0.0", Some(2))
            .unwrap();
        assert_eq!(
            store.get_cli_version("claude").unwrap(),
            Some(("2.0.0".to_string(), Some(2)))
        );
    }

    // --- collab stats (issue #121) -----------------------------------------

    use crate::collab::COLLAB_MODE_ADVISOR;

    /// Insert a scored issue-backed run with an explicit collab plane (and
    /// optional routing arm), then close it. `collab = None` leaves the column
    /// NULL (the read-side `off`).
    #[allow(clippy::too_many_arguments)]
    fn seed_collab_run(
        store: &Store,
        project: &str,
        issue: i64,
        loop_kind: &str,
        profile: Option<&str>,
        arm: Option<&str>,
        collab: Option<&str>,
        turns: i64,
        status: RunStatus,
    ) {
        let run = store
            .create_run_for_loop(project, loop_kind, issue, "t")
            .unwrap();
        if let Some(p) = profile {
            store.update_run_agent_profile(&run.id, p).unwrap();
        }
        if let Some(a) = arm {
            store.update_run_routing_arm(&run.id, Some(a)).unwrap();
        }
        if let Some(c) = collab {
            store.update_run_collab_mode(&run.id, c).unwrap();
        }
        store
            .with_conn(|c| {
                c.execute(
                    "UPDATE runs SET turn_no = ?2 WHERE id = ?1",
                    params![run.id, turns],
                )?;
                Ok(())
            })
            .unwrap();
        store
            .update_run_status(&run.id, RunStatus::Running, None)
            .unwrap();
        store.update_run_status(&run.id, status, None).unwrap();
    }

    #[test]
    fn collab_stats_splits_off_and_advisor_holding_routing_constant() {
        // Same (worker, sonnet, main): 2 unstamped (off) runs and 2 advisor
        // runs → two rows, distinguished only by the collab plane. Success
        // rates are computed within each plane.
        let store = Store::open_in_memory().unwrap();
        // off: 1 succeeded, 1 failed → 50%.
        seed_collab_run(
            &store,
            "demo",
            1,
            "worker",
            Some("sonnet"),
            None,
            None,
            4,
            RunStatus::Succeeded,
        );
        seed_collab_run(
            &store,
            "demo",
            2,
            "worker",
            Some("sonnet"),
            None,
            None,
            8,
            RunStatus::Failed,
        );
        // advisor: 2 succeeded → 100%.
        seed_collab_run(
            &store,
            "demo",
            3,
            "worker",
            Some("sonnet"),
            None,
            Some(COLLAB_MODE_ADVISOR),
            2,
            RunStatus::Succeeded,
        );
        seed_collab_run(
            &store,
            "demo",
            4,
            "worker",
            Some("sonnet"),
            None,
            Some(COLLAB_MODE_ADVISOR),
            2,
            RunStatus::Succeeded,
        );

        let rows = store.collab_stats(Some("demo"), 20).unwrap();
        assert_eq!(rows.len(), 2, "one off row and one advisor row");
        let off = rows.iter().find(|r| r.collab_mode == COLLAB_OFF).unwrap();
        let adv = rows
            .iter()
            .find(|r| r.collab_mode == COLLAB_MODE_ADVISOR)
            .unwrap();
        // Both share the same routing (profile, arm) — collab is the only diff.
        assert_eq!(off.agent_profile, "sonnet");
        assert_eq!(adv.agent_profile, "sonnet");
        assert_eq!(off.routing_arm, ARM_MAIN);
        assert_eq!(adv.routing_arm, ARM_MAIN);
        assert!((off.success_rate - 50.0).abs() < 1e-9);
        assert!((adv.success_rate - 100.0).abs() < 1e-9);
    }

    #[test]
    fn collab_stats_reads_null_as_off() {
        let store = Store::open_in_memory().unwrap();
        seed_collab_run(
            &store,
            "demo",
            1,
            "worker",
            Some("sonnet"),
            None,
            None,
            3,
            RunStatus::Succeeded,
        );
        let rows = store.collab_stats(None, 20).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].collab_mode, COLLAB_OFF);
    }

    #[test]
    fn collab_stats_keeps_routing_axes_separate() {
        // Different profile or different arm must not collapse into one row,
        // even at the same collab plane — otherwise collab and routing mix.
        let store = Store::open_in_memory().unwrap();
        seed_collab_run(
            &store,
            "demo",
            1,
            "worker",
            Some("sonnet"),
            None,
            None,
            3,
            RunStatus::Succeeded,
        );
        seed_collab_run(
            &store,
            "demo",
            2,
            "worker",
            Some("opus"),
            None,
            None,
            3,
            RunStatus::Succeeded,
        );
        seed_collab_run(
            &store,
            "demo",
            3,
            "worker",
            Some("sonnet"),
            Some("escalated"),
            None,
            3,
            RunStatus::Succeeded,
        );
        let rows = store.collab_stats(Some("demo"), 20).unwrap();
        // (sonnet, main), (opus, main), (sonnet, escalated) → 3 distinct rows.
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter()
                .any(|r| r.agent_profile == "opus" && r.routing_arm == ARM_MAIN)
        );
        assert!(
            rows.iter()
                .any(|r| r.agent_profile == "sonnet" && r.routing_arm == "escalated")
        );
    }

    #[test]
    fn collab_stats_excludes_ineligible_loops_and_local_tasks() {
        let store = Store::open_in_memory().unwrap();
        // An advisor-eligible, issue-backed worker → included.
        seed_collab_run(
            &store,
            "demo",
            1,
            "worker",
            Some("sonnet"),
            None,
            None,
            3,
            RunStatus::Succeeded,
        );
        // A planner run (not advisor-eligible) → excluded.
        seed_collab_run(
            &store,
            "demo",
            2,
            "planner",
            Some("sonnet"),
            None,
            None,
            3,
            RunStatus::Succeeded,
        );
        // A local-task worker (no issue backing) → excluded.
        let local = store
            .create_run_for_task("demo", crate::engine::worker::KIND, 42, "t")
            .unwrap();
        store
            .update_run_status(&local.id, RunStatus::Running, None)
            .unwrap();
        store
            .update_run_status(&local.id, RunStatus::Succeeded, None)
            .unwrap();

        let rows = store.collab_stats(Some("demo"), 20).unwrap();
        assert_eq!(rows.len(), 1, "only the issue-backed worker survives");
        assert_eq!(rows[0].loop_kind, "worker");
    }

    #[test]
    fn collab_stats_window_keeps_only_recent() {
        let store = Store::open_in_memory().unwrap();
        // 3 older failures, then 2 newer successes, all same group; window=2
        // keeps the 2 newest → 100%.
        for i in 0..3 {
            seed_collab_run(
                &store,
                "demo",
                i,
                "worker",
                Some("sonnet"),
                None,
                None,
                1,
                RunStatus::Failed,
            );
        }
        for i in 3..5 {
            seed_collab_run(
                &store,
                "demo",
                i,
                "worker",
                Some("sonnet"),
                None,
                None,
                1,
                RunStatus::Succeeded,
            );
        }
        let rows = store.collab_stats(Some("demo"), 2).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].runs, 2);
        assert!((rows[0].success_rate - 100.0).abs() < 1e-9);
    }
}
