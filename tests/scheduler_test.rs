//! Watch-loop tests: discovery dispatches labeled issues; startup recovery
//! resumes runs orphaned by a dead orchestrator.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::scheduler::{Reload, Scheduler};
use meguri::engine::{Deps, RecipeFn, WorkerOutcome, default_recipe};
use meguri::forge::fake::FakeForge;
use meguri::forge::{Forge, LABEL_IMPLEMENTING, LABEL_READY, LABEL_WORKING};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};

async fn init_origin_and_clone(root: &Path) -> PathBuf {
    let origin = root.join("origin.git");
    let clone = root.join("clone");
    std::fs::create_dir_all(&origin).unwrap();
    run_git(&origin, &["init", "--bare", "-b", "main"])
        .await
        .unwrap();
    run_git(
        root,
        &["clone", origin.to_str().unwrap(), clone.to_str().unwrap()],
    )
    .await
    .unwrap();
    for args in [
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "meguri-test"],
        vec!["commit", "--allow-empty", "-m", "init"],
        vec!["push", "-u", "origin", "main"],
    ] {
        run_git(&clone, &args).await.unwrap();
    }
    clone
}

async fn setup(root: &Path, forge: Arc<FakeForge>) -> Deps {
    let clone = init_origin_and_clone(root).await;

    // Quiesce the cleaner loop: a report issue whose marker already covers
    // the current head keeps these tests about worker discovery
    // (cleaner_test drives the cleaner itself).
    let head = run_git(&clone, &["rev-parse", "HEAD"]).await.unwrap();
    let scanned = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    forge
        .create_issue(
            meguri::engine::cleaner::REPORT_TITLE,
            &meguri::engine::cleaner::clean_marker(&head, scanned),
            &[meguri::forge::LABEL_CLEAN_REPORT],
        )
        .await
        .unwrap();

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600;
    config.limits.result_grace_secs = 1;
    config.review.enabled = false; // self-review not under test in the scheduler suite
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: Some(clone),
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: Some(root.join("worktrees")),
        pr: None,
        clean: None,
        triage: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
        autonomy: None,
        cadence: Vec::new(),
        prompts: Default::default(),
        notify: None,
    };
    Deps::with_label_source(
        Store::open_in_memory().unwrap(),
        Arc::new(FakeMux::new(false)),
        forge,
        config,
        project,
    )
}

