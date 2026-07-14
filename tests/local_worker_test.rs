//! End-to-end local-mode worker tests: `meguri add` → sqlite `tasks` claim →
//! `deliver = "branch"` → task `done`, with no forge at all. The scripted
//! agent plays the pane side exactly as in `worker_test.rs`; the difference is
//! the coordination layer (`LocalTaskSource`) and the deliverable (a verified
//! local branch, no push, no PR).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig, ProjectMode};
use meguri::engine::Deps;
use meguri::engine::worker::{WorkerOutcome, run_worker};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};
use meguri::tasks::{LOCAL_HOST, LocalTaskSource, TaskSource};

async fn init_repo(root: &Path) -> PathBuf {
    // A local clone with a committed `main`; deliver = branch never pushes, so
    // no bare origin is needed — only a real default branch to cut from.
    let clone = root.join("clone");
    std::fs::create_dir_all(&clone).unwrap();
    run_git(&clone, &["init", "-b", "main"]).await.unwrap();
    for args in [
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "meguri-test"],
        vec!["commit", "--allow-empty", "-m", "init"],
    ] {
        run_git(&clone, &args).await.unwrap();
    }
    clone
}

struct TestEnv {
    deps: Deps,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    worktree_root: PathBuf,
}

async fn setup(check_command: Option<&str>) -> TestEnv {
    let root = tempfile::tempdir().unwrap();
    let clone = init_repo(root.path()).await;
    let worktree_root = root.path().join("worktrees");

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600;
    config.limits.result_grace_secs = 1;
    // These happy-path local tests don't exercise the self-review phase (the
    // scripted agent only answers the execute turn); the dedicated self-review
    // tests enable it explicitly.
    config.review.enabled = false;

    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: None,
        mode: ProjectMode::Local,
        deliver: None, // local default is `branch`
        default_branch: "main".into(),
        language: None,
        check_command: check_command.map(str::to_string),
        worktree_root: Some(worktree_root.clone()),
        pr: None,
        clean: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
    };

    let store = Store::open_in_memory().unwrap();
    let task_source: Arc<dyn TaskSource> =
        Arc::new(LocalTaskSource::new(store.clone(), project.id.clone()));
    let deps = Deps {
        store,
        mux: Arc::new(FakeMux::new(false)),
        forge: None,
        task_source,
        notifier: meguri::notify::fake::recording_notifier().0,
        forge_factory: Arc::new(meguri::forge::gh::GhForgeFactory),
        config,
        project,
        open_prs: Default::default(),
    };
    TestEnv {
        deps,
        root,
        worktree_root,
    }
}

fn find_worktree(worktree_root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(worktree_root.join("proj")).ok()?;
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
}

fn pending_turn(worktree: &Path) -> Option<String> {
    let meguri = worktree.join(".meguri");
    let current_result: Option<String> = std::fs::read_to_string(meguri.join("result.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| {
            v.get("turn_id")
                .and_then(|t| t.as_str())
                .map(str::to_string)
        });
    let mut ids: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(&meguri)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = name
                .strip_prefix("prompt-")?
                .strip_suffix(".md")?
                .to_string();
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, id))
        })
        .collect();
    ids.sort();
    let latest = ids.last()?.1.clone();
    (Some(&latest) != current_result.as_ref()).then_some(latest)
}

fn write_result(worktree: &Path, turn_id: &str, status: &str) {
    let result = serde_json::json!({
        "turn_id": turn_id, "status": status, "summary": "scripted local task",
    });
    std::fs::write(worktree.join(".meguri/result.json"), result.to_string()).unwrap();
}

fn spawn_scripted_agent<F>(worktree_root: PathBuf, mut action: F) -> tokio::task::JoinHandle<u32>
where
    F: FnMut(u32, &Path, &str) + Send + 'static,
{
    tokio::spawn(async move {
        let mut turns = 0u32;
        for _ in 0..600 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let Some(wt) = find_worktree(&worktree_root) else {
                continue;
            };
            if let Some(turn_id) = pending_turn(&wt) {
                turns += 1;
                action(turns, &wt, &turn_id);
            }
        }
        turns
    })
}

async fn commit_greeting(wt: &Path) {
    std::fs::write(wt.join("greeting.txt"), "hello\n").unwrap();
    run_git(wt, &["add", "greeting.txt"]).await.unwrap();
    run_git(
        wt,
        &[
            "-c",
            "user.email=a@example.com",
            "-c",
            "user.name=agent",
            "commit",
            "-m",
            "Add greeting file",
        ],
    )
    .await
    .unwrap();
}

