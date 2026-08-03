//! CLI command implementations.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::config::{self, Config, ProjectConfig, ProjectMode};
use crate::engine::reaper;
use crate::engine::scheduler::{Reload, Scheduler};
use crate::engine::worker::WorkerOutcome;
use crate::engine::{self, Deps};
use crate::forge::Forge;
use crate::forge::gh::GhForge;
use crate::mux;
use crate::store::{DesiredState, LANE_AUTHOR, RunRecord, RunStatus, Store};
use crate::tasks::{LabelTaskSource, LocalTaskSource, TaskKind, TaskSource};

pub fn open_store() -> Result<Store> {
    Store::open(&config::db_path())
}

/// A project's coordination layer: its optional forge (github only) and its
/// task source.
type Coordination = (Option<Arc<dyn Forge>>, Arc<dyn TaskSource>);

/// The coordination layer (and whether there is a forge at all) is chosen by
/// the project mode: labels+GitHub for github, the local sqlite `tasks` table
/// for local. Shared by `build_deps` and the driverless `cmd_stop` finalize.
fn build_coordination(
    _cfg: &Config,
    project: &ProjectConfig,
    store: &Store,
) -> Result<Coordination> {
    match project.mode {
        ProjectMode::Github => {
            let slug = project.repo_slug.clone().context(
                "github-mode project has no repo_slug (config validation should have caught this)",
            )?;
            let forge: Arc<dyn Forge> = Arc::new(GhForge::new(&slug));
            let ts: Arc<dyn TaskSource> = Arc::new(LabelTaskSource::new(
                forge.clone(),
                store.clone(),
                project.id.clone(),
            ));
            Ok((Some(forge), ts))
        }
        ProjectMode::Local => {
            let ts: Arc<dyn TaskSource> =
                Arc::new(LocalTaskSource::new(store.clone(), project.id.clone()));
            Ok((None, ts))
        }
    }
}

fn build_deps(cfg: &Config, project: &ProjectConfig, mux_override: Option<&str>) -> Result<Deps> {
    let kind = mux_override.unwrap_or(&cfg.mux.kind);
    // Per-project workspace: this project's panes live in `<session>:<project>`
    // (herdr) / `<session>-<project>` (tmux), not the shared base workspace.
    let mux = mux::detect(kind, &cfg.mux.session, Some(&project.id))?;
    let store = open_store()?;
    let (forge, task_source) = build_coordination(cfg, project, &store)?;
    Ok(Deps {
        store,
        mux,
        forge,
        task_source,
        forge_factory: Arc::new(crate::forge::gh::GhForgeFactory),
        config: cfg.clone(),
        project: project.clone(),
        preflight_enabled: true,
    })
}

fn pick_project<'a>(cfg: &'a Config, id: Option<&str>) -> Result<&'a ProjectConfig> {
    match id {
        Some(id) => cfg
            .project(id)
            .with_context(|| format!("project {id:?} not in config")),
        None => match cfg.projects.as_slice() {
            [] => bail!(
                "no projects configured — edit {}",
                config::config_path().display()
            ),
            [only] => Ok(only),
            _ => bail!("multiple projects configured — pass --project <id>"),
        },
    }
}

/// `meguri add` — low-friction capture; the behavior follows the project mode.
/// github → create a GitHub issue immediately (never via the LLM) and refine it
/// best-effort (ADR 0006). local → queue a task in the sqlite `tasks` table for
/// the watch (issue #148 / ADR 0003). Both share the "capture now, sort later"
/// intent; the mode-specific flags are rejected on the wrong mode.
pub async fn cmd_add(
    project: Option<&str>,
    text: Option<&str>,
    ready: bool,
    file: Option<&str>,
) -> Result<()> {
    let cfg = Config::load()?;
    let cwd = std::env::current_dir().context("resolving the current directory")?;
    let project = infer_project(&cfg, project, &cwd)?;
    check_add_flags(project, ready, file.is_some())?;
    match project.mode {
        ProjectMode::Github => {
            let text = github_memo(text)?;
            add_github(&cfg, project, text, ready).await
        }
        ProjectMode::Local => add_local(project, text, file),
    }
}

/// The github-mode memo check, factored out of [`cmd_add`] so it is testable
/// without a config file. Emptiness is judged on a trimmed view only; the
/// memo itself is returned untouched, because `add_core` stores it verbatim
/// (ADR 0006 原則2) — trimming here would silently strip the quoted
/// whitespace/newlines from the issue body and the 原文メモ footer.
pub fn github_memo(text: Option<&str>) -> Result<&str> {
    text.filter(|t| !t.trim().is_empty())
        .context("give `meguri add` a one-line memo to capture")
}

/// Flag ↔ mode compatibility for `meguri add`, factored out of [`cmd_add`] so
/// it is testable without a config file on disk. Notably, `--plan` needs a
/// github-mode project: local mode has no planner yet (issue #54 Phase 3) —
/// `PlannerLoop::discover` returns nothing without a forge — so a local plan
/// task would sit queued forever. Reject it up front, mirroring the
/// config-side check that refuses a local-mode `plan` schedule.
pub fn check_add_flags(project: &ProjectConfig, ready: bool, has_file: bool) -> Result<()> {
    match project.mode {
        ProjectMode::Github => {
            if has_file {
                bail!(
                    "--file is a local-mode option; a github-mode \
                     `meguri add` captures a one-line memo as an issue"
                );
            }
        }
        ProjectMode::Local => {
            if ready {
                bail!(
                    "--ready is a github-mode option; a local-mode `meguri add` \
                     queues a work task"
                );
            }
        }
    }
    Ok(())
}

/// github-mode capture: an issue is created immediately with the raw memo.
/// Lives outside the issue↔pane↔session lifetime model (#92): only the config
/// and the forge — no run, no pane.
async fn add_github(cfg: &Config, project: &ProjectConfig, text: &str, ready: bool) -> Result<()> {
    let repo_slug = project.repo_slug.as_deref().context(
        "github-mode project has no repo_slug (config validation should have caught this)",
    )?;
    let forge = GhForge::new(repo_slug);

    let mut labels: Vec<&str> = Vec::new();
    if ready {
        labels.push(crate::forge::LABEL_READY);
    }

    let _ = cfg;
    let params = AddParams {
        text,
        labels: &labels,
        repo_slug,
    };
    add_core(&forge, params).await?;
    Ok(())
}

/// Inputs `add_core` needs beyond the forge, gathered so the orchestration is
/// testable against a `FakeForge` without a live config.
pub struct AddParams<'a> {
    pub text: &'a str,
    pub labels: &'a [&'a str],
    pub repo_slug: &'a str,
}

