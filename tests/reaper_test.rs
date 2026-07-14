//! Reaper tests: `meguri prune` / watch-sweep classification and reclamation
//! of panes and worktrees whose issue closed on the forge.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use meguri::config::{Config, ProjectConfig};
use meguri::engine::Deps;
use meguri::engine::reaper::{self, Verdict};
use meguri::forge::fake::FakeForge;
use meguri::gitops::{self, run_git};
use meguri::mux::PaneSpec;
use meguri::mux::fake::FakeMux;
use meguri::store::{ROLE_AUTHOR, RunStatus, Store};

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

async fn setup(root: &Path, forge: Arc<FakeForge>) -> Deps {
    let clone = init_origin_and_clone(root).await;
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: Some(root.join("worktrees")),
        pr: None,
        clean: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
        autonomy: None,
        prompts: Default::default(),
    };
    Deps::with_label_source(
        Store::open_in_memory().unwrap(),
        Arc::new(FakeMux::new(false)),
        forge,
        Config::default(),
        project,
    )
}

/// Create a meguri worktree for `issue` with one committed file; returns
/// (branch, worktree path).
async fn add_worktree(deps: &Deps, issue: i64, title: &str) -> (String, PathBuf) {
    let branch = gitops::branch_name(issue, title, &format!("run-{issue}"));
    let root = deps.project.worktree_root.clone().unwrap();
    let wt = gitops::worktree_path(&root, &deps.project.id, &branch);
    gitops::create_worktree(&deps.project.repo_path, &wt, &branch, "main", &[])
        .await
        .unwrap();
    std::fs::write(wt.join("work.txt"), format!("issue {issue}\n")).unwrap();
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
    (branch, wt)
}

/// Create a reviewer-style worktree for PR `pr`: a detached checkout named
/// `review-<PR#>-<run id>` whose finished run carries the PR number as
/// issue_number (that's all the reaper has to resolve the forge state from).
async fn add_review_worktree(deps: &Deps, pr: i64) -> PathBuf {
    let run = deps
        .store
        .create_run_for_loop("proj", "spec-reviewer", pr, &format!("Review PR #{pr}"))
        .unwrap();
    let root = deps.project.worktree_root.clone().unwrap();
    let wt = gitops::worktree_path(&root, &deps.project.id, &format!("review-{pr}-{}", run.id));
    let head = run_git(&deps.project.repo_path, &["rev-parse", "HEAD"])
        .await
        .unwrap();
    gitops::create_review_worktree(&deps.project.repo_path, &wt, "pr-head", head.trim(), &[])
        .await
        .unwrap();
    // A detached checkout reports no branch, so the run lookup goes by path;
    // store it canonicalized the way the reaper compares it.
    let wt = std::fs::canonicalize(&wt).unwrap();
    deps.store
        .update_run_worktree(&run.id, "pr-head", &wt.to_string_lossy())
        .unwrap();
    deps.store
        .update_run_status(&run.id, RunStatus::Succeeded, None)
        .unwrap();
    wt
}

fn verdict_of(candidates: &[reaper::Candidate], issue: i64) -> &reaper::Candidate {
    candidates
        .iter()
        .find(|c| c.issue == Some(issue))
        .unwrap_or_else(|| panic!("no candidate for issue #{issue}: {candidates:?}"))
}

