//! End-to-end pr-reviewer-loop tests with FakeMux + FakeForge and a real
//! local git origin (ADR 0008). The pr-reviewer is the optional external
//! review, symmetric across plan and impl: its output is a `meguri/pr-review`
//! commit status + a folded PR-body `<details>` — never inline threads. A
//! clean plan review flips `spec-reviewing → spec-ready`; the impl review
//! never touches spec labels. A scripted "agent" plays the pane side (same
//! protocol as planner_test).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{AgentProfile, Config, LaunchMode, ProjectConfig};
use meguri::engine::pr_reviewer::{
    self, DIFF_FILE, PR_REVIEW_STATUS, PrReviewerLoop, REVIEW_FILE, run_pr_reviewer,
};
use meguri::engine::spec_fixer::SpecFixerLoop;
use meguri::engine::{Deps, Loop, WorkerOutcome};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    CommitStatusState, Forge, LABEL_HOLD, LABEL_IMPLEMENTING, LABEL_NEEDS_HUMAN, LABEL_SPEC_READY,
    LABEL_SPEC_REVIEWING, LABEL_WORKING,
};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{LANE_PR_REVIEW, RunStatus, Store};

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

/// Create the PR branch on origin and return its head sha.
async fn push_pr_branch(clone: &Path) -> String {
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
    #[allow(dead_code)]
    mux: Arc<FakeMux>,
    head_sha: String,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    worktree_root: PathBuf,
}

/// `labels` seed the PR (a plan PR carries `spec-reviewing`; an impl PR does
/// not). `impl_review` enables the impl-kind review.
async fn setup(labels: &[&str], impl_review: bool) -> TestEnv {
    setup_with(labels, impl_review, |_| {}).await
}

/// [`setup`] plus a config tweak applied before `Deps` is built (e.g. the
/// direct launch-mode tests below).
async fn setup_with(
    labels: &[&str],
    impl_review: bool,
    tweak: impl FnOnce(&mut Config),
) -> TestEnv {
    let root = tempfile::tempdir().unwrap();
    let (_origin, clone) = init_origin_and_clone(root.path()).await;
    let head_sha = push_pr_branch(&clone).await;
    let worktree_root = root.path().join("worktrees");

    let forge = Arc::new(FakeForge::default());
    forge.add_pr(
        PR,
        "Spec: Add caching layer (#5)",
        "Refs #5.\n\nSpec for review.",
        labels,
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
    config.review.guard.impl_enabled = impl_review;
    tweak(&mut config);
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
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
        head_sha,
        root,
        worktree_root,
    }
}

