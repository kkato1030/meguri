//! Auto-merge sweep tests (auto-merge 1/3, #41): FakeForge seeded with PRs,
//! `auto_merger::sweep` driven directly, and the recorded arm/merge/marker
//! state asserted. Same shape as `reaper_test.rs` / `fixer_test.rs`.

use std::sync::Arc;

use meguri::config::{AutoMergeMode, AutoMergeOptIn, Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::auto_merger::{ARMED_MARKER_PREFIX, armed_marker, sweep};
use meguri::engine::pr_reviewer::PR_REVIEW_STATUS;
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    CommitStatusState, Forge, LABEL_AUTOMERGE, LABEL_HOLD, LABEL_NEEDS_HUMAN, LABEL_SPEC_REVIEWING,
    MergePolicy, MergeStrategy, MergeableState,
};

/// A Deps over the given forge with auto-merge enabled (label opt-in, squash).
fn deps_with(forge: Arc<FakeForge>) -> Deps {
    let mut config = Config::default();
    config.pr.auto_merge.enabled = true;
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: "/tmp/unused".into(),
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: None,
        pr: None,
        clean: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
        prompts: Default::default(),
    };
    Deps::with_label_source(
        meguri::store::Store::open_in_memory().unwrap(),
        Arc::new(meguri::mux::fake::FakeMux::new(false)),
        forge,
        config,
        project,
    )
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

/// Deps with the impl review enabled (so the auto-merger applies the
/// pr-review gate).
fn deps_with_pr_review(forge: Arc<FakeForge>) -> Deps {
    let mut deps = deps_with(forge);
    deps.config.review.guard.impl_enabled = true;
    deps
}

#[tokio::test]
async fn pr_review_gate_arms_on_a_success_status() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha-head");
    forge.set_commit_status_direct("sha-head", PR_REVIEW_STATUS, CommitStatusState::Success);
    let deps = deps_with_pr_review(forge.clone());

    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(pr),
        Some((MergeStrategy::Squash, "sha-head".into())),
        "a green pr-review status lets the arm proceed"
    );
}

#[tokio::test]
async fn pr_review_gate_escalates_a_failure_status_and_does_not_arm() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha-head");
    forge.set_commit_status_direct("sha-head", PR_REVIEW_STATUS, CommitStatusState::Failure);
    let deps = deps_with_pr_review(forge.clone());

    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(pr),
        None,
        "a red pr-review status blocks arming"
    );
    let labels = forge.pr_labels_of(pr);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "{labels:?}"
    );
    assert!(
        forge
            .pr_comments_of(pr)
            .iter()
            .any(|c| c.contains("PR review"))
    );
}

#[tokio::test]
async fn pr_review_gate_waits_when_status_absent() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha-head"); // no pr-review status posted yet
    let deps = deps_with_pr_review(forge.clone());

    sweep(&deps).await.unwrap();
    assert_eq!(forge.armed_of(pr), None, "no status yet: wait, don't arm");
    // Waiting is silent — no escalation while the pr-reviewer has not run.
    assert!(
        !forge
            .pr_labels_of(pr)
            .contains(&LABEL_NEEDS_HUMAN.to_string()),
        "an absent status must not escalate"
    );
}

#[tokio::test]
async fn pr_review_gate_is_skipped_when_impl_review_disabled() {
    // The default (impl review off): the auto-merger arms without any status,
    // never demanding one that nothing produces (no ADR-0007 deadlock).
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha-head");
    let deps = deps_with(forge.clone()); // impl review stays off

    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(pr),
        Some((MergeStrategy::Squash, "sha-head".into()))
    );
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
async fn require_branch_protection_false_arms_without_protection() {
    // The escape hatch: with `require_branch_protection = false`, the base
    // having no required-checks protection must not block arming (and the
    // GhForge probe — which 403s on a non-admin token — is skipped entirely).
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha");
    forge.set_merge_policy(MergePolicy {
        auto_merge_allowed: true,
        allowed_strategies: vec![MergeStrategy::Squash],
        protected_with_required_checks: false,
    });
    let mut deps = deps_with(forge.clone());
    deps.config.pr.auto_merge.require_branch_protection = false;

    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(pr),
        Some((MergeStrategy::Squash, "sha".into())),
        "escape hatch arms without branch protection"
    );
}

#[tokio::test]
async fn merge_policy_skips_protection_probe_when_not_required() {
    // FakeForge mirrors GhForge: when protection isn't required the probe is
    // skipped and its result reads false, even if a policy set it true.
    let forge = FakeForge::default();
    forge.set_merge_policy(MergePolicy {
        auto_merge_allowed: true,
        allowed_strategies: vec![MergeStrategy::Squash],
        protected_with_required_checks: true,
    });
    let probed = forge.merge_policy("main", true).await.unwrap();
    assert!(
        probed.protected_with_required_checks,
        "required: probe runs"
    );
    let skipped = forge.merge_policy("main", false).await.unwrap();
    assert!(
        !skipped.protected_with_required_checks,
        "not required: probe skipped, reports false"
    );
}

