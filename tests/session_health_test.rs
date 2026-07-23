//! セッション健全性(issue #245)の統合テスト: resume の条件を「pane が開くか」
//! から「その会話がまだ生きているか」へ変える3つの栓を、FakeMux + 実 git
//! worktree + スクリプト化エージェントで通しで検証する。
//!
//! - 栓1: resume 前の transcript サイズゲート(超過は fresh spawn へ、
//!   特定不能は fail-open、`0` で無効、root は lane の pinned profile から)
//! - 栓2: agent 不在 pane を adopt しない(素のシェルに trigger を打たない)
//! - 栓3: agent_quiet の strike ladder(1=猶予 / 2=session 破棄 /
//!   3=needs-human + サニタイズ済み pane tail)

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::worker::{WorkerOutcome, run_worker};
use meguri::forge::fake::FakeForge;
use meguri::forge::{LABEL_NEEDS_HUMAN, LABEL_READY};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::mux::{AgentState, Multiplexer, PaneId};
use meguri::store::{LANE_AUTHOR, Store};

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
    forge: Arc<FakeForge>,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    worktree_root: PathBuf,
    session_root: PathBuf,
}

async fn setup_with(tune: impl FnOnce(&mut Config)) -> TestEnv {
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
    config.review.enabled = false; // session health, not self-review, is under test
    config.agent.session_dir = Some(session_root.clone());
    tune(&mut config);
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: Some(clone),
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: Some(worktree_root.clone()),
        pr: None,
        clean: None,
        triage: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
        autonomy: None,
        cadence: Vec::new(),
        prompts: Default::default(),
        notify: None,
    };

    let deps = Deps::with_label_source(
        Store::open_in_memory().unwrap(),
        mux.clone(),
        forge.clone(),
        config,
        project,
    );
    TestEnv {
        deps,
        mux,
        forge,
        root,
        worktree_root,
        session_root,
    }
}

/// The lane's resumable context, as an earlier turn (or the reaper) left it:
/// a reclaimed pane row carrying the saved agent session id. The recorded
/// worktree is where the resume gate looks the transcript up.
fn seed_pane_session(env: &TestEnv, session: &str, worktree: &str) {
    let store = &env.deps.store;
    store
        .upsert_pane("proj", 7, LANE_AUTHOR, "fake", "meguri", "%gone", worktree)
        .unwrap();
    store
        .save_pane_session("proj", 7, LANE_AUTHOR, Some(session))
        .unwrap();
    store.mark_pane_reclaimed("proj", 7, LANE_AUTHOR).unwrap();
}

/// Claude Code's directory name for a project cwd (mirrors agent_session).
fn munged(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn write_transcript(session_root: &Path, worktree: &str, session_id: &str, bytes: usize) {
    let dir = session_root.join("projects").join(munged(worktree));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{session_id}.jsonl")), "x".repeat(bytes)).unwrap();
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

/// Scripted pane-side agent: `action` runs exactly once per new prompt turn.
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

fn write_result(worktree: &Path, turn_id: &str, status: &str) {
    std::fs::write(
        worktree.join(".meguri/result.json"),
        serde_json::json!({ "turn_id": turn_id, "status": status, "summary": "scripted" })
            .to_string(),
    )
    .unwrap();
}

/// Completes the first turn like a healthy agent (commit + success result).
fn completing_agent(worktree_root: PathBuf) -> tokio::task::JoinHandle<u32> {
    spawn_scripted_agent(worktree_root, |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_greeting(&wt).await;
            write_result(&wt, &turn_id, "success");
        });
    })
}