#[tokio::test]
async fn plan_classifies_closed_open_dirty_active_and_orphan() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(1, "Closed clean", "", &[]));
    for (n, t) in [(2, "Still open"), (3, "Closed dirty"), (4, "Active run")] {
        forge.issues.lock().unwrap().push(meguri::forge::Issue {
            number: n,
            title: t.into(),
            body: String::new(),
            labels: vec![],
        });
    }
    let deps = setup(root.path(), forge.clone()).await;

    add_worktree(&deps, 1, "Closed clean").await;
    add_worktree(&deps, 2, "Still open").await;
    let (_, dirty_wt) = add_worktree(&deps, 3, "Closed dirty").await;
    std::fs::write(dirty_wt.join("uncommitted.txt"), "wip").unwrap();
    let (active_branch, active_wt) = add_worktree(&deps, 4, "Active run").await;

    // Issue #4's worktree belongs to a run that is still active.
    let run = deps.store.create_run("proj", 4, "Active run").unwrap();
    deps.store
        .update_run_worktree(&run.id, &active_branch, &active_wt.to_string_lossy())
        .unwrap();
    deps.store
        .update_run_status(&run.id, RunStatus::Running, None)
        .unwrap();

    // An orphan worktree under meguri's root with a non-meguri branch.
    let orphan_wt = root.path().join("worktrees/proj/manual-experiment");
    run_git(
        &deps.project.repo_path,
        &[
            "worktree",
            "add",
            "-b",
            "manual-experiment",
            orphan_wt.to_str().unwrap(),
            "main",
        ],
    )
    .await
    .unwrap();

    forge.close_issue(1);
    forge.close_issue(3);
    forge.close_issue(4); // even closed, the active run protects it

    let candidates = reaper::plan(&deps).await.unwrap();
    assert_eq!(candidates.len(), 5, "{candidates:?}");
    assert_eq!(verdict_of(&candidates, 1).verdict, Verdict::Reclaim);
    assert_eq!(verdict_of(&candidates, 2).verdict, Verdict::Open);
    assert_eq!(verdict_of(&candidates, 3).verdict, Verdict::Dirty);
    assert_eq!(verdict_of(&candidates, 4).verdict, Verdict::ActiveRun);
    let orphan = candidates
        .iter()
        .find(|c| c.issue.is_none())
        .expect("orphan candidate listed");
    assert_eq!(orphan.verdict, Verdict::Orphan);
}

#[tokio::test]
async fn reclaim_removes_worktree_and_merged_branch() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(5, "Shipped", "", &[]));
    let deps = setup(root.path(), forge.clone()).await;

    let (branch, wt) = add_worktree(&deps, 5, "Shipped").await;
    // The PR merged: the branch's commits landed on origin/main.
    run_git(&deps.project.repo_path, &["merge", &branch])
        .await
        .unwrap();
    run_git(&deps.project.repo_path, &["push", "origin", "main"])
        .await
        .unwrap();
    forge.close_issue(5);

    let candidates = reaper::plan(&deps).await.unwrap();
    let reclaimed = reaper::reclaim(&deps, &candidates, false).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert!(reclaimed[0].branch_deleted);
    assert!(!wt.exists());

    // git worktree list is clean: only the primary checkout remains.
    let listed = gitops::list_worktrees(&deps.project.repo_path)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert!(
        run_git(&deps.project.repo_path, &["rev-parse", "--verify", &branch])
            .await
            .is_err(),
        "merged local branch is deleted"
    );
}

#[tokio::test]
async fn unmerged_branch_is_kept_without_force() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(6, "Squash merged", "", &[]));
    let deps = setup(root.path(), forge.clone()).await;

    let (branch, wt) = add_worktree(&deps, 6, "Squash merged").await;
    forge.close_issue(6); // closed on the forge, but not merged in git terms

    let candidates = reaper::plan(&deps).await.unwrap();
    let reclaimed = reaper::reclaim(&deps, &candidates, false).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert!(!wt.exists(), "worktree goes even when the branch stays");
    assert!(!reclaimed[0].branch_deleted);
    run_git(&deps.project.repo_path, &["rev-parse", "--verify", &branch])
        .await
        .expect("unmerged branch survives without --force");
}

/// Simulate a squash merge of `branch`: main gains an equivalent commit with
/// a different sha, so the branch tip is *not* an ancestor of origin/main.
async fn squash_merge_onto_main(deps: &Deps, issue: i64) {
    let repo = &deps.project.repo_path;
    std::fs::write(repo.join("work.txt"), format!("issue {issue}\n")).unwrap();
    run_git(repo, &["add", "work.txt"]).await.unwrap();
    run_git(
        repo,
        &[
            "-c",
            "user.email=a@a",
            "-c",
            "user.name=agent",
            "commit",
            "-m",
            "squashed",
        ],
    )
    .await
    .unwrap();
    run_git(repo, &["push", "origin", "main"]).await.unwrap();
}

