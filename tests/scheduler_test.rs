//! Watch-loop tests: discovery dispatches labeled issues; startup recovery
//! resumes runs orphaned by a dead orchestrator.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::scheduler::{Reload, Scheduler};
use meguri::engine::{Deps, Loop, Target, WorkerOutcome, default_loops};
use meguri::forge::fake::FakeForge;
use meguri::forge::{Forge, LABEL_READY, LABEL_WORKING};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};
use meguri::tasks::TaskKey;

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
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: Some(root.join("worktrees")),
        pr: None,
        clean: None,
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
        loops: default_loops(),
        poll_interval: Duration::from_millis(300),
        max_concurrent: 2,
        reload: None,
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
    let labels = forge.labels_of(11);
    assert!(!labels.contains(&LABEL_READY.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
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
        loops: default_loops(),
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
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
        loops: default_loops(),
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
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
        loops: default_loops(),
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
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
        loops: default_loops(),
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
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
        loops: default_loops(),
        poll_interval: Duration::from_millis(300),
        max_concurrent: 2,
        reload: None,
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
        loops: default_loops(),
        poll_interval: Duration::from_millis(300),
        max_concurrent: 2,
        reload: None,
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
        loops: default_loops(),
        poll_interval: Duration::from_millis(300),
        max_concurrent: 2,
        reload: None,
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

    // Closing the issue (PR merged) lets the watch sweep reclaim it.
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

/// A minimal non-worker loop: discovers one fixed target and drives its run
/// straight to success.
struct FixedLoop;

#[async_trait::async_trait]
impl Loop for FixedLoop {
    fn kind(&self) -> &'static str {
        "fixed"
    }

    async fn discover(&self, deps: &Deps) -> anyhow::Result<Vec<Target>> {
        if deps
            .store
            .issue_has_succeeded_run(&deps.project.id, "fixed", 99)?
        {
            return Ok(vec![]);
        }
        Ok(vec![Target {
            key: TaskKey::Issue(99),
            title: "Fixed target".into(),
        }])
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> anyhow::Result<WorkerOutcome> {
        deps.store
            .update_run_status(run_id, RunStatus::Succeeded, None)?;
        Ok(WorkerOutcome::Succeeded {
            pr_url: "fixed://pr".into(),
        })
    }
}

/// Shared dispatch log: (loop kind, project id, issue number).
type DispatchLog = Arc<std::sync::Mutex<Vec<(String, String, i64)>>>;

/// A parameterized fake loop for the priority tests: fixed (project, issue)
/// targets, each run driven straight to success while the dispatch order is
/// recorded in a shared log.
struct StubLoop {
    kind: &'static str,
    /// (project id, issue number) pairs this loop discovers.
    targets: Vec<(&'static str, i64)>,
    order: DispatchLog,
}

#[async_trait::async_trait]
impl Loop for StubLoop {
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn discover(&self, deps: &Deps) -> anyhow::Result<Vec<Target>> {
        let mut targets = Vec::new();
        for (project, n) in &self.targets {
            if *project == deps.project.id
                && !deps
                    .store
                    .issue_has_succeeded_run(&deps.project.id, self.kind, *n)?
            {
                targets.push(Target {
                    key: TaskKey::Issue(*n),
                    title: format!("stub {n}"),
                });
            }
        }
        Ok(targets)
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> anyhow::Result<WorkerOutcome> {
        let run = deps.store.get_run(run_id)?.expect("run exists");
        self.order
            .lock()
            .unwrap()
            .push((run.loop_kind, run.project_id, run.issue_number));
        deps.store
            .update_run_status(run_id, RunStatus::Succeeded, None)?;
        Ok(WorkerOutcome::Succeeded {
            pr_url: "stub://pr".into(),
        })
    }
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
async fn watch_prioritizes_loops_in_list_order() {
    let root = tempfile::tempdir().unwrap();
    let deps = setup(root.path(), Arc::new(FakeForge::default())).await;
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    let scheduler = Scheduler {
        projects: vec![deps],
        loops: vec![
            Arc::new(StubLoop {
                kind: "stub-fixer",
                targets: vec![("proj", 200)],
                order: order.clone(),
            }),
            Arc::new(StubLoop {
                kind: "stub-worker",
                targets: vec![("proj", 100)],
                order: order.clone(),
            }),
        ],
        poll_interval: Duration::from_millis(100),
        max_concurrent: 1,
        reload: None,
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });
    let log = wait_for_dispatches(&order, 2).await;
    watch.abort();

    assert_eq!(log[0], ("stub-fixer".into(), "proj".into(), 200));
    assert_eq!(log[1], ("stub-worker".into(), "proj".into(), 100));
}

/// Within one loop, targets dispatch oldest-first (FIFO by number) no matter
/// what order discover returns them in.
#[tokio::test(flavor = "multi_thread")]
async fn watch_dispatches_targets_of_one_loop_in_fifo_order() {
    let root = tempfile::tempdir().unwrap();
    let deps = setup(root.path(), Arc::new(FakeForge::default())).await;
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    let scheduler = Scheduler {
        projects: vec![deps],
        loops: vec![Arc::new(StubLoop {
            kind: "stub",
            targets: vec![("proj", 33), ("proj", 11), ("proj", 22)],
            order: order.clone(),
        })],
        poll_interval: Duration::from_millis(100),
        max_concurrent: 1,
        reload: None,
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });
    let log = wait_for_dispatches(&order, 3).await;
    watch.abort();

    let issues: Vec<i64> = log.iter().map(|(_, _, n)| *n).collect();
    assert_eq!(issues, vec![11, 22, 33]);
}

/// Loop priority beats project order: project B's high-priority loop takes
/// the slot before project A's low-priority loop (nest inversion).
#[tokio::test(flavor = "multi_thread")]
async fn watch_prioritizes_loop_order_over_project_order() {
    let root = tempfile::tempdir().unwrap();
    let deps_a = setup(root.path(), Arc::new(FakeForge::default())).await;
    let mut deps_b = deps_a.clone();
    deps_b.project.id = "proj-b".into();
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    let scheduler = Scheduler {
        projects: vec![deps_a, deps_b],
        loops: vec![
            Arc::new(StubLoop {
                kind: "stub-fixer",
                targets: vec![("proj-b", 300)],
                order: order.clone(),
            }),
            Arc::new(StubLoop {
                kind: "stub-planner",
                targets: vec![("proj", 1)],
                order: order.clone(),
            }),
        ],
        poll_interval: Duration::from_millis(100),
        max_concurrent: 1,
        reload: None,
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });
    let log = wait_for_dispatches(&order, 2).await;
    watch.abort();

    assert_eq!(log[0], ("stub-fixer".into(), "proj-b".into(), 300));
    assert_eq!(log[1], ("stub-planner".into(), "proj".into(), 1));
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
        loops: default_loops(),
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
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

/// A scripted loop for dispatch-priority tests: discovers fixed
/// (project, issue) targets and records drive order in a shared log.
struct RecordingLoop {
    kind: &'static str,
    /// (project_id, issue_number) pairs this loop discovers.
    targets: Vec<(&'static str, i64)>,
    /// (loop kind, project_id, issue_number) in drive order.
    log: Arc<Mutex<Vec<(String, String, i64)>>>,
}

#[async_trait::async_trait]
impl Loop for RecordingLoop {
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn discover(&self, deps: &Deps) -> anyhow::Result<Vec<Target>> {
        let mut targets = Vec::new();
        for (project, issue) in &self.targets {
            if *project != deps.project.id
                || deps
                    .store
                    .issue_has_succeeded_run(&deps.project.id, self.kind, *issue)?
            {
                continue;
            }
            targets.push(Target {
                key: TaskKey::Issue(*issue),
                title: format!("target {issue}"),
            });
        }
        Ok(targets)
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> anyhow::Result<WorkerOutcome> {
        let run = deps.store.get_run(run_id)?.expect("run exists");
        self.log
            .lock()
            .unwrap()
            .push((self.kind.into(), run.project_id, run.issue_number));
        deps.store
            .update_run_status(run_id, RunStatus::Succeeded, None)?;
        Ok(WorkerOutcome::Succeeded {
            pr_url: "fake://pr".into(),
        })
    }
}

/// Run a single-slot scheduler with `loops` over `projects` until `expected`
/// drives are logged, then return the log in drive order.
async fn drive_order(
    projects: Vec<Deps>,
    loops: Vec<Arc<dyn Loop>>,
    log: Arc<Mutex<Vec<(String, String, i64)>>>,
    expected: usize,
) -> Vec<(String, String, i64)> {
    let scheduler = Scheduler {
        projects,
        loops,
        poll_interval: Duration::from_millis(100),
        // One slot at a time so drive order mirrors dispatch priority.
        max_concurrent: 1,
        reload: None,
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if log.lock().unwrap().len() >= expected {
            break;
        }
        if tokio::time::Instant::now() > deadline {
            panic!("only {:?} of {expected} drives ran", log.lock().unwrap());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    watch.abort();
    log.lock().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_dispatches_loops_in_priority_order() {
    let root = tempfile::tempdir().unwrap();
    let deps = setup(root.path(), Arc::new(FakeForge::default())).await;
    let log = Arc::new(Mutex::new(Vec::new()));

    // The worker target has the smaller issue number; only loop priority
    // (fixer listed first) can put the fixer target ahead of it.
    let order = drive_order(
        vec![deps],
        vec![
            Arc::new(RecordingLoop {
                kind: "fixer",
                targets: vec![("proj", 201)],
                log: log.clone(),
            }),
            Arc::new(RecordingLoop {
                kind: "worker",
                targets: vec![("proj", 101)],
                log: log.clone(),
            }),
        ],
        log,
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
    let log = Arc::new(Mutex::new(Vec::new()));

    // discover() returns the targets unsorted; the scheduler normalizes
    // to oldest-first (FIFO).
    let order = drive_order(
        vec![deps],
        vec![Arc::new(RecordingLoop {
            kind: "fixer",
            targets: vec![("proj", 305), ("proj", 301), ("proj", 303)],
            log: log.clone(),
        })],
        log,
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
    let log = Arc::new(Mutex::new(Vec::new()));

    // Project beta only has fixer work, project alpha only planner work;
    // the fixer loop must win even though alpha comes first.
    let order = drive_order(
        vec![deps_a, deps_b],
        vec![
            Arc::new(RecordingLoop {
                kind: "fixer",
                targets: vec![("beta", 501)],
                log: log.clone(),
            }),
            Arc::new(RecordingLoop {
                kind: "planner",
                targets: vec![("alpha", 401)],
                log: log.clone(),
            }),
        ],
        log,
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

    let scheduler = Scheduler {
        projects: vec![deps],
        loops: vec![Arc::new(FixedLoop)],
        poll_interval: Duration::from_millis(200),
        max_concurrent: 2,
        reload: None,
    };
    let watch = tokio::spawn(async move { scheduler.watch().await });

    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::time::Instant::now() > deadline {
            panic!(
                "fixed-loop run never succeeded; runs: {:?}",
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
    assert_eq!(runs.len(), 1, "one run per discovered target: {runs:?}");
    assert_eq!(runs[0].loop_kind, "fixed");
}

/// A loop that records the `config.language` each of its drives sees:
/// issues 71 and 72, driven straight to success.
struct LanguageRecordingLoop {
    log: Arc<Mutex<Vec<Option<String>>>>,
}

#[async_trait::async_trait]
impl Loop for LanguageRecordingLoop {
    fn kind(&self) -> &'static str {
        "lang"
    }

    async fn discover(&self, deps: &Deps) -> anyhow::Result<Vec<Target>> {
        let mut targets = Vec::new();
        for n in [71, 72] {
            if !deps
                .store
                .issue_has_succeeded_run(&deps.project.id, self.kind(), n)?
            {
                targets.push(Target {
                    key: TaskKey::Issue(n),
                    title: format!("lang {n}"),
                });
            }
        }
        Ok(targets)
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> anyhow::Result<WorkerOutcome> {
        self.log.lock().unwrap().push(deps.config.language.clone());
        deps.store
            .update_run_status(run_id, RunStatus::Succeeded, None)?;
        Ok(WorkerOutcome::Succeeded {
            pr_url: "lang://pr".into(),
        })
    }
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

    let scheduler = Scheduler {
        projects: vec![deps],
        loops: vec![Arc::new(LanguageRecordingLoop { log: log.clone() })],
        poll_interval: Duration::from_millis(100),
        // One slot: issue 71 drives before the "edit", issue 72 after it.
        max_concurrent: 1,
        reload: Some(reload),
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