/// The capture→refine→write-back core, split out from [`cmd_add`] so tests can
/// drive it with a fake forge and a fake refiner. Returns the created issue
/// number. `create_issue` failing is a real error (no issue exists); every
/// later failure — including refiner resolution itself, which only runs after
/// capture — leaves the raw issue in place and reports capture success.
pub async fn add_core(forge: &dyn Forge, params: AddParams<'_>) -> Result<i64> {
    // The memo is stored verbatim (ADR 0006 原則2): the raw `params.text`
    // becomes the body, so quoted leading/trailing whitespace and newlines
    // survive. A trimmed view is only for validation and the title.
    let raw = params.text;
    let title0 = initial_title(raw);
    let body0 = raw.to_string();

    // Capture: the one step that may hard-fail (auth/network/slug/permissions).
    let number = forge
        .create_issue(&title0, &body0, params.labels)
        .await
        .context("creating the issue (capture)")?;
    println!(
        "issue #{number} created: {}",
        issue_url(params.repo_slug, number)
    );
    Ok(number)
}

/// Which project `meguri add` targets: explicit `--project` wins; otherwise
/// infer from the cwd — a project whose canonical `repo_path` is a
/// path-component ancestor of the cwd. A single cwd match wins even among many
/// projects; multiple matches (or none with several projects configured) is an
/// explicit error; none with a single project falls back to that sole project.
pub fn infer_project<'a>(
    cfg: &'a Config,
    explicit: Option<&str>,
    cwd: &Path,
) -> Result<&'a ProjectConfig> {
    if let Some(id) = explicit {
        return cfg
            .project(id)
            .with_context(|| format!("project {id:?} not in config"));
    }
    // Canonicalize both sides so symlinks and `.`/`..` don't defeat the
    // ancestor test; fall back to the raw path when it can't be canonicalized.
    let cwd_c = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let matches: Vec<&ProjectConfig> = cfg
        .projects
        .iter()
        .filter(|p| {
            // A managed clone is bare (no working tree the cwd could sit under),
            // so this cwd-based match only ever hits explicit-`repo_path`
            // projects — which is the intended pre-managed-clone behavior.
            let effective = cfg.repo_path_for(p);
            let rp = effective.canonicalize().unwrap_or(effective);
            // starts_with is component-wise, so `/repo` never matches `/repo2`.
            cwd_c.starts_with(&rp)
        })
        .collect();
    match matches.as_slice() {
        [one] => Ok(one),
        [_, _, ..] => bail!(
            "the cwd is under multiple configured projects ({}) — pass --project <id>",
            matches
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        [] => match cfg.projects.as_slice() {
            [] => bail!(
                "no projects configured — edit {}",
                config::config_path().display()
            ),
            [only] => Ok(only),
            _ => bail!(
                "multiple projects configured and the cwd is under none — pass --project <id>"
            ),
        },
    }
}

/// The GitHub issue URL for a freshly created issue. `create_issue` returns
/// only the number, so the URL is composed from the `owner/repo` slug — its
/// shape is stable and this avoids widening the forge trait.
pub fn issue_url(repo_slug: &str, number: i64) -> String {
    format!("https://github.com/{repo_slug}/issues/{number}")
}

/// Pre-refine title from a raw memo: the first non-empty line, trimmed and
/// truncated so a paragraph-long memo doesn't become a monstrous title. The
/// full memo still lands in the body verbatim, so nothing is lost.
pub fn initial_title(text: &str) -> String {
    const MAX: usize = 72;
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    if line.chars().count() > MAX {
        let mut t: String = line.chars().take(MAX - 1).collect();
        t.push('…');
        t
    } else {
        line.to_string()
    }
}

/// Refined body followed by the verbatim original memo. This preservation is
/// the orchestrator's job, never the model's (ADR 0006 原則2): the model's
/// output is the scaffold, the original memo keeps authoring authority. The
/// original is embedded byte-for-byte (no trimming) — quoted whitespace and
/// newlines are part of what the author wrote.
pub fn compose_refined_body(refined_body: &str, original: &str) -> String {
    format!("{}\n\n---\n## 原文メモ\n{}", refined_body.trim(), original)
}

// ---- meguri add-project (issue #196, ADR 0019) ----

/// A validated `meguri add-project` plan, produced by pure logic (no disk, no
/// network) so it is unit-testable: the `[[projects]]` draft to append, and —
/// for github — the slug (repo to clone / optionally create) and the mode.
pub struct AddProjectPlan {
    pub draft: config::ProjectDraft,
    /// `Some(slug)` for github mode; `None` for local mode.
    pub slug: Option<String>,
    pub is_local: bool,
}

/// Turn the raw CLI arguments into a validated [`AddProjectPlan`], or explain
/// why they are rejected. Pure: validates the slug/id/path, derives the id, and
/// checks for a collision against the already-loaded `cfg` — no side effects, so
/// it carries the whole "reject before touching config" contract (spec issue-196
/// steps 1–2). clap already enforces flag exclusivity, so this does not re-check
/// `--create`×`--local`; a caller that hands in both simply gets the local plan.
pub fn plan_add_project(
    cfg: &Config,
    slug: Option<&str>,
    id: Option<&str>,
    local: Option<&str>,
) -> Result<AddProjectPlan> {
    let plan = match local {
        Some(path) => {
            let path_buf = PathBuf::from(path);
            if !path_buf.is_absolute() {
                bail!("--local path {path:?} must be absolute");
            }
            let id = resolve_project_id(id, config::default_id_from_path(&path_buf))?;
            AddProjectPlan {
                draft: config::ProjectDraft {
                    id,
                    repo_slug: None,
                    repo_path: Some(path.to_string()),
                    mode: Some("local".to_string()),
                },
                slug: None,
                is_local: true,
            }
        }
        None => {
            let slug = slug.context("github mode needs an owner/repo (or use --local)")?;
            config::validate_repo_slug(slug)?;
            let id = resolve_project_id(id, Some(config::default_id_from_slug(slug)))?;
            AddProjectPlan {
                draft: config::ProjectDraft {
                    id,
                    repo_slug: Some(slug.to_string()),
                    repo_path: None,
                    mode: None,
                },
                slug: Some(slug.to_string()),
                is_local: false,
            }
        }
    };
    check_project_collision(cfg, &plan.draft)?;
    Ok(plan)
}

/// Resolve the effective project id: an explicit `--id` wins, else the derived
/// default. Either way it must pass [`config::validate_project_id`]; a derived id
/// that fails (a repo name that is not a safe path component) asks for `--id`.
fn resolve_project_id(explicit: Option<&str>, derived: Option<&str>) -> Result<String> {
    match explicit {
        Some(id) => {
            config::validate_project_id(id)?;
            Ok(id.to_string())
        }
        None => {
            let d = derived.context("could not derive a project id — pass --id <id>")?;
            config::validate_project_id(d).with_context(|| {
                format!("derived project id {d:?} is not usable — pass --id <id>")
            })?;
            Ok(d.to_string())
        }
    }
}

