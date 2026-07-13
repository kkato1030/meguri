//! Recovery via the agent's native session (issue #92): every completed
//! turn saves the lane's session id on the pane row (file scan of the
//! worktree's transcripts first, agent self-report and mux as fallbacks), a
//! dead pane is respawned with `--resume <id>` read from
//! `panes.agent_session_id`, and a rejected resume clears the id and falls
//! back to the plain full-prompt spawn.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::worker::{WorkerOutcome, run_worker};
use meguri::forge::LABEL_READY;
use meguri::forge::fake::FakeForge;
use meguri::gitops::run_git;
use meguri::mux::PaneId;
use meguri::mux::fake::FakeMux;
use meguri::store::{ROLE_AUTHOR, Store};

async fn init_origin_and_clone(root: &Path) -> (PathBuf, PathBuf) {
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
    for args in [
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "meguri-test"],
        vec!["commit", "--allow-empty", "-m", "init"],
        vec!["push", "-u", "origin", "main"],
    ] {
        run_git(&clone, &args).await.unwrap();
    }
    (origin, clone)
}

struct TestEnv {
    deps: Deps,
    mux: Arc<FakeMux>,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    worktree_root: PathBuf,
    session_root: PathBuf,
}

async fn setup() -> TestEnv {
    let root = tempfile::tempdir().unwrap();
    let (_origin, clone) = init_origin_and_clone(root.path()).await;
    let worktree_root = root.path().join("worktrees");
    let session_root = root.path().join("claude");

    let forge = Arc::new(FakeForge::with_issue(
        7,
        "Add greeting file",
        "Create `greeting.txt` containing hello.",
        &[LABEL_READY],
    ));
    let mux = Arc::new(FakeMux::new(false));

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600; // scripted agent: no nudging wanted
    config.limits.result_grace_secs = 1; // FakeMux always reads Working; don't linger
    config.review.enabled = false; // resume/session behavior, not self-review, is under test
    config.agent.session_dir = Some(session_root.clone());
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: Some(worktree_root.clone()),
        pr: None,
        clean: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
    };

    let deps = Deps::with_label_source(
        Store::open_in_memory().unwrap(),
        mux.clone(),
        forge,
        config,
        project,
    );
    TestEnv {
        deps,
        mux,
        root,
        worktree_root,
        session_root,
    }
}

/// The lane's resumable context, as an earlier turn (or the reaper) left it:
/// a reclaimed pane row carrying the saved agent session id.
fn seed_pane_session(env: &TestEnv, session: &str) {
    let store = &env.deps.store;
    store
        .upsert_pane("proj", 7, ROLE_AUTHOR, "fake", "meguri", "%gone", "/wt/old")
        .unwrap();
    store
        .save_pane_session("proj", 7, ROLE_AUTHOR, Some(session))
        .unwrap();
    store.mark_pane_reclaimed("proj", 7, ROLE_AUTHOR).unwrap();
}

fn pane_session(env: &TestEnv) -> Option<String> {
    env.deps
        .store
        .get_pane("proj", 7, ROLE_AUTHOR)
        .unwrap()
        .and_then(|p| p.agent_session_id)
}

/// Claude Code's directory name for a project cwd (mirrors agent_session).
fn munged(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn write_session_transcript(session_root: &Path, worktree: &Path, session_id: &str) {
    let dir = session_root.join("projects").join(munged(worktree));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{session_id}.jsonl")), "{}\n").unwrap();
}

fn find_worktree(worktree_root: &Path) -> Option<PathBuf> {
    let proj = worktree_root.join("proj");
    let entries = std::fs::read_dir(proj).ok()?;
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
}

fn latest_prompt_turn(worktree: &Path) -> Option<String> {
    let meguri = worktree.join(".meguri");
    let mut ids: Vec<(std::time::SystemTime, String)> = std::fs::read_dir(&meguri)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let id = name
                .strip_prefix("prompt-")?
                .strip_suffix(".md")?
                .to_string();
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, id))
        })
        .collect();
    ids.sort();
    ids.pop().map(|(_, id)| id)
}

fn write_result_with_session(worktree: &Path, turn_id: &str, status: &str, session: Option<&str>) {
    let mut result = serde_json::json!({
        "turn_id": turn_id, "status": status, "summary": "scripted",
    });
    if let Some(session) = session {
        result["agent_session_id"] = serde_json::Value::String(session.into());
    }
    std::fs::write(worktree.join(".meguri/result.json"), result.to_string()).unwrap();
}

