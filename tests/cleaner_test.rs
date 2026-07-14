//! End-to-end cleaner-loop tests with FakeMux + FakeForge and a real local
//! git origin: a sweep of the default branch head lands in a single
//! `meguri:clean-report` issue — created on the first pass, rewritten as a
//! snapshot afterwards — and nothing else on the forge or origin is touched.
//! A scripted "agent" plays the pane side (same protocol as reviewer_test).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{CleanConfig, Config, ProjectConfig};
use meguri::engine::cleaner::{
    self, CleanerLoop, MARKER_HEAD_NONE, REPORT_FILE, clean_marker, parse_clean_marker, run_cleaner,
};
use meguri::engine::{Deps, Loop, WorkerOutcome};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    Forge, LABEL_CLEAN_REPORT, LABEL_HOLD, LABEL_IMPLEMENTING, LABEL_NEEDS_HUMAN, LABEL_READY,
    LABEL_SPECCING, LABEL_WORKING,
};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};

/// A bystander issue the cleaner must never touch (write-boundary checks).
const BYSTANDER: i64 = 50;

fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

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

/// git with explicit author/committer dates (for stale-branch seeding).
async fn git_dated(dir: &Path, args: &[&str], epoch: u64) {
    let date = format!("{epoch} +0000");
    let status = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", &date)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

struct TestEnv {
    deps: Deps,
    forge: Arc<FakeForge>,
    head_sha: String,
    #[allow(dead_code)]
    root: tempfile::TempDir,
    worktree_root: PathBuf,
    origin: PathBuf,
    clone: PathBuf,
}

async fn setup() -> TestEnv {
    setup_with_clean(CleanConfig::default()).await
}

async fn setup_with_clean(clean: CleanConfig) -> TestEnv {
    let root = tempfile::tempdir().unwrap();
    let (origin, clone) = init_origin_and_clone(root.path()).await;
    let head_sha = run_git(&clone, &["rev-parse", "HEAD"]).await.unwrap();
    let worktree_root = root.path().join("worktrees");

    let forge = Arc::new(FakeForge::with_issue(
        BYSTANDER,
        "bystander",
        "must stay untouched",
        &[LABEL_READY],
    ));

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600; // scripted agent: no nudging wanted
    config.limits.result_grace_secs = 1; // FakeMux always reads Working; don't linger
    config.clean = clean;
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone.clone(),
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
        autonomy: None,
    };

    let deps = Deps::with_label_source(
        Store::open_in_memory().unwrap(),
        Arc::new(FakeMux::new(false)),
        forge.clone(),
        config,
        project,
    );
    TestEnv {
        deps,
        forge,
        head_sha,
        root,
        worktree_root,
        origin,
        clone,
    }
}

fn create_cleaner_run(env: &TestEnv, issue: i64) -> meguri::store::RunRecord {
    env.deps
        .store
        .create_run_for_loop("proj", cleaner::KIND, issue, cleaner::REPORT_TITLE)
        .unwrap()
}

fn find_worktree(worktree_root: &Path) -> Option<PathBuf> {
    let proj = worktree_root.join("proj");
    let entries = std::fs::read_dir(proj).ok()?;
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir())
}

fn pending_turn(worktree: &Path) -> Option<String> {
    // A prompt file whose turn id doesn't yet have a matching result.
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

fn write_report(worktree: &Path, findings_json: &str) {
    std::fs::write(
        worktree.join(REPORT_FILE),
        format!(r#"{{"findings": {findings_json}}}"#),
    )
    .unwrap();
}

fn write_result(worktree: &Path, turn_id: &str, status: &str) {
    let result = serde_json::json!({
        "turn_id": turn_id, "status": status, "summary": "scripted sweep",
    });
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
            if let Some(turn_id) = pending_turn(&wt)
                && seen.insert(turn_id.clone())
            {
                action(seen.len() as u32, &wt, &turn_id);
            }
        }
        seen.len() as u32
    })
}

/// The project's report issue as the forge sees it (label-selected).
async fn report_issue(env: &TestEnv) -> Option<meguri::forge::Issue> {
    env.forge
        .list_issues_with_label(LABEL_CLEAN_REPORT)
        .await
        .unwrap()
        .into_iter()
        .min_by_key(|i| i.number)
}

