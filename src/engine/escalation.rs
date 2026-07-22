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
//!
//! A fourth destination, [`escalate_infra`], deliberately does NOT hand the
//! task to a human. A run that fails because forge/mux itself is unreachable
//! (a stopped mux, a dropped connection) says nothing about the issue — it is
//! retryable once the dependency recovers, and must not occupy the
//! needs-human queue (design doc §3-E / P6, issue #250). [`infra_reason`]
//! classifies a run failure as this kind of fault; [`super::flow::run_flow`]
//! routes to `escalate_infra` instead of `Flavor::escalate` when it applies.

use serde_json::json;

use super::Deps;
use super::flow;
use crate::forge;
use crate::notify::Notification;
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
    deps.notifier
        .notify(&Notification::escalation_task(key.number(), target, reason))
        .await;
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
    deps.notifier
        .notify(&Notification::escalation_task(issue, "issue", reason))
        .await;
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
    deps.notifier.notify(&Notification::escalation_pr(pr)).await;
}

/// Classify a run failure as a forge/mux command fault rather than something
/// a human must judge (issue #250). Looks for a [`crate::mux::MuxError`] (a
/// stopped mux, a dead pane, a command it refused) or a
/// [`crate::forge::gh::GhSpawnFailed`] (the `gh` binary itself could not be
/// started) anywhere in the error chain — both say "a command to a
/// dependency failed", never "the agent needs a decision".
///
/// Deliberately NOT a bare `std::io::Error` check (issue #250 f1, self-review
/// finding): `io::Error` also comes from git (`src/gitops.rs`), the
/// direct-mode agent's own `cmd.spawn()`, and plain prompt/log file writes —
/// none of those are forge/mux command faults, and misclassifying them would
/// silently drop genuine failures from the needs-human queue and retry them
/// forever instead. Everything else (including forge's own business-logic
/// failures, which carry no distinct type) keeps escalating to needs-human
/// as before — only the two typed, unambiguous boundaries above are
/// reclassified here.
pub fn infra_reason(err: &anyhow::Error) -> Option<&'static str> {
    for cause in err.chain() {
        if let Some(mux_err) = cause.downcast_ref::<crate::mux::MuxError>() {
            return Some(mux_infra_reason(mux_err));
        }
        if cause
            .downcast_ref::<crate::forge::gh::GhSpawnFailed>()
            .is_some()
        {
            return Some("gh_spawn_failed");
        }
    }
    None
}

fn mux_infra_reason(err: &crate::mux::MuxError) -> &'static str {
    use crate::mux::MuxError;
    match err {
        MuxError::Io(io_err) if io_err.kind() == std::io::ErrorKind::ConnectionRefused => {
            "mux_connection_refused"
        }
        MuxError::Io(_) => "mux_io_error",
        MuxError::CommandFailed { .. } => "mux_command_failed",
        MuxError::WaitTimeout(_) => "mux_wait_timeout",
        MuxError::PaneNotFound(_) => "mux_pane_not_found",
        MuxError::Other(_) => "mux_other",
    }
}

/// Retry cap for [`escalate_infra`]: once one target has tripped this many
/// infra faults inside [`INFRA_FAULT_WINDOW_SECS`], the fault stops cycling
/// as retryable and escalates to needs-human. Each retry costs a run record,
/// a worktree and an agent spawn, and an open issue's worktree is never
/// reaped — so a *permanently* broken mux/gh (a bad config, an uninstalled
/// binary) must not be allowed to grow disk and API spend on every poll
/// forever.
const MAX_INFRA_FAULTS_PER_WINDOW: usize = 5;

/// The sliding window for [`MAX_INFRA_FAULTS_PER_WINDOW`]. Wide enough that a
/// flaky-but-recovering dependency (an overnight outage) stays classified as
/// infra, narrow enough that a permanent fault reaches a human within a day.
const INFRA_FAULT_WINDOW_SECS: u64 = 24 * 3600;

/// The `not_before` sentinel a capped *local* task is parked behind —
/// effectively "until a human intervenes" (local discovery skips a future
/// `not_before`; there is no hold label on the local surface).
const INFRA_PARKED_UNTIL: &str = "9999-01-01T00:00:00Z";

