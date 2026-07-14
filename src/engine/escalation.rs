//! Central escalation: the one place meguri raises `meguri:needs-human`
//! (issue #176, ADR 0012). Every branch where a human must take over routes
//! through here, so the invariant "human-needed ⇒ needs-human" is guaranteed
//! in a single spot instead of being re-implemented (and occasionally forgotten)
//! per loop.
//!
//! Three destinations, one per coordination surface:
//! - [`escalate_task`] — issue/local task via the coordination layer
//!   (`task_source`): github ⇒ needs-human label + comment, local ⇒
//!   `status=needs_human` + reason. Used by the worker/planner default path.
//! - [`escalate_issue`] — a github issue directly through the forge, for the
//!   forge-native loops that hold an issue number but no task handle
//!   (spec worker, and the guard/conflict fallbacks before a PR is claimed).
//! - [`escalate_pr`] — a pull request: park it on needs-human, drop the
//!   `working` claim, and leave a human-facing comment.
//!
//! Every helper emits an `escalation.raised` event so "which site called a
//! human, and how often" is observable — a regression in the aggregation
//! (a forgotten label) shows up as a missing event.

use serde_json::json;

use super::Deps;
use super::flow;
use crate::forge;
use crate::store::RunRecord;
use crate::tasks::{self, TaskKey};

/// Escalate an issue or local task through the coordination layer. The worker
/// and planner default `Flavor::escalate` funnels here. The closing attach hint
/// is launch-mode-aware (issue #169), derived from the run's lane.
pub async fn escalate_task(deps: &Deps, run: &RunRecord, reason: &str) {
    let key = run.task_key();
    let hint = flow::attach_hint(deps, run);
    let _ = deps.task_source.escalate(&key, reason, &hint).await;
    let target = match key {
        TaskKey::Issue(_) => "issue",
        TaskKey::Local(_) => "local",
    };
    let _ = deps.store.emit(
        None,
        "escalation.raised",
        json!({ "target": target, "id": key.number(), "reason": reason }),
    );
}

/// Escalate a github issue directly through the forge: needs-human label,
/// drop `working`, and post the standard needs-human comment. For forge-native
/// loops that only hold an issue number (replaces the old
/// `flow::escalate_on_forge`); its callers all default to `pane` launch mode, so
/// the generic attach hint applies (issue #169, ADR 0012).
pub async fn escalate_issue(deps: &Deps, issue: i64, reason: &str) {
    let forge = deps.forge();
    let _ = forge.add_label(issue, forge::LABEL_NEEDS_HUMAN).await;
    let _ = forge.remove_label(issue, forge::LABEL_WORKING).await;
    let _ = forge
        .comment(
            issue,
            &tasks::needs_human_comment(reason, tasks::DEFAULT_ATTACH_HINT),
        )
        .await;
    let _ = deps.store.emit(
        None,
        "escalation.raised",
        json!({ "target": "issue", "issue": issue, "reason": reason }),
    );
}

/// Park a pull request on `needs-human`: add the label, drop the `working`
/// claim, post `comment`, and emit the event. The ONE place a PR receives the
/// needs-human label. `comment` is the caller's fully-composed human message
/// (use [`pr_needs_human_comment`] for the standard shape).
pub async fn escalate_pr(deps: &Deps, pr: i64, comment: &str) {
    let forge = deps.forge();
    let _ = forge.add_pr_label(pr, forge::LABEL_NEEDS_HUMAN).await;
    let _ = forge.remove_pr_label(pr, forge::LABEL_WORKING).await;
    let _ = forge.pr_comment(pr, comment).await;
    let _ = deps.store.emit(
        None,
        "escalation.raised",
        json!({ "target": "pr", "pr": pr }),
    );
}

/// The standard needs-human PR comment. `lead` is the site-specific clause
/// (e.g. "could not resolve the merge conflicts on this PR and needs a human."),
/// `reason` the underlying detail (quoted below it), and `hint` the closing
/// "how to look at this" sentence — launch-mode-aware, from [`flow::attach_hint`]
/// (issue #169). Pass [`tasks::DEFAULT_ATTACH_HINT`] where there is no run.
pub fn pr_needs_human_comment(lead: &str, reason: &str, hint: &str) -> String {
    format!("🔁 **meguri** {lead}\n\n> {reason}\n\n{hint}")
}
