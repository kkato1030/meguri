//! reconcile loop (issue #142): a once-shipped issue whose body is edited is
//! detected — the succeeded-run suppression becomes body-aware (half A, the
//! discover guard) and a poll sweep leaves a re-attention signal on issues the
//! label-filtered discovery can no longer see (half B). Neither ever launches
//! an agent: the execution gate stays the collaborator-applied label.

use std::sync::Arc;

use meguri::config::{Config, ProjectConfig, ReconcileConfig};
use meguri::engine::{Deps, reconcile, worker};
use meguri::forge::fake::FakeForge;
use meguri::forge::{Forge, LABEL_IMPLEMENTING, LABEL_READY};
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};
use meguri::tasks::{TaskKind, body_digest};

fn deps_with(forge: Arc<FakeForge>, store: Store, reconcile: ReconcileConfig) -> Deps {
    let config = Config {
        reconcile,
        ..Default::default()
    };
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: "/tmp/unused".into(),
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        plan_delivery: Default::default(),
        review: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: None,
        pr: None,
        clean: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
    };
    Deps::with_label_source(store, Arc::new(FakeMux::new(false)), forge, config, project)
}

/// Seed a terminal succeeded worker run that recorded `body`'s digest, exactly
/// as the flow's shared prepare step would.
fn seed_succeeded_run(store: &Store, issue: i64, body: &str) {
    let run = store
        .create_run_for_loop("proj", worker::KIND, issue, "t")
        .unwrap();
    store
        .set_run_body_digest(&run.id, &body_digest(body))
        .unwrap();
    store
        .update_run_status(&run.id, RunStatus::Succeeded, None)
        .unwrap();
}

fn body_changed_events(store: &Store) -> Vec<meguri::events::EventRecord> {
    // The event is tied to the issue's latest succeeded run, so read it back
    // through that run (mirrors `meguri logs`).
    let mut out = Vec::new();
    for run in store.list_runs(false).unwrap() {
        for ev in store.events_for_run(&run.id, 100).unwrap() {
            if ev.kind == "issue.body_changed" {
                out.push(ev);
            }
        }
    }
    out
}

// ---- half A: the discover guard is body-aware -----------------------------

#[tokio::test]
async fn discover_suppresses_shipped_issue_until_its_body_changes() {
    let forge = Arc::new(FakeForge::with_issue(
        7,
        "T",
        "original body",
        &[LABEL_READY],
    ));
    let store = Store::open_in_memory().unwrap();
    seed_succeeded_run(&store, 7, "original body");
    let deps = deps_with(forge.clone(), store.clone(), ReconcileConfig::default());

    // Shipped and unchanged: not rediscovered, and no signal.
    let tasks = deps.task_source.discover(TaskKind::Work).await.unwrap();
    assert!(tasks.is_empty(), "unchanged body must stay suppressed");
    assert!(body_changed_events(&store).is_empty());

    // A label churn (add/remove a ball label) does not change the body digest,
    // so suppression holds — the criterion-1 distinction.
    forge.add_label(7, "meguri:automerge").await.unwrap();
    forge.remove_label(7, "meguri:automerge").await.unwrap();
    assert!(
        deps.task_source
            .discover(TaskKind::Work)
            .await
            .unwrap()
            .is_empty(),
        "label churn must not lift suppression"
    );
    assert!(body_changed_events(&store).is_empty());

    // Edit the body: suppression lifts, the issue is rediscovered, and the
    // body-changed signal fires exactly once.
    forge
        .update_issue_body(7, "a materially different body")
        .await
        .unwrap();
    let tasks = deps.task_source.discover(TaskKind::Work).await.unwrap();
    assert_eq!(tasks.len(), 1, "edited body must be rediscovered");
    assert_eq!(tasks[0].key.number(), 7);
    assert_eq!(body_changed_events(&store).len(), 1);

    // Re-poll with the same edited body: no second signal (dedup).
    deps.task_source.discover(TaskKind::Work).await.unwrap();
    assert_eq!(
        body_changed_events(&store).len(),
        1,
        "same body must not re-signal on the next tick"
    );
}

#[tokio::test]
async fn whitespace_only_edits_do_not_lift_suppression() {
    let forge = Arc::new(FakeForge::with_issue(
        7,
        "T",
        "line one\nline two",
        &[LABEL_READY],
    ));
    let store = Store::open_in_memory().unwrap();
    seed_succeeded_run(&store, 7, "line one\nline two");
    let deps = deps_with(forge.clone(), store.clone(), ReconcileConfig::default());

    // Reflow / trailing whitespace / CRLF: normalized digest is unchanged.
    forge
        .update_issue_body(7, "  line one   line two  \n")
        .await
        .unwrap();
    assert!(
        deps.task_source
            .discover(TaskKind::Work)
            .await
            .unwrap()
            .is_empty(),
        "whitespace-only edits must not re-fire"
    );
    assert!(body_changed_events(&store).is_empty());
}