fn create_pr_reviewer_run(env: &TestEnv) -> meguri::store::RunRecord {
    env.deps
        .store
        .create_run_for_loop(
            "proj",
            pr_reviewer::KIND,
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

/// pr-reviewer(Plan) clean: spec-reviewing → spec-ready, a success pr-review
/// status, a folded body `<details>`, and NO inline threads or comments —
/// the fixer never reacts (criterion 3, 3a).
#[tokio::test(flavor = "multi_thread")]
async fn plan_review_clean_flips_to_spec_ready_via_status_and_body() {
    let env = setup(&[LABEL_SPEC_REVIEWING], false).await;
    let run = create_pr_reviewer_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_review(wt, "clean", "LGTM — spec covers the issue.");
        write_result(wt, turn_id, "success");
    });

    let outcome =
        tokio::time::timeout(Duration::from_secs(60), run_pr_reviewer(&env.deps, &run.id))
            .await
            .expect("pr-review timed out")
            .unwrap();
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Succeeded);
    assert_eq!(record.step, pr_reviewer::STEP_SETTLE);
    assert_eq!(record.loop_kind, pr_reviewer::KIND);

    // Plan review drives the label state machine: spec-reviewing → spec-ready.
    let labels = env.forge.pr_labels_of(PR);
    assert!(labels.contains(&LABEL_SPEC_READY.to_string()), "{labels:?}");
    assert!(!labels.contains(&LABEL_SPEC_REVIEWING.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));

    // A success pr-review commit status on the reviewed head.
    assert_eq!(
        env.forge.commit_status_of(&env.head_sha, PR_REVIEW_STATUS),
        Some(CommitStatusState::Success)
    );

    // The verdict folds into the PR body — not a conversation comment, and
    // never an inline thread (the fixer stays inert, criterion 3).
    let pr = env
        .forge
        .prs()
        .into_iter()
        .find(|p| p.number == PR)
        .unwrap();
    assert!(pr.body.contains("<details>"), "body: {}", pr.body);
    assert!(pr.body.contains("pr review (plan) — clean"));
    assert!(
        env.forge.pr_comments_of(PR).is_empty(),
        "pr-reviewer posts no conversation comment"
    );
    assert!(
        env.forge.threads_of(PR).is_empty(),
        "pr-reviewer posts no inline review thread"
    );

    // The agent reviewed the PR head in a `pr-reviewer-<issue>` detached checkout.
    let wt = find_worktree(&env.worktree_root).unwrap();
    assert!(
        wt.ends_with(format!("pr-reviewer-{ISSUE}")),
        "{}",
        wt.display()
    );
    assert_eq!(
        run_git(&wt, &["rev-parse", "HEAD"]).await.unwrap(),
        env.head_sha
    );
    assert!(wt.join(DIFF_FILE).exists());
}

/// pr-reviewer(Plan) findings do NOT escalate (issue #192, ADR 0013): a
/// failure status, the summary in the body, spec-reviewing kept, working
/// released, and no needs-human — `spec_fixer` owns the plan-side fix loop
/// and its discover must be able to pick the PR up on the very next poll.
/// Escalating here would starve spec_fixer's discover (which skips
/// needs-human PRs) before it ever ran (the #176/#188 integration bug this
/// issue fixes).
#[tokio::test(flavor = "multi_thread")]
async fn plan_review_findings_defer_to_spec_fixer_without_escalating() {
    let env = setup(&[LABEL_SPEC_REVIEWING], false).await;
    let run = create_pr_reviewer_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_review(wt, "findings", "- The spec lacks acceptance criteria.");
        write_result(wt, turn_id, "success");
    });

    let outcome =
        tokio::time::timeout(Duration::from_secs(60), run_pr_reviewer(&env.deps, &run.id))
            .await
            .expect("pr-review timed out")
            .unwrap();
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let labels = env.forge.pr_labels_of(PR);
    assert!(
        labels.contains(&LABEL_SPEC_REVIEWING.to_string()),
        "{labels:?}"
    );
    assert!(!labels.contains(&LABEL_SPEC_READY.to_string()));
    assert!(
        !labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "plan findings must not escalate — spec_fixer owns the fix loop: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(
        env.forge.comments_of(PR).is_empty(),
        "no escalation comment for plan findings"
    );
    assert_eq!(
        env.forge.commit_status_of(&env.head_sha, PR_REVIEW_STATUS),
        Some(CommitStatusState::Failure)
    );
    let pr = env
        .forge
        .prs()
        .into_iter()
        .find(|p| p.number == PR)
        .unwrap();
    assert!(pr.body.contains("acceptance criteria"), "body: {}", pr.body);

    // pr_reviewer itself does not re-review a head it already settled...
    assert!(
        PrReviewerLoop.discover(&env.deps).await.unwrap().is_empty(),
        "an already-reviewed head is not re-reviewed"
    );
    // ...but spec_fixer's discover fires on the very next poll (issue #192,
    // acceptance criterion 1): no needs-human means it is free to pick up
    // the PR whose head pr-review status it just saw settle to Failure.
    let targets = SpecFixerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![ISSUE],
        "spec_fixer must discover the findings PR now that it is not escalated"
    );
}