/// Scripted pane-side agent: `action` runs exactly once per new prompt turn
/// (deduplicated by turn id, so slow actions aren't re-fired by the poll).
fn spawn_scripted_agent<F>(worktree_root: PathBuf, mut action: F) -> tokio::task::JoinHandle<u32>
where
    F: FnMut(u32, &Path, &str) + Send + 'static,
{
    tokio::spawn(async move {
        let mut seen: HashSet<String> = HashSet::new();
        for _ in 0..600 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let Some(wt) = find_worktree(&worktree_root) else {
                continue;
            };
            if let Some(turn_id) = latest_prompt_turn(&wt)
                && seen.insert(turn_id.clone())
            {
                action(seen.len() as u32, &wt, &turn_id);
            }
        }
        seen.len() as u32
    })
}

async fn commit_greeting(wt: &Path) {
    std::fs::write(wt.join("greeting.txt"), "hello\n").unwrap();
    run_git(wt, &["add", "greeting.txt"]).await.unwrap();
    run_git(
        wt,
        &[
            "-c",
            "user.email=a@example.com",
            "-c",
            "user.name=agent",
            "commit",
            "-m",
            "Add greeting file",
        ],
    )
    .await
    .unwrap();
}

fn event_kinds(deps: &Deps, run_id: &str) -> Vec<String> {
    deps.store
        .events_for_run(run_id, 200)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn session_id_from_result_file_is_recorded_on_the_run() {
    let env = setup().await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_greeting(&wt).await;
            write_result_with_session(&wt, &turn_id, "success", Some("sess-abc"));
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    // Written to the lane's pane row (the resume source of truth) and to the
    // run (observability).
    assert_eq!(pane_session(&env).as_deref(), Some("sess-abc"));
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.agent_session_id.as_deref(), Some("sess-abc"));
}

