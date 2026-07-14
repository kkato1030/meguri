//! The worker loop: `meguri:ready` issue → worktree → interactive agent
//! turns in a mux pane → verified commits → implementation PR. The heavy
//! lifting lives in [`super::flow`]; this module only plugs in the
//! worker-specific label, prompt, and PR shape.
//!
//! Lifetime (issue #92): keyed by the issue, new branch and worktree, pane
//! in the issue's author lane — kept after success and shared with every
//! later loop on the same branch (fixer, ci-fixer, conflict resolver), so
//! the implementation context continues; the reaper reclaims it when the
//! issue closes.

use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

pub use super::WorkerOutcome;
use super::flow::{self, Checkpoint, Flavor, NeedsHuman};
use super::{Deps, Target};
use crate::config::Deliver;
use crate::forge;
use crate::store::RunRecord;
use crate::tasks::{TaskKey, TaskKind};

/// `runs.loop_kind` value for worker runs (the schema default).
pub const KIND: &str = "worker";

/// The worker as a schedulable loop: `meguri:ready` issues in, PRs out.
pub struct WorkerLoop;

#[async_trait]
impl super::Loop for WorkerLoop {
    fn kind(&self) -> &'static str {
        KIND
    }

    async fn discover(&self, deps: &Deps) -> Result<Vec<Target>> {
        Ok(deps
            .task_source
            .discover(TaskKind::Work)
            .await?
            .into_iter()
            .map(|t| Target {
                key: t.key,
                title: t.title,
                cadence_label: t.cadence_label,
            })
            .collect())
    }

    async fn drive(&self, deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
        run_worker(deps, run_id).await
    }
}

pub async fn run_worker(deps: &Deps, run_id: &str) -> Result<WorkerOutcome> {
    let flavor = WorkerFlavor {
        separate_delivery: deps.project.plan_delivery == crate::config::PlanDelivery::Separate,
    };
    flow::run_flow(deps, run_id, &flavor).await
}

struct WorkerFlavor {
    /// Whether the project uses separate plan delivery (ADR 0008): the worker
    /// then reads/prunes a landed spec (finding 1). Carried on the flavor
    /// because [`Flavor::verify_work`] has no `deps`.
    separate_delivery: bool,
}

