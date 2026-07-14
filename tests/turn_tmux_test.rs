//! End-to-end turn-engine tests against a REAL tmux running the fake agent.
//! Uses real time; skipped when tmux is unavailable.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meguri::mux::tmux::TmuxMux;
use meguri::mux::{Multiplexer, PaneSpec};
use meguri::store::{DesiredState, InteractionState};
use meguri::turn::{TurnConfig, TurnControl, TurnEngine, TurnOutcome, TurnStatus, prepare_turn};

fn tmux_available() -> bool {
    std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn fake_agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.sh")
}

struct RecordingControl {
    events: Mutex<Vec<String>>,
}

impl RecordingControl {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
        })
    }

    fn has_event(&self, kind: &str) -> bool {
        self.events.lock().unwrap().iter().any(|k| k == kind)
    }
}

#[async_trait::async_trait]
impl TurnControl for RecordingControl {
    async fn desired(&self) -> Option<DesiredState> {
        None
    }
    async fn set_interaction(&self, _state: InteractionState) {}
    async fn event(&self, kind: &str, _data: serde_json::Value) {
        self.events.lock().unwrap().push(kind.to_string());
    }
}

fn fast_cfg() -> TurnConfig {
    TurnConfig {
        poll_interval: Duration::from_millis(500),
        idle_grace: Duration::from_secs(8),
        nudge_limit: 2,
        max_turn_runtime: Duration::from_secs(120),
        result_grace: Duration::from_secs(3),
    }
}

async fn cleanup(session: &str) {
    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", session])
        .output()
        .await;
}