#[tokio::test]
async fn squash_merged_branch_is_deleted_via_forge_pr_state() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(11, "Squashed", "", &[]));
    let deps = setup(root.path(), forge.clone()).await;

    let (branch, wt) = add_worktree(&deps, 11, "Squashed").await;
    squash_merge_onto_main(&deps, 11).await;
    forge.add_pr(41, "Squashed", "", &[], &branch, "sha41");
    forge.set_pr_state(41, "merged");
    forge.close_issue(11);

    let candidates = reaper::plan(&deps).await.unwrap();
    let reclaimed = reaper::reclaim(&deps, &candidates, false).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert!(!wt.exists());
    assert!(
        reclaimed[0].branch_deleted,
        "merged PR state deletes the squash-merged branch"
    );
    assert!(
        run_git(&deps.project.repo_path, &["rev-parse", "--verify", &branch])
            .await
            .is_err(),
        "squash-merged local branch is deleted"
    );
}

#[tokio::test]
async fn open_pr_branch_is_kept_without_force() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(12, "Still open PR", "", &[]));
    let deps = setup(root.path(), forge.clone()).await;

    let (branch, wt) = add_worktree(&deps, 12, "Still open PR").await;
    forge.add_pr(42, "Still open PR", "", &[], &branch, "sha42");
    forge.close_issue(12); // issue closed, but the PR never merged

    let candidates = reaper::plan(&deps).await.unwrap();
    let reclaimed = reaper::reclaim(&deps, &candidates, false).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert!(!wt.exists());
    assert!(!reclaimed[0].branch_deleted);
    run_git(&deps.project.repo_path, &["rev-parse", "--verify", &branch])
        .await
        .expect("branch with an open PR survives without --force");
}

#[tokio::test]
async fn forge_lookup_failure_keeps_branch_but_reclaims_worktree() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(13, "Forge down", "", &[]));
    let deps = setup(root.path(), forge.clone()).await;

    let (branch, wt) = add_worktree(&deps, 13, "Forge down").await;
    forge.add_pr(43, "Forge down", "", &[], &branch, "sha43");
    forge.set_pr_state(43, "merged"); // merged, but the lookup will fail
    forge.fail_pr_for_branch(&branch);
    forge.close_issue(13);

    let candidates = reaper::plan(&deps).await.unwrap();
    let reclaimed = reaper::reclaim(&deps, &candidates, false).await.unwrap();
    assert_eq!(reclaimed.len(), 1, "worktree reclamation still succeeds");
    assert!(!wt.exists());
    assert!(!reclaimed[0].branch_deleted);
    run_git(&deps.project.repo_path, &["rev-parse", "--verify", &branch])
        .await
        .expect("branch survives when the forge cannot answer");
}

#[tokio::test]
async fn dirty_worktree_needs_force() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(7, "Dirty", "", &[]));
    let deps = setup(root.path(), forge.clone()).await;

    let (_, wt) = add_worktree(&deps, 7, "Dirty").await;
    std::fs::write(wt.join("uncommitted.txt"), "precious wip").unwrap();
    forge.close_issue(7);

    let candidates = reaper::plan(&deps).await.unwrap();
    assert_eq!(verdict_of(&candidates, 7).verdict, Verdict::Dirty);

    let reclaimed = reaper::reclaim(&deps, &candidates, false).await.unwrap();
    assert!(reclaimed.is_empty());
    assert!(wt.exists(), "dirty worktree survives without --force");

    let reclaimed = reaper::reclaim(&deps, &candidates, true).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert!(!wt.exists(), "--force reclaims the dirty worktree");
}