/// pr-reviewer(Impl): reviews an implementation PR, writes the pr-review
/// status + body summary, escalates findings to needs-human (issue #176), and
/// NEVER touches spec-* labels (criterion 3a). No inline threads.
#[tokio::test(flavor = "multi_thread")]
async fn impl_review_reviews_without_touching_spec_labels() {
    // An impl PR: no spec-reviewing label, impl review enabled.
    let env = setup(&[LABEL_IMPLEMENTING], true).await;
    let run = create_pr_reviewer_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_review(wt, "findings", "- edge case unhandled");
        write_result(wt, turn_id, "success");
    });

    let outcome =
        tokio::time::timeout(Duration::from_secs(60), run_pr_reviewer(&env.deps, &run.id))
            .await
            .expect("pr-review timed out")
            .unwrap();
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    // The impl review writes the status + body but leaves labels as-is (only
    // `implementing`, plus the claim which it releases).
    assert_eq!(
        env.forge.commit_status_of(&env.head_sha, PR_REVIEW_STATUS),
        Some(CommitStatusState::Failure)
    );
    let labels = env.forge.pr_labels_of(PR);
    assert!(
        labels.contains(&LABEL_IMPLEMENTING.to_string()),
        "{labels:?}"
    );
    assert!(!labels.contains(&LABEL_SPEC_READY.to_string()));
    assert!(!labels.contains(&LABEL_SPEC_REVIEWING.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    // Findings escalate: the impl PR is parked on needs-human (issue #176).
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "{labels:?}"
    );
    let pr = env
        .forge
        .prs()
        .into_iter()
        .find(|p| p.number == PR)
        .unwrap();
    assert!(pr.body.contains("pr review (impl)"), "body: {}", pr.body);
    assert!(env.forge.threads_of(PR).is_empty());
    // The escalation comment is a normal PR comment (not an inline review thread).
    assert_eq!(env.forge.comments_of(PR).len(), 1);
}

/// pr-reviewer(Impl) OFF (the default): impl PRs are not discovered.
#[tokio::test(flavor = "multi_thread")]
async fn impl_review_off_discovers_nothing() {
    let env = setup(&[LABEL_IMPLEMENTING], false).await;
    assert!(
        PrReviewerLoop.discover(&env.deps).await.unwrap().is_empty(),
        "impl review is off by default"
    );
}

/// Discovery filters: held / claimed / already-reviewed heads are skipped; a
/// plan PR whose plan review is off is skipped too.
#[tokio::test(flavor = "multi_thread")]
async fn discovery_filters_hold_claimed_and_reviewed_heads() {
    let mut env = setup(&[LABEL_SPEC_REVIEWING], false).await;

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
    // Already reviewed at its head.
    env.forge
        .add_pr(15, "reviewed", "", &[LABEL_SPEC_REVIEWING], "b15", "sha15");
    env.forge
        .set_commit_status_direct("sha15", PR_REVIEW_STATUS, CommitStatusState::Failure);

    let targets = PrReviewerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![ISSUE]
    );

    // Plan review off → the spec PR is not discovered either.
    env.deps.config.review.guard.plan = false;
    assert!(PrReviewerLoop.discover(&env.deps).await.unwrap().is_empty());
}

/// A benign race (label removed after discovery) skips quietly.
#[tokio::test(flavor = "multi_thread")]
async fn skips_quietly_when_label_removed_after_discovery() {
    let env = setup(&[LABEL_SPEC_REVIEWING], false).await;
    let run = create_pr_reviewer_run(&env);
    env.forge
        .remove_pr_label(PR, LABEL_SPEC_REVIEWING)
        .await
        .unwrap();

    let outcome =
        tokio::time::timeout(Duration::from_secs(30), run_pr_reviewer(&env.deps, &run.id))
            .await
            .expect("pr-review timed out")
            .unwrap();
    assert!(matches!(outcome, WorkerOutcome::Skipped(_)), "{outcome:?}");
    assert_eq!(
        env.deps.store.get_run(&run.id).unwrap().unwrap().status,
        RunStatus::Skipped
    );
    assert!(
        !env.forge
            .pr_labels_of(PR)
            .contains(&LABEL_WORKING.to_string())
    );
}