/// Reject a draft whose id (or, for github, repo_slug) already names an existing
/// project — add-project only ever *adds*, never overwrites.
fn check_project_collision(cfg: &Config, draft: &config::ProjectDraft) -> Result<()> {
    if cfg.projects.iter().any(|p| p.id == draft.id) {
        bail!("project id {:?} already exists in config", draft.id);
    }
    // GitHub slugs are case-insensitive: `Owner/Repo` and `owner/repo` are the
    // same repository. Compare case-insensitively (as `gitops::clone_health`
    // does) so the same repo can't be watched as two projects racing for the
    // same issues/labels.
    if let Some(slug) = &draft.repo_slug
        && cfg.projects.iter().any(|p| {
            p.repo_slug
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(slug))
        })
    {
        bail!("repo_slug {slug:?} is already configured (project exists)");
    }
    Ok(())
}

/// `meguri add-project` — append a project to config.toml in one command
/// (issue #196, ADR 0019). github: validate the slug, optionally `--create` the
/// repo (initial commit included), append the `[[projects]]` block, materialize
/// the managed clone, and run a scoped environment check. local: append a
/// local-mode entry rooted at `--local <path>`. clap enforces the flag shapes;
/// everything past parsing lives here.
pub async fn cmd_add_project(
    slug: Option<&str>,
    create: bool,
    public: bool,
    id: Option<&str>,
    local: Option<&str>,
) -> Result<()> {
    let cfg_path = config::config_path();
    if !cfg_path.exists() {
        bail!(
            "no config at {} — run `meguri init` first",
            cfg_path.display()
        );
    }
    let cfg = Config::load()?;
    let plan = plan_add_project(&cfg, slug, id, local)?;

    // Best-effort audit trail; a missing/broken store must not block onboarding.
    let store = open_store().ok();

    // (3) Irreversible first, alone, loudly. meguri never deletes what it made,
    // so if a later step fails the repo simply stays (reported below).
    if create {
        let slug = plan
            .slug
            .as_deref()
            .context("--create needs an owner/repo slug")?;
        println!(
            "creating GitHub repo {slug} ({}) …",
            if public { "public" } else { "private" }
        );
        crate::forge::gh::create_repo(slug, public)
            .await
            .with_context(|| format!("could not create repo {slug}"))?;
        println!("✅ created {slug} (initial commit + default branch)");
        if let Some(store) = &store {
            let _ = store.emit(None, "repo.created", serde_json::json!({ "slug": slug }));
        }
    }

    // (4–5) Append atomically, then reparse; restore the original bytes on
    // failure so a created repo is never paired with a corrupt config.
    let original = std::fs::read_to_string(&cfg_path)
        .with_context(|| format!("cannot read config at {}", cfg_path.display()))?;
    config::append_project(&cfg_path, &plan.draft)?;
    let cfg = match Config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            config::write_atomic(&cfg_path, &original)?;
            return Err(e).context("appended config failed to reparse — rolled back");
        }
    };
    println!(
        "✅ added project {:?} to {}",
        plan.draft.id,
        cfg_path.display()
    );
    if let Some(store) = &store {
        let _ = store.emit(
            None,
            "project.added",
            serde_json::json!({
                "id": plan.draft.id,
                "mode": if plan.is_local { "local" } else { "github" },
                "slug": plan.slug,
            }),
        );
    }

    let project = cfg
        .project(&plan.draft.id)
        .context("just-added project not found after reload (should not happen)")?;

    // (6) Materialize the managed clone now so doctor is green immediately; a
    // failure self-heals on the next watch tick (ADR 0018), so it never fails
    // the command.
    if let Some(slug) = &plan.slug
        && cfg.is_managed_clone(project)
    {
        let dest = cfg.repo_path_for(project);
        print!("cloning {slug} → {} … ", dest.display());
        match crate::gitops::ensure_bare_clone(&dest, slug).await {
            Ok(()) => println!("✅"),
            Err(e) => println!("⚠️  {e:#} (will retry on the next `meguri watch` tick)"),
        }
    }

    // (7) Scoped environment check — surface reds now, not at first run.
    add_project_preflight(&cfg, project).await;

    println!();
    match project.mode {
        ProjectMode::Github => {
            println!("Next: `meguri watch`, then label an issue `meguri:ready` (or `meguri:plan`).")
        }
        ProjectMode::Local => {
            println!("Next: `meguri add \"タスク\"` to queue work, then `meguri watch`.")
        }
    }
    Ok(())
}

/// A focused, add-project-scoped rerun of the checks `meguri doctor` does for
/// one project: git / gh / gh-auth / gh write permission (github) / a usable
/// multiplexer / the default agent CLI. Advisory — it prints ✅/❌ and never
/// fails the command (doctor stays the full surface). Shares the write-scope
/// decision with doctor via [`can_push`] / [`gh_viewer_permission`].
pub async fn add_project_preflight(cfg: &Config, project: &ProjectConfig) {
    let mark = |ok: bool| if ok { "✅" } else { "❌" };
    println!("\nenvironment:");

    let git = version_line("git", &["--version"]);
    println!("  {} git: {}", mark(git.is_ok()), git.unwrap_or_else(|e| e));

    if project.mode != ProjectMode::Local {
        let gh = version_line("gh", &["--version"]);
        println!(
            "  {} gh: {}",
            mark(gh.is_ok()),
            gh.map(|v| v.lines().next().unwrap_or_default().to_string())
                .unwrap_or_else(|e| e)
        );
        let auth = version_line("gh", &["auth", "status"]);
        println!(
            "  {} gh auth: {}",
            mark(auth.is_ok()),
            auth.map(|_| "authenticated".to_string())
                .unwrap_or_else(|e| e)
        );
        if let Some(slug) = &project.repo_slug {
            match gh_viewer_permission(slug).await {
                Ok(perm) if can_push(&perm) => println!("  ✅ gh token can push ({perm})"),
                Ok(perm) => {
                    println!("  ❌ gh token cannot push (permission {perm}) — need write access")
                }
                Err(e) => println!("  ⚠️  gh push-permission check inconclusive: {e:#}"),
            }
        }
    }

    // A usable multiplexer: a live herdr socket or an installed tmux.
    let herdr_sock = std::env::var("HERDR_SOCKET_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".config/herdr/herdr.sock")
        });
    let tmux_ok = version_line("tmux", &["-V"]).is_ok();
    let mux_ok = herdr_sock.exists() || tmux_ok;
    println!(
        "  {} multiplexer: {}",
        mark(mux_ok),
        if herdr_sock.exists() {
            "herdr socket live"
        } else if tmux_ok {
            "tmux installed"
        } else {
            "none (start herdr or install tmux)"
        }
    );

    let agent = version_line(&cfg.agent.command, &["--version"]);
    println!(
        "  {} agent CLI ({}): {}",
        mark(agent.is_ok()),
        cfg.agent.command,
        agent
            .map(|v| v.lines().next().unwrap_or_default().to_string())
            .unwrap_or_else(|e| e)
    );
}

