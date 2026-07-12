//! End-to-end worker-loop tests with FakeMux + FakeForge and a real local
//! git origin. A scripted "agent" task plays the pane side: it watches the
//! worktree for prompt files and reacts (commit work, write results).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::worker::{WorkerOutcome, run_worker};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    Forge, LABEL_AUTOMERGE, LABEL_IMPLEMENTING, LABEL_NEEDS_HUMAN, LABEL_PLAN, LABEL_READY,
    LABEL_WORKING,
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
    mux: Arc<FakeMux>,
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
    // These happy-path tests don't exercise the self-review phase; the
    // dedicated self-review tests enable it explicitly.
    config.review.enabled = false;
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: "me/proj".into(),
        default_branch: "main".into(),
        language: None,
        check_command: check_command.map(str::to_string),
        worktree_root: Some(worktree_root.clone()),
        pr: None,
        clean: None,
    };

    let mux = Arc::new(FakeMux::new(false));
    let deps = Deps {
        store: Store::open_in_memory().unwrap(),
        notifier: meguri::notify::fake::recording_notifier().0,
        mux: mux.clone(),
        forge: forge.clone(),
        config,
        project,
    };
    TestEnv {
        deps,
        forge,
        mux,
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
    write_result_with(worktree, turn_id, status, None);
}

