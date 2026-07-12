//! End-to-end reviewer-loop tests with FakeMux + FakeForge and a real local
//! git origin: a `meguri:spec-reviewing` PR gets a review — findings become a
//! marked PR comment, a clean review flips the label to `meguri:spec-ready`,
//! and the head-sha marker prevents double reviews of the same head. A
//! scripted "agent" plays the pane side (same protocol as planner_test).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::reviewer::{
    self, DIFF_FILE, REVIEW_FILE, ReviewerLoop, review_marker, run_reviewer,
};
use meguri::engine::{Deps, Loop, WorkerOutcome};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    Forge, LABEL_HOLD, LABEL_NEEDS_HUMAN, LABEL_SPEC_READY, LABEL_SPEC_REVIEWING, LABEL_WORKING,
};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{ROLE_REVIEW, RunStatus, Store};

const PR: i64 = 12;
/// The canonical issue the PR's head branch encodes — runs are keyed by it.
const ISSUE: i64 = 5;
const PR_BRANCH: &str = "meguri/5-add-caching-layer-abc123";

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

/// Create the spec PR branch on origin and return its head sha.
async fn push_spec_branch(clone: &Path) -> String {
    run_git(clone, &["checkout", "-b", PR_BRANCH])
        .await
        .unwrap();
    let spec = clone.join("docs/specs/issue-5.md");
    std::fs::create_dir_all(spec.parent().unwrap()).unwrap();
    std::fs::write(&spec, "# Spec: Add caching layer\n\n- criteria\n").unwrap();
    run_git(clone, &["add", "."]).await.unwrap();
    run_git(clone, &["commit", "-m", "Add spec for issue 5"])
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
    let head_sha = push_spec_branch(&clone).await;
    let worktree_root = root.path().join("worktrees");

    let forge = Arc::new(FakeForge::default());
    forge.add_pr(
        PR,
        "Spec: Add caching layer (#5)",
        "Closes #5.\n\nSpec for review.",
        &[LABEL_SPEC_REVIEWING],
        PR_BRANCH,
        &head_sha,
    );
    forge.set_pr_diff(
        PR,
        "diff --git a/docs/specs/issue-5.md b/docs/specs/issue-5.md\n+# Spec\n",
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
        check_command: None,
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
        head_sha,
        root,
        worktree_root,
    }
}