/// Run `cmd args` and return its trimmed stdout, or a human-readable error
/// string. A small mirror of doctor's capture helper — add-project needs only a
/// handful of version probes.
fn version_line(cmd: &str, args: &[&str]) -> std::result::Result<String, String> {
    match std::process::Command::new(cmd).args(args).output() {
        Ok(out) if out.status.success() => {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(out) => Err(format!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => Err(format!("not found ({e})")),
    }
}

/// Whether a GitHub `viewerPermission` string allows pushing. Shared with
/// `meguri doctor`; split from the gh call so it is unit-tested without the
/// network.
pub fn can_push(viewer_permission: &str) -> bool {
    matches!(viewer_permission, "ADMIN" | "MAINTAIN" | "WRITE")
}

/// The caller's permission on a repo, via `gh repo view`. `Err` means the check
/// was inconclusive (gh missing, network, private-repo visibility) — callers
/// treat that as a warning, not a failure. Shared with `meguri doctor`.
pub async fn gh_viewer_permission(slug: &str) -> Result<String> {
    let out = tokio::process::Command::new("gh")
        .args([
            "repo",
            "view",
            slug,
            "--json",
            "viewerPermission",
            "-q",
            ".viewerPermission",
        ])
        .output()
        .await
        .context("spawning gh")?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
}

/// The typed identity selector the three operator verbs share (ADR 0016):
/// exactly one of issue / PR / run / local task.
#[derive(Debug, Clone)]
pub enum RunSelector {
    Issue(i64),
    Pr(i64),
    RunId(String),
    Task(i64),
}

/// `meguri run` (ADR 0016): role-agnostic — resolve the selector to its
/// owning decider, freshly observe, and dispatch the arm the decider picks.
/// `--run` resumes with the run's stored loop kind; `--pr` routes to the
/// PR-side decider; `--issue` follows the ownership boundary (an open meguri
/// PR owns the identity); `--task` is the local decider. A manual run
/// bypasses the discovery throttles (already-shipped / cadence window) but
/// never the safety gates (hold/needs-human, busy, not-before) — and still
/// stamps the cadence bucket so the window's consumption is counted.
pub async fn cmd_run(
    project: Option<&str>,
    sel: RunSelector,
    mux_override: Option<&str>,
) -> Result<()> {
    let cfg = Config::load()?;
    crate::routing::validate(&cfg, &crate::routing::detect_command)?;
    crate::launch::validate(&cfg)?;
    crate::routing::warn_unsafe_preflight_overrides(&cfg);
    let project = pick_project(&cfg, project)?;
    if project.mode == ProjectMode::Local
        && !matches!(sel, RunSelector::Task(_) | RunSelector::RunId(_))
    {
        bail!(
            "a local project has no issues/PRs; use `meguri run --task <id>`              (or `meguri add` and let `meguri watch` pick it up)"
        );
    }
    let deps = build_deps(&cfg, project, mux_override)?;
    if project.mode != ProjectMode::Local {
        // Materialize the managed bare clone before anything touches
        // `repo_path` (ADR 0018) — the one-shot counterpart of the tick gate.
        engine::ensure_project_clone(&deps).await?;
    }

    match sel {
        RunSelector::RunId(id) => {
            // Keep the stored loop kind (finding 1): an existing run resumes
            // its own recipe, never re-routed through the issue side.
            let run = deps
                .store
                .find_run(&id)?
                .with_context(|| format!("no run matches {id:?}"))?;
            println!(
                "resuming run {} ({}, step {}) — watch with: meguri attach {}",
                run.id, run.loop_kind, run.step, run.id
            );
            let outcome = engine::run_recipe(&deps, &run.id, &run.loop_kind).await?;
            print_run_outcome(outcome)
        }
        RunSelector::Pr(n) => run_pr_side(&deps, n).await,
        RunSelector::Task(id) => run_local_task(&deps, id).await,
        RunSelector::Issue(n) => {
            // Ownership boundary (決定1): an open meguri PR owns the identity,
            // so `run --issue` on such an issue drives the PR-side decider.
            if let Some(pr) = engine::open_pr_for_issue(&deps, n).await? {
                println!(
                    "issue #{n} is owned by its open PR #{} — routing to the PR side",
                    pr.number
                );
                return run_pr_side(&deps, pr.number).await;
            }
            run_issue_side(&deps, n).await
        }
    }
}

/// PR-side manual run: fresh observe → the PR decider's step. An `Agent` arm
/// dispatches; anything else is explained and nothing launches.
async fn run_pr_side(deps: &engine::Deps, pr: i64) -> Result<()> {
    use crate::engine::issue_reconciler::{self, Step};
    let Some((_, step)) = issue_reconciler::observe_pr_step(deps, pr).await? else {
        bail!("PR #{pr} is not an open PR of this project");
    };
    match step {
        Step::Agent(arm) => {
            let obs_pr = engine::open_pr_by_number(deps, pr)
                .await?
                .with_context(|| format!("PR #{pr} vanished between observe and dispatch"))?;
            let issue = engine::canonical_key(&obs_pr);
            let run = match deps.store.create_run_for_loop(
                &deps.project.id,
                arm.loop_kind(),
                issue,
                &obs_pr.title,
            ) {
                Ok(run) => run,
                Err(_) => resume_existing(deps, issue)?,
            };
            println!(
                "run {} — PR #{pr} → {} — watch with: meguri attach {}",
                run.id,
                arm.loop_kind(),
                run.id
            );
            let outcome = engine::run_recipe(deps, &run.id, arm.loop_kind()).await?;
            print_run_outcome(outcome)
        }
        other => {
            println!("nothing to launch for PR #{pr}: {other:?}");
            Ok(())
        }
    }
}

/// Issue-side manual run (`Mode::ManualRun`): the decider's arm with the
/// discovery throttles bypassed and the safety gates kept.
async fn run_issue_side(deps: &engine::Deps, issue: i64) -> Result<()> {
    use crate::engine::issue_reconciler::{self, IssueStep, Mode};
    let gh_issue = deps.forge().get_issue(issue).await?;
    let open_prs = engine::reaper::open_meguri_pr_issues(deps).await;
    let snap = issue_reconciler::build_issue_snapshot(deps, &gh_issue, &open_prs).await?;
    match issue_reconciler::next_step_issue(&snap, Mode::ManualRun) {
        IssueStep::Agent(arm) => {
            let run = match deps.store.create_run_for_loop(
                &deps.project.id,
                arm.loop_kind(),
                issue,
                &gh_issue.title,
            ) {
                Ok(run) => run,
                Err(_) => resume_existing(deps, issue)?,
            };
            println!(
                "run {} — issue #{issue} {:?} → {} — watch with: meguri attach {}",
                run.id,
                gh_issue.title,
                arm.loop_kind(),
                run.id
            );
            let outcome = engine::run_recipe(deps, &run.id, &run.loop_kind).await?;
            print_run_outcome(outcome)
        }
        other => {
            println!("nothing to launch for issue #{issue}: {other:?}");
            Ok(())
        }
    }
}

/// Local-task manual run (`next_step_local`, ADR 0016): local mode's input
/// path — no more bail for local projects.
async fn run_local_task(deps: &engine::Deps, task_id: i64) -> Result<()> {
    use crate::engine::issue_reconciler::{
        LocalArm, LocalSnapshot, LocalStep, Mode, next_step_local,
    };
    let task = deps
        .store
        .get_task(task_id)?
        .with_context(|| format!("no local task {task_id}"))?;
    let snap = LocalSnapshot {
        human_stop: false,
        issue_busy: false,
        already_shipped: false,
        deps_unmet: false,
    };
    match next_step_local(&snap, Mode::ManualRun) {
        LocalStep::Agent(LocalArm::Worker) => {
            let run = match deps.store.create_run_for_task(
                &deps.project.id,
                engine::worker::KIND,
                task_id,
                &task.title,
            ) {
                Ok(run) => run,
                Err(_) => {
                    let existing = deps
                        .store
                        .list_runs(true)?
                        .into_iter()
                        .find(|r| r.project_id == deps.project.id && r.task_id == Some(task_id))
                        .context("an active run exists but could not be loaded")?;
                    println!("resuming run {} (step {})", existing.id, existing.step);
                    existing
                }
            };
            println!("run {} — task {task_id} {:?}", run.id, task.title);
            let outcome = engine::run_recipe(deps, &run.id, engine::worker::KIND).await?;
            print_run_outcome(outcome)
        }
        other => {
            println!("nothing to launch for task {task_id}: {other:?}");
            Ok(())
        }
    }
}

/// Resume the active run of `issue` when creation hit the unique index.
fn resume_existing(deps: &engine::Deps, issue: i64) -> Result<crate::store::RunRecord> {
    let existing = deps
        .store
        .list_runs(true)?
        .into_iter()
        .find(|r| r.project_id == deps.project.id && r.issue_number == issue)
        .context("an active run exists but could not be loaded")?;
    println!("resuming run {} (step {})", existing.id, existing.step);
    Ok(existing)
}

/// The human-facing outcome of a synchronously-driven run.
fn print_run_outcome(outcome: WorkerOutcome) -> Result<()> {
    match outcome {
        WorkerOutcome::Succeeded { pr_url } => {
            println!("✅ PR: {pr_url}");
            Ok(())
        }
        WorkerOutcome::Stopped => {
            println!("🛑 stopped");
            Ok(())
        }
        WorkerOutcome::Interrupted(reason) => {
            bail!(
                "run interrupted: {reason} — rerun `meguri run` with the same selector to resume"
            );
        }
        WorkerOutcome::Skipped(reason) => {
            println!("⏭️  skipped: {reason}");
            Ok(())
        }
    }
}

pub async fn cmd_watch() -> Result<()> {
    let mut reloader = config::ConfigReloader::load(&config::config_path())?;
    let cfg = reloader.current().clone();
    crate::routing::validate(&cfg, &crate::routing::detect_command)?;
    crate::launch::validate(&cfg)?;
    crate::routing::warn_unsafe_preflight_overrides(&cfg);
    if cfg.projects.is_empty() {
        bail!(
            "no projects configured — edit {}",
            config::config_path().display()
        );
    }

    // Single-instance guard: held for the watch's whole lifetime, so a second
    // scheduler fails loudly instead of double-driving. The OS releases the
    // flock on any exit — no stale-lock cleanup.
    let _lock = acquire_watch_lock(&config::meguri_home())?;

    let mut projects = Vec::new();
    for project in &cfg.projects {
        projects.push(build_deps(&cfg, project, None)?);
    }

    // Auto-merge fail-fast (ADR 0003): if a project enabled auto-merge but its
    // repository can't honor it, refuse to start rather than degrade silently
    // at merge time.
    for deps in &projects {
        auto_merge_preflight(deps).await?;
    }

    println!(
        "watching {} project(s) for {} issues (poll {}s, slots {})",
        projects.len(),
        crate::forge::LABEL_READY,
        cfg.scheduler.poll_interval_secs,
        cfg.scheduler.max_concurrent_runs,
    );

    // Hot reload (issue #73): every tick re-reads config.toml, so edits reach
    // the runs spawned after them without a restart.
    let reload = Box::new(move || {
        let next = reloader.poll(|_prev, next| {
            let mut fresh = Vec::new();
            for project in &next.projects {
                fresh.push(build_deps(next, project, None)?);
            }
            Ok(Reload {
                projects: fresh,
                poll_interval: Duration::from_secs(next.scheduler.poll_interval_secs),
                max_concurrent: next.scheduler.max_concurrent_runs as usize,
            })
        })?;
        Some(next)
    });

    let scheduler = Scheduler {
        projects,
        // ADR 0012 §決定8: production dispatches via the recipe table.
        recipe: crate::engine::default_recipe(),
        poll_interval: Duration::from_secs(cfg.scheduler.poll_interval_secs),
        max_concurrent: cfg.scheduler.max_concurrent_runs as usize,
        reload: Some(reload),
    };
    scheduler.watch().await
}

/// Exclusive flock on `daemon/watch.lock`, held for the watch's lifetime.
fn acquire_watch_lock(home: &Path) -> Result<std::fs::File> {
    let path = home.join("daemon").join("watch.lock");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open lock file {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("meguri watch is already running")
        }
        Err(std::fs::TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("cannot lock {}", path.display()))
        }
    }
}

