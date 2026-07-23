//! End-to-end worker-loop tests with FakeMux + FakeForge and a real local
//! git origin. A scripted "agent" task plays the pane side: it watches the
//! worktree for prompt files and reacts (commit work, write results).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meguri::config::{Config, LaunchMode, PrConfig, ProjectConfig, RepoConfig, RepoPrConfig};
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
    // This suite plays the scripted agent through FakeMux (pane protocol);
    // pin self-reviewer to pane so the self-review tests below don't fall
    // through to its recommended `direct` mode, which would spawn a *real*
    // `claude` subprocess instead of going through the fake (issue #169).
    config
        .launch
        .roles
        .insert("self-reviewer".into(), LaunchMode::Pane);
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: Some(clone),
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: check_command.map(str::to_string),
        worktree_root: Some(worktree_root.clone()),
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

    let mux = Arc::new(FakeMux::new(false));
    let deps = Deps::with_label_source(
        Store::open_in_memory().unwrap(),
        mux.clone(),
        forge.clone(),
        config,
        project,
    );
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
    let clone = env.deps.project.repo_path.as_ref().unwrap();
    let branches = run_git(clone, &["ls-remote", "--heads", "origin"])
        .await
        .unwrap();
    assert!(
        branches.contains("meguri/7-add-greeting-file-"),
        "{branches}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_skips_pr_when_rail_external_pr_already_linked_to_issue() {
    // issue #249: a human already opened a hand-written PR for this issue on
    // a non-meguri branch (the #236/#237/#238 double-delivery scenario). The
    // worker must not open a second PR — it escalates instead.
    let env = setup(Some("test -f greeting.txt")).await;
    env.forge.add_pr(
        99,
        "Hand-written fix",
        "",
        &[],
        "adr/0026-review-efficacy",
        "deadbeef",
    );
    env.forge.link_pr_to_issue(7, 99);

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

    let result = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out");
    agent.abort();

    assert!(
        result.is_err(),
        "a rail-external linked PR must escalate, not succeed"
    );
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);

    // No second PR was opened — only the pre-existing hand-written one.
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1, "worker must not open a duplicate PR: {prs:?}");
    assert_eq!(prs[0].number, 99);

    let labels = env.forge.labels_of(7);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );

    let comments = env.forge.comments_of(7);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains('#'), "{}", comments[0]);
    assert!(comments[0].contains("99"), "{}", comments[0]);
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
    let clone = env.deps.project.repo_path.clone().unwrap();
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

