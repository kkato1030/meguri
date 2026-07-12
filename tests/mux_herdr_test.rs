//! Integration tests for the herdr Multiplexer implementation.
//!
//! These need a *live* herdr server and mutate it (they create and remove a
//! test workspace), so they are gated behind MEGURI_TEST_HERDR=1.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use meguri::mux::{AgentState, Multiplexer, MuxError, PaneId, PaneSpec, Split, herdr::HerdrMux};

fn herdr_enabled() -> bool {
    std::env::var("MEGURI_TEST_HERDR").as_deref() == Ok("1") && HerdrMux::socket_live()
}

fn fake_agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.sh")
}

/// The workspace id herdr assigned to `label`, if such a workspace exists.
async fn workspace_id_for(label: &str) -> Option<String> {
    let out = tokio::process::Command::new("herdr")
        .args(["workspace", "list"])
        .output()
        .await
        .ok()?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let parsed = serde_json::from_str::<serde_json::Value>(raw.trim()).ok()?;
    let workspaces = parsed
        .pointer("/result/workspaces")
        .and_then(|w| w.as_array())
        .cloned()
        .unwrap_or_default();
    workspaces.into_iter().find_map(|ws| {
        (ws.get("label").and_then(|l| l.as_str()) == Some(label))
            .then(|| {
                ws.get("workspace_id")
                    .and_then(|i| i.as_str())
                    .map(str::to_string)
            })
            .flatten()
    })
}

async fn cleanup_workspace(label: &str) {
    // Find and close the test workspace, best-effort.
    if let Some(id) = workspace_id_for(label).await {
        let _ = tokio::process::Command::new("herdr")
            .args(["workspace", "close", &id])
            .output()
            .await;
    }
}

#[tokio::test]
async fn herdr_spawn_send_read_kill() {
    if !herdr_enabled() {
        eprintln!("skipping: set MEGURI_TEST_HERDR=1 with a live herdr server");
        return;
    }
    let label = format!("meguri-test-{}", std::process::id());
    let mux = HerdrMux::new(&label);
    let dir = tempfile::tempdir().unwrap();

    let pane = mux
        .spawn_pane(&PaneSpec {
            title: "fake".into(),
            cwd: dir.path().to_path_buf(),
            command: vec![
                "bash".into(),
                fake_agent_path().to_string_lossy().to_string(),
            ],
            env: vec![("FAKE_AGENT_SCRIPT".into(), "work:1,result:success".into())],
        })
        .await
        .expect("spawn pane");

    assert!(mux.pane_alive(&pane).await.unwrap());

    // Banner + prompt should appear.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let tail = mux.read_tail(&pane, 20).await.unwrap();
    assert!(
        tail.iter().any(|l| l.contains("fake-agent v0.1")),
        "banner missing in tail: {tail:?}"
    );

    // send_line drives the fake TUI.
    mux.send_line(&pane, "work 1").await.unwrap();
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let tail = mux.read_tail(&pane, 30).await.unwrap();
    assert!(
        tail.iter().any(|l| l.contains("working... step")),
        "no working output in tail: {tail:?}"
    );

    // herdr won't recognize the fake TUI as a known agent: state is Unknown.
    // (Real claude/codex panes report working/idle/blocked natively.)
    let state = mux.agent_state(&pane).await.unwrap();
    assert!(
        matches!(
            state,
            AgentState::Unknown | AgentState::Idle | AgentState::Working
        ),
        "unexpected state: {state:?}"
    );

    // The result contract file lands in the cwd.
    mux.send_line(&pane, "result success").await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(dir.path().join(".meguri/result.json").exists());

    mux.kill_pane(&pane).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!mux.pane_alive(&pane).await.unwrap());

    cleanup_workspace(&label).await;
}

/// Acceptance (issue #105): a project-scoped label (`<base>:<project>`, the
/// real per-project workspace) gets its own workspace, and the spawned tab's
/// pane belongs to it — the pane id `wN:pM` is prefixed with that workspace id.
/// Also exercises the "existing pane" path: kill by id still works.
#[tokio::test]
async fn herdr_spawn_lands_in_project_workspace() {
    if !herdr_enabled() {
        eprintln!("skipping: set MEGURI_TEST_HERDR=1 with a live herdr server");
        return;
    }
    let label = format!("meguri-test-{}:proj", std::process::id());
    let mux = HerdrMux::new(&label);
    let dir = tempfile::tempdir().unwrap();

    let pane = mux
        .spawn_pane(&PaneSpec {
            title: "fake".into(),
            cwd: dir.path().to_path_buf(),
            command: vec![
                "bash".into(),
                fake_agent_path().to_string_lossy().to_string(),
            ],
            env: vec![],
        })
        .await
        .expect("spawn pane");

    let ws_id = workspace_id_for(&label)
        .await
        .expect("project-scoped workspace was created");
    assert_eq!(
        pane.0.split(':').next(),
        Some(ws_id.as_str()),
        "pane {} should live in workspace {ws_id}",
        pane.0
    );

    // Existing-pane operations address the pane by id (no label needed).
    assert!(mux.pane_alive(&pane).await.unwrap());
    mux.kill_pane(&pane).await.unwrap();

    cleanup_workspace(&label).await;
}

