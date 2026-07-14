//! End-to-end conflict-resolver tests with FakeMux + FakeForge and a real
//! local git origin: an open meguri PR whose branch conflicts with main gets
//! the base merged, the conflicts resolved, and the merge commit pushed onto
//! its existing branch. A scripted "agent" plays the pane side (same
//! protocol as worker_test / fixer_test).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, PlanDelivery, ProjectConfig};
use meguri::engine::conflict_resolver::{
    self, ConflictResolverLoop, MAX_RESOLVE_RUNS, run_conflict_resolver,
};
use meguri::engine::{Deps, Loop, WorkerOutcome};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    LABEL_HOLD, LABEL_NEEDS_HUMAN, LABEL_SPEC_READY, LABEL_WORKING, MergeableState,
};
use meguri::gitops::{is_ancestor, run_git};
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

/// Seed a genuine conflict: the PR branch and main both rewrite the same
/// line of `feature.txt` after a shared base commit.
async fn seed_conflicting_pr_branch(clone: &Path) {
    std::fs::write(clone.join("feature.txt"), "base line\n").unwrap();
    run_git(clone, &["add", "."]).await.unwrap();
    run_git(clone, &["commit", "-m", "add feature file"])
        .await
        .unwrap();
    run_git(clone, &["push", "origin", "main"]).await.unwrap();

    run_git(clone, &["checkout", "-b", PR_BRANCH])
        .await
        .unwrap();
    std::fs::write(clone.join("feature.txt"), "pr line\n").unwrap();
    run_git(clone, &["commit", "-am", "pr change"])
        .await
        .unwrap();
    run_git(clone, &["push", "origin", PR_BRANCH])
        .await
        .unwrap();

    run_git(clone, &["checkout", "main"]).await.unwrap();
    std::fs::write(clone.join("feature.txt"), "main line\n").unwrap();
    run_git(clone, &["commit", "-am", "main change"])
        .await
        .unwrap();
    run_git(clone, &["push", "origin", "main"]).await.unwrap();
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
    seed_conflicting_pr_branch(&clone).await;
    let worktree_root = root.path().join("worktrees");

    // The PR a worker shipped earlier, now reported CONFLICTING by the forge.
    let forge = Arc::new(FakeForge::default());
    let pr = forge.push_pr(PR_BRANCH, "Add feature (#9)", &[]);
    assert_eq!(pr, 1);
    forge.set_pr_mergeable(1, MergeableState::Conflicting);

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600; // scripted agent: no nudging wanted
    config.limits.result_grace_secs = 1; // FakeMux always reads Working; don't linger
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        check_command: check_command.map(str::to_string),
        worktree_root: Some(worktree_root.clone()),
        language: None,
        pr: None,
        clean: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
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
        root,
        worktree_root,
    }
}

fn create_resolver_run(env: &TestEnv) -> meguri::store::RunRecord {
    env.deps
        .store
        .create_run_for_loop("proj", conflict_resolver::KIND, 9, "Add feature (#9)")
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
        "turn_id": turn_id, "status": status, "summary": "scripted resolve",
    });
    std::fs::write(worktree.join(".meguri/result.json"), result.to_string()).unwrap();
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