fn create_reviewer_run(env: &TestEnv) -> meguri::store::RunRecord {
    env.deps
        .store
        .create_run_for_loop(
            "proj",
            reviewer::KIND,
            ISSUE,
            "Spec: Add caching layer (#5)",
        )
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

fn write_review(worktree: &Path, verdict: &str, review: &str) {
    let body = serde_json::json!({ "verdict": verdict, "review": review });
    std::fs::write(worktree.join(REVIEW_FILE), body.to_string()).unwrap();
}

fn write_result(worktree: &Path, turn_id: &str, status: &str) {
    let result = serde_json::json!({
        "turn_id": turn_id, "status": status, "summary": "scripted review",
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

#[tokio::test(flavor = "multi_thread")]
async fn reviewer_clean_flips_spec_reviewing_to_spec_ready() {
    let env = setup().await;
    let run = create_reviewer_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_review(wt, "clean", "LGTM — spec covers the issue.");
        write_result(wt, turn_id, "success");
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_reviewer(&env.deps, &run.id))
        .await
        .expect("reviewer timed out")
        .unwrap();
    agent.abort();

    let WorkerOutcome::Succeeded { pr_url } = outcome else {
        panic!("expected success, got {outcome:?}");
    };
    assert_eq!(pr_url, format!("https://fake.example/pr/{PR}"));

    // Run record is terminal and complete under the reviewer loop kind.
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Succeeded);
    assert_eq!(record.step, reviewer::STEP_SETTLE);
    assert_eq!(record.loop_kind, reviewer::KIND);

    // Label transition: spec-reviewing → spec-ready, claim released.
    let labels = env.forge.pr_labels_of(PR);
    assert!(
        labels.contains(&LABEL_SPEC_READY.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_SPEC_REVIEWING.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));

    // The review comment carries the idempotency marker and the verdict.
    let comments = env.forge.pr_comments_of(PR);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains(&review_marker(&env.head_sha)));
    assert!(comments[0].contains("clean"));
    assert!(comments[0].contains("LGTM"));

    // The agent reviewed the PR head: detached checkout at the head sha,
    // with the diff dropped where the prompt says.
    let wt = find_worktree(&env.worktree_root).unwrap();
    assert_eq!(
        run_git(&wt, &["rev-parse", "HEAD"]).await.unwrap(),
        env.head_sha
    );
    assert!(wt.join("docs/specs/issue-5.md").exists());
    assert!(wt.join(DIFF_FILE).exists());
    let prompts = prompts_in(&wt);
    let execute_prompt = prompts
        .iter()
        .find(|p| p.contains("# PR:"))
        .expect("review prompt exists");
    assert!(execute_prompt.contains(DIFF_FILE));
    assert!(execute_prompt.contains(REVIEW_FILE));
    assert!(execute_prompt.contains(&env.head_sha));
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewer_findings_comment_then_re_review_after_push() {
    let env = setup().await;
    let run = create_reviewer_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_review(wt, "findings", "- The spec lacks acceptance criteria.");
        write_result(wt, turn_id, "success");
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_reviewer(&env.deps, &run.id))
        .await
        .expect("reviewer timed out")
        .unwrap();
    agent.abort();

    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "a findings review is still a successful run: {outcome:?}"
    );

    // Findings: comment posted, PR stays in review (waiting for a push).
    let comments = env.forge.pr_comments_of(PR);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains(&review_marker(&env.head_sha)));
    assert!(comments[0].contains("- The spec lacks acceptance criteria."));
    assert!(comments[0].contains("re-review"));

    let labels = env.forge.pr_labels_of(PR);
    assert!(
        labels.contains(&LABEL_SPEC_REVIEWING.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_SPEC_READY.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));

    // Idempotency: the reviewed head is skipped by discovery...
    let targets = ReviewerLoop.discover(&env.deps).await.unwrap();
    assert!(
        targets.is_empty(),
        "same head must not be reviewed twice: {targets:?}"
    );

    // ...until a new push moves the head.
    env.forge.set_pr_head(PR, "feedfacefeedface");
    let targets = ReviewerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.issue_number).collect::<Vec<_>>(),
        vec![ISSUE],
        "targets are keyed by the canonical issue, not the PR"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewer_double_review_skipped_even_after_claim() {
    let env = setup().await;
    let run = create_reviewer_run(&env);

    // Another host reviewed this head between discovery and claim.
    env.forge
        .comment_pr(
            PR,
            &format!("{}\nolder review", review_marker(&env.head_sha)),
        )
        .await
        .unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(30), run_reviewer(&env.deps, &run.id))
        .await
        .expect("reviewer timed out")
        .unwrap();

    assert!(
        matches!(outcome, WorkerOutcome::Skipped(_)),
        "expected skip, got {outcome:?}"
    );
    // Quiet skip: no claim, no second comment, label untouched.
    assert_eq!(env.forge.pr_comments_of(PR).len(), 1);
    let labels = env.forge.pr_labels_of(PR);
    assert!(labels.contains(&LABEL_SPEC_REVIEWING.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewer_skips_quietly_when_label_removed_after_discovery() {
    let env = setup().await;
    let run = create_reviewer_run(&env);

    // The benign race: a human took the PR out of review.
    env.forge
        .remove_pr_label(PR, LABEL_SPEC_REVIEWING)
        .await
        .unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(30), run_reviewer(&env.deps, &run.id))
        .await
        .expect("reviewer timed out")
        .unwrap();

    assert!(
        matches!(outcome, WorkerOutcome::Skipped(_)),
        "expected skip, got {outcome:?}"
    );
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Skipped);
    assert!(env.forge.pr_comments_of(PR).is_empty());
    assert!(
        !env.forge
            .pr_labels_of(PR)
            .contains(&LABEL_WORKING.to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewer_corrective_turn_when_review_file_missing() {
    let env = setup().await;
    let run = create_reviewer_run(&env);

    // Turn 1: claim success without writing the review (a misbehaving
    // agent). Turn 2 (the corrective turn): write the actual review.
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |turn, wt, turn_id| {
        if turn > 1 {
            write_review(wt, "clean", "ok");
        }
        write_result(wt, turn_id, "success");
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_reviewer(&env.deps, &run.id))
        .await
        .expect("reviewer timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
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
        correction.data.to_string().contains("review.json"),
        "correction must name the review file: {}",
        correction.data
    );
    assert!(
        env.forge
            .pr_labels_of(PR)
            .contains(&LABEL_SPEC_READY.to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewer_needs_human_escalates_on_the_pr() {
    let env = setup().await;
    let run = create_reviewer_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_human");
    });

    let result = tokio::time::timeout(Duration::from_secs(60), run_reviewer(&env.deps, &run.id))
        .await
        .expect("reviewer timed out");
    agent.abort();

    assert!(result.is_err(), "needs_human must fail the run");
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);

    // Escalation lands on the PR: needs-human label + comment, claim
    // released, spec-reviewing stays for a human to re-triage.
    let labels = env.forge.pr_labels_of(PR);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(labels.contains(&LABEL_SPEC_REVIEWING.to_string()));

    let comments = env.forge.pr_comments_of(PR);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("needs a human"));
}

#[tokio::test(flavor = "multi_thread")]
async fn reviewer_discovery_filters_hold_working_and_reviewed_heads() {
    let env = setup().await;

    // Alongside the actionable PR: held, claimed, and already-reviewed ones.
    env.forge.add_pr(
        13,
        "held",
        "",
        &[LABEL_SPEC_REVIEWING, LABEL_HOLD],
        "b13",
        "sha13",
    );
    env.forge.add_pr(
        14,
        "claimed",
        "",
        &[LABEL_SPEC_REVIEWING, LABEL_WORKING],
        "b14",
        "sha14",
    );
    env.forge
        .add_pr(15, "reviewed", "", &[LABEL_SPEC_REVIEWING], "b15", "sha15");
    env.forge
        .comment_pr(15, &format!("{}\nreview", review_marker("sha15")))
        .await
        .unwrap();
    env.forge.add_pr(16, "unlabeled", "", &[], "b16", "sha16");

    let targets = ReviewerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.issue_number).collect::<Vec<_>>(),
        vec![ISSUE]
    );
}

