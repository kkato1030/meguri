//! Cron schedule sweep (issue #146): firing, catch-up folding, backfill
//! suppression, the overlap guard, and hot-reload definition additions —
//! driven with a fixed injected clock against a FakeForge / in-memory store.

use std::sync::Arc;

use meguri::config::{Config, ProjectConfig, ProjectMode, ScheduleConfig, ScheduleKind};
use meguri::engine::Deps;
use meguri::engine::schedule::{DiagMemory, sweep as sweep_impl};
use meguri::forge::fake::FakeForge;
use meguri::forge::{LABEL_PLAN, LABEL_READY};
use meguri::mux::fake::FakeMux;
use meguri::store::{Store, parse_ts};
use meguri::tasks::{LocalTaskSource, TaskKind, TaskSource};

/// The pre-#222 `sweep(deps, now)` shape, wrapped over the reconciler's
/// `sweep(deps, now, &mut DiagMemory)` with a throwaway diagnostic memory — the
/// existing cases don't assert on edge-triggered emission, so a fresh memory
/// per call is fine (the edge-triggered case builds its own shared memory).
async fn sweep(deps: &Deps, now: u64) -> anyhow::Result<()> {
    let mut diag = DiagMemory::new();
    sweep_impl(deps, now, &mut diag).await
}

fn ts(s: &str) -> u64 {
    parse_ts(s).unwrap()
}

/// A schedule with an inline body (`name {{date}}` title, overlap guard as
/// given).
fn sched(name: &str, cron: &str, kind: ScheduleKind, allow_overlap: bool) -> ScheduleConfig {
    ScheduleConfig {
        name: name.into(),
        cron: cron.into(),
        kind,
        title: name.to_string() + " {{date}}",
        body_file: None,
        body: Some(format!("body of {name}")),
        allow_overlap,
    }
}

fn make_project(
    mode: ProjectMode,
    repo_path: std::path::PathBuf,
    schedules: Vec<ScheduleConfig>,
) -> ProjectConfig {
    ProjectConfig {
        id: "proj".into(),
        repo_path: Some(repo_path),
        repo_slug: if mode == ProjectMode::Local {
            None
        } else {
            Some("me/proj".into())
        },
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
        autonomy: None,
        worktree_setup: Default::default(),
        schedules,
        cadence: Vec::new(),
        prompts: Default::default(),
        notify: None,
    }
}

fn github_deps_on(
    store: Store,
    forge: Arc<FakeForge>,
    repo_path: std::path::PathBuf,
    schedules: Vec<ScheduleConfig>,
) -> Deps {
    let project = make_project(ProjectMode::Github, repo_path, schedules);
    Deps::with_label_source(
        store,
        Arc::new(FakeMux::new(false)),
        forge,
        Config::default(),
        project,
    )
}

fn local_deps(schedules: Vec<ScheduleConfig>) -> (Deps, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let project = make_project(ProjectMode::Local, dir.path().to_path_buf(), schedules);
    let task_source: Arc<dyn TaskSource> =
        Arc::new(LocalTaskSource::new(store.clone(), project.id.clone()));
    let deps = Deps {
        store,
        mux: Arc::new(FakeMux::new(false)),
        forge: None,
        task_source,
        notifier: meguri::notify::fake::recording_notifier().0,
        forge_factory: Arc::new(meguri::forge::gh::GhForgeFactory),
        config: Config::default(),
        project,
        open_prs: Default::default(),
    };
    (deps, dir)
}

fn issue_count(forge: &FakeForge) -> usize {
    forge.issues.lock().unwrap().len()
}

#[tokio::test]
async fn github_fires_labeled_issue_and_is_discoverable() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );

    // First sweep only seeds — no backfill of today's earlier occurrences.
    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 0);

    // Once 09:00 has passed, the schedule fires exactly one issue.
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap();
    {
        let issues = forge.issues.lock().unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].has_label(LABEL_READY));
        assert_eq!(issues[0].title, "daily 2026-07-13");
        assert!(
            issues[0]
                .body
                .contains("<!-- meguri:schedule name=daily -->"),
            "body carries the provenance marker"
        );
    }

    // The created issue is what the worker task source discovers.
    let targets = deps.task_source.discover(TaskKind::Work).await.unwrap();
    assert!(targets.iter().any(|t| t.key.number() == 1));
}

