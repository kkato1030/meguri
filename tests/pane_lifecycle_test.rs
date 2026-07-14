//! Pane lifecycle tests (#13, #92): the issue is the unit of lifetime — one
//! author pane shared by every branch-editing loop. Later runs on the same
//! issue (same loop or a fixer-family one) reuse the live pane, a moved
//! worktree retires-and-respawns it (the session id saved first), and
//! `keep_pane = "never"` releases it as soon as the run succeeds.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use meguri::config::{Config, ProjectConfig};
use meguri::engine::flow::{self, Checkpoint, Flavor};
use meguri::engine::{Deps, WorkerOutcome};
use meguri::forge::LABEL_READY;
use meguri::forge::fake::FakeForge;
use meguri::gitops::{self, run_git};
use meguri::mux::PaneId;
use meguri::mux::fake::FakeMux;
use meguri::store::{ROLE_AUTHOR, Store};

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
    let clone = init_origin_and_clone(root.path()).await;
    let worktree_root = root.path().join("worktrees");
    let session_root = root.path().join("claude");

    let forge = Arc::new(FakeForge::with_issue(
        7,
        "Add greeting file",
        "Create `greeting.txt`.",
        &[LABEL_READY],
    ));

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600; // scripted agent: no nudging wanted
    config.limits.result_grace_secs = 1; // FakeMux always reads Working
    config.agent.session_dir = Some(session_root.clone());
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: Some("me/proj".into()),
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: Some(worktree_root.clone()),
        pr: None,
        clean: None,
        plan_delivery: Default::default(),
        review: None,
        mode: Default::default(),
        deliver: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
        autonomy: None,
    };

    let mux = Arc::new(FakeMux::new(false));
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

/// A worker-shaped flavor pinned to one branch, so consecutive runs on the
/// same issue share the worktree (the fixer/spec-worker continuity shape).
struct FixedBranchFlavor {
    branch: String,
}

#[async_trait]
impl Flavor for FixedBranchFlavor {
    fn trigger_label(&self) -> &'static str {
        LABEL_READY
    }

    async fn prepare_worktree(
        &self,
        deps: &Deps,
        run: &meguri::store::RunRecord,
        _cp: &Checkpoint,
    ) -> Result<()> {
        let root = deps.project.worktree_root.clone().unwrap();
        let wt = gitops::worktree_path(&root, &deps.project.id, &self.branch);
        if !wt.exists() {
            gitops::create_worktree(&deps.project.repo_path, &wt, &self.branch, "main", &[])
                .await?;
        }
        deps.store
            .update_run_worktree(&run.id, &self.branch, &wt.to_string_lossy())?;
        Ok(())
    }

    fn execute_prompt(
        &self,
        _deps: &Deps,
        run: &meguri::store::RunRecord,
        _cp: &Checkpoint,
        _worktree: &Path,
    ) -> String {
        format!("Work on issue #{}.", run.issue_number)
    }

    fn verify_work(
        &self,
        _run: &meguri::store::RunRecord,
        _cp: &Checkpoint,
        _worktree: &Path,
    ) -> std::result::Result<(), String> {
        Ok(())
    }

    fn pr_title(&self, run: &meguri::store::RunRecord, cp: &Checkpoint) -> String {
        format!("{} (#{})", cp.issue_title, run.issue_number)
    }

    async fn settle_labels(
        &self,
        _deps: &Deps,
        _run: &meguri::store::RunRecord,
        _cp: &Checkpoint,
    ) -> Result<()> {
        Ok(()) // keep the trigger label so a follow-up run can claim again
    }
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

fn pending_turn(worktree: &Path) -> Option<String> {
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

/// Scripted pane-side agent over every worktree of the project: commit a
/// file (once per worktree) and report success for each new prompt turn.
fn spawn_scripted_agent(worktree_root: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let Ok(entries) = std::fs::read_dir(worktree_root.join("proj")) else {
                continue;
            };
            for wt in entries.filter_map(|e| e.ok()).map(|e| e.path()) {
                if !wt.is_dir() {
                    continue;
                }
                let Some(turn_id) = pending_turn(&wt) else {
                    continue;
                };
                if !wt.join("work.txt").exists() {
                    std::fs::write(wt.join("work.txt"), "done\n").unwrap();
                    run_git(&wt, &["add", "work.txt"]).await.unwrap();
                    run_git(
                        &wt,
                        &[
                            "-c",
                            "user.email=a@a",
                            "-c",
                            "user.name=agent",
                            "commit",
                            "-m",
                            "work",
                        ],
                    )
                    .await
                    .unwrap();
                }
                std::fs::write(
                    wt.join(".meguri/result.json"),
                    serde_json::json!({
                        "turn_id": turn_id, "status": "success", "summary": "scripted",
                    })
                    .to_string(),
                )
                .unwrap();
            }
        }
    })
}

async fn drive_to_success(env: &TestEnv, flavor: &dyn Flavor) -> String {
    let run = env
        .deps
        .store
        .create_run("proj", 7, "Add greeting file")
        .unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        flow::run_flow(&env.deps, &run.id, flavor),
    )
    .await
    .expect("run timed out")
    .unwrap();
    let WorkerOutcome::Succeeded { .. } = outcome else {
        panic!("expected success, got {outcome:?}");
    };
    run.id
}