fn write_result_with(worktree: &Path, turn_id: &str, status: &str, pr_body: Option<&str>) {
    let mut result = serde_json::json!({
        "turn_id": turn_id, "status": status, "summary": "scripted",
    });
    if let Some(body) = pr_body {
        result["pr_body"] = serde_json::Value::String(body.into());
    }
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

/// Scripted pane-side agent: `action` runs exactly once per new prompt turn
/// (deduplicated by turn id, so slow actions aren't re-fired by the poll).
fn spawn_scripted_agent<F>(worktree_root: PathBuf, mut action: F) -> tokio::task::JoinHandle<u32>
where
    F: FnMut(u32, &Path, &str) + Send + 'static,
{
    tokio::spawn(async move {
        let mut seen: HashSet<String> = HashSet::new();
        for _ in 0..600 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let Some(wt) = find_worktree(&worktree_root) else {
                continue;
            };
            if let Some(turn_id) = pending_turn(&wt)
                && seen.insert(turn_id.clone())
            {
                action(seen.len() as u32, &wt, &turn_id);
            }
        }
        seen.len() as u32
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

    // PR recorded with the right shape: draft by default, agent summary as
    // the fallback body (no pr_body written), no raw issue-body excerpt.
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].base, "main");
    assert!(prs[0].head.starts_with("meguri/7-add-greeting-file-"));
    assert!(prs[0].body.contains("Closes #7"));
    assert!(prs[0].draft, "pr.draft defaults to true");
    assert!(prs[0].body.contains("scripted"), "body: {}", prs[0].body);
    assert!(
        !prs[0].body.contains("Create `greeting.txt`"),
        "issue body must no longer be embedded: {}",
        prs[0].body
    );

    // Without a repo PR template, the execute prompt carries the default one.
    let wt = find_worktree(&env.worktree_root).unwrap();
    let prompts = prompts_in(&wt);
    let execute_prompt = prompts
        .iter()
        .find(|p| p.contains("# Issue:"))
        .expect("execute prompt exists");
    assert!(execute_prompt.contains("# Pull request description"));
    assert!(execute_prompt.contains("## Summary"));

    // Phase settled (ADR 0005): the claim + ready trigger are gone and the
    // issue moved to `implementing` (its implementation PR is open) — exactly
    // one phase label, no escalation.
    let labels = env.forge.labels_of(7);
    assert!(
        !labels.contains(&LABEL_READY.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
    assert!(
        labels.contains(&LABEL_IMPLEMENTING.to_string()),
        "issue must carry {LABEL_IMPLEMENTING} after the PR opens: {labels:?}"
    );

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
async fn worker_uses_agent_pr_body_in_pr() {
    let env = setup(None).await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_greeting(&wt).await;
            write_result_with(
                &wt,
                &turn_id,
                "success",
                Some("## Summary\nAdded greeting.txt so newcomers get a hello."),
            );
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1);
    assert!(prs[0].body.contains("Closes #7"));
    assert!(
        prs[0]
            .body
            .contains("## Summary\nAdded greeting.txt so newcomers get a hello."),
        "body: {}",
        prs[0].body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_pr_draft_false_opens_normal_pr() {
    let mut env = setup(None).await;
    env.deps.config.pr.draft = false;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
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

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1);
    assert!(!prs[0].draft, "pr.draft = false must open a normal PR");
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_automerge_issue_opens_nondraft_and_copies_label() {
    // `meguri:automerge` on the issue: the PR opens non-draft (even though
    // pr.draft defaults to true) and the label is copied onto the PR so the
    // auto-merger sweep can arm it (auto-merge 1/3, #41).
    let env = setup(None).await;
    env.forge.add_label(7, LABEL_AUTOMERGE).await.unwrap();
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
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

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1);
    assert!(
        !prs[0].draft,
        "automerge PR opens non-draft despite pr.draft = true"
    );
    let pr_labels = env.forge.pr_labels_of(prs[0].number);
    assert!(
        pr_labels.contains(&LABEL_AUTOMERGE.to_string()),
        "automerge label copied onto the PR: {pr_labels:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_prompt_carries_repo_pr_template() {
    let env = setup(None).await;

    // Ship a PR template in the repo so the worktree (cut from origin/main)
    // contains it.
    let clone = env.deps.project.repo_path.clone();
    std::fs::create_dir_all(clone.join(".github")).unwrap();
    std::fs::write(
        clone.join(".github/pull_request_template.md"),
        "## Repo Template\n- custom section\n",
    )
    .unwrap();
    run_git(&clone, &["add", ".github"]).await.unwrap();
    run_git(&clone, &["commit", "-m", "add PR template"])
        .await
        .unwrap();
    run_git(&clone, &["push", "origin", "main"]).await.unwrap();

    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
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

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let wt = find_worktree(&env.worktree_root).unwrap();
    let prompts = prompts_in(&wt);
    let execute_prompt = prompts
        .iter()
        .find(|p| p.contains("# Issue:"))
        .expect("execute prompt exists");
    assert!(
        execute_prompt.contains("## Repo Template"),
        "repo template must win over the default"
    );
    assert!(!execute_prompt.contains("<what & why>"));
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
async fn worker_skips_quietly_when_ready_label_removed_after_discovery() {
    let env = setup(None).await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // The benign race: between discovery and claim, a concurrent run
    // succeeded and removed the ready label.
    env.forge.remove_label(7, LABEL_READY).await.unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(30), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();

    assert!(
        matches!(outcome, WorkerOutcome::Skipped(_)),
        "expected skip, got {outcome:?}"
    );
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Skipped);

    // Quiet skip: no escalation label, no claim, no comment.
    let labels = env.forge.labels_of(7);
    assert!(
        !labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(env.forge.comments_of(7).is_empty());
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
async fn worker_needs_plan_hands_issue_to_planner() {
    let env = setup(None).await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // The agent investigates and finds a design decision is needed first.
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_plan");
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    let WorkerOutcome::NeedsPlan(reason) = outcome else {
        panic!("expected NeedsPlan, got {outcome:?}");
    };
    assert_eq!(reason, "scripted");

    // Normal ending, not a failure.
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::NeedsPlan);

    // Labels swapped: the claim and the ready trigger are gone, the planner
    // trigger is on, and nobody called for a human.
    let labels = env.forge.labels_of(7);
    assert!(
        labels.contains(&LABEL_PLAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_READY.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));

    // The findings are on the issue for the planner's next poll.
    let comments = env.forge.comments_of(7);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("scripted"), "comment: {}", comments[0]);
    assert!(comments[0].contains(LABEL_PLAN));

    // The execute prompt invited the signal.
    let wt = find_worktree(&env.worktree_root).unwrap();
    let prompts = prompts_in(&wt);
    assert!(prompts.iter().any(|p| p.contains("needs_plan")));

    // No PR, nothing pushed.
    assert!(env.forge.prs().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_needs_plan_with_existing_spec_escalates_to_human() {
    let env = setup(None).await;

    // The issue already went through planning: its spec is merged on main,
    // so the worker's worktree contains it.
    let clone = env.deps.project.repo_path.clone();
    std::fs::create_dir_all(clone.join("docs/specs")).unwrap();
    std::fs::write(clone.join("docs/specs/issue-7.md"), "# Spec\n").unwrap();
    run_git(&clone, &["add", "docs/specs"]).await.unwrap();
    run_git(&clone, &["commit", "-m", "add spec"])
        .await
        .unwrap();
    run_git(&clone, &["push", "origin", "main"]).await.unwrap();

    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_plan");
    });

    let result = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out");
    agent.abort();

    // The one-shot rule: a second needs-plan is not a plan handoff.
    assert!(
        result.is_err(),
        "needs-plan with an existing spec must fail"
    );
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);

    let labels = env.forge.labels_of(7);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_PLAN.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));

    let comments = env.forge.comments_of(7);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("needs a human"), "{}", comments[0]);
    assert!(comments[0].contains("docs/specs/issue-7.md"));
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_claim_clears_stale_needs_human_from_previous_run() {
    let env = setup(None).await;
    // A previous run escalated and left its label behind; this retry run
    // discovers the issue with both ready and needs-human.
    env.forge.add_label(7, LABEL_NEEDS_HUMAN).await.unwrap();
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // Snapshot the issue labels as the agent's first turn starts, i.e.
    // right after the claim.
    let labels_at_claim: Arc<Mutex<Option<Vec<String>>>> = Arc::new(Mutex::new(None));
    let forge = env.forge.clone();
    let snapshot = labels_at_claim.clone();
    let agent = spawn_scripted_agent(env.worktree_root.clone(), move |_, wt, turn_id| {
        snapshot
            .lock()
            .unwrap()
            .get_or_insert_with(|| forge.labels_of(7));
        write_result(wt, turn_id, "needs_human");
    });

    let result = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out");
    agent.abort();

    // The claim superseded the previous escalation.
    let seen = labels_at_claim.lock().unwrap().clone().expect("agent ran");
    assert!(
        !seen.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels at claim: {seen:?}"
    );
    assert!(seen.contains(&LABEL_WORKING.to_string()));

    // ...and this run's own failure re-escalates as before.
    assert!(result.is_err(), "needs_human must fail the run");
    let labels = env.forge.labels_of(7);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
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

// ---- self-review phase (ADR 0006) ----------------------------------------

/// Read the prompt delivered for a turn, so a scripted agent can tell the
/// execute / review / fix turns apart.
fn prompt_of(wt: &Path, turn_id: &str) -> String {
    std::fs::read_to_string(wt.join(format!(".meguri/prompt-{turn_id}.md"))).unwrap_or_default()
}

fn write_review(wt: &Path, verdict: &str, findings: serde_json::Value) {
    let body = serde_json::json!({
        "verdict": verdict, "review": "self-review note", "findings": findings,
    });
    std::fs::write(
        wt.join(meguri::engine::impl_reviewer::REVIEW_FILE),
        body.to_string(),
    )
    .unwrap();
}

async fn commit_fix(wt: &Path) {
    std::fs::write(wt.join("greeting.txt"), "hello there\n").unwrap();
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
            "Address self-review",
        ],
    )
    .await
    .unwrap();
}