/// Startup fail-fast for one project's auto-merge config (ADR 0003): if
/// enabled, the repository must allow auto-merge, permit the configured
/// strategy, and (when required) carry required-checks branch protection.
/// A miss bails with every reason so the operator fixes them at once.
async fn auto_merge_preflight(deps: &Deps) -> Result<()> {
    let am = &deps.config.pr_for(&deps.project).auto_merge;
    if !am.enabled {
        return Ok(());
    }
    // Auto-merge is a GitHub-PR concern; a forge-less (local-mode) project has
    // no PRs to arm, so there is nothing to fail-fast on.
    let Some(forge) = &deps.forge else {
        return Ok(());
    };
    let slug = deps
        .project
        .repo_slug
        .as_deref()
        .unwrap_or(&deps.project.id);
    let policy = forge
        .merge_policy(&deps.project.default_branch, am.require_branch_protection)
        .await
        .with_context(|| format!("cannot read merge settings for {slug} to validate auto-merge"))?;
    if let Err(problems) = crate::engine::issue_reconciler::validate_policy(am, &policy) {
        bail!(
            "auto-merge is enabled for project `{}` ({}) but the repository cannot \
             honor it:\n  - {}",
            deps.project.id,
            slug,
            problems.join("\n  - "),
        );
    }
    Ok(())
}

