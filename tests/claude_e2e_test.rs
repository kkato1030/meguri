//! Full worker-loop e2e with a REAL `claude` agent in a REAL tmux pane
//! (forge faked, git origin local). Exercises the actual TUI: prompt-arg
//! injection, screen-heuristic state detection, permission dialogs answered
//! by a simulated human, the result-file contract, and verification.
//!
//! Costs real Claude usage and takes minutes — gated behind
//! MEGURI_TEST_CLAUDE=1. Requires `claude` and `tmux` on PATH.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::worker::{WorkerOutcome, run_worker};
use meguri::forge::LABEL_READY;
use meguri::forge::fake::FakeForge;
use meguri::gitops::run_git;
use meguri::mux::tmux::TmuxMux;
use meguri::mux::{AgentState, Multiplexer, PaneId};
use meguri::store::Store;

fn enabled() -> bool {
    if std::env::var("MEGURI_TEST_CLAUDE").as_deref() != Ok("1") {
        eprintln!("skipping: set MEGURI_TEST_CLAUDE=1 (spends real Claude usage)");
        return false;
    }
    for (cmd, arg) in [("tmux", "-V"), ("claude", "--version")] {
        if !std::process::Command::new(cmd)
            .arg(arg)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            eprintln!("skipping: {cmd} not available");
            return false;
        }
    }
    true
}

async fn init_origin_and_clone(root: &Path) -> std::path::PathBuf {
    let origin = root.join("origin.git");
    let clone = root.join("clone");
    std::fs::create_dir_all(&origin).unwrap();
    run_git(&origin, &["init", "--bare", "-b", "main"])
        .await
        .unwrap();
    run_git(
        root,
        &["clone", origin.to_str().unwrap(), clone.to_str().unwrap()],
    )
    .await
    .unwrap();

    std::fs::write(
        clone.join("greet.py"),
        "def greet(name: str) -> str:\n    return f\"Hello, {name}!\"\n",
    )
    .unwrap();
    std::fs::write(
        clone.join("test_greet.py"),
        r#"import unittest

from greet import greet


class TestGreet(unittest.TestCase):
    def test_greet(self):
        self.assertEqual(greet("world"), "Hello, world!")


if __name__ == "__main__":
    unittest.main()
"#,
    )
    .unwrap();
    std::fs::write(
        clone.join("README.md"),
        "# sandbox\n\nRun checks: `python3 -m unittest discover -q`\n",
    )
    .unwrap();

    for args in [
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "meguri-e2e"],
        vec!["add", "-A"],
        vec!["commit", "-q", "-m", "Seed sandbox project"],
        vec!["push", "-q", "-u", "origin", "main"],
    ] {
        run_git(&clone, &args).await.unwrap();
    }
    clone
}

/// A simulated human: watches the pane and answers any permission/trust
/// dialog with "1" (Yes). This is exactly what a real attached human does.
fn spawn_dialog_answerer(
    mux: Arc<TmuxMux>,
    store: Store,
    run_id: String,
) -> tokio::task::JoinHandle<u32> {
    tokio::spawn(async move {
        let mut answered = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            let Ok(Some(run)) = store.get_run(&run_id) else {
                continue;
            };
            let Some(pane_id) = run.mux_pane_id else {
                continue;
            };
            let pane = PaneId(pane_id);
            if mux.agent_state(&pane).await.ok() == Some(AgentState::Blocked) {
                eprintln!("[human-sim] answering a dialog with '1'");
                if mux.send_line(&pane, "1").await.is_ok() {
                    answered += 1;
                }
                // Give the TUI a moment so we don't double-answer.
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
        #[allow(unreachable_code)]
        answered
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn real_claude_implements_issue_in_tmux() {
    if !enabled() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let clone = init_origin_and_clone(root.path()).await;
    let session = format!("meguri-claude-{}", std::process::id());
    let mux = Arc::new(TmuxMux::new(&session));

    let forge = Arc::new(FakeForge::with_issue(
        1,
        "Add a farewell function",
        "Add `farewell(name: str) -> str` to `greet.py` returning `\"Goodbye, {name}!\"`, \
         and add a unit test for it in `test_greet.py` following the existing style.",
        &[LABEL_READY],
    ));

    let mut config = Config::default();
    config.agent.command = "claude".into();
    config.agent.args = vec!["--permission-mode".into(), "acceptEdits".into()];
    config.limits.idle_grace_secs = 120; // claude thinks quietly at times
    config.limits.result_grace_secs = 20;
    config.mux.session = session.clone();

    let deps = Deps {
        store: Store::open_in_memory().unwrap(),
        mux: mux.clone(),
        forge: forge.clone(),
        config,
        project: ProjectConfig {
            id: "sandbox".into(),
            repo_path: clone.clone(),
            repo_slug: "local/sandbox".into(),
            default_branch: "main".into(),
            check_command: Some("python3 -m unittest discover -q".into()),
            worktree_root: Some(root.path().join("worktrees")),
        },
    };

    let run = deps
        .store
        .create_run("sandbox", 1, "Add a farewell function")
        .unwrap();
    let human = spawn_dialog_answerer(mux.clone(), deps.store.clone(), run.id.clone());

    let outcome = tokio::time::timeout(Duration::from_secs(600), run_worker(&deps, &run.id))
        .await
        .expect("real-claude e2e timed out after 10min")
        .unwrap();
    human.abort();

    // PR "created" on the fake forge with verified commits behind it.
    let WorkerOutcome::Succeeded { .. } = outcome else {
        panic!("expected success, got {outcome:?}");
    };
    let prs = forge.prs();
    assert_eq!(prs.len(), 1);

    // The pushed branch on the local origin really contains the feature.
    let branch = &prs[0].head;
    let show = run_git(&clone, &["show", &format!("origin/{branch}:greet.py")])
        .await
        .unwrap();
    assert!(show.contains("farewell"), "greet.py on origin:\n{show}");
    let tests = run_git(&clone, &["show", &format!("origin/{branch}:test_greet.py")])
        .await
        .unwrap();
    assert!(
        tests.contains("farewell"),
        "test_greet.py on origin:\n{tests}"
    );

    let events = deps.store.events_for_run(&run.id, 300).unwrap();
    eprintln!("--- event trail ---");
    for e in &events {
        eprintln!("{} {}", e.ts, e.kind);
    }

    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", &session])
        .output()
        .await;
}
