//! Integration tests for the tmux Multiplexer implementation, using the
//! fake interactive agent. Skipped when tmux is not installed.

use std::path::PathBuf;
use std::time::Duration;

use meguri::mux::{AgentState, Multiplexer, PaneId, PaneSpec, tmux::TmuxMux};

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

fn test_session() -> String {
    format!("meguri-test-{}", std::process::id())
}

async fn cleanup(session: &str) {
    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", session])
        .output()
        .await;
}

#[tokio::test]
async fn tmux_spawn_send_read_state_kill() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let session = test_session();
    let mux = TmuxMux::new(&session);
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
    tokio::time::sleep(Duration::from_millis(800)).await;
    let tail = mux.read_tail(&pane, 20).await.unwrap();
    assert!(
        tail.iter().any(|l| l.contains("fake-agent v0.1")),
        "banner missing in tail: {tail:?}"
    );

    // While printing activity the heuristic must report Working.
    mux.send_line(&pane, "work 3").await.unwrap();
    tokio::time::sleep(Duration::from_millis(900)).await;
    let mut saw_working = false;
    for _ in 0..6 {
        if mux.agent_state(&pane).await.unwrap() == AgentState::Working {
            saw_working = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(saw_working, "never observed Working while agent printed");

    // After output stops the screen settles into Idle.
    let state = mux
        .wait_state(&pane, &[AgentState::Idle], Duration::from_secs(20))
        .await
        .expect("settle to Idle");
    assert_eq!(state, AgentState::Idle);

    // A permission-question screen must be detected as Blocked.
    mux.send_line(&pane, "block").await.unwrap();
    let state = mux
        .wait_state(&pane, &[AgentState::Blocked], Duration::from_secs(20))
        .await
        .expect("detect Blocked");
    assert_eq!(state, AgentState::Blocked);

    // Answering (like a human would) unblocks back to Idle.
    mux.send_line(&pane, "1").await.unwrap();
    let state = mux
        .wait_state(&pane, &[AgentState::Idle], Duration::from_secs(20))
        .await
        .expect("unblock to Idle");
    assert_eq!(state, AgentState::Idle);

    // The result command writes the contract file into the cwd.
    mux.send_line(&pane, "result success").await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    let result_path = dir.path().join(".meguri/result.json");
    assert!(result_path.exists(), "result.json not written");

    mux.kill_pane(&pane).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!mux.pane_alive(&pane).await.unwrap());

    cleanup(&session).await;
}

/// Regression for issue #105 (finding 1): with per-project sessions, the tmux
/// attach hint must resolve the pane's *own* session (`#{session_name}`), never
/// hard-code `self.session`. A mux built on the base label attaching a pane that
/// lives in a project session (or one predating the split) would otherwise
/// attach to the wrong session and not show the pane. No tmux needed — pure
/// string formatting.
#[test]
fn attach_command_resolves_session_from_pane() {
    let mux = TmuxMux::new("meguri-base");
    let cmd = mux.attach_command(&PaneId("%7".into()));
    assert!(
        cmd.contains("#{session_name}"),
        "attach must resolve the session from the pane: {cmd}"
    );
    assert!(
        !cmd.contains("attach -t meguri-base"),
        "attach must not hard-code the mux label: {cmd}"
    );
    assert!(cmd.contains("%7"), "attach must target the pane id: {cmd}");
}

#[tokio::test]
async fn tmux_pane_survives_agent_exit() {
    if !tmux_available() {
        eprintln!("skipping: tmux not available");
        return;
    }
    let session = test_session() + "-exit";
    let mux = TmuxMux::new(&session);
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
        .unwrap();

    tokio::time::sleep(Duration::from_millis(500)).await;
    mux.send_line(&pane, "exit").await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // remain-on-exit keeps the pane object (dead) so the screen is inspectable;
    // pane_alive must report false either way.
    assert!(!mux.pane_alive(&pane).await.unwrap());
    let tail = mux.read_tail(&pane, 10).await.unwrap_or_default();
    assert!(
        tail.iter().any(|l| l.contains("bye")) || tail.is_empty(),
        "unexpected tail: {tail:?}"
    );

    cleanup(&session).await;
}
