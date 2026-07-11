//! Watch-loop tests: discovery dispatches labeled issues; startup recovery
//! resumes runs orphaned by a dead orchestrator.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::scheduler::Scheduler;
use meguri::engine::{Deps, Loop, Target, WorkerOutcome, default_loops};
use meguri::forge::fake::FakeForge;
use meguri::forge::{Forge, LABEL_READY, LABEL_WORKING};
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
    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600;
    config.limits.result_grace_secs = 1;
    Deps {
        store: Store::open_in_memory().unwrap(),
        mux: Arc::new(FakeMux::new(false)),
        forge,
        config,
        project: ProjectConfig {
            id: "proj".into(),
            repo_path: clone,
            repo_slug: "me/proj".into(),
            default_branch: "main".into(),
            check_command: None,
            worktree_root: Some(root.join("worktrees")),
            pr: None,
        },
    }
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
            issue_number: 99,
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