pub async fn cmd_prune(project: Option<&str>, dry_run: bool, force: bool) -> Result<()> {
    let cfg = Config::load()?;
    let projects: Vec<&ProjectConfig> = match project {
        Some(id) => vec![pick_project(&cfg, Some(id))?],
        None => cfg.projects.iter().collect(),
    };
    if projects.is_empty() {
        bail!(
            "no projects configured — edit {}",
            config::config_path().display()
        );
    }

    for project in projects {
        let deps = build_deps(&cfg, project, None)?;
        let mut states = reaper::IssueStates::default();
        // Manual prune honors the same open-PR ownership boundary as the
        // automatic Finalize pass (finding 4b).
        let open_pr_issues = reaper::open_meguri_pr_issues(&deps).await;
        let pane_candidates = reaper::plan_panes(&deps, &mut states, &open_pr_issues).await?;

        // Panes go first so their worktrees become reclaimable in this same
        // pass (a closed issue's live pane no longer protects its worktree).
        if !pane_candidates.is_empty() {
            println!("{}:", project.id);
            println!("  {:<9} {:<18} PANE", "ISSUE", "STATE");
            for c in &pane_candidates {
                let state = match c.verdict {
                    reaper::Verdict::Reclaim => "reclaim".to_string(),
                    other => format!("{} (skip)", other.as_str()),
                };
                println!("  {:<9} {:<18} {}", format!("#{}", c.issue), state, c.pane);
            }
        }
        if !dry_run {
            let reclaimed = reaper::reclaim_panes(&deps, &pane_candidates).await?;
            if !reclaimed.is_empty() {
                println!("  reclaimed {} pane(s)", reclaimed.len());
                for p in &reclaimed {
                    if let Some(id) = &p.agent_session_id {
                        println!("  saved session for #{}: claude --resume {id}", p.issue);
                    }
                }
            }
        }

        let candidates = reaper::plan_with(&deps, &mut states, &open_pr_issues).await?;
        if candidates.is_empty() {
            if pane_candidates.is_empty() {
                println!("{}: no meguri panes or worktrees", project.id);
            }
            continue;
        }

        if pane_candidates.is_empty() {
            println!("{}:", project.id);
        }
        println!("  {:<9} {:<18} {:>9}  PATH", "ISSUE", "STATE", "SIZE");
        for c in &candidates {
            let state = match c.verdict {
                reaper::Verdict::Reclaim => "reclaim".to_string(),
                reaper::Verdict::Dirty if force => "reclaim (dirty)".to_string(),
                reaper::Verdict::Dirty => "dirty (skip)".to_string(),
                other => format!("{} (skip)", other.as_str()),
            };
            println!(
                "  {:<9} {:<18} {:>9}  {}",
                c.issue
                    .map(|n| format!("#{n}"))
                    .unwrap_or_else(|| "-".into()),
                state,
                human_size(reaper::dir_size(&c.path)),
                c.path.display(),
            );
        }
        if dry_run {
            continue;
        }

        let reclaimed = reaper::reclaim(&deps, &candidates, force).await?;
        let dirty_skipped = candidates
            .iter()
            .filter(|c| c.verdict == reaper::Verdict::Dirty)
            .count();
        println!("  reclaimed {} worktree(s)", reclaimed.len());
        for r in &reclaimed {
            if !r.branch_deleted
                && let Some(branch) = &r.branch
            {
                println!("  kept branch {branch} (not merged; delete with --force)");
            }
        }
        if !force && dirty_skipped > 0 {
            println!("  skipped {dirty_skipped} dirty worktree(s) — rerun with --force to reclaim");
        }
    }
    Ok(())
}

fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn require_run(store: &Store, needle: &str) -> Result<RunRecord> {
    store
        .find_run(needle)?
        .with_context(|| format!("no run matches {needle:?} (try `meguri ps --all`)"))
}

/// local-mode capture: queue a task in the sqlite `tasks` table for the watch
/// to pick up (issue #148 / ADR 0003). The project is already resolved and
/// mode-checked by [`cmd_add`]. Always `TaskKind::Work` — `--plan` is rejected
/// by [`check_add_flags`] until local mode grows a planner (issue #54).
fn add_local(project: &ProjectConfig, title: Option<&str>, file: Option<&str>) -> Result<()> {
    let (title, body) = resolve_task_input(title, file)?;
    let kind = TaskKind::Work;
    let store = open_store()?;
    let task = store.create_task(&project.id, kind.as_str(), &title, &body, "local")?;
    println!(
        "queued task #{} [{}] {}",
        task.id,
        kind.as_str(),
        task.title
    );
    println!("`meguri watch` will pick it up within one poll interval.");
    Ok(())
}

/// Resolve a task's `(title, body)` from an optional title argument and an
/// optional `--file`. `--file` loads the markdown as the body and, absent an
/// explicit title, lifts the first heading line as the title.
fn resolve_task_input(title: Option<&str>, file: Option<&str>) -> Result<(String, String)> {
    match file {
        Some(path) => {
            let body = std::fs::read_to_string(path)
                .with_context(|| format!("cannot read task file {path}"))?;
            let title = match title {
                Some(t) => t.to_string(),
                None => first_heading(&body)
                    .context("--file has no heading line; pass a title explicitly")?,
            };
            Ok((title, body))
        }
        None => {
            let title = title
                .context("provide a task title (or --file <path>)")?
                .to_string();
            Ok((title, String::new()))
        }
    }
}

/// The first non-empty line of a markdown document, with leading `#`/spaces
/// stripped — the task title lifted from a `--file`.
fn first_heading(markdown: &str) -> Option<String> {
    let line = markdown.lines().find(|l| !l.trim().is_empty())?;
    Some(line.trim_start_matches('#').trim().to_string())
}

/// `meguri tasks`: inspect a project's discovery queue and why each item is (or
/// is not) running. In local mode it lists the local tasks; in github mode it
/// fetches the `ready`/`plan` issues and shows each one's disposition — the same
/// gate discovery applies (issue #148), so silently-skipped work (not-before /
/// cadence) that leaves no trace on the forge is visible here.
pub async fn cmd_tasks(project: Option<&str>, all: bool) -> Result<()> {
    let cfg = Config::load()?;
    let project = pick_project(&cfg, project)?;
    match project.mode {
        ProjectMode::Local => cmd_tasks_local(project, all),
        ProjectMode::Github => {
            let _ = cfg;
            bail!("`meguri tasks` lists local tasks; github mode uses issue labels")
        }
    }
}

