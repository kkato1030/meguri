//! Operator surface (ADR 0016 / ADR 0012 S4 決定9): the three verbs share one
//! typed identity selector; `why` is a read-only observation window routed to
//! the owning decider; a manual `run` bypasses the discovery throttles but
//! never the safety gates.

use std::sync::Arc;

use meguri::app::{RunSelector, selector, why_text};
use meguri::config::{Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::issue_reconciler::{IssueSnapshot, IssueStep, Mode, next_step_issue};
use meguri::forge::fake::FakeForge;
use meguri::forge::{LABEL_HOLD, LABEL_READY, MergeableState};
use meguri::mux::fake::FakeMux;
use meguri::store::Store;

fn deps_with(forge: Arc<FakeForge>) -> Deps {
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
        Store::open_in_memory().unwrap(),
        Arc::new(FakeMux::new(false)),
        forge,
        Config::default(),
        project,
    )
}

#[test]
fn selector_takes_exactly_one_identity() {
    assert!(matches!(
        selector(Some(7), None, None, None),
        Ok(RunSelector::Issue(7))
    ));
    assert!(matches!(
        selector(None, Some(9), None, None),
        Ok(RunSelector::Pr(9))
    ));
    assert!(matches!(
        selector(None, None, Some("run-1a2b3c4d".into()), None),
        Ok(RunSelector::RunId(_))
    ));
    assert!(matches!(
        selector(None, None, None, Some(42)),
        Ok(RunSelector::Task(42))
    ));
    assert!(selector(None, None, None, None).is_err(), "none = error");
    assert!(
        selector(Some(7), Some(9), None, None).is_err(),
        "two identities = error"
    );
}

/// f6 (受け入れ10): `why` writes nothing to the forge and creates no run.
#[tokio::test]
async fn why_is_read_only_and_names_the_step() {
    let forge = Arc::new(FakeForge::with_issue(
        7,
        "Add greeting",
        "hello",
        &[LABEL_READY],
    ));
    let deps = deps_with(forge.clone());

    let text = why_text(&deps, &RunSelector::Issue(7)).await.unwrap();
    assert!(text.contains("issue-side decider"), "{text}");
    assert!(text.contains("Agent(Worker)"), "{text}");

    // Read-only: no labels/comments changed, no runs created.
    assert_eq!(forge.labels_of(7), vec![LABEL_READY.to_string()]);
    assert!(forge.comments_of(7).is_empty());
    assert!(deps.store.list_runs(false).unwrap().is_empty());
}

/// finding 1 (受け入れ15): `why --pr` routes to the PR-side decider — a
/// conflicting open meguri PR reads as the ConflictResolver arm, not as its
/// issue's phase.
#[tokio::test]
async fn why_pr_routes_to_the_pr_side_decider() {
    let forge = Arc::new(FakeForge::with_issue(9, "impl", "", &[LABEL_READY]));
    forge.add_pr(
        90,
        "impl",
        "Closes #9.\n\nbody",
        &[],
        "meguri/9-impl-abc",
        "sha-9",
    );
    forge.set_pr_mergeable(90, MergeableState::Conflicting);
    let deps = deps_with(forge.clone());

    let text = why_text(&deps, &RunSelector::Pr(90)).await.unwrap();
    assert!(text.contains("PR-side decider"), "{text}");
    assert!(text.contains("Agent(ConflictResolver)"), "{text}");

    // The issue identity is owned by its open PR: `why --issue` says so and
    // shows the PR-side view instead of the issue phase.
    let text = why_text(&deps, &RunSelector::Issue(9)).await.unwrap();
    assert!(text.contains("owner: its open PR #90"), "{text}");
    assert!(text.contains("Agent(ConflictResolver)"), "{text}");
    assert!(deps.store.list_runs(false).unwrap().is_empty());
}

/// `why --run` keeps the run's stored loop kind (finding 1) — never re-routed.
#[tokio::test]
async fn why_run_shows_the_stored_loop_kind() {
    let deps = deps_with(Arc::new(FakeForge::default()));
    let run = deps
        .store
        .create_run_for_loop("proj", "conflict-resolver", 9, "t")
        .unwrap();
    let text = why_text(&deps, &RunSelector::RunId(run.id.clone()))
        .await
        .unwrap();
    assert!(text.contains("recipe conflict-resolver"), "{text}");
}

/// finding 2 (受け入れ16): ManualRun bypasses the discovery throttles
/// (already-shipped / cadence window) but keeps the safety gates
/// (hold/needs-human, not-before fail-closed, busy).
#[test]
fn manual_run_bypasses_throttles_but_keeps_safety_gates() {
    let base = IssueSnapshot {
        human_stop: false,
        has_open_meguri_pr: false,
        issue_busy: false,
        has_plan: false,
        has_ready: true,
        has_speccing: false,
        has_implementing: false,
        spec_pr_state: None,
        already_shipped: false,
        not_before_wait: false,
        deps_unmet: false,
        cadence_full: false,
    };
    // Throttles bypassed: shipped and cadence-full both still dispatch.
    for tweak in [
        IssueSnapshot {
            already_shipped: true,
            ..base
        },
        IssueSnapshot {
            cadence_full: true,
            ..base
        },
    ] {
        assert!(
            matches!(
                next_step_issue(&tweak, Mode::ManualRun),
                IssueStep::Agent(_)
            ),
            "{tweak:?}"
        );
        assert!(
            !matches!(
                next_step_issue(&tweak, Mode::Reconcile),
                IssueStep::Agent(_)
            ),
            "the watch path keeps the throttle: {tweak:?}"
        );
    }
    // Safety gates kept even under ManualRun.
    for tweak in [
        IssueSnapshot {
            human_stop: true,
            ..base
        },
        IssueSnapshot {
            not_before_wait: true,
            ..base
        },
        IssueSnapshot {
            issue_busy: true,
            ..base
        },
    ] {
        assert!(
            !matches!(
                next_step_issue(&tweak, Mode::ManualRun),
                IssueStep::Agent(_)
            ),
            "safety gate must hold under ManualRun: {tweak:?}"
        );
    }
}

/// The `hold` label is a safety gate for `why` display too: the step reads as
/// the human stop, whatever the phase labels say.
#[tokio::test]
async fn why_shows_the_human_stop_first() {
    let forge = Arc::new(FakeForge::with_issue(
        7,
        "held",
        "",
        &[LABEL_READY, LABEL_HOLD],
    ));
    let deps = deps_with(forge);
    let text = why_text(&deps, &RunSelector::Issue(7)).await.unwrap();
    assert!(text.contains("human stop"), "{text}");
}
