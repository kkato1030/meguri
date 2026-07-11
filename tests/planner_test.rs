//! End-to-end planner-loop tests with FakeMux + FakeForge and a real local
//! git origin: a `meguri:plan` issue becomes a spec PR labeled
//! `meguri:spec-reviewing`. A scripted "agent" plays the pane side (same
//! protocol as worker_test).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::planner::{self, PlannerLoop, run_planner, spec_rel_path};
use meguri::engine::worker;
use meguri::engine::{Deps, Loop, WorkerOutcome};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    Forge, LABEL_HOLD, LABEL_NEEDS_HUMAN, LABEL_PLAN, LABEL_SPEC_REVIEWING, LABEL_WORKING,
};
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
        5,
        "Add caching layer",
        "Requests are slow; add a cache.",
        &[LABEL_PLAN],
    ));

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600; // scripted agent: no nudging wanted
    config.limits.result_grace_secs = 1; // FakeMux always reads Working; don't linger
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: "me/proj".into(),
        default_branch: "main".into(),
        language: None,
        check_command: check_command.map(str::to_string),
        worktree_root: Some(worktree_root.clone()),
        pr: None,
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

fn create_planner_run(env: &TestEnv) -> meguri::store::RunRecord {
    env.deps
        .store
        .create_run_for_loop("proj", planner::KIND, 5, "Add caching layer")
        .unwrap()
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
    let result = serde_json::json!({
        "turn_id": turn_id, "status": status, "summary": "scripted spec",
    });
    std::fs::write(worktree.join(".meguri/result.json"), result.to_string()).unwrap();
}

