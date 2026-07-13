//! End-to-end planner-loop tests with FakeMux + FakeForge and a real local
//! git origin: a `meguri:plan` issue becomes a spec PR labeled
//! `meguri:spec-reviewing`. A scripted "agent" plays the pane side (same
//! protocol as worker_test).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use meguri::config::{Config, ProjectConfig, WorkspaceConfig};
use meguri::engine::planner::{
    self, DECOMPOSED_MARKER, PlannerLoop, decompose_child_footer, run_planner, spec_rel_path,
};
use meguri::engine::worker;
use meguri::engine::{Deps, Loop, WorkerOutcome};
use meguri::forge::fake::FakeForge;
use meguri::forge::{
    Forge, ForgeFactory, Issue, LABEL_HOLD, LABEL_NEEDS_HUMAN, LABEL_PLAN, LABEL_READY,
    LABEL_SPEC_REVIEWING, LABEL_SPECCING, LABEL_WORKING,
};
use meguri::gitops::run_git;
use meguri::mux::fake::FakeMux;
use meguri::store::{RunStatus, Store};

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
    #[allow(dead_code)]
    root: tempfile::TempDir,
    worktree_root: PathBuf,
}

async fn setup(check_command: Option<&str>) -> TestEnv {
    let root = tempfile::tempdir().unwrap();
    let (_origin, clone) = init_origin_and_clone(root.path()).await;
    let worktree_root = root.path().join("worktrees");

    let forge = Arc::new(FakeForge::with_issue(
        5,
        "Add caching layer",
        "Requests are slow; add a cache.",
        &[LABEL_PLAN],
    ));

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600; // scripted agent: no nudging wanted
    config.limits.result_grace_secs = 1; // FakeMux always reads Working; don't linger
    // These planner tests don't exercise the self-review phase (ADR 0008); the
    // dedicated self-review test enables it explicitly.
    config.review.enabled = false;
    let project = ProjectConfig {
        id: "proj".into(),
        repo_path: clone,
        repo_slug: Some("me/proj".into()),
        mode: Default::default(),
        deliver: None,
        default_branch: "main".into(),
        language: None,
        check_command: check_command.map(str::to_string),
        worktree_root: Some(worktree_root.clone()),
        pr: None,
        clean: None,
        plan_delivery: Default::default(),
        review: None,
        worktree_setup: Default::default(),
        schedules: Vec::new(),
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
        root,
        worktree_root,
    }
}

