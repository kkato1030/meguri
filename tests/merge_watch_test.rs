//! Merge-watch sweep tests (auto-merge 2/3, #42): FakeForge seeded with armed
//! PRs in each drift state, `merge_watch::sweep` driven directly, and the
//! escalation (or deliberate lack of it) asserted. Same shape as
//! `auto_merge_test.rs`.
//!
//! The central invariant under test (ADR 0007): merge-watch escalates *only*
//! the Stuck class. Conflict / RedCI / HumanDisabled / Transient must never
//! get `meguri:needs-human`, because that label would evict the PR from the
//! conflict-resolver / ci-fixer loops and deadlock the drift they fix.

use std::sync::Arc;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::auto_merger::armed_marker;
use meguri::engine::merge_watch::sweep;
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    CheckState, Forge, LABEL_HOLD, LABEL_NEEDS_HUMAN, MergeStateStatus, MergeableState,
};

/// A `Deps` over the given forge with auto-merge enabled (the sweep is gated
/// on the same switch as the auto-merger).
fn deps_with(forge: Arc<FakeForge>) -> Deps {
    deps_with_store(forge, meguri::store::Store::open_in_memory().unwrap())
}

/// `deps_with`, but with an explicit store — the restart test hands a fresh
/// one to prove watch keeps no local state. Built through the same
/// `with_label_source` seam production uses (issue #54).
fn deps_with_store(forge: Arc<FakeForge>, store: meguri::store::Store) -> Deps {
    let mut config = Config::default();
    config.pr.auto_merge.enabled = true;
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: Some("/tmp/unused".into()),
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: None,
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
    Deps::with_label_source(
        store,
        Arc::new(meguri::mux::fake::FakeMux::new(false)),
        forge,
        config,
        project,
    )
}

/// A timestamp comfortably older than `STALE_AFTER` (24h) — an armed marker at
/// this time makes the PR stale.
const STALE_TS: &str = "2020-01-01T00:00:00Z";

/// Seed an open, armed meguri PR: meguri branch, `Closes #N.` body, and the
/// #41 arm marker comment stamped `created_at` (use `STALE_TS` for stuck).
fn seed_armed(forge: &FakeForge, number: i64, created_at: &str) {
    forge.add_pr(
        number,
        &format!("Add feature (#{number})"),
        &format!("Closes #{number}.\n\nbody"),
        &[],
        &format!("meguri/{number}-add-feature-abc"),
        "sha-head",
    );
    forge.add_pr_comment_at(number, &armed_marker("sha-head"), created_at);
    // Auto-merge reads as armed by default (via the injected marker only some
    // tests need to override).
    forge.set_auto_merge_enabled(number, true);
}

/// How many merge-watch stuck escalations landed on the PR (by comment
/// signature).
fn stuck_comments(forge: &FakeForge, pr: i64) -> usize {
    // The central escalation helper posts via `pr_comment` (→ `comments`),
    // issue #176.
    forge
        .comments_of(pr)
        .iter()
        .filter(|c| c.contains("止まっています"))
        .count()
}

/// The sweep must not escalate: no needs-human, no stuck comment.
fn assert_not_escalated(forge: &FakeForge, pr: i64) {
    assert!(
        !forge
            .pr_labels_of(pr)
            .contains(&LABEL_NEEDS_HUMAN.to_string()),
        "PR #{pr} must not be escalated to needs-human"
    );
    assert_eq!(stuck_comments(forge, pr), 0, "PR #{pr} must get no comment");
}

#[tokio::test]
async fn disabled_config_is_a_no_op() {
    let forge = Arc::new(FakeForge::default());
    seed_armed(&forge, 1, STALE_TS);
    forge.set_merge_state_status(1, MergeStateStatus::Blocked);
    let mut deps = deps_with(forge.clone());
    deps.config.pr.auto_merge.enabled = false;

    sweep(&deps).await.unwrap();
    assert_not_escalated(&forge, 1);
}

#[tokio::test]
async fn merged_pr_is_left_alone() {
    let forge = Arc::new(FakeForge::default());
    seed_armed(&forge, 1, STALE_TS);
    forge.set_pr_state(1, "merged");
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert_not_escalated(&forge, 1);
}

#[tokio::test]
async fn conflicting_pr_is_deferred_not_escalated() {
    let forge = Arc::new(FakeForge::default());
    seed_armed(&forge, 1, STALE_TS);
    forge.set_merge_state_status(1, MergeStateStatus::Dirty);
    forge.set_pr_mergeable(1, MergeableState::Conflicting);
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    // conflict-resolver owns it — escalating would deadlock that loop.
    assert_not_escalated(&forge, 1);
}

