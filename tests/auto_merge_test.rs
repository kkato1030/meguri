//! Auto-merge sweep tests (auto-merge 1/3, #41): FakeForge seeded with PRs,
//! `auto_merger::sweep` driven directly, and the recorded arm/merge/marker
//! state asserted. Same shape as `reaper_test.rs` / `fixer_test.rs`.

use std::sync::Arc;

use meguri::config::{AutoMergeOptIn, Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::auto_merger::{armed_marker, sweep};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    Forge, LABEL_AUTOMERGE, LABEL_HOLD, LABEL_SPEC_REVIEWING, MergePolicy, MergeStrategy,
};

/// A Deps over the given forge with auto-merge enabled (label opt-in, squash).
fn deps_with(forge: Arc<FakeForge>) -> Deps {
    let mut config = Config::default();
    config.pr.auto_merge.enabled = true;
    Deps {
        store: meguri::store::Store::open_in_memory().unwrap(),
        mux: Arc::new(meguri::mux::fake::FakeMux::new(false)),
        forge,
        config,
        project: ProjectConfig {
            id: "proj".into(),
            repo_path: "/tmp/unused".into(),
            repo_slug: "me/proj".into(),
            default_branch: "main".into(),
            language: None,
            check_command: None,
            worktree_root: None,
            pr: None,
        },
    }
}

/// Seed a fully-armable PR: meguri branch, `Closes #N.` link, the opt-in
/// label on the PR, non-draft, no threads. Returns the PR number.
fn seed_armable(forge: &FakeForge, number: i64, head_sha: &str) -> i64 {
    forge.add_pr(
        number,
        "Add feature (#100)",
        &format!("Closes #{number}.\n\nbody"),
        &[LABEL_AUTOMERGE],
        "meguri/100-add-feature-abc",
        head_sha,
    );
    number
}

#[tokio::test]
async fn arms_when_all_conditions_met() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha-head");
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();

    assert_eq!(
        forge.armed_of(pr),
        Some((MergeStrategy::Squash, "sha-head".into())),
        "PR armed at its head with the configured strategy"
    );
    // The marker comment is posted for the current head.
    let comments = forge.pr_comments_of(pr);
    assert!(
        comments
            .iter()
            .any(|c| c.contains(&armed_marker("sha-head"))),
        "marker comment posted: {comments:?}"
    );
    assert!(comments.iter().any(|c| c.contains("arm しました")));
}

#[tokio::test]
async fn does_not_arm_when_disabled() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha");
    let mut deps = deps_with(forge.clone());
    deps.config.pr.auto_merge.enabled = false;

    sweep(&deps).await.unwrap();
    assert_eq!(forge.armed_of(pr), None, "disabled: nothing armed");
}

#[tokio::test]
async fn spec_hold_thread_and_non_opt_in_are_not_armed() {
    let forge = Arc::new(FakeForge::default());

    // spec-reviewing: mid-spec, never arm.
    forge.add_pr(
        1,
        "Spec",
        "Closes #1.\n",
        &[LABEL_AUTOMERGE, LABEL_SPEC_REVIEWING],
        "meguri/1-x",
        "s1",
    );
    // on hold.
    forge.add_pr(
        2,
        "Held",
        "Closes #2.\n",
        &[LABEL_AUTOMERGE, LABEL_HOLD],
        "meguri/2-x",
        "s2",
    );
    // unresolved review thread.
    forge.add_pr(
        3,
        "Threaded",
        "Closes #3.\n",
        &[LABEL_AUTOMERGE],
        "meguri/3-x",
        "s3",
    );
    forge.add_review_thread(3, "t1", "src/lib.rs", "reviewer", "please fix");
    // not opted in (label opt-in, no label on PR or issue).
    forge.add_pr(4, "No opt-in", "Closes #4.\n", &[], "meguri/4-x", "s4");
    // not a meguri branch.
    forge.add_pr(
        5,
        "Human branch",
        "Closes #5.\n",
        &[LABEL_AUTOMERGE],
        "feature/x",
        "s5",
    );
    // no Closes link.
    forge.add_pr(
        6,
        "No link",
        "just a body",
        &[LABEL_AUTOMERGE],
        "meguri/6-x",
        "s6",
    );

    let deps = deps_with(forge.clone());
    sweep(&deps).await.unwrap();

    for pr in 1..=6 {
        assert_eq!(forge.armed_of(pr), None, "PR #{pr} must not be armed");
    }
}

#[tokio::test]
async fn draft_is_readied_before_arming() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha");
    forge.set_pr_draft(pr, true);
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();

    assert!(
        !forge.is_draft(pr),
        "draft is readied first (never armed while draft)"
    );
    assert_eq!(
        forge.armed_of(pr),
        Some((MergeStrategy::Squash, "sha".into()))
    );
}

