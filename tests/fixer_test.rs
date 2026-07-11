//! End-to-end fixer-loop tests with FakeMux + FakeForge and a real local
//! git origin: an open meguri PR with unresolved review comments gets fix
//! commits pushed onto its existing branch, and the reviewer↔fixer
//! ping-pong converges. A scripted "agent" plays the pane side (same
//! protocol as worker_test / planner_test).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::fixer::{self, FIXER_REPLY_MARKER, FixerLoop, run_fixer};
use meguri::engine::{Deps, Loop, WorkerOutcome};
use meguri::forge::fake::FakeForge;
use meguri::forge::{Forge, LABEL_HOLD, LABEL_NEEDS_HUMAN, LABEL_SPEC_READY, LABEL_WORKING};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};

const PR_BRANCH: &str = "meguri/9-add-feature-abc123";

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

/// Seed what a finished worker run leaves behind: the PR branch pushed to
/// origin (the clone itself stays on main, branch not checked out anywhere).
async fn seed_pr_branch(clone: &Path) {
    run_git(clone, &["branch", PR_BRANCH]).await.unwrap();
    run_git(clone, &["push", "origin", PR_BRANCH])
        .await
        .unwrap();
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
    seed_pr_branch(&clone).await;
    let worktree_root = root.path().join("worktrees");

    // The PR the worker shipped earlier, now carrying a review comment.
    let forge = Arc::new(FakeForge::default());
    let pr = forge.push_pr(PR_BRANCH, "Add feature (#9)", &[]);
    assert_eq!(pr, 1);
    forge.add_review_thread(
        1,
        "t1",
        "feature.txt",
        "reviewer",
        "Please fix the wording in this file.",
    );

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
        language: None,
        pr: None,
        clean: None,
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

fn create_fixer_run(env: &TestEnv) -> meguri::store::RunRecord {
    env.deps
        .store
        .create_run_for_loop("proj", fixer::KIND, 1, "Add feature (#9)")
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
        "turn_id": turn_id, "status": status, "summary": "scripted fix",
    });
    std::fs::write(worktree.join(".meguri/result.json"), result.to_string()).unwrap();
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

async fn commit_fix(wt: &Path, contents: &str) {
    std::fs::write(wt.join("feature.txt"), contents).unwrap();
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
            "Address review comments",
        ],
    )
    .await
    .unwrap();
}

async fn origin_tip(clone: &Path) -> String {
    let refs = run_git(clone, &["ls-remote", "--heads", "origin", PR_BRANCH])
        .await
        .unwrap();
    refs.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn fixer_happy_path_pushes_fix_to_pr_branch_and_replies() {
    // The check command also proves validation runs inside the fix worktree.
    let env = setup(Some("test -f feature.txt")).await;
    let run = create_fixer_run(&env);
    let tip_before = origin_tip(&env.deps.project.repo_path).await;

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_fix(&wt, "fixed wording\n").await;
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_fixer(&env.deps, &run.id))
        .await
        .expect("fixer timed out")
        .unwrap();
    agent.abort();

    let WorkerOutcome::Succeeded { pr_url } = outcome else {
        panic!("expected success, got {outcome:?}");
    };
    // The existing PR is the deliverable: no new PR was opened.
    assert_eq!(pr_url, "https://fake.example/pr/1");
    assert_eq!(env.forge.prs().len(), 1);

    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Succeeded);
    assert_eq!(record.loop_kind, fixer::KIND);
    assert_eq!(record.branch.as_deref(), Some(PR_BRANCH));

    // The fix commit landed on the PR's branch on origin.
    let tip_after = origin_tip(&env.deps.project.repo_path).await;
    assert_ne!(tip_before, tip_after, "origin tip must advance");

    // The thread is parked: meguri replied last, asking for re-review.
    let threads = env.forge.threads_of(1);
    assert_eq!(threads.len(), 1);
    let last = threads[0].comments.last().unwrap();
    assert_eq!(last.author, "meguri");
    assert!(last.body.starts_with(FIXER_REPLY_MARKER), "{}", last.body);
    assert!(!threads[0].resolved, "resolution is the reviewer's call");

    // Claim released, no escalation; and the parked PR is no longer
    // discoverable until the reviewer answers.
    let labels = env.forge.pr_labels(1);
    assert!(!labels.contains(&LABEL_WORKING.to_string()), "{labels:?}");
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
    assert!(FixerLoop.discover(&env.deps).await.unwrap().is_empty());

    // The prompt carried the review comment and the no-push rule.
    let wt = find_worktree(&env.worktree_root).unwrap();
    let prompt = std::fs::read_dir(wt.join(".meguri"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("prompt-"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .find(|p| p.contains("Unresolved review comments"))
        .expect("execute prompt exists");
    assert!(prompt.contains("Please fix the wording"));
    assert!(prompt.contains("Do NOT push"));
}

#[tokio::test(flavor = "multi_thread")]
async fn fixer_reviewer_ping_pong_converges() {
    let env = setup(None).await;

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |turn, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_fix(&wt, &format!("attempt {turn}\n")).await;
            write_result(&wt, &turn_id, "success");
        });
    });

    // Round 1: the reviewer's comment gets fixed and pushed.
    let targets = FixerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.issue_number).collect::<Vec<_>>(),
        vec![1]
    );
    let run1 = create_fixer_run(&env);
    let outcome = tokio::time::timeout(Duration::from_secs(60), run_fixer(&env.deps, &run1.id))
        .await
        .expect("fixer round 1 timed out")
        .unwrap();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let tip_round1 = origin_tip(&env.deps.project.repo_path).await;

    // Parked: awaiting re-review, discovery stays quiet.
    assert!(FixerLoop.discover(&env.deps).await.unwrap().is_empty());

    // Round 2: the reviewer pushes back on the same thread.
    env.forge
        .add_thread_comment(1, "t1", "reviewer", "Closer, but still wrong.");
    let targets = FixerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(targets.len(), 1, "reviewer reply must reopen the ping-pong");

    let run2 = create_fixer_run(&env);
    assert_ne!(run1.id, run2.id, "each round is its own run");
    let outcome = tokio::time::timeout(Duration::from_secs(60), run_fixer(&env.deps, &run2.id))
        .await
        .expect("fixer round 2 timed out")
        .unwrap();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    agent.abort();

    let tip_round2 = origin_tip(&env.deps.project.repo_path).await;
    assert_ne!(tip_round1, tip_round2, "round 2 must push a new fix");

    // The reviewer accepts: the thread resolves and the loop converges.
    env.forge.resolve_thread(1, "t1");
    assert!(
        FixerLoop.discover(&env.deps).await.unwrap().is_empty(),
        "resolved threads must end the ping-pong"
    );

    // Both fixer replies are on the thread, in order, after each push.
    let threads = env.forge.threads_of(1);
    let meguri_replies: Vec<_> = threads[0]
        .comments
        .iter()
        .filter(|c| c.body.starts_with(FIXER_REPLY_MARKER))
        .collect();
    assert_eq!(meguri_replies.len(), 2);
    assert!(meguri_replies[0].body.contains(&run1.id));
    assert!(meguri_replies[1].body.contains(&run2.id));
}

