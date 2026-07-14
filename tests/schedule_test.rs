//! Cron schedule sweep (issue #146): firing, catch-up folding, backfill
//! suppression, the overlap guard, and hot-reload definition additions —
//! driven with a fixed injected clock against a FakeForge / in-memory store.

use std::sync::Arc;

use meguri::config::{Config, ProjectConfig, ProjectMode, ScheduleConfig, ScheduleKind};
use meguri::engine::Deps;
use meguri::engine::scheduler_fire::sweep;
use meguri::forge::fake::FakeForge;
use meguri::forge::{LABEL_PLAN, LABEL_READY};
use meguri::mux::fake::FakeMux;
use meguri::store::{Store, parse_ts};
use meguri::tasks::{LocalTaskSource, TaskKind, TaskSource};

fn ts(s: &str) -> u64 {
    parse_ts(s).unwrap()
}

/// A schedule with an inline body (`name {{date}}` title, overlap guard as
/// given).
fn sched(name: &str, cron: &str, kind: ScheduleKind, allow_overlap: bool) -> ScheduleConfig {
    ScheduleConfig {
        name: name.into(),
        cron: cron.into(),
        kind,
        title: name.to_string() + " {{date}}",
        body_file: None,
        body: Some(format!("body of {name}")),
        allow_overlap,
    }
}

fn make_project(
    mode: ProjectMode,
    repo_path: std::path::PathBuf,
    schedules: Vec<ScheduleConfig>,
) -> ProjectConfig {
    ProjectConfig {
        id: "proj".into(),
        repo_path,
        repo_slug: if mode == ProjectMode::Local {
            None
        } else {
            Some("me/proj".into())
        },
        mode,
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
        schedules,
        prompts: Default::default(),
    }
}

fn github_deps_on(
    store: Store,
    forge: Arc<FakeForge>,
    repo_path: std::path::PathBuf,
    schedules: Vec<ScheduleConfig>,
) -> Deps {
    let project = make_project(ProjectMode::Github, repo_path, schedules);
    Deps::with_label_source(
        store,
        Arc::new(FakeMux::new(false)),
        forge,
        Config::default(),
        project,
    )
}

fn local_deps(schedules: Vec<ScheduleConfig>) -> (Deps, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let project = make_project(ProjectMode::Local, dir.path().to_path_buf(), schedules);
    let task_source: Arc<dyn TaskSource> =
        Arc::new(LocalTaskSource::new(store.clone(), project.id.clone()));
    let deps = Deps {
        store,
        mux: Arc::new(FakeMux::new(false)),
        forge: None,
        task_source,
        notifier: meguri::notify::fake::recording_notifier().0,
        forge_factory: Arc::new(meguri::forge::gh::GhForgeFactory),
        config: Config::default(),
        project,
        open_prs: Default::default(),
    };
    (deps, dir)
}

fn issue_count(forge: &FakeForge) -> usize {
    forge.issues.lock().unwrap().len()
}

#[tokio::test]
async fn github_fires_labeled_issue_and_is_discoverable() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );

    // First sweep only seeds — no backfill of today's earlier occurrences.
    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 0);

    // Once 09:00 has passed, the schedule fires exactly one issue.
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap();
    {
        let issues = forge.issues.lock().unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].has_label(LABEL_READY));
        assert_eq!(issues[0].title, "daily 2026-07-13");
        assert!(
            issues[0]
                .body
                .contains("<!-- meguri:schedule name=daily -->"),
            "body carries the provenance marker"
        );
    }

    // The created issue is what the worker task source discovers.
    let targets = deps.task_source.discover(TaskKind::Work).await.unwrap();
    assert!(targets.iter().any(|t| t.key.number() == 1));
}

#[tokio::test]
async fn github_plan_kind_gets_the_plan_label() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("weekly-plan", "0 9 * * 1", ScheduleKind::Plan, false)],
    );

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // Monday, seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire
    {
        let issues = forge.issues.lock().unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].has_label(LABEL_PLAN));
    }
    let targets = deps.task_source.discover(TaskKind::Plan).await.unwrap();
    assert!(targets.iter().any(|t| t.key.number() == 1));
}

#[tokio::test]
async fn body_file_is_read_from_the_repo() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("ops")).unwrap();
    std::fs::write(
        dir.path().join("ops/task.md"),
        "# From a file\nrun the tidy",
    )
    .unwrap();
    let forge = Arc::new(FakeForge::default());
    let mut s = sched("filed", "0 9 * * *", ScheduleKind::Ready, false);
    s.body = None;
    s.body_file = Some("ops/task.md".into());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![s],
    );

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap();
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap();
    let issues = forge.issues.lock().unwrap();
    assert!(issues[0].body.contains("run the tidy"));
    assert!(
        issues[0]
            .body
            .contains("<!-- meguri:schedule name=filed -->")
    );
}

