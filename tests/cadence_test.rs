//! Discovery throttles (issue #148): not-before and cadence, driven through the
//! real `TaskSource` seam with a `FakeForge` and an injected clock. These assert
//! the two gates skip *silently* (no label, no comment on the forge) and count
//! consumption from the local run history.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use meguri::cadence::Disposition;
use meguri::config::{CadenceRule, ReconcileConfig};
use meguri::forge::fake::FakeForge;
use meguri::forge::{Forge, LABEL_READY, LABEL_WORKING};
use meguri::store::{RunStatus, Store, parse_ts};
use meguri::tasks::{
    EpochClock, LOCAL_HOST, LabelTaskSource, LocalTaskSource, TaskKey, TaskKind, TaskSource,
};

fn ts(s: &str) -> u64 {
    parse_ts(s).unwrap_or_else(|| panic!("bad ts {s}"))
}

/// Real "now" as epoch seconds. Cadence window tests anchor the injected clock
/// here so the fake clock and the runs' real `created_at` timestamps share a
/// timeline (as they do in production) — a run consumed "now" then falls inside
/// the window computed from "now".
fn base_now() -> u64 {
    parse_ts(&meguri::store::now()).expect("now() is RFC3339")
}

/// A clock whose value the test can advance to cross a not-before / window edge.
fn movable_clock(at: u64) -> (EpochClock, Arc<AtomicU64>) {
    let cell = Arc::new(AtomicU64::new(at));
    let handle = cell.clone();
    let clock: EpochClock = Arc::new(move || cell.load(Ordering::Relaxed));
    (clock, handle)
}

fn day(label: &str, n: u32) -> CadenceRule {
    CadenceRule {
        label: label.into(),
        max_per_day: Some(n),
        per_hours: None,
        max: None,
    }
}

fn hours(label: &str, h: u32, n: u32) -> CadenceRule {
    CadenceRule {
        label: label.into(),
        max_per_day: None,
        per_hours: Some(h),
        max: Some(n),
    }
}

fn label_source(
    forge: Arc<FakeForge>,
    store: Store,
    cadence: Vec<CadenceRule>,
    clock: EpochClock,
) -> LabelTaskSource {
    LabelTaskSource::new(
        forge,
        store,
        "proj".into(),
        ReconcileConfig::default(),
        cadence,
    )
    .with_clock(clock)
}

/// A prior consumption of a cadence bucket: a run stamped with `label`, exactly
/// as the scheduler / manual run would create it.
fn consume(store: &Store, issue: i64, label: &str, status: RunStatus) {
    let run = store
        .create_run_for_loop_cadence("proj", "worker", issue, "t", Some(label))
        .unwrap();
    store.update_run_status(&run.id, status, None).unwrap();
}

async fn discovered_issues(src: &LabelTaskSource, kind: TaskKind) -> Vec<i64> {
    let mut ns: Vec<i64> = src
        .discover(kind)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.key.number())
        .collect();
    ns.sort();
    ns
}

// ---- not-before -----------------------------------------------------------

#[tokio::test]
async fn not_before_github_marker_gates_until_passed() {
    let forge = Arc::new(FakeForge::with_issue(
        1,
        "Launch post",
        "body\n<!-- meguri:not-before 2026-07-20 -->",
        &[LABEL_READY],
    ));
    let store = Store::open_in_memory().unwrap();
    let (clock, time) = movable_clock(ts("2026-07-19T23:59:00Z"));
    let src = label_source(forge.clone(), store, Vec::new(), clock);

    // Before the instant: not discovered, and not a mark on the forge.
    assert!(discovered_issues(&src, TaskKind::Work).await.is_empty());
    assert_eq!(forge.labels_of(1), vec![LABEL_READY.to_string()]);
    assert!(forge.comments_of(1).is_empty());

    // After: discovered.
    time.store(ts("2026-07-20T00:00:00Z"), Ordering::Relaxed);
    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![1]);
}