#[async_trait]
impl Flavor for WorkerFlavor {
    fn trigger_label(&self) -> &'static str {
        forge::LABEL_READY
    }

    /// The worker self-reviews its own diff before opening the PR (ADR 0006):
    /// the internal review→fix loop runs in the run's worktree with no forge
    /// calls, so the human sees an already-self-reviewed PR.
    fn self_reviews(&self) -> bool {
        true
    }

    fn execute_prompt(
        &self,
        deps: &Deps,
        run: &RunRecord,
        cp: &Checkpoint,
        worktree: &Path,
    ) -> String {
        let branch = run.branch.as_deref().unwrap_or("?");
        let lang_section = flow::language_instruction(deps.config.language_for(&deps.project));
        // The PR-body section only matters when the deliverable is a PR.
        let pr_section = if deps.config.deliver_for(&deps.project) == Deliver::Pr {
            flow::pr_body_instruction(worktree)
        } else {
            String::new()
        };
        // Separate delivery (ADR 0008 finding 1): if a reviewed spec landed on
        // the default branch (from a merged spec/ADR PR), the worker inherits
        // the spec worker's read-and-prune responsibilities — inject the spec
        // and ask for its deletion. A normal issue with no spec degrades to the
        // ordinary flow (the section is empty).
        let spec_section = if deps.project.plan_delivery == crate::config::PlanDelivery::Separate {
            match run.task_key() {
                TaskKey::Issue(number) => {
                    super::spec_worker::reviewed_spec_section(worktree, number).unwrap_or_default()
                }
                TaskKey::Local(_) => String::new(),
            }
        } else {
            String::new()
        };
        // Collab advisor consult block (issue #111): non-empty only when a live
        // advisor pane exists for this run (collab on + spawn succeeded).
        let consult_section = flow::advisor_consult_section(deps, run);
        match run.task_key() {
            // github issue: the familiar prompt, including the needs-plan
            // handoff (only the label flow has a planner to hand to).
            TaskKey::Issue(number) => format!(
                "You are implementing GitHub issue #{number} in this repository \
                 (branch `{branch}`, a dedicated worktree).\n\n\
                 # Issue: {title}\n\n{body}\n\n\
                 {spec_section}\
                 # Instructions\n\
                 - Explore the repository first and follow its existing conventions.\n\
                 - Implement the issue completely, including tests where the project has them.\n\
                 - Run the relevant tests/checks yourself before declaring success.\n\
                 - COMMIT all your work to the current branch with clear messages. \
                   Leave the working tree clean.\n\
                 - Do NOT push and do NOT create a pull request; meguri handles both.\n\
                 - Do NOT switch branches or touch other worktrees.\n\n\
                 # Needs a design decision first?\n\
                 If your investigation shows a design decision must be settled before \
                 this issue can be implemented, do NOT implement a guess. Instead end \
                 the turn with `\"status\": \"needs_plan\"` in the result file (accepted \
                 here in addition to the completion contract's statuses) and put one \
                 paragraph in `summary` explaining what you found and which decision \
                 is needed. meguri will hand the issue to the planning flow with that \
                 paragraph.\n\n\
                 {consult_section}{pr_section}{lang_section}",
                title = cp.issue_title,
                body = cp.issue_body,
            ),
            // local task: no issue number, no planner handoff; the deliverable
            // is the verified branch.
            TaskKey::Local(_) => format!(
                "You are implementing a local task in this repository \
                 (branch `{branch}`, a dedicated worktree).\n\n\
                 # Task: {title}\n\n{body}\n\n\
                 # Instructions\n\
                 - Explore the repository first and follow its existing conventions.\n\
                 - Implement the task completely, including tests where the project has them.\n\
                 - Run the relevant tests/checks yourself before declaring success.\n\
                 - COMMIT all your work to the current branch with clear messages. \
                   Leave the working tree clean.\n\
                 - Do NOT push and do NOT create a pull request; meguri leaves the \
                   verified branch in place for you to review.\n\
                 - Do NOT switch branches or touch other worktrees.\n\n\
                 {pr_section}{lang_section}",
                title = cp.issue_title,
                body = cp.issue_body,
            ),
        }
        // The completion contract is appended by prepare_turn.
    }

    fn verify_work(
        &self,
        run: &RunRecord,
        _cp: &Checkpoint,
        worktree: &Path,
    ) -> std::result::Result<(), String> {
        // Separate delivery (ADR 0008 finding 1): the spec is disposable, so a
        // spec that survived implementation gets a corrective turn — the same
        // symmetric check the spec worker runs under combined delivery.
        if let TaskKey::Issue(issue) = run.task_key()
            && self.separate_delivery
        {
            return super::spec_worker::verify_spec_pruned(worktree, issue);
        }
        Ok(()) // committed work is all the worker requires
    }

    fn pr_title(&self, run: &RunRecord, cp: &Checkpoint) -> String {
        flow::default_pr_title(run, cp)
    }

    /// Phase transition (ADR 0005) + claim release. In github mode the issue's
    /// `meguri:ready` becomes `meguri:implementing` — the implementation PR is
    /// now open. The `implementing` add is load-bearing (it backs the
    /// "unlabeled = untriaged" invariant), so it runs *before* the claim is
    /// released (which drops `working`+`ready`), keeping the issue always
    /// labeled; failing the add fails the run. The coordination layer's
    /// `complete` then releases the claim: github drops `working`+`ready`
    /// best-effort, local flips the task to `done`. No-op forge in local mode.
    async fn settle_labels(&self, deps: &Deps, run: &RunRecord, _cp: &Checkpoint) -> Result<()> {
        if let Some(f) = &deps.forge {
            f.add_label(run.issue_number, forge::LABEL_IMPLEMENTING)
                .await?;
        }
        deps.task_source.complete(&run.task_key()).await
    }

    /// needs-plan demotion (issue #22): release the claim and swap the issue
    /// to `meguri:plan`, leaving the agent's findings as a comment for the
    /// planner to pick up on the next poll. Ping-pong guard (the one-shot
    /// rule, hardened by issue #135): a spec on disk means the issue has been
    /// through planning already; a prior `needs_plan` run on record means it
    /// retreated once already even if no spec landed. Either trips the
    /// guard — a second needs-plan hands the issue to a human instead of
    /// bouncing `ready` ⇄ `plan` forever.
    async fn on_needs_plan(
        &self,
        deps: &Deps,
        run: &RunRecord,
        worktree: &Path,
        reason: &str,
    ) -> Result<WorkerOutcome> {
        // Local mode has no planner loop yet (issue #54 Phase 3), so a local
        // task that asks for a plan goes to a human via the task source.
        if let TaskKey::Local(_) = run.task_key() {
            return Err(NeedsHuman(format!(
                "a local task asked for a plan, but local mode has no planner \
                 yet (issue #54 Phase 3): {reason}"
            ))
            .into());
        }
        let spec = super::planner::spec_rel_path(run.issue_number);
        if worktree.join(&spec).is_file() {
            return Err(NeedsHuman(format!(
                "agent asked for a plan on issue #{} but a spec (`{spec}`) \
                 already exists — planning did not resolve it: {reason}",
                run.issue_number
            ))
            .into());
        }
        if deps
            .store
            .issue_has_needs_plan_run(&deps.project.id, KIND, run.issue_number)?
        {
            return Err(NeedsHuman(format!(
                "agent asked for a plan on issue #{} but this issue already \
                 retreated to planning once before — the ready/plan cycle \
                 isn't converging: {reason}",
                run.issue_number
            ))
            .into());
        }
        // Comment first so the planner's prompt (built from the issue +
        // comments) always sees the findings once the label is on.
        deps.forge()
            .comment(
                run.issue_number,
                &format!(
                    "🔁 **meguri**: the worker found that a design decision is \
                     needed before implementation, so this issue moves to the \
                     planning flow (`{plan}`).\n\n> {reason}",
                    plan = forge::LABEL_PLAN
                ),
            )
            .await?;
        // The plan label is load-bearing (planner discovery keys off it), so
        // failing to apply it fails the run instead of passing silently; the
        // removals are best-effort like every other label release.
        deps.forge()
            .add_label(run.issue_number, forge::LABEL_PLAN)
            .await?;
        deps.forge()
            .remove_label(run.issue_number, forge::LABEL_READY)
            .await
            .ok();
        deps.forge()
            .remove_label(run.issue_number, forge::LABEL_WORKING)
            .await
            .ok();
        deps.store.emit(
            Some(&run.id),
            "issue.needs_plan",
            json!({ "issue": run.issue_number }),
        )?;
        Ok(WorkerOutcome::NeedsPlan(reason.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::{Config, ProjectConfig};
    use crate::forge::fake::FakeForge;
    use crate::store::Store;

    #[test]
    fn pr_title_prefers_subject_and_falls_back_to_issue_title() {
        let (_deps, run, _forge) = fake_env(&[forge::LABEL_READY]);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        };
        let flavor = WorkerFlavor {
            separate_delivery: false,
        };
        assert_eq!(flavor.pr_title(&run, &cp), "Add caching (#7)");

        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            subject: Some("Cache API responses in memory".into()),
            ..Default::default()
        };
        assert_eq!(
            flavor.pr_title(&run, &cp),
            "Cache API responses in memory (#7)"
        );
    }

    #[test]
    fn prompt_invites_needs_plan() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, run, _forge) = fake_env(&[forge::LABEL_READY]);
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            issue_body: "Cache the thing.".into(),
            ..Default::default()
        };
        let prompt = WorkerFlavor {
            separate_delivery: false,
        }
        .execute_prompt(&deps, &run, &cp, dir.path());
        assert!(prompt.contains("# Issue: Add caching"));
        assert!(prompt.contains("# Needs a design decision first?"));
        assert!(prompt.contains(r#""status": "needs_plan""#));
    }

    #[test]
    fn consult_block_only_appears_when_advisor_is_live() {
        let dir = tempfile::tempdir().unwrap();
        let cp = Checkpoint {
            issue_title: "Add caching".into(),
            issue_body: "Cache the thing.".into(),
            ..Default::default()
        };
        let flavor = WorkerFlavor {
            separate_delivery: false,
        };

        // collab off (default): no consult block.
        let (deps_off, run, _f) = fake_env(&[forge::LABEL_READY]);
        let off = flavor.execute_prompt(&deps_off, &run, &cp, dir.path());
        assert!(!off.contains("agmsg"), "collab off ⇒ no consult block");

        // collab on, but the advisor spawn failed (no live pane): the prompt is
        // byte-for-byte identical to collab-off (criteria 1/4 — never advertise
        // an absent advisor).
        let (mut deps_on, run_on, _f2) = fake_env(&[forge::LABEL_READY]);
        deps_on.config.collab = Some(crate::config::CollabConfig {
            mode: crate::config::CollabMode::Advisor,
            advisor_role: "planner".into(),
        });
        let on_no_pane = flavor.execute_prompt(&deps_on, &run_on, &cp, dir.path());
        assert_eq!(on_no_pane, off, "spawn failure ⇒ prompt unchanged");

        // collab on + a live advisor pane (spawn succeeded): the block appears,
        // carrying the team name and the agmsg scripts.
        deps_on
            .store
            .upsert_pane(
                "proj",
                7,
                crate::store::LANE_ADVISOR,
                "fake",
                "meguri",
                "%adv",
                "/tmp/adv",
            )
            .unwrap();
        let on_live = flavor.execute_prompt(&deps_on, &run_on, &cp, dir.path());
        assert!(on_live.contains("meguri-proj-7"), "team name in block");
        assert!(on_live.contains("advisor"));
        assert!(on_live.contains("agmsg"));
    }

    #[tokio::test]
    async fn needs_plan_hands_the_issue_to_the_planner() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, run, forge) = fake_env(&[forge::LABEL_READY, forge::LABEL_WORKING]);

        let outcome = WorkerFlavor {
            separate_delivery: false,
        }
        .on_needs_plan(&deps, &run, dir.path(), "auth model undecided")
        .await
        .unwrap();
        let WorkerOutcome::NeedsPlan(reason) = outcome else {
            panic!("expected NeedsPlan, got {outcome:?}");
        };
        assert_eq!(reason, "auth model undecided");

        let labels = forge.labels_of(7);
        assert!(
            labels.contains(&forge::LABEL_PLAN.to_string()),
            "{labels:?}"
        );
        assert!(!labels.contains(&forge::LABEL_READY.to_string()));
        assert!(!labels.contains(&forge::LABEL_WORKING.to_string()));

        let comments = forge.comments_of(7);
        assert_eq!(comments.len(), 1);
        assert!(comments[0].contains("auth model undecided"));
        assert!(comments[0].contains(forge::LABEL_PLAN));
    }

    #[tokio::test]
    async fn needs_plan_with_existing_spec_escalates_instead() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
        std::fs::write(dir.path().join("docs/specs/issue-7.md"), "# Spec\n").unwrap();
        let (deps, run, forge) = fake_env(&[forge::LABEL_READY, forge::LABEL_WORKING]);

        let err = WorkerFlavor {
            separate_delivery: false,
        }
        .on_needs_plan(&deps, &run, dir.path(), "still unclear")
        .await
        .unwrap_err();
        assert!(err.to_string().contains("docs/specs/issue-7.md"), "{err}");

        // The hook only reports; run_flow's failure path does the labeling.
        let labels = forge.labels_of(7);
        assert!(
            !labels.contains(&forge::LABEL_PLAN.to_string()),
            "{labels:?}"
        );
        assert!(forge.comments_of(7).is_empty());
    }

    #[test]
    fn separate_delivery_injects_and_prunes_a_landed_spec() {
        // A reviewed spec landed on the branch (from a merged spec PR): the
        // worker injects it and must see it deleted (ADR 0008 finding 1).
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("docs/specs")).unwrap();
        std::fs::write(
            dir.path().join("docs/specs/issue-7.md"),
            "# Spec\n\n- do X\n",
        )
        .unwrap();
        let (deps, run, _forge) = fake_env(&[forge::LABEL_READY]);
        let cp = Checkpoint {
            issue_title: "T".into(),
            issue_body: "B".into(),
            ..Default::default()
        };
        let flavor = WorkerFlavor {
            separate_delivery: true,
        };
        let prompt = flavor.execute_prompt(&deps, &run, &cp, dir.path());
        assert!(prompt.contains("# Reviewed spec (`docs/specs/issue-7.md`)"));
        assert!(prompt.contains("- do X"));
        assert!(prompt.contains("delete `docs/specs/issue-7.md`"));

        // verify_work rejects a surviving spec, accepts its absence.
        assert!(flavor.verify_work(&run, &cp, dir.path()).is_err());
        std::fs::remove_file(dir.path().join("docs/specs/issue-7.md")).unwrap();
        assert!(flavor.verify_work(&run, &cp, dir.path()).is_ok());
    }

    #[test]
    fn a_normal_issue_without_a_spec_degrades_to_the_ordinary_flow() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, run, _forge) = fake_env(&[forge::LABEL_READY]);
        let cp = Checkpoint::default();
        let flavor = WorkerFlavor {
            separate_delivery: true,
        };
        let prompt = flavor.execute_prompt(&deps, &run, &cp, dir.path());
        assert!(!prompt.contains("# Reviewed spec"));
        assert!(flavor.verify_work(&run, &cp, dir.path()).is_ok());
    }

    /// The vibration guard's other leg (issue #135): even when no spec ever
    /// landed on disk, an issue that already retreated to planning once
    /// before must not bounce `ready` ⇄ `plan` a second time.
    #[tokio::test]
    async fn needs_plan_a_second_time_escalates_instead() {
        let dir = tempfile::tempdir().unwrap();
        let (deps, first_run, forge) = fake_env(&[forge::LABEL_READY, forge::LABEL_WORKING]);
        deps.store
            .update_run_status(&first_run.id, crate::store::RunStatus::NeedsPlan, None)
            .unwrap();

        // A later worker run reclaims the same issue after planning sent it
        // back to `ready` without ever writing a spec file.
        let second_run = deps
            .store
            .create_run_for_loop("proj", KIND, 7, "t")
            .unwrap();

        let err = WorkerFlavor {
            separate_delivery: false,
        }
        .on_needs_plan(&deps, &second_run, dir.path(), "still unclear")
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already retreated"), "{err}");

        // The hook only reports; run_flow's failure path does the labeling.
        let labels = forge.labels_of(7);
        assert!(
            !labels.contains(&forge::LABEL_PLAN.to_string()),
            "{labels:?}"
        );
        assert!(forge.comments_of(7).is_empty());
    }

    fn fake_env(labels: &[&str]) -> (Deps, RunRecord, Arc<FakeForge>) {
        let forge = Arc::new(FakeForge::with_issue(
            7,
            "Add caching",
            "Cache the thing.",
            labels,
        ));
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run_for_loop("proj", KIND, 7, "t").unwrap();
        let mut run = store.get_run(&run.id).unwrap().unwrap();
        run.branch = Some("meguri/test".into());
        let project = ProjectConfig {
            id: "proj".into(),
            repo_path: "/tmp/unused".into(),
            repo_slug: Some("me/proj".into()),
            mode: Default::default(),
            deliver: None,
            default_branch: "main".into(),
            language: None,
            check_command: None,
            worktree_root: None,
            pr: None,
            clean: None,
            plan_delivery: Default::default(),
            review: None,
            worktree_setup: Default::default(),
            schedules: Vec::new(),
            cadence: Vec::new(),
            prompts: Default::default(),
        };
        let deps = Deps::with_label_source(
            store,
            Arc::new(crate::mux::fake::FakeMux::new(false)),
            forge.clone(),
            Config::default(),
            project,
        );
        (deps, run, forge)
    }
}