#[tokio::test]
async fn downtime_folds_to_a_single_fire() {
    // allow_overlap so the fold is isolated from the overlap guard.
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("hourly", "0 * * * *", ScheduleKind::Ready, true)],
    );

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T01:30:00Z")).await.unwrap(); // fires (01:00)
    assert_eq!(issue_count(&forge), 1);

    // Down for hours: occurrences 02:00..06:00 all elapsed, but the catch-up
    // folds to ONE fire, not five.
    sweep(&deps, ts("2026-07-13T06:10:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 2);
}

#[tokio::test]
async fn a_newly_added_schedule_does_not_backfill() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );

    // First observed at 09:30 — today's 09:00 already passed and must NOT fire.
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap();
    sweep(&deps, ts("2026-07-13T09:31:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 0);

    // Tomorrow's 09:00 fires normally.
    sweep(&deps, ts("2026-07-14T09:05:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 1);
}

#[tokio::test]
async fn overlap_guard_skips_while_open_and_consumes_the_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire issue #1
    assert_eq!(issue_count(&forge), 1);

    // Day 2's occurrence is due, but issue #1 is still open → skip (no #2).
    sweep(&deps, ts("2026-07-14T09:30:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 1);

    // The skip CONSUMED day 2's occurrence: closing #1 later the same day does
    // not backfill — nothing fires until the next cron hit.
    forge.close_issue(1);
    sweep(&deps, ts("2026-07-14T18:00:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 1);

    // Day 3's occurrence fires, since #1 is now closed.
    sweep(&deps, ts("2026-07-15T09:05:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 2);
}

#[tokio::test]
async fn allow_overlap_fires_every_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, true)],
    );
    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // #1 (still open)
    sweep(&deps, ts("2026-07-14T09:30:00Z")).await.unwrap(); // #2 despite #1 open
    assert_eq!(issue_count(&forge), 2);
}

#[tokio::test]
async fn hot_reload_adds_a_schedule_without_losing_existing_state() {
    // Shared store + forge across two Deps: the second stands in for the
    // config hot-reload that swaps Deps mid-run (issue #73).
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let forge = Arc::new(FakeForge::default());

    let deps1 = github_deps_on(
        store.clone(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("alpha", "0 9 * * *", ScheduleKind::Ready, false)],
    );
    sweep(&deps1, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed alpha

    // "Reload": a new Deps that adds `beta`, over the SAME store.
    let deps2 = github_deps_on(
        store.clone(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![
            sched("alpha", "0 9 * * *", ScheduleKind::Ready, false),
            sched("beta", "0 9 * * *", ScheduleKind::Ready, false),
        ],
    );
    // At 09:30: alpha's window (seeded at 00:00) contains 09:00 → fires; beta
    // is seen for the first time → seeds, does NOT backfill.
    sweep(&deps2, ts("2026-07-13T09:30:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 1, "only alpha fires; beta just seeds");

    // Close alpha's issue so its overlap guard lets it fire again; next day
    // both fire (alpha again, beta for the first time) — proving beta's state
    // was seeded across the reload and alpha's survived it.
    forge.close_issue(1);
    sweep(&deps2, ts("2026-07-14T09:05:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 3);
}

#[tokio::test]
async fn local_mode_fires_a_work_task_and_dedups() {
    let (deps, _dir) = local_deps(vec![sched(
        "daily",
        "0 9 * * *",
        ScheduleKind::Ready,
        false,
    )]);

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire task
    let tasks = deps.store.list_tasks("proj", true).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].kind, "work");
    assert_eq!(tasks[0].origin, "schedule:daily");
    assert!(
        tasks[0]
            .body
            .contains("<!-- meguri:schedule name=daily -->")
    );

    // Discoverable by the local worker task source.
    let targets = deps.task_source.discover(TaskKind::Work).await.unwrap();
    assert_eq!(targets.len(), 1);

    // Still-open task blocks day 2's fire (overlap guard).
    sweep(&deps, ts("2026-07-14T09:30:00Z")).await.unwrap();
    assert_eq!(deps.store.list_tasks("proj", true).unwrap().len(), 1);

    // Complete it, and the next occurrence fires again.
    let id = tasks[0].id;
    deps.store.complete_task(id).unwrap();
    sweep(&deps, ts("2026-07-15T09:05:00Z")).await.unwrap();
    assert_eq!(deps.store.list_tasks("proj", true).unwrap().len(), 2);
}
