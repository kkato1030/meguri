//! Unit tests for `TurnEngine::await_completion_direct` (issue #169): the
//! direct launch mode executor, which watches a plain subprocess exit
//! instead of polling a mux pane. Real (short-lived) child processes —
//! `try_wait` polling doesn't need a fake mux.

#![cfg(unix)]

use std::sync::Mutex;
use std::time::Duration;

use meguri::store::{DesiredState, InteractionState};
use meguri::turn::{TurnConfig, TurnControl, TurnEngine, TurnOutcome, TurnStatus, prepare_turn};

struct FakeControl {
    desired: Mutex<Option<DesiredState>>,
    interactions: Mutex<Vec<InteractionState>>,
    events: Mutex<Vec<String>>,
}

impl FakeControl {
    fn new() -> Self {
        Self {
            desired: Mutex::new(None),
            interactions: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
        }
    }

    fn event_kinds(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl TurnControl for FakeControl {
    async fn desired(&self) -> Option<DesiredState> {
        *self.desired.lock().unwrap()
    }

    async fn set_interaction(&self, state: InteractionState) {
        self.interactions.lock().unwrap().push(state);
    }

    async fn event(&self, kind: &str, _data: serde_json::Value) {
        self.events.lock().unwrap().push(kind.to_string());
    }
}

fn engine(cfg: TurnConfig) -> TurnEngine {
    // await_completion_direct never touches `self.mux` — a FakeMux stands in
    // purely to satisfy the field.
    TurnEngine {
        mux: std::sync::Arc::new(meguri::mux::fake::FakeMux::new(false)),
        cfg,
    }
}

fn fast_cfg() -> TurnConfig {
    TurnConfig {
        poll_interval: Duration::from_millis(20),
        idle_grace: Duration::from_secs(600),
        nudge_limit: 0,
        max_turn_runtime: Duration::from_secs(600),
        result_grace: Duration::from_secs(1),
    }
}

fn sh_child(worktree: &std::path::Path, script: &str) -> tokio::process::Child {
    tokio::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(worktree)
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

#[tokio::test]
async fn completes_when_the_process_exits_with_a_matching_result() {
    let dir = tempfile::tempdir().unwrap();
    let prepared = prepare_turn(dir.path(), "do the thing", "").unwrap();
    let control = FakeControl::new();

    let child = sh_child(
        dir.path(),
        &format!(
            "mkdir -p .meguri && printf '{{\"turn_id\":\"{}\",\"status\":\"success\",\"summary\":\"s\"}}' > .meguri/result.json",
            prepared.turn_id
        ),
    );

    let outcome = engine(fast_cfg())
        .await_completion_direct(child, dir.path(), &prepared.turn_id, &control)
        .await
        .unwrap();

    match outcome {
        TurnOutcome::Completed(r) => assert_eq!(r.status, TurnStatus::Success),
        other => panic!("expected Completed, got {other:?}"),
    }
    assert!(
        control
            .event_kinds()
            .contains(&"turn.completed".to_string())
    );
}

#[tokio::test]
async fn exiting_without_a_result_maps_to_pane_died() {
    let dir = tempfile::tempdir().unwrap();
    let prepared = prepare_turn(dir.path(), "do the thing", "").unwrap();
    let control = FakeControl::new();

    // The subprocess exits (e.g. the CLI crashed) without writing the result
    // file — same Interrupted-style handling as a dead pane upstream.
    let child = sh_child(dir.path(), "exit 1");

    let outcome = engine(fast_cfg())
        .await_completion_direct(child, dir.path(), &prepared.turn_id, &control)
        .await
        .unwrap();

    assert!(matches!(outcome, TurnOutcome::PaneDied));
}

#[tokio::test]
async fn stop_kills_the_subprocess() {
    let dir = tempfile::tempdir().unwrap();
    let prepared = prepare_turn(dir.path(), "do the thing", "").unwrap();
    let control = FakeControl::new();
    *control.desired.lock().unwrap() = Some(DesiredState::Stopped);

    let child = sh_child(dir.path(), "sleep 30");

    let outcome = engine(fast_cfg())
        .await_completion_direct(child, dir.path(), &prepared.turn_id, &control)
        .await
        .unwrap();

    assert!(matches!(outcome, TurnOutcome::Stopped));
}

#[tokio::test]
async fn runtime_budget_escalates_without_killing() {
    let dir = tempfile::tempdir().unwrap();
    let prepared = prepare_turn(dir.path(), "do the thing", "").unwrap();
    let control = FakeControl::new();
    let mut cfg = fast_cfg();
    cfg.max_turn_runtime = Duration::from_millis(60);

    // Runs well past the (tiny) runtime budget, then finishes successfully —
    // the direct executor must escalate but never kill (same contract as the
    // pane executor).
    let child = sh_child(
        dir.path(),
        &format!(
            "sleep 0.3 && mkdir -p .meguri && printf '{{\"turn_id\":\"{}\",\"status\":\"success\",\"summary\":\"s\"}}' > .meguri/result.json",
            prepared.turn_id
        ),
    );

    let outcome = engine(cfg)
        .await_completion_direct(child, dir.path(), &prepared.turn_id, &control)
        .await
        .unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert!(
        control
            .event_kinds()
            .contains(&"turn.awaiting_human".to_string())
    );
}
