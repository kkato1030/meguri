//! End-to-end impl-reviewer-loop tests with FakeMux + FakeForge and a real
//! local git origin: a quiet, green meguri implementation PR gets an AI
//! review — findings become inline review threads (the fixer's input, the
//! ping-pong connection) plus a marked summary comment, a clean review posts
//! only the marked comment, and the head marker / rounds cap / kill switch
//! keep the loop finite. A scripted "agent" plays the pane side (same
//! protocol as reviewer_test).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::fixer::FixerLoop;
use meguri::engine::impl_reviewer::{
    self, ImplReviewerLoop, impl_review_marker, run_impl_reviewer,
};
use meguri::engine::reviewer::{DIFF_FILE, REVIEW_FILE};
use meguri::engine::{Deps, Loop, WorkerOutcome};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    CheckState, Forge, LABEL_HOLD, LABEL_NEEDS_HUMAN, LABEL_SPEC_READY, LABEL_SPEC_REVIEWING,
    LABEL_WORKING,
};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};

const PR: i64 = 21;
const PR_BRANCH: &str = "meguri/7-add-cache-impl-xyz789";

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

/// Create the implementation PR branch on origin and return its head sha.
async fn push_impl_branch(clone: &Path) -> String {
    run_git(clone, &["checkout", "-b", PR_BRANCH])
        .await
        .unwrap();
    let src = clone.join("src/cache.rs");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(
        &src,
        "pub fn get(key: &str) -> Option<String> {\n    None\n}\n",
    )
    .unwrap();
    run_git(clone, &["add", "."]).await.unwrap();
    run_git(clone, &["commit", "-m", "Implement cache lookup"])
        .await
        .unwrap();
    run_git(clone, &["push", "-u", "origin", PR_BRANCH])
        .await
        .unwrap();
    let sha = run_git(clone, &["rev-parse", "HEAD"]).await.unwrap();
    run_git(clone, &["checkout", "main"]).await.unwrap();
    sha
}

struct TestEnv {
    deps: Deps,
    forge: Arc<FakeForge>,
    head_sha: String,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    worktree_root: PathBuf,
}

async fn setup() -> TestEnv {
    let root = tempfile::tempdir().unwrap();
    let (_origin, clone) = init_origin_and_clone(root.path()).await;
    let head_sha = push_impl_branch(&clone).await;
    let worktree_root = root.path().join("worktrees");

    let forge = Arc::new(FakeForge::default());
    // An unlabeled open meguri PR: the implementation shipped, labels
    // settled, CI green (FakeForge reports Success when no checks are set).
    forge.add_pr(
        PR,
        "Add cache lookup (#7)",
        "Closes #7.",
        &[],
        PR_BRANCH,
        &head_sha,
    );
    forge.set_pr_diff(
        PR,
        "diff --git a/src/cache.rs b/src/cache.rs\n+pub fn get(key: &str) -> Option<String> {\n",
    );

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600; // scripted agent: no nudging wanted
    config.limits.result_grace_secs = 1; // FakeMux always reads Working; don't linger
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: Some("me/proj".into()),
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: Some(worktree_root.clone()),
        pr: None,
        clean: None,
        mode: Default::default(),
        deliver: None,
    };

    let deps = Deps::with_label_source(
        Store::open_in_memory().unwrap(),
        Arc::new(FakeMux::new(false)),
        forge.clone(),
        config,
        project,
    );
    TestEnv {
        deps,
        forge,
        head_sha,
        root,
        worktree_root,
    }
}