/// A stopped mux (herdr/tmux down) must fail the run but never occupy the
/// needs-human queue — it says nothing about the issue and clears itself
/// once the dependency is back (design doc §3-E / P6, issue #250).
#[tokio::test(flavor = "multi_thread")]
async fn worker_mux_down_does_not_escalate_to_needs_human() {
    let env = setup(None).await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    env.mux.stop();

    let result = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out");

    assert!(result.is_err(), "a stopped mux must fail the run");
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);

    let labels = env.forge.labels_of(7);
    assert!(
        !labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(
        !labels.contains(&LABEL_WORKING.to_string()),
        "the claim must be released so the next sweep retries"
    );
    assert!(labels.contains(&LABEL_READY.to_string()));
    assert!(
        env.forge.comments_of(7).is_empty(),
        "an infra fault must not leave a needs-human comment"
    );

    let events = env.deps.store.events_for_run(&run.id, 20).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "infra.raised"),
        "events: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.kind == "escalation.raised"),
        "events: {events:?}"
    );
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
    let clone = env.deps.project.repo_path.clone().unwrap();
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
async fn worker_needs_plan_a_second_time_escalates_to_human() {
    let env = setup(None).await;

    // A first worker run already retreated to planning on this issue, but no
    // spec ever landed on disk (issue #135's other vibration-guard leg).
    let first_run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    env.deps
        .store
        .update_run_status(&first_run.id, RunStatus::NeedsPlan, None)
        .unwrap();

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

    // The one-shot rule: a second needs-plan on the same issue is not a plan
    // handoff, even without a spec file present.
    assert!(
        result.is_err(),
        "a second needs-plan on the same issue must fail"
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
    assert!(comments[0].contains("already retreated"));
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
        wt.join(meguri::engine::self_review::REVIEW_FILE),
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

/// Write the per-turn result file for an isolated (parallel review) turn
/// (issue #214): `.meguri/result-<turn_id>.json`.
fn write_isolated_result(wt: &Path, turn_id: &str, status: &str) {
    let result = serde_json::json!({
        "turn_id": turn_id, "status": status, "summary": "scripted",
    });
    std::fs::write(
        wt.join(format!(".meguri/result-{turn_id}.json")),
        result.to_string(),
    )
    .unwrap();
}

/// The per-reviewer review file named in a parallel round-1 prompt (issue #214),
/// e.g. `.meguri/self-review-r0.json`, or None for a non-parallel review prompt.
fn parallel_review_file_in(prompt: &str) -> Option<String> {
    let start = prompt.find(".meguri/self-review-r")?;
    let rest = &prompt[start..];
    let end = rest.find(".json")? + ".json".len();
    Some(rest[..end].to_string())
}

/// Whether a turn's result is present (shared `result.json` or the per-turn
/// `result-<id>.json`), matching by turn id.
fn result_present(meguri: &Path, id: &str) -> bool {
    let matches = |path: PathBuf| -> bool {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| {
                v.get("turn_id")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
            })
            .as_deref()
            == Some(id)
    };
    matches(meguri.join(format!("result-{id}.json"))) || matches(meguri.join("result.json"))
}

/// Every prompt turn id without a matching result — surfaces CONCURRENT pending
/// turns (issue #214 parallel review), unlike `pending_turn` which returns one.
fn pending_turns(wt: &Path) -> Vec<String> {
    let meguri = wt.join(".meguri");
    let Ok(rd) = std::fs::read_dir(&meguri) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = name
                .strip_prefix("prompt-")?
                .strip_suffix(".md")?
                .to_string();
            (!result_present(&meguri, &id)).then_some(id)
        })
        .collect()
}

/// A scripted agent that services CONCURRENT pending turns (issue #214), firing
/// `action` once per new turn id across all panes.
fn spawn_multi_agent<F>(worktree_root: PathBuf, mut action: F) -> tokio::task::JoinHandle<u32>
where
    F: FnMut(&Path, &str) + Send + 'static,
{
    tokio::spawn(async move {
        let mut seen: HashSet<String> = HashSet::new();
        for _ in 0..900 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let Some(wt) = find_worktree(&worktree_root) else {
                continue;
            };
            for turn_id in pending_turns(&wt) {
                if seen.insert(turn_id.clone()) {
                    action(&wt, &turn_id);
                }
            }
        }
        seen.len() as u32
    })
}

/// Pull the finding ids (`f1`, `f2`, …) out of the `# Findings` section of a fix
/// prompt, so a scripted fix agent can declare a disposition for each (issue
/// #212). Only the findings block is scanned, not the instruction bullets.
fn fix_ids_from_prompt(prompt: &str) -> Vec<String> {
    let start = prompt.find("# Findings").unwrap_or(0);
    let end = prompt[start..]
        .find("# Instructions")
        .map(|i| start + i)
        .unwrap_or(prompt.len());
    prompt[start..end]
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("- `")?;
            let e = rest.find('`')?;
            Some(rest[..e].to_string())
        })
        .collect()
}