fn create_planner_run(env: &TestEnv) -> meguri::store::RunRecord {
    env.deps
        .store
        .create_run_for_loop("proj", planner::KIND, 5, "Add caching layer")
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

fn write_result(worktree: &Path, turn_id: &str, status: &str) {
    write_result_with_subject(worktree, turn_id, status, None);
}

fn write_result_with_subject(worktree: &Path, turn_id: &str, status: &str, subject: Option<&str>) {
    let mut result = serde_json::json!({
        "turn_id": turn_id, "status": status, "summary": "scripted spec",
    });
    if let Some(subject) = subject {
        result["subject"] = serde_json::Value::String(subject.to_string());
    }
    std::fs::write(worktree.join(".meguri/result.json"), result.to_string()).unwrap();
}

/// Contents of the prompt files delivered to the (scripted) agent.
fn prompts_in(worktree: &Path) -> Vec<String> {
    std::fs::read_dir(worktree.join(".meguri"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("prompt-") && name.ends_with(".md")
        })
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect()
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

async fn commit_files(wt: &Path, files: &[(&str, &str)], message: &str) {
    for (rel, contents) in files {
        let path = wt.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    run_git(wt, &["add", "."]).await.unwrap();
    run_git(
        wt,
        &[
            "-c",
            "user.email=a@example.com",
            "-c",
            "user.name=agent",
            "commit",
            "-m",
            message,
        ],
    )
    .await
    .unwrap();
}

async fn commit_spec(wt: &Path) {
    commit_files(
        wt,
        &[(
            "docs/specs/issue-5.md",
            "# Spec: Add caching layer\n\n- acceptance criteria\n- files to touch\n- decisions\n",
        )],
        "Add spec for issue 5",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_happy_path_plan_issue_to_spec_pr() {
    // The check command also proves spec-only changes survive validation.
    let env = setup(Some("test -f docs/specs/issue-5.md")).await;
    let run = create_planner_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            commit_spec(&wt).await;
            write_result_with_subject(
                &wt,
                &turn_id,
                "success",
                Some("Write a spec for the caching layer"),
            );
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out")
        .unwrap();
    agent.abort();

    let WorkerOutcome::Succeeded { pr_url } = outcome else {
        panic!("expected success, got {outcome:?}");
    };
    assert!(pr_url.contains("fake.example"));

    // Run record is terminal and complete under the planner loop kind.
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Succeeded);
    assert_eq!(record.step, "open-pr");
    assert_eq!(record.loop_kind, planner::KIND);

    // Spec PR shape: the agent-authored subject becomes the title (issue
    // #136 — no more mechanical `Spec:` prefix hack), worker branch
    // conventions (the worker later takes this same branch over),
    // spec-reviewing label on the PR.
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].base, "main");
    assert_eq!(prs[0].title, "Write a spec for the caching layer (#5)");
    assert!(
        prs[0].head.starts_with("meguri/5-add-caching-layer-"),
        "branch must follow the worker naming convention: {}",
        prs[0].head
    );
    // Separate delivery (the default, ADR 0008): the spec PR uses a
    // non-closing reference so merging it does not close the issue.
    assert!(prs[0].body.contains("Refs #5"));
    assert!(!prs[0].body.contains("Closes #5"));
    assert!(prs[0].draft, "pr.draft defaults to true");
    assert!(
        prs[0].labels.contains(&LABEL_SPEC_REVIEWING.to_string()),
        "spec PR must carry {LABEL_SPEC_REVIEWING}: {:?}",
        prs[0].labels
    );

    // Phase transition on the issue (ADR 0005): plan (and the claim) are gone,
    // the phase moved to speccing (a spec PR is now open), no escalation.
    let labels = env.forge.labels_of(5);
    assert!(
        !labels.contains(&LABEL_PLAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
    assert!(
        labels.contains(&LABEL_SPECCING.to_string()),
        "issue must carry {LABEL_SPECCING} after the spec PR opens: {labels:?}"
    );

    // The prompt asked for a spec, not an implementation.
    let wt = find_worktree(&env.worktree_root).unwrap();
    let prompts = prompts_in(&wt);
    let execute_prompt = prompts
        .iter()
        .find(|p| p.contains("# Issue:"))
        .expect("execute prompt exists");
    assert!(execute_prompt.contains(&spec_rel_path(5)));
    assert!(execute_prompt.contains("do NOT implement"));
    assert!(execute_prompt.contains("# Pull request description"));
    // The adaptive spec-depth section rides along (issue #133).
    assert!(execute_prompt.contains("# Spec depth"));

    // The spec branch actually landed on origin (the worker resumes there).
    let clone = &env.deps.project.repo_path;
    let branches = run_git(clone, &["ls-remote", "--heads", "origin"])
        .await
        .unwrap();
    assert!(
        branches.contains("meguri/5-add-caching-layer-"),
        "{branches}"
    );
}

/// The planner self-reviews its spec/ADR before opening the spec PR (ADR 0008,
/// acceptance criterion 1): a review turn runs (in the self-review lane), and
/// the folded `<details>` inspection history rides the spec PR body.
#[tokio::test(flavor = "multi_thread")]
async fn planner_self_reviews_the_spec_before_opening_the_pr() {
    let mut env = setup(None).await;
    env.deps.config.review.enabled = true;
    let run = create_planner_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            let prompt = std::fs::read_to_string(wt.join(format!(".meguri/prompt-{turn_id}.md")))
                .unwrap_or_default();
            if prompt.contains("self-review round") {
                // A clean spec review: write the verdict file, touch nothing.
                let body = serde_json::json!({
                    "verdict": "clean", "review": "spec looks sound", "findings": [],
                });
                std::fs::write(
                    wt.join(meguri::engine::impl_reviewer::REVIEW_FILE),
                    body.to_string(),
                )
                .unwrap();
            } else {
                commit_spec(&wt).await;
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out")
        .unwrap();
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));

    // A self-review actually ran (the plan side is symmetric now, ADR 0008).
    let kinds: Vec<String> = env
        .deps
        .store
        .events_for_run(&run.id, 200)
        .unwrap()
        .iter()
        .map(|e| e.kind.clone())
        .collect();
    assert!(
        kinds.contains(&"self_review.reviewed".to_string()),
        "planner must self-review the spec: {kinds:?}"
    );

    // The spec PR body folds the self-review inspection history.
    let prs = env.forge.prs();
    assert_eq!(prs.len(), 1);
    assert!(
        prs[0].body.contains("<details>") && prs[0].body.contains("self-review"),
        "spec PR body must fold a self-review summary: {}",
        prs[0].body
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_corrective_turn_when_spec_missing() {
    let env = setup(None).await;
    let run = create_planner_run(&env);

    // Turn 1: commit *something* but not the spec (a misbehaving agent).
    // Turn 2 (the corrective turn): write the actual spec.
    let agent = spawn_scripted_agent(env.worktree_root.clone(), |turn, wt, turn_id| {
        let wt = wt.to_path_buf();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            if turn == 1 {
                commit_files(&wt, &[("notes.txt", "wip\n")], "notes").await;
            } else {
                commit_spec(&wt).await;
            }
            write_result(&wt, &turn_id, "success");
        });
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out")
        .unwrap();
    agent.abort();

    assert!(matches!(outcome, WorkerOutcome::Succeeded { .. }));
    // The corrective loop recorded the missing spec.
    let events = env.deps.store.events_for_run(&run.id, 100).unwrap();
    let correction = events
        .iter()
        .find(|e| e.kind == "execute.correction")
        .unwrap_or_else(|| {
            panic!(
                "missing correction event: {:?}",
                events.iter().map(|e| e.kind.clone()).collect::<Vec<_>>()
            )
        });
    assert!(
        correction
            .data
            .to_string()
            .contains("docs/specs/issue-5.md"),
        "correction must name the spec file: {}",
        correction.data
    );
}

/// Result file for a decompose ending: three children, sequential deps, one
/// of them still needing its own design pass.
fn write_decompose_result(worktree: &Path, turn_id: &str) {
    let result = serde_json::json!({
        "turn_id": turn_id, "status": "decompose",
        "summary": "read path, write path and invalidation are separable",
        "children": [
            {"title": "Cache read path", "body": "Read-through cache.", "kind": "ready"},
            {"title": "Cache write path", "body": "Write-behind cache.", "kind": "ready",
             "blocked_by": [0]},
            {"title": "Cache invalidation design", "body": "Needs its own spec.",
             "kind": "plan", "blocked_by": [0, 1]},
        ],
    });
    std::fs::write(worktree.join(".meguri/result.json"), result.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_decompose_files_children_with_deps_labels_and_parent_comment() {
    let env = setup(None).await;
    let run = create_planner_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_decompose_result(wt, turn_id);
    });

    let outcome = tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out")
        .unwrap();
    agent.abort();

    let WorkerOutcome::Decomposed(reason) = outcome else {
        panic!("expected Decomposed, got {outcome:?}");
    };
    assert!(reason.contains("separable"));
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Decomposed);

    // Children were filed in order with size-appropriate labels, and every
    // body references the parent (plus the machine marker for the one-level
    // guard).
    let issues = env.forge.all_issues();
    assert_eq!(issues.len(), 4, "parent + 3 children: {issues:?}");
    let children = &issues[1..];
    assert_eq!(children[0].title, "Cache read path");
    assert_eq!(children[1].title, "Cache write path");
    assert_eq!(children[2].title, "Cache invalidation design");
    assert!(children[0].labels.contains(&LABEL_READY.to_string()));
    assert!(children[1].labels.contains(&LABEL_READY.to_string()));
    assert!(children[2].labels.contains(&LABEL_PLAN.to_string()));
    for child in children {
        assert!(child.body.contains("#5"), "body: {}", child.body);
        assert!(child.body.contains(DECOMPOSED_MARKER));
    }

    // Dependencies: sibling order via blocked_by, and the parent waits for
    // every child.
    let (c0, c1, c2) = (children[0].number, children[1].number, children[2].number);
    assert!(env.forge.blockers_of(c0).is_empty());
    assert_eq!(env.forge.blockers_of(c1), vec![c0]);
    assert_eq!(env.forge.blockers_of(c2), vec![c0, c1]);
    assert_eq!(env.forge.blockers_of(5), vec![c0, c1, c2]);

    // The rationale landed on the parent, naming the children.
    let comments = env.forge.comments_of(5);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("separable"));
    assert!(comments[0].contains(&format!("#{c0}")));
    assert!(comments[0].contains(&format!("#{c2}")));

    // The parent left the planner queue without escalation: no plan label,
    // no claim, no needs-human — and no spec PR.
    let labels = env.forge.labels_of(5);
    assert!(
        !labels.contains(&LABEL_PLAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
    assert!(env.forge.prs().is_empty());

    // Nothing was pushed either — decompose ends the run before open-pr.
    let branches = run_git(
        &env.deps.project.repo_path,
        &["ls-remote", "--heads", "origin"],
    )
    .await
    .unwrap();
    assert!(!branches.contains("meguri/"), "{branches}");

    // The prompt invited the decompose ending.
    let wt = find_worktree(&env.worktree_root).unwrap();
    let prompts = prompts_in(&wt);
    let execute_prompt = prompts
        .iter()
        .find(|p| p.contains("# Issue:"))
        .expect("execute prompt exists");
    assert!(execute_prompt.contains("# Too big for one spec?"));
    assert!(execute_prompt.contains(r#""status": "decompose""#));
}

/// A [`ForgeFactory`] backed by a slug→FakeForge map: the cross-repo
/// decomposition path resolves a workspace sibling's forge through it (#154).
struct MapForgeFactory {
    map: HashMap<String, Arc<FakeForge>>,
}

impl ForgeFactory for MapForgeFactory {
    fn for_slug(&self, slug: &str) -> Arc<dyn Forge> {
        self.map
            .get(slug)
            .cloned()
            .map(|f| f as Arc<dyn Forge>)
            .unwrap_or_else(|| panic!("no fake forge registered for slug {slug}"))
    }
}

/// A planner env whose project `proj` (repo `me/proj`) is in a workspace with
/// a sibling `sib` (repo `me/sib`), each backed by its own FakeForge so the
/// cross-repo issue-filing path is exercised end-to-end (#154).
async fn setup_cross_repo() -> (TestEnv, Arc<FakeForge>) {
    let root = tempfile::tempdir().unwrap();
    let (_origin, clone) = init_origin_and_clone(root.path()).await;
    let worktree_root = root.path().join("worktrees");

    let parent = Arc::new(FakeForge::with_slug("me/proj"));
    parent.issues.lock().unwrap().push(Issue {
        number: 5,
        title: "Split shop into api and web".into(),
        body: "Big cross-repo change.".into(),
        labels: vec![LABEL_PLAN.into()],
    });
    let sibling = Arc::new(FakeForge::with_slug("me/sib"));

    let mut config = Config::default();
    config.limits.idle_grace_secs = 3600;
    config.limits.result_grace_secs = 1;
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
        schedules: Vec::new(),
    };
    let sibling_project = ProjectConfig {
        id: "sib".into(),
        repo_path: root.path().join("sib-unused"),
        repo_slug: Some("me/sib".into()),
        ..project.clone()
    };
    config.projects = vec![project.clone(), sibling_project];
    config.workspaces = vec![WorkspaceConfig {
        id: "shop".into(),
        projects: vec!["proj".into(), "sib".into()],
    }];

    let factory = Arc::new(MapForgeFactory {
        map: HashMap::from([
            ("me/proj".to_string(), parent.clone()),
            ("me/sib".to_string(), sibling.clone()),
        ]),
    });
    let deps = Deps::with_label_source(
        Store::open_in_memory().unwrap(),
        Arc::new(FakeMux::new(false)),
        parent.clone(),
        config,
        project,
    )
    .with_forge_factory(factory);
    (
        TestEnv {
            deps,
            forge: parent,
            root,
            worktree_root,
        },
        sibling,
    )
}

/// Decompose with a sibling-repo child and a human node: child 0 in the parent
/// repo, child 1 in sibling `sib`, child 2 a `human` node — all chained.
fn write_cross_repo_decompose_result(worktree: &Path, turn_id: &str) {
    let result = serde_json::json!({
        "turn_id": turn_id, "status": "decompose",
        "summary": "api and web move together; the shared repo is a human step",
        "children": [
            {"title": "API side change", "body": "Change the API.", "kind": "ready"},
            {"title": "Web side change", "body": "Follow the API.", "kind": "ready",
             "blocked_by": [0], "project": "sib"},
            {"title": "Create shared infra repo", "body": "meguri can't do this.",
             "kind": "human", "blocked_by": [0, 1]},
        ],
    });
    std::fs::write(worktree.join(".meguri/result.json"), result.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_decompose_files_cross_repo_child_and_human_node() {
    let (env, sibling) = setup_cross_repo().await;
    let run = create_planner_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_cross_repo_decompose_result(wt, turn_id);
    });
    let outcome = tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out")
        .unwrap();
    agent.abort();
    assert!(matches!(outcome, WorkerOutcome::Decomposed(_)));

    // Parent repo holds the parent + the two same-repo children (api + human);
    // the sibling child was filed in the sibling repo's forge.
    let parent_issues = env.forge.all_issues();
    assert_eq!(
        parent_issues.len(),
        3,
        "parent + api + human: {parent_issues:?}"
    );
    let api = &parent_issues[1];
    let human = &parent_issues[2];
    assert_eq!(api.title, "API side change");
    assert!(api.labels.contains(&LABEL_READY.to_string()));
    // The human node is filed with NO trigger label so discovery never drives
    // it — a person closes it, unblocking dependents (#154).
    assert_eq!(human.title, "Create shared infra repo");
    assert!(
        human.labels.is_empty(),
        "human node labels: {:?}",
        human.labels
    );

    let sib_issues = sibling.all_issues();
    assert_eq!(
        sib_issues.len(),
        1,
        "web child in the sibling repo: {sib_issues:?}"
    );
    let web = &sib_issues[0];
    assert_eq!(web.title, "Web side change");
    assert!(web.labels.contains(&LABEL_READY.to_string()));
    // A cross-repo child references the parent as `owner/repo#N` so the link
    // resolves back to the parent's repo, not a same-numbered sibling issue.
    assert!(web.body.contains("me/proj#5"), "web body: {}", web.body);
    assert!(web.body.contains(DECOMPOSED_MARKER));

    // Dependency wiring spans repos: the web child (in sib) waits on the api
    // child (in proj); the parent waits on all three children.
    assert_eq!(sibling.blockers_of(web.number), vec![api.number]);
    let parent_blockers = env.forge.blockers_of(5);
    assert!(parent_blockers.contains(&api.number));
    assert!(parent_blockers.contains(&web.number));
    assert!(parent_blockers.contains(&human.number));

    // The rationale comment on the parent names the cross-repo child qualified.
    let comments = env.forge.comments_of(5);
    assert_eq!(comments.len(), 1);
    assert!(
        comments[0].contains(&format!("me/sib#{}", web.number)),
        "{}",
        comments[0]
    );

    // The decompose prompt advertised the workspace scope + human kind.
    let wt = find_worktree(&env.worktree_root).unwrap();
    let execute_prompt = prompts_in(&wt)
        .into_iter()
        .find(|p| p.contains("# Issue:"))
        .expect("execute prompt exists");
    assert!(execute_prompt.contains("Cross-repo scope"));
    assert!(execute_prompt.contains("workspace `shop`"));
    assert!(execute_prompt.contains("`sib`"));
    assert!(execute_prompt.contains(r#""human""#));
}

/// A child targeting a repo outside the parent's workspace is rejected and the
/// whole decomposition escalates to a human — scope stays pinned to config.
#[tokio::test(flavor = "multi_thread")]
async fn planner_decompose_rejects_out_of_scope_project() {
    let (env, sibling) = setup_cross_repo().await;
    let run = create_planner_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        let result = serde_json::json!({
            "turn_id": turn_id, "status": "decompose",
            "summary": "tries to reach a repo it must not",
            "children": [
                {"title": "Sneaky", "body": "x", "kind": "ready", "project": "stranger"},
            ],
        });
        std::fs::write(wt.join(".meguri/result.json"), result.to_string()).unwrap();
    });
    let result =
        tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id)).await;
    agent.abort();
    assert!(
        result.expect("planner timed out").is_err(),
        "out-of-scope decompose must fail"
    );

    // No child was filed anywhere; the parent is escalated to a human.
    assert_eq!(env.forge.all_issues().len(), 1);
    assert!(sibling.all_issues().is_empty());
    assert!(
        env.forge
            .labels_of(5)
            .contains(&LABEL_NEEDS_HUMAN.to_string())
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_re_decompose_on_child_escalates_to_needs_human() {
    let env = setup(None).await;
    // The issue is itself a decomposition child (as filed by a previous
    // decompose run): its body carries the parent reference + marker.
    env.forge
        .issues
        .lock()
        .unwrap()
        .iter_mut()
        .find(|i| i.number == 5)
        .unwrap()
        .body = format!("Do one part.{}", decompose_child_footer(3));
    let run = create_planner_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_decompose_result(wt, turn_id);
    });

    let result = tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out");
    agent.abort();

    // One level only: the second decompose fails the run and hands the
    // issue to a human.
    assert!(result.is_err(), "re-decompose must fail the run");
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);

    let labels = env.forge.labels_of(5);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));

    let comments = env.forge.comments_of(5);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("needs a human"));
    assert!(comments[0].contains("one level"));

    // No grandchildren were filed.
    assert_eq!(env.forge.all_issues().len(), 1);
    assert!(env.forge.blockers_of(5).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_needs_human_escalates_on_forge() {
    let env = setup(None).await;
    let run = create_planner_run(&env);

    let agent = spawn_scripted_agent(env.worktree_root.clone(), |_, wt, turn_id| {
        write_result(wt, turn_id, "needs_human");
    });

    let result = tokio::time::timeout(Duration::from_secs(60), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out");
    agent.abort();

    assert!(result.is_err(), "needs_human must fail the run");
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Failed);

    // Same escalation as the worker: needs-human label + comment, claim
    // released, plan stays for a human to re-triage.
    let labels = env.forge.labels_of(5);
    assert!(
        labels.contains(&LABEL_NEEDS_HUMAN.to_string()),
        "labels: {labels:?}"
    );
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(labels.contains(&LABEL_PLAN.to_string()));

    let comments = env.forge.comments_of(5);
    assert_eq!(comments.len(), 1);
    assert!(comments[0].contains("needs a human"));
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_skips_quietly_when_plan_label_removed_after_discovery() {
    let env = setup(None).await;
    let run = create_planner_run(&env);

    // The benign race: the plan label vanished between discovery and claim.
    env.forge.remove_label(5, LABEL_PLAN).await.unwrap();

    let outcome = tokio::time::timeout(Duration::from_secs(30), run_planner(&env.deps, &run.id))
        .await
        .expect("planner timed out")
        .unwrap();

    assert!(
        matches!(outcome, WorkerOutcome::Skipped(_)),
        "expected skip, got {outcome:?}"
    );
    let record = env.deps.store.get_run(&run.id).unwrap().unwrap();
    assert_eq!(record.status, RunStatus::Skipped);

    // Quiet skip: no escalation label, no claim, no comment.
    let labels = env.forge.labels_of(5);
    assert!(!labels.contains(&LABEL_NEEDS_HUMAN.to_string()));
    assert!(!labels.contains(&LABEL_WORKING.to_string()));
    assert!(env.forge.comments_of(5).is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_discovery_filters_hold_working_and_shipped() {
    let env = setup(None).await;

    // Alongside the actionable plan issue: held, claimed, and unlabeled ones.
    for (number, labels) in [
        (6, vec![LABEL_PLAN.to_string(), LABEL_HOLD.to_string()]),
        (7, vec![LABEL_PLAN.to_string(), LABEL_WORKING.to_string()]),
        (8, vec![]),
    ] {
        env.forge.issues.lock().unwrap().push(meguri::forge::Issue {
            number,
            title: format!("issue {number}"),
            body: String::new(),
            labels,
        });
    }

    let targets = PlannerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![5]
    );

    // A *worker* success on the issue must not block the planner...
    let done = env
        .deps
        .store
        .create_run_for_loop("proj", worker::KIND, 5, "Add caching layer")
        .unwrap();
    env.deps
        .store
        .update_run_status(&done.id, RunStatus::Succeeded, None)
        .unwrap();
    assert_eq!(PlannerLoop.discover(&env.deps).await.unwrap().len(), 1);

    // ...but a planner success does (the plan label lingered).
    let shipped = create_planner_run(&env);
    env.deps
        .store
        .update_run_status(&shipped.id, RunStatus::Succeeded, None)
        .unwrap();
    assert!(PlannerLoop.discover(&env.deps).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_discovery_gates_on_unresolved_blockers() {
    let env = setup(None).await;

    // Open blocker: skipped.
    env.forge.block_issue(5, 4);
    assert!(PlannerLoop.discover(&env.deps).await.unwrap().is_empty());

    // duplicate does not resolve the dependency either (same as not_planned).
    env.forge.close_issue_as(4, "duplicate");
    assert!(PlannerLoop.discover(&env.deps).await.unwrap().is_empty());

    // Only closed-as-completed lets the issue through.
    env.forge.close_issue(4);
    let targets = PlannerLoop.discover(&env.deps).await.unwrap();
    assert_eq!(
        targets.iter().map(|t| t.key.number()).collect::<Vec<_>>(),
        vec![5]
    );

    // Unreadable blockers count as unresolved, never as resolved.
    env.forge.fail_blocked_by(5);
    assert!(PlannerLoop.discover(&env.deps).await.unwrap().is_empty());

    // Every skip above was silent: no comment, no extra label.
    assert!(env.forge.comments_of(5).is_empty());
    assert_eq!(env.forge.labels_of(5), vec![LABEL_PLAN.to_string()]);
}
