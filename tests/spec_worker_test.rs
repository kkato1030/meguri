//! End-to-end spec-worker-loop tests with FakeMux + FakeForge and a real
//! local git origin: an open spec PR labeled `meguri:spec-ready` gets
//! implementation commits stacked onto its existing branch — no second PR.
//! A scripted "agent" plays the pane side (same protocol as worker_test).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::spec_worker::{self, SpecWorkerLoop, run_spec_worker};
use meguri::engine::{Deps, Loop, WorkerOutcome};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    Forge, LABEL_HOLD, LABEL_IMPLEMENTING, LABEL_NEEDS_HUMAN, LABEL_SPEC_READY, LABEL_SPECCING,
    LABEL_WORKING,
};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};

/// The spec PR's branch, following the worker/planner naming convention for
/// issue #5 (the takeover parses the issue number out of it).
const SPEC_BRANCH: &str = "meguri/5-add-caching-layer-abc123";

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

/// Seed what a finished planner run leaves behind: the spec branch (one
/// commit containing only `docs/specs/issue-5.md`) pushed to origin, the
/// clone itself back on main. Returns the spec head sha.
async fn seed_spec_branch(clone: &Path) -> String {
    run_git(clone, &["checkout", "-b", SPEC_BRANCH])
        .await
        .unwrap();
    std::fs::create_dir_all(clone.join("docs/specs")).unwrap();
    std::fs::write(
        clone.join("docs/specs/issue-5.md"),
        "# Spec: Add caching layer\n\n- acceptance: cache.txt exists\n",
    )
    .unwrap();
    run_git(clone, &["add", "."]).await.unwrap();
    run_git(clone, &["commit", "-m", "Add spec for issue 5"])
        .await
        .unwrap();
    let sha = run_git(clone, &["rev-parse", "HEAD"]).await.unwrap();
    run_git(clone, &["push", "-u", "origin", SPEC_BRANCH])
        .await
        .unwrap();
    run_git(clone, &["checkout", "main"]).await.unwrap();
    sha
}

struct TestEnv {
    deps: Deps,
    forge: Arc<FakeForge>,
    spec_head: String,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    worktree_root: PathBuf,
}

async fn setup(check_command: Option<&str>) -> TestEnv {
    let root = tempfile::tempdir().unwrap();
    let (_origin, clone) = init_origin_and_clone(root.path()).await;
    let spec_head = seed_spec_branch(&clone).await;
    let worktree_root = root.path().join("worktrees");

    // The issue the planner consumed `meguri:plan` from — it now carries the
    // `meguri:speccing` phase label (ADR 0005) — plus the reviewed spec PR
    // that carries `meguri:spec-ready`.
    let forge = Arc::new(FakeForge::with_issue(
        5,
        "Add caching layer",
        "Requests are slow; add a cache.",
        &[LABEL_SPECCING],
    ));
    forge.add_pr(
        1,
        "Spec: Add caching layer (#5)",
        "Closes #5.",
        &[LABEL_SPEC_READY],
        SPEC_BRANCH,
        &spec_head,
    );

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
        clean: None,
    };

    let deps = Deps {
        store: Store::open_in_memory().unwrap(),
        notifier: meguri::notify::fake::recording_notifier().0,
        mux: Arc::new(FakeMux::new(false)),
        forge: forge.clone(),
        config,
        project,
    };
    TestEnv {
        deps,
        forge,
        spec_head,
        root,
        worktree_root,
    }
}

