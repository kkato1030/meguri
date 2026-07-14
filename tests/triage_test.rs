//! End-to-end triage-loop tests with FakeMux + FakeForge and a real local git
//! origin: a read-only sweep of the untriaged open issues lands recommendations
//! in a single `meguri:triage-report` issue — created on the first pass,
//! rewritten as a snapshot afterwards — and nothing else on the forge or origin
//! is touched (v0 never labels or comments on the triaged issues). A scripted
//! "agent" plays the pane side (same protocol as cleaner_test).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, LaunchMode, ProjectConfig, TriageAction, TriageConfig, TriageMode};
use meguri::engine::triage::{
    self, MARKER_HEAD_NONE, REPORT_FILE, TriageLoop, parse_triage_marker, run_triage, triage_marker,
};
use meguri::engine::{Deps, Loop, WorkerOutcome};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    Forge, Issue, LABEL_HOLD, LABEL_NEEDS_HUMAN, LABEL_PLAN, LABEL_READY, LABEL_TRIAGE_PLAN,
    LABEL_TRIAGE_READY, LABEL_TRIAGE_REPORT,
};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};

/// An unlabeled open issue: the one thing triage should recommend on.
const CANDIDATE: i64 = 60;
/// A `meguri:ready` issue (already engaged) triage must ignore and never touch.
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

/// Report-mode triage with the default interval.
async fn setup() -> TestEnv {
    setup_with_triage(TriageConfig {
        mode: TriageMode::Report,
        ..TriageConfig::default()
    })
    .await
}

