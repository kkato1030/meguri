//! The separate-mode plan→impl handoff (ADR 0008 §6, criterion 7): a merged
//! spec/ADR PR flips its `speccing` issue to `ready` so the worker implements
//! it in a fresh PR. Since ADR 0012 S4 (決定5) this is the `Op(Handoff)`
//! branch of the Issue Kind decider, driven through
//! `issue_reconciler::reconcile_issues`. Combined delivery does not use it.

use std::sync::Arc;

use meguri::config::{Config, PlanDelivery, ProjectConfig};
use meguri::engine::issue_reconciler;
use meguri::engine::pr_reviewer;
use meguri::engine::{Deps, planner};
use meguri::forge::fake::FakeForge;
use meguri::forge::{Issue, LABEL_READY, LABEL_SPECCING};
use meguri::store::{InteractionState, RunStatus};

fn deps_with(forge: Arc<FakeForge>, delivery: PlanDelivery) -> Deps {
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
        plan_delivery: delivery,
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
        autonomy: None,
        cadence: Vec::new(),
        prompts: Default::default(),
        notify: None,
    };
    Deps::with_label_source(
        meguri::store::Store::open_in_memory().unwrap(),
        Arc::new(meguri::mux::fake::FakeMux::new(false)),
        forge,
        Config::default(),
        project,
    )
}

/// Seed a `speccing` issue whose planner run recorded a spec PR branch, and a
/// spec PR on that branch in the given state.
fn seed(forge: &FakeForge, deps: &Deps, issue: i64, branch: &str, pr_state: &str) -> i64 {
    forge.issues.lock().unwrap().push(Issue {
        number: issue,
        title: format!("issue {issue}"),
        body: String::new(),
        labels: vec![LABEL_SPECCING.to_string()],
    });
    // The planner run that recorded the branch (the sweep looks it up).
    let run = deps
        .store
        .create_run_for_loop("proj", planner::KIND, issue, "t")
        .unwrap();
    deps.store
        .update_run_worktree(&run.id, branch, "/wt/x")
        .unwrap();
    deps.store
        .update_run_status(&run.id, RunStatus::Succeeded, None)
        .unwrap();
    let pr = forge.push_pr(branch, &format!("Spec: issue {issue}"), &[]);
    forge.set_pr_state(pr, pr_state);
    pr
}

#[tokio::test]
async fn merged_spec_pr_flips_speccing_to_ready() {
    let forge = Arc::new(FakeForge::default());
    let deps = deps_with(forge.clone(), PlanDelivery::Separate);
    let pr = seed(&forge, &deps, 5, "meguri/5-thing-abc", "merged");

    issue_reconciler::reconcile_issues(&deps, &Default::default())
        .await
        .unwrap();

    let labels = forge.labels_of(5);
    assert!(labels.contains(&LABEL_READY.to_string()), "{labels:?}");
    assert!(!labels.contains(&LABEL_SPECCING.to_string()));
    // The handoff left a note naming the spec PR.
    assert!(
        forge
            .comments_of(5)
            .iter()
            .any(|c| c.contains(&format!("#{pr}")))
    );

    // Idempotent: a second sweep (issue no longer speccing) is a no-op.
    issue_reconciler::reconcile_issues(&deps, &Default::default())
        .await
        .unwrap();
    assert_eq!(forge.comments_of(5).len(), 1);
}

/// Park a clean plan review of `issue` the way the pr-reviewer does (ADR
/// 0009): a `Succeeded` run carrying `AwaitingHuman` plus the
/// `review.awaiting_human` event, so it shows in `list_parked_reviews()`.
fn park_review(deps: &Deps, issue: i64) -> String {
    let run = deps
        .store
        .create_run_for_loop("proj", pr_reviewer::KIND, issue, "t")
        .unwrap();
    deps.store
        .update_interaction_state(&run.id, Some(InteractionState::AwaitingHuman))
        .unwrap();
    deps.store
        .emit(
            Some(&run.id),
            "review.awaiting_human",
            serde_json::json!({}),
        )
        .unwrap();
    deps.store
        .update_run_status(&run.id, RunStatus::Succeeded, None)
        .unwrap();
    run.id
}

/// The handoff clears the parked plan review (ADR 0009): the park waited
/// exactly for this merge, and `Refs #N` keeps the issue open, so without the
/// clear the resolved awaiting row would linger on the dashboard.
#[tokio::test]
async fn handoff_clears_the_parked_plan_review() {
    let forge = Arc::new(FakeForge::default());
    let deps = deps_with(forge.clone(), PlanDelivery::Separate);
    seed(&forge, &deps, 5, "meguri/5-thing-abc", "merged");
    let parked = park_review(&deps, 5);
    assert_eq!(deps.store.list_parked_reviews().unwrap().len(), 1);

    issue_reconciler::reconcile_issues(&deps, &Default::default())
        .await
        .unwrap();

    // Handed off…
    assert!(forge.labels_of(5).contains(&LABEL_READY.to_string()));
    // …and the park is gone with it.
    assert!(deps.store.list_parked_reviews().unwrap().is_empty());
    assert_eq!(
        deps.store
            .get_run(&parked)
            .unwrap()
            .unwrap()
            .interaction_state,
        None
    );
}

/// A spec PR that has not merged yet keeps its park: the human is still the
/// one who has to act, so the awaiting row must stay on the dashboard.
#[tokio::test]
async fn no_handoff_keeps_the_park() {
    let forge = Arc::new(FakeForge::default());
    let deps = deps_with(forge.clone(), PlanDelivery::Separate);
    seed(&forge, &deps, 5, "meguri/5-thing-abc", "open");
    park_review(&deps, 5);

    issue_reconciler::reconcile_issues(&deps, &Default::default())
        .await
        .unwrap();

    assert_eq!(deps.store.list_parked_reviews().unwrap().len(), 1);
}

#[tokio::test]
async fn an_open_or_unmerged_spec_pr_does_not_hand_off() {
    let forge = Arc::new(FakeForge::default());
    let deps = deps_with(forge.clone(), PlanDelivery::Separate);
    // Still open (under review / awaiting merge): no handoff.
    seed(&forge, &deps, 5, "meguri/5-thing-abc", "open");
    issue_reconciler::reconcile_issues(&deps, &Default::default())
        .await
        .unwrap();
    assert!(forge.labels_of(5).contains(&LABEL_SPECCING.to_string()));
    assert!(!forge.labels_of(5).contains(&LABEL_READY.to_string()));

    // Closed unmerged (abandoned spec): also no handoff — a human re-triages.
    seed(&forge, &deps, 6, "meguri/6-thing-def", "closed");
    issue_reconciler::reconcile_issues(&deps, &Default::default())
        .await
        .unwrap();
    assert!(forge.labels_of(6).contains(&LABEL_SPECCING.to_string()));
    assert!(!forge.labels_of(6).contains(&LABEL_READY.to_string()));
}

#[tokio::test]
async fn combined_delivery_never_hands_off() {
    let forge = Arc::new(FakeForge::default());
    let deps = deps_with(forge.clone(), PlanDelivery::Combined);
    seed(&forge, &deps, 5, "meguri/5-thing-abc", "merged");

    issue_reconciler::reconcile_issues(&deps, &Default::default())
        .await
        .unwrap();
    // Combined delivery hands off via the spec worker, not this sweep.
    assert!(forge.labels_of(5).contains(&LABEL_SPECCING.to_string()));
    assert!(!forge.labels_of(5).contains(&LABEL_READY.to_string()));
}