/// Record a forge/mux command fault without touching needs-human (issue
/// #250). The caller has already released the run's claim (`Flavor::escalate`
/// would have dropped it too, via `escalate_task`/`escalate_issue`) so the
/// next sweep can simply retry once the dependency is back. Emits
/// `infra.raised` on every occurrence (for the full history), but the
/// notification's dedup key is `reason` alone — not per issue/run — so N
/// issues tripping the same fault collapse into one page inside the
/// notifier's throttle window ("同一 reason は backoff 付きで1本化").
///
/// Bounded: past [`MAX_INFRA_FAULTS_PER_WINDOW`] faults for the same target
/// inside the window, the fault is no longer treated as transient and falls
/// back to the standard needs-human escalation ([`escalate_task`]) **plus
/// `meguri:hold`** — needs-human alone would not stop a ready/plan issue's
/// discovery (the next claim clears it and retries), so hold is what makes
/// the stop durable until a human removes it.
pub async fn escalate_infra(deps: &Deps, run: &RunRecord, reason: &str, detail: &str) {
    let key = run.task_key();
    let target = match key {
        TaskKey::Issue(_) => "issue",
        TaskKey::Local(_) => "local",
    };
    let _ = deps.store.emit(
        Some(&run.id),
        "infra.raised",
        json!({ "target": target, "id": key.number(), "reason": reason, "detail": detail }),
    );
    let since = crate::store::format_epoch(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .saturating_sub(INFRA_FAULT_WINDOW_SECS),
    );
    let recent = deps
        .store
        .infra_raised_since(target, key.number(), &since)
        .unwrap_or(0);
    if recent >= MAX_INFRA_FAULTS_PER_WINDOW {
        escalate_task(
            deps,
            run,
            &format!(
                "infra fault persisted after {recent} retries within 24h ({reason}): {detail} — \
                 the dependency looks permanently broken. `meguri:hold` was added so discovery \
                 stops re-dispatching; remove it (with `meguri:needs-human`) to resume once the \
                 dependency is fixed"
            ),
        )
        .await;
        // needs-human alone is NOT a discovery stop on either surface: github
        // discovery only skips hold/working and the next claim clears
        // needs-human; the local queue returns and re-claims needs_human rows
        // too ("a re-claim un-escalates"). Without a durable stop the capped
        // target retries on the very next poll. So: github gets `hold` (the
        // label discovery and claim both respect — re-attempted on every
        // capped fault, covering a transiently unwritable forge), local gets
        // a far-future `not_before` (the store-side gate local discovery
        // respects). A human resumes by removing the hold / clearing the gate.
        match key {
            TaskKey::Issue(n) => {
                if deps.forge.is_some() {
                    let _ = deps.forge().add_label(n, forge::LABEL_HOLD).await;
                }
            }
            TaskKey::Local(id) => {
                let _ = deps.store.park_task_until(id, INFRA_PARKED_UNTIL);
            }
        }
        return;
    }
    deps.notifier
        .notify(&Notification::infra(reason, detail))
        .await;
}

