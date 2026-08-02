//! Operator surface (ADR 0016 / ADR 0012 S4 決定9): the verbs share one typed
//! identity selector; a manual `run` bypasses the discovery throttles but
//! never the safety gates.

use meguri::app::{RunSelector, selector};
use meguri::engine::issue_reconciler::{IssueSnapshot, IssueStep, Mode, next_step_issue};

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
        deps_unmet: false,
    };
    // Throttles bypassed: an already-shipped issue still dispatches.
    let shipped = IssueSnapshot {
        already_shipped: true,
        ..base
    };
    assert!(matches!(
        next_step_issue(&shipped, Mode::ManualRun),
        IssueStep::Agent(_)
    ));
    assert!(
        !matches!(
            next_step_issue(&shipped, Mode::Reconcile),
            IssueStep::Agent(_)
        ),
        "the watch path keeps the throttle"
    );
    // Safety gates kept even under ManualRun.
    for tweak in [
        IssueSnapshot {
            human_stop: true,
            ..base
        },
        IssueSnapshot {
            deps_unmet: true,
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