/// Scripted pane-side agent (same protocol as worker_test).
fn spawn_scripted_agent(worktree_root: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut done: std::collections::HashSet<String> = Default::default();
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let Ok(projects) = std::fs::read_dir(worktree_root.join("proj")) else {
                continue;
            };
            for wt in projects.filter_map(|e| e.ok()).map(|e| e.path()) {
                let meguri = wt.join(".meguri");
                let Ok(entries) = std::fs::read_dir(&meguri) else {
                    continue;
                };
                for id in entries.filter_map(|e| e.ok()).filter_map(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .strip_prefix("prompt-")?
                        .strip_suffix(".md")
                        .map(str::to_string)
                }) {
                    if done.contains(&id) {
                        continue;
                    }
                    done.insert(id.clone());
                    std::fs::write(wt.join("work.txt"), format!("turn {id}\n")).unwrap();
                    run_git(&wt, &["add", "work.txt"]).await.unwrap();
                    run_git(
                        &wt,
                        &[
                            "-c",
                            "user.email=a@a",
                            "-c",
                            "user.name=agent",
                            "commit",
                            "-m",
                            "work",
                        ],
                    )
                    .await
                    .unwrap();
                    std::fs::write(
                        meguri.join("result.json"),
                        format!(r#"{{"turn_id":"{id}","status":"success","summary":"ok"}}"#),
                    )
                    .unwrap();
                }
            }
        }
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_discovers_and_completes_labeled_issue() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(
        11,
        "Scheduled work",
        "Do it.",
        &[LABEL_READY],
    ));
    let deps = setup(root.path(), forge.clone()).await;
    let store = deps.store.clone();

    let agent = spawn_scripted_agent(root.path().join("worktrees"));
    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(300),
        max_concurrent: 2,
        reload: None,
        recipe: default_recipe(),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    // Wait until the run driven by discovery succeeds.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!(
                "run never succeeded; runs: {:?}",
                store.list_runs(false).unwrap()
            );
        }
        let runs = store.list_runs(false).unwrap();
        if runs
            .iter()
            .any(|r| r.status == RunStatus::Succeeded && r.issue_number == 11)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    watch.abort();
    agent.abort();

    assert_eq!(forge.prs().len(), 1);
    // Phase transition (ADR 0005): ready/working gone, implementing applied —
    // the issue is no longer untriaged, it is "implementation PR open".
    let labels = forge.labels_of(11);
    assert!(!labels.contains(&LABEL_READY.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(
        labels.contains(&LABEL_IMPLEMENTING.to_string()),
        "issue must carry {LABEL_IMPLEMENTING} after the PR opens: {labels:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_skips_working_and_hold_issues() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(
        12,
        "Claimed elsewhere",
        "Another host is on it.",
        &[LABEL_READY, LABEL_WORKING],
    ));
    forge.issues.lock().unwrap().push(meguri::forge::Issue {
        number: 13,
        title: "Held".into(),
        body: String::new(),
        labels: vec![LABEL_READY.into(), meguri::forge::LABEL_HOLD.into()],
    });
    let deps = setup(root.path(), forge.clone()).await;
    let store = deps.store.clone();

    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
        recipe: default_recipe(),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });
    tokio::time::sleep(Duration::from_secs(2)).await;
    watch.abort();

    assert!(
        store.list_runs(false).unwrap().is_empty(),
        "no runs may be created for claimed/held issues"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_gates_on_open_blocker_until_closed_as_completed() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(
        41,
        "Blocked work",
        "Depends on #40.",
        &[LABEL_READY],
    ));
    // GitHub-native dependency: #41 is blocked by the still-open #40.
    forge.block_issue(41, 40);
    let deps = setup(root.path(), forge.clone()).await;
    let store = deps.store.clone();

    let agent = spawn_scripted_agent(root.path().join("worktrees"));
    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
        recipe: default_recipe(),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    // While the blocker is open, discovery must skip — quietly: no run, no
    // claim, no escalation label, no comment.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        store.list_runs(false).unwrap().is_empty(),
        "no runs may start while a blocker is open"
    );
    let labels = forge.labels_of(41);
    assert!(labels.contains(&LABEL_READY.to_string()), "{labels:?}");
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&meguri::forge::LABEL_NEEDS_HUMAN.to_string()));
    assert!(forge.comments_of(41).is_empty(), "skips must be silent");

    // Closing the blocker as completed resolves the dependency; the next
    // discovery pass picks the issue up and drives it to a PR.
    forge.close_issue(40);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!(
                "run never succeeded after the blocker closed; runs: {:?}",
                store.list_runs(false).unwrap()
            );
        }
        let runs = store.list_runs(false).unwrap();
        if runs
            .iter()
            .any(|r| r.status == RunStatus::Succeeded && r.issue_number == 41)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    watch.abort();
    agent.abort();

    assert_eq!(forge.prs().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_keeps_skipping_when_blocker_closed_as_not_planned() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(
        43,
        "Blocked work",
        "Depends on #42.",
        &[LABEL_READY],
    ));
    forge.block_issue(43, 42);
    // not_planned does not resolve the dependency: the plan this issue was
    // built on never happened, so a human has to re-triage it.
    forge.close_issue_as(42, "not_planned");
    let deps = setup(root.path(), forge.clone()).await;
    let store = deps.store.clone();

    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
        recipe: default_recipe(),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });
    tokio::time::sleep(Duration::from_secs(2)).await;
    watch.abort();

    assert!(
        store.list_runs(false).unwrap().is_empty(),
        "a not_planned blocker must keep the issue skipped"
    );
    // Still a quiet skip: no escalation, no comment.
    let labels = forge.labels_of(43);
    assert!(labels.contains(&LABEL_READY.to_string()), "{labels:?}");
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&meguri::forge::LABEL_NEEDS_HUMAN.to_string()));
    assert!(forge.comments_of(43).is_empty(), "skips must be silent");
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_does_not_refile_issue_with_succeeded_run() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(
        15,
        "Already shipped",
        "A PR exists; the ready label lingered.",
        &[LABEL_READY],
    ));
    let deps = setup(root.path(), forge.clone()).await;
    let store = deps.store.clone();

    // A previous run already shipped this issue.
    let done = store.create_run("proj", 15, "Already shipped").unwrap();
    store
        .update_run_status(&done.id, RunStatus::Succeeded, None)
        .unwrap();

    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
        recipe: default_recipe(),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });
    tokio::time::sleep(Duration::from_secs(2)).await;
    watch.abort();

    let runs = store.list_runs(false).unwrap();
    assert_eq!(
        runs.len(),
        1,
        "discovery must not re-file a shipped issue: {runs:?}"
    );
    assert_eq!(runs[0].id, done.id);
    assert!(forge.prs().is_empty(), "no duplicate PR may be opened");
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_resumes_interrupted_run_to_success() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(
        14,
        "Crashed mid-flight",
        "Resume me.",
        &[LABEL_READY],
    ));
    // Simulate a claim from the previous (crashed) orchestrator.
    forge.add_label(14, LABEL_WORKING).await.unwrap();

    let deps = setup(root.path(), forge.clone()).await;
    let store = deps.store.clone();

    // A run the dead orchestrator left in `running` at prepare-worktree,
    // with a pane that no longer exists.
    let run = store.create_run("proj", 14, "Crashed mid-flight").unwrap();
    store
        .update_run_status(&run.id, RunStatus::Running, None)
        .unwrap();
    store
        .update_run_step(
            &run.id,
            "prepare-worktree",
            r#"{"issue_title":"Crashed mid-flight","issue_body":"Resume me."}"#,
        )
        .unwrap();
    store
        .update_run_mux(&run.id, "tmux", "meguri", "%99999")
        .unwrap();

    let agent = spawn_scripted_agent(root.path().join("worktrees"));
    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(300),
        max_concurrent: 2,
        reload: None,
        recipe: default_recipe(),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!(
                "recovered run never succeeded; runs: {:?}",
                store.list_runs(false).unwrap()
            );
        }
        if let Some(r) = store.get_run(&run.id).unwrap()
            && r.status == RunStatus::Succeeded
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    watch.abort();
    agent.abort();

    // The recovery event trail exists.
    let events = store.events_for_run(&run.id, 200).unwrap();
    assert!(events.iter().any(|e| e.kind == "run.recovered"));
    assert_eq!(forge.prs().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_dispatches_multiple_ready_issues_concurrently() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(
        21,
        "First ready issue",
        "Do it.",
        &[LABEL_READY],
    ));
    forge.issues.lock().unwrap().push(meguri::forge::Issue {
        number: 22,
        title: "Second ready issue".into(),
        body: "Do it too.".into(),
        labels: vec![LABEL_READY.into()],
    });
    let deps = setup(root.path(), forge.clone()).await;
    let store = deps.store.clone();

    let agent = spawn_scripted_agent(root.path().join("worktrees"));
    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(300),
        max_concurrent: 2,
        reload: None,
        recipe: default_recipe(),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!(
                "both runs never succeeded; runs: {:?}",
                store.list_runs(false).unwrap()
            );
        }
        let runs = store.list_runs(false).unwrap();
        let done = [21, 22]
            .iter()
            .filter(|n| {
                runs.iter()
                    .any(|r| r.status == RunStatus::Succeeded && r.issue_number == **n)
            })
            .count();
        if done == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    watch.abort();
    agent.abort();

    assert_eq!(forge.prs().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_reclaims_worktree_after_issue_closes() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(
        31,
        "Ship then close",
        "Do it.",
        &[LABEL_READY],
    ));
    let deps = setup(root.path(), forge.clone()).await;
    let store = deps.store.clone();

    let agent = spawn_scripted_agent(root.path().join("worktrees"));
    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(300),
        max_concurrent: 2,
        reload: None,
        recipe: default_recipe(),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    // Drive the issue to a successful run first.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let worktree = loop {
        if tokio::time::Instant::now() > deadline {
            panic!(
                "run never succeeded; runs: {:?}",
                store.list_runs(false).unwrap()
            );
        }
        let runs = store.list_runs(false).unwrap();
        if let Some(run) = runs
            .iter()
            .find(|r| r.status == RunStatus::Succeeded && r.issue_number == 31)
        {
            break PathBuf::from(run.worktree_path.clone().unwrap());
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert!(
        worktree.exists(),
        "worktree survives while the issue is open"
    );

    // Closing the issue (PR merged) lets the watch sweep reclaim it. The PR
    // must actually be terminal too: an *open* meguri PR keeps the identity's
    // resources on the PR side (finding 4b), so mark it merged first.
    forge
        .prs
        .lock()
        .unwrap()
        .iter_mut()
        .for_each(|p| p.state = "merged".into());
    forge.close_issue(31);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while worktree.exists() {
        if tokio::time::Instant::now() > deadline {
            panic!("worktree was not reclaimed after the issue closed");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    watch.abort();
    agent.abort();
}

/// Shared dispatch log: (loop kind, project id, issue number).
type DispatchLog = Arc<std::sync::Mutex<Vec<(String, String, i64)>>>;

/// A recording recipe (the test seam of ADR 0012 決定8): logs each dispatch
/// and drives the run straight to success. Enqueue is the reconcilers' job
/// now, so ordering tests seed `queued` runs directly and observe the
/// workqueue's dispatch order.
fn recording_recipe(order: DispatchLog) -> RecipeFn {
    Arc::new(move |deps, run_id, _kind| {
        let order = order.clone();
        Box::pin(async move {
            let run = deps.store.get_run(&run_id)?.expect("run exists");
            order.lock().unwrap().push((
                run.loop_kind.clone(),
                run.project_id.clone(),
                run.issue_number,
            ));
            deps.store
                .update_run_status(&run_id, RunStatus::Succeeded, None)?;
            Ok(WorkerOutcome::Succeeded {
                pr_url: "stub://pr".into(),
            })
        })
    })
}

/// Seed a queued run of `kind` for (project, issue) — what a reconciler's
/// enqueue leaves for the workqueue.
fn seed_queued_run(store: &Store, project: &str, kind: &str, issue: i64) {
    store
        .create_run_for_loop(project, kind, issue, &format!("target {issue}"))
        .unwrap();
}

/// Wait until `order` has `expected` entries, then return them.
async fn wait_for_dispatches(order: &DispatchLog, expected: usize) -> Vec<(String, String, i64)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let log = order.lock().unwrap().clone();
        if log.len() >= expected {
            return log;
        }
        if tokio::time::Instant::now() > deadline {
            panic!(
                "only {} of {expected} dispatches happened: {log:?}",
                log.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Loop-list order is dispatch priority: with one slot, the first loop's
/// target wins even though the other loop's issue number is smaller.
#[tokio::test(flavor = "multi_thread")]
async fn watch_prioritizes_runs_by_dispatch_rank() {
    let root = tempfile::tempdir().unwrap();
    let deps = setup(root.path(), Arc::new(FakeForge::default())).await;
    let order: DispatchLog = Arc::new(std::sync::Mutex::new(Vec::new()));

    // The worker run has the smaller issue number; only dispatch_rank
    // (fixer = closer to merge) can put the fixer run ahead of it.
    seed_queued_run(&deps.store, "proj", "fixer", 200);
    seed_queued_run(&deps.store, "proj", "worker", 100);
    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(100),
        max_concurrent: 1,
        reload: None,
        recipe: recording_recipe(order.clone()),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });
    let log = wait_for_dispatches(&order, 2).await;
    watch.abort();

    assert_eq!(log[0], ("fixer".into(), "proj".into(), 200));
    assert_eq!(log[1], ("worker".into(), "proj".into(), 100));
}

/// Within one kind, runs dispatch oldest-first (FIFO by issue number) no
/// matter what order they were enqueued in.
#[tokio::test(flavor = "multi_thread")]
async fn watch_dispatches_runs_of_one_kind_in_fifo_order() {
    let root = tempfile::tempdir().unwrap();
    let deps = setup(root.path(), Arc::new(FakeForge::default())).await;
    let order: DispatchLog = Arc::new(std::sync::Mutex::new(Vec::new()));

    for issue in [33, 11, 22] {
        seed_queued_run(&deps.store, "proj", "fixer", issue);
    }
    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(100),
        max_concurrent: 1,
        reload: None,
        recipe: recording_recipe(order.clone()),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });
    let log = wait_for_dispatches(&order, 3).await;
    watch.abort();

    let issues: Vec<i64> = log.iter().map(|(_, _, n)| *n).collect();
    assert_eq!(issues, vec![11, 22, 33]);
}