/// Acceptance: a state transition must be seen in well under the old 2s poll
/// interval. The transition is driven via `pane report-agent` (the same
/// reporting API real agent integrations use) 600ms into the wait.
#[tokio::test]
async fn herdr_wait_state_detects_transition_faster_than_poll_interval() {
    if !herdr_enabled() {
        eprintln!("skipping: set MEGURI_TEST_HERDR=1 with a live herdr server");
        return;
    }
    let label = format!("meguri-test-wait-{}", std::process::id());
    let mux = HerdrMux::new(&label);
    let dir = tempfile::tempdir().unwrap();

    let pane = mux
        .spawn_pane(&PaneSpec {
            title: "wait".into(),
            cwd: dir.path().to_path_buf(),
            command: vec![
                "bash".into(),
                fake_agent_path().to_string_lossy().to_string(),
            ],
            env: vec![],
        })
        .await
        .expect("spawn pane");

    const REPORT_AFTER: Duration = Duration::from_millis(600);
    let pane_id = pane.0.clone();
    let reporter = tokio::spawn(async move {
        tokio::time::sleep(REPORT_AFTER).await;
        let out = tokio::process::Command::new("herdr")
            .args([
                "pane",
                "report-agent",
                &pane_id,
                "--source",
                "meguri-test",
                "--agent",
                "fake",
                "--state",
                "working",
                "--seq",
                "1",
            ])
            .output()
            .await
            .expect("report-agent runs");
        assert!(
            out.status.success(),
            "report-agent failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    });

    let started = Instant::now();
    let state = mux
        .wait_state(&pane, &[AgentState::Working], Duration::from_secs(10))
        .await
        .expect("wait_state");
    let elapsed = started.elapsed();
    reporter.await.unwrap();

    assert_eq!(state, AgentState::Working);
    assert!(
        elapsed >= Duration::from_millis(400),
        "returned before the transition was reported: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_millis(1800),
        "detection latency {elapsed:?} is not below the 2s poll interval \
         (transition reported at {REPORT_AFTER:?})"
    );

    // The event-fed cache now serves agent_state without a round trip.
    assert_eq!(mux.agent_state(&pane).await.unwrap(), AgentState::Working);

    mux.kill_pane(&pane).await.unwrap();
    cleanup_workspace(&label).await;
}

/// Acceptance (#96): tiling a pane into a `meguri top` dashboard tab must keep
/// its process live and driveable — herdr `pane move` relocates the pane, it
/// does not restart it. Confirms the D1 premise on real herdr.
#[tokio::test]
async fn herdr_tile_pane_preserves_live_process() {
    if !herdr_enabled() {
        eprintln!("skipping: set MEGURI_TEST_HERDR=1 with a live herdr server");
        return;
    }
    let label = format!("meguri-test-top-{}", std::process::id());
    let mux = HerdrMux::new(&label);
    let dir = tempfile::tempdir().unwrap();

    let pane = mux
        .spawn_pane(&PaneSpec {
            title: "tile".into(),
            cwd: dir.path().to_path_buf(),
            command: vec![
                "bash".into(),
                fake_agent_path().to_string_lossy().to_string(),
            ],
            env: vec![],
        })
        .await
        .expect("spawn pane");

    // Banner appears in the pane's original tab.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(mux.pane_alive(&pane).await.unwrap());

    // Move the live pane into the dashboard tab (dedicated dashboard workspace,
    // scoped to this test so it is cleaned up).
    let dash_label = format!("{label}:top");
    let dashboard = mux.ensure_dashboard(&dash_label).await.expect("dashboard");
    assert!(dashboard.fresh, "first ensure_dashboard must be fresh");
    mux.tile_pane(&pane, &dashboard.tile, Split::Down)
        .await
        .expect("tile pane");

    // The process survived the move: still alive and still driveable by id.
    assert!(
        mux.pane_alive(&pane).await.unwrap(),
        "pane died on move — pane move must preserve the process"
    );
    mux.send_line(&pane, "work 1").await.unwrap();
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let tail = mux.read_tail(&pane, 30).await.unwrap();
    assert!(
        tail.iter().any(|l| l.contains("working... step")),
        "moved pane no longer responds to input: {tail:?}"
    );

    // Idempotent dashboard: a second ensure reuses the same tile tab and does
    // not report a fresh status pane (so the render loop is not restarted).
    let again = mux.ensure_dashboard(&dash_label).await.expect("dashboard");
    assert_eq!(
        again.tile, dashboard.tile,
        "ensure_dashboard must reuse the tab"
    );
    assert!(!again.fresh, "second ensure_dashboard must not be fresh");

    mux.kill_pane(&pane).await.unwrap();
    cleanup_workspace(&dash_label).await;
    cleanup_workspace(&label).await;
}

/// Acceptance: with herdr dead (socket gone), wait_state must degrade to a
/// clean WaitTimeout after the full timeout instead of erroring immediately.
/// Needs no live herdr, so it always runs.
#[tokio::test]
async fn herdr_wait_state_survives_dead_herdr() {
    let dir = tempfile::tempdir().unwrap();
    let mux = HerdrMux::with_socket("meguri-test-dead", dir.path().join("no-such.sock"));
    let pane = PaneId("wZZ:p99".into());

    let started = Instant::now();
    let err = mux
        .wait_state(&pane, &[AgentState::Working], Duration::from_millis(1500))
        .await
        .expect_err("nothing can reach Working");
    let elapsed = started.elapsed();

    assert!(
        matches!(err, MuxError::WaitTimeout(_)),
        "expected WaitTimeout, got: {err:?}"
    );
    assert!(
        elapsed >= Duration::from_millis(1400),
        "gave up before the timeout: {elapsed:?}"
    );
}