#[tokio::test]
async fn red_required_check_is_deferred_not_escalated() {
    let forge = Arc::new(FakeForge::default());
    seed_armed(&forge, 1, STALE_TS);
    forge.set_merge_state_status(1, MergeStateStatus::Blocked);
    forge.set_pr_check(1, "test", CheckState::Failure);
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    // ci-fixer owns a BLOCKED + red rollup — merge-watch stays out.
    assert_not_escalated(&forge, 1);
}

#[tokio::test]
async fn non_required_failure_unstable_is_not_escalated() {
    let forge = Arc::new(FakeForge::default());
    seed_armed(&forge, 1, STALE_TS);
    // UNSTABLE = a non-required check failing; GitHub still merges.
    forge.set_merge_state_status(1, MergeStateStatus::Unstable);
    forge.set_pr_check(1, "optional-lint", CheckState::Failure);
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert_not_escalated(&forge, 1);
}

#[tokio::test]
async fn human_disabled_pr_is_left_alone_silently() {
    let forge = Arc::new(FakeForge::default());
    seed_armed(&forge, 1, STALE_TS);
    // Arm marker present, but a human turned auto-merge off.
    forge.set_auto_merge_enabled(1, false);
    forge.set_merge_state_status(1, MergeStateStatus::Blocked);
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert_not_escalated(&forge, 1);
}

#[tokio::test]
async fn transient_failure_never_escalates_even_when_stale() {
    let forge = Arc::new(FakeForge::default());
    seed_armed(&forge, 1, STALE_TS);
    forge.fail_merge_state(1);
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert_not_escalated(&forge, 1);

    // Recovery reveals a conflict: still must not escalate (fixer's job).
    forge.merge_state_errors.lock().unwrap().remove(&1);
    forge.set_merge_state_status(1, MergeStateStatus::Dirty);
    sweep(&deps).await.unwrap();
    assert_not_escalated(&forge, 1);
}

#[tokio::test]
async fn stuck_pr_is_escalated_once() {
    let forge = Arc::new(FakeForge::default());
    seed_armed(&forge, 1, STALE_TS);
    // Blocked with a green rollup and no conflict: no loop rescues it.
    forge.set_merge_state_status(1, MergeStateStatus::Blocked);
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert!(
        forge
            .pr_labels_of(1)
            .contains(&LABEL_NEEDS_HUMAN.to_string()),
        "stuck armed PR is escalated"
    );
    assert_eq!(stuck_comments(&forge, 1), 1, "exactly one comment");

    // Second sweep: needs-human already present → skipped, no second comment.
    sweep(&deps).await.unwrap();
    assert_eq!(stuck_comments(&forge, 1), 1, "idempotent, no re-comment");
}

#[tokio::test]
async fn fresh_arm_is_not_yet_stuck() {
    let forge = Arc::new(FakeForge::default());
    // Armed just now (current timestamp): blocked but under the threshold.
    seed_armed(&forge, 1, &meguri::store::now());
    forge.set_merge_state_status(1, MergeStateStatus::Blocked);
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert_not_escalated(&forge, 1);
}

#[tokio::test]
async fn hold_label_is_left_alone() {
    let forge = Arc::new(FakeForge::default());
    seed_armed(&forge, 1, STALE_TS);
    forge.set_merge_state_status(1, MergeStateStatus::Blocked);
    forge.add_pr_label(1, LABEL_HOLD).await.unwrap();
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert_not_escalated(&forge, 1);
}

#[tokio::test]
async fn unarmed_pr_is_ignored() {
    let forge = Arc::new(FakeForge::default());
    // A stuck-looking meguri PR with no arm marker: not watch's business.
    forge.add_pr(
        1,
        "Add feature (#1)",
        "Closes #1.\n\nbody",
        &[],
        "meguri/1-add-feature-abc",
        "sha-head",
    );
    forge.set_merge_state_status(1, MergeStateStatus::Blocked);
    let deps = deps_with(forge.clone());

    sweep(&deps).await.unwrap();
    assert_not_escalated(&forge, 1);
}

#[tokio::test]
async fn watch_survives_restart_via_forge_only() {
    // No local state: a brand-new store (as after a kill/restart) still
    // escalates a stuck PR, because watch derives everything from the forge.
    let forge = Arc::new(FakeForge::default());
    seed_armed(&forge, 1, STALE_TS);
    forge.set_merge_state_status(1, MergeStateStatus::Blocked);
    let deps = deps_with_store(
        forge.clone(),
        meguri::store::Store::open_in_memory().unwrap(),
    );

    sweep(&deps).await.unwrap();
    assert!(
        forge
            .pr_labels_of(1)
            .contains(&LABEL_NEEDS_HUMAN.to_string()),
        "watch recovers from the forge alone after a restart"
    );
}
