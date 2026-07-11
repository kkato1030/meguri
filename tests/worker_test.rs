//! End-to-end worker-loop tests with FakeMux + FakeForge and a real local
//! git origin. A scripted "agent" task plays the pane side: it watches the
//! worktree for prompt files and reacts (commit work, write results).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::worker::{WorkerOutcome, run_worker};
use meguri::forge::fake::FakeForge;
use meguri::forge::{LABEL_NEEDS_HUMAN, LABEL_READY, LABEL_WORKING};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};

async fn init_origin_and_clone(root: &Path) -> (PathBuf, PathBuf) {
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
    (origin, clone)
}

struct TestEnv {
    deps: Deps,
    forge: Arc<FakeForge>,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    worktree_root: PathBuf,
}

async fn setup(check_command: Option<&str>) -> TestEnv {
    let root = tempfile::tempdir().unwrap();
    let (_origin, clone) = init_origin_and_clone(root.path()).await;
    let worktree_root = root.path().join("worktrees");

    let forge = Arc::new(FakeForge::with_issue(
        7,
        "Add greeting file",
        "Create `greeting.txt` containing hello.",
        &[LABEL_READY],
    ));

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600; // scripted agent: no nudging wanted
    config.limits.result_grace_secs = 1; // FakeMux always reads Working; don't linger
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: "me/proj".into(),
        default_branch: "main".into(),
        check_command: check_command.map(str::to_string),
        worktree_root: Some(worktree_root.clone()),
    };

    let deps = Deps {
        store: Store::open_in_memory().unwrap(),
        mux: Arc::new(FakeMux::new(false)),
        forge: forge.clone(),
        config,
        project,
    };
    TestEnv {
        deps,
        forge,
        root,
        worktree_root,
    }
}

fn find_worktree(worktree_root: &Path) -> Option<PathBuf> {
    let proj = worktree_root.join("proj");
    let entries = std::fs::read_dir(proj).ok()?;
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
}

fn pending_turn(worktree: &Path) -> Option<String> {
    // A prompt file whose turn id doesn't yet have a matching result.
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
    if Some(&latest) == current_result.as_ref() {
        None
    } else {
        Some(latest)
    }
}

fn write_result(worktree: &Path, turn_id: &str, status: &str) {
    std::fs::write(
        worktree.join(".meguri/result.json"),
        format!(r#"{{"turn_id":"{turn_id}","status":"{status}","summary":"scripted"}}"#),
    )
    .unwrap();
}

/// Scripted pane-side agent: for each new prompt turn, run `action`.
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

#[tokio::test(flavor = "multi_thread")]
async fn worker_happy_path_issue_to_pr() {
    let env = setup(Some("test -f greeting.txt")).await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        // Commit synchronously-ish inside the poll task.
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

    let WorkerOutcome::Succeeded { pr_url } = outcome else {
        panic!("expected success, got {outcome:?}");
    };
    assert!(pr_url.contains("fake.example"));

    // Run record is terminal and complete.
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Succeeded);
    assert_eq!(record.step, "open-pr");

    // PR recorded with the right shape.
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].base, "main");
    assert!(prs[0].head.starts_with("meguri/7-add-greeting-file-"));
    assert!(prs[0].body.contains("Closes #7"));

    // Labels settled: claim + trigger removed, no escalation.
    let labels = env.forge.labels_of(7);
    assert!(
        !labels.contains(&LABEL_READY.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));

    // The branch actually landed on origin.
    let clone = &env.deps.project.repo_path;
    let branches = run_git(clone, &["ls-remote", "--heads", "origin"])
        .await
        .unwrap();
    assert!(
        branches.contains("meguri/7-add-greeting-file-"),
        "{branches}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_corrective_turn_when_no_commits() {
    let env = setup(None).await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // Turn 1: claim success without committing anything (a lying agent).
    // Turn 2 (the corrective turn): actually do the work.
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |turn, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            if turn == 1 {
                write_result(&wt, &turn_id, "success");
            } else {
                commit_greeting(&wt).await;
                write_result(&wt, &turn_id, "success");
            }
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    // The corrective loop must have recorded the mismatch.
    let events = env.deps.store.events_for_run(&run.id, 100).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "execute.correction"),
        "missing correction event: {:?}",
        events.iter().map(|e| e.kind.clone()).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_needs_human_escalates_on_forge() {
    let env = setup(None).await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_human");
    });

    let result = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out");
    agent.abort();

    assert!(result.is_err(), "needs_human must fail the run");
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);

    let labels = env.forge.labels_of(7);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    // Ready stays: a human can re-triage and requeue.
    assert!(labels.contains(&LABEL_READY.to_string()));

    let comments = env.forge.comments_of(7);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("needs a human"));
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_validation_failure_feeds_back_then_passes() {
    // Validation requires TWO files; the scripted agent only creates the
    // second one when the fix-validation prompt arrives.
    let env = setup(Some("test -f greeting.txt && test -f extra.txt")).await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |turn, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            if turn == 1 {
                commit_greeting(&wt).await;
            } else {
                std::fs::write(wt.join("extra.txt"), "fixed\n").unwrap();
                run_git(&wt, &["add", "extra.txt"]).await.unwrap();
                run_git(
                    &wt,
                    &[
                        "-c",
                        "user.email=a@example.com",
                        "-c",
                        "user.name=agent",
                        "commit",
                        "-m",
                        "Add extra file",
                    ],
                )
                .await
                .unwrap();
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(90), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let events = env.deps.store.events_for_run(&run.id, 200).unwrap();
    let kinds: Vec<String> = events.iter().map(|e| e.kind.clone()).collect();
    assert!(kinds.contains(&"validate.failed".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"validate.passed".to_string()), "{kinds:?}");
}
