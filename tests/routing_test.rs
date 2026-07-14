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
        ("pr-reviewer", "p-reviewer", "reviewer-cli", "reviewer"),
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
