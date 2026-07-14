//! End-to-end role-based routing (issue #64, re-scoped to the 6-role
//! `[routing.roles]` design in issue #167): each loop kind's routing role
//! (`routing::routing_role_for_loop`) resolves to its role's launch profile,
//! and the pane is spawned from that profile's command/args. Driven with a
//! manual-mode config so resolution is deterministic and never depends on
//! which agent CLIs the test host has.
//!
//! The flow itself is the worker flow for every case — only `runs.loop_kind`
//! differs, which is exactly the axis routing keys on. That isolates the
//! resolution/spawn behavior without standing up all six loops' full setups.
//! Several loop kinds intentionally share a role (`ci-fixer` /
//! `conflict-resolver` share `fixer`'s profile with `fixer` itself;
//! `spec-worker` shares `worker`'s) — `ci-fixer` covers the issue #167
//! registration bug where it fell through to `default` instead of riding its
//! family's chain.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::worker::{WorkerOutcome, run_worker};
use meguri::forge::LABEL_READY;
use meguri::forge::fake::FakeForge;
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::Store;

async fn init_origin_and_clone(root: &Path) -> PathBuf {
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
    clone
}

/// A manual-routing config wiring every loop kind to a distinctively-named
/// profile. Manual + explicit means resolution returns the mapped name
/// verbatim with no CLI detection, so the fake command names need not exist.
fn routing_config() -> Config {
    let toml = r#"
[agents.profiles.p-planner]
command = "planner-cli"
args = ["--role", "planner"]

[agents.profiles.p-reviewer]
command = "reviewer-cli"
args = ["--role", "reviewer"]

[agents.profiles.p-worker]
command = "worker-cli"
args = ["--role", "worker"]

[agents.profiles.p-fixer]
command = "fixer-cli"
args = ["--role", "fixer"]

[routing]
mode = "manual"

[routing.roles]
planner = "p-planner"
pr-reviewer = "p-reviewer"
worker = "p-worker"
fixer = "p-fixer"
"#;
    let mut config: Config = toml::from_str(toml).unwrap();
    config.limits.idle_grace_secs = 3600; // scripted agent: no nudging wanted
    config.limits.result_grace_secs = 1; // FakeMux always reads Working; don't linger
    // This suite drives every kind through run_worker only to observe profile
    // resolution; the worker's self-review phase is not under test here.
    config.review.enabled = false;
    config
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

fn find_worktree(worktree_root: &Path) -> Option<PathBuf> {
    let proj = worktree_root.join("proj");
    std::fs::read_dir(proj)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
}

fn latest_prompt_turn(worktree: &Path) -> Option<String> {
    let meguri = worktree.join(".meguri");
    let current_result: Option<String> = std::fs::read_to_string(meguri.join("result.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .and_then(|v| {
            v.get("turn_id")
                .and_then(|t| t.as_str())
                .map(str::to_string)
        });
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
    let latest = ids.last()?.1.clone();
    if Some(&latest) == current_result.as_ref() {
        None
    } else {
        Some(latest)
    }
}

fn spawn_scripted_agent(worktree_root: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for _ in 0..600 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let Some(wt) = find_worktree(&worktree_root) else {
                continue;
            };
            if let Some(turn_id) = latest_prompt_turn(&wt)
                && seen.insert(turn_id.clone())
            {
                let wt = wt.clone();
                tokio::spawn(async move {
                    commit_greeting(&wt).await;
                    let result = serde_json::json!({
                        "turn_id": turn_id, "status": "success", "summary": "scripted",
                    });
                    std::fs::write(wt.join(".meguri/result.json"), result.to_string()).unwrap();
                });
            }
        }
    })
}