/// Acceptance (issue #92): the reviewer's pane and worktree are keyed by the
/// issue's review lane and survive rounds — the second review of a new head
/// reuses both, with the checkout re-pointed to the new sha.
#[tokio::test(flavor = "multi_thread")]
async fn reviewer_second_round_reuses_pane_and_worktree() {
    let env = setup().await;
    let clone = env.deps.project.repo_path.clone();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_review(wt, "findings", "- still missing acceptance criteria");
        write_result(wt, turn_id, "success");
    });

    // Round 1: findings at the first head.
    let run1 = create_reviewer_run(&env);
    let outcome = tokio::time::timeout(Duration::from_secs(60), run_reviewer(&env.deps, &run1.id))
        .await
        .expect("reviewer timed out")
        .unwrap();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let pane1 = env
        .deps
        .store
        .get_pane("proj", ISSUE, ROLE_REVIEW)
        .unwrap()
        .unwrap();
    let pane1_id = pane1.mux_pane_id.expect("review pane registered");
    let wt = PathBuf::from(pane1.worktree_path.expect("worktree recorded"));
    assert!(
        wt.ends_with(format!("review-{ISSUE}")),
        "worktree fixed per issue: {}",
        wt.display()
    );

    // The author pushes a fix: the PR head moves to a real new commit.
    run_git(&clone, &["checkout", PR_BRANCH]).await.unwrap();
    std::fs::write(clone.join("docs/specs/issue-5.md"), "# Spec v2\n").unwrap();
    run_git(&clone, &["commit", "-am", "address findings"])
        .await
        .unwrap();
    run_git(&clone, &["push", "origin", PR_BRANCH])
        .await
        .unwrap();
    let head2 = run_git(&clone, &["rev-parse", "HEAD"]).await.unwrap();
    run_git(&clone, &["checkout", "main"]).await.unwrap();
    env.forge.set_pr_head(PR, &head2);

    // Round 2: same pane, same worktree, new head checked out.
    let run2 = create_reviewer_run(&env);
    let outcome = tokio::time::timeout(Duration::from_secs(60), run_reviewer(&env.deps, &run2.id))
        .await
        .expect("reviewer timed out")
        .unwrap();
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let pane2 = env
        .deps
        .store
        .get_pane("proj", ISSUE, ROLE_REVIEW)
        .unwrap()
        .unwrap();
    assert_eq!(
        pane2.mux_pane_id.as_deref(),
        Some(pane1_id.as_str()),
        "round 2 reuses the review pane"
    );
    assert_eq!(
        pane2.worktree_path.as_deref(),
        Some(wt.to_string_lossy().as_ref()),
        "round 2 reuses the review worktree"
    );
    assert_eq!(
        run_git(&wt, &["rev-parse", "HEAD"]).await.unwrap(),
        head2,
        "the standing checkout was re-pointed to the new head"
    );
}