fn create_spec_worker_run(env: &TestEnv) -> meguri::store::RunRecord {
    env.deps
        .store
        .create_run_for_loop("proj", spec_worker::KIND, 5, "Spec: Add caching layer (#5)")
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
        "turn_id": turn_id, "status": status, "summary": "scripted implementation",
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

async fn commit_implementation(wt: &Path) {
    std::fs::write(wt.join("cache.txt"), "cached\n").unwrap();
    // The spec is disposable scaffolding: implementation prunes it. Idempotent
    // on purpose — a fix-validation turn may run after the spec is already gone.
    let _ = std::fs::remove_file(wt.join("docs/specs/issue-5.md"));
    run_git(wt, &["add", "-A"]).await.unwrap();
    run_git(
        wt,
        &[
            "-c",
            "user.email=a@example.com",
            "-c",
            "user.name=agent",
            "commit",
            "-m",
            "Implement caching layer",
        ],
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn spec_worker_happy_path_spec_ready_pr_to_implementation_commits() {
    // The check command also proves the spec was pruned from the branch.
    let env = setup(Some("test -f cache.txt && test ! -f docs/specs/issue-5.md")).await;

    // Discovery keys the run to the issue the branch encodes, not the PR.
    let targets = SpecWorkerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.issue_number).collect::<Vec<_>>(),
        vec![5]
    );

    let run = create_spec_worker_run(&env);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_implementation(&wt).await;
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome =
        tokio::time::timeout(Duration::from_secs(60), run_spec_worker(&env.deps, &run.id))
            .await
            .expect("spec worker timed out")
            .unwrap();
    agent.abort();

    let WorkerOutcome::Succeeded { pr_url } = outcome else {
        panic!("expected success, got {outcome:?}");
    };
    assert_eq!(pr_url, "https://fake.example/pr/1", "the existing spec PR");

    // Run record is terminal and complete under the spec-worker loop kind,
    // tied to the issue and the spec PR's branch.
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Succeeded);
    assert_eq!(record.step, "open-pr");
    assert_eq!(record.loop_kind, spec_worker::KIND);
    assert_eq!(record.issue_number, 5);
    assert_eq!(record.branch.as_deref(), Some(SPEC_BRANCH));

    // No second PR was created: the spec PR is the implementation PR.
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1, "the takeover must never open a new PR");
    assert_eq!(prs[0].number, 1);

    // Presentation transitioned from spec to implementation (issue #98): the
    // planner opened it as `Spec: Add caching layer (#5)` with a `Closes #5.`
    // body; settle dropped the `Spec:` prefix and rewrote the body to the
    // implementation description the agent authored.
    assert_eq!(
        prs[0].title, "Add caching layer (#5)",
        "the `Spec:` prefix must be gone"
    );
    assert!(
        prs[0].body.contains("scripted implementation"),
        "body must reflect the implementation, not the spec: {}",
        prs[0].body
    );
    assert!(
        prs[0].body.contains("Opened by [meguri]"),
        "body keeps the meguri footer: {}",
        prs[0].body
    );

    // The execute prompt carried the issue AND the reviewed spec's contents.
    let wt = find_worktree(&env.worktree_root).unwrap();
    let prompts = prompts_in(&wt);
    let execute_prompt = prompts
        .iter()
        .find(|p| p.contains("# Issue:"))
        .expect("execute prompt exists");
    assert!(execute_prompt.contains("# Issue: Add caching layer"));
    assert!(execute_prompt.contains("Requests are slow; add a cache."));
    assert!(execute_prompt.contains("# Reviewed spec (`docs/specs/issue-5.md`)"));
    assert!(
        execute_prompt.contains("- acceptance: cache.txt exists"),
        "spec contents must be embedded: {execute_prompt}"
    );
    assert!(execute_prompt.contains("the PR already exists"));
    assert!(
        execute_prompt.contains("delete `docs/specs/issue-5.md`"),
        "the prune instruction must be in the prompt: {execute_prompt}"
    );
    assert!(
        execute_prompt.contains("# Pull request description"),
        "the takeover authors pr_body so settle can rewrite the PR body: {execute_prompt}"
    );

    // Label transition on the PR: spec-ready consumed, claim released, no
    // escalation — the PR is now ordinary fixer territory.
    let labels = env.forge.pr_labels_of(1);
    assert!(
        !labels.contains(&LABEL_SPEC_READY.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
    // Phase flip on the issue (ADR 0005): the claim moved it from speccing to
    // implementing — the spec PR is now an implementation PR.
    let issue_labels = env.forge.labels_of(5);
    assert!(
        !issue_labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {issue_labels:?}"
    );
    assert!(
        !issue_labels.contains(&LABEL_SPECCING.to_string()),
        "speccing must be gone once implementation starts: {issue_labels:?}"
    );
    assert!(
        issue_labels.contains(&LABEL_IMPLEMENTING.to_string()),
        "issue must carry {LABEL_IMPLEMENTING} once the claim succeeds: {issue_labels:?}"
    );

    // The implementation commit actually landed on the spec branch at origin.
    let clone = &env.deps.project.repo_path;
    run_git(clone, &["fetch", "origin", SPEC_BRANCH])
        .await
        .unwrap();
    let tip = run_git(clone, &["rev-parse", "FETCH_HEAD"]).await.unwrap();
    assert_ne!(tip, env.spec_head, "origin tip must move past the spec");
    let ahead = run_git(clone, &["rev-list", "--count", "origin/main..FETCH_HEAD"])
        .await
        .unwrap();
    assert_eq!(ahead, "2", "spec commit + implementation commit");
    let specs_in_tree = run_git(
        clone,
        &["ls-tree", "--name-only", "FETCH_HEAD", "docs/specs/"],
    )
    .await
    .unwrap();
    assert!(
        !specs_in_tree.contains("issue-5.md"),
        "the spec must be pruned by the implementation commit: {specs_in_tree}"
    );

    // Success dedups discovery even while the fake label state lingers
    // elsewhere: a second takeover of the same issue is never queued.
    env.forge.add_pr_label(1, LABEL_SPEC_READY).await.unwrap();
    assert!(SpecWorkerLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn spec_worker_needs_human_escalates_like_the_worker() {
    let env = setup(None).await;
    let run = create_spec_worker_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_human");
    });

    let result = tokio::time::timeout(Duration::from_secs(60), run_spec_worker(&env.deps, &run.id))
        .await
        .expect("spec worker timed out");
    agent.abort();

    assert!(result.is_err(), "needs_human must fail the run");
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);

    // Same escalation as the worker: needs-human label + comment on the
    // issue; the PR claim is released and spec-ready stays for a retrigger.
    // The phase label survives the escalation (ADR 0005): the claim already
    // flipped the issue to implementing, so it reads as "stuck in
    // implementation" (implementing + needs-human), not "stuck in spec".
    let labels = env.forge.labels_of(5);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(
        labels.contains(&LABEL_IMPLEMENTING.to_string()),
        "the phase label must survive needs-human: {labels:?}"
    );
    let comments = env.forge.comments_of(5);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("needs a human"));

    let pr_labels = env.forge.pr_labels_of(1);
    assert!(!pr_labels.contains(&LABEL_WORKING.to_string()));
    assert!(pr_labels.contains(&LABEL_SPEC_READY.to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn spec_worker_skips_quietly_when_label_removed_after_discovery() {
    let env = setup(None).await;
    let run = create_spec_worker_run(&env);

    // The benign race: spec-ready vanished between discovery and claim.
    env.forge
        .remove_pr_label(1, LABEL_SPEC_READY)
        .await
        .unwrap();

    let outcome =
        tokio::time::timeout(Duration::from_secs(30), run_spec_worker(&env.deps, &run.id))
            .await
            .expect("spec worker timed out")
            .unwrap();

    assert!(
        matches!(outcome, WorkerOutcome::Skipped(_)),
        "expected skip, got {outcome:?}"
    );
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Skipped);

    // Quiet skip: no claim, no escalation, no comment anywhere.
    assert!(
        !env.forge
            .pr_labels_of(1)
            .contains(&LABEL_WORKING.to_string())
    );
    assert!(
        !env.forge
            .labels_of(5)
            .contains(&LABEL_NEEDS_HUMAN.to_string())
    );
    assert!(env.forge.comments_of(5).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn spec_worker_discovery_filters_hold_working_foreign_and_shipped() {
    let env = setup(None).await;

    // Alongside the actionable spec PR: held, claimed, non-meguri-branch,
    // and closed ones (each on its own issue-numbered branch).
    for (number, branch, labels, state) in [
        (
            2,
            "meguri/6-held-abc",
            vec![LABEL_SPEC_READY, LABEL_HOLD],
            "open",
        ),
        (
            3,
            "meguri/7-claimed-abc",
            vec![LABEL_SPEC_READY, LABEL_WORKING],
            "open",
        ),
        (4, "feature/manual", vec![LABEL_SPEC_READY], "open"),
        (5, "meguri/8-merged-abc", vec![LABEL_SPEC_READY], "merged"),
    ] {
        env.forge
            .add_pr(number, "other", "", &labels, branch, "sha");
        env.forge.set_pr_state(number, state);
    }

    let targets = SpecWorkerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.issue_number).collect::<Vec<_>>(),
        vec![5]
    );

    // A *worker* success on the issue must not block the takeover...
    let done = env
        .deps
        .store
        .create_run_for_loop("proj", meguri::engine::worker::KIND, 5, "t")
        .unwrap();
    env.deps
        .store
        .update_run_status(&done.id, RunStatus::Succeeded, None)
        .unwrap();
    assert_eq!(SpecWorkerLoop.discover(&env.deps).await.unwrap().len(), 1);

    // ...but a spec-worker success does (the spec-ready label lingered).
    let shipped = create_spec_worker_run(&env);
    env.deps
        .store
        .update_run_status(&shipped.id, RunStatus::Succeeded, None)
        .unwrap();
    assert!(SpecWorkerLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn spec_worker_validation_failure_feeds_back_then_passes() {
    // Validation requires the implementation file; the scripted agent only
    // creates it when the fix-validation prompt arrives.
    let env = setup(Some("test -f cache.txt")).await;
    let run = create_spec_worker_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |turn, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            if turn == 1 {
                // Committed work with the spec pruned (so execute-verify
                // passes), but not what validation wants: no cache.txt yet.
                std::fs::write(wt.join("notes.txt"), "wip\n").unwrap();
                std::fs::remove_file(wt.join("docs/specs/issue-5.md")).unwrap();
                run_git(&wt, &["add", "-A"]).await.unwrap();
                run_git(
                    &wt,
                    &[
                        "-c",
                        "user.email=a@example.com",
                        "-c",
                        "user.name=agent",
                        "commit",
                        "-m",
                        "notes",
                    ],
                )
                .await
                .unwrap();
            } else {
                commit_implementation(&wt).await;
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome =
        tokio::time::timeout(Duration::from_secs(90), run_spec_worker(&env.deps, &run.id))
            .await
            .expect("spec worker timed out")
            .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let events = env.deps.store.events_for_run(&run.id, 200).unwrap();
    let kinds: Vec<String> = events.iter().map(|e| e.kind.clone()).collect();
    assert!(kinds.contains(&"validate.failed".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"validate.passed".to_string()), "{kinds:?}");
    assert_eq!(env.forge.prs().len(), 1, "still no second PR");
}