async fn run_to_outcome(env: &TestEnv, run_id: &str) -> WorkerOutcome {
    tokio::time::timeout(Duration::from_secs(60), run_cleaner(&env.deps, run_id))
        .await
        .expect("cleaner timed out")
        .unwrap()
}

const SWEEP_FINDING: &str = r#"[{"category": "spec-drift", "file": "docs/specs/issue-9.md",
    "line": 4, "note": "spec promises a flag the code removed", "confidence": "high"},
   {"category": "todo", "file": "src/main.rs", "line": null,
    "note": "TODO(2019) still here", "confidence": "low"}]"#;

#[tokio::test(flavor = "multi_thread")]
async fn first_sweep_creates_report_issue_and_touches_nothing_else() {
    let env = setup().await;

    // Discovery: no report issue yet → the synthetic target 0.
    let targets = CleanerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![0]
    );

    let origin_refs_before = run_git(&env.origin, &["for-each-ref"]).await.unwrap();
    let run = create_cleaner_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(wt, SWEEP_FINDING);
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();

    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Succeeded);
    assert_eq!(record.step, cleaner::STEP_SETTLE);
    assert_eq!(record.loop_kind, cleaner::KIND);

    // The report issue exists, labeled, findings and marker in the body.
    let report = report_issue(&env).await.expect("report issue created");
    assert_eq!(report.title, cleaner::REPORT_TITLE);
    assert!(report.has_label(LABEL_CLEAN_REPORT));
    let marker = parse_clean_marker(&report.body).expect("marker present");
    assert_eq!(marker.head, env.head_sha);
    assert!(marker.scanned > 0);
    assert!(report.body.contains("docs/specs/issue-9.md:4"));
    assert!(report.body.contains("spec promises a flag"));
    assert!(report.body.contains("TODO(2019)"));

    // Write boundary: origin refs unchanged (no push, no branches), no PRs,
    // no comments anywhere, and the bystander issue is untouched.
    let origin_refs_after = run_git(&env.origin, &["for-each-ref"]).await.unwrap();
    assert_eq!(origin_refs_before, origin_refs_after);
    assert!(env.forge.prs().is_empty());
    assert!(env.forge.comments_of(BYSTANDER).is_empty());
    assert!(env.forge.pr_comments_of(BYSTANDER).is_empty());
    let bystander = env.forge.get_issue(BYSTANDER).await.unwrap();
    assert_eq!(bystander.body, "must stay untouched");
    assert_eq!(bystander.labels, vec![LABEL_READY.to_string()]);
    assert!(env.forge.comments_of(report.number).is_empty());

    // D9: the cleaner reclaims its own detached worktree after settling.
    assert!(
        find_worktree(&env.worktree_root).is_none(),
        "worktree must be removed after settle"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rediscovery_respects_head_marker_and_interval() {
    let env = setup().await;

    // First sweep (seeded directly as a report issue, as settle writes it).
    let body = format!(
        "{}\nreport with docs/specs/issue-9.md finding",
        clean_marker(&env.head_sha, epoch_now())
    );
    let report = env
        .forge
        .create_issue(cleaner::REPORT_TITLE, &body, &[LABEL_CLEAN_REPORT])
        .await
        .unwrap();

    // Same head: nothing to do.
    assert!(CleanerLoop.discover(&env.deps).await.unwrap().is_empty());

    // Head moves, but within the interval: still nothing.
    run_git(&env.clone, &["commit", "--allow-empty", "-m", "advance"])
        .await
        .unwrap();
    run_git(&env.clone, &["push", "origin", "main"])
        .await
        .unwrap();
    let new_head = run_git(&env.clone, &["rev-parse", "HEAD"]).await.unwrap();
    assert!(CleanerLoop.discover(&env.deps).await.unwrap().is_empty());

    // Interval elapsed (seed an old `scanned`): the report issue is due.
    let stale_scanned = epoch_now() - 25 * 3600;
    env.forge
        .update_issue_body(
            report,
            &format!(
                "{}\nreport with docs/specs/issue-9.md finding",
                clean_marker(&env.head_sha, stale_scanned)
            ),
        )
        .await
        .unwrap();
    let targets = CleanerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![report]
    );

    // The sweep rewrites the body: new findings in, previous items gone.
    let run = create_cleaner_run(&env, report);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"category": "convention", "file": "src/new.rs", "line": 1,
                "note": "fresh finding", "confidence": "medium"}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let updated = env.forge.get_issue(report).await.unwrap();
    let marker = parse_clean_marker(&updated.body).unwrap();
    assert_eq!(marker.head, new_head);
    assert!(updated.body.contains("fresh finding"));
    assert!(
        !updated.body.contains("docs/specs/issue-9.md"),
        "snapshot, not history: {}",
        updated.body
    );

    // And the new head is now settled: no further target.
    assert!(CleanerLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn hold_on_the_report_issue_stops_the_loop() {
    let env = setup().await;
    // Even a sweep that is otherwise overdue is skipped under hold.
    let body = format!(
        "{}\nold report",
        clean_marker("some-old-head", epoch_now() - 48 * 3600)
    );
    let report = env
        .forge
        .create_issue(cleaner::REPORT_TITLE, &body, &[LABEL_CLEAN_REPORT])
        .await
        .unwrap();
    env.forge.add_label(report, LABEL_HOLD).await.unwrap();

    assert!(CleanerLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_agent_skips_quietly_and_paces_retries() {
    let env = setup().await;
    let run = create_cleaner_run(&env, 0);

    // The agent claims success but never writes the report — the corrective
    // turn fails the same way, and the run gives up.
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();

    assert!(
        matches!(outcome, WorkerOutcome::Skipped(_)),
        "expected quiet skip, got {outcome:?}"
    );
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Skipped);

    // Quiet: no needs-human, no comments anywhere.
    let report = report_issue(&env).await.expect("initializing issue exists");
    assert!(!report.has_label(LABEL_NEEDS_HUMAN));
    assert!(env.forge.comments_of(report.number).is_empty());
    assert!(env.forge.comments_of(BYSTANDER).is_empty());

    // The marker records only the attempt time, not the head — so the head
    // is retried, but no sooner than the interval.
    let marker = parse_clean_marker(&report.body).unwrap();
    assert_eq!(marker.head, MARKER_HEAD_NONE);
    assert!(marker.scanned > 0);
    assert!(
        CleanerLoop.discover(&env.deps).await.unwrap().is_empty(),
        "retry must wait for the interval"
    );

    // The worktree is reclaimed on the skip path too (D9).
    assert!(find_worktree(&env.worktree_root).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn ignore_list_silences_false_positives() {
    let env = setup_with_clean(CleanConfig {
        ignore: vec!["docs/specs/issue-9.md".into(), "TODO(2019)".into()],
        ..CleanConfig::default()
    })
    .await;
    let run = create_cleaner_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(wt, SWEEP_FINDING);
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let report = report_issue(&env).await.unwrap();
    assert!(!report.body.contains("docs/specs/issue-9.md"));
    assert!(!report.body.contains("TODO(2019)"));
    assert!(parse_clean_marker(&report.body).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn machine_checks_report_stale_branches_and_orphan_working() {
    let env = setup().await;

    // A merged leftover (tip == main head, age 0) and an abandoned branch
    // (last commit 40 days old, not merged).
    run_git(
        &env.clone,
        &["push", "origin", "main:refs/heads/merged-leftover"],
    )
    .await
    .unwrap();
    run_git(&env.clone, &["checkout", "-b", "abandoned-work"])
        .await
        .unwrap();
    git_dated(
        &env.clone,
        &["commit", "--allow-empty", "-m", "old work"],
        epoch_now() - 40 * 86_400,
    )
    .await;
    run_git(&env.clone, &["push", "-u", "origin", "abandoned-work"])
        .await
        .unwrap();
    run_git(&env.clone, &["checkout", "main"]).await.unwrap();

    // A fresh, unmerged branch must NOT be reported...
    run_git(&env.clone, &["checkout", "-b", "fresh-work"])
        .await
        .unwrap();
    run_git(&env.clone, &["commit", "--allow-empty", "-m", "wip"])
        .await
        .unwrap();
    run_git(&env.clone, &["push", "-u", "origin", "fresh-work"])
        .await
        .unwrap();
    run_git(&env.clone, &["checkout", "main"]).await.unwrap();

    // Orphaned working label (no active run) vs. a legitimately claimed one.
    // Both carry a phase label so this exercises only the orphan-working check,
    // not the phase-label-anomaly one (a bare `working` with no phase is itself
    // an anomaly — covered by its own test).
    env.forge.issues.lock().unwrap().push(meguri::forge::Issue {
        number: 7,
        title: "orphaned claim".into(),
        body: String::new(),
        labels: vec![LABEL_IMPLEMENTING.to_string(), LABEL_WORKING.to_string()],
    });
    env.forge.issues.lock().unwrap().push(meguri::forge::Issue {
        number: 8,
        title: "active claim".into(),
        body: String::new(),
        labels: vec![LABEL_IMPLEMENTING.to_string(), LABEL_WORKING.to_string()],
    });
    env.deps
        .store
        .create_run_for_loop("proj", "worker", 8, "active claim")
        .unwrap(); // queued = active

    let run = create_cleaner_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(wt, "[]");
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let report = report_issue(&env).await.unwrap();
    assert!(
        report
            .body
            .contains("`merged-leftover` — merged into the default branch"),
        "{}",
        report.body
    );
    assert!(
        report
            .body
            .contains("`abandoned-work` — last commit 40 days ago")
    );
    assert!(!report.body.contains("fresh-work"));
    assert!(report.body.contains("issue #7 — orphaned claim"));
    assert!(!report.body.contains("#8"), "{}", report.body);

    // Machine checks read the forge and git but never write: the labels on
    // the orphan candidates are exactly as seeded.
    assert_eq!(
        env.forge.labels_of(7),
        vec![LABEL_IMPLEMENTING.to_string(), LABEL_WORKING.to_string()]
    );
    assert_eq!(
        env.forge.labels_of(8),
        vec![LABEL_IMPLEMENTING.to_string(), LABEL_WORKING.to_string()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn machine_checks_report_phase_label_anomalies() {
    let env = setup().await;

    // Anomaly 1 — two phase labels (a swap that dropped the old one).
    env.forge.issues.lock().unwrap().push(meguri::forge::Issue {
        number: 20,
        title: "double phase".into(),
        body: String::new(),
        labels: vec![LABEL_SPECCING.to_string(), LABEL_IMPLEMENTING.to_string()],
    });
    // Anomaly 2 — a ball label with no phase label (engaged, phase missing).
    env.forge.issues.lock().unwrap().push(meguri::forge::Issue {
        number: 21,
        title: "ball no phase".into(),
        body: String::new(),
        labels: vec![LABEL_WORKING.to_string()],
    });
    // Healthy — exactly one phase label (plus a ball) is the invariant, so it
    // must NOT be flagged.
    env.forge.issues.lock().unwrap().push(meguri::forge::Issue {
        number: 22,
        title: "healthy implementing".into(),
        body: String::new(),
        labels: vec![
            LABEL_IMPLEMENTING.to_string(),
            LABEL_NEEDS_HUMAN.to_string(),
        ],
    });
    // Untriaged — no labels at all is legitimately unlabeled, not an anomaly.
    env.forge.issues.lock().unwrap().push(meguri::forge::Issue {
        number: 23,
        title: "untriaged".into(),
        body: String::new(),
        labels: vec![],
    });

    let run = create_cleaner_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(wt, "[]");
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let report = report_issue(&env).await.unwrap();
    assert!(
        report.body.contains("### Phase-label anomalies"),
        "{}",
        report.body
    );
    assert!(
        report.body.contains("issue #20 — carries 2 phase labels"),
        "{}",
        report.body
    );
    assert!(
        report
            .body
            .contains("issue #21 — has a ball label but no phase label"),
        "{}",
        report.body
    );
    // Healthy (one phase), untriaged (no labels), and the ready bystander must
    // never be flagged.
    assert!(!report.body.contains("#22"), "{}", report.body);
    assert!(!report.body.contains("#23"), "{}", report.body);
    assert!(
        !report.body.contains(&format!("#{BYSTANDER}")),
        "{}",
        report.body
    );

    // Report-only: the flagged issues' labels are untouched.
    assert_eq!(
        env.forge.labels_of(20),
        vec![LABEL_SPECCING.to_string(), LABEL_IMPLEMENTING.to_string()]
    );
    assert_eq!(env.forge.labels_of(21), vec![LABEL_WORKING.to_string()]);
}
