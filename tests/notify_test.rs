//! awaiting_human 通知の統合テスト: TurnEngine を StoreControl + 記録専用の
//! FakeGateway で駆動し(paused tokio time)、blocked→解除→再 blocked の
//! 2 回目がスロットル窓(60 秒)の内側では抑制され、外側では配送される
//! 境界を検証する。イベントログには両方とも残ることも確認する。

use std::sync::Arc;
use std::time::Duration;

use meguri::engine::StoreControl;
use meguri::mux::fake::FakeMux;
use meguri::mux::{AgentState, Multiplexer, PaneId, PaneSpec};
use meguri::notify::fake::{FakeGateway, recording_notifier};
use meguri::store::Store;
use meguri::turn::{TurnConfig, TurnEngine, TurnOutcome, prepare_turn};

struct Setup {
    mux: Arc<FakeMux>,
    engine: TurnEngine,
    pane: PaneId,
    dir: tempfile::TempDir,
    turn_id: String,
    store: Store,
    run_id: String,
    control: StoreControl,
    gateway: Arc<FakeGateway>,
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
        cfg: TurnConfig {
            poll_interval: Duration::from_secs(2),
            // Long grace/budget so only Blocked transitions escalate here.
            idle_grace: Duration::from_secs(600),
            nudge_limit: 2,
            max_turn_runtime: Duration::from_secs(3600),
            result_grace: Duration::from_secs(6),
        },
    };
    let store = Store::open_in_memory().unwrap();
    let run = store.create_run("proj", 7, "awaiting_human 通知").unwrap();
    let (notifier, gateway) = recording_notifier(); // 60s throttle
    let control = StoreControl {
        store: store.clone(),
        run_id: run.id.clone(),
        notifier,
    };
    Setup {
        mux,
        engine,
        pane,
        dir,
        turn_id: prepared.turn_id,
        store,
        run_id: run.id,
        control,
        gateway,
    }
}

fn write_result(dir: &std::path::Path, turn_id: &str) {
    std::fs::write(
        dir.join(".meguri/result.json"),
        format!(r#"{{"turn_id":"{turn_id}","status":"success","summary":"s"}}"#),
    )
    .unwrap();
}

fn awaiting_human_events(store: &Store, run_id: &str) -> usize {
    store
        .events_for_run(run_id, 100)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "turn.awaiting_human")
        .count()
}

/// blocked→解除→再 blocked が throttle 窓の内側(≈26 秒)で起きると、
/// イベントは 2 回出るが通知は 1 回だけ配送される。
#[tokio::test(start_paused = true)]
async fn reblock_inside_throttle_window_notifies_once() {
    let s = setup().await;

    let driver = {
        let mux = s.mux.clone();
        let pane = s.pane.clone();
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            mux.set_state(&pane, AgentState::Blocked);
            tokio::time::sleep(Duration::from_secs(20)).await;
            // Human answered; agent resumes…
            mux.set_state(&pane, AgentState::Working);
            tokio::time::sleep(Duration::from_secs(6)).await;
            // …then blocks again well inside the 60s throttle window.
            mux.set_state(&pane, AgentState::Blocked);
            tokio::time::sleep(Duration::from_secs(10)).await;
            mux.set_state(&pane, AgentState::Idle);
            write_result(&dir, &turn_id);
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, &s.control)
        .await
        .unwrap();
    driver.await.unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert_eq!(
        awaiting_human_events(&s.store, &s.run_id),
        2,
        "both escalations must reach the event log"
    );
    let delivered = s.gateway.delivered();
    assert_eq!(delivered.len(), 1, "second escalation must be throttled");
    assert_eq!(delivered[0].event, "awaiting_human");
    assert_eq!(delivered[0].dedup_key, s.run_id);
    assert!(delivered[0].title.contains("#7"));
    assert!(delivered[0].title.contains("awaiting_human 通知"));
    assert!(
        delivered[0]
            .body
            .contains("エージェントが人の入力を待っています"),
        "reason surfaces in the body: {}",
        delivered[0].body
    );
    assert!(
        delivered[0].body.contains("fake pane"),
        "turn escalation points at the live pane: {}",
        delivered[0].body
    );
    assert!(
        delivered[0].url.is_none(),
        "turn escalation has no PR url — it points at a pane"
    );
}

/// 再 blocked が throttle 窓を過ぎてから(≥60 秒)起きると 2 回とも配送される。
#[tokio::test(start_paused = true)]
async fn reblock_past_throttle_window_notifies_again() {
    let s = setup().await;

    let driver = {
        let mux = s.mux.clone();
        let pane = s.pane.clone();
        let dir = s.dir.path().to_path_buf();
        let turn_id = s.turn_id.clone();
        tokio::spawn(async move {
            mux.set_state(&pane, AgentState::Blocked);
            tokio::time::sleep(Duration::from_secs(30)).await;
            mux.set_state(&pane, AgentState::Working);
            // Re-block only after the 60s window (measured from the first
            // delivery at t≈0) has fully elapsed.
            tokio::time::sleep(Duration::from_secs(40)).await;
            mux.set_state(&pane, AgentState::Blocked);
            tokio::time::sleep(Duration::from_secs(10)).await;
            mux.set_state(&pane, AgentState::Idle);
            write_result(&dir, &turn_id);
        })
    };

    let outcome = s
        .engine
        .await_completion(&s.pane, s.dir.path(), &s.turn_id, false, &s.control)
        .await
        .unwrap();
    driver.await.unwrap();

    assert!(matches!(outcome, TurnOutcome::Completed(_)));
    assert_eq!(awaiting_human_events(&s.store, &s.run_id), 2);
    let delivered = s.gateway.delivered();
    assert_eq!(
        delivered.len(),
        2,
        "past the throttle window the human must be paged again"
    );
    assert!(
        delivered
            .iter()
            .all(|n| n.body.contains("エージェントが人の入力を待っています"))
    );
}