/// A fix turn (issue #212): make a unique commit (so back-to-back fix turns each
/// have something to commit) and write a `fixed` disposition for every open
/// finding named in the prompt.
async fn commit_fix_and_dispositions(wt: &Path, turn_id: &str, prompt: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(wt.join("greeting.txt"))
        .unwrap();
    f.write_all(format!("fix {turn_id}\n").as_bytes()).unwrap();
    drop(f);
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
    let dispositions: Vec<_> = fix_ids_from_prompt(prompt)
        .iter()
        .map(|id| serde_json::json!({ "id": id, "action": "fixed" }))
        .collect();
    std::fs::write(
        wt.join(meguri::engine::self_review::FIX_FILE),
        serde_json::json!({ "dispositions": dispositions }).to_string(),
    )
    .unwrap();
}

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
    // A clean review still records its inspection history in a folded
    // <details> (ADR 0008) — but no non-convergence warning.
    assert!(
        prs[0].body.contains("<details>") && prs[0].body.contains("self-review"),
        "clean review still folds a self-review summary: {}",
        prs[0].body
    );
    assert!(
        !prs[0].body.contains("収束しませんでした"),
        "a clean review must not warn about non-convergence: {}",
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
                commit_fix_and_dispositions(&wt, &turn_id, &prompt).await;
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

/// Configure N parallel round-1 reviewers under detectable profiles (`git` is
/// always present, so `detect_command` succeeds; FakeMux fakes the launch).
fn enable_parallel_reviewers(env: &mut TestEnv, n: usize) {
    use meguri::config::{AgentProfile, AgentsConfig, ReviewerConfig};
    env.deps.config.review.enabled = true;
    let mut profiles = std::collections::HashMap::new();
    let mut reviewers = Vec::new();
    for i in 0..n {
        let name = format!("rev{i}");
        profiles.insert(
            name.clone(),
            AgentProfile {
                command: "git".into(),
                ..Default::default()
            },
        );
        reviewers.push(ReviewerConfig {
            profile: Some(name),
            lenses: None,
        });
    }
    env.deps.config.agents = Some(AgentsConfig { profiles });
    env.deps.config.review.reviewers = reviewers;
}

/// Issue #214: round 1 fans out to two parallel reviewers, their findings union-
/// merge, a fix turn clears them, and round 2 (single anchor) converges. Both
/// reviewers report, and the per-turn result files never collide.
#[tokio::test(flavor = "multi_thread")]
async fn parallel_round1_reviewers_merge_then_converge() {
    let mut env = setup(None).await;
    enable_parallel_reviewers(&mut env, 2);
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let agent = spawn_multi_agent(env.worktree_root.clone(), |wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            let prompt = prompt_of(&wt, &turn_id);
            if let Some(review_file) = parallel_review_file_in(&prompt) {
                // r0 flags one finding; r1 is clean. Union → one finding.
                let body = if review_file.contains("self-review-r0") {
                    serde_json::json!({
                        "verdict": "fixable", "review": "r0 note",
                        "findings": [{"path": "greeting.txt", "line": 1, "kind": "defect", "body": "friendlier"}],
                    })
                } else {
                    serde_json::json!({"verdict": "clean", "review": "", "findings": []})
                };
                std::fs::write(wt.join(&review_file), body.to_string()).unwrap();
                write_isolated_result(&wt, &turn_id, "success");
            } else if prompt.contains(REVIEW_MARK) {
                // Round 2: single anchor reviewer, clean.
                write_review(&wt, "clean", serde_json::json!([]));
                write_result(&wt, &turn_id, "success");
            } else if prompt.contains(FIX_MARK) {
                commit_fix_and_dispositions(&wt, &turn_id, &prompt).await;
                write_result(&wt, &turn_id, "success");
            } else {
                commit_greeting(&wt).await;
                write_result(&wt, &turn_id, "success");
            }
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(90), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let kinds: Vec<String> = env
        .deps
        .store
        .events_for_run(&run.id, 300)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "self_review.reviewer_reported")
            .count(),
        2,
        "both parallel reviewers reported: {kinds:?}"
    );
    assert!(
        kinds.contains(&"self_review.clean".to_string()),
        "phase converged clean: {kinds:?}"
    );
    // Self-review stayed local — no forge threads.
    let pr = env.forge.prs()[0].number;
    assert!(env.forge.threads_of(pr).is_empty());
}

