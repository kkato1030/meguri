//! merge-tail BEHIND regression (ADR 0012 slice 1, issue #221): the acceptance
//! core. An armed PR whose base moves (`mergeStateStatus = BEHIND`) is closed by
//! `Op(UpdateBranch)` + an emergent re-arm — no loop is left owning it and no
//! human is paged. Covers native and orchestrator modes, plus the observe
//! bulk-query API-cost measurement.

use std::sync::Arc;

use meguri::config::{AutoMergeMode, Autonomy, Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::issue_reconciler::sweep;
use meguri::forge::fake::FakeForge;
use meguri::forge::{Forge, LABEL_AUTOMERGE, MergeStateStatus, MergeStrategy, MergeableState};

fn project() -> ProjectConfig {
    ProjectConfig {
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
    }
}

/// Deps with auto-merge enabled, full autonomy (arming is on), and the given
/// merge mode.
fn deps_with(forge: Arc<FakeForge>, mode: AutoMergeMode) -> Deps {
    let mut config = Config::default();
    config.pr.auto_merge.enabled = true;
    config.pr.auto_merge.mode = mode;
    config.autonomy = Autonomy::Full;
    Deps::with_label_source(
        meguri::store::Store::open_in_memory().unwrap(),
        Arc::new(meguri::mux::fake::FakeMux::new(false)),
        forge,
        config,
        project(),
    )
}

/// A fully-armable PR: meguri branch, `Closes #N.` link, PR-level opt-in label.
fn seed_armable(forge: &FakeForge, number: i64, head_sha: &str) {
    forge.add_pr(
        number,
        &format!("Add feature (#{number})"),
        &format!("Closes #{number}.\n\nbody"),
        &[LABEL_AUTOMERGE],
        &format!("meguri/{number}-add-feature-abc"),
        head_sha,
    );
}

#[tokio::test]
async fn native_behind_is_closed_by_update_branch_then_re_arm() {
    let forge = Arc::new(FakeForge::default());
    seed_armable(&forge, 1, "sha1");
    let deps = deps_with(forge.clone(), AutoMergeMode::Native);

    // Tick 0 (setup): the PR arms at its head.
    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(1),
        Some((MergeStrategy::Squash, "sha1".into())),
        "armed at the original head"
    );

    // The base moves ahead: GitHub now reports BEHIND. No conflict, no red CI —
    // the class no loop rescued (the BEHIND hole).
    forge.set_merge_state_status(1, MergeStateStatus::Behind);

    // Tick 1: the merge tail owns it now — it re-bases the branch.
    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.update_branch_calls_of(1),
        1,
        "the branch was updated exactly once"
    );
    let new_head = forge.get_pr(1).await.unwrap().head_sha;
    assert_eq!(new_head, "sha1-u", "the head advanced past the base merge");
    // The old head's arm marker is still on the PR (armed-since is unchanged) —
    // it must NOT keep the moved head looking armed.
    assert!(
        forge
            .pr_comments_of(1)
            .iter()
            .any(|c| c.contains("head=sha1 ")),
        "the original arm marker is retained"
    );

    // GitHub recomputes the freshly-updated branch as mergeable.
    forge.set_merge_state_status(1, MergeStateStatus::Clean);

    // Tick 2: the moved head reads as unarmed → re-arm emerges (no explicit
    // second step), closing BEHIND. Retaining the old marker did not block it.
    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(1),
        Some((MergeStrategy::Squash, "sha1-u".into())),
        "re-armed at the advanced head"
    );
    assert!(
        !forge
            .pr_labels_of(1)
            .contains(&meguri::forge::LABEL_NEEDS_HUMAN.to_string()),
        "BEHIND closed without paging a human"
    );
}

#[tokio::test]
async fn orchestrator_behind_is_closed_by_update_branch_then_merge() {
    let forge = Arc::new(FakeForge::default());
    seed_armable(&forge, 1, "sha1");
    // Behind and (once updated) mergeable: orchestrator never arms, it merges.
    forge.set_merge_state_status(1, MergeStateStatus::Behind);
    forge.set_pr_mergeable(1, MergeableState::Mergeable);
    let deps = deps_with(forge.clone(), AutoMergeMode::Orchestrator);

    // Tick 1: a behind PR is re-based before merging (the orchestrator stall).
    sweep(&deps).await.unwrap();
    assert_eq!(forge.update_branch_calls_of(1), 1);
    assert_eq!(forge.get_pr(1).await.unwrap().head_sha, "sha1-u");
    assert!(forge.merged_head(1).is_none(), "not merged while behind");

    // GitHub recomputes the updated branch as clean & mergeable.
    forge.set_merge_state_status(1, MergeStateStatus::Clean);

    // Tick 2: mergeable now → merged at the advanced head (TOCTOU-pinned).
    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.merged_head(1),
        Some("sha1-u".into()),
        "merged at the advanced head"
    );
}