const REVIEW_MARK: &str = "self-review round";
const FIX_MARK: &str = "# Findings";

#[tokio::test(flavor = "multi_thread")]
async fn self_review_clean_publishes_without_touching_the_forge() {
    let mut env = setup(None).await;
    env.deps.config.review.enabled = true;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            if prompt_of(&wt, &turn_id).contains(REVIEW_MARK) {
                write_review(&wt, "clean", serde_json::json!([]));
            } else {
                commit_greeting(&wt).await;
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    // A self-review actually ran and resolved clean...
    let kinds: Vec<String> = env
        .deps
        .store
        .events_for_run(&run.id, 200)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(
        kinds.contains(&"self_review.reviewed".to_string()),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&"self_review.clean".to_string()),
        "{kinds:?}"
    );

    // ...without leaving a single thread or comment on the PR (the forge is
    // untouched by the self-review leg).
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1);
    let pr = prs[0].number;
    assert!(
        env.forge.threads_of(pr).is_empty(),
        "self-review must post no threads"
    );
    assert!(
        env.forge.pr_comments_of(pr).is_empty(),
        "self-review must post no comments"
    );
    // A clean review leaves no footer on the PR body.
    assert!(
        !prs[0].body.contains("self-review"),
        "body: {}",
        prs[0].body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn self_review_findings_then_fix_converge_in_one_run() {
    use std::sync::atomic::{AtomicU32, Ordering};

    let mut env = setup(None).await;
    env.deps.config.review.enabled = true;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let reviews = Arc::new(AtomicU32::new(0));
    let r = reviews.clone();
    let agent = spawn_scripted_agent(env.worktree_root.clone(), move |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        let r = r.clone();
        tokio::spawn(async move {
            let prompt = prompt_of(&wt, &turn_id);
            if prompt.contains(REVIEW_MARK) {
                // First review flags a finding; the second (post-fix) is clean.
                if r.fetch_add(1, Ordering::SeqCst) == 0 {
                    write_review(
                        &wt,
                        "findings",
                        serde_json::json!([
                            {"path": "greeting.txt", "line": 1, "body": "make it friendlier"}
                        ]),
                    );
                } else {
                    write_review(&wt, "clean", serde_json::json!([]));
                }
            } else if prompt.contains(FIX_MARK) {
                commit_fix(&wt).await;
            } else {
                commit_greeting(&wt).await;
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
    assert_eq!(
        reviews.load(Ordering::SeqCst),
        2,
        "review→fix→review ran in one run"
    );

    let kinds: Vec<String> = env
        .deps
        .store
        .events_for_run(&run.id, 200)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(
        kinds.contains(&"self_review.fixed".to_string()),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&"self_review.clean".to_string()),
        "{kinds:?}"
    );

    // Still no forge threads/comments: the review→fix stayed local.
    let pr = env.forge.prs()[0].number;
    assert!(env.forge.threads_of(pr).is_empty());
    assert!(env.forge.pr_comments_of(pr).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn self_review_publishes_with_footer_when_rounds_run_out() {
    let mut env = setup(None).await;
    env.deps.config.review.enabled = true;
    env.deps.config.review.max_rounds = 2;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // The review never converges; the fix "addresses" it each round.
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            let prompt = prompt_of(&wt, &turn_id);
            if prompt.contains(REVIEW_MARK) {
                write_review(
                    &wt,
                    "findings",
                    serde_json::json!([
                        {"path": "greeting.txt", "line": 1, "body": "still not right"}
                    ]),
                );
            } else if prompt.contains(FIX_MARK) {
                commit_fix(&wt).await;
            } else {
                commit_greeting(&wt).await;
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(90), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    // The cap does not block: the PR is published anyway.
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let kinds: Vec<String> = env
        .deps
        .store
        .events_for_run(&run.id, 200)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(
        kinds.contains(&"self_review.unconverged".to_string()),
        "{kinds:?}"
    );

    // The only trace on the PR is the single footer line — no threads, no
    // conversation.
    let prs = env.forge.prs();
    let pr = prs[0].number;
    assert!(env.forge.threads_of(pr).is_empty());
    assert!(env.forge.pr_comments_of(pr).is_empty());
    assert!(
        prs[0].body.contains("self-review"),
        "unconverged PR needs a footer line: {}",
        prs[0].body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn self_review_turn_uses_the_impl_reviewer_profile() {
    // Model separation survives the internal loop: the review turn spawns
    // under the `impl-reviewer` profile, the author's turns under `worker`.
    let mut env = setup(None).await;
    let mut config: Config = toml::from_str(
        r#"
[agents.profiles.p-worker]
command = "worker-cli"
args = ["--go"]

[agents.profiles.p-review]
command = "review-cli"
args = ["--review"]

[routing]
mode = "manual"

[routing.roles]
worker = "p-worker"
impl-reviewer = "p-review"
"#,
    )
    .unwrap();
    config.limits.idle_grace_secs = 3600;
    config.limits.result_grace_secs = 1;
    config.review.enabled = true;
    env.deps.config = config;

    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            if prompt_of(&wt, &turn_id).contains(REVIEW_MARK) {
                write_review(&wt, "clean", serde_json::json!([]));
            } else {
                commit_greeting(&wt).await;
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let commands = env.mux.spawned_commands();
    assert!(
        commands
            .iter()
            .any(|c| c.first().map(String::as_str) == Some("review-cli")),
        "the review turn must spawn under the impl-reviewer profile: {commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|c| c.first().map(String::as_str) == Some("worker-cli")),
        "the author turns must spawn under the worker profile: {commands:?}"
    );
}