#[tokio::test(flavor = "multi_thread")]
async fn fixer_discovery_skips_spec_ready_merged_held_and_foreign_prs() {
    let env = setup(None).await;

    // PR #2: spec approved — the worker owns the branch now.
    let pr = env.forge.push_pr(
        "meguri/12-spec-def456",
        "Spec: thing (#12)",
        &[LABEL_SPEC_READY],
    );
    env.forge
        .add_review_thread(pr, "t-spec", "docs/spec.md", "reviewer", "nit");
    // PR #3: merged — history is immutable.
    let pr = env.forge.push_pr("meguri/13-old-abc999", "Old (#13)", &[]);
    env.forge
        .add_review_thread(pr, "t-old", "old.txt", "reviewer", "too late");
    env.forge.set_pr_state(pr, "merged");
    // PR #4: a human's PR — not meguri's to touch.
    let pr = env.forge.push_pr("feature/manual", "Manual work", &[]);
    env.forge
        .add_review_thread(pr, "t-human", "x.txt", "reviewer", "please fix");
    // PR #5: on hold.
    let pr = env
        .forge
        .push_pr("meguri/14-held-aaa111", "Held (#14)", &[LABEL_HOLD]);
    env.forge
        .add_review_thread(pr, "t-held", "y.txt", "reviewer", "fix");
    // PR #6: already claimed by another host.
    let pr = env
        .forge
        .push_pr("meguri/15-busy-bbb222", "Busy (#15)", &[LABEL_WORKING]);
    env.forge
        .add_review_thread(pr, "t-busy", "z.txt", "reviewer", "fix");

    let targets = FixerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.issue_number).collect::<Vec<_>>(),
        vec![1],
        "only the open, unclaimed meguri PR with an awaiting thread is actionable"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn fixer_skips_quietly_when_pr_flips_spec_ready_after_discovery() {
    let env = setup(None).await;
    let run = create_fixer_run(&env);

    // The benign race: the spec was approved between discovery and claim.
    env.forge.add_pr_label(1, LABEL_SPEC_READY).await.unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(30), run_fixer(&env.deps, &run.id))
        .await
        .expect("fixer timed out")
        .unwrap();

    assert!(
        matches!(outcome, WorkerOutcome::Skipped(_)),
        "expected skip, got {outcome:?}"
    );
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Skipped);

    // Quiet skip: no claim, no escalation, no comment, branch untouched.
    let labels = env.forge.pr_labels(1);
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
    assert!(env.forge.comments_of(1).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn fixer_needs_human_escalates_on_the_pr() {
    let env = setup(None).await;
    let run = create_fixer_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_human");
    });

    let result = tokio::time::timeout(Duration::from_secs(60), run_fixer(&env.deps, &run.id))
        .await
        .expect("fixer timed out");
    agent.abort();

    assert!(result.is_err(), "needs_human must fail the run");
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);

    // Escalation lands on the PR: needs-human label + comment, claim released.
    let labels = env.forge.pr_labels(1);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));

    let comments = env.forge.comments_of(1);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("needs a human"));

    // The thread is NOT parked: nothing was pushed, so once a human clears
    // the needs-human state the comment is still actionable.
    let threads = env.forge.threads_of(1);
    assert_eq!(threads[0].comments.len(), 1);
}