#[tokio::test]
async fn human_disarm_survives_more_comments_than_the_observe_window() {
    // f1 regression: the arm marker is the durable idempotency + human-override
    // key. Buried under far more later comments than any recent-comment window,
    // the current head must still read as armed — so a human's disarm holds and
    // the head is never wrongly re-armed. (GhForge paginates on overflow to keep
    // this true; FakeForge returns every comment, so it guards the invariant.)
    let forge = Arc::new(FakeForge::default());
    seed_armable(&forge, 1, "sha1");
    let deps = deps_with(forge.clone(), AutoMergeMode::Native);

    // Arm it: exactly one arm marker + comment posted.
    sweep(&deps).await.unwrap();
    let arm_comments = |f: &FakeForge| {
        f.pr_comments_of(1)
            .iter()
            .filter(|c| c.contains("arm しました"))
            .count()
    };
    assert_eq!(arm_comments(&forge), 1, "armed once");

    // A human disables auto-merge on this same head, then the PR accrues far
    // more chatter than any recent-comment window would hold.
    forge.set_auto_merge_enabled(1, false);
    for i in 0..40 {
        forge.comment_pr(1, &format!("chatter {i}")).await.unwrap();
    }

    // The marker must not be lost: still armed → HumanDisabled, never re-armed.
    sweep(&deps).await.unwrap();
    assert_eq!(
        arm_comments(&forge),
        1,
        "same head not re-armed after human disarm despite >window comments"
    );
    assert_eq!(
        forge.armed_of(1),
        Some((MergeStrategy::Squash, "sha1".into())),
        "arm state unchanged"
    );
}

#[tokio::test]
async fn clipped_label_window_is_treated_as_a_human_stop() {
    // pr-review finding: a bounded label window could hide a `hold` /
    // `needs-human` label, letting a write slip the human stop. When the observe
    // reports the label set as incomplete, the merge tail must not act.
    let forge = Arc::new(FakeForge::default());
    seed_armable(&forge, 1, "sha1");
    forge.mark_labels_incomplete(1);
    let deps = deps_with(forge.clone(), AutoMergeMode::Native);

    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(1),
        None,
        "an incomplete label observation must not arm (a hidden stop label is assumed)"
    );
}

#[tokio::test]
async fn truncated_comment_pagination_is_treated_as_a_human_stop() {
    // pr-review finding (#231 guard): the comment overflow pagination is
    // budgeted, so a pathologically chatty PR cannot spend unbounded API cost
    // per resync. A truncated conversation could hide an arm/claim marker, so
    // the engine must park the PR (no writes) instead of acting on it.
    let forge = Arc::new(FakeForge::default());
    seed_armable(&forge, 1, "sha1");
    forge.mark_comments_incomplete(1);
    let deps = deps_with(forge.clone(), AutoMergeMode::Native);

    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(1),
        None,
        "a truncated comment observation must not arm (a hidden marker is assumed)"
    );
}

#[tokio::test]
async fn clipped_thread_window_is_treated_as_unresolved() {
    // pr-review finding: a bounded review-thread window could hide an unresolved
    // thread, letting an arm slip the review gate. An incomplete thread set must
    // read as "has an unresolved thread" → the arm waits.
    let forge = Arc::new(FakeForge::default());
    seed_armable(&forge, 1, "sha1");
    forge.mark_threads_incomplete(1);
    let deps = deps_with(forge.clone(), AutoMergeMode::Native);

    sweep(&deps).await.unwrap();
    assert_eq!(
        forge.armed_of(1),
        None,
        "an incomplete thread observation must not arm (a hidden unresolved thread is assumed)"
    );
}

#[tokio::test]
async fn observe_cost_is_constant_and_recorded() {
    // Acceptance 2: the informer-cache observe costs one request regardless of
    // PR count, and each sweep records the measured cost as an event.
    let forge = Arc::new(FakeForge::default());
    for n in 1..=3 {
        seed_armable(&forge, n, "sha");
    }
    let one = forge
        .observe_open_prs(meguri::engine::pr_reviewer::PR_REVIEW_STATUS)
        .await
        .unwrap();
    assert_eq!(one.prs.len(), 3);
    assert_eq!(one.cost.requests, 1, "one bulk read for three PRs");

    let deps = deps_with(forge.clone(), AutoMergeMode::Native);
    sweep(&deps).await.unwrap();
    assert_eq!(
        deps.store.count_events("reconciler.observe_cost").unwrap(),
        1,
        "each sweep records the observe cost"
    );
}