#[tokio::test]
async fn require_branch_protection_false_escapes_a_non_admin_403() {
    // The reviewer's exact scenario (issue #41): a non-admin / write-only
    // token 403s on the branch-protection probe. With protection required the
    // probe runs and the 403 surfaces as an error (admin token needed); with
    // `require_branch_protection = false` the probe is skipped, so no 403 —
    // `merge_policy` (hence `meguri watch` / `doctor` preflight) succeeds and
    // the documented escape hatch actually works for non-admin tokens.
    let forge = FakeForge::default();
    forge.forbid_protection_probe();

    let err = forge.merge_policy("main", true).await.unwrap_err();
    assert!(err.to_string().contains("HTTP 403"), "{err}");
    assert!(
        err.to_string()
            .contains("require_branch_protection = false"),
        "{err}"
    );

    let escaped = forge
        .merge_policy("main", false)
        .await
        .expect("escape hatch: probe skipped, no 403");
    assert!(!escaped.protected_with_required_checks);
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

// --- orchestrator mode (auto-merge orchestrator-side merge, #157) -----------

/// A Deps in orchestrator mode: meguri merges eligible PRs itself instead of
/// arming. `require_branch_protection` is false (mandatory in this mode).
fn deps_orchestrator(forge: Arc<FakeForge>) -> Deps {
    let mut deps = deps_with(forge);
    deps.config.pr.auto_merge.mode = AutoMergeMode::Orchestrator;
    deps.config.pr.auto_merge.require_branch_protection = false;
    deps
}

#[tokio::test]
async fn orchestrator_merges_a_mergeable_pr() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha-head");
    forge.set_pr_mergeable(pr, MergeableState::Mergeable);
    let deps = deps_orchestrator(forge.clone());

    sweep(&deps).await.unwrap();

    assert_eq!(
        forge.merged_head(pr),
        Some("sha-head".into()),
        "eligible + MERGEABLE PR merged at its confirmed head"
    );
    assert_eq!(forge.armed_of(pr), None, "orchestrator never arms");
    // The audit comment carries no arm marker (merge-watch invariant).
    let comments = forge.pr_comments_of(pr);
    assert!(
        comments.iter().all(|c| !c.contains(ARMED_MARKER_PREFIX)),
        "no arm marker in orchestrator comments: {comments:?}"
    );
    assert!(
        comments.iter().any(|c| c.contains("orchestrator")),
        "audit comment posted: {comments:?}"
    );
}

#[tokio::test]
async fn orchestrator_skips_conflicting_pr() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha");
    forge.set_pr_mergeable(pr, MergeableState::Conflicting);
    let deps = deps_orchestrator(forge.clone());

    sweep(&deps).await.unwrap();

    assert_eq!(
        forge.merged_head(pr),
        None,
        "CONFLICTING is the conflict-resolver's, not merged here"
    );
}

#[tokio::test]
async fn orchestrator_skips_unknown_mergeability() {
    let forge = Arc::new(FakeForge::default());
    // Default mergeability is Unknown (GitHub still computing).
    let pr = seed_armable(&forge, 10, "sha");
    let deps = deps_orchestrator(forge.clone());

    sweep(&deps).await.unwrap();

    assert_eq!(
        forge.merged_head(pr),
        None,
        "UNKNOWN is carried over to the next sweep, not merged"
    );
}

#[tokio::test]
async fn orchestrator_does_not_merge_blocked_or_threaded_prs() {
    let forge = Arc::new(FakeForge::default());

    // On hold, but otherwise mergeable.
    forge.add_pr(
        1,
        "Held",
        "Closes #1.\n",
        &[LABEL_AUTOMERGE, LABEL_HOLD],
        "meguri/1-x",
        "s1",
    );
    forge.set_pr_mergeable(1, MergeableState::Mergeable);
    // Unresolved review thread (self-review not accepted), otherwise mergeable.
    forge.add_pr(
        2,
        "Threaded",
        "Closes #2.\n",
        &[LABEL_AUTOMERGE],
        "meguri/2-x",
        "s2",
    );
    forge.set_pr_mergeable(2, MergeableState::Mergeable);
    forge.add_review_thread(2, "t1", "src/lib.rs", "reviewer", "please fix");

    let deps = deps_orchestrator(forge.clone());
    sweep(&deps).await.unwrap();

    assert_eq!(forge.merged_head(1), None, "blocked label: not merged");
    assert_eq!(forge.merged_head(2), None, "unresolved thread: not merged");
}

#[tokio::test]
async fn orchestrator_readies_draft_before_merging() {
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha");
    forge.set_pr_draft(pr, true);
    forge.set_pr_mergeable(pr, MergeableState::Mergeable);
    let deps = deps_orchestrator(forge.clone());

    sweep(&deps).await.unwrap();

    assert!(!forge.is_draft(pr), "draft readied before merge");
    assert_eq!(forge.merged_head(pr), Some("sha".into()));
}

#[tokio::test]
async fn orchestrator_merges_even_without_native_auto_merge_allowed() {
    // The whole point (#157): a repo that cannot enable "Allow auto-merge"
    // (private + Free) still merges under orchestrator mode.
    let forge = Arc::new(FakeForge::default());
    let pr = seed_armable(&forge, 10, "sha");
    forge.set_pr_mergeable(pr, MergeableState::Mergeable);
    forge.set_merge_policy(MergePolicy {
        auto_merge_allowed: false,
        allowed_strategies: vec![MergeStrategy::Squash],
        protected_with_required_checks: false,
    });
    let deps = deps_orchestrator(forge.clone());

    sweep(&deps).await.unwrap();
    assert_eq!(forge.merged_head(pr), Some("sha".into()));
}