#[tokio::test(flavor = "multi_thread")]
async fn second_run_on_same_issue_reuses_live_pane() {
    let env = setup().await;
    let flavor = FixedBranchFlavor {
        branch: "meguri/7-fixed".into(),
    };
    let agent = spawn_scripted_agent(env.worktree_root.clone());

    let run1 = drive_to_success(&env, &flavor).await;

    // keep_pane default (until-issue-closed): the pane survives success.
    assert_eq!(env.mux.pane_count(), 1);
    let pane = env
        .deps
        .store
        .get_pane("proj", 7, ROLE_AUTHOR)
        .unwrap()
        .unwrap();
    let pane_id = PaneId(pane.mux_pane_id.clone().unwrap());
    assert!(env.deps.mux.pane_alive(&pane_id).await.unwrap());

    let run2 = drive_to_success(&env, &flavor).await;
    agent.abort();
    assert_ne!(run1, run2);

    // Same pane, no new spawn; run 2's trigger was typed into it.
    assert_eq!(
        env.mux.pane_count(),
        1,
        "pane must be reused, not respawned"
    );
    let sent = env.mux.sent_lines(&pane_id);
    assert_eq!(sent.len(), 1, "one reused-turn trigger, got {sent:?}");
    assert!(sent[0].contains("prompt-"));

    // The second run adopted the issue's pane in its own record.
    let rec2 = env.deps.store.get_run(&run2).unwrap().unwrap();
    assert_eq!(rec2.mux_pane_id.as_deref(), Some(pane_id.0.as_str()));
}

#[tokio::test(flavor = "multi_thread")]
async fn keep_pane_never_releases_pane_after_success() {
    let mut env = setup().await;
    env.deps.config.mux.keep_pane = "never".into();
    let flavor = FixedBranchFlavor {
        branch: "meguri/7-fixed".into(),
    };
    let agent = spawn_scripted_agent(env.worktree_root.clone());

    // The agent's native session exists before the run ends.
    let wt = gitops::worktree_path(&env.worktree_root, "proj", "meguri/7-fixed");
    write_session_transcript(&env.session_root, &wt, "sess-never");

    drive_to_success(&env, &flavor).await;
    agent.abort();

    let pane = env
        .deps
        .store
        .get_pane("proj", 7, ROLE_AUTHOR)
        .unwrap()
        .unwrap();
    assert_eq!(pane.mux_pane_id, None, "pane released at run end");
    assert_eq!(
        pane.agent_session_id.as_deref(),
        Some("sess-never"),
        "session id saved before the kill"
    );
    assert!(pane.reclaimed_at.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn moved_worktree_retires_old_pane_and_respawns() {
    let env = setup().await;
    let agent = spawn_scripted_agent(env.worktree_root.clone());

    let old_wt = gitops::worktree_path(&env.worktree_root, "proj", "meguri/7-first");
    write_session_transcript(&env.session_root, &old_wt, "sess-first");
    drive_to_success(
        &env,
        &FixedBranchFlavor {
            branch: "meguri/7-first".into(),
        },
    )
    .await;
    let first_pane = PaneId(
        env.deps
            .store
            .get_pane("proj", 7, ROLE_AUTHOR)
            .unwrap()
            .unwrap()
            .mux_pane_id
            .unwrap(),
    );

    // Same issue, new branch: the old pane can't see the new worktree.
    drive_to_success(
        &env,
        &FixedBranchFlavor {
            branch: "meguri/7-second".into(),
        },
    )
    .await;
    agent.abort();

    assert_eq!(env.mux.pane_count(), 2, "old retired, new spawned");
    assert!(!env.deps.mux.pane_alive(&first_pane).await.unwrap());
    let pane = env
        .deps
        .store
        .get_pane("proj", 7, ROLE_AUTHOR)
        .unwrap()
        .unwrap();
    let new_id = pane.mux_pane_id.expect("new pane registered");
    assert_ne!(new_id, first_pane.0);
    assert!(
        pane.worktree_path.unwrap().ends_with("meguri-7-second"),
        "mapping follows the new worktree"
    );
    assert_eq!(
        pane.agent_session_id.as_deref(),
        Some("sess-first"),
        "old pane's session saved on retirement"
    );
}

/// Acceptance (issue #92): a fixer-family run on the same issue joins the
/// author lane — it adopts the worker's live pane instead of spawning its
/// own, so the implementation session continues.
#[tokio::test(flavor = "multi_thread")]
async fn fixer_family_run_adopts_the_workers_author_pane() {
    let env = setup().await;
    let flavor = FixedBranchFlavor {
        branch: "meguri/7-fixed".into(),
    };
    let agent = spawn_scripted_agent(env.worktree_root.clone());

    // Round 1: the worker ships the issue (create_run → loop_kind "worker").
    drive_to_success(&env, &flavor).await;
    let pane = env
        .deps
        .store
        .get_pane("proj", 7, ROLE_AUTHOR)
        .unwrap()
        .unwrap();
    let pane_id = pane.mux_pane_id.expect("worker pane registered");

    // Round 2: review feedback arrives — a fixer run on the same issue and
    // branch (the flow under test is ensure_pane's lane key, so the
    // worker-shaped flavor stands in for the fixer's).
    let run = env
        .deps
        .store
        .create_run_for_loop("proj", "fixer", 7, "Add greeting file")
        .unwrap();
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        flow::run_flow(&env.deps, &run.id, &flavor),
    )
    .await
    .expect("fixer run timed out")
    .unwrap();
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    assert_eq!(env.mux.pane_count(), 1, "no second pane for the same lane");
    let rec = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(
        rec.mux_pane_id.as_deref(),
        Some(pane_id.as_str()),
        "the fixer run adopted the worker's pane"
    );
}