#[tokio::test]
async fn not_before_unparsable_marker_fails_closed() {
    let forge = Arc::new(FakeForge::with_issue(
        1,
        "Typo'd date",
        "<!-- meguri:not-before 2026-13-40 -->",
        &[LABEL_READY],
    ));
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(ts("2030-01-01T00:00:00Z"));
    let src = label_source(forge.clone(), store, Vec::new(), clock);

    // Even far in the future, a garbled marker never opens the gate.
    assert!(discovered_issues(&src, TaskKind::Work).await.is_empty());
    assert!(forge.comments_of(1).is_empty());
}

#[tokio::test]
async fn not_before_local_field_gates_until_passed() {
    let store = Store::open_in_memory().unwrap();
    store
        .create_task_with_not_before(
            "proj",
            "work",
            "Local launch",
            "",
            "local",
            Some("2026-07-20T00:00:00Z"),
        )
        .unwrap();
    let (clock, time) = movable_clock(ts("2026-07-19T00:00:00Z"));
    let src = LocalTaskSource::new(store, "proj".into()).with_clock(clock);

    assert!(src.discover(TaskKind::Work).await.unwrap().is_empty());
    time.store(ts("2026-07-20T00:00:00Z"), Ordering::Relaxed);
    assert_eq!(src.discover(TaskKind::Work).await.unwrap().len(), 1);
}

// ---- cadence --------------------------------------------------------------

#[tokio::test]
async fn cadence_blocks_after_daily_limit_and_stamps_the_bucket() {
    let forge = Arc::new(FakeForge::with_issue(
        1,
        "Post A",
        "",
        &[LABEL_READY, "sns"],
    ));
    forge.add_issue(2, "Plain", "", &[LABEL_READY]); // control: no cadence label
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(base_now());
    let src = label_source(forge.clone(), store.clone(), vec![day("sns", 1)], clock);

    // No consumption yet: the sns issue and the plain issue both discover, and
    // the sns task carries its bucket.
    let tasks = src.discover(TaskKind::Work).await.unwrap();
    let sns = tasks.iter().find(|t| t.key.number() == 1).unwrap();
    assert_eq!(sns.cadence_label.as_deref(), Some("sns"));
    assert_eq!(tasks.len(), 2);

    // A same-day sns run consumes the day's single slot.
    consume(&store, 99, "sns", RunStatus::Succeeded);
    // sns is now held; the plain issue is unaffected. No forge trace.
    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![2]);
    assert_eq!(
        forge.labels_of(1),
        vec![LABEL_READY.to_string(), "sns".to_string()]
    );
    assert!(forge.comments_of(1).is_empty());
}

#[tokio::test]
async fn cadence_one_pass_never_over_emits_the_bucket() {
    let forge = Arc::new(FakeForge::with_issue(1, "A", "", &[LABEL_READY, "sns"]));
    forge.add_issue(2, "B", "", &[LABEL_READY, "sns"]);
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(ts("2026-07-20T09:00:00Z"));
    let src = label_source(forge, store, vec![day("sns", 1)], clock);

    // Two eligible sns issues, limit 1: exactly one is emitted this pass.
    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![1]);
}

#[tokio::test]
async fn cadence_day_window_rolls_over() {
    let forge = Arc::new(FakeForge::with_issue(1, "A", "", &[LABEL_READY, "sns"]));
    let store = Store::open_in_memory().unwrap();
    let base = base_now();
    let (clock, time) = movable_clock(base);
    let src = label_source(forge, store.clone(), vec![day("sns", 1)], clock);

    consume(&store, 99, "sns", RunStatus::Succeeded);
    assert!(discovered_issues(&src, TaskKind::Work).await.is_empty());

    // Next UTC day: the window (from that day's midnight) no longer includes the
    // run consumed today, so the issue is discoverable again.
    let next_midnight = base - (base % 86_400) + 86_400;
    time.store(next_midnight, Ordering::Relaxed);
    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![1]);
}

#[tokio::test]
async fn cadence_rolling_window_rolls_over() {
    let forge = Arc::new(FakeForge::with_issue(1, "A", "", &[LABEL_READY, "nl"]));
    let store = Store::open_in_memory().unwrap();
    let base = base_now();
    let (clock, time) = movable_clock(base);
    let src = label_source(forge, store.clone(), vec![hours("nl", 24, 1)], clock);

    consume(&store, 99, "nl", RunStatus::Succeeded);
    // 23h later: still inside the 24h window.
    time.store(base + 23 * 3600, Ordering::Relaxed);
    assert!(discovered_issues(&src, TaskKind::Work).await.is_empty());
    // 25h later: the consuming run has aged out of the window.
    time.store(base + 25 * 3600, Ordering::Relaxed);
    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![1]);
}