/// needs_human on the review turn escalates on the PR.
#[tokio::test(flavor = "multi_thread")]
async fn needs_human_escalates_on_the_pr() {
    let env = setup(&[LABEL_SPEC_REVIEWING], false).await;
    let run = create_pr_reviewer_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_human");
    });

    let result = tokio::time::timeout(Duration::from_secs(60), run_pr_reviewer(&env.deps, &run.id))
        .await
        .expect("pr-review timed out");
    agent.abort();
    assert!(result.is_err());
    let labels = env.forge.pr_labels_of(PR);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "{labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(labels.contains(&LABEL_SPEC_REVIEWING.to_string()));
    let comments = env.forge.comments_of(PR);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("needs a human"));
}

/// Issue #92: the pr-reviewer's pane and worktree are keyed by the issue's
/// pr-review lane and survive rounds — a second review of a new head reuses
/// both.
#[tokio::test(flavor = "multi_thread")]
async fn second_round_reuses_pane_and_worktree() {
    let env = setup(&[LABEL_SPEC_REVIEWING], false).await;
    let clone = env.deps.project.repo_path.clone();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_review(wt, "findings", "- still missing acceptance criteria");
        write_result(wt, turn_id, "success");
    });

    let run1 = create_pr_reviewer_run(&env);
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        run_pr_reviewer(&env.deps, &run1.id),
    )
    .await
    .expect("pr-review timed out")
    .unwrap();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let pane1 = env
        .deps
        .store
        .get_pane("proj", ISSUE, LANE_PR_REVIEW)
        .unwrap()
        .unwrap();
    let pane1_id = pane1.mux_pane_id.expect("review pane registered");
    let wt = PathBuf::from(pane1.worktree_path.expect("worktree recorded"));
    assert!(
        wt.ends_with(format!("pr-reviewer-{ISSUE}")),
        "{}",
        wt.display()
    );

    // The author pushes a fix: the PR head moves.
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
    // Round 1's findings escalated to needs-human (issue #176); a human clears
    // the label so the pushed fix can be re-guarded on the review lane.
    env.forge
        .remove_pr_label(PR, LABEL_NEEDS_HUMAN)
        .await
        .unwrap();

    let run2 = create_pr_reviewer_run(&env);
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        run_pr_reviewer(&env.deps, &run2.id),
    )
    .await
    .expect("pr-review timed out")
    .unwrap();
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let pane2 = env
        .deps
        .store
        .get_pane("proj", ISSUE, LANE_PR_REVIEW)
        .unwrap()
        .unwrap();
    assert_eq!(pane2.mux_pane_id.as_deref(), Some(pane1_id.as_str()));
    assert_eq!(
        pane2.worktree_path.as_deref(),
        Some(wt.to_string_lossy().as_ref())
    );
    assert_eq!(run_git(&wt, &["rev-parse", "HEAD"]).await.unwrap(), head2);
}