/// The standard needs-human PR comment. `lead` is the site-specific clause
/// (e.g. "could not resolve the merge conflicts on this PR and needs a human."),
/// `reason` the underlying detail (quoted below it), and `hint` the closing
/// "how to look at this" sentence — launch-mode-aware, from [`flow::attach_hint`]
/// (issue #169). Pass [`tasks::DEFAULT_ATTACH_HINT`] where there is no run.
pub fn pr_needs_human_comment(lead: &str, reason: &str, hint: &str) -> String {
    format!("🔁 **meguri** {lead}\n\n> {reason}\n\n{hint}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::ProjectConfig;
    use crate::forge::fake::FakeForge;
    use crate::mux::fake::FakeMux;
    use crate::notify::fake::{FakeGateway, recording_notifier_with_events};
    use crate::store::Store;

    fn deps_with(forge: Arc<FakeForge>, events: &[&str]) -> (Deps, Arc<FakeGateway>) {
        let project = ProjectConfig {
            id: "proj".into(),
            repo_path: Some("/tmp/unused".into()),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            check_command: None,
            worktree_root: None,
            language: None,
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
        let mut deps = Deps::with_label_source(
            Store::open_in_memory().unwrap(),
            Arc::new(FakeMux::new(false)),
            forge,
            crate::config::Config::default(),
            project,
        );
        let (notifier, gw) = recording_notifier_with_events(events);
        deps.notifier = notifier;
        (deps, gw)
    }

    #[tokio::test]
    async fn escalate_issue_pages_when_escalation_subscribed() {
        let forge = Arc::new(FakeForge::default());
        forge.add_issue(7, "t", "b", &[]);
        let (deps, gw) = deps_with(forge, &["escalation"]);

        escalate_issue(&deps, 7, "ci red").await;

        let delivered = gw.delivered();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].event, "escalation");
        assert_eq!(delivered[0].dedup_key, "issue:7");
        assert!(delivered[0].body.contains("ci red"));
    }

    #[tokio::test]
    async fn escalate_issue_is_silent_when_escalation_not_subscribed() {
        let forge = Arc::new(FakeForge::default());
        forge.add_issue(7, "t", "b", &[]);
        // Default allowlist (awaiting_human only): escalation must not page.
        let (deps, gw) = deps_with(forge, &["awaiting_human"]);

        escalate_issue(&deps, 7, "ci red").await;

        assert!(gw.delivered().is_empty());
    }

    #[test]
    fn infra_reason_classifies_mux_connection_refused() {
        let mux_err = crate::mux::MuxError::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "boom",
        ));
        let err = anyhow::Error::new(mux_err).context("ensuring mux session");
        assert_eq!(infra_reason(&err), Some("mux_connection_refused"));
    }

    #[test]
    fn infra_reason_classifies_mux_command_failed() {
        let mux_err = crate::mux::MuxError::CommandFailed {
            kind: "tmux",
            detail: "no server running".into(),
        };
        let err = anyhow::Error::new(mux_err);
        assert_eq!(infra_reason(&err), Some("mux_command_failed"));
    }

    #[test]
    fn infra_reason_classifies_gh_spawn_failure() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "gh: command not found");
        let err = anyhow::Error::new(crate::forge::gh::GhSpawnFailed::from(io_err));
        assert_eq!(infra_reason(&err), Some("gh_spawn_failed"));
    }

    #[test]
    fn infra_reason_is_none_for_needs_human_style_failures() {
        let err = anyhow::anyhow!("agent reported needs_human: could not resolve the ask");
        assert_eq!(infra_reason(&err), None);
    }

    /// Regression for issue #250 f1: a bare `io::Error` (git, direct-mode
    /// agent spawn, prompt/log file writes, ...) must NOT be classified as
    /// infra just because it happens to be an `io::Error` — only the typed
    /// `GhSpawnFailed`/`MuxError` boundaries above may. Misclassifying this
    /// would drop a genuine failure out of the needs-human queue and retry it
    /// forever instead of ever telling a human.
    #[test]
    fn infra_reason_is_none_for_a_bare_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = anyhow::Error::new(io_err).context("writing .meguri/prompt-x.md");
        assert_eq!(infra_reason(&err), None);
    }

    #[tokio::test]
    async fn escalate_infra_never_touches_needs_human_and_pages_once_per_reason() {
        let forge = Arc::new(FakeForge::default());
        forge.add_issue(7, "t", "b", &[forge::LABEL_WORKING]);
        let (deps, gw) = deps_with(forge.clone(), &["infra"]);
        let run = deps.store.create_run("proj", 7, "t").unwrap();

        escalate_infra(&deps, &run, "mux_connection_refused", "connection refused").await;

        let labels = forge.labels_of(7);
        assert!(!labels.contains(&forge::LABEL_NEEDS_HUMAN.to_string()));
        assert!(forge.comments_of(7).is_empty());

        let events = deps.store.events_for_run(&run.id, 10).unwrap();
        assert!(events.iter().any(|e| e.kind == "infra.raised"));

        let delivered = gw.delivered();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].event, "infra");
        assert_eq!(delivered[0].dedup_key, "infra:mux_connection_refused");
    }

    #[tokio::test]
    async fn escalate_infra_caps_retries_then_hands_the_target_to_a_human() {
        let forge = Arc::new(FakeForge::default());
        forge.add_issue(7, "t", "b", &[forge::LABEL_WORKING]);
        let (deps, _gw) = deps_with(forge.clone(), &["infra"]);
        let run = deps.store.create_run("proj", 7, "t").unwrap();

        // Up to the cap: infra only — no needs-human.
        for _ in 0..MAX_INFRA_FAULTS_PER_WINDOW - 1 {
            escalate_infra(&deps, &run, "mux_connection_refused", "refused").await;
        }
        assert!(
            !forge
                .labels_of(7)
                .contains(&forge::LABEL_NEEDS_HUMAN.to_string()),
            "below the cap the fault must stay classified as infra"
        );

        // The capping fault: no longer transient — a human takes over, and
        // hold makes the stop durable (needs-human alone is cleared by the
        // next claim, so discovery would retry without it).
        escalate_infra(&deps, &run, "mux_connection_refused", "refused").await;
        let labels = forge.labels_of(7);
        assert!(
            labels.contains(&forge::LABEL_NEEDS_HUMAN.to_string()),
            "past the cap the target must escalate to needs-human"
        );
        assert!(
            labels.contains(&forge::LABEL_HOLD.to_string()),
            "past the cap the issue must be held so discovery stops re-dispatching"
        );
    }

    #[tokio::test]
    async fn escalate_infra_cap_parks_a_local_task_behind_not_before() {
        let forge = Arc::new(FakeForge::default());
        let (deps, _gw) = deps_with(forge, &["infra"]);
        let task_id = deps
            .store
            .create_task("proj", "work", "t", "b", "local")
            .unwrap()
            .id;
        let run = deps
            .store
            .create_run_for_task("proj", "worker", task_id, "t")
            .unwrap();

        for _ in 0..MAX_INFRA_FAULTS_PER_WINDOW {
            escalate_infra(&deps, &run, "mux_connection_refused", "refused").await;
        }

        // The local queue re-claims needs_human rows, so the durable stop is
        // the far-future not_before gate local discovery respects.
        let parked = deps.store.get_task(task_id).unwrap().unwrap();
        assert_eq!(
            parked.not_before.as_deref(),
            Some(INFRA_PARKED_UNTIL),
            "a capped local task must be parked behind not_before"
        );
    }

    #[tokio::test]
    async fn escalate_infra_is_silent_when_infra_not_subscribed() {
        let forge = Arc::new(FakeForge::default());
        forge.add_issue(7, "t", "b", &[]);
        // Default allowlist (awaiting_human only): infra must not page.
        let (deps, gw) = deps_with(forge, &["awaiting_human"]);
        let run = deps.store.create_run("proj", 7, "t").unwrap();

        escalate_infra(&deps, &run, "mux_connection_refused", "boom").await;

        assert!(gw.delivered().is_empty());
    }
}