#[tokio::test]
async fn cadence_labels_have_independent_windows() {
    let forge = Arc::new(FakeForge::with_issue(1, "Post", "", &[LABEL_READY, "sns"]));
    forge.add_issue(2, "News", "", &[LABEL_READY, "nl"]);
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(base_now());
    let src = label_source(
        forge,
        store.clone(),
        vec![day("sns", 1), day("nl", 1)],
        clock,
    );

    // Filling the sns window leaves the newsletter window untouched.
    consume(&store, 99, "sns", RunStatus::Succeeded);
    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![2]);
}

#[tokio::test]
async fn skipped_runs_do_not_consume_the_window() {
    let forge = Arc::new(FakeForge::with_issue(1, "A", "", &[LABEL_READY, "sns"]));
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(base_now());
    let src = label_source(forge, store.clone(), vec![day("sns", 1)], clock);

    // A skipped run is a benign race that touched nothing: it must not count
    // (it lands inside the window, yet the sns issue still discovers).
    consume(&store, 99, "sns", RunStatus::Skipped);
    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![1]);
}

#[tokio::test]
async fn conflicting_cadence_labels_fail_closed() {
    let forge = Arc::new(FakeForge::with_issue(
        1,
        "Both",
        "",
        &[LABEL_READY, "sns", "nl"],
    ));
    forge.add_issue(2, "Only sns", "", &[LABEL_READY, "sns"]);
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(ts("2026-07-20T09:00:00Z"));
    let src = label_source(
        forge.clone(),
        store,
        vec![day("sns", 5), day("nl", 5)],
        clock,
    );

    // Issue 1 matches two rules → fail-closed (never discovered, no trace).
    // Issue 2 matches one → discovered normally.
    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![2]);
    assert!(forge.comments_of(1).is_empty());
}

#[tokio::test]
async fn blocked_candidate_does_not_consume_the_quota() {
    // Acceptance criterion 11: cadence is the last gate, so a dependency-blocked
    // candidate is filtered before it can eat the shared allowance — the later
    // actionable candidate still gets the day's slot.
    let forge = Arc::new(FakeForge::with_issue(
        1,
        "Blocked old",
        "",
        &[LABEL_READY, "sns"],
    ));
    forge.add_issue(2, "Actionable", "", &[LABEL_READY, "sns"]);
    forge.block_issue(1, 100); // blocker 100 is open → unresolved
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(ts("2026-07-20T09:00:00Z"));
    let src = label_source(forge, store, vec![day("sns", 1)], clock);

    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![2]);
}

// ---- the `meguri tasks` disposition view -----------------------------------

#[tokio::test]
async fn dispositions_share_the_pass_allowance_with_discover() {
    // Finding: the queue view must decrement the same remaining counter as
    // discovery, not read the store per issue. Two sns issues, limit 1, nothing
    // consumed yet → discover emits one; the view shows one ready, one waiting.
    let forge = Arc::new(FakeForge::with_issue(1, "A", "", &[LABEL_READY, "sns"]));
    forge.add_issue(2, "B", "", &[LABEL_READY, "sns"]);
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(base_now());
    let src = label_source(forge, store, vec![day("sns", 1)], clock);

    let rows = src.dispositions(TaskKind::Work).await.unwrap();
    let by_num: Vec<(i64, Disposition)> = rows.into_iter().map(|(i, d)| (i.number, d)).collect();
    assert_eq!(by_num[0].0, 1);
    assert_eq!(by_num[0].1, Disposition::Ready);
    assert_eq!(by_num[1].0, 2);
    match &by_num[1].1 {
        Disposition::WaitingCadence {
            label,
            consumed,
            max,
            ..
        } => {
            assert_eq!(label, "sns");
            assert_eq!(*consumed, 1); // effective: this pass took the only slot
            assert_eq!(*max, 1);
        }
        other => panic!("expected WaitingCadence, got {other:?}"),
    }
    // And discover agrees: exactly issue 1 runs.
    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![1]);
}