#[tokio::test(flavor = "multi_thread")]
async fn session_id_from_the_file_scan_wins_over_the_result_file() {
    let env = setup().await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // The primary source: a transcript in the worktree's session directory.
    // The agent's self-report says something else — the scan wins.
    let session_root = env.session_root.clone();
    let agent = spawn_scripted_agent(env.worktree_root.clone(), move |_, wt, turn_id| {
        write_session_transcript(&session_root, wt, "sess-scanned");
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_greeting(&wt).await;
            write_result_with_session(&wt, &turn_id, "success", Some("sess-selfreport"));
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    assert_eq!(pane_session(&env).as_deref(), Some("sess-scanned"));
}

#[tokio::test(flavor = "multi_thread")]
async fn session_id_from_the_mux_is_recorded_when_result_omits_it() {
    let env = setup().await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // The herdr path: the agent never self-reports, but the mux carries the
    // session id (reported via `pane report-agent-session`).
    let store = env.deps.store.clone();
    let mux = env.mux.clone();
    let run_id = run.id.clone();
    let agent = spawn_scripted_agent(env.worktree_root.clone(), move |_, wt, turn_id| {
        // The pane handle is persisted concurrently with the prompt file;
        // poll for it rather than racing the spawn.
        let store = store.clone();
        let mux = mux.clone();
        let run_id = run_id.clone();
        tokio::spawn(async move {
            for _ in 0..100 {
                if let Ok(Some(r)) = store.get_run(&run_id)
                    && let Some(pane) = r.mux_pane_id
                {
                    mux.set_agent_session(&PaneId(pane), Some("sess-mux".into()));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_greeting(&wt).await;
            write_result_with_session(&wt, &turn_id, "success", None);
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    assert_eq!(pane_session(&env).as_deref(), Some("sess-mux"));
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.agent_session_id.as_deref(), Some("sess-mux"));
}

/// Manual-routing config: the worker role maps to a profile whose
/// `resume_args` differ from the default, so a resume proves the run's pinned
/// profile — not `[agent]` — drives the respawn.
fn routing_config() -> Config {
    let toml = r#"
[agents.profiles.p-worker]
command = "worker-cli"
args = ["--go"]
resume_args = ["resume", "--continue-session"]

[routing]
mode = "manual"

[routing.roles]
worker = "p-worker"
"#;
    let mut config: Config = toml::from_str(toml).unwrap();
    config.limits.idle_grace_secs = 3600;
    config.limits.result_grace_secs = 1;
    config.review.enabled = false; // resume behavior, not self-review, is under test
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_uses_the_pinned_profile_resume_args() {
    let mut env = setup().await;
    env.deps.config = routing_config();
    let run = env
        .deps
        .store
        .create_run_for_loop("proj", "worker", 7, "Add greeting file")
        .unwrap();
    // A pane died mid-run: the profile is already pinned on the run, and the
    // lane's native session is on record on the pane row (issue lifetime).
    env.deps
        .store
        .update_run_agent_profile(&run.id, "p-worker")
        .unwrap();
    seed_pane_session(&env, "sess-123");

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_greeting(&wt).await;
            write_result_with_session(&wt, &turn_id, "success", Some("sess-123"));
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let cmd = &env.mux.spawned_commands()[0];
    // The profile's command and resume_args, not the default `--resume`.
    assert_eq!(cmd[0], "worker-cli", "profile command: {cmd:?}");
    let resume_at = cmd
        .iter()
        .position(|a| a == "resume")
        .unwrap_or_else(|| panic!("profile resume_args missing: {cmd:?}"));
    assert_eq!(cmd[resume_at + 1], "--continue-session", "{cmd:?}");
    assert_eq!(cmd[resume_at + 2], "sess-123", "{cmd:?}");
    assert!(
        !cmd.iter().any(|a| a == "--resume"),
        "default resume_args must not leak in: {cmd:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_respawns_with_resume_when_session_is_on_record() {
    let env = setup().await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    // The state an interrupted run is redispatched in: the pane is gone but
    // an earlier turn saved the lane's native session on the pane row.
    seed_pane_session(&env, "sess-123");

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_greeting(&wt).await;
            write_result_with_session(&wt, &turn_id, "success", Some("sess-123"));
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let commands = env.mux.spawned_commands();
    assert_eq!(commands.len(), 1, "one resume spawn only: {commands:?}");
    let cmd = &commands[0];
    let resume_at = cmd
        .iter()
        .position(|a| a == "--resume")
        .unwrap_or_else(|| panic!("--resume missing in spawn: {cmd:?}"));
    assert_eq!(cmd[resume_at + 1], "sess-123");
    assert!(
        cmd.last().unwrap().contains(".meguri/prompt-"),
        "trigger must follow the session id: {cmd:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rejected_resume_falls_back_to_full_prompt_spawn() {
    let env = setup().await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    seed_pane_session(&env, "sess-expired");
    // `claude --resume <unknown-id>` prints an error and exits immediately;
    // emulate that with a dead-on-arrival pane for resume spawns.
    env.mux.fail_spawns_matching("--resume");

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_greeting(&wt).await;
            write_result_with_session(&wt, &turn_id, "success", None);
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let commands = env.mux.spawned_commands();
    assert_eq!(
        commands.len(),
        2,
        "resume attempt then fallback: {commands:?}"
    );
    assert!(commands[0].iter().any(|a| a == "--resume"));
    assert!(commands[0].iter().any(|a| a == "sess-expired"));
    assert!(
        !commands[1].iter().any(|a| a == "--resume"),
        "fallback must be a plain spawn: {:?}",
        commands[1]
    );
    assert!(
        commands[1].last().unwrap().contains(".meguri/prompt-"),
        "fallback still carries the full-prompt trigger: {:?}",
        commands[1]
    );

    // The rejected id is forgotten on the pane row so the next recovery
    // doesn't retry it (the fresh spawn's completed turn saved nothing new:
    // no transcript, no self-report, no mux report).
    assert_eq!(pane_session(&env), None);
    assert!(
        event_kinds(&env.deps, &run.id).contains(&"pane.resume_failed".to_string()),
        "events: {:?}",
        event_kinds(&env.deps, &run.id)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn resumed_pane_dying_without_result_forgets_the_session() {
    let env = setup().await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    seed_pane_session(&env, "sess-stale");

    // The resumed agent boots fine but crashes mid-turn (after the resume
    // probe window) without ever writing a result.
    let store = env.deps.store.clone();
    let mux = env.mux.clone();
    let run_id = run.id.clone();
    let agent = spawn_scripted_agent(env.worktree_root.clone(), move |_, _, _| {
        let store = store.clone();
        let mux = mux.clone();
        let run_id = run_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(8)).await;
            if let Ok(Some(r)) = store.get_run(&run_id)
                && let Some(pane) = r.mux_pane_id
            {
                mux.kill(&PaneId(pane));
            }
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(
        matches!(outcome, WorkerOutcome::Interrupted(_)),
        "expected interruption, got {outcome:?}"
    );
    // The resume was attempted...
    assert!(
        env.mux.spawned_commands()[0]
            .iter()
            .any(|a| a == "--resume")
    );
    // ...but the session died with the pane: forget it so the next recovery
    // re-injects the full prompt instead of resume-looping.
    assert_eq!(pane_session(&env), None);
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.agent_session_id, None);
    assert!(
        event_kinds(&env.deps, &run.id).contains(&"agent_session.cleared".to_string()),
        "events: {:?}",
        event_kinds(&env.deps, &run.id)
    );
}