/// Keeps the run's live pane reading as Idle with a fixed tail, so an agent
/// that never writes a result goes quiet on the stagnation clock.
fn idle_pane_driver(
    env: &TestEnv,
    run_id: String,
    tail: Vec<String>,
) -> tokio::task::JoinHandle<()> {
    let mux = env.mux.clone();
    let store = env.deps.store.clone();
    tokio::spawn(async move {
        loop {
            if let Ok(Some(r)) = store.get_run(&run_id)
                && let Some(p) = r.mux_pane_id
            {
                let pane = PaneId(p);
                mux.set_state(&pane, AgentState::Idle);
                mux.set_tail(&pane, tail.clone());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
}

fn event_kinds(deps: &Deps, run_id: &str) -> Vec<String> {
    deps.store
        .events_for_run(run_id, 200)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect()
}

/// 栓1(受け入れ1の前段): 閾値超の transcript を持つ session は resume されず、
/// session が消えて fresh spawn(`--resume` 無し)に落ちる。
#[tokio::test(flavor = "multi_thread")]
async fn oversized_transcript_clears_session_and_spawns_fresh() {
    let env = setup_with(|c| c.agent.resume_transcript_limit_bytes = 1024).await;
    seed_pane_session(&env, "sess-big", "/wt/old");
    write_transcript(&env.session_root, "/wt/old", "sess-big", 4096);
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    let agent = completing_agent(env.worktree_root.clone());

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let spawns = env.mux.spawned_commands();
    assert!(
        !spawns[0].iter().any(|a| a == "--resume"),
        "oversized session must not be resumed: {spawns:?}"
    );
    let kinds = event_kinds(&env.deps, &run.id);
    assert!(
        kinds.contains(&"agent_session.cleared".to_string()),
        "gate must clear the session: {kinds:?}"
    );
    assert!(!kinds.contains(&"pane.resume_failed".to_string()));
}

/// 栓1 fail-open: transcript を特定できない session は従来どおり resume を
/// 試み、`pane.resume_gate_skipped` で可観測になる。
#[tokio::test(flavor = "multi_thread")]
async fn missing_transcript_fails_open_and_resumes() {
    let env = setup_with(|_| {}).await;
    seed_pane_session(&env, "sess-ghost", "/wt/old");
    // No transcript anywhere: the gate cannot judge, so it must not block.
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    let agent = completing_agent(env.worktree_root.clone());

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let spawns = env.mux.spawned_commands();
    assert!(
        spawns[0].iter().any(|a| a == "--resume"),
        "an unjudgeable transcript must fail open into a resume: {spawns:?}"
    );
    assert!(spawns[0].iter().any(|a| a == "sess-ghost"));
    let kinds = event_kinds(&env.deps, &run.id);
    assert!(kinds.contains(&"pane.resume_gate_skipped".to_string()));
}

/// 栓1 kill switch(受け入れ5): `resume_transcript_limit_bytes = 0` はゲートを
/// 無効化し、超過 transcript でも既存挙動(resume)のまま。
#[tokio::test(flavor = "multi_thread")]
async fn zero_limit_disables_the_gate() {
    let env = setup_with(|c| c.agent.resume_transcript_limit_bytes = 0).await;
    seed_pane_session(&env, "sess-big", "/wt/old");
    write_transcript(&env.session_root, "/wt/old", "sess-big", 4096);
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    let agent = completing_agent(env.worktree_root.clone());

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let spawns = env.mux.spawned_commands();
    assert!(
        spawns[0].iter().any(|a| a == "--resume"),
        "a zero limit must disable the gate: {spawns:?}"
    );
    let kinds = event_kinds(&env.deps, &run.id);
    assert!(!kinds.contains(&"agent_session.cleared".to_string()));
}

/// 栓1 の root 解決(受け入れ5, spec f4): named profile が独自の `session_dir`
/// を持つとき、ゲートは lane の pinned profile の root で transcript を測る。
/// default root には何も置かないので、default を見てしまう実装なら fail-open
/// で resume してしまう — no-resume がプロファイル root 使用の証明になる。
#[tokio::test(flavor = "multi_thread")]
async fn named_profile_session_dir_drives_the_gate() {
    let profile_root = tempfile::tempdir().unwrap();
    let profile_root_path = profile_root.path().to_path_buf();
    let env = setup_with(move |c| {
        let toml = r#"
[agents.profiles.p-worker]
command = "worker-cli"
args = ["--go"]
resume_args = ["resume", "--continue-session"]
resume_transcript_limit_bytes = 1024

[routing]
mode = "manual"

[routing.roles]
worker = "p-worker"
"#;
        let overlay: Config = toml::from_str(toml).unwrap();
        c.agents = overlay.agents;
        c.routing = overlay.routing;
        c.agents
            .as_mut()
            .unwrap()
            .profiles
            .get_mut("p-worker")
            .unwrap()
            .session_dir = Some(profile_root_path.clone());
    })
    .await;
    seed_pane_session(&env, "sess-big", "/wt/old");
    write_transcript(profile_root.path(), "/wt/old", "sess-big", 4096);
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    let agent = completing_agent(env.worktree_root.clone());

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let spawns = env.mux.spawned_commands();
    assert!(
        !spawns[0].iter().any(|a| a == "resume"),
        "the profile-root transcript must gate the resume: {spawns:?}"
    );
    let kinds = event_kinds(&env.deps, &run.id);
    assert!(kinds.contains(&"agent_session.cleared".to_string()));
}

/// 栓3(受け入れ1・3): quiet 1回目は猶予(再 adopt)、2回目で session と pane を
/// 捨てて fresh spawn、3回目で needs-human — escalation コメントには
/// サニタイズ済み tail が載り、credential はマスクされる。
#[tokio::test(flavor = "multi_thread")]
async fn quiet_strikes_rotate_the_session_then_escalate() {
    // idle_grace 1s (not 0): a zero grace would trip the quiet branch on the
    // very first poll, before the driver has marked the pane Idle with a tail.
    let env = setup_with(|c| {
        c.limits.idle_grace_secs = 1;
        c.limits.nudge_limit = 0;
    })
    .await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    let tail = vec![
        "API Error: 400 input exceeds the context window".to_string(),
        "export GITHUB_TOKEN=ghp_0123456789abcdefghijklmnop".to_string(),
    ];
    let driver = idle_pane_driver(&env, run.id.clone(), tail);

    // Strike 1: interrupted, session/pane kept (a one-off hiccup gets grace).
    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("strike 1 timed out")
        .unwrap();
    assert!(matches!(outcome, WorkerOutcome::Interrupted(_)));
    let pane = env
        .deps
        .store
        .get_pane("proj", 7, LANE_AUTHOR)
        .unwrap()
        .unwrap();
    assert_eq!(pane.quiet_strikes, 1);
    assert!(pane.mux_pane_id.is_some(), "strike 1 keeps the pane");

    // Strike 2: the session is rotated — pane killed + reclaimed, saved id
    // cleared — so the next dispatch has nothing to adopt or resume.
    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("strike 2 timed out")
        .unwrap();
    assert!(matches!(outcome, WorkerOutcome::Interrupted(_)));
    let pane = env
        .deps
        .store
        .get_pane("proj", 7, LANE_AUTHOR)
        .unwrap()
        .unwrap();
    assert_eq!(pane.quiet_strikes, 2);
    assert_eq!(pane.mux_pane_id, None, "strike 2 reclaims the pane");
    assert_eq!(pane.agent_session_id, None, "strike 2 clears the session");
    let kinds = event_kinds(&env.deps, &run.id);
    assert!(
        kinds.contains(&"agent_session.cleared".to_string()),
        "quiet_loop rotation must be observable: {kinds:?}"
    );

    // Strike 3: even a fresh session went quiet — a human takes over.
    let err = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("strike 3 timed out")
        .expect_err("strike 3 must escalate");
    driver.abort();
    assert!(format!("{err:#}").contains("quiet"), "got: {err:#}");

    // The third spawn was fresh: two spawns total (strike 1 adopted the
    // live pane), neither resuming.
    let spawns = env.mux.spawned_commands();
    assert_eq!(
        spawns.len(),
        2,
        "adopt on strike 1, fresh on strike 3: {spawns:?}"
    );
    assert!(spawns.iter().all(|s| !s.iter().any(|a| a == "--resume")));

    // The escalation reaches the issue with a sanitized tail: the API error
    // survives for diagnosis, the credential does not.
    let labels = env.forge.labels_of(7);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "{labels:?}"
    );
    let comments = env.forge.comments_of(7);
    let escalation = comments.last().expect("needs-human comment");
    assert!(escalation.contains("API Error: 400"), "{escalation}");
    assert!(!escalation.contains("ghp_0123456789"), "{escalation}");
    assert!(escalation.contains("‹redacted›"), "{escalation}");
}

/// 栓3 の reset(受け入れ4): 完了した turn は strike を 0 に戻す。
#[tokio::test(flavor = "multi_thread")]
async fn completed_turn_resets_quiet_strikes() {
    // idle_grace 1s (not 0): a zero grace would trip the quiet branch on the
    // very first poll, before the driver has marked the pane Idle with a tail.
    let env = setup_with(|c| {
        c.limits.idle_grace_secs = 1;
        c.limits.nudge_limit = 0;
    })
    .await;
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();

    // Turn 1 goes quiet (strike 1)…
    let driver = idle_pane_driver(&env, run.id.clone(), vec!["thinking…".into()]);
    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("strike 1 timed out")
        .unwrap();
    driver.abort();
    assert!(matches!(outcome, WorkerOutcome::Interrupted(_)));
    let pane = env
        .deps
        .store
        .get_pane("proj", 7, LANE_AUTHOR)
        .unwrap()
        .unwrap();
    assert_eq!(pane.quiet_strikes, 1);

    // …turn 2 completes: the ladder resets, the next quiet starts over at 1.
    let agent = completing_agent(env.worktree_root.clone());
    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("completion timed out")
        .unwrap();
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    let pane = env
        .deps
        .store
        .get_pane("proj", 7, LANE_AUTHOR)
        .unwrap()
        .unwrap();
    assert_eq!(pane.quiet_strikes, 0, "a completed turn resets the ladder");
}

/// 栓2 の adopt ゲート(受け入れ2・4, spec f2): agent が居ない live pane は
/// adopt されず(trigger が打ち込まれず)、release されて fresh spawn に落ちる。
#[tokio::test(flavor = "multi_thread")]
async fn bare_shell_pane_is_not_adopted() {
    let env = setup_with(|_| {}).await;
    // A live pane from an earlier turn whose agent has since exited to a
    // bare shell: pane_alive still true, agent_present definitively false.
    let stale = env.mux.register_live_pane("fake:stale");
    env.mux.set_agent_present(&stale, Some(false));
    env.deps
        .store
        .upsert_pane(
            "proj",
            7,
            LANE_AUTHOR,
            "tmux",
            "meguri",
            "fake:stale",
            "/wt/old",
        )
        .unwrap();
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    let agent = completing_agent(env.worktree_root.clone());

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&env.deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    assert!(
        env.mux.sent_lines(&stale).is_empty(),
        "no trigger may be typed into a bare shell"
    );
    assert!(
        !env.mux.pane_alive(&stale).await.unwrap(),
        "the stale pane must be killed, not adopted"
    );
    assert_eq!(
        env.mux.spawned_commands().len(),
        1,
        "the turn runs in a fresh spawn instead"
    );
    let kinds = event_kinds(&env.deps, &run.id);
    assert!(
        kinds.contains(&"pane.agent_absent".to_string()),
        "the refusal must be observable: {kinds:?}"
    );
}