/// Drive one run of the given loop kind to a PR and return the pane commands
/// FakeMux recorded plus the profile pinned on the run.
async fn drive_loop_kind(loop_kind: &str) -> (Vec<Vec<String>>, Option<String>) {
    let root = tempfile::tempdir().unwrap();
    let clone = init_origin_and_clone(root.path()).await;
    let worktree_root = root.path().join("worktrees");

    let forge = Arc::new(FakeForge::with_issue(
        7,
        "Add greeting file",
        "Create `greeting.txt` containing hello.",
        &[LABEL_READY],
    ));
    let mux = Arc::new(FakeMux::new(false));
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: Some("me/proj".into()),
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: Some(worktree_root.clone()),
        pr: None,
        mode: Default::default(),
        deliver: None,
        clean: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
    };
    let deps = Deps::with_label_source(
        Store::open_in_memory().unwrap(),
        mux.clone(),
        forge,
        routing_config(),
        project,
    );

    let run = deps
        .store
        .create_run_for_loop("proj", loop_kind, 7, "Add greeting file")
        .unwrap();
    let agent = spawn_scripted_agent(worktree_root.clone());
    let outcome = tokio::time::timeout(Duration::from_secs(60), run_worker(&deps, &run.id))
        .await
        .expect("worker timed out")
        .unwrap();
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "loop {loop_kind}: expected success, got {outcome:?}"
    );

    let profile = deps.store.get_run(&run.id).unwrap().unwrap().agent_profile;
    (mux.spawned_commands(), profile)
}

#[tokio::test(flavor = "multi_thread")]
async fn every_loop_kind_spawns_from_its_role_resolved_profile() {
    // (loop_kind, expected profile name, expected command, first arg). Loop
    // kinds that share a role (worker/spec-worker; fixer/ci-fixer/
    // conflict-resolver) resolve to the same profile as their sibling.
    let cases = [
        ("planner", "p-planner", "planner-cli", "planner"),
        ("guard", "p-reviewer", "reviewer-cli", "reviewer"),
        ("worker", "p-worker", "worker-cli", "worker"),
        ("spec-worker", "p-worker", "worker-cli", "worker"),
        ("fixer", "p-fixer", "fixer-cli", "fixer"),
        ("ci-fixer", "p-fixer", "fixer-cli", "fixer"),
        ("conflict-resolver", "p-fixer", "fixer-cli", "fixer"),
    ];

    for (loop_kind, profile_name, command, role_arg) in cases {
        let (commands, pinned) = drive_loop_kind(loop_kind).await;
        assert!(!commands.is_empty(), "loop {loop_kind}: a pane was spawned");
        let first = &commands[0];
        assert_eq!(
            first[0], command,
            "loop {loop_kind}: pane command is the profile's: {first:?}"
        );
        assert_eq!(
            &first[1..3],
            &["--role".to_string(), role_arg.to_string()],
            "loop {loop_kind}: profile args lead the command: {first:?}"
        );
        assert!(
            first.last().unwrap().contains(".meguri/prompt-"),
            "loop {loop_kind}: trigger follows the profile args: {first:?}"
        );
        // The resolved profile is pinned on the run for ps/serve/2-3.
        assert_eq!(
            pinned.as_deref(),
            Some(profile_name),
            "loop {loop_kind}: profile pinned on the run"
        );
    }
}

// --- routing 3/3 (issue #66): escalation + explore, end to end -------------

/// Commit everything in the worktree under a fixed identity.
async fn commit_all(wt: &Path, msg: &str) {
    run_git(wt, &["add", "-A"]).await.unwrap();
    run_git(
        wt,
        &[
            "-c",
            "user.email=a@example.com",
            "-c",
            "user.name=agent",
            "commit",
            "-m",
            msg,
        ],
    )
    .await
    .unwrap();
}