/// Contents of the prompt files delivered to the (scripted) agent.
fn prompts_in(worktree: &Path) -> Vec<String> {
    std::fs::read_dir(worktree.join(".meguri"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("prompt-") && name.ends_with(".md")
        })
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect()
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

async fn commit_files(wt: &Path, files: &[(&str, &str)], message: &str) {
    for (rel, contents) in files {
        let path = wt.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    run_git(wt, &["add", "."]).await.unwrap();
    run_git(
        wt,
        &[
            "-c",
            "user.email=a@example.com",
            "-c",
            "user.name=agent",
            "commit",
            "-m",
            message,
        ],
    )
    .await
    .unwrap();
}

async fn commit_spec(wt: &Path) {
    commit_files(
        wt,
        &[(
            "docs/specs/issue-5.md",
            "# Spec: Add caching layer\n\n- acceptance criteria\n- files to touch\n- decisions\n",
        )],
        "Add spec for issue 5",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_happy_path_plan_issue_to_spec_pr() {
    // The check command also proves spec-only changes survive validation.
    let env = setup(Some("test -f docs/specs/issue-5.md")).await;
    let run = create_planner_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_spec(&wt).await;
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out")
        .unwrap();
    agent.abort();

    let WorkerOutcome::Succeeded { pr_url } = outcome else {
        panic!("expected success, got {outcome:?}");
    };
    assert!(pr_url.contains("fake.example"));

    // Run record is terminal and complete under the planner loop kind.
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Succeeded);
    assert_eq!(record.step, "open-pr");
    assert_eq!(record.loop_kind, planner::KIND);

    // Spec PR shape: Spec-prefixed title, worker branch conventions (the
    // worker later takes this same branch over), spec-reviewing label on
    // the PR.
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].base, "main");
    assert_eq!(prs[0].title, "Spec: Add caching layer (#5)");
    assert!(
        prs[0].head.starts_with("meguri/5-add-caching-layer-"),
        "branch must follow the worker naming convention: {}",
        prs[0].head
    );
    assert!(prs[0].body.contains("Closes #5"));
    assert!(prs[0].draft, "pr.draft defaults to true");
    assert!(
        prs[0].labels.contains(&LABEL_SPEC_REVIEWING.to_string()),
        "spec PR must carry {LABEL_SPEC_REVIEWING}: {:?}",
        prs[0].labels
    );

    // Label transition on the issue: plan (and the claim) are gone, no
    // escalation.
    let labels = env.forge.labels_of(5);
    assert!(
        !labels.contains(&LABEL_PLAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));

    // The prompt asked for a spec, not an implementation.
    let wt = find_worktree(&env.worktree_root).unwrap();
    let prompts = prompts_in(&wt);
    let execute_prompt = prompts
        .iter()
        .find(|p| p.contains("# Issue:"))
        .expect("execute prompt exists");
    assert!(execute_prompt.contains(&spec_rel_path(5)));
    assert!(execute_prompt.contains("do NOT implement"));
    assert!(execute_prompt.contains("# Pull request description"));

    // The spec branch actually landed on origin (the worker resumes there).
    let clone = &env.deps.project.repo_path;
    let branches = run_git(clone, &["ls-remote", "--heads", "origin"])
        .await
        .unwrap();
    assert!(
        branches.contains("meguri/5-add-caching-layer-"),
        "{branches}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_corrective_turn_when_spec_missing() {
    let env = setup(None).await;
    let run = create_planner_run(&env);

    // Turn 1: commit *something* but not the spec (a misbehaving agent).
    // Turn 2 (the corrective turn): write the actual spec.
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |turn, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            if turn == 1 {
                commit_files(&wt, &[("notes.txt", "wip\n")], "notes").await;
            } else {
                commit_spec(&wt).await;
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    // The corrective loop recorded the missing spec.
    let events = env.deps.store.events_for_run(&run.id, 100).unwrap();
    let correction = events
        .iter()
        .find(|e| e.kind == "execute.correction")
        .unwrap_or_else(|| {
            panic!(
                "missing correction event: {:?}",
                events.iter().map(|e| e.kind.clone()).collect::<Vec<_>>()
            )
        });
    assert!(
        correction
            .data
            .to_string()
            .contains("docs/specs/issue-5.md"),
        "correction must name the spec file: {}",
        correction.data
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_needs_human_escalates_on_forge() {
    let env = setup(None).await;
    let run = create_planner_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_human");
    });

    let result = tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out");
    agent.abort();

    assert!(result.is_err(), "needs_human must fail the run");
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);

    // Same escalation as the worker: needs-human label + comment, claim
    // released, plan stays for a human to re-triage.
    let labels = env.forge.labels_of(5);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(labels.contains(&LABEL_PLAN.to_string()));

    let comments = env.forge.comments_of(5);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("needs a human"));
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_skips_quietly_when_plan_label_removed_after_discovery() {
    let env = setup(None).await;
    let run = create_planner_run(&env);

    // The benign race: the plan label vanished between discovery and claim.
    env.forge.remove_label(5, LABEL_PLAN).await.unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(30), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out")
        .unwrap();

    assert!(
        matches!(outcome, WorkerOutcome::Skipped(_)),
        "expected skip, got {outcome:?}"
    );
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Skipped);

    // Quiet skip: no escalation label, no claim, no comment.
    let labels = env.forge.labels_of(5);
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(env.forge.comments_of(5).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_discovery_filters_hold_working_and_shipped() {
    let env = setup(None).await;

    // Alongside the actionable plan issue: held, claimed, and unlabeled ones.
    for (number, labels) in [
        (6, vec![LABEL_PLAN.to_string(), LABEL_HOLD.to_string()]),
        (7, vec![LABEL_PLAN.to_string(), LABEL_WORKING.to_string()]),
        (8, vec![]),
    ] {
        env.forge.issues.lock().unwrap().push(meguri::forge::Issue {
            number,
            title: format!("issue {number}"),
            body: String::new(),
            labels,
        });
    }

    let targets = PlannerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.issue_number).collect::<Vec<_>>(),
        vec![5]
    );

    // A *worker* success on the issue must not block the planner...
    let done = env
        .deps
        .store
        .create_run_for_loop("proj", worker::KIND, 5, "Add caching layer")
        .unwrap();
    env.deps
        .store
        .update_run_status(&done.id, RunStatus::Succeeded, None)
        .unwrap();
    assert_eq!(PlannerLoop.discover(&env.deps).await.unwrap().len(), 1);

    // ...but a planner success does (the plan label lingered).
    let shipped = create_planner_run(&env);
    env.deps
        .store
        .update_run_status(&shipped.id, RunStatus::Succeeded, None)
        .unwrap();
    assert!(PlannerLoop.discover(&env.deps).await.unwrap().is_empty());
}
