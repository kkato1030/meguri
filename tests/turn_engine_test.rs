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
    let prepared = prepare_turn(dir.path(), "do the thing", "").unwrap();
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
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
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
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
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
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
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
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
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

/// Issue #245: an agent that stays silent past its nudge budget is no longer
/// parked on awaiting_human — the quiet is returned as `AgentQuiet` (with the
/// pane tail for diagnosis) so the flow layer can rotate the session instead
/// of resume-looping forever.
#[tokio::test(start_paused = true)]
async fn quiet_agent_gets_nudged_then_returns_agent_quiet() {
    let s = setup().await;
    let control = FakeControl::new();
    s.mux.set_state(&s.pane, AgentState::Idle);
    s.mux.set_tail(
        &s.pane,
        vec!["API Error: 400 input exceeds the context window".into()],
    );

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
        .await
        .unwrap();

    let TurnOutcome::AgentQuiet { tail } = outcome else {
        panic!("expected AgentQuiet, got {outcome:?}");
    };
    assert!(tail.iter().any(|l| l.contains("API Error: 400")));
    let sent = s.mux.sent_lines(&s.pane);
    assert_eq!(sent.len(), 2, "expected exactly 2 nudges, got {sent:?}");
    assert!(sent[0].contains("result.json"));
    let kinds = control.event_kinds();
    assert_eq!(kinds.iter().filter(|k| *k == "turn.nudged").count(), 2);
    assert!(
        !kinds.contains(&"turn.awaiting_human".to_string()),
        "quiet no longer parks on awaiting_human: {kinds:?}"
    );
}

/// Issue #245 (acceptance 2): a pane whose agent exited to a bare shell must
/// not receive a nudge — typing into a shell is the failure. The definite
/// absent reads as an immediate pane death instead.
#[tokio::test(start_paused = true)]
async fn agent_absent_pane_is_never_nudged() {
    let s = setup().await;
    let control = FakeControl::new();
    s.mux.set_state(&s.pane, AgentState::Idle);
    s.mux.set_agent_present(&s.pane, Some(false));

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
        .await
        .unwrap();

    assert!(matches!(outcome, TurnOutcome::PaneDied));
    assert!(
        s.mux.sent_lines(&s.pane).is_empty(),
        "no nudge may be typed into a bare shell"
    );
    let kinds = control.event_kinds();
    assert!(!kinds.contains(&"turn.nudged".to_string()));
    assert!(kinds.contains(&"turn.pane_died".to_string()));
}

/// Issue #245: `agent_present == None` (mux cannot tell) must keep the
/// pre-existing nudge behavior — the gate only acts on a definite absent.
#[tokio::test(start_paused = true)]
async fn unknown_agent_presence_keeps_nudging() {
    let s = setup().await;
    let control = FakeControl::new();
    s.mux.set_state(&s.pane, AgentState::Idle);
    s.mux.set_agent_present(&s.pane, None);

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
        .await
        .unwrap();

    assert!(matches!(outcome, TurnOutcome::AgentQuiet { .. }));
    assert_eq!(s.mux.sent_lines(&s.pane).len(), 2, "nudges still happen");
}

/// Issue #214: a stalled *isolated* (parallel round-1) reviewer in pane mode
/// must be nudged to write its per-turn `result-<turn_id>.json`, not the shared
/// `result.json` its siblings also use — otherwise the nudge re-introduces the
/// very race the per-turn result file exists to prevent.
#[tokio::test(start_paused = true)]
async fn quiet_isolated_agent_is_nudged_to_its_per_turn_result() {
    let s = setup().await;
    let control = FakeControl::new();
    s.mux.set_state(&s.pane, AgentState::Idle);

    let driver = {
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            // Past the first nudge (idle_grace 10s) but inside the nudge
            // budget — a quiet that outlives it now returns AgentQuiet
            // (issue #245) instead of waiting for this rescue.
            tokio::time::sleep(Duration::from_secs(15)).await;
            // The isolated turn's completion authority is the per-turn file.
            std::fs::write(
                dir.join(format!(".meguri/result-{turn_id}.json")),
                format!(r#"{{"turn_id":"{turn_id}","status":"needs_human","summary":"s"}}"#),
            )
            .unwrap();
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, true, control.as_ref())
        .await
        .unwrap();
    driver.await.unwrap();

    assert!(matches!(
        outcome,
        TurnOutcome::Completed(r) if r.status == TurnStatus::NeedsHuman
    ));
    let sent = s.mux.sent_lines(&s.pane);
    assert!(!sent.is_empty(), "expected at least one nudge");
    let want = format!("result-{}.json", s.turn_id);
    assert!(
        sent[0].contains(&want),
        "isolated nudge must name the per-turn result file, got {:?}",
        sent[0]
    );
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
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
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
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
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
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
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
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
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
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
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
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, control.as_ref())
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