/// What a competent agent does with the resolve prompt: merge the base,
/// resolve the conflicted file, conclude the merge.
async fn merge_and_resolve(wt: &Path, resolution: &str) {
    // The merge stops on the conflict; concluding it below is the fix.
    let _ = run_git(wt, &["merge", "--no-edit", "origin/main"]).await;
    std::fs::write(wt.join("feature.txt"), resolution).unwrap();
    run_git(wt, &["add", "."]).await.unwrap();
    run_git(
        wt,
        &[
            "-c",
            "user.email=a@example.com",
            "-c",
            "user.name=agent",
            "commit",
            "--no-edit",
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
async fn resolver_happy_path_merges_base_and_pushes() {
    // The check command also proves validation runs on the resolved tree.
    let env = setup(Some("test -f feature.txt")).await;
    let run = create_resolver_run(&env);
    let clone = env.deps.project.repo_path.clone();
    let tip_before = origin_tip(&clone).await;
    let base_tip = run_git(&clone, &["rev-parse", "origin/main"])
        .await
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            merge_and_resolve(&wt, "merged line\n").await;
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        run_conflict_resolver(&env.deps, &run.id),
    )
    .await
    .expect("resolver timed out")
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
    assert_eq!(record.loop_kind, conflict_resolver::KIND);
    assert_eq!(record.branch.as_deref(), Some(PR_BRANCH));

    // The merge commit landed on the PR's branch on origin, with the base
    // tip merged in and the conflict actually resolved.
    let tip_after = origin_tip(&clone).await;
    assert_ne!(tip_before, tip_after, "origin tip must advance");
    run_git(&clone, &["fetch", "origin", PR_BRANCH])
        .await
        .unwrap();
    assert!(
        is_ancestor(&clone, &base_tip, &format!("origin/{PR_BRANCH}")).unwrap(),
        "the pinned base tip must be merged into the pushed head"
    );
    let pushed = run_git(
        &clone,
        &["show", &format!("origin/{PR_BRANCH}:feature.txt")],
    )
    .await
    .unwrap();
    assert_eq!(pushed, "merged line");

    // Claim released, no escalation, and a durable trace on the PR.
    let labels = env.forge.pr_labels(1);
    assert!(!labels.contains(&LABEL_WORKING.to_string()), "{labels:?}");
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
    let comments = env.forge.comments_of(1);
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].contains("resolved"), "{}", comments[0]);
    assert!(comments[0].contains(&base_tip), "{}", comments[0]);

    // The prompt pinned the base commit and forbade push/rebase.
    let wt = find_worktree(&env.worktree_root).unwrap();
    let prompt = std::fs::read_dir(wt.join(".meguri"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("prompt-"))
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .find(|p| p.contains("resolving merge conflicts"))
        .expect("execute prompt exists");
    assert!(prompt.contains(&format!("git merge {base_tip}")));
    assert!(prompt.contains("Do NOT push"));
    assert!(prompt.contains("Do NOT rebase"));
}

#[tokio::test(flavor = "multi_thread")]
async fn resolver_discovery_wants_conflicting_unclaimed_meguri_prs_only() {
    let env = setup(None).await;

    // PR #2: on hold — a human parked it.
    let pr = env
        .forge
        .push_pr("meguri/12-held-def456", "Held (#12)", &[LABEL_HOLD]);
    env.forge.set_pr_mergeable(pr, MergeableState::Conflicting);
    // PR #3: a human's PR — not meguri's to touch.
    let pr = env.forge.push_pr("feature/manual", "Manual work", &[]);
    env.forge.set_pr_mergeable(pr, MergeableState::Conflicting);
    // PR #4: already claimed by another host.
    let pr = env
        .forge
        .push_pr("meguri/13-busy-aaa111", "Busy (#13)", &[LABEL_WORKING]);
    env.forge.set_pr_mergeable(pr, MergeableState::Conflicting);
    // PR #5: escalated — waits for a human to clear the label.
    let pr = env.forge.push_pr(
        "meguri/14-stuck-bbb222",
        "Stuck (#14)",
        &[LABEL_NEEDS_HUMAN],
    );
    env.forge.set_pr_mergeable(pr, MergeableState::Conflicting);
    // PR #6: mergeable — nothing to do.
    let pr = env
        .forge
        .push_pr("meguri/15-fine-ccc333", "Fine (#15)", &[]);
    env.forge.set_pr_mergeable(pr, MergeableState::Mergeable);
    // PR #7: mergeability still being computed — retry next poll.
    let pr = env.forge.push_pr("meguri/16-new-ddd444", "New (#16)", &[]);
    env.forge.set_pr_mergeable(pr, MergeableState::Unknown);
    // PR #8: merged — history is immutable.
    let pr = env.forge.push_pr("meguri/17-old-eee555", "Old (#17)", &[]);
    env.forge.set_pr_mergeable(pr, MergeableState::Conflicting);
    env.forge.set_pr_state(pr, "merged");

    let targets = ConflictResolverLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![9],
        "only the open, unclaimed, unescalated meguri PR that conflicts is actionable"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resolver_discovery_skips_spec_ready_prs_under_combined_delivery() {
    // Under combined plan delivery (ADR 0008 §6) a `spec-ready` PR's branch
    // belongs to the spec worker's takeover; the resolver must keep off it
    // even though it conflicts. Until issue #170 this was the one gate the
    // resolver's guard was missing relative to fixer/ci-fixer.
    let mut env = setup(None).await;
    env.deps.project.plan_delivery = PlanDelivery::Combined;

    let pr = env.forge.push_pr(
        "meguri/12-spec-def456",
        "Spec: thing (#12)",
        &[LABEL_SPEC_READY],
    );
    env.forge.set_pr_mergeable(pr, MergeableState::Conflicting);

    // PR #1 (from `setup`) still conflicts and is ordinary — only the
    // spec-ready PR must be skipped.
    let targets = ConflictResolverLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![9],
        "a spec-ready PR belongs to the spec worker under combined delivery"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resolver_stops_rediscovering_after_the_resolve_budget() {
    let env = setup(None).await;
    assert_eq!(
        ConflictResolverLoop
            .discover(&env.deps)
            .await
            .unwrap()
            .len(),
        1
    );

    // The resolve→re-conflict ping-pong: every round ends in a succeeded
    // run, so cap the successes rather than looping forever.
    for _ in 0..MAX_RESOLVE_RUNS {
        let run = create_resolver_run(&env);
        env.deps
            .store
            .update_run_status(&run.id, RunStatus::Succeeded, None)
            .unwrap();
    }
    // A new tick: the scheduler would clear the shared open-PR cache here
    // (issue #170) before calling discover again.
    env.deps.open_prs.clear().await;
    assert!(
        ConflictResolverLoop
            .discover(&env.deps)
            .await
            .unwrap()
            .is_empty(),
        "a PR that keeps re-conflicting must stop being rediscovered"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resolver_skips_quietly_when_pr_stops_conflicting_after_discovery() {
    let env = setup(None).await;
    let run = create_resolver_run(&env);

    // The benign race: someone resolved (or the base retreated) between
    // discovery and claim.
    env.forge.set_pr_mergeable(1, MergeableState::Mergeable);

    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        run_conflict_resolver(&env.deps, &run.id),
    )
    .await
    .expect("resolver timed out")
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
async fn resolver_needs_human_escalates_on_the_pr_and_stays_quiet() {
    let env = setup(None).await;
    let run = create_resolver_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_human");
    });

    let result = tokio::time::timeout(
        Duration::from_secs(60),
        run_conflict_resolver(&env.deps, &run.id),
    )
    .await
    .expect("resolver timed out");
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
    assert!(comments[0].contains("could not resolve"), "{}", comments[0]);

    // The PR still conflicts, but the escalation parks it: no failure loop.
    assert!(
        ConflictResolverLoop
            .discover(&env.deps)
            .await
            .unwrap()
            .is_empty(),
        "an escalated PR must wait for a human, not re-trigger"
    );

    // Nothing was pushed.
    let tip = origin_tip(&env.deps.project.repo_path).await;
    let pr_commit = run_git(&env.deps.project.repo_path, &["rev-parse", PR_BRANCH])
        .await
        .unwrap();
    assert_eq!(tip, pr_commit);
}