/// Write a fake *headless* agent CLI for direct launch mode (issue #169): a
/// shell script standing in for `claude -p`, invoked as `{command} <trigger>`
/// with the worktree as cwd. It extracts the turn id from the trigger line
/// and writes a `needs_human` result — the pr-reviewer then escalates on the PR.
fn fake_headless_agent(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("fake-direct-agent.sh");
    std::fs::write(
        &path,
        r#"#!/bin/sh
trigger="$1"
turn="${trigger#*prompt-}"
turn="${turn%%.md*}"
mkdir -p .meguri
printf '{"turn_id":"%s","status":"needs_human","summary":"scripted direct review: stuck"}' "$turn" > .meguri/result.json
"#,
    )
    .unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// A `[launch.roles] pr-reviewer = "direct"` pr-reviewer test env: the default
/// profile is the fake headless script above (nothing spawns a real
/// `claude`), and every pr-review turn runs as a plain subprocess.
async fn setup_direct(script_dir: &Path) -> TestEnv {
    let agent = fake_headless_agent(script_dir);
    setup_with(&[LABEL_SPEC_REVIEWING], false, move |cfg| {
        cfg.launch
            .roles
            .insert("pr-reviewer".into(), LaunchMode::Direct);
        cfg.agent = AgentProfile {
            command: agent.to_string_lossy().into_owned(),
            args: vec![],
            resume_args: vec![],
            headless_args: None,
            direct_args: vec![],
            herdr_agent_hint: None,
            session_dir: None,
        };
    })
    .await
}

/// Issue #169 pr-reviewer (then guard) finding 2: with `[launch.roles] pr-reviewer = "direct"`
/// there is no pane, so the escalation comment must not advertise
/// `meguri attach` — it points at the headless session instead.
#[tokio::test(flavor = "multi_thread")]
async fn direct_pr_reviewer_escalation_comment_does_not_advertise_attach() {
    let script_dir = tempfile::tempdir().unwrap();
    let env = setup_direct(script_dir.path()).await;
    let run = create_pr_reviewer_run(&env);

    let result = tokio::time::timeout(Duration::from_secs(60), run_pr_reviewer(&env.deps, &run.id))
        .await
        .expect("pr-reviewer timed out");
    assert!(result.is_err());

    let labels = env.forge.pr_labels_of(PR);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "{labels:?}"
    );
    // The central escalation helper posts via `pr_comment` (→ `comments`),
    // issue #176.
    let comments = env.forge.comments_of(PR);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("needs a human"), "{}", comments[0]);
    assert!(
        !comments[0].contains("meguri attach"),
        "a direct-mode role has no pane to attach to: {}",
        comments[0]
    );
    assert!(
        comments[0].contains("headless"),
        "the comment should explain the headless mode instead: {}",
        comments[0]
    );
}

/// Issue #169 pr-reviewer (then guard) finding 1: a lane that ran in pane mode before the role
/// was switched to `direct` still has a live pane — the next direct turn
/// must release it (kill on the mux + detach in the store) instead of
/// leaving it alive forever, keeping ADR 0012's "a direct lane has no live
/// pane" invariant.
#[tokio::test(flavor = "multi_thread")]
async fn direct_pr_reviewer_releases_the_lanes_leftover_pane_mode_pane() {
    use meguri::mux::Multiplexer;

    let script_dir = tempfile::tempdir().unwrap();
    let env = setup_direct(script_dir.path()).await;

    // Simulate the lane's pane-mode past: a live pane on the mux, mapped on
    // the panes table (as `ensure_pane` would have left it).
    let pane = env.mux.register_live_pane("%leftover");
    env.deps
        .store
        .upsert_pane(
            "proj",
            ISSUE,
            LANE_PR_REVIEW,
            "tmux",
            "meguri",
            "%leftover",
            "/nonexistent/worktree",
        )
        .unwrap();

    let run = create_pr_reviewer_run(&env);
    let _ = tokio::time::timeout(Duration::from_secs(60), run_pr_reviewer(&env.deps, &run.id))
        .await
        .expect("pr-reviewer timed out");

    // The direct turn released the stale pane through the shared reaper
    // path: killed on the mux, detached (reclaimed) in the store.
    assert!(
        !env.mux.pane_alive(&pane).await.unwrap(),
        "the leftover pane must be killed before a direct turn"
    );
    let record = env
        .deps
        .store
        .get_pane("proj", ISSUE, LANE_PR_REVIEW)
        .unwrap()
        .unwrap();
    assert_eq!(
        record.mux_pane_id, None,
        "the pane mapping must be detached"
    );
    assert!(record.reclaimed_at.is_some());
}