/// A worktree-watching fake agent for the escalation/explore scenarios. It
/// commits a unique file on the execute turn (so the tree is ahead of base),
/// and — once it sees the escalation note in a fix prompt — drops the
/// `pass.txt` marker the check command waits on. Pane-agnostic (it reacts to
/// prompt files), so it keeps working after the pane is retired on escalation.
/// Every file it writes is turn-unique, so no commit is ever empty.
fn spawn_escalation_agent(worktree_root: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for _ in 0..600 {
            tokio::time::sleep(Duration::from_millis(150)).await;
            let Some(wt) = find_worktree(&worktree_root) else {
                continue;
            };
            let Some(turn_id) = latest_prompt_turn(&wt) else {
                continue;
            };
            if !seen.insert(turn_id.clone()) {
                continue;
            }
            let prompt = std::fs::read_to_string(wt.join(format!(".meguri/prompt-{turn_id}.md")))
                .unwrap_or_default();
            tokio::spawn(async move {
                if prompt.contains("You are implementing") {
                    std::fs::write(wt.join("greeting.txt"), format!("hello {turn_id}\n")).unwrap();
                    commit_all(&wt, "implement").await;
                }
                if prompt.contains("escalated to a stronger model") {
                    std::fs::write(wt.join("pass.txt"), format!("ok {turn_id}\n")).unwrap();
                    commit_all(&wt, "add pass marker").await;
                }
                let result = serde_json::json!({
                    "turn_id": turn_id, "status": "success", "summary": "scripted",
                });
                std::fs::write(wt.join(".meguri/result.json"), result.to_string()).unwrap();
            });
        }
    })
}

/// Common limits for the scripted scenarios: no nudging, don't linger on the
/// always-Working FakeMux, and skip the self-review phase (not under test).
fn tune(mut config: Config) -> Config {
    config.limits.idle_grace_secs = 3600;
    config.limits.result_grace_secs = 1;
    config.review.enabled = false;
    config
}

/// Manual routing that pins the worker to a cheap profile with a two-step
/// escalation chain to a strong one. Both profiles use `echo` as the command so
/// the real `echo --version` detection (which `next_escalation` runs) passes on
/// any host; the distinguishing first arg tags which one a pane spawned from.
fn escalation_config() -> Config {
    let toml = r#"
[agents.profiles.worker-cheap]
command = "echo"
args = ["cheap"]

[agents.profiles.worker-strong]
command = "echo"
args = ["strong"]

[routing]
mode = "manual"

[routing.roles]
worker = "worker-cheap"

[escalation]
worker = ["worker-cheap", "worker-strong"]
"#;
    tune(toml::from_str(toml).unwrap())
}

/// Like [`escalation_config`] but a three-step chain, and a higher fix-turn
/// budget so the run can reach the top rung. Used to prove no intermediate
/// profile is skipped.
fn escalation_config_3step() -> Config {
    let toml = r#"
[agents.profiles.worker-cheap]
command = "echo"
args = ["cheap"]

[agents.profiles.worker-mid]
command = "echo"
args = ["mid"]

[agents.profiles.worker-strong]
command = "echo"
args = ["strong"]

[routing]
mode = "manual"

[routing.roles]
worker = "worker-cheap"

[escalation]
worker = ["worker-cheap", "worker-mid", "worker-strong"]
"#;
    let mut config = tune(toml::from_str(toml).unwrap());
    config.limits.validate_turns = 5;
    config
}

/// Auto routing where the worker's mainline pick (`claude-sonnet`) is overridden
/// to a detectable `echo` command, so `resolve` lands on it and the explore
/// alternative is the recommendation chain's next entry (`default`, wired to a
/// second distinct `echo`). `ratio` sets `explore_ratio`.
fn explore_config(ratio: &str) -> Config {
    let toml = format!(
        r#"
[agent]
command = "echo"
args = ["default-agent"]

[agents.profiles.claude-sonnet]
command = "echo"
args = ["sonnet"]

[routing]
mode = "auto"
explore_ratio = {ratio}
"#
    );
    tune(toml::from_str(&toml).unwrap())
}