#[tokio::test]
async fn full_turn_happy_path_in_tmux() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let session = format!("meguri-turn-{}", std::process::id());
    let mux = Arc::new(TmuxMux::new(&session));
    let dir = tempfile::tempdir().unwrap();
    let control = RecordingControl::new();

    let prepared = prepare_turn(dir.path(), "Implement the feature.", "").unwrap();

    // Turn 1: the trigger line is the agent's initial prompt argument.
    let pane = mux
        .spawn_pane(&PaneSpec {
            title: "worker".into(),
            cwd: dir.path().to_path_buf(),
            command: vec![
                "bash".into(),
                fake_agent_path().to_string_lossy().to_string(),
                prepared.trigger_line.clone(),
            ],
            env: vec![("FAKE_AGENT_SCRIPT".into(), "work:2,result:success".into())],
        })
        .await
        .expect("spawn pane");

    let engine = TurnEngine {
        mux: mux.clone(),
        cfg: fast_cfg(),
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        engine.await_completion(&pane, dir.path(), &prepared.turn_id, control.as_ref()),
    )
    .await
    .expect("turn timed out")
    .unwrap();

    match outcome {
        TurnOutcome::Completed(r) => {
            assert_eq!(r.status, TurnStatus::Success);
            assert!(r.summary.contains("success"));
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    mux.kill_pane(&pane).await.ok();
    cleanup(&session).await;
}

#[tokio::test]
async fn fake_agent_reports_session_id_through_result_contract() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let session = format!("meguri-turnsess-{}", std::process::id());
    let mux = Arc::new(TmuxMux::new(&session));
    let dir = tempfile::tempdir().unwrap();
    let control = RecordingControl::new();

    let prepared = prepare_turn(dir.path(), "Implement the feature.", "").unwrap();

    let pane = mux
        .spawn_pane(&PaneSpec {
            title: "worker".into(),
            cwd: dir.path().to_path_buf(),
            command: vec![
                "bash".into(),
                fake_agent_path().to_string_lossy().to_string(),
                prepared.trigger_line.clone(),
            ],
            env: vec![
                ("FAKE_AGENT_SCRIPT".into(), "work:1,result:success".into()),
                ("FAKE_AGENT_SESSION_ID".into(), "sess-tmux-1".into()),
            ],
        })
        .await
        .expect("spawn pane");

    let engine = TurnEngine {
        mux: mux.clone(),
        cfg: fast_cfg(),
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        engine.await_completion(&pane, dir.path(), &prepared.turn_id, control.as_ref()),
    )
    .await
    .expect("turn timed out")
    .unwrap();

    match outcome {
        TurnOutcome::Completed(r) => {
            assert_eq!(r.status, TurnStatus::Success);
            assert_eq!(r.agent_session_id.as_deref(), Some("sess-tmux-1"));
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    mux.kill_pane(&pane).await.ok();
    cleanup(&session).await;
}

#[tokio::test]
async fn fake_agent_resumed_with_session_argv_reports_it_back() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let session = format!("meguri-turnres-{}", std::process::id());
    let mux = Arc::new(TmuxMux::new(&session));
    let dir = tempfile::tempdir().unwrap();
    let control = RecordingControl::new();

    let prepared = prepare_turn(dir.path(), "Pick up where you left off.", "").unwrap();

    // The recovery spawn shape: `<agent> --resume <id> <trigger>`.
    let pane = mux
        .spawn_pane(&PaneSpec {
            title: "worker".into(),
            cwd: dir.path().to_path_buf(),
            command: vec![
                "bash".into(),
                fake_agent_path().to_string_lossy().to_string(),
                "--resume".into(),
                "sess-resumed-9".into(),
                prepared.trigger_line.clone(),
            ],
            env: vec![("FAKE_AGENT_SCRIPT".into(), "work:1,result:success".into())],
        })
        .await
        .expect("spawn pane");

    let engine = TurnEngine {
        mux: mux.clone(),
        cfg: fast_cfg(),
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        engine.await_completion(&pane, dir.path(), &prepared.turn_id, control.as_ref()),
    )
    .await
    .expect("turn timed out")
    .unwrap();

    match outcome {
        TurnOutcome::Completed(r) => {
            assert_eq!(r.status, TurnStatus::Success);
            assert_eq!(r.agent_session_id.as_deref(), Some("sess-resumed-9"));
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    mux.kill_pane(&pane).await.ok();
    cleanup(&session).await;
}

#[tokio::test]
async fn blocked_turn_waits_for_human_answer_in_tmux() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let session = format!("meguri-turnblk-{}", std::process::id());
    let mux = Arc::new(TmuxMux::new(&session));
    let dir = tempfile::tempdir().unwrap();
    let control = RecordingControl::new();

    let prepared = prepare_turn(dir.path(), "Do something needing approval.", "").unwrap();

    let pane = mux
        .spawn_pane(&PaneSpec {
            title: "worker".into(),
            cwd: dir.path().to_path_buf(),
            command: vec![
                "bash".into(),
                fake_agent_path().to_string_lossy().to_string(),
                prepared.trigger_line.clone(),
            ],
            env: vec![(
                "FAKE_AGENT_SCRIPT".into(),
                "work:1,block,result:success".into(),
            )],
        })
        .await
        .expect("spawn pane");

    // A "human" watches for the awaiting_human escalation and answers the
    // agent's question directly in the pane.
    let human = {
        let mux = mux.clone();
        let pane = pane.clone();
        let control = control.clone();
        tokio::spawn(async move {
            for _ in 0..120 {
                if control.has_event("turn.awaiting_human") {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    mux.send_line(&pane, "1").await.unwrap();
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            false
        })
    };

    let engine = TurnEngine {
        mux: mux.clone(),
        cfg: fast_cfg(),
    };
    let outcome = tokio::time::timeout(
        Duration::from_secs(90),
        engine.await_completion(&pane, dir.path(), &prepared.turn_id, control.as_ref()),
    )
    .await
    .expect("turn timed out")
    .unwrap();

    assert!(
        human.await.unwrap(),
        "human helper never saw awaiting_human"
    );
    assert!(control.has_event("turn.awaiting_human"));
    match outcome {
        TurnOutcome::Completed(r) => assert_eq!(r.status, TurnStatus::Success),
        other => panic!("expected Completed, got {other:?}"),
    }

    mux.kill_pane(&pane).await.ok();
    cleanup(&session).await;
}
