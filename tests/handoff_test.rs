//! The separate-mode plan→impl handoff sweep (ADR 0008 §6, criterion 7): a
//! merged spec/ADR PR flips its `speccing` issue to `ready` so the worker
//! implements it in a fresh PR. Combined delivery does not use this path.

use std::sync::Arc;

use meguri::config::{Config, PlanDelivery, ProjectConfig};
use meguri::engine::handoff;
use meguri::engine::{Deps, planner};
use meguri::forge::fake::FakeForge;
use meguri::forge::{Forge, Issue, LABEL_READY, LABEL_SPECCING};

fn deps_with(forge: Arc<FakeForge>, delivery: PlanDelivery) -> Deps {
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
        plan_delivery: delivery,
        review: None,
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
    let pr = forge.push_pr(branch, &format!("Spec: issue {issue}"), &[]);
    forge.set_pr_state(pr, pr_state);
    pr
}

#[tokio::test]
async fn merged_spec_pr_flips_speccing_to_ready() {
    let forge = Arc::new(FakeForge::default());
    let deps = deps_with(forge.clone(), PlanDelivery::Separate);
    let pr = seed(&forge, &deps, 5, "meguri/5-thing-abc", "merged");

    handoff::sweep(&deps).await.unwrap();

    let labels = forge.labels_of(5);
    assert!(labels.contains(&LABEL_READY.to_string()), "{labels:?}");
    assert!(!labels.contains(&LABEL_SPECCING.to_string()));
    // The handoff left a note naming the spec PR.
    assert!(forge.comments_of(5).iter().any(|c| c.contains(&format!("#{pr}"))));

    // Idempotent: a second sweep (issue no longer speccing) is a no-op.
    handoff::sweep(&deps).await.unwrap();
    assert_eq!(forge.comments_of(5).len(), 1);
}

#[tokio::test]
async fn an_open_or_unmerged_spec_pr_does_not_hand_off() {
    let forge = Arc::new(FakeForge::default());
    let deps = deps_with(forge.clone(), PlanDelivery::Separate);
    // Still open (under review / awaiting merge): no handoff.
    seed(&forge, &deps, 5, "meguri/5-thing-abc", "open");
    handoff::sweep(&deps).await.unwrap();
    assert!(forge.labels_of(5).contains(&LABEL_SPECCING.to_string()));
    assert!(!forge.labels_of(5).contains(&LABEL_READY.to_string()));

    // Closed unmerged (abandoned spec): also no handoff — a human re-triages.
    seed(&forge, &deps, 6, "meguri/6-thing-def", "closed");
    handoff::sweep(&deps).await.unwrap();
    assert!(forge.labels_of(6).contains(&LABEL_SPECCING.to_string()));
    assert!(!forge.labels_of(6).contains(&LABEL_READY.to_string()));
}

#[tokio::test]
async fn combined_delivery_never_hands_off() {
    let forge = Arc::new(FakeForge::default());
    let deps = deps_with(forge.clone(), PlanDelivery::Combined);
    seed(&forge, &deps, 5, "meguri/5-thing-abc", "merged");

    handoff::sweep(&deps).await.unwrap();
    // Combined delivery hands off via the spec worker, not this sweep.
    assert!(forge.labels_of(5).contains(&LABEL_SPECCING.to_string()));
    assert!(!forge.labels_of(5).contains(&LABEL_READY.to_string()));
}
