//! `ensure_project_clone` — the tick-top reconcile hook that materializes a
//! managed bare clone (issue #195). Because the derived path
//! (`~/.meguri/repos/<id>`) is resolved through `MEGURI_HOME`, this file owns
//! that env in its own test process and runs its cases sequentially in one
//! `#[test]` so nothing races on it.
//!
//! The actual clone/health logic is unit-tested in `gitops`; here we cover the
//! wrapper's behavior: which projects it acts on, the `repo.clone.failed`
//! observability, and that a failure never reaches for a needs-human surface
//! (there is no run/issue/PR at tick top — ADR 0018).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use meguri::config::{Config, ProjectConfig, ProjectMode};
use meguri::engine::{Deps, ensure_project_clone};
use meguri::forge::fake::FakeForge;
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::Store;

fn managed_project(id: &str, repo_path: Option<PathBuf>, mode: ProjectMode) -> ProjectConfig {
    ProjectConfig {
        id: id.into(),
        repo_path,
        repo_slug: Some("owner/repo".into()),
        mode,
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: None,
        worktree_root: None,
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
    }
}

fn deps_for(project: ProjectConfig) -> Deps {
    Deps::with_label_source(
        Store::open_in_memory().unwrap(),
        Arc::new(FakeMux::new(false)),
        Arc::new(FakeForge::default()),
        Config::default(),
        project,
    )
}

/// A local bare origin at `<root>/owner/repo.git`, so a managed clone of it has
/// a `remote.origin.url` whose tail resolves to the slug `owner/repo` (matching
/// the project's `repo_slug` — the health check verifies this).
async fn bare_origin(root: &Path) -> PathBuf {
    let work = root.join("work");
    std::fs::create_dir_all(&work).unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "t@example.com"],
        vec!["config", "user.name", "t"],
        vec!["commit", "--allow-empty", "-m", "seed"],
    ] {
        run_git(&work, &args).await.unwrap();
    }
    let origin = root.join("owner").join("repo.git");
    std::fs::create_dir_all(origin.parent().unwrap()).unwrap();
    run_git(
        root,
        &[
            "clone",
            "--bare",
            work.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    )
    .await
    .unwrap();
    origin
}

/// Materialize a healthy managed bare clone at `dest` from `origin` (what
/// `ensure_bare_clone`'s clone step leaves behind, minus gh).
async fn healthy_managed_clone(origin: &Path, dest: &Path) {
    let parent = dest.parent().unwrap();
    std::fs::create_dir_all(parent).unwrap();
    run_git(
        parent,
        &[
            "clone",
            "--bare",
            origin.to_str().unwrap(),
            dest.to_str().unwrap(),
        ],
    )
    .await
    .unwrap();
    run_git(
        dest,
        &[
            "config",
            "remote.origin.fetch",
            "+refs/heads/*:refs/remotes/origin/*",
        ],
    )
    .await
    .unwrap();
    run_git(dest, &["fetch", "origin"]).await.unwrap();
}

#[tokio::test]
async fn ensure_project_clone_gating_and_failure_surface() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    // Owned by this test process; no other test in this binary reads it.
    unsafe { std::env::set_var("MEGURI_HOME", &home) };
    let repos = home.join("repos");
    let origin = bare_origin(root.path()).await;

    // --- Case 1: managed github project, derived clone already healthy → Ok,
    // no `repo.clone.failed`, and no `repo.cloned` (it wasn't absent).
    {
        let deps = deps_for(managed_project("healthy", None, ProjectMode::Github));
        healthy_managed_clone(&origin, &repos.join("healthy")).await;
        ensure_project_clone(&deps).await.unwrap();
        assert_eq!(deps.store.count_events("repo.clone.failed").unwrap(), 0);
        assert_eq!(deps.store.count_events("repo.cloned").unwrap(), 0);
    }

    // --- Case 2: managed github project, derived path is a broken remnant → Err,
    // `repo.clone.failed` emitted, and NO needs-human escalation (there is no
    // run/issue/PR to key it to — ADR 0018).
    {
        let deps = deps_for(managed_project("broken", None, ProjectMode::Github));
        let dest = repos.join("broken");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(ensure_project_clone(&deps).await.is_err());
        assert_eq!(deps.store.count_events("repo.clone.failed").unwrap(), 1);
        assert_eq!(deps.store.count_events("escalation.raised").unwrap(), 0);
    }

    // --- Case 3: explicit repo_path (host-owned) → never a managed clone, so a
    // no-op even when the path is missing (meguri must not clone over it).
    {
        let explicit = root.path().join("host-clone-does-not-exist");
        let deps = deps_for(managed_project(
            "explicit",
            Some(explicit),
            ProjectMode::Github,
        ));
        ensure_project_clone(&deps).await.unwrap();
        assert_eq!(deps.store.count_events("repo.clone.failed").unwrap(), 0);
        assert_eq!(deps.store.count_events("repo.cloned").unwrap(), 0);
    }

    // --- Case 4: local mode → no remote to clone from, always a no-op.
    {
        let deps = deps_for(managed_project(
            "loc",
            Some(root.path().join("nope")),
            ProjectMode::Local,
        ));
        ensure_project_clone(&deps).await.unwrap();
        assert_eq!(deps.store.count_events("repo.clone.failed").unwrap(), 0);
    }

    unsafe { std::env::remove_var("MEGURI_HOME") };
}
