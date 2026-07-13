//! End-to-end routing outcome drift (routing 2/3, issue #65): seed scored run
//! history, run the scheduler's drift sweep, and assert the `routing_drift`
//! state table and the `routing.drift` / `routing.drift_cleared` event journal
//! behave — including dedup across identical sweeps and per-project scoping.

use std::sync::Arc;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::{Deps, routing_drift};
use meguri::forge::fake::FakeForge;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};

/// A Deps whose only live parts are the store, config, and project id — all
/// the drift sweep touches. Every project shares the one in-memory store.
fn deps_for(store: &Store, project: &str, window: usize) -> Deps {
    let mut config = Config::default();
    config.drift.window = window;
    let project_cfg = ProjectConfig {
        id: project.into(),
        repo_path: std::path::PathBuf::from("/tmp/none"),
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        plan_delivery: Default::default(),
        review: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: None,
        pr: None,
        clean: None,
        worktree_setup: Default::default(),
    };
    Deps::with_label_source(
        store.clone(),
        Arc::new(FakeMux::new(false)),
        Arc::new(FakeForge::default()),
        config,
        project_cfg,
    )
}

/// Insert one terminal scored run in the given group with `turns` turns.
/// Only public store API is used, so turn_no is driven via begin_turn.
fn seed(
    store: &Store,
    project: &str,
    issue: i64,
    loop_kind: &str,
    profile: &str,
    turns: i64,
    status: RunStatus,
) {
    let run = store
        .create_run_for_loop(project, loop_kind, issue, "t")
        .unwrap();
    store.update_run_agent_profile(&run.id, profile).unwrap();
    store
        .update_run_status(&run.id, RunStatus::Running, None)
        .unwrap();
    for i in 0..turns {
        let tid = format!("{}-t{i}", run.id);
        store
            .begin_turn(&run.id, &tid, "execute", "/tmp/p.md")
            .unwrap();
        store.finish_turn(&tid, "success", None).unwrap();
    }
    store.update_run_status(&run.id, status, None).unwrap();
}

fn drift_events(store: &Store, kind: &str) -> usize {
    // Drift events carry no run_id, so count by kind directly.
    store.count_events(kind).unwrap()
}

#[test]
fn sweep_detects_records_and_clears_drift() {
    let store = Store::open_in_memory().unwrap();
    let deps = deps_for(&store, "demo", 2); // window=2 → need 4 runs

    // Previous window (older): 2 clean successes, 4 turns each.
    seed(
        &store,
        "demo",
        1,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Succeeded,
    );
    seed(
        &store,
        "demo",
        2,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Succeeded,
    );
    // Recent window (newer): 2 failures → success 100%→0% (−100pt) = drift.
    seed(
        &store,
        "demo",
        3,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Failed,
    );
    seed(
        &store,
        "demo",
        4,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Failed,
    );

    routing_drift::sweep(&deps).unwrap();
    let active = store.active_drift(Some("demo")).unwrap();
    assert_eq!(active.len(), 1, "drift recorded");
    assert_eq!(active[0].loop_kind, "worker");
    assert_eq!(active[0].agent_profile, "claude-sonnet");
    assert_eq!(drift_events(&store, "routing.drift"), 1);

    // Idempotent: a second identical sweep must not re-emit.
    routing_drift::sweep(&deps).unwrap();
    assert_eq!(store.active_drift(Some("demo")).unwrap().len(), 1);
    assert_eq!(
        drift_events(&store, "routing.drift"),
        1,
        "dedup: no repeat event"
    );

    // Recovery: two fresh successes push the failures into the previous window.
    seed(
        &store,
        "demo",
        5,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Succeeded,
    );
    seed(
        &store,
        "demo",
        6,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Succeeded,
    );
    routing_drift::sweep(&deps).unwrap();
    assert!(
        store.active_drift(Some("demo")).unwrap().is_empty(),
        "recovered → cleared"
    );
    assert_eq!(drift_events(&store, "routing.drift_cleared"), 1);
    // A repeat sweep after recovery emits nothing more.
    routing_drift::sweep(&deps).unwrap();
    assert_eq!(drift_events(&store, "routing.drift_cleared"), 1);
}

#[test]
fn drift_is_scoped_per_project() {
    let store = Store::open_in_memory().unwrap();

    // demo regresses; other stays healthy — with identical (role, profile).
    for p in ["demo", "other"] {
        seed(
            &store,
            p,
            1,
            "worker",
            "claude-sonnet",
            4,
            RunStatus::Succeeded,
        );
        seed(
            &store,
            p,
            2,
            "worker",
            "claude-sonnet",
            4,
            RunStatus::Succeeded,
        );
    }
    // demo's recent window fails; other's recent window keeps succeeding.
    seed(
        &store,
        "demo",
        3,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Failed,
    );
    seed(
        &store,
        "demo",
        4,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Failed,
    );
    seed(
        &store,
        "other",
        3,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Succeeded,
    );
    seed(
        &store,
        "other",
        4,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Succeeded,
    );

    routing_drift::sweep(&deps_for(&store, "demo", 2)).unwrap();
    routing_drift::sweep(&deps_for(&store, "other", 2)).unwrap();

    // Only demo drifts; each read is scoped by project_id.
    assert_eq!(store.active_drift(Some("demo")).unwrap().len(), 1);
    assert!(store.active_drift(Some("other")).unwrap().is_empty());
    let all = store.active_drift(None).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].project_id, "demo");
}

#[test]
fn stats_report_groups_by_role_and_profile() {
    let store = Store::open_in_memory().unwrap();
    seed(
        &store,
        "demo",
        1,
        "worker",
        "claude-sonnet",
        4,
        RunStatus::Succeeded,
    );
    seed(
        &store,
        "demo",
        2,
        "worker",
        "claude-sonnet",
        6,
        RunStatus::Failed,
    );
    seed(
        &store,
        "demo",
        3,
        "planner",
        "claude-opus",
        2,
        RunStatus::Succeeded,
    );

    let rows = store.routing_stats(Some("demo"), 20).unwrap();
    assert_eq!(rows.len(), 2, "one row per (role, profile)");
    let worker = rows.iter().find(|r| r.loop_kind == "worker").unwrap();
    assert_eq!(worker.runs, 2);
    assert!((worker.success_rate - 50.0).abs() < 1e-9);
    assert!((worker.avg_turns - 5.0).abs() < 1e-9);
}