fn create_run(env: &TestEnv) -> meguri::store::RunRecord {
    env.deps
        .store
        .create_run_for_loop("proj", impl_reviewer::KIND, PR, "Add cache lookup (#7)")
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

fn write_review(worktree: &Path, body: serde_json::Value) {
    std::fs::write(worktree.join(REVIEW_FILE), body.to_string()).unwrap();
}

fn write_result(worktree: &Path, turn_id: &str, status: &str) {
    let result = serde_json::json!({
        "turn_id": turn_id, "status": status, "summary": "scripted impl review",
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

#[tokio::test(flavor = "multi_thread")]
async fn findings_create_inline_threads_that_feed_the_fixer() {
    let env = setup().await;
    let run = create_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_review(
            wt,
            serde_json::json!({
                "verdict": "findings",
                "review": "- `get` never populates the cache.",
                "findings": [
                    {"path": "src/cache.rs", "line": 2, "body": "This always returns None; wire up the store."}
                ],
            }),
        );
        write_result(wt, turn_id, "success");
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        run_impl_reviewer(&env.deps, &run.id),
    )
    .await
    .expect("impl reviewer timed out")
    .unwrap();
    agent.abort();

    let WorkerOutcome::Succeeded { pr_url } = outcome else {
        panic!("expected success, got {outcome:?}");
    };
    assert_eq!(pr_url, format!("https://fake.example/pr/{PR}"));

    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Succeeded);
    assert_eq!(record.step, impl_reviewer::STEP_SETTLE);
    assert_eq!(record.loop_kind, impl_reviewer::KIND);

    // The findings became an inline review thread (with the review body
    // posted alongside) — the fixer's native input.
    let threads = env.forge.threads_of(PR);
    assert_eq!(threads.len(), 1, "threads: {threads:?}");
    assert!(!threads[0].resolved);
    assert_eq!(threads[0].path.as_deref(), Some("src/cache.rs"));
    assert_eq!(threads[0].line, Some(2));
    assert!(threads[0].comments[0].body.contains("wire up the store"));
    let reviews = env.forge.pr_reviews_of(PR);
    assert_eq!(reviews.len(), 1);
    assert!(reviews[0].contains("never populates the cache"));

    // The marked summary comment is the idempotency/rounds record.
    let comments = env.forge.pr_comments_of(PR);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains(&impl_review_marker(&env.head_sha)));

    // No label transitions: claim released, nothing else touched.
    let labels = env.forge.pr_labels_of(PR);
    assert!(!labels.contains(&LABEL_WORKING.to_string()), "{labels:?}");
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));

    // The ping-pong connection: the fixer discovers this PR from the
    // AI-created thread, exactly as it would from a human's. Its target is
    // keyed by the PR's canonical issue (recovered from the `meguri/7-…`
    // head branch, issue #7), not the PR number.
    let fixer_targets = FixerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        fixer_targets
            .iter()
            .map(|t| t.key.number())
            .collect::<Vec<_>>(),
        vec![7]
    );

    // The agent reviewed the PR head: detached checkout at the head sha,
    // with the diff dropped where the prompt says.
    let wt = find_worktree(&env.worktree_root).unwrap();
    assert_eq!(
        run_git(&wt, &["rev-parse", "HEAD"]).await.unwrap(),
        env.head_sha
    );
    assert!(wt.join("src/cache.rs").exists());
    assert!(wt.join(DIFF_FILE).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn clean_posts_marker_comment_only_and_nothing_reacts() {
    let env = setup().await;
    let run = create_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_review(
            wt,
            serde_json::json!({"verdict": "clean", "review": "Solid, tests cover it."}),
        );
        write_result(wt, turn_id, "success");
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        run_impl_reviewer(&env.deps, &run.id),
    )
    .await
    .expect("impl reviewer timed out")
    .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    // Clean creates no threads and posts no review — only the marked
    // comment. The fixer stays quiet: the loop rests here.
    assert!(env.forge.threads_of(PR).is_empty());
    assert!(env.forge.pr_reviews_of(PR).is_empty());
    let comments = env.forge.pr_comments_of(PR);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains(&impl_review_marker(&env.head_sha)));
    assert!(comments[0].contains("clean"));
    assert!(comments[0].contains("Solid, tests cover it."));
    assert!(FixerLoop.discover(&env.deps).await.unwrap().is_empty());

    // ...and the reviewed head is not discovered again.
    assert!(
        ImplReviewerLoop
            .discover(&env.deps)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_rejection_falls_back_to_marked_comment() {
    let env = setup().await;
    let run = create_run(&env);
    env.forge.fail_create_pr_review(PR);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_review(
            wt,
            serde_json::json!({
                "verdict": "findings",
                "review": "- off-by-one in the lookup.",
                "findings": [
                    {"path": "src/cache.rs", "line": 2, "body": "Boundary is wrong."}
                ],
            }),
        );
        write_result(wt, turn_id, "success");
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        run_impl_reviewer(&env.deps, &run.id),
    )
    .await
    .expect("impl reviewer timed out")
    .unwrap();
    agent.abort();

    // The review is not lost: everything folds into the marked comment.
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    assert!(env.forge.threads_of(PR).is_empty());
    let comments = env.forge.pr_comments_of(PR);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains(&impl_review_marker(&env.head_sha)));
    assert!(comments[0].contains("off-by-one"));
    assert!(comments[0].contains("`src/cache.rs:2`"));
    assert!(comments[0].contains("Boundary is wrong."));

    // The degradation is observable: the fixer won't see this review.
    let events = env.deps.store.events_for_run(&run.id, 100).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "impl_review.fallback"),
        "events: {:?}",
        events.iter().map(|e| e.kind.clone()).collect::<Vec<_>>()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn double_review_skipped_even_after_claim() {
    let env = setup().await;
    let run = create_run(&env);

    // Another host reviewed this head between discovery and claim.
    env.forge
        .comment_pr(
            PR,
            &format!("{}\nolder impl review", impl_review_marker(&env.head_sha)),
        )
        .await
        .unwrap();

    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        run_impl_reviewer(&env.deps, &run.id),
    )
    .await
    .expect("impl reviewer timed out")
    .unwrap();

    assert!(
        matches!(outcome, WorkerOutcome::Skipped(_)),
        "expected skip, got {outcome:?}"
    );
    // Quiet skip: no claim, no second comment.
    assert_eq!(env.forge.pr_comments_of(PR).len(), 1);
    assert!(
        !env.forge
            .pr_labels_of(PR)
            .contains(&LABEL_WORKING.to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_filters_labels_ci_threads_and_branches() {
    let env = setup().await;

    // Alongside the eligible PR: every exclusion the spec names.
    env.forge.add_pr(
        30,
        "spec phase",
        "",
        &[LABEL_SPEC_REVIEWING],
        "meguri/a",
        "s30",
    );
    env.forge.add_pr(
        31,
        "implementing",
        "",
        &[LABEL_SPEC_READY],
        "meguri/b",
        "s31",
    );
    env.forge
        .add_pr(32, "claimed", "", &[LABEL_WORKING], "meguri/c", "s32");
    env.forge
        .add_pr(33, "held", "", &[LABEL_HOLD], "meguri/d", "s33");
    env.forge
        .add_pr(34, "not ours", "", &[], "feature/human", "s34");
    env.forge.add_pr(35, "red ci", "", &[], "meguri/e", "s35");
    env.forge.set_pr_check(35, "ci", CheckState::Failure);
    env.forge
        .add_pr(36, "ci running", "", &[], "meguri/f", "s36");
    env.forge.set_pr_check(36, "ci", CheckState::Pending);
    env.forge
        .add_pr(37, "awaiting fixer", "", &[], "meguri/g", "s37");
    env.forge
        .add_review_thread(37, "t37", "src/x.rs", "human", "please fix");
    env.forge
        .add_pr(38, "already reviewed", "", &[], "meguri/h", "s38");
    env.forge
        .comment_pr(38, &format!("{}\nreview", impl_review_marker("s38")))
        .await
        .unwrap();

    let targets = ImplReviewerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![PR]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn re_review_follows_the_head_until_rounds_run_out() {
    let mut env = setup().await;

    // Round 1 happened: marker for the old head.
    env.forge
        .comment_pr(PR, &format!("{}\nround 1", impl_review_marker("oldhead")))
        .await
        .unwrap();

    // The head moved past it (the fixer pushed): discoverable again...
    let targets = ImplReviewerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![PR]
    );

    // ...but not once the rounds cap is spent.
    env.deps.config.review.impl_max_rounds = 1;
    assert!(
        ImplReviewerLoop
            .discover(&env.deps)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_switch_silences_discovery() {
    let mut env = setup().await;
    env.deps.config.review.impl_enabled = false;
    assert!(
        ImplReviewerLoop
            .discover(&env.deps)
            .await
            .unwrap()
            .is_empty()
    );
}