/// Local-mode listing: the sqlite `tasks`, with a not-before annotation on any
/// still-queued task whose gate has not yet opened.
fn cmd_tasks_local(project: &ProjectConfig, all: bool) -> Result<()> {
    let store = open_store()?;
    let tasks = store.list_tasks(&project.id, all)?;
    if tasks.is_empty() {
        println!("no {}tasks", if all { "" } else { "open " });
        return Ok(());
    }
    println!("{:>4}  {:<6} {:<12} TITLE", "ID", "KIND", "STATUS");
    for t in tasks {
        let flag = if t.status == "needs_human" {
            "⚠️ "
        } else {
            ""
        };
        println!(
            "{:>4}  {:<6} {}{:<12} {}",
            t.id, t.kind, flag, t.status, t.title
        );
        if let Some(reason) = t.reason.filter(|_| t.status == "needs_human") {
            println!("        ↳ {reason}");
        }
        if t.status == "queued"
            && let Some(nb) = &t.not_before
        {
            println!("        ↳ ⏳ not-before 待ち(until {nb})");
        }
    }
    Ok(())
}

pub fn cmd_ps(all: bool) -> Result<()> {
    let store = open_store()?;
    let runs = store.list_runs(!all)?;
    if runs.is_empty() {
        println!("no {}runs", if all { "" } else { "active " });
        return Ok(());
    }
    // Workspace grouping (issue #154) is display-only and opt-in: with no
    // workspaces configured — or an unreadable config — the listing is exactly
    // as before (acceptance criterion 5). The same config also resolves each
    // run's launch mode (issue #169) — unreadable config falls back to "-"
    // rather than guessing.
    let cfg = Config::load().ok();
    let print_header = || {
        println!(
            "{:<14} {:<8} {:>6}  {:<12} {:<16} {:<10} {:<14} {:<7} PANE",
            "RUN", "PROJECT", "TARGET", "STATUS", "INTERACTION", "STEP", "PROFILE", "MODE"
        );
    };
    let print_row = |run: &RunRecord| {
        // A github run is keyed by its issue (`#7`), a local run by its task
        // row (`t3`); the branch prefix uses the same convention.
        let target = match run.task_key() {
            crate::tasks::TaskKey::Issue(n) => format!("#{n}"),
            crate::tasks::TaskKey::Local(id) => format!("t{id}"),
        };
        let mode = cfg.as_ref().map_or("-", |c| {
            crate::launch::resolve(c, crate::routing::routing_role_for_loop(&run.loop_kind))
                .as_str()
        });
        println!(
            "{:<14} {:<8} {:>6}  {:<12} {:<16} {:<10} {:<14} {:<7} {}",
            run.id,
            run.project_id,
            target,
            run.status.as_str(),
            run.interaction_state.map(|s| s.as_str()).unwrap_or("-"),
            run.step,
            run.agent_profile.as_deref().unwrap_or("-"),
            mode,
            run.mux_pane_id.as_deref().unwrap_or("-"),
        );
    };

    let groups = group_by_workspace(cfg.as_ref(), &runs);
    match groups {
        None => {
            print_header();
            for run in &runs {
                print_row(run);
            }
        }
        Some(groups) => {
            for (i, (label, group_runs)) in groups.iter().enumerate() {
                if i > 0 {
                    println!();
                }
                println!("[{label}]");
                print_header();
                for run in group_runs {
                    print_row(run);
                }
            }
        }
    }
    Ok(())
}

/// Group runs by the workspace their project belongs to, for display only
/// (issue #154). Returns `None` when no workspaces are configured (the caller
/// then prints the flat, unchanged listing); otherwise groups in config order,
/// with an "(no workspace)" bucket last for projects that joined none. Empty
/// groups are omitted so a workspace with no active runs prints nothing.
fn group_by_workspace<'a>(
    cfg: Option<&Config>,
    runs: &'a [RunRecord],
) -> Option<Vec<(String, Vec<&'a RunRecord>)>> {
    let cfg = cfg?;
    if cfg.workspaces.is_empty() {
        return None;
    }
    let mut groups: Vec<(String, Vec<&RunRecord>)> = Vec::new();
    for ws in &cfg.workspaces {
        let members: Vec<&RunRecord> = runs
            .iter()
            .filter(|r| ws.projects.iter().any(|p| p == &r.project_id))
            .collect();
        if !members.is_empty() {
            groups.push((format!("workspace: {}", ws.id), members));
        }
    }
    let orphans: Vec<&RunRecord> = runs
        .iter()
        .filter(|r| cfg.workspace_of(&r.project_id).is_none())
        .collect();
    if !orphans.is_empty() {
        groups.push(("no workspace".to_string(), orphans));
    }
    Some(groups)
}

pub async fn cmd_logs(needle: &str) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let run = require_run(&store, needle)?;

    for event in store.events_for_run(&run.id, 100)? {
        println!("{} {:<24} {}", event.ts, event.kind, event.data);
    }

    if let (Some(kind), Some(pane)) = (&run.mux_kind, &run.mux_pane_id)
        // Addresses an existing pane by id; no project-scoped label needed.
        && let Ok(mux) = mux::from_kind(kind, &cfg.mux.session, None)
    {
        let pane = mux::PaneId(pane.clone());
        if mux.pane_alive(&pane).await.unwrap_or(false) {
            println!("\n--- pane tail ({kind} {pane}) ---");
            for line in mux.read_tail(&pane, 25).await.unwrap_or_default() {
                println!("{line}");
            }
            println!("--- attach: {} ---", mux.attach_command(&pane));
        }
    }
    Ok(())
}

/// Fold the four selector flags into exactly one [`RunSelector`] (ADR 0016).
pub fn selector(
    issue: Option<i64>,
    pr: Option<i64>,
    run: Option<String>,
    task: Option<i64>,
) -> Result<RunSelector> {
    let mut picked: Vec<RunSelector> = Vec::new();
    if let Some(n) = issue {
        picked.push(RunSelector::Issue(n));
    }
    if let Some(n) = pr {
        picked.push(RunSelector::Pr(n));
    }
    if let Some(id) = run {
        picked.push(RunSelector::RunId(id));
    }
    if let Some(id) = task {
        picked.push(RunSelector::Task(id));
    }
    match picked.len() {
        1 => Ok(picked.remove(0)),
        0 => bail!("pass exactly one of --issue / --pr / --run / --task"),
        _ => bail!("--issue / --pr / --run / --task are mutually exclusive"),
    }
}