/// Acceptance criteria 2 + 4: `meguri add` queues a task; watch's worker run
/// claims it, delivers a verified `meguri/t<id>-…` branch (no push, no PR),
/// and flips the task to `done`.
#[tokio::test(flavor = "multi_thread")]
async fn local_task_to_verified_branch() {
    let env = setup(Some("test -f greeting.txt")).await;
    let task = env
        .deps
        .store
        .create_task(
            "proj",
            "work",
            "Add greeting",
            "Create greeting.txt.",
            "local",
        )
        .unwrap();
    assert_eq!(task.status, "queued");

    let run = env
        .deps
        .store
        .create_run_for_task("proj", "worker", task.id, "Add greeting")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_greeting(&wt).await;
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    // The deliverable is the branch name (open_pr's URL generalized).
    let WorkerOutcome::Succeeded { pr_url } = outcome else {
        panic!("expected success, got {outcome:?}");
    };
    assert!(
        pr_url.starts_with(&format!("meguri/t{}-", task.id)),
        "deliverable should be the t-prefixed branch, got {pr_url}"
    );

    // The task is done and drops out of the default `meguri tasks` listing.
    assert_eq!(
        env.deps.store.get_task(task.id).unwrap().unwrap().status,
        "done"
    );

    // The run landed on the local branch; nothing was pushed (no remote at all).
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Succeeded);
    let clone = &env.deps.project.repo_path;
    let branches = run_git(clone, &["branch", "--list"]).await.unwrap();
    assert!(
        branches.contains(&format!("meguri/t{}-", task.id)),
        "local branch missing: {branches}"
    );
    let remotes = run_git(clone, &["remote"]).await.unwrap();
    assert!(remotes.trim().is_empty(), "no remote expected: {remotes:?}");

    // The execute prompt addressed a local task, not a GitHub issue.
    let wt = find_worktree(&env.worktree_root).unwrap();
    let prompt = std::fs::read_dir(wt.join(".meguri"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| std::fs::read_to_string(e.path()).unwrap_or_default())
        .find(|p| p.contains("# Task:"))
        .expect("local execute prompt exists");
    assert!(prompt.contains("# Task: Add greeting"));
    assert!(!prompt.contains("GitHub issue"));
}

/// Acceptance criterion 3: an already-claimed task is a benign race — the run
/// ends Skipped, exactly like the label-mode `meguri:working` collision.
#[tokio::test(flavor = "multi_thread")]
async fn second_claim_skips_quietly() {
    let env = setup(None).await;
    let task = env
        .deps
        .store
        .create_task("proj", "work", "Add greeting", "", "local")
        .unwrap();
    let run = env
        .deps
        .store
        .create_run_for_task("proj", "worker", task.id, "Add greeting")
        .unwrap();

    // Another host already claimed it between discovery and this run's claim.
    assert!(
        env.deps
            .store
            .claim_task(task.id, "proj", LOCAL_HOST)
            .unwrap()
            .is_some()
    );

    let outcome = tokio::time::timeout(Duration::from_secs(30), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    assert!(
        matches!(outcome, WorkerOutcome::Skipped(_)),
        "expected skip, got {outcome:?}"
    );
    assert_eq!(
        env.deps.store.get_run(&run.id).unwrap().unwrap().status,
        RunStatus::Skipped
    );
}

/// Acceptance criterion 5: a failed run escalates the task to `needs_human`
/// with a reason, and a fresh claim clears it (mirrors the label version).
#[tokio::test(flavor = "multi_thread")]
async fn failure_escalates_then_reclaim_clears() {
    let env = setup(None).await;
    let task = env
        .deps
        .store
        .create_task("proj", "work", "Add greeting", "", "local")
        .unwrap();
    let run = env
        .deps
        .store
        .create_run_for_task("proj", "worker", task.id, "Add greeting")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_human");
    });
    let result = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out");
    agent.abort();
    assert!(result.is_err(), "needs_human must fail the run");

    let escalated = env.deps.store.get_task(task.id).unwrap().unwrap();
    assert_eq!(escalated.status, "needs_human");
    assert!(
        escalated.reason.as_deref().is_some_and(|r| !r.is_empty()),
        "a needs_human task carries a reason"
    );

    // A fresh claim (the next run) un-escalates and clears the reason.
    let reclaimed = env
        .deps
        .store
        .claim_task(task.id, "proj", LOCAL_HOST)
        .unwrap()
        .expect("needs_human is re-claimable");
    assert_eq!(reclaimed.status, "claimed");
    assert_eq!(reclaimed.reason, None);
}