#[tokio::test]
async fn github_plan_kind_gets_the_plan_label() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("weekly-plan", "0 9 * * 1", ScheduleKind::Plan, false)],
    );

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // Monday, seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire
    {
        let issues = forge.issues.lock().unwrap();
        assert_eq!(issues.len(), 1);
        assert!(issues[0].has_label(LABEL_PLAN));
    }
    let targets = deps.task_source.discover(TaskKind::Plan).await.unwrap();
    assert!(targets.iter().any(|t| t.key.number() == 1));
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

#[tokio::test]
async fn body_file_is_read_from_the_default_branch_not_working_tree() {
    // body_file is read from the default branch (ADR 0015): the committed
    // content fires, a working-tree-only edit does not.
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-b", "main"]);
    git(dir.path(), &["config", "user.email", "t@example.com"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::create_dir_all(dir.path().join("ops")).unwrap();
    std::fs::write(
        dir.path().join("ops/task.md"),
        "# From a file\nrun the tidy",
    )
    .unwrap();
    git(dir.path(), &["add", "ops/task.md"]);
    git(dir.path(), &["commit", "-m", "add task"]);
    // Uncommitted tamper: must be ignored by the default-branch read.
    std::fs::write(dir.path().join("ops/task.md"), "TAMPERED work-tree only").unwrap();

    let forge = Arc::new(FakeForge::default());
    let mut s = sched("filed", "0 9 * * *", ScheduleKind::Ready, false);
    s.body = None;
    s.body_file = Some("ops/task.md".into());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![s],
    );

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap();
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap();
    let issues = forge.issues.lock().unwrap();
    assert!(
        issues[0].body.contains("run the tidy"),
        "{}",
        issues[0].body
    );
    assert!(
        !issues[0].body.contains("TAMPERED"),
        "working-tree edit must not reach the issue body: {}",
        issues[0].body
    );
    assert!(
        issues[0]
            .body
            .contains("<!-- meguri:schedule name=filed -->")
    );
}

#[tokio::test]
async fn downtime_folds_to_a_single_fire() {
    // allow_overlap so the fold is isolated from the overlap guard.
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("hourly", "0 * * * *", ScheduleKind::Ready, true)],
    );

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T01:30:00Z")).await.unwrap(); // fires (01:00)
    assert_eq!(issue_count(&forge), 1);

    // Down for hours: occurrences 02:00..06:00 all elapsed, but the catch-up
    // folds to ONE fire, not five.
    sweep(&deps, ts("2026-07-13T06:10:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 2);
}

#[tokio::test]
async fn a_newly_added_schedule_does_not_backfill() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );

    // First observed at 09:30 — today's 09:00 already passed and must NOT fire.
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap();
    sweep(&deps, ts("2026-07-13T09:31:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 0);

    // Tomorrow's 09:00 fires normally.
    sweep(&deps, ts("2026-07-14T09:05:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 1);
}

#[tokio::test]
async fn overlap_guard_skips_while_open_and_consumes_the_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire issue #1
    assert_eq!(issue_count(&forge), 1);

    // Day 2's occurrence is due, but issue #1 is still open → skip (no #2).
    sweep(&deps, ts("2026-07-14T09:30:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 1);

    // The skip CONSUMED day 2's occurrence: closing #1 later the same day does
    // not backfill — nothing fires until the next cron hit.
    forge.close_issue(1);
    sweep(&deps, ts("2026-07-14T18:00:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 1);

    // Day 3's occurrence fires, since #1 is now closed.
    sweep(&deps, ts("2026-07-15T09:05:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 2);
}

#[tokio::test]
async fn allow_overlap_fires_every_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, true)],
    );
    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // #1 (still open)
    sweep(&deps, ts("2026-07-14T09:30:00Z")).await.unwrap(); // #2 despite #1 open
    assert_eq!(issue_count(&forge), 2);
}

#[tokio::test]
async fn hot_reload_adds_a_schedule_without_losing_existing_state() {
    // Shared store + forge across two Deps: the second stands in for the
    // config hot-reload that swaps Deps mid-run (issue #73).
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_in_memory().unwrap();
    let forge = Arc::new(FakeForge::default());

    let deps1 = github_deps_on(
        store.clone(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("alpha", "0 9 * * *", ScheduleKind::Ready, false)],
    );
    sweep(&deps1, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed alpha

    // "Reload": a new Deps that adds `beta`, over the SAME store.
    let deps2 = github_deps_on(
        store.clone(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![
            sched("alpha", "0 9 * * *", ScheduleKind::Ready, false),
            sched("beta", "0 9 * * *", ScheduleKind::Ready, false),
        ],
    );
    // At 09:30: alpha's window (seeded at 00:00) contains 09:00 → fires; beta
    // is seen for the first time → seeds, does NOT backfill.
    sweep(&deps2, ts("2026-07-13T09:30:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 1, "only alpha fires; beta just seeds");

    // Close alpha's issue so its overlap guard lets it fire again; next day
    // both fire (alpha again, beta for the first time) — proving beta's state
    // was seeded across the reload and alpha's survived it.
    forge.close_issue(1);
    sweep(&deps2, ts("2026-07-14T09:05:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 3);
}

#[tokio::test]
async fn local_mode_fires_a_work_task_and_dedups() {
    let (deps, _dir) = local_deps(vec![sched(
        "daily",
        "0 9 * * *",
        ScheduleKind::Ready,
        false,
    )]);

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire task
    let tasks = deps.store.list_tasks("proj", true).unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].kind, "work");
    assert_eq!(tasks[0].origin, "schedule:daily");
    assert!(
        tasks[0]
            .body
            .contains("<!-- meguri:schedule name=daily -->")
    );

    // Discoverable by the local worker task source.
    let targets = deps.task_source.discover(TaskKind::Work).await.unwrap();
    assert_eq!(targets.len(), 1);

    // Still-open task blocks day 2's fire (overlap guard).
    sweep(&deps, ts("2026-07-14T09:30:00Z")).await.unwrap();
    assert_eq!(deps.store.list_tasks("proj", true).unwrap().len(), 1);

    // Complete it, and the next occurrence fires again.
    let id = tasks[0].id;
    deps.store.complete_task(id).unwrap();
    sweep(&deps, ts("2026-07-15T09:05:00Z")).await.unwrap();
    assert_eq!(deps.store.list_tasks("proj", true).unwrap().len(), 2);
}

// --- issue #205: schedule anomalies + watched labels reach the notify sink ---

use meguri::config::ProjectNotifyConfig;
use meguri::notify::fake::{FakeGateway, recording_notifier};

/// Github deps whose notifier records to a returned `FakeGateway` (all event
/// tokens enabled), so a test can assert what reached the sink.
fn github_deps_recording(
    forge: Arc<FakeForge>,
    repo_path: std::path::PathBuf,
    schedules: Vec<ScheduleConfig>,
) -> (Deps, Arc<FakeGateway>) {
    let mut deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge,
        repo_path,
        schedules,
    );
    let (notifier, gw) = recording_notifier();
    deps.notifier = notifier;
    (deps, gw)
}

#[tokio::test]
async fn schedule_failure_emits_event_and_notifies() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    // An unparseable cron makes `fire_one` return Err inside the sweep.
    let (deps, gw) = github_deps_recording(
        forge,
        dir.path().to_path_buf(),
        vec![sched("broken", "not a cron", ScheduleKind::Ready, false)],
    );

    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap();

    assert_eq!(deps.store.count_events("schedule.failed").unwrap(), 1);
    let delivered = gw.delivered();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].event, "schedule.failed");
    assert_eq!(delivered[0].dedup_key, "schedule:proj:broken");
}

#[tokio::test]
async fn schedule_skip_notifies_via_overlap_guard() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let (deps, gw) = github_deps_recording(
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire one open issue
    assert_eq!(issue_count(&forge), 1);
    // Next day's occurrence: the first issue is still open → skipped + notify.
    sweep(&deps, ts("2026-07-14T09:30:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 1, "overlap guard held");

    let skips: Vec<_> = gw
        .delivered()
        .into_iter()
        .filter(|n| n.event == "schedule.skipped")
        .collect();
    assert_eq!(skips.len(), 1);
    assert_eq!(skips[0].dedup_key, "schedule:proj:daily");
}

#[tokio::test]
async fn watched_label_on_created_issue_notifies() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let (mut deps, gw) = github_deps_recording(
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );
    // Watch the label the schedule stamps on its issue.
    deps.project.notify = Some(ProjectNotifyConfig {
        labels: vec![LABEL_READY.to_string()],
    });

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire

    let labels: Vec<_> = gw
        .delivered()
        .into_iter()
        .filter(|n| n.event == "label")
        .collect();
    assert_eq!(
        labels.len(),
        1,
        "the created ready issue is a watched label"
    );
    assert!(labels[0].body.contains(LABEL_READY));
}

#[tokio::test]
async fn unwatched_label_does_not_notify() {
    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let (mut deps, gw) = github_deps_recording(
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );
    deps.project.notify = Some(ProjectNotifyConfig {
        labels: vec!["human:todo".to_string()], // not the ready label
    });

    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap();
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap();

    assert!(
        gw.delivered().iter().all(|n| n.event != "label"),
        "no watched label matched"
    );
}

// --- repo-side schedules (issue #222 / ADR 0026) ---------------------------

/// A bare `origin`, a `pusher` clone that publishes commits to it, and a `work`
/// clone whose `origin/main` the resolver reads (a deliberately stale mirror,
/// so a fetch is what makes a freshly-pushed schedule visible).
struct RemoteRepo {
    _origin: tempfile::TempDir,
    pusher: tempfile::TempDir,
    parent: tempfile::TempDir,
}

fn remote_repo() -> RemoteRepo {
    let origin = tempfile::tempdir().unwrap();
    git(origin.path(), &["init", "--bare", "-b", "main"]);
    let url = origin.path().to_str().unwrap().to_string();

    let pusher = tempfile::tempdir().unwrap();
    git(pusher.path(), &["init", "-b", "main"]);
    git(pusher.path(), &["config", "user.email", "t@example.com"]);
    git(pusher.path(), &["config", "user.name", "t"]);
    std::fs::write(pusher.path().join("README.md"), "seed").unwrap();
    git(pusher.path(), &["add", "."]);
    git(pusher.path(), &["commit", "-m", "seed"]);
    git(pusher.path(), &["remote", "add", "origin", &url]);
    git(pusher.path(), &["push", "-u", "origin", "main"]);

    let parent = tempfile::tempdir().unwrap();
    git(parent.path(), &["clone", &url, "work"]);
    RemoteRepo {
        _origin: origin,
        pusher,
        parent,
    }
}

impl RemoteRepo {
    fn work(&self) -> std::path::PathBuf {
        self.parent.path().join("work")
    }

    /// Publish a file on origin's `main`. The `work` clone stays stale until the
    /// resolver fetches.
    fn push_file(&self, rel: &str, content: &str, msg: &str) {
        let p = self.pusher.path().join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        git(self.pusher.path(), &["add", rel]);
        git(self.pusher.path(), &["commit", "-m", msg]);
        git(self.pusher.path(), &["push", "origin", "main"]);
    }

    /// Update the work clone's `origin/main` now (what a healthy resolver fetch
    /// would do), so a later [`break_origin`] leaves the schedule present but
    /// only in the *stale* ref.
    fn fetch_work(&self) {
        git(&self.work(), &["fetch", "origin", "main"]);
    }

    /// Point the work clone's origin at a nonexistent path so the resolver's
    /// freshness fetch fails (the fail-closed abstain path).
    fn break_origin(&self) {
        git(
            &self.work(),
            &[
                "remote",
                "set-url",
                "origin",
                "/nonexistent/definitely-not-a-repo.git",
            ],
        );
    }
}

#[tokio::test]
async fn repo_only_schedule_is_fetched_seeded_then_fires() {
    // 芯4/芯5 + f1: a stale managed clone with no host schedules. The schedule
    // lives only in the remote default branch's meguri.toml. The resolver's
    // fetch makes it visible; the first sweep only seeds (no backfill); the next
    // window fires.
    let repo = remote_repo();
    repo.push_file(
        "meguri.toml",
        "[[schedules]]\nname = \"repo-daily\"\ncron = \"0 9 * * *\"\ntitle = \"repo {{date}}\"\nbody = \"do it\"\n",
        "add repo schedule",
    );
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        repo.work(),
        vec![], // no host schedules — repo-only
    );

    // sweep-1: fetch + observe + seed, no fire (no-backfill).
    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 0);

    // sweep-2 after 09:00: fires exactly one issue from the repo schedule.
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap();
    let issues = forge.issues.lock().unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].title, "repo 2026-07-13");
    assert!(issues[0].has_label(LABEL_READY));
    assert!(
        issues[0]
            .body
            .contains("<!-- meguri:schedule name=repo-daily -->")
    );
}

#[tokio::test]
async fn host_schedule_shadows_a_repo_schedule_of_the_same_name() {
    // D5: a host and a repo schedule share a name → host wins, repo dropped.
    // Firing once (the host one) proves there is no double-fire on the shared
    // sqlite key.
    let repo = remote_repo();
    repo.push_file(
        "meguri.toml",
        "[[schedules]]\nname = \"daily\"\ncron = \"0 9 * * *\"\ntitle = \"REPO {{date}}\"\nbody = \"repo\"\n",
        "add repo schedule",
    );
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        repo.work(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );
    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire
    let issues = forge.issues.lock().unwrap();
    assert_eq!(issues.len(), 1, "exactly one fire (host wins, no double)");
    // The host schedule's title ("daily {{date}}"), not the repo's "REPO ...".
    assert_eq!(issues[0].title, "daily 2026-07-13");
}

#[tokio::test]
async fn broken_repo_meguri_toml_still_lets_host_schedules_fire() {
    // D6 / f2: a host-only key in the repo meguri.toml is a collection error —
    // the whole repo set is dropped, but the host schedule fires unaffected.
    let repo = remote_repo();
    repo.push_file(
        "meguri.toml",
        "repo_slug = \"me/x\"\n[[schedules]]\nname = \"repo-daily\"\ncron = \"0 9 * * *\"\ntitle = \"t\"\nbody = \"b\"\n",
        "add invalid repo config",
    );
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        repo.work(),
        vec![sched("host-daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );
    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire
    let issues = forge.issues.lock().unwrap();
    assert_eq!(issues.len(), 1, "only the host schedule fired");
    assert_eq!(issues[0].title, "host-daily 2026-07-13");
}

#[tokio::test]
async fn fetch_failure_abstains_repo_layer_but_host_still_fires() {
    // f2(a) / ADR 0026 fail-closed: the repo schedule IS present in the (stale)
    // origin/main, but the freshness fetch fails — so meguri abstains and does
    // NOT fire it (never fire a possibly-deleted schedule from a stale ref),
    // while the host schedule fires unaffected.
    let repo = remote_repo();
    repo.push_file(
        "meguri.toml",
        "[[schedules]]\nname = \"repo-daily\"\ncron = \"0 9 * * *\"\ntitle = \"repo\"\nbody = \"r\"\n",
        "repo schedule",
    );
    repo.fetch_work(); // work's origin/main now has the repo schedule …
    repo.break_origin(); // … but future fetches fail.

    let store = Store::open_in_memory().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        store.clone(),
        forge.clone(),
        repo.work(),
        vec![sched("host-daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );
    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // fire

    let issues = forge.issues.lock().unwrap();
    assert_eq!(issues.len(), 1, "only the host schedule fired");
    assert_eq!(issues[0].title, "host-daily 2026-07-13");
    assert!(
        store.count_events("schedule.repo_unavailable").unwrap() >= 1,
        "the abstain is surfaced, not silent"
    );
}

#[tokio::test]
async fn invalid_cron_in_repo_drops_only_that_entry() {
    // f2(d) / D6: a single bad-cron entry is dropped; the sibling repo schedule
    // still fires (a per-schedule error, not a whole-set collection error).
    let repo = remote_repo();
    repo.push_file(
        "meguri.toml",
        "[[schedules]]\nname = \"good\"\ncron = \"0 9 * * *\"\ntitle = \"good {{date}}\"\nbody = \"g\"\n\
         [[schedules]]\nname = \"bad\"\ncron = \"not a cron\"\ntitle = \"bad\"\nbody = \"b\"\n",
        "one good one bad",
    );
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        repo.work(),
        vec![],
    );
    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap(); // seed good
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap(); // good fires

    let issues = forge.issues.lock().unwrap();
    assert_eq!(issues.len(), 1, "only the valid entry fired");
    assert_eq!(issues[0].title, "good 2026-07-13");
}

#[tokio::test]
async fn edge_triggered_diagnostic_emits_once_and_again_on_clear() {
    // f2(e) / D12: a persisting condition (host shadows a repo schedule) emits a
    // single event across steady ticks, and its resolution emits one clear.
    let repo = remote_repo();
    repo.push_file(
        "meguri.toml",
        "[[schedules]]\nname = \"daily\"\ncron = \"0 9 * * *\"\ntitle = \"REPO\"\nbody = \"r\"\n",
        "shadowing repo schedule",
    );
    let store = Store::open_in_memory().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        store.clone(),
        forge.clone(),
        repo.work(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );
    // A shared memory across ticks (the watch loop keeps one).
    let mut diag = DiagMemory::new();
    for t in ["00:00:00", "00:05:00", "00:10:00"] {
        sweep_impl(&deps, ts(&format!("2026-07-13T{t}Z")), &mut diag)
            .await
            .unwrap();
    }
    assert_eq!(
        store.count_events("schedule.shadowed").unwrap(),
        1,
        "steady state does not re-emit every tick"
    );

    // Resolve the shadow: the repo no longer defines `daily`.
    repo.push_file(
        "meguri.toml",
        "[[schedules]]\nname = \"other\"\ncron = \"0 9 * * *\"\ntitle = \"o\"\nbody = \"o\"\n",
        "drop the shadow",
    );
    sweep_impl(&deps, ts("2026-07-13T00:15:00Z"), &mut diag)
        .await
        .unwrap();
    assert_eq!(
        store.count_events("schedule.shadowed").unwrap(),
        1,
        "no new shadow event after it cleared"
    );
    assert_eq!(
        store.count_events("schedule.diagnostic_cleared").unwrap(),
        1,
        "the resolution emits exactly one clear"
    );
}

#[tokio::test]
async fn crash_between_enqueue_and_record_refires_at_least_once() {
    // f2(f): the at-least-once boundary. A fire that created its item but was
    // killed before `record_schedule_fire` leaves the window un-advanced and no
    // key saved — so the next sweep re-fires (a duplicate), and the overlap
    // guard cannot prevent it (there is no saved key to check).
    use meguri::forge::Forge;

    let dir = tempfile::tempdir().unwrap();
    let forge = Arc::new(FakeForge::default());
    let deps = github_deps_on(
        Store::open_in_memory().unwrap(),
        forge.clone(),
        dir.path().to_path_buf(),
        vec![sched("daily", "0 9 * * *", ScheduleKind::Ready, false)],
    );

    // Seed only (window bottom at 00:00, no key).
    sweep(&deps, ts("2026-07-13T00:00:00Z")).await.unwrap();
    // Simulate the enqueue that happened just before the crash: the item exists…
    forge
        .create_issue("crashed daily", "body", &[LABEL_READY])
        .await
        .unwrap();
    assert_eq!(issue_count(&forge), 1);
    // …but the window was never advanced. The next sweep re-fires the window:
    sweep(&deps, ts("2026-07-13T09:30:00Z")).await.unwrap();
    assert_eq!(
        issue_count(&forge),
        2,
        "at-least-once: the un-recorded window re-fires (guard is blind, no saved key)"
    );

    // Once recorded, the same window does not fire again (the non-crash path).
    sweep(&deps, ts("2026-07-13T09:31:00Z")).await.unwrap();
    assert_eq!(issue_count(&forge), 2, "a recorded window is not re-fired");
}