/// Drive one worker run to completion under `config` and `check_command`,
/// returning the pane commands FakeMux recorded, the store (for events/stats),
/// the run id, and the run's terminal outcome.
async fn drive_worker_scenario(
    config: Config,
    check_command: Option<&str>,
) -> (
    Vec<Vec<String>>,
    Store,
    String,
    std::result::Result<WorkerOutcome, String>,
) {
    let root = tempfile::tempdir().unwrap();
    let clone = init_origin_and_clone(root.path()).await;
    let worktree_root = root.path().join("worktrees");

    let forge = Arc::new(FakeForge::with_issue(
        7,
        "Add greeting file",
        "Create `greeting.txt` containing hello.",
        &[LABEL_READY],
    ));
    let mux = Arc::new(FakeMux::new(false));
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: Some("me/proj".into()),
        default_branch: "main".into(),
        language: None,
        check_command: check_command.map(str::to_string),
        worktree_root: Some(worktree_root.clone()),
        pr: None,
        mode: Default::default(),
        deliver: None,
        clean: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
    };
    let store = Store::open_in_memory().unwrap();
    let deps = Deps::with_label_source(store.clone(), mux.clone(), forge, config, project);

    let run = deps
        .store
        .create_run_for_loop("proj", "worker", 7, "Add greeting file")
        .unwrap();
    let agent = spawn_escalation_agent(worktree_root.clone());
    let outcome = tokio::time::timeout(Duration::from_secs(90), run_worker(&deps, &run.id))
        .await
        .expect("worker timed out")
        .map_err(|e| format!("{e:#}"));
    agent.abort();
    (mux.spawned_commands(), store, run.id, outcome)
}

#[tokio::test(flavor = "multi_thread")]
async fn validation_failure_escalates_to_the_next_profile() {
    // The check waits on `pass.txt`, which the agent only drops once it has been
    // escalated — so the run must climb cheap → strong to pass.
    let (commands, store, run_id, outcome) =
        drive_worker_scenario(escalation_config(), Some("test -f pass.txt")).await;
    assert!(
        matches!(outcome, Ok(WorkerOutcome::Succeeded { .. })),
        "escalated run should succeed: {outcome:?}"
    );

    // Exactly two spawns: the cheap profile, then the escalated strong one.
    assert_eq!(commands.len(), 2, "one spawn per profile: {commands:?}");
    assert_eq!(
        &commands[0][..2],
        &["echo".to_string(), "cheap".to_string()]
    );
    assert_eq!(
        &commands[1][..2],
        &["echo".to_string(), "strong".to_string()]
    );
    // The escalated spawn is a fresh session, never a --resume (the model
    // changed, so the old native session can't be restored).
    assert!(
        !commands[1].iter().any(|a| a == "--resume"),
        "escalation must spawn fresh, not resume: {:?}",
        commands[1]
    );

    // The run ends pinned to the strong profile and marked escalated.
    let run = store.get_run(&run_id).unwrap().unwrap();
    assert_eq!(run.agent_profile.as_deref(), Some("worker-strong"));
    assert_eq!(run.routing_arm.as_deref(), Some("escalated"));
    let events = store.events_for_run(&run_id, 200).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "run.escalated"),
        "run.escalated is on the event log"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn escalation_stops_at_the_chain_end_and_hands_to_human() {
    // Validation never passes: the run climbs the chain once, then — at the
    // top with nowhere to go — exhausts its fix turns and fails to needs-human.
    let (commands, store, run_id, outcome) =
        drive_worker_scenario(escalation_config(), Some("false")).await;
    assert!(
        outcome.is_err(),
        "a chain-exhausted run must fail to needs-human: {outcome:?}"
    );

    // Escalated exactly once (cheap → strong), never past the chain end.
    assert_eq!(commands.len(), 2, "no infinite escalation: {commands:?}");
    assert_eq!(
        &commands[0][..2],
        &["echo".to_string(), "cheap".to_string()]
    );
    assert_eq!(
        &commands[1][..2],
        &["echo".to_string(), "strong".to_string()]
    );
    let escalations = store
        .events_for_run(&run_id, 500)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "run.escalated")
        .count();
    assert_eq!(escalations, 1, "exactly one escalation, not a loop");
}