/// `meguri attach` (ADR 0016): resolve any of the four identities to its live
/// pane. The positional issue-number/run-id argument stays for back-compat;
/// `--pr` resolves the PR's canonical issue (pane lanes are issue-keyed) and
/// `--task` the task's latest run.
pub async fn cmd_attach(
    needle: Option<&str>,
    issue: Option<i64>,
    pr: Option<i64>,
    run_id: Option<&str>,
    task: Option<i64>,
) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let needle: String = if let Some(n) = needle {
        n.to_string()
    } else if let Some(n) = issue {
        n.to_string()
    } else if let Some(id) = run_id {
        id.to_string()
    } else if let Some(n) = pr {
        // A pane lane is keyed by the canonical issue, so resolve the PR to
        // it through the forge (the one selector that needs a read).
        let project = pick_project(&cfg, None)?;
        let deps = build_deps(&cfg, project, None)?;
        let pr_obj = engine::open_pr_by_number(&deps, n)
            .await?
            .with_context(|| format!("no open PR #{n}"))?;
        engine::canonical_key(&pr_obj).to_string()
    } else if let Some(id) = task {
        // Local runs have no issue number; their panes hang off the run.
        let run = store
            .list_runs(false)?
            .into_iter()
            .filter(|r| r.task_id == Some(id))
            .max_by(|a, b| a.created_at.cmp(&b.created_at))
            .with_context(|| format!("no runs for local task {id}"))?;
        run.id
    } else {
        bail!("pass an issue/run id, or one of --issue / --pr / --run-id / --task");
    };
    let (kind, pane) = resolve_attach_pane(&store, &needle)?;
    // Attach addresses an existing pane by id; the tmux attach command resolves
    // the pane's own session, so no project-scoped label is needed here.
    let mux = mux::from_kind(&kind, &cfg.mux.session, None)?;
    let command = mux.attach_command(&mux::PaneId(pane));
    println!("attaching: {command}");
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("sh")
        .arg("-c")
        .arg(&command)
        .exec();
    bail!("exec failed: {err}");
}

/// Resolve what `meguri attach <needle>` should attach to. Panes belong to
/// the issue's author lane (kept until the issue closes), so the issue's
/// persistent lane pane wins over whatever pane id a run once recorded — and
/// a bare issue number keeps working after its runs finished.
fn resolve_attach_pane(store: &Store, needle: &str) -> Result<(String, String)> {
    let wanted_lane = LANE_AUTHOR;
    if let Some(run) = store.find_run(needle)? {
        // Derive the run's lane from its loop kind: a pr-review-lane run
        // resolves its pr-review pane, everything else the author pane —
        // `--review` only matters for the bare-issue-number path below.
        let lane = engine::lane_for_loop(&run.loop_kind);
        if let Some(p) = store.get_pane(&run.project_id, run.issue_number, lane)?
            && let (Some(kind), Some(id)) = (p.mux_kind, p.mux_pane_id)
        {
            return Ok((kind, id));
        }
        if let (Some(kind), Some(id)) = (&run.mux_kind, &run.mux_pane_id) {
            return Ok((kind.clone(), id.clone()));
        }
        bail!("run {} has no pane yet", run.id);
    }
    if let Ok(issue) = needle.parse::<i64>() {
        let panes: Vec<_> = store
            .panes_for_issue(issue)?
            .into_iter()
            .filter(|p| p.lane == wanted_lane)
            .collect();
        match panes.as_slice() {
            [] => {}
            [p] => {
                if let (Some(kind), Some(id)) = (&p.mux_kind, &p.mux_pane_id) {
                    return Ok((kind.clone(), id.clone()));
                }
            }
            many => {
                let projects: Vec<&str> = many.iter().map(|p| p.project_id.as_str()).collect();
                bail!(
                    "issue #{issue} has {wanted_lane} panes in multiple projects ({}) — \
                     pass a run id instead",
                    projects.join(", ")
                );
            }
        }
    }
    bail!("no run or pane matches {needle:?} (try `meguri ps --all`)")
}

fn set_desired(needle: &str, desired: Option<DesiredState>, verb: &str) -> Result<()> {
    let store = open_store()?;
    let run = require_run(&store, needle)?;
    if !run.status.is_active() {
        bail!("run {} is {}; cannot {verb}", run.id, run.status.as_str());
    }
    store.set_desired_state(&run.id, desired)?;
    store.emit(
        Some(&run.id),
        "control.requested",
        serde_json::json!({ "verb": verb }),
    )?;
    println!("{verb} requested for {}", run.id);
    Ok(())
}

pub fn cmd_pause(needle: &str) -> Result<()> {
    set_desired(needle, Some(DesiredState::Paused), "pause")
}

pub fn cmd_resume(needle: &str) -> Result<()> {
    set_desired(needle, None, "resume")
}

pub fn cmd_takeover(needle: &str) -> Result<()> {
    let out = set_desired(needle, Some(DesiredState::Takeover), "takeover");
    if out.is_ok() {
        println!("the orchestrator is hands-off; `meguri attach` and drive the agent.");
        println!("hand control back with: meguri handback <run>");
    }
    out
}

pub fn cmd_handback(needle: &str) -> Result<()> {
    set_desired(needle, None, "handback")
}

pub async fn cmd_stop(needle: &str) -> Result<()> {
    let cfg = Config::load()?;
    let store = open_store()?;
    let run = require_run(&store, needle)?;
    if !run.status.is_active() {
        bail!("run {} is already {}", run.id, run.status.as_str());
    }
    store.set_desired_state(&run.id, Some(DesiredState::Stopped))?;

    if run.status == RunStatus::Running {
        // A live driver will observe desired=stopped within a poll tick and
        // finalize (labels, pane, status) itself.
        println!(
            "stop requested for {}; the orchestrator will finalize it",
            run.id
        );
        return Ok(());
    }

    // No driver is running this (queued/interrupted): finalize here.
    store.update_run_status(&run.id, RunStatus::Cancelled, Some("stopped by user"))?;
    let released = match cfg.project(&run.project_id) {
        Some(project) => match build_deps(&cfg, project, None) {
            Ok(deps) => {
                // Session id is saved before the kill — resumable later.
                let released = reaper::release_pane(
                    &deps,
                    run.issue_number,
                    engine::lane_for_loop(&run.loop_kind),
                    "stopped by user",
                )
                .await;
                // Drop the claim through the coordination layer, keyed by
                // whatever this run targets (github: the working label; local:
                // back to queued).
                let _ = deps.task_source.release(&run.task_key()).await;
                // A PR-claiming loop (fixer family, spec_worker, pr-reviewer)
                // tracks its claim in the run's checkpoint, not the
                // coordination layer above — the live-driver finalize path
                // (`engine::flow::finalize_cancelled` / pr_reviewer's own)
                // knows to drop it via each loop's `Flavor`, but this
                // no-driver path never reaches a `Flavor`. Mirror that
                // release directly (issue #252).
                engine::flow::release_stray_pr_claim(&deps, &run).await;
                released.is_some()
            }
            Err(_) => false,
        },
        None => false,
    };
    if !released
        && let (Some(kind), Some(pane)) = (&run.mux_kind, &run.mux_pane_id)
        && let Ok(mux) = mux::from_kind(kind, &cfg.mux.session, None)
    {
        // Fallback for panes that predate the pane registry. Kills by pane id,
        // so the base label is fine — no project-scoped workspace needed.
        let _ = mux.kill_pane(&mux::PaneId(pane.clone())).await;
    }
    store.emit(Some(&run.id), "run.cancelled", serde_json::json!({}))?;
    println!("run {} cancelled", run.id);
    Ok(())
}