#[tokio::test]
async fn live_pane_protects_closed_issue_worktree() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(8, "Pane alive", "", &[]));
    let deps = setup(root.path(), forge.clone()).await;

    let (branch, wt) = add_worktree(&deps, 8, "Pane alive").await;
    let pane = deps
        .mux
        .spawn_pane(&PaneSpec {
            title: "meguri#8".into(),
            cwd: wt.clone(),
            command: vec!["agent".into()],
            env: vec![],
        })
        .await
        .unwrap();
    let run = deps.store.create_run("proj", 8, "Pane alive").unwrap();
    deps.store
        .update_run_worktree(&run.id, &branch, &wt.to_string_lossy())
        .unwrap();
    deps.store
        .update_run_mux(&run.id, deps.mux.kind().as_str(), "meguri", &pane.0)
        .unwrap();
    deps.store
        .update_run_status(&run.id, RunStatus::Succeeded, None)
        .unwrap();
    forge.close_issue(8);

    let candidates = reaper::plan(&deps).await.unwrap();
    assert_eq!(verdict_of(&candidates, 8).verdict, Verdict::PaneAlive);
    assert!(
        reaper::reclaim(&deps, &candidates, false)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(wt.exists());

    // Once the pane dies the worktree becomes reclaimable.
    deps.mux.kill_pane(&pane).await.unwrap();
    let candidates = reaper::plan(&deps).await.unwrap();
    assert_eq!(verdict_of(&candidates, 8).verdict, Verdict::Reclaim);
    reaper::reclaim(&deps, &candidates, false).await.unwrap();
    assert!(!wt.exists());
}

/// Register a live pane for the issue the way ensure_pane does.
async fn register_pane(deps: &Deps, issue: i64, wt: &Path) -> meguri::mux::PaneId {
    let pane = deps
        .mux
        .spawn_pane(&PaneSpec {
            title: format!("meguri#{issue}"),
            cwd: wt.to_path_buf(),
            command: vec!["agent".into()],
            env: vec![],
        })
        .await
        .unwrap();
    deps.store
        .upsert_pane(
            "proj",
            issue,
            ROLE_AUTHOR,
            deps.mux.kind().as_str(),
            "meguri",
            &pane.0,
            &wt.to_string_lossy(),
        )
        .unwrap();
    pane
}

/// Claude Code's directory name for a project cwd (mirrors agent_session).
fn munged(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[tokio::test]
async fn sweep_reclaims_pane_then_worktree_of_closed_issue() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(11, "Shipped", "", &[]));
    let mut deps = setup(root.path(), forge.clone()).await;
    let session_root = root.path().join("claude");
    deps.config.agent.session_dir = Some(session_root.clone());

    let (_, wt) = add_worktree(&deps, 11, "Shipped").await;
    let pane = register_pane(&deps, 11, &wt).await;
    // The agent's native session transcript exists for the worktree.
    let dir = session_root.join("projects").join(munged(&wt));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sess-11.jsonl"), "{}\n").unwrap();

    forge.close_issue(11);
    // The plan carries the real reclamation reason into the emitted event.
    let mut states = reaper::IssueStates::default();
    let candidates = reaper::plan_panes(&deps, &mut states).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].verdict, Verdict::Reclaim);
    assert_eq!(candidates[0].reason, reaper::REASON_ISSUE_CLOSED);
    reaper::sweep(&deps).await.unwrap();

    // Pane killed and detached; the session id survived the kill.
    assert!(!deps.mux.pane_alive(&pane).await.unwrap());
    let record = deps
        .store
        .get_pane("proj", 11, ROLE_AUTHOR)
        .unwrap()
        .unwrap();
    assert_eq!(record.mux_pane_id, None);
    assert_eq!(record.agent_session_id.as_deref(), Some("sess-11"));
    // And the worktree fell in the same sweep (no PaneAlive protection left).
    assert!(!wt.exists());
}

