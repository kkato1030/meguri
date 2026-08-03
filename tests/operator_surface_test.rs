//! Operator surface (ADR 0016 / ADR 0012 S4 決定9): the verbs share one typed
//! identity selector; a manual `run` bypasses the discovery throttles but
//! never the safety gates.

use meguri::app::{RunSelector, selector};

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