#[tokio::test]
async fn body_edits_kill_switch_restores_permanent_suppression() {
    let forge = Arc::new(FakeForge::with_issue(7, "T", "original", &[LABEL_READY]));
    let store = Store::open_in_memory().unwrap();
    seed_succeeded_run(&store, 7, "original");
    let off = ReconcileConfig {
        body_edits: false,
        signal_comment: true,
    };
    let deps = deps_with(forge.clone(), store.clone(), off);

    forge
        .update_issue_body(7, "totally rewritten")
        .await
        .unwrap();
    assert!(
        deps.task_source
            .discover(TaskKind::Work)
            .await
            .unwrap()
            .is_empty(),
        "with body_edits off, a shipped issue stays permanently suppressed"
    );
    assert!(body_changed_events(&store).is_empty());
}

// ---- half B: the poll sweep signals shipped (implementing) issues ----------

#[tokio::test]
async fn sweep_signals_a_changed_body_once_and_only_via_the_gate() {
    let forge = Arc::new(FakeForge::with_issue(
        7,
        "T",
        "original body",
        &[LABEL_IMPLEMENTING],
    ));
    let store = Store::open_in_memory().unwrap();
    seed_succeeded_run(&store, 7, "original body");
    let deps = deps_with(forge.clone(), store.clone(), ReconcileConfig::default());

    // Unchanged body: the sweep is silent.
    reconcile::sweep(&deps).await.unwrap();
    assert!(forge.comments_of(7).is_empty());
    assert!(body_changed_events(&store).is_empty());

    // Edit the body: one event + one comment nudging a human to re-label.
    forge
        .update_issue_body(7, "edited by a human")
        .await
        .unwrap();
    reconcile::sweep(&deps).await.unwrap();
    assert_eq!(body_changed_events(&store).len(), 1);
    let comments = forge.comments_of(7);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("meguri:ready"), "{}", comments[0]);

    // Re-sweep with the same body: deduped, still one comment and one event.
    reconcile::sweep(&deps).await.unwrap();
    assert_eq!(forge.comments_of(7).len(), 1);
    assert_eq!(body_changed_events(&store).len(), 1);
}

#[tokio::test]
async fn sweep_only_touches_shipped_issues() {
    // An implementing issue with NO succeeded run (e.g. a human-driven PR) is
    // not meguri's to signal.
    let forge = Arc::new(FakeForge::with_issue(9, "T", "body", &[LABEL_IMPLEMENTING]));
    let store = Store::open_in_memory().unwrap();
    let deps = deps_with(forge.clone(), store.clone(), ReconcileConfig::default());

    forge.update_issue_body(9, "changed").await.unwrap();
    reconcile::sweep(&deps).await.unwrap();
    assert!(forge.comments_of(9).is_empty());
    assert!(body_changed_events(&store).is_empty());
}

#[tokio::test]
async fn sweep_signal_comment_toggle_and_kill_switch() {
    // signal_comment = false: the durable event still fires, but no forge write.
    let forge = Arc::new(FakeForge::with_issue(
        7,
        "T",
        "original",
        &[LABEL_IMPLEMENTING],
    ));
    let store = Store::open_in_memory().unwrap();
    seed_succeeded_run(&store, 7, "original");
    let no_comment = ReconcileConfig {
        body_edits: true,
        signal_comment: false,
    };
    let deps = deps_with(forge.clone(), store.clone(), no_comment);
    forge.update_issue_body(7, "changed").await.unwrap();
    reconcile::sweep(&deps).await.unwrap();
    assert_eq!(body_changed_events(&store).len(), 1, "event still emitted");
    assert!(forge.comments_of(7).is_empty(), "no comment when disabled");

    // body_edits = false: the whole sweep is inert.
    let forge2 = Arc::new(FakeForge::with_issue(
        8,
        "T",
        "original",
        &[LABEL_IMPLEMENTING],
    ));
    let store2 = Store::open_in_memory().unwrap();
    seed_succeeded_run(&store2, 8, "original");
    let off = ReconcileConfig {
        body_edits: false,
        signal_comment: true,
    };
    let deps2 = deps_with(forge2.clone(), store2.clone(), off);
    forge2.update_issue_body(8, "changed").await.unwrap();
    reconcile::sweep(&deps2).await.unwrap();
    assert!(forge2.comments_of(8).is_empty());
    assert!(body_changed_events(&store2).is_empty());
}