#[tokio::test(flavor = "multi_thread")]
async fn escalation_climbs_a_multi_step_chain_without_skipping() {
    // A three-step chain with a never-passing check: the run must visit each
    // rung in turn (cheap → mid → strong), spawning each exactly once. If a
    // resume/replay ever escalated off a pin that hadn't run yet, `mid` would be
    // skipped and the spawns would read cheap → strong — this pins that it can't.
    let (commands, store, run_id, outcome) =
        drive_worker_scenario(escalation_config_3step(), Some("false")).await;
    assert!(
        outcome.is_err(),
        "a never-passing check ends in needs-human: {outcome:?}"
    );

    let tags: Vec<&str> = commands.iter().map(|c| c[1].as_str()).collect();
    assert_eq!(
        tags,
        vec!["cheap", "mid", "strong"],
        "every chain entry gets a turn, in order: {commands:?}"
    );
    let escalations = store
        .events_for_run(&run_id, 500)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "run.escalated")
        .count();
    assert_eq!(escalations, 2, "two escalations across a three-step chain");
}

#[tokio::test(flavor = "multi_thread")]
async fn explore_diverts_to_the_alternative_and_marks_the_arm() {
    // explore_ratio = 1.0 forces this issue onto the explore arm: instead of the
    // mainline `claude-sonnet`, it spawns the recommendation chain's next entry
    // (`default`), and stats keep it on its own arm row.
    let (commands, store, run_id, outcome) =
        drive_worker_scenario(explore_config("1.0"), None).await;
    assert!(
        matches!(outcome, Ok(WorkerOutcome::Succeeded { .. })),
        "explore run should still succeed: {outcome:?}"
    );

    assert_eq!(
        &commands[0][..2],
        &["echo".to_string(), "default-agent".to_string()],
        "explore spawns the alternative, not the mainline: {commands:?}"
    );
    let run = store.get_run(&run_id).unwrap().unwrap();
    assert_eq!(run.agent_profile.as_deref(), Some("default"));
    assert_eq!(run.routing_arm.as_deref(), Some("explore"));
    let events = store.events_for_run(&run_id, 200).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "run.explore_assigned"),
        "run.explore_assigned is on the event log"
    );
    let rows = store.routing_stats(Some("proj"), 20).unwrap();
    assert!(
        rows.iter().any(|r| r.loop_kind == "worker"
            && r.agent_profile == "default"
            && r.routing_arm == "explore"),
        "explore run gets its own arm row in stats: {rows:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn explore_ratio_zero_keeps_the_mainline_pick() {
    // The default (0.0) diverts nothing: the mainline `claude-sonnet` spawns and
    // the arm stays unset — byte-for-byte the routing-1/3 assignment.
    let (commands, store, run_id, outcome) =
        drive_worker_scenario(explore_config("0.0"), None).await;
    assert!(
        matches!(outcome, Ok(WorkerOutcome::Succeeded { .. })),
        "{outcome:?}"
    );

    assert_eq!(
        &commands[0][..2],
        &["echo".to_string(), "sonnet".to_string()],
        "explore_ratio 0 leaves the mainline pick: {commands:?}"
    );
    let run = store.get_run(&run_id).unwrap().unwrap();
    assert_eq!(run.agent_profile.as_deref(), Some("claude-sonnet"));
    assert_eq!(run.routing_arm, None, "mainline arm stays unset");
    let events = store.events_for_run(&run_id, 200).unwrap();
    assert!(
        !events.iter().any(|e| e.kind == "run.explore_assigned"),
        "no explore assignment when the ratio is 0"
    );
}
