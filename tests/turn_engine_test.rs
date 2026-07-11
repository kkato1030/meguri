//! Unit tests for the turn engine over FakeMux, using paused tokio time so
//! the whole suite runs in milliseconds.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use meguri::mux::fake::FakeMux;
use meguri::mux::{AgentState, Multiplexer, PaneId, PaneSpec};
use meguri::store::{DesiredState, InteractionState};
use meguri::turn::{TurnConfig, TurnControl, TurnEngine, TurnOutcome, TurnStatus, prepare_turn};

struct FakeControl {
    desired: Mutex<Option<DesiredState>>,
    interactions: Mutex<Vec<InteractionState>>,
    events: Mutex<Vec<String>>,
}

impl FakeControl {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            desired: Mutex::new(None),
            interactions: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
        })
    }

    fn set_desired(&self, d: Option<DesiredState>) {
        *self.desired.lock().unwrap() = d;
    }

    fn event_kinds(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn saw_interaction(&self, s: InteractionState) -> bool {
        self.interactions.lock().unwrap().contains(&s)
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

struct Setup {
    mux: Arc<FakeMux>,
    engine: TurnEngine,
    pane: PaneId,
    dir: tempfile::TempDir,
    turn_id: String,
}

fn cfg() -> TurnConfig {
    TurnConfig {
        poll_interval: Duration::from_secs(2),
        idle_grace: Duration::from_secs(10),
        nudge_limit: 2,
        max_turn_runtime: Duration::from_secs(600),
        result_grace: Duration::from_secs(6),
    }
}

async fn setup() -> Setup {
    let mux = Arc::new(FakeMux::new(false));
    let pane = mux
        .spawn_pane(&PaneSpec {
            title: "t".into(),
            cwd: std::env::temp_dir(),
            command: vec!["agent".into()],
            env: vec![],
        })
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let prepared = prepare_turn(dir.path(), "do the thing").unwrap();
    let engine = TurnEngine {
        mux: mux.clone(),
        cfg: cfg(),
    };
    Setup {
        mux,
        engine,
        pane,
        dir,
        turn_id: prepared.turn_id,
    }
}

fn write_result(dir: &std::path::Path, turn_id: &str, status: &str) {
    std::fs::write(
        dir.join(".meguri/result.json"),
        format!(r#"{{"turn_id":"{turn_id}","status":"{status}","summary":"s"}}"#),
    )
    .unwrap();
}

#[tokio::test(start_paused = true)]
async fn happy_path_completes_on_result_file() {
    let s = setup().await;
    let control = FakeControl::new();

    let driver = {
        let mux = s.mux.clone();
        let pane = s.pane.clone();
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(6)).await;
            mux.set_state(&pane, AgentState::Idle);
            write_result(&dir, &turn_id, "success");
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    driver.await.unwrap();

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

#[tokio::test(start_paused = true)]
async fn stale_turn_id_is_ignored() {
    let s = setup().await;
    let control = FakeControl::new();

    let driver = {
        let mux = s.mux.clone();
        let pane = s.pane.clone();
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            mux.set_state(&pane, AgentState::Idle);
            write_result(&dir, "some-old-turn", "success");
            tokio::time::sleep(Duration::from_secs(8)).await;
            write_result(&dir, &turn_id, "failure");
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    driver.await.unwrap();

    match outcome {
        TurnOutcome::Completed(r) => assert_eq!(r.status, TurnStatus::Failure),
        other => panic!("expected Completed(failure), got {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn result_acceptance_waits_for_working_to_settle() {
    let s = setup().await;
    let control = FakeControl::new();

    let driver = {
        let mux = s.mux.clone();
        let pane = s.pane.clone();
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            // Result appears while the agent is still Working…
            write_result(&dir, &turn_id, "success");
            // …and the agent keeps working past the grace period; the engine
            // must accept after result_grace rather than waiting forever.
            tokio::time::sleep(Duration::from_secs(60)).await;
            mux.set_state(&pane, AgentState::Idle);
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    driver.abort();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
}

#[tokio::test(start_paused = true)]
async fn blocked_escalates_to_awaiting_human_then_recovers() {
    let s = setup().await;
    let control = FakeControl::new();

    let driver = {
        let mux = s.mux.clone();
        let pane = s.pane.clone();
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            mux.set_state(&pane, AgentState::Blocked);
            tokio::time::sleep(Duration::from_secs(30)).await;
            // Human answered the prompt; agent resumes, then finishes.
            mux.set_state(&pane, AgentState::Working);
            tokio::time::sleep(Duration::from_secs(10)).await;
            mux.set_state(&pane, AgentState::Idle);
            write_result(&dir, &turn_id, "success");
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    driver.await.unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert!(control.saw_interaction(InteractionState::AwaitingHuman));
    assert!(control.saw_interaction(InteractionState::AgentWorking));
    assert!(
        control
            .event_kinds()
            .contains(&"turn.awaiting_human".to_string())
    );
    // No nudges may be sent while the agent waits for a human.
    assert!(!control.event_kinds().contains(&"turn.nudged".to_string()));
    assert!(s.mux.sent_lines(&s.pane).is_empty());
}

#[tokio::test(start_paused = true)]
async fn quiet_agent_gets_nudged_then_escalates() {
    let s = setup().await;
    let control = FakeControl::new();
    s.mux.set_state(&s.pane, AgentState::Idle);

    let driver = {
        let mux = s.mux.clone();
        let pane = s.pane.clone();
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            // Stay silent long enough for 2 nudges + escalation, then a
            // human rescues the turn.
            tokio::time::sleep(Duration::from_secs(60)).await;
            write_result(&dir, &turn_id, "needs_human");
            let _ = (mux, pane);
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    driver.await.unwrap();

    assert!(matches!(
        outcome,
        TurnOutcome::Completed(r) if r.status == TurnStatus::NeedsHuman
    ));
    let sent = s.mux.sent_lines(&s.pane);
    assert_eq!(sent.len(), 2, "expected exactly 2 nudges, got {sent:?}");
    assert!(sent[0].contains("result.json"));
    let kinds = control.event_kinds();
    assert_eq!(kinds.iter().filter(|k| *k == "turn.nudged").count(), 2);
    assert!(kinds.contains(&"turn.awaiting_human".to_string()));
}

#[tokio::test(start_paused = true)]
async fn moving_screen_defers_nudges() {
    let s = setup().await;
    let control = FakeControl::new();
    s.mux.set_state(&s.pane, AgentState::Idle);

    let driver = {
        let mux = s.mux.clone();
        let pane = s.pane.clone();
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            // A human is typing: the screen changes every few seconds even
            // though the agent state reads Idle.
            for i in 0..20 {
                mux.set_tail(&pane, vec![format!("human typing {i}")]);
                tokio::time::sleep(Duration::from_secs(4)).await;
            }
            write_result(&dir, &turn_id, "success");
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    driver.await.unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert!(
        s.mux.sent_lines(&s.pane).is_empty(),
        "no nudge may fire while the screen is moving"
    );
}

#[tokio::test(start_paused = true)]
async fn pause_suspends_then_resume_continues() {
    let s = setup().await;
    let control = FakeControl::new();
    control.set_desired(Some(DesiredState::Paused));

    let driver = {
        let control = control.clone();
        let mux = s.mux.clone();
        let pane = s.pane.clone();
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            control.set_desired(None);
            tokio::time::sleep(Duration::from_secs(10)).await;
            mux.set_state(&pane, AgentState::Idle);
            write_result(&dir, &turn_id, "success");
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    driver.await.unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert!(control.saw_interaction(InteractionState::Paused));
    let kinds = control.event_kinds();
    assert!(kinds.contains(&"turn.paused".to_string()));
    assert!(kinds.contains(&"turn.resumed".to_string()));
    // While paused, nothing may be typed into the pane.
    assert!(s.mux.sent_lines(&s.pane).is_empty());
}

#[tokio::test(start_paused = true)]
async fn stop_exits_immediately() {
    let s = setup().await;
    let control = FakeControl::new();
    control.set_desired(Some(DesiredState::Stopped));

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    assert!(matches!(outcome, TurnOutcome::Stopped));
}

#[tokio::test(start_paused = true)]
async fn takeover_goes_hands_off_but_honors_result() {
    let s = setup().await;
    let control = FakeControl::new();
    control.set_desired(Some(DesiredState::Takeover));
    s.mux.set_state(&s.pane, AgentState::Idle);

    let driver = {
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            // Long silence during human driving must produce no nudges.
            tokio::time::sleep(Duration::from_secs(120)).await;
            write_result(&dir, &turn_id, "success");
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    driver.await.unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert!(control.saw_interaction(InteractionState::HumanDriving));
    assert!(s.mux.sent_lines(&s.pane).is_empty());
}

#[tokio::test(start_paused = true)]
async fn dead_pane_ends_turn() {
    let s = setup().await;
    let control = FakeControl::new();

    let driver = {
        let mux = s.mux.clone();
        let pane = s.pane.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(6)).await;
            mux.kill(&pane);
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    driver.await.unwrap();

    assert!(matches!(outcome, TurnOutcome::PaneDied));
}

#[tokio::test(start_paused = true)]
async fn runtime_budget_escalates_without_killing() {
    let s = setup().await;
    let control = FakeControl::new();
    // Agent stays busy forever (Working resets the stagnation clock but
    // accumulates the runtime budget).
    s.mux.set_state(&s.pane, AgentState::Working);

    let driver = {
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            // Past the 600s budget, then the (notified) human wraps it up.
            tokio::time::sleep(Duration::from_secs(700)).await;
            write_result(&dir, &turn_id, "success");
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, control.as_ref())
        .await
        .unwrap();
    driver.await.unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert!(control.saw_interaction(InteractionState::AwaitingHuman));
    assert!(
        control
            .event_kinds()
            .contains(&"turn.awaiting_human".to_string())
    );
}