// ---- claim re-verifies the gates (a late body/label edit) ------------------

#[tokio::test]
async fn claim_rechecks_not_before_added_after_discovery() {
    let forge = Arc::new(FakeForge::with_issue(1, "Launch", "", &[LABEL_READY]));
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(base_now());
    let src = label_source(forge.clone(), store, Vec::new(), clock);

    // Discovered while actionable...
    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![1]);
    // ...but a future not-before is added to the body before the claim lands.
    forge
        .update_issue_body(1, "<!-- meguri:not-before 2999-01-01 -->")
        .await
        .unwrap();
    // The run carried no bucket (no cadence rules).
    assert!(
        src.claim(&TaskKey::Issue(1), LOCAL_HOST, None)
            .await
            .unwrap()
            .is_none()
    );
    // No `working` label written (benign race → Skipped, no forge trace).
    assert!(!forge.labels_of(1).contains(&LABEL_WORKING.to_string()));
}

#[tokio::test]
async fn claim_rechecks_cadence_label_conflict_added_after_discovery() {
    let forge = Arc::new(FakeForge::with_issue(1, "Post", "", &[LABEL_READY, "sns"]));
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(base_now());
    let src = label_source(
        forge.clone(),
        store,
        vec![day("sns", 5), day("nl", 5)],
        clock,
    );

    assert_eq!(discovered_issues(&src, TaskKind::Work).await, vec![1]);
    // A second cadence label is added, so the bucket is now ambiguous — the run
    // was stamped `sns`, but no single bucket matches now.
    forge.add_label(1, "nl").await.unwrap();
    assert!(
        src.claim(&TaskKey::Issue(1), LOCAL_HOST, Some("sns"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(!forge.labels_of(1).contains(&LABEL_WORKING.to_string()));
}

#[tokio::test]
async fn claim_rejects_bucket_label_swapped_after_discovery() {
    // The run was stamped `sns`, but the label was swapped to `nl` before claim:
    // a single rule still matches, yet consuming `sns` would leave `nl` free to
    // over-run. The stamp no longer matches → benign race → None.
    let forge = Arc::new(FakeForge::with_issue(1, "Post", "", &[LABEL_READY, "sns"]));
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(base_now());
    let src = label_source(
        forge.clone(),
        store,
        vec![day("sns", 5), day("nl", 5)],
        clock,
    );

    let tasks = src.discover(TaskKind::Work).await.unwrap();
    assert_eq!(tasks[0].cadence_label.as_deref(), Some("sns"));

    forge.remove_label(1, "sns").await.unwrap();
    forge.add_label(1, "nl").await.unwrap();
    assert!(
        src.claim(&TaskKey::Issue(1), LOCAL_HOST, Some("sns"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(!forge.labels_of(1).contains(&LABEL_WORKING.to_string()));
}

#[tokio::test]
async fn claim_rejects_bucket_added_to_unbucketed_run() {
    // The run carried no bucket (issue had no cadence label at discovery), but a
    // cadence label was added before claim. Running it would consume nothing
    // while the issue is now `sns` → over-run. Stamp mismatch → None.
    let forge = Arc::new(FakeForge::with_issue(1, "Post", "", &[LABEL_READY]));
    let store = Store::open_in_memory().unwrap();
    let (clock, _time) = movable_clock(base_now());
    let src = label_source(forge.clone(), store, vec![day("sns", 5)], clock);

    let tasks = src.discover(TaskKind::Work).await.unwrap();
    assert_eq!(tasks[0].cadence_label, None);

    forge.add_label(1, "sns").await.unwrap();
    assert!(
        src.claim(&TaskKey::Issue(1), LOCAL_HOST, None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(!forge.labels_of(1).contains(&LABEL_WORKING.to_string()));

    // Sanity: a run correctly stamped `sns` still claims.
    assert!(
        src.claim(&TaskKey::Issue(1), LOCAL_HOST, Some("sns"))
            .await
            .unwrap()
            .is_some()
    );
    assert!(forge.labels_of(1).contains(&LABEL_WORKING.to_string()));
}