/// Issue #214, ADR 0023 §2: a parallel `needs_human` is NOT OR'd — an anchor
/// confirmation turn runs, and when it overrules (clean) the phase continues to
/// publish instead of escalating.
#[tokio::test(flavor = "multi_thread")]
async fn parallel_needs_human_is_confirmed_by_anchor_then_overruled() {
    let mut env = setup(None).await;
    enable_parallel_reviewers(&mut env, 2);
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let anchor_turns = Arc::new(Mutex::new(0u32));
    let anchor_seen = anchor_turns.clone();
    let agent = spawn_multi_agent(env.worktree_root.clone(), move |wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        let anchor_seen = anchor_seen.clone();
        tokio::spawn(async move {
            let prompt = prompt_of(&wt, &turn_id);
            if let Some(review_file) = parallel_review_file_in(&prompt) {
                // r0 flags needs_human; r1 is clean.
                let body = if review_file.contains("self-review-r0") {
                    serde_json::json!({"verdict": "needs_human", "review": "is this in scope?", "findings": []})
                } else {
                    serde_json::json!({"verdict": "clean", "review": "", "findings": []})
                };
                std::fs::write(wt.join(&review_file), body.to_string()).unwrap();
                write_isolated_result(&wt, &turn_id, "success");
            } else if prompt.contains("anchor reviewer confirming") {
                // The anchor overrules the escalation: clean, publish.
                *anchor_seen.lock().unwrap() += 1;
                write_review(&wt, "clean", serde_json::json!([]));
                write_result(&wt, &turn_id, "success");
            } else if prompt.contains(REVIEW_MARK) {
                write_review(&wt, "clean", serde_json::json!([]));
                write_result(&wt, &turn_id, "success");
            } else if prompt.contains(FIX_MARK) {
                commit_fix_and_dispositions(&wt, &turn_id, &prompt).await;
                write_result(&wt, &turn_id, "success");
            } else {
                commit_greeting(&wt).await;
                write_result(&wt, &turn_id, "success");
            }
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(90), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "anchor overruled needs_human → publish, not escalate: {outcome:?}"
    );
    assert_eq!(
        *anchor_turns.lock().unwrap(),
        1,
        "exactly one anchor confirmation turn ran"
    );
    let kinds: Vec<String> = env
        .deps
        .store
        .events_for_run(&run.id, 300)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(
        kinds.contains(&"self_review.anchor_confirm".to_string()),
        "anchor confirmation emitted an event: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"self_review.needs_human".to_string()),
        "overruled → no escalation: {kinds:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn self_review_cap_runs_final_fix_and_publishes() {
    let mut env = setup(None).await;
    env.deps.config.review.enabled = true;
    env.deps.config.review.max_rounds = 2;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // Each review raises a fresh minor finding (no id) — never the same one, so
    // it is not a ping-pong. On the cap, only minor blocking remains, so a final
    // fix + validate publishes instead of escalating (issue #212, ADR 0022).
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            let prompt = prompt_of(&wt, &turn_id);
            if prompt.contains(REVIEW_MARK) {
                write_review(
                    &wt,
                    "fixable",
                    serde_json::json!([
                        {"path": "greeting.txt", "line": 1, "body": "one more nit"}
                    ]),
                );
            } else if prompt.contains(FIX_MARK) {
                commit_fix_and_dispositions(&wt, &turn_id, &prompt).await;
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

    // The run succeeds and publishes a real PR — not an escalation.
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
        kinds.contains(&"self_review.final_fix".to_string()),
        "cap with minor remainder runs a final fix: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"self_review.unconverged".to_string()),
        "the final-fix path must not escalate: {kinds:?}"
    );
    // A real delivery (`pr.created`), not an escalate-time evidence draft.
    assert!(
        kinds.contains(&"pr.created".to_string()),
        "a delivered PR is created: {kinds:?}"
    );
    assert!(
        !kinds.contains(&"self_review.escalated_draft".to_string()),
        "the final-fix path delivers, it does not escalate a draft: {kinds:?}"
    );
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1, "one delivered PR");
    assert!(
        prs[0].body.contains("最終ラウンドの fix は未再レビュー"),
        "the PR body records the un-re-reviewed final fix: {}",
        prs[0].body
    );
    assert!(
        !env.forge
            .pr_labels_of(prs[0].number)
            .contains(&LABEL_NEEDS_HUMAN.to_string()),
        "a delivered PR is not labeled needs-human"
    );
    assert!(
        !env.forge
            .labels_of(7)
            .contains(&LABEL_NEEDS_HUMAN.to_string()),
        "the issue is not parked on a human"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn self_review_ping_pong_escalates() {
    let mut env = setup(None).await;
    env.deps.config.review.enabled = true;
    env.deps.config.review.max_rounds = 3;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // The reviewer keeps re-raising the SAME finding (by id) even though the
    // author "fixes" it each round — a genuine ping-pong. After two fix turns it
    // is still open, so the run escalates to a human (issue #212, reason 2).
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            let prompt = prompt_of(&wt, &turn_id);
            if prompt.contains(REVIEW_MARK) {
                // Round 1 raises it fresh (no id); round 2+ re-lists `f1` by id.
                let findings = if prompt.contains("self-review round 1") {
                    serde_json::json!([{"path": "greeting.txt", "line": 1, "body": "still not right"}])
                } else {
                    serde_json::json!([{"id": "f1", "path": "greeting.txt", "line": 1, "body": "still not right"}])
                };
                write_review(&wt, "fixable", findings);
            } else if prompt.contains(FIX_MARK) {
                commit_fix_and_dispositions(&wt, &turn_id, &prompt).await;
            } else {
                commit_greeting(&wt).await;
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    let result = tokio::time::timeout(Duration::from_secs(90), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out");
    agent.abort();

    assert!(result.is_err(), "a ping-pong must fail the run to a human");
    let kinds: Vec<String> = env
        .deps
        .store
        .events_for_run(&run.id, 200)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(
        kinds.contains(&"self_review.pingpong".to_string()),
        "{kinds:?}"
    );
    assert!(
        !kinds.contains(&"self_review.final_fix".to_string()),
        "a ping-pong escalates, it does not final-fix: {kinds:?}"
    );
    assert!(
        kinds.contains(&"self_review.escalated_draft".to_string()),
        "the committed work is published as evidence: {kinds:?}"
    );
    assert!(
        env.forge
            .labels_of(7)
            .contains(&LABEL_NEEDS_HUMAN.to_string()),
        "{:?}",
        env.forge.labels_of(7)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn final_fix_resumes_into_publish_not_ping_pong() {
    use meguri::engine::flow::{Checkpoint, STEP_SELF_REVIEW};
    use meguri::engine::self_review::FindingStatus;

    // The resume race (issue #212): a run interrupted mid-final-fix, with a
    // finding already at fix_attempts == 2, must resume back INTO the final-fix
    // publish — the persisted `self_review_final_fix_started` marker routes it
    // past the ping-pong check, so it does not mis-escalate. Built by running the
    // cap→final-fix scenario, then rewinding the checkpoint to the interrupted
    // state and resuming on the same worktree.
    let mut env = setup(None).await;
    env.deps.config.review.enabled = true;
    env.deps.config.review.max_rounds = 2;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            let prompt = prompt_of(&wt, &turn_id);
            if prompt.contains(REVIEW_MARK) {
                write_review(
                    &wt,
                    "fixable",
                    serde_json::json!([{"path": "greeting.txt", "line": 1, "body": "one more nit"}]),
                );
            } else if prompt.contains(FIX_MARK) {
                commit_fix_and_dispositions(&wt, &turn_id, &prompt).await;
            } else {
                commit_greeting(&wt).await;
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    // First pass: run to completion so a real worktree/branch/PR exist.
    tokio::time::timeout(Duration::from_secs(90), run_worker(&env.deps, &run.id))
        .await
        .expect("first pass timed out")
        .unwrap();

    // Rewind to the interrupted mid-final-fix state: committed to the final-fix
    // path, one finding open at two fix attempts, not yet re-reviewed.
    let rec = env.deps.store.get_run(&run.id).unwrap().unwrap();
    let mut cp: Checkpoint = serde_json::from_str(&rec.checkpoint_json).unwrap();
    cp.self_review_final_fix_started = true;
    cp.self_review_final_fix_unreviewed = false;
    cp.self_review_converged = false;
    cp.self_review_rounds = env.deps.config.review.max_rounds;
    for e in cp.self_review_ledger.iter_mut() {
        e.status = FindingStatus::Open;
        e.fix_attempts = 2;
    }
    assert!(
        cp.self_review_ledger
            .iter()
            .any(|e| e.status == FindingStatus::Open && e.fix_attempts >= 2),
        "the rewound state must look like a ping-pong to prove it is not treated as one"
    );
    env.deps
        .store
        .update_run_step(
            &run.id,
            STEP_SELF_REVIEW,
            &serde_json::to_string(&cp).unwrap(),
        )
        .unwrap();

    // Resume: it must publish via the final-fix path, never escalate a ping-pong.
    let outcome = tokio::time::timeout(Duration::from_secs(90), run_worker(&env.deps, &run.id))
        .await
        .expect("resume timed out")
        .unwrap();
    agent.abort();

    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "resume must publish, not escalate: {outcome:?}"
    );
    let kinds: Vec<String> = env
        .deps
        .store
        .events_for_run(&run.id, 400)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    // Had the fix been absent, the phase would have escalated a ping-pong and
    // `run_worker` above would have returned Err (unwrap would panic). The
    // absence of the event is the belt-and-braces check.
    assert!(
        !kinds.contains(&"self_review.pingpong".to_string()),
        "the interrupted final fix must not be mis-read as a ping-pong: {kinds:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn self_review_needs_human_escalates_immediately() {
    let mut env = setup(None).await;
    env.deps.config.review.enabled = true;
    env.deps.config.review.max_rounds = 3;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // The very first review classifies the diff as needs_human: a fix round is
    // never spent (issue #176), the run escalates at once.
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            let prompt = prompt_of(&wt, &turn_id);
            if prompt.contains(REVIEW_MARK) {
                write_review(&wt, "needs_human", serde_json::json!([]));
            } else if prompt.contains(FIX_MARK) {
                commit_fix(&wt).await;
            } else {
                commit_greeting(&wt).await;
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    let result = tokio::time::timeout(Duration::from_secs(90), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out");
    agent.abort();

    assert!(result.is_err(), "needs_human self-review must fail the run");
    let kinds: Vec<String> = env
        .deps
        .store
        .events_for_run(&run.id, 200)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(
        kinds.contains(&"self_review.needs_human".to_string()),
        "{kinds:?}"
    );
    // No fix round was spent. The committed work is still published as a
    // needs-human draft (issue #209, ADR 0020) — the diff is unverified evidence,
    // not a delivered PR (`pr.created` is never emitted).
    assert!(
        !kinds.contains(&"self_review.fixed".to_string()),
        "needs_human must not spend a fix round: {kinds:?}"
    );
    assert!(
        kinds.contains(&"self_review.escalated_draft".to_string()),
        "{kinds:?}"
    );
    assert!(!kinds.contains(&"pr.created".to_string()), "{kinds:?}");
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1, "exactly one needs-human draft");
    assert!(prs[0].draft, "published as a draft");
    assert!(
        env.forge
            .pr_labels_of(prs[0].number)
            .contains(&LABEL_NEEDS_HUMAN.to_string()),
        "draft is labeled needs-human at birth: {:?}",
        prs[0].labels
    );
    assert!(
        env.forge
            .labels_of(7)
            .contains(&LABEL_NEEDS_HUMAN.to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn self_review_turn_uses_the_self_reviewer_profile() {
    // Model separation survives the internal loop: the review turn spawns
    // under the `self-reviewer` profile, the author's turns under `worker`.
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
self-reviewer = "p-review"

[launch.roles]
self-reviewer = "pane"
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
        "the review turn must spawn under the self-reviewer profile: {commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|c| c.first().map(String::as_str) == Some("worker-cli")),
        "the author turns must spawn under the worker profile: {commands:?}"
    );
}

// ---- repo config: meguri.toml pinned at run start (issue #165) ------------

/// Commit a repo-root `meguri.toml` on the clone's default branch and push it,
/// so a run's worktree (branched off `origin/main`) checks it out.
async fn seed_repo_toml(clone: &Path, contents: &str) {
    std::fs::write(clone.join("meguri.toml"), contents).unwrap();
    run_git(clone, &["add", "meguri.toml"]).await.unwrap();
    run_git(
        clone,
        &[
            "-c",
            "user.email=t@example.com",
            "-c",
            "user.name=meguri-test",
            "commit",
            "-m",
            "add meguri.toml",
        ],
    )
    .await
    .unwrap();
    run_git(clone, &["push", "origin", "main"]).await.unwrap();
}

/// The command each `validate.running` event recorded, in order — how a test
/// observes which `check_command` the run actually executed.
fn validate_commands(env: &TestEnv, run_id: &str) -> Vec<String> {
    env.deps
        .store
        .events_for_run(run_id, 200)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "validate.running")
        .filter_map(|e| {
            e.data
                .get("command")
                .and_then(|c| c.as_str())
                .map(str::to_string)
        })
        .collect()
}

#[tokio::test]
async fn with_repo_config_folds_under_host_precedence() {
    // Baseline project: host sets none of the repo-eligible keys.
    let env = setup(None).await;
    let repo = RepoConfig {
        language: Some("日本語".into()),
        check_command: Some("cargo test".into()),
        pr: Some(RepoPrConfig { draft: Some(false) }),
    };

    // Host left them unset → repo fills them in.
    let folded = env.deps.with_repo_config(&repo);
    assert_eq!(folded.project.check_command.as_deref(), Some("cargo test"));
    assert_eq!(folded.config.language_for(&folded.project), Some("日本語"));
    assert!(!folded.config.pr_for(&folded.project).draft);
    // auto_merge is never contributed by the repo — it stays host-global.
    assert!(!folded.config.pr_for(&folded.project).auto_merge.enabled);

    // Host [projects.*] override wins wholesale over the repo layer.
    let mut deps = env.deps;
    deps.project.check_command = Some("host-check".into());
    deps.project.language = Some("English".into());
    deps.project.pr = Some(PrConfig {
        draft: true,
        auto_merge: Default::default(),
    });
    let folded = deps.with_repo_config(&repo);
    assert_eq!(folded.project.check_command.as_deref(), Some("host-check"));
    assert_eq!(folded.config.language_for(&folded.project), Some("English"));
    assert!(
        folded.config.pr_for(&folded.project).draft,
        "host [projects.pr] wins wholesale over repo pr.draft"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn repo_check_command_from_meguri_toml_is_applied() {
    // Host project has no check_command; the repo declares one plus pr.draft.
    let env = setup(None).await;
    seed_repo_toml(
        env.deps.project.repo_path.as_ref().unwrap(),
        "check_command = \"test -f greeting.txt\"\n\n[pr]\ndraft = false\n",
    )
    .await;
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

    // The repo's check_command actually ran (not skipped as it would be with a
    // None host command) — proof the repo layer reached validation.
    let cmds = validate_commands(&env, &run.id);
    assert!(
        cmds.iter().any(|c| c == "test -f greeting.txt"),
        "repo check_command must be the one validate ran: {cmds:?}"
    );

    // The repo's pr.draft = false took effect (host default is draft = true).
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1);
    assert!(
        !prs[0].draft,
        "repo pr.draft = false must open a non-draft PR"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn run_pins_repo_check_command_against_mid_run_tamper() {
    // Acceptance criterion 2: a run's completion contract is fixed at claim.
    // The repo's check_command needs greeting.txt; the agent commits it, then
    // rewrites (and commits) meguri.toml to a check that would fail — the run
    // must still validate against the PINNED command, so it succeeds and the
    // tampered command never runs.
    let env = setup(None).await;
    seed_repo_toml(
        env.deps.project.repo_path.as_ref().unwrap(),
        "check_command = \"test -f greeting.txt\"\n",
    )
    .await;
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
            // Tamper: weaken the contract in this branch and commit it (so the
            // worktree stays clean for git verification).
            std::fs::write(wt.join("meguri.toml"), "check_command = \"false\"\n").unwrap();
            run_git(&wt, &["add", "meguri.toml"]).await.unwrap();
            run_git(
                &wt,
                &[
                    "-c",
                    "user.email=a@example.com",
                    "-c",
                    "user.name=agent",
                    "commit",
                    "-m",
                    "weaken check",
                ],
            )
            .await
            .unwrap();
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    // Pinned command ran (greeting.txt exists) → success; the tampered "false"
    // was never executed.
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "run must validate against the pinned command, not the tampered one: {outcome:?}"
    );
    let cmds = validate_commands(&env, &run.id);
    assert!(
        cmds.iter().all(|c| c == "test -f greeting.txt"),
        "only the claim-time pinned command may run, never the tampered `false`: {cmds:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_repo_config_warns_and_falls_back() {
    // Acceptance criterion 4: a broken meguri.toml doesn't kill the run — it is
    // warned about, an event is emitted, and the run continues on host config.
    let env = setup(None).await;
    seed_repo_toml(
        env.deps.project.repo_path.as_ref().unwrap(),
        "check_command = \"x\"\nrepo_slug = \"me/x\"\n", // host-only key → parse error
    )
    .await;
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

    // Run still succeeds (host has no check_command → validation is skipped).
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
        kinds.contains(&"repo_config.invalid".to_string()),
        "an invalid meguri.toml must emit repo_config.invalid: {kinds:?}"
    );
}