async fn setup_with_triage(triage: TriageConfig) -> TestEnv {
    let root = tempfile::tempdir().unwrap();
    let (origin, clone) = init_origin_and_clone(root.path()).await;
    let head_sha = run_git(&clone, &["rev-parse", "HEAD"]).await.unwrap();
    let worktree_root = root.path().join("worktrees");

    // A ready bystander (engaged, excluded) and an unlabeled candidate.
    let forge = Arc::new(FakeForge::default());
    forge.add_issue(
        BYSTANDER,
        "bystander",
        "must stay untouched",
        &[LABEL_READY],
    );
    forge.add_issue(CANDIDATE, "add caching", "we should cache X", &[]);

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600; // scripted agent: no nudging wanted
    config.limits.result_grace_secs = 1; // FakeMux always reads Working; don't linger
    config.triage = triage;
    // Play the scripted agent through FakeMux (pane protocol); pin triage to
    // pane so it doesn't fall through to its recommended `direct` mode, which
    // would spawn a real `claude` subprocess instead of the fake (issue #169).
    config
        .launch
        .roles
        .insert("triage".into(), LaunchMode::Pane);
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
        triage: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
        autonomy: None,
        cadence: Vec::new(),
        prompts: Default::default(),
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

fn create_triage_run(env: &TestEnv, issue: i64) -> meguri::store::RunRecord {
    env.deps
        .store
        .create_run_for_loop("proj", triage::KIND, issue, triage::REPORT_TITLE)
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

fn write_report(worktree: &Path, recs_json: &str) {
    std::fs::write(
        worktree.join(REPORT_FILE),
        format!(r#"{{"recommendations": {recs_json}}}"#),
    )
    .unwrap();
}

fn write_result(worktree: &Path, turn_id: &str, status: &str) {
    let result = serde_json::json!({
        "turn_id": turn_id, "status": status, "summary": "scripted triage",
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
async fn report_issue(env: &TestEnv) -> Option<Issue> {
    env.forge
        .list_issues_with_label(LABEL_TRIAGE_REPORT)
        .await
        .unwrap()
        .into_iter()
        .min_by_key(|i| i.number)
}

async fn run_to_outcome(env: &TestEnv, run_id: &str) -> WorkerOutcome {
    tokio::time::timeout(Duration::from_secs(60), run_triage(&env.deps, run_id))
        .await
        .expect("triage timed out")
        .unwrap()
}

const READY_REC: &str = r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.8,
    "estimated_complexity": "small", "rationale": "clear small change", "missing_info": null}]"#;

#[tokio::test(flavor = "multi_thread")]
async fn off_mode_discovers_nothing() {
    // The opt-in default: triage stays fully quiet until turned on.
    let env = setup_with_triage(TriageConfig::default()).await;
    assert_eq!(env.deps.config.triage.mode, TriageMode::Off);
    assert!(TriageLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn first_sweep_creates_report_issue_and_touches_nothing_else() {
    let env = setup().await;

    // Discovery: no report issue yet → the synthetic target 0.
    let targets = TriageLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![0]
    );

    let origin_refs_before = run_git(&env.origin, &["for-each-ref"]).await.unwrap();
    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(wt, READY_REC);
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
    assert_eq!(record.step, triage::STEP_SETTLE);
    assert_eq!(record.loop_kind, triage::KIND);

    // The report issue exists, labeled, recommendation and marker in the body.
    let report = report_issue(&env).await.expect("report issue created");
    assert_eq!(report.title, triage::REPORT_TITLE);
    assert!(report.has_label(LABEL_TRIAGE_REPORT));
    let marker = parse_triage_marker(&report.body).expect("marker present");
    assert_eq!(marker.head, env.head_sha);
    assert!(marker.scanned > 0);
    // max_issue records the largest non-report open issue (the candidate), so
    // the report issue's own creation does not re-trigger the next sweep.
    assert_eq!(marker.max_issue, CANDIDATE);
    assert!(
        report.body.contains("| #60 | ready | 0.80 | small |"),
        "{}",
        report.body
    );

    // Write boundary: origin refs unchanged (no push, no branches), no PRs, no
    // comments anywhere, and neither the bystander nor the triaged candidate is
    // touched (v0 never labels/comments the issues it triages).
    let origin_refs_after = run_git(&env.origin, &["for-each-ref"]).await.unwrap();
    assert_eq!(origin_refs_before, origin_refs_after);
    assert!(env.forge.prs().is_empty());
    assert!(env.forge.comments_of(BYSTANDER).is_empty());
    assert!(env.forge.comments_of(CANDIDATE).is_empty());
    let bystander = env.forge.get_issue(BYSTANDER).await.unwrap();
    assert_eq!(bystander.body, "must stay untouched");
    assert_eq!(bystander.labels, vec![LABEL_READY.to_string()]);
    let candidate = env.forge.get_issue(CANDIDATE).await.unwrap();
    assert_eq!(candidate.body, "we should cache X");
    assert!(candidate.labels.is_empty());
    assert!(env.forge.comments_of(report.number).is_empty());

    // The triage loop reclaims its own detached worktree after settling.
    assert!(
        find_worktree(&env.worktree_root).is_none(),
        "worktree must be removed after settle"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn rediscovery_respects_head_marker_and_interval() {
    let env = setup().await;

    // First sweep (seeded directly as a report issue, as settle writes it).
    // max_issue = the candidate; the report issue takes the next number.
    let body = format!(
        "{}\nrecommendation for #60",
        triage_marker(&env.head_sha, epoch_now(), CANDIDATE, false)
    );
    let report = env
        .forge
        .create_issue(triage::REPORT_TITLE, &body, &[LABEL_TRIAGE_REPORT])
        .await
        .unwrap();

    // Same head, no new issue: nothing to do.
    assert!(TriageLoop.discover(&env.deps).await.unwrap().is_empty());

    // Head moves, but within the interval: still nothing.
    run_git(&env.clone, &["commit", "--allow-empty", "-m", "advance"])
        .await
        .unwrap();
    run_git(&env.clone, &["push", "origin", "main"])
        .await
        .unwrap();
    let new_head = run_git(&env.clone, &["rev-parse", "HEAD"]).await.unwrap();
    assert!(TriageLoop.discover(&env.deps).await.unwrap().is_empty());

    // Interval elapsed (seed an old `scanned`): the report issue is due.
    let stale_scanned = epoch_now() - 7 * 3600;
    env.forge
        .update_issue_body(
            report,
            &format!(
                "{}\nrecommendation for #60",
                triage_marker(&env.head_sha, stale_scanned, CANDIDATE, false)
            ),
        )
        .await
        .unwrap();
    let targets = TriageLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![report]
    );

    // The sweep rewrites the body: new recommendation in, previous line gone.
    let run = create_triage_run(&env, report);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "plan", "confidence": 0.4,
                "estimated_complexity": "large", "rationale": "actually needs a spec",
                "missing_info": "which backend?"}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let updated = env.forge.get_issue(report).await.unwrap();
    let marker = parse_triage_marker(&updated.body).unwrap();
    assert_eq!(marker.head, new_head);
    assert!(updated.body.contains("| #60 | plan | 0.40 | large |"));
    assert!(updated.body.contains("⚠️ 要確認: which backend?"));

    // And the new head is now settled: no further target.
    assert!(TriageLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn new_issue_triggers_rescan_even_with_a_still_head() {
    let env = setup().await;

    // A settled report: same head, max_issue = the candidate, but the scan is
    // old enough that only a *change* is missing to re-trigger.
    let stale_scanned = epoch_now() - 7 * 3600;
    let body = format!(
        "{}\nrecommendation for #60",
        triage_marker(&env.head_sha, stale_scanned, CANDIDATE, false)
    );
    env.forge
        .create_issue(triage::REPORT_TITLE, &body, &[LABEL_TRIAGE_REPORT])
        .await
        .unwrap();

    // Head is still and no issue is above max_issue → no rescan, however old.
    // (The report issue itself carries a `meguri:` label, so it is not a
    // candidate and does not count as a new untriaged issue.)
    assert!(TriageLoop.discover(&env.deps).await.unwrap().is_empty());

    // A brand-new unlabeled issue appears above max_issue → rescan, head still.
    env.forge.add_issue(70, "new bug", "just filed", &[]);
    assert!(!TriageLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn hold_on_the_report_issue_stops_the_loop() {
    let env = setup().await;
    // Even a sweep that is otherwise overdue is skipped under hold.
    let body = format!(
        "{}\nold report",
        triage_marker("some-old-head", epoch_now() - 48 * 3600, 0, false)
    );
    let report = env
        .forge
        .create_issue(triage::REPORT_TITLE, &body, &[LABEL_TRIAGE_REPORT])
        .await
        .unwrap();
    env.forge.add_label(report, LABEL_HOLD).await.unwrap();

    assert!(TriageLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_agent_skips_quietly_and_paces_retries() {
    let env = setup().await;
    let run = create_triage_run(&env, 0);

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

    // Quiet: an initializing report issue exists, but no needs-human, no
    // comments anywhere, and the triaged issues are untouched.
    let report = report_issue(&env).await.expect("initializing issue exists");
    assert!(env.forge.comments_of(report.number).is_empty());
    assert!(env.forge.comments_of(CANDIDATE).is_empty());
    assert!(env.forge.comments_of(BYSTANDER).is_empty());
    assert!(env.forge.prs().is_empty());

    // The marker records only the attempt time, not the head — so the head is
    // retried, but no sooner than the interval.
    let marker = parse_triage_marker(&report.body).unwrap();
    assert_eq!(marker.head, MARKER_HEAD_NONE);
    assert_eq!(marker.max_issue, 0);
    assert!(marker.scanned > 0);
    assert!(
        TriageLoop.discover(&env.deps).await.unwrap().is_empty(),
        "retry must wait for the interval"
    );

    // The worktree is reclaimed on the skip path too.
    assert!(find_worktree(&env.worktree_root).is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn ignore_list_silences_recommendations() {
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Report,
        ignore: vec!["#60".into()],
        ..TriageConfig::default()
    })
    .await;
    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(wt, READY_REC);
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    let report = report_issue(&env).await.unwrap();
    assert!(!report.body.contains("| #60 |"));
    assert!(report.body.contains("_No open issues to triage._"));
    assert!(parse_triage_marker(&report.body).is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn incomplete_report_is_corrected_then_succeeds() {
    let env = setup().await;
    // A second unlabeled candidate, so an under-report can drop one.
    env.forge.add_issue(61, "second", "also untriaged", &[]);

    let run = create_triage_run(&env, 0);
    // First turn covers only #60 (drops #61 → coverage correction); the
    // corrective turn covers both and the run then succeeds.
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |n, wt, turn_id| {
        if n == 1 {
            write_report(wt, READY_REC);
        } else {
            write_report(
                wt,
                r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.8,
                    "estimated_complexity": "small", "rationale": "clear", "missing_info": null},
                   {"issue": 61, "recommendation": "plan", "confidence": 0.5,
                    "estimated_complexity": "large", "rationale": "vague", "missing_info": null}]"#,
            );
        }
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    // The run only reaches Succeeded after settle, which follows the verified
    // corrective turn — so #61 (written *only* on that turn) being in the final
    // report is proof the coverage correction ran.
    let report = report_issue(&env).await.unwrap();
    assert!(report.body.contains("| #60 | ready"), "{}", report.body);
    assert!(report.body.contains("| #61 | plan"), "{}", report.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn advise_mode_proposes_label_and_evidence_comment_on_recommended_issues() {
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Advise,
        ..TriageConfig::default()
    })
    .await;
    // A second candidate that gets a `hold` recommendation — report-only,
    // never labeled or commented on.
    env.forge.add_issue(61, "vague ask", "needs a human", &[]);

    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.8,
                "estimated_complexity": "small", "rationale": "clear small change", "missing_info": null},
               {"issue": 61, "recommendation": "hold", "confidence": 0.3,
                "estimated_complexity": "medium", "rationale": "wait for discussion", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    // ready → labeled + one evidence comment carrying the hidden marker.
    let candidate = env.forge.get_issue(CANDIDATE).await.unwrap();
    assert_eq!(candidate.labels, vec![LABEL_TRIAGE_READY.to_string()]);
    let comments = env.forge.comments_of(CANDIDATE);
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].starts_with("<!-- meguri:triage-advise hash="));
    assert!(comments[0].contains("clear small change"));
    assert!(comments[0].contains(LABEL_READY));

    // hold → report-only, nothing written on the issue itself.
    let held = env.forge.get_issue(61).await.unwrap();
    assert!(held.labels.is_empty());
    assert!(env.forge.comments_of(61).is_empty());

    // The bystander (already a real workflow label) is never touched.
    assert!(env.forge.comments_of(BYSTANDER).is_empty());
    let bystander = env.forge.get_issue(BYSTANDER).await.unwrap();
    assert_eq!(bystander.labels, vec![LABEL_READY.to_string()]);

    // The report is still published, its footer now describing the advise flow.
    let report = report_issue(&env).await.unwrap();
    assert!(report.body.contains("| #60 | ready"), "{}", report.body);
    assert!(report.body.contains("| #61 | hold"), "{}", report.body);
    assert!(report.body.contains("meguri:triage-"), "{}", report.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn advise_mode_throttles_writes_by_max_actions_per_tick() {
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Advise,
        max_actions_per_tick: 1,
        ..TriageConfig::default()
    })
    .await;
    env.forge.add_issue(61, "second", "also untriaged", &[]);

    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.8,
                "estimated_complexity": "small", "rationale": "clear", "missing_info": null},
               {"issue": 61, "recommendation": "ready", "confidence": 0.7,
                "estimated_complexity": "small", "rationale": "also clear", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    let c60 = env.forge.get_issue(CANDIDATE).await.unwrap();
    let c61 = env.forge.get_issue(61).await.unwrap();
    let proposed = usize::from(!c60.labels.is_empty()) + usize::from(!c61.labels.is_empty());
    assert_eq!(
        proposed, 1,
        "max_actions_per_tick=1 must cap the tick to one proposal: #60={:?} #61={:?}",
        c60.labels, c61.labels
    );

    // Both recommendations still land in the report regardless of the budget.
    let report = report_issue(&env).await.unwrap();
    assert!(report.body.contains("| #60 |"), "{}", report.body);
    assert!(report.body.contains("| #61 |"), "{}", report.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn advise_mode_is_idempotent_and_respects_rejection_until_content_changes() {
    // interval_hours = 0: only the "changed" half of the rescan gate matters,
    // so each sweep below just needs a new issue to be due, no marker surgery.
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Advise,
        interval_hours: 0,
        ..TriageConfig::default()
    })
    .await;

    // Sweep 1: propose `ready` on the candidate.
    let run1 = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(wt, READY_REC);
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run1.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );
    assert_eq!(env.forge.comments_of(CANDIDATE).len(), 1);
    let report = report_issue(&env).await.unwrap();

    // Human rejects: removes the proposal label. A new issue forces a
    // rescan, and — even though #60 no longer carries a proposal label —
    // its content hasn't changed since the evidence marker, so the rejection
    // must stick all the way up in candidate gathering: #60 is not offered
    // to the agent at all (the scripted report covering only #71 passes the
    // exact-coverage check), gets no label and no new comment, and is not
    // re-listed in the central report.
    env.forge
        .remove_label(CANDIDATE, LABEL_TRIAGE_READY)
        .await
        .unwrap();
    env.forge.add_issue(71, "unrelated", "new", &[]);
    let run2 = create_triage_run(&env, report.number);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 71, "recommendation": "ready", "confidence": 0.6,
                "estimated_complexity": "small", "rationale": "new one", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run2.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "a report covering only #71 must satisfy coverage — the rejected, \
         unchanged #60 must not be a candidate: {outcome:?}"
    );
    assert!(
        env.forge
            .get_issue(CANDIDATE)
            .await
            .unwrap()
            .labels
            .is_empty(),
        "a rejected proposal must not come back while the content is unchanged"
    );
    assert_eq!(
        env.forge.comments_of(CANDIDATE).len(),
        1,
        "no duplicate comment"
    );
    // #71 is fresh, so it gets its own proposal.
    assert_eq!(
        env.forge.get_issue(71).await.unwrap().labels,
        vec![LABEL_TRIAGE_READY.to_string()]
    );
    // ...and the rewritten central report re-lists only #71, not the
    // rejected #60.
    let report2 = env.forge.get_issue(report.number).await.unwrap();
    assert!(report2.body.contains("| #71 |"), "{}", report2.body);
    assert!(
        !report2.body.contains("| #60 |"),
        "a rejected, unchanged issue must not re-appear in the report: {}",
        report2.body
    );

    // Now the candidate's content actually changes — re-triage is warranted,
    // and this time the new recommendation (`plan`) lands for real.
    env.forge
        .update_issue_body(CANDIDATE, "totally different ask now")
        .await
        .unwrap();
    env.forge.add_issue(72, "another", "new", &[]);
    let run3 = create_triage_run(&env, report.number);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "plan", "confidence": 0.4,
                "estimated_complexity": "large", "rationale": "actually needs a spec", "missing_info": null},
               {"issue": 72, "recommendation": "ready", "confidence": 0.6,
                "estimated_complexity": "small", "rationale": "another one", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run3.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );
    let candidate = env.forge.get_issue(CANDIDATE).await.unwrap();
    assert_eq!(candidate.labels, vec![LABEL_TRIAGE_PLAN.to_string()]);
    assert_eq!(
        env.forge.comments_of(CANDIDATE).len(),
        2,
        "the content change must produce a fresh evidence comment"
    );
    // The edited issue is back in the central report as well.
    let report3 = env.forge.get_issue(report.number).await.unwrap();
    assert!(report3.body.contains("| #60 | plan"), "{}", report3.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn advise_mode_content_edit_alone_triggers_rediscovery() {
    // interval_hours = 0: isolates the advise-drift signal from the interval
    // rate-limit so this test can assert on discovery directly, the way
    // `new_issue_triggers_rescan_even_with_a_still_head` does for the
    // new-issue signal.
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Advise,
        interval_hours: 0,
        ..TriageConfig::default()
    })
    .await;

    // Sweep 1: propose `ready` on the candidate.
    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(wt, READY_REC);
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    // Head still, no new issue, content unchanged: quiet, as usual.
    assert!(TriageLoop.discover(&env.deps).await.unwrap().is_empty());

    // The candidate's title/body changes — no push, no new issue — and
    // discovery alone must still notice: `report`/`advise`'s README/ADR
    // promise ("content change re-triages") has to hold even when neither
    // of the other two signals fires.
    env.forge
        .update_issue_body(CANDIDATE, "totally different ask now")
        .await
        .unwrap();
    assert!(!TriageLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn advise_mode_budget_backlog_alone_triggers_rediscovery() {
    // interval_hours = 0, same reasoning as the content-edit test above.
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Advise,
        interval_hours: 0,
        max_actions_per_tick: 1,
        ..TriageConfig::default()
    })
    .await;
    env.forge.add_issue(61, "second", "also untriaged", &[]);

    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.8,
                "estimated_complexity": "small", "rationale": "clear", "missing_info": null},
               {"issue": 61, "recommendation": "ready", "confidence": 0.7,
                "estimated_complexity": "small", "rationale": "also clear", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    // budget=1 leaves one of the two proposals unwritten.
    let c60 = env.forge.get_issue(CANDIDATE).await.unwrap();
    let c61 = env.forge.get_issue(61).await.unwrap();
    let proposed = usize::from(!c60.labels.is_empty()) + usize::from(!c61.labels.is_empty());
    assert_eq!(proposed, 1, "#60={:?} #61={:?}", c60.labels, c61.labels);

    // The report marker records the leftover backlog...
    let report = report_issue(&env).await.unwrap();
    let marker = parse_triage_marker(&report.body).unwrap();
    assert!(marker.backlog, "{}", report.body);
    assert!(
        report.body.contains("max_actions_per_tick"),
        "{}",
        report.body
    );

    // ...and discovery alone notices it: no new issue, no head move, yet a
    // sweep is still due — otherwise the un-proposed leftover would be
    // stranded until some unrelated trigger happened to fire (README/ADR's
    // "the rest carry over to the next sweep" promise).
    assert!(!TriageLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn advise_mode_rejected_then_edited_issue_alone_triggers_rediscovery() {
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Advise,
        interval_hours: 0,
        ..TriageConfig::default()
    })
    .await;

    // Sweep 1: propose `ready`.
    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(wt, READY_REC);
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    // Human rejects: the proposal label is removed. Content unchanged, so
    // discovery stays quiet.
    env.forge
        .remove_label(CANDIDATE, LABEL_TRIAGE_READY)
        .await
        .unwrap();
    assert!(TriageLoop.discover(&env.deps).await.unwrap().is_empty());

    // The rejected issue's content changes — no new issue, no push, and no
    // proposal label anymore either. Discovery must still notice: the
    // evidence comment's hidden marker survives the label removal, so a
    // rejected-then-edited issue is not stuck behind an unrelated trigger.
    env.forge
        .update_issue_body(CANDIDATE, "totally different ask now")
        .await
        .unwrap();
    assert!(!TriageLoop.discover(&env.deps).await.unwrap().is_empty());
}

// ---- v2 auto (issue #88) ---------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn auto_mode_promotes_above_threshold_and_leaves_the_rest() {
    // apply defaults to ["ready"], confidence_threshold to 0.7.
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Auto,
        ..TriageConfig::default()
    })
    .await;
    // #61: a `plan` recommendation — not in the default apply, so report-only.
    env.forge
        .add_issue(61, "big rework", "vague and large", &[]);
    // #62: a `ready` recommendation below the confidence bar — report-only.
    env.forge.add_issue(62, "maybe cache", "unsure", &[]);

    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.85,
                "estimated_complexity": "small", "rationale": "clear small change", "missing_info": null},
               {"issue": 61, "recommendation": "plan", "confidence": 0.9,
                "estimated_complexity": "large", "rationale": "needs a spec", "missing_info": null},
               {"issue": 62, "recommendation": "ready", "confidence": 0.5,
                "estimated_complexity": "small", "rationale": "not sure", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    // #60: ready ≥ 0.7 and in apply → real label + reason comment (applied=real).
    let c60 = env.forge.get_issue(CANDIDATE).await.unwrap();
    assert_eq!(c60.labels, vec![LABEL_READY.to_string()]);
    let comments = env.forge.comments_of(CANDIDATE);
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].contains("applied=real"), "{}", comments[0]);
    assert!(comments[0].contains("clear small change"));
    assert!(comments[0].contains(LABEL_READY));

    // #61: plan is not in the default apply → untouched.
    let c61 = env.forge.get_issue(61).await.unwrap();
    assert!(c61.labels.is_empty(), "{:?}", c61.labels);
    assert!(env.forge.comments_of(61).is_empty());

    // #62: ready below threshold → untouched.
    let c62 = env.forge.get_issue(62).await.unwrap();
    assert!(c62.labels.is_empty(), "{:?}", c62.labels);
    assert!(env.forge.comments_of(62).is_empty());

    // The engaged bystander is never touched.
    assert!(env.forge.comments_of(BYSTANDER).is_empty());

    // A `triage.promoted` audit event was recorded for #60 only.
    let promoted: Vec<_> = env
        .deps
        .store
        .events_for_run(&run.id, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "triage.promoted")
        .collect();
    assert_eq!(promoted.len(), 1, "{promoted:?}");
    assert_eq!(promoted[0].data["issue"], 60);
    assert_eq!(promoted[0].data["label"], LABEL_READY);

    // The report marks the promoted row and lists all three recommendations.
    let report = report_issue(&env).await.unwrap();
    assert!(report.body.contains("| #60 | ready"), "{}", report.body);
    assert!(report.body.contains("✅ 昇格"), "{}", report.body);
    assert!(report.body.contains("| #61 | plan"), "{}", report.body);
    assert!(report.body.contains("| #62 | ready"), "{}", report.body);
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_mode_rolls_back_the_label_when_the_reason_comment_fails() {
    // A real label with no reason comment would engage the issue for
    // worker/planner while breaking auto's "reason comment mandatory /
    // removable to revert" invariant — so a comment failure must roll the
    // label back, not leave it bare.
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Auto,
        ..TriageConfig::default()
    })
    .await;
    env.forge.fail_comment(CANDIDATE);

    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.85,
                "estimated_complexity": "small", "rationale": "clear", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    // The sweep as a whole still succeeds (per-issue promotion is best-effort).
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    // The label was rolled back: no bare real label, and no comment landed.
    let c60 = env.forge.get_issue(CANDIDATE).await.unwrap();
    assert!(
        c60.labels.is_empty(),
        "the real label must be rolled back when the reason comment fails: {:?}",
        c60.labels
    );
    assert!(env.forge.comments_of(CANDIDATE).is_empty());

    // No `triage.promoted` event was recorded (the promotion did not complete).
    let promoted = env
        .deps
        .store
        .events_for_run(&run.id, 100)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "triage.promoted")
        .count();
    assert_eq!(promoted, 0);

    // The failed-but-promotable recommendation is recorded as backlog so the
    // next sweep retries it.
    let report = report_issue(&env).await.unwrap();
    assert!(
        parse_triage_marker(&report.body).unwrap().backlog,
        "{}",
        report.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_mode_apply_ready_and_plan_promotes_both() {
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Auto,
        apply: vec![TriageAction::Ready, TriageAction::Plan],
        ..TriageConfig::default()
    })
    .await;
    env.forge.add_issue(61, "big rework", "needs a spec", &[]);

    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.85,
                "estimated_complexity": "small", "rationale": "clear", "missing_info": null},
               {"issue": 61, "recommendation": "plan", "confidence": 0.8,
                "estimated_complexity": "large", "rationale": "spec-first", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    assert_eq!(
        env.forge.get_issue(CANDIDATE).await.unwrap().labels,
        vec![LABEL_READY.to_string()]
    );
    let c61 = env.forge.get_issue(61).await.unwrap();
    assert_eq!(c61.labels, vec![LABEL_PLAN.to_string()]);
    assert!(
        env.forge
            .comments_of(61)
            .iter()
            .any(|c| c.contains("applied=real") && c.contains(LABEL_PLAN))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_mode_never_promotes_needs_human() {
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Auto,
        apply: vec![TriageAction::Ready, TriageAction::Plan],
        ..TriageConfig::default()
    })
    .await;

    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "needs-human", "confidence": 0.95,
                "estimated_complexity": "medium", "rationale": "needs a decision", "missing_info": "which backend?"}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    // Even at 0.95 confidence, needs-human is a ball label (ADR 0005) — never
    // applied alone, so the untriaged issue stays label-free (no phase-0 anomaly).
    let c60 = env.forge.get_issue(CANDIDATE).await.unwrap();
    assert!(c60.labels.is_empty(), "{:?}", c60.labels);
    assert!(!c60.has_label(LABEL_NEEDS_HUMAN));
    assert!(env.forge.comments_of(CANDIDATE).is_empty());
    // It still appears in the central report.
    let report = report_issue(&env).await.unwrap();
    assert!(
        report.body.contains("| #60 | needs-human"),
        "{}",
        report.body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_mode_escalates_pending_proposal_and_respects_rejection() {
    // Start in advise, propose on two issues, reject one, switch to auto, and
    // verify the pending proposal is escalated while the rejection is respected
    // (the advise→auto migration + rejection guardrail, ADR 0017 decision 3).
    let mut env = setup_with_triage(TriageConfig {
        mode: TriageMode::Advise,
        interval_hours: 0,
        ..TriageConfig::default()
    })
    .await;
    env.forge.add_issue(61, "second", "also untriaged", &[]);

    // Sweep 1 (advise): propose `ready` on #60 and #61.
    let run1 = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.85,
                "estimated_complexity": "small", "rationale": "clear", "missing_info": null},
               {"issue": 61, "recommendation": "ready", "confidence": 0.85,
                "estimated_complexity": "small", "rationale": "also clear", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run1.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );
    let report = report_issue(&env).await.unwrap();
    assert_eq!(
        env.forge.get_issue(CANDIDATE).await.unwrap().labels,
        vec![LABEL_TRIAGE_READY.to_string()]
    );

    // Human rejects #61's proposal by removing the label (content unchanged).
    env.forge
        .remove_label(61, LABEL_TRIAGE_READY)
        .await
        .unwrap();

    // Operator switches the loop to auto. The still-pending #60 makes discovery
    // due (the advise→auto migration signal); without the level-aware check it
    // would stay quiet and #60 would never reach promotion.
    env.deps.config.triage.mode = TriageMode::Auto;
    assert!(!TriageLoop.discover(&env.deps).await.unwrap().is_empty());

    // Sweep 2 (auto): #60 (pending proposal) is a candidate; the rejected #61
    // is not — a report covering only #60 satisfies exact coverage.
    let run2 = create_triage_run(&env, report.number);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.85,
                "estimated_complexity": "small", "rationale": "clear", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run2.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "a report covering only the pending #60 must satisfy coverage — the \
         rejected, unchanged #61 must not be a candidate: {outcome:?}"
    );

    // #60: the pending proposal is escalated to the real label; the proposal
    // label is superseded and a real-level reason comment is added.
    let c60 = env.forge.get_issue(CANDIDATE).await.unwrap();
    assert_eq!(
        c60.labels,
        vec![LABEL_READY.to_string()],
        "a pending proposal must escalate to the real label under auto"
    );
    assert!(!c60.has_label(LABEL_TRIAGE_READY));
    assert!(
        env.forge
            .comments_of(CANDIDATE)
            .iter()
            .any(|c| c.contains("applied=real"))
    );

    // #61: the rejection is respected — no label, no new comment.
    let c61 = env.forge.get_issue(61).await.unwrap();
    assert!(
        c61.labels.is_empty(),
        "a rejected proposal must not be auto-promoted: {:?}",
        c61.labels
    );
    assert_eq!(
        env.forge.comments_of(61).len(),
        1,
        "no new comment on the rejected issue"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_mode_does_not_rescan_a_pending_proposal_kind_outside_apply() {
    // A pending proposal whose kind is not in `apply` can never be escalated
    // (promote_one no-ops it), so it must not keep the sweep perpetually due —
    // otherwise it re-scans every interval forever.
    let mut env = setup_with_triage(TriageConfig {
        mode: TriageMode::Advise,
        interval_hours: 0,
        ..TriageConfig::default()
    })
    .await;

    // Sweep 1 (advise): propose `plan` on the candidate.
    let run1 = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "plan", "confidence": 0.9,
                "estimated_complexity": "large", "rationale": "needs a spec", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run1.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );
    assert_eq!(
        env.forge.get_issue(CANDIDATE).await.unwrap().labels,
        vec![LABEL_TRIAGE_PLAN.to_string()]
    );

    // Switch to auto with the default apply = ["ready"]. The pending `plan`
    // proposal is not promotable here, so discovery must stay quiet (content
    // unchanged, head still, no new issue).
    env.deps.config.triage.mode = TriageMode::Auto;
    assert!(
        TriageLoop.discover(&env.deps).await.unwrap().is_empty(),
        "a pending proposal whose kind is outside `apply` must not re-trigger the sweep"
    );

    // Add `plan` to apply — now the same pending proposal is real pending work,
    // so discovery fires.
    env.deps.config.triage.apply = vec![TriageAction::Ready, TriageAction::Plan];
    assert!(
        !TriageLoop.discover(&env.deps).await.unwrap().is_empty(),
        "once `plan` is in `apply`, the pending plan proposal must re-trigger the sweep"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_mode_throttles_promotions_by_max_actions_per_tick() {
    let env = setup_with_triage(TriageConfig {
        mode: TriageMode::Auto,
        max_actions_per_tick: 1,
        ..TriageConfig::default()
    })
    .await;
    env.forge.add_issue(61, "second", "also untriaged", &[]);

    let run = create_triage_run(&env, 0);
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_report(
            wt,
            r#"[{"issue": 60, "recommendation": "ready", "confidence": 0.85,
                "estimated_complexity": "small", "rationale": "clear", "missing_info": null},
               {"issue": 61, "recommendation": "ready", "confidence": 0.85,
                "estimated_complexity": "small", "rationale": "also clear", "missing_info": null}]"#,
        );
        write_result(wt, turn_id, "success");
    });
    let outcome = run_to_outcome(&env, &run.id).await;
    agent.abort();
    assert!(
        matches!(outcome, WorkerOutcome::Succeeded { .. }),
        "{outcome:?}"
    );

    let c60 = env.forge.get_issue(CANDIDATE).await.unwrap();
    let c61 = env.forge.get_issue(61).await.unwrap();
    let promoted =
        usize::from(c60.has_label(LABEL_READY)) + usize::from(c61.has_label(LABEL_READY));
    assert_eq!(
        promoted, 1,
        "budget of 1 must cap promotions: #60={:?} #61={:?}",
        c60.labels, c61.labels
    );

    // The leftover promotable recommendation is recorded as backlog so the next
    // sweep fires even if nothing else moves.
    let report = report_issue(&env).await.unwrap();
    assert!(
        parse_triage_marker(&report.body).unwrap().backlog,
        "{}",
        report.body
    );
    assert!(
        report.body.contains("max_actions_per_tick"),
        "{}",
        report.body
    );
}