/// dispatch_rank beats project order: project B's fixer run takes the slot
/// before project A's planner run (nest inversion).
#[tokio::test(flavor = "multi_thread")]
async fn watch_prioritizes_dispatch_rank_over_project_order() {
    let root = tempfile::tempdir().unwrap();
    let deps_a = setup(root.path(), Arc::new(FakeForge::default())).await;
    let mut deps_b = deps_a.clone();
    deps_b.project.id = "proj-b".into();
    let order: DispatchLog = Arc::new(std::sync::Mutex::new(Vec::new()));

    seed_queued_run(&deps_a.store, "proj", "planner", 1);
    seed_queued_run(&deps_a.store, "proj-b", "fixer", 300);
    let scheduler = Scheduler {
        projects: vec![deps_a, deps_b],
        poll_interval: Duration::from_millis(100),
        max_concurrent: 1,
        reload: None,
        recipe: recording_recipe(order.clone()),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });
    let log = wait_for_dispatches(&order, 2).await;
    watch.abort();

    assert_eq!(log[0], ("fixer".into(), "proj-b".into(), 300));
    assert_eq!(log[1], ("planner".into(), "proj".into(), 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_ticks_write_a_heartbeat() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = setup(root.path(), forge).await;
    let store = deps.store.clone();
    assert_eq!(store.latest_heartbeat("watch").unwrap(), None);

    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
        recipe: default_recipe(),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if store.latest_heartbeat("watch").unwrap().is_some() {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("watch never wrote a heartbeat");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    watch.abort();
}

/// Run a single-slot scheduler over `projects` with the seeded queued runs
/// until `expected` dispatches are logged, then return them in order.
async fn drive_order(
    projects: Vec<Deps>,
    runs: Vec<(&'static str, &'static str, i64)>,
    expected: usize,
) -> Vec<(String, String, i64)> {
    let log: DispatchLog = Arc::new(Mutex::new(Vec::new()));
    let store = projects[0].store.clone();
    for (project, kind, issue) in runs {
        seed_queued_run(&store, project, kind, issue);
    }
    let scheduler = Scheduler {
        projects,
        poll_interval: Duration::from_millis(100),
        // One slot at a time so drive order mirrors dispatch priority.
        max_concurrent: 1,
        reload: None,
        recipe: recording_recipe(log.clone()),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });
    let order = wait_for_dispatches(&log, expected).await;
    watch.abort();
    order
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_dispatches_loops_in_priority_order() {
    let root = tempfile::tempdir().unwrap();
    let deps = setup(root.path(), Arc::new(FakeForge::default())).await;

    // The worker run has the smaller issue number; only dispatch_rank can
    // put the fixer run ahead of it.
    let order = drive_order(
        vec![deps],
        vec![("proj", "fixer", 201), ("proj", "worker", 101)],
        2,
    )
    .await;

    assert_eq!(
        order,
        vec![
            ("fixer".into(), "proj".into(), 201),
            ("worker".into(), "proj".into(), 101),
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_orders_targets_within_a_loop_by_issue_number() {
    let root = tempfile::tempdir().unwrap();
    let deps = setup(root.path(), Arc::new(FakeForge::default())).await;

    // Runs are seeded unsorted; the workqueue normalizes to oldest-first.
    let order = drive_order(
        vec![deps],
        vec![
            ("proj", "fixer", 305),
            ("proj", "fixer", 301),
            ("proj", "fixer", 303),
        ],
        3,
    )
    .await;

    let issues: Vec<i64> = order.iter().map(|(_, _, n)| *n).collect();
    assert_eq!(issues, vec![301, 303, 305]);
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_prefers_loop_priority_over_project_order() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    // Two projects sharing one store; "alpha" is listed first.
    let mut deps_a = setup(&root.path().join("alpha"), forge.clone()).await;
    deps_a.project.id = "alpha".into();
    let mut deps_b = setup(&root.path().join("beta"), forge).await;
    deps_b.store = deps_a.store.clone();
    deps_b.project.id = "beta".into();

    // Project beta only has fixer work, project alpha only planner work;
    // the fixer rank must win even though alpha comes first.
    let order = drive_order(
        vec![deps_a, deps_b],
        vec![("beta", "fixer", 501), ("alpha", "planner", 401)],
        2,
    )
    .await;

    assert_eq!(order[0], ("fixer".into(), "beta".into(), 501));
    assert_eq!(order[1], ("planner".into(), "alpha".into(), 401));
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_dispatches_any_registered_loop_by_kind() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = setup(root.path(), forge).await;
    let store = deps.store.clone();

    // A queued run of an arbitrary kind (what a reconciler's enqueue leaves)
    // dispatches through the recipe seam by its `loop_kind`.
    seed_queued_run(&store, "proj", "fixed", 99);
    let order: DispatchLog = Arc::new(Mutex::new(Vec::new()));
    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
        recipe: recording_recipe(order.clone()),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!(
                "fixed-kind run never succeeded; runs: {:?}",
                store.list_runs(false).unwrap()
            );
        }
        let runs = store.list_runs(false).unwrap();
        if runs
            .iter()
            .any(|r| r.status == RunStatus::Succeeded && r.issue_number == 99)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    watch.abort();

    let runs = store.list_runs(false).unwrap();
    assert_eq!(runs.len(), 1, "exactly the seeded run: {runs:?}");
    assert_eq!(runs[0].loop_kind, "fixed");
}

/// A recipe that records the `config.language` each dispatch's Deps carries,
/// then drives the run straight to success.
fn language_recording_recipe(log: Arc<Mutex<Vec<Option<String>>>>) -> RecipeFn {
    Arc::new(move |deps, run_id, _kind| {
        let log = log.clone();
        Box::pin(async move {
            log.lock().unwrap().push(deps.config.language.clone());
            deps.store
                .update_run_status(&run_id, RunStatus::Succeeded, None)?;
            Ok(WorkerOutcome::Succeeded {
                pr_url: "lang://pr".into(),
            })
        })
    })
}

/// Config hot reload (issue #73): a swap delivered by the reload hook reaches
/// the runs spawned after it, while the run already driven keeps the startup
/// config.
#[tokio::test(flavor = "multi_thread")]
async fn watch_applies_reloaded_config_to_new_runs() {
    let root = tempfile::tempdir().unwrap();
    let deps = setup(root.path(), Arc::new(FakeForge::default())).await;
    let log: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));

    // Simulated config edit: once the first run has driven, the hook starts
    // returning Deps whose config carries a language.
    let mut reloaded = deps.clone();
    reloaded.config.language = Some("日本語".into());
    let hook_log = log.clone();
    let reload = Box::new(move || {
        (hook_log.lock().unwrap().len() == 1).then(|| Reload {
            projects: vec![reloaded.clone()],
            poll_interval: Duration::from_millis(100),
            max_concurrent: 1,
        })
    });

    seed_queued_run(&deps.store, "proj", "lang", 71);
    seed_queued_run(&deps.store, "proj", "lang", 72);
    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(100),
        // One slot: issue 71 drives before the "edit", issue 72 after it.
        max_concurrent: 1,
        reload: Some(reload),
        recipe: language_recording_recipe(log.clone()),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if log.lock().unwrap().len() >= 2 {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("both runs never drove: {:?}", log.lock().unwrap());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    watch.abort();

    let languages = log.lock().unwrap().clone();
    assert_eq!(languages[0], None, "the first run uses the startup config");
    assert_eq!(
        languages[1].as_deref(),
        Some("日本語"),
        "runs spawned after the reload see the new config"
    );
}

/// A recipe that records each dispatched run id, sits for `delay` (simulating
/// the async gap between `dispatch` spawning the driver and the driver
/// recording `running`), then succeeds. Nothing enqueues these runs — the
/// tests seed them `interrupted`, so the only dispatch path is the per-tick
/// redispatch.
fn delayed_recording_recipe(dispatched: Arc<Mutex<Vec<String>>>, delay: Duration) -> RecipeFn {
    Arc::new(move |deps, run_id, _kind| {
        let dispatched = dispatched.clone();
        Box::pin(async move {
            dispatched.lock().unwrap().push(run_id.clone());
            tokio::time::sleep(delay).await;
            deps.store
                .update_run_status(&run_id, RunStatus::Succeeded, None)?;
            Ok(WorkerOutcome::Succeeded {
                pr_url: "no-discovery://pr".into(),
            })
        })
    })
}

/// #183 regression: a run that goes `interrupted` (pane died mid-execute)
/// while `watch` is already several ticks into its loop must resume from its
/// checkpoint on a later tick, not only at the next orchestrator restart.
/// Discovery is neutered (`NoDiscoveryLoop` never returns a target), so the
/// only path that can drive this run is the per-tick redispatch.
#[tokio::test(flavor = "multi_thread")]
async fn watch_redispatches_a_run_interrupted_after_watch_is_already_running() {
    let root = tempfile::tempdir().unwrap();
    let deps = setup(root.path(), Arc::new(FakeForge::default())).await;
    let store = deps.store.clone();
    let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(80),
        max_concurrent: 2,
        reload: None,
        recipe: delayed_recording_recipe(dispatched.clone(), Duration::from_millis(10)),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    // Let several ticks pass with no runs at all, so `watch` is well past
    // its startup-only recovery pass before the "pane death" happens.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        dispatched.lock().unwrap().is_empty(),
        "nothing should dispatch before the run exists"
    );

    let run = store
        .create_run_for_loop("proj", "no-discovery", 183, "Mid-flight pane death")
        .unwrap();
    store
        .update_run_status(
            &run.id,
            RunStatus::Interrupted,
            Some("pane died during execute"),
        )
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(r) = store.get_run(&run.id).unwrap()
            && r.status == RunStatus::Succeeded
        {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!(
                "run never redispatched after mid-flight interruption: {:?}",
                store.get_run(&run.id).unwrap()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    watch.abort();

    assert_eq!(dispatched.lock().unwrap().clone(), vec![run.id.clone()]);
}

/// #183 acceptance criteria 2+3: redispatch respects the `max_concurrent`
/// slot budget, and `active_run_ids` keeps a run whose driver is still in
/// flight (even before its DB status catches up to `running`) from being
/// dispatched a second time on a later tick.
#[tokio::test(flavor = "multi_thread")]
async fn watch_redispatch_respects_slots_and_avoids_double_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let deps = setup(root.path(), Arc::new(FakeForge::default())).await;
    let store = deps.store.clone();
    let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let run_a = store
        .create_run_for_loop("proj", "no-discovery", 201, "Pane death A")
        .unwrap();
    store
        .update_run_status(
            &run_a.id,
            RunStatus::Interrupted,
            Some("pane died during execute"),
        )
        .unwrap();
    let run_b = store
        .create_run_for_loop("proj", "no-discovery", 202, "Pane death B")
        .unwrap();
    store
        .update_run_status(
            &run_b.id,
            RunStatus::Interrupted,
            Some("pane died during execute"),
        )
        .unwrap();

    let scheduler = Scheduler {
        projects: vec![deps],
        poll_interval: Duration::from_millis(80),
        max_concurrent: 1,
        reload: None,
        // Long enough that several ticks land while each run is mid-flight
        // and its store status still reads `interrupted`.
        recipe: delayed_recording_recipe(dispatched.clone(), Duration::from_millis(400)),
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let a = store.get_run(&run_a.id).unwrap().unwrap().status;
        let b = store.get_run(&run_b.id).unwrap().unwrap().status;
        if a == RunStatus::Succeeded && b == RunStatus::Succeeded {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("both interrupted runs never succeeded: a={a:?} b={b:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    watch.abort();

    // Exactly one dispatch per run — no duplicates from the slot-1 budget
    // colliding with the still-interrupted-looking in-flight run.
    let log = dispatched.lock().unwrap().clone();
    assert_eq!(
        log.len(),
        2,
        "expected exactly one dispatch per run: {log:?}"
    );
    assert!(log.contains(&run_a.id));
    assert!(log.contains(&run_b.id));
}