#[tokio::test]
async fn sweep_keeps_pane_of_open_issue_and_active_run() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(12, "Open", "", &[]));
    forge.issues.lock().unwrap().push(meguri::forge::Issue {
        number: 13,
        title: "Closed but active".into(),
        body: String::new(),
        labels: vec![],
    });
    let deps = setup(root.path(), forge.clone()).await;

    let (_, open_wt) = add_worktree(&deps, 12, "Open").await;
    let open_pane = register_pane(&deps, 12, &open_wt).await;

    let (branch, active_wt) = add_worktree(&deps, 13, "Closed but active").await;
    let active_pane = register_pane(&deps, 13, &active_wt).await;
    let run = deps
        .store
        .create_run("proj", 13, "Closed but active")
        .unwrap();
    deps.store
        .update_run_worktree(&run.id, &branch, &active_wt.to_string_lossy())
        .unwrap();
    deps.store
        .update_run_status(&run.id, RunStatus::Running, None)
        .unwrap();
    forge.close_issue(13);

    let mut states = reaper::IssueStates::default();
    let candidates = reaper::plan_panes(&deps, &mut states).await.unwrap();
    let verdict = |issue: i64| {
        candidates
            .iter()
            .find(|c| c.issue == issue)
            .unwrap()
            .verdict
    };
    assert_eq!(verdict(12), Verdict::Open);
    assert_eq!(verdict(13), Verdict::ActiveRun);

    reaper::sweep(&deps).await.unwrap();
    assert!(deps.mux.pane_alive(&open_pane).await.unwrap());
    assert!(deps.mux.pane_alive(&active_pane).await.unwrap());
    assert!(open_wt.exists());
    assert!(active_wt.exists());
}

#[tokio::test]
async fn sweep_clears_stale_mapping_of_dead_pane() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(14, "Open, pane crashed", "", &[]));
    let deps = setup(root.path(), forge.clone()).await;

    let (_, wt) = add_worktree(&deps, 14, "Open, pane crashed").await;
    let pane = register_pane(&deps, 14, &wt).await;
    deps.mux.kill_pane(&pane).await.unwrap(); // crashed outside meguri

    // A dead mapping is reclaimed for what it is — not "issue closed".
    let mut states = reaper::IssueStates::default();
    let candidates = reaper::plan_panes(&deps, &mut states).await.unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].verdict, Verdict::Reclaim);
    assert_eq!(candidates[0].reason, reaper::REASON_PANE_DEAD);
    reaper::sweep(&deps).await.unwrap();
    let record = deps
        .store
        .get_pane("proj", 14, ROLE_AUTHOR)
        .unwrap()
        .unwrap();
    assert_eq!(record.mux_pane_id, None, "stale mapping cleared");
    assert!(wt.exists(), "open issue's worktree untouched");
}

#[tokio::test]
async fn merged_pr_review_worktree_is_reclaimed() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    // No issue #32 exists — the number belongs to a PR, and it merged.
    forge.add_pr(32, "Shipped", "", &[], "pr-head", "sha32");
    forge.set_pr_state(32, "merged");
    let deps = setup(root.path(), forge.clone()).await;

    let wt = add_review_worktree(&deps, 32).await;

    let candidates = reaper::plan(&deps).await.unwrap();
    assert_eq!(verdict_of(&candidates, 32).verdict, Verdict::Reclaim);

    let reclaimed = reaper::reclaim(&deps, &candidates, false).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert!(!wt.exists(), "merged PR's review worktree reclaimed");
}

#[tokio::test]
async fn open_pr_review_worktree_is_skipped() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    forge.add_pr(33, "Under review", "", &[], "pr-head", "sha33");
    let deps = setup(root.path(), forge.clone()).await;

    let wt = add_review_worktree(&deps, 33).await;

    let candidates = reaper::plan(&deps).await.unwrap();
    assert_eq!(verdict_of(&candidates, 33).verdict, Verdict::Open);

    reaper::sweep(&deps).await.unwrap();
    assert!(wt.exists(), "open PR's review worktree untouched");
}

#[tokio::test]
async fn sweep_reclaims_only_closed_clean_worktrees() {
    let root = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::with_issue(9, "Closed", "", &[]));
    forge.issues.lock().unwrap().push(meguri::forge::Issue {
        number: 10,
        title: "Open".into(),
        body: String::new(),
        labels: vec![],
    });
    let deps = setup(root.path(), forge.clone()).await;

    let (_, closed_wt) = add_worktree(&deps, 9, "Closed").await;
    let (_, open_wt) = add_worktree(&deps, 10, "Open").await;
    forge.close_issue(9);

    reaper::sweep(&deps).await.unwrap();
    assert!(!closed_wt.exists(), "closed issue's worktree reclaimed");
    assert!(open_wt.exists(), "open issue's worktree untouched");
}