#[tokio::test]
async fn opt_in_by_issue_label_arms() {
    // The PR itself has no automerge label; its tracked issue does.
    let forge = Arc::new(FakeForge::with_issue(10, "Feature", "", &[LABEL_AUTOMERGE]));
    forge.add_pr(20, "PR", "Closes #10.\n", &[], "meguri/10-x", "sha");
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(20),
        Some((MergeStrategy::Squash, "sha".into()))
    );
}

#[tokio::test]
async fn opt_in_all_arms_without_any_label() {
    let forge = Arc::new(FakeForge::default());
    forge.add_pr(10, "PR", "Closes #10.\n", &[], "meguri/10-x", "sha");
    let mut deps = deps_with(forge.clone());
    deps.config.pr.auto_merge.opt_in = AutoMergeOptIn::All;

    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(10),
        Some((MergeStrategy::Squash, "sha".into()))
    );
}

#[tokio::test]
async fn already_armed_head_is_idempotent() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha");
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    sweep(&deps).await.unwrap();

    // The marker keeps the second sweep from posting a duplicate.
    let markers = forge
        .pr_comments_of(pr)
        .into_iter()
        .filter(|c| c.contains(&armed_marker("sha")))
        .count();
    assert_eq!(markers, 1, "exactly one marker after two sweeps");
    assert_eq!(
        forge.armed_of(pr),
        Some((MergeStrategy::Squash, "sha".into()))
    );
}

#[tokio::test]
async fn new_head_after_push_is_re_evaluated_and_re_armed() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "old-head");
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(pr),
        Some((MergeStrategy::Squash, "old-head".into()))
    );

    // A push moves the head; the old marker no longer matches.
    forge.set_pr_head(pr, "new-head");
    sweep(&deps).await.unwrap();

    assert_eq!(
        forge.armed_of(pr),
        Some((MergeStrategy::Squash, "new-head".into())),
        "re-armed at the new head"
    );
    let comments = forge.pr_comments_of(pr);
    assert!(
        comments
            .iter()
            .any(|c| c.contains(&armed_marker("old-head")))
    );
    assert!(
        comments
            .iter()
            .any(|c| c.contains(&armed_marker("new-head")))
    );
}

#[tokio::test]
async fn human_disarm_of_the_same_head_is_respected() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha");
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert!(forge.armed_of(pr).is_some());

    // A human disables auto-merge on the PR (clear the armed state) but the
    // marker for this head stays. The next sweep must NOT re-arm this head.
    forge.armed.lock().unwrap().clear();
    sweep(&deps).await.unwrap();

    assert_eq!(
        forge.armed_of(pr),
        None,
        "the marker keeps the disarmed head from being re-armed"
    );
}

#[tokio::test]
async fn insufficient_repo_policy_warns_and_skips() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha");
    // Squash not allowed by the repo.
    forge.set_merge_policy(MergePolicy {
        auto_merge_allowed: true,
        allowed_strategies: vec![MergeStrategy::Merge],
        protected_with_required_checks: true,
    });
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert_eq!(forge.armed_of(pr), None, "policy mismatch skips arming");
}

#[tokio::test]
async fn clean_status_pr_is_finalized_with_merge() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha-head");
    // GitHub already judges it mergeable → enable_auto_merge returns
    // AlreadyClean → the sweep finalizes with merge_pr.
    forge.set_clean(pr);
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();

    assert_eq!(
        forge.merged_head(pr),
        Some("sha-head".into()),
        "clean PR merged at its confirmed head"
    );
    assert_eq!(forge.armed_of(pr), None, "clean path does not arm");
    let comments = forge.pr_comments_of(pr);
    assert!(
        comments
            .iter()
            .any(|c| c.contains(&armed_marker("sha-head")))
    );
    assert!(
        comments.iter().any(|c| c.contains("確定")),
        "merge prose: {comments:?}"
    );
}

#[tokio::test]
async fn merge_pr_refuses_a_moved_head() {
    // --match-head-commit: a stale head is rejected, mirroring GitHub (#41
    // acceptance 7). Direct forge check of the TOCTOU guard.
    let forge = FakeForge::default();
    forge.add_pr(
        10,
        "PR",
        "Closes #10.\n",
        &[],
        "meguri/10-x",
        "current-head",
    );
    let err = forge
        .merge_pr(10, MergeStrategy::Squash, "stale-head")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("head moved"), "{err}");
    assert_eq!(
        forge.merged_head(10),
        None,
        "nothing merged on a stale head"
    );
}
