//! Integration tests for the herdr Multiplexer implementation.
//!
//! These need a *live* herdr server and mutate it (they create and remove a
//! test workspace), so they are gated behind MEGURI_TEST_HERDR=1.

use std::path::PathBuf;
use std::time::Duration;

use meguri::mux::{AgentState, Multiplexer, PaneSpec, herdr::HerdrMux};

fn herdr_enabled() -> bool {
    std::env::var("MEGURI_TEST_HERDR").as_deref() == Ok("1") && HerdrMux::socket_live()
}

fn fake_agent_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.sh")
}

async fn cleanup_workspace(label: &str) {
    // Find and close the test workspace, best-effort.
    let out = tokio::process::Command::new("herdr")
        .args(["workspace", "list"])
        .output()
        .await
        .ok();
    let Some(out) = out else { return };
    let raw = String::from_utf8_lossy(&out.stdout);
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return;
    };
    let workspaces = parsed
        .pointer("/result/workspaces")
        .and_then(|w| w.as_array())
        .cloned()
        .unwrap_or_default();
    for ws in workspaces {
        if ws.get("label").and_then(|l| l.as_str()) == Some(label)
            && let Some(id) = ws.get("workspace_id").and_then(|i| i.as_str())
        {
            let _ = tokio::process::Command::new("herdr")
                .args(["workspace", "close", id])
                .output()
                .await;
        }
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
