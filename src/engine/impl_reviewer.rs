//! The impl reviewer: no longer a schedulable loop, but the worker's
//! **internal** self-review phase (ADR 0006). It runs inside the run's own
//! worktree, between `validate` and `open-pr`, and **never touches the
//! forge** — the review→fix ping-pong that used to travel as PR threads now
//! stays entirely local:
//!
//! 1. **review turn** — reads `git diff <base>...HEAD` locally (dropped at
//!    [`DIFF_FILE`]) in a separate `impl-review` lane under the
//!    `impl-reviewer` routing profile (model separation survives), and writes
//!    `{verdict, findings[]}` to [`REVIEW_FILE`]. `clean` ends the phase.
//! 2. **fix turn** — the author lane addresses the findings and commits;
//!    the project check is re-run; then back to a review turn.
//! 3. **rounds cap** — `review.max_rounds` bounds the loop with a *local*
//!    counter (no forge marker). If the cap is hit without a clean verdict
//!    the PR is published anyway (the human merge gate is the backstop), and
//!    a single footer line records the non-convergence.
//!
//! Findings ride the run's checkpoint in-memory; nothing is posted, so the
//! human's PR conversation stays a clean, human/external-review-only space,
//! and the fixer's discovery naturally narrows to human/external threads.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::Deps;
use super::flow::{self, Checkpoint, Flavor, Kind, NeedsHuman};
use crate::gitops;
use crate::store::RunRecord;
use crate::turn::prompts::MEGURI_DIR;
use crate::turn::{TurnOutcome, TurnStatus};

/// Where the orchestrator drops the local diff for the review turn to read
/// (worktree-relative; `.meguri/` is git-excluded, so it never dirties the
/// tree).
pub const DIFF_FILE: &str = ".meguri/self-review-diff.patch";
/// Where the review turn writes its verdict + findings (worktree-relative).
pub const REVIEW_FILE: &str = ".meguri/self-review.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewVerdict {
    Clean,
    Findings,
}

/// One finding from a review turn, anchored to a line on the NEW side of the
/// diff so the fix turn can locate it. Carried in the run's checkpoint
/// (`self_review_pending`) rather than posted as a forge thread.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub path: String,
    pub line: u64,
    pub body: String,
    /// Which review lens surfaced it (ADR 0008), if the reviewer tagged one.
    #[serde(default)]
    pub lens: Option<String>,
}

/// One self-review round's outcome, for the PR-body `<details>` (ADR 0008):
/// the round number and how many findings it raised. Verdict is implicit —
/// zero findings means clean.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoundRecord {
    pub round: u32,
    pub findings: usize,
}

/// What the review turn writes to [`REVIEW_FILE`].
#[derive(Debug, Deserialize)]
pub struct ImplReviewFile {
    pub verdict: ReviewVerdict,
    #[serde(default)]
    pub review: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
}

/// The worker's self-review phase: review→fix until clean or the rounds cap,
/// then hand back to the flow to open the PR. Forge calls: zero. Interruption
/// resumes from the checkpoint (rounds + pending findings persist).
pub(crate) async fn self_review(
    deps: &Deps,
    run: &RunRecord,
    cp: &mut Checkpoint,
    worktree: &Path,
    flavor: &dyn Flavor,
) -> Result<flow::StepFlow> {
    let review_cfg = deps.config.review_for(&deps.project);
    let max_rounds = review_cfg.max_rounds;
    let lenses = review_cfg.lenses.clone();
    let kind = flavor.kind();
    let base = deps.project.default_branch.clone();
    let language = deps.config.language_for(&deps.project);

    loop {
        // Backstop / resume guard: the cap is spent and the last verdict was
        // not clean — publish as-is (ADR 0006).
        if cp.self_review_rounds >= max_rounds {
            return mark_unconverged(deps, run, cp);
        }

        // ---- review turn (in the self-review lane) ----
        let review = match review_turn(deps, run, cp, worktree, &base, kind, &lenses, language)
            .await?
        {
            ReviewTurn::Reviewed(review) => review,
            ReviewTurn::Stopped => return Ok(flow::StepFlow::Stopped),
            ReviewTurn::Interrupted(r) => return Ok(flow::StepFlow::Interrupted(r)),
        };
        cp.self_review_rounds += 1;
        cp.self_review_pending = review.findings.clone();
        cp.self_review_log.push(RoundRecord {
            round: cp.self_review_rounds,
            findings: review.findings.len(),
        });
        persist(deps, run, cp)?;
        deps.store.emit(
            Some(&run.id),
            "self_review.reviewed",
            json!({ "round": cp.self_review_rounds, "verdict": review.verdict,
                    "findings": review.findings.len() }),
        )?;

        if review.verdict == ReviewVerdict::Clean {
            cp.self_review_unconverged = false;
            cp.self_review_pending.clear();
            persist(deps, run, cp)?;
            deps.store.emit(
                Some(&run.id),
                "self_review.clean",
                json!({ "rounds": cp.self_review_rounds }),
            )?;
            return Ok(flow::StepFlow::Continue);
        }

        // Findings remain but no rounds left to re-review a fix — publish.
        if cp.self_review_rounds >= max_rounds {
            return mark_unconverged(deps, run, cp);
        }

        // ---- fix turn (in the author lane) ----
        match fix_turn(deps, run, cp, worktree, language).await? {
            flow::StepFlow::Continue => {}
            other => return Ok(other),
        }

        // Re-validate the fixed tree before the next review; a failing check
        // is fixed here (its own bounded corrective turns) so the review
        // always reads a green tree.
        match flow::validate(deps, run, cp, worktree, flow::STEP_SELF_REVIEW).await? {
            flow::StepFlow::Continue => {}
            other => return Ok(other),
        }
    }
}

/// Persist the checkpoint under the self-review step so a crash resumes here.
fn persist(deps: &Deps, run: &RunRecord, cp: &Checkpoint) -> Result<()> {
    deps.store
        .update_run_step(&run.id, flow::STEP_SELF_REVIEW, &serde_json::to_string(cp)?)?;
    Ok(())
}

/// The rounds cap was hit without a clean verdict: flag the non-convergence
/// (footer + event) and let the PR open.
fn mark_unconverged(deps: &Deps, run: &RunRecord, cp: &mut Checkpoint) -> Result<flow::StepFlow> {
    cp.self_review_unconverged = true;
    persist(deps, run, cp)?;
    deps.store.emit(
        Some(&run.id),
        "self_review.unconverged",
        json!({ "rounds": cp.self_review_rounds,
                "pending": cp.self_review_pending.len() }),
    )?;
    Ok(flow::StepFlow::Continue)
}

enum ReviewTurn {
    Reviewed(ImplReviewFile),
    Stopped,
    Interrupted(String),
}

/// One review turn (plus at most one corrective turn). The review runs in the
/// `impl-review` lane; verification is the orchestrator's: the checkout must
/// stay pristine and at the same HEAD, and the review file must parse.
async fn review_turn(
    deps: &Deps,
    run: &RunRecord,
    cp: &Checkpoint,
    worktree: &Path,
    base: &str,
    kind: Kind,
    lenses: &[String],
    language: Option<&str>,
) -> Result<ReviewTurn> {
    // Drop the local diff where the prompt says it is, and clear any stale
    // review file so we read *this* turn's verdict.
    let diff = gitops::diff_against_base(worktree, base).await?;
    std::fs::create_dir_all(worktree.join(MEGURI_DIR))?;
    std::fs::write(worktree.join(DIFF_FILE), &diff)?;
    let _ = std::fs::remove_file(worktree.join(REVIEW_FILE));

    let head_before = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
    let mut prompt = review_prompt(run, cp, kind, lenses, language);
    let mut corrective_turns = 0u32;

    loop {
        let (outcome, _) =
            flow::run_review_turn(deps, run, worktree, "self-review", &prompt).await?;
        let result = match outcome {
            TurnOutcome::Completed(r) => r,
            TurnOutcome::Stopped => return Ok(ReviewTurn::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(ReviewTurn::Interrupted(
                    "pane died during self-review".into(),
                ));
            }
        };

        match result.status {
            TurnStatus::Success => {}
            TurnStatus::Failure => {
                return Err(NeedsHuman(format!(
                    "agent reported failure self-reviewing issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
            // needs_plan/decompose make no sense on a review turn once work is
            // committed — a human looks.
            TurnStatus::NeedsHuman | TurnStatus::NeedsPlan | TurnStatus::Decompose => {
                return Err(NeedsHuman(format!(
                    "agent needs a human self-reviewing issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
        }

        // Trust but verify: the review turn must not touch the tree.
        let clean = gitops::status_clean(worktree).await?;
        let head_now = gitops::run_git(worktree, &["rev-parse", "HEAD"]).await?;
        let problem = if !clean || head_now != head_before {
            Some(format!(
                "- the review must not modify the tree: working tree clean: \
                 {clean} (must be true), HEAD: {head_now} (must be {head_before}) — \
                 discard any changes; write only your review to `{REVIEW_FILE}`"
            ))
        } else {
            read_review(worktree).err()
        };
        let Some(problem) = problem else {
            return Ok(ReviewTurn::Reviewed(
                read_review(worktree).expect("verified above"),
            ));
        };

        corrective_turns += 1;
        if corrective_turns > 1 {
            return Err(NeedsHuman(format!(
                "agent claimed a self-review but it doesn't verify after a \
                 corrective turn:\n{problem}"
            ))
            .into());
        }
        deps.store.emit(
            Some(&run.id),
            "self_review.correction",
            json!({ "problem": problem }),
        )?;
        prompt = format!(
            "Your previous result claimed a completed review, but verification failed:\n{problem}\n\n\
             Fix this. Do not modify the checkout; write your review to `{REVIEW_FILE}` as instructed.",
        );
    }
}

/// One fix turn (plus at most one corrective turn) in the author lane: the
/// author addresses the pending findings and commits, leaving a clean tree.
async fn fix_turn(
    deps: &Deps,
    run: &RunRecord,
    cp: &Checkpoint,
    worktree: &Path,
    language: Option<&str>,
) -> Result<flow::StepFlow> {
    let mut prompt = fix_prompt(&cp.self_review_pending, language);
    let mut corrective_turns = 0u32;

    loop {
        let (outcome, _) = flow::run_turn(deps, run, worktree, "self-review-fix", &prompt).await?;
        let result = match outcome {
            TurnOutcome::Completed(r) => r,
            TurnOutcome::Stopped => return Ok(flow::StepFlow::Stopped),
            TurnOutcome::PaneDied => {
                return Ok(flow::StepFlow::Interrupted(
                    "pane died during self-review fix".into(),
                ));
            }
        };

        match result.status {
            TurnStatus::Success => {}
            TurnStatus::Failure | TurnStatus::NeedsHuman => {
                return Err(NeedsHuman(format!(
                    "agent could not fix its self-review findings on issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
            // needs_plan/decompose are meaningless once the work is committed
            // and merely being polished — a human looks.
            TurnStatus::NeedsPlan | TurnStatus::Decompose => {
                return Err(NeedsHuman(format!(
                    "agent asked to re-plan while fixing its self-review on issue #{}: {}",
                    run.issue_number, result.summary
                ))
                .into());
            }
        }

        // The only hard invariant: the tree is clean (nothing uncommitted to
        // push). Whether the findings were real is the next review's call.
        if gitops::status_clean(worktree).await? {
            deps.store.emit(
                Some(&run.id),
                "self_review.fixed",
                json!({ "round": cp.self_review_rounds }),
            )?;
            return Ok(flow::StepFlow::Continue);
        }

        corrective_turns += 1;
        if corrective_turns > 1 {
            return Err(NeedsHuman(format!(
                "agent left an uncommitted tree after fixing its self-review on \
                 issue #{}",
                run.issue_number
            ))
            .into());
        }
        deps.store.emit(
            Some(&run.id),
            "self_review.fix_correction",
            json!({ "round": cp.self_review_rounds }),
        )?;
        prompt = "Your working tree is not clean. Commit (or discard) every change so nothing \
             dangles, then report success. Do not create a pull request; meguri handles that."
            .to_string();
    }
}

/// The multi-lens review instruction (ADR 0008): one review turn considers
/// every configured perspective. For a spec/ADR (Plan) the code lenses are
/// re-read as document lenses; for code (Impl) they are taken literally.
fn lens_instruction(kind: Kind, lenses: &[String]) -> String {
    if lenses.is_empty() {
        return String::new();
    }
    let list = lenses
        .iter()
        .map(|l| format!("`{l}`"))
        .collect::<Vec<_>>()
        .join(", ");
    match kind {
        Kind::Plan => format!(
            "- Review through each of these lenses, adapted to a design document: {list} \
             (e.g. `correctness` = are the decisions sound and internally consistent; \
             `tests` = is the plan verifiable / are acceptance criteria present; \
             `simplicity` = is the scope minimal; `security` = are risks acknowledged).\n"
        ),
        Kind::Impl => format!(
            "- Review through each of these lenses: {list}.\n"
        ),
    }
}

fn review_prompt(
    run: &RunRecord,
    cp: &Checkpoint,
    kind: Kind,
    lenses: &[String],
    language: Option<&str>,
) -> String {
    let round = cp.self_review_rounds + 1;
    let subject = match kind {
        Kind::Plan => "spec/ADR",
        Kind::Impl => "implementation",
    };
    format!(
        "You are self-reviewing your own {subject} of issue #{number} before it is \
         published as a pull request (self-review round {round}). The worktree holds the \
         committed work; `{diff}` is its full diff against the base branch.\n\n\
         # Issue: {title}\n\n\
         # Instructions\n\
         - Read the diff at `{diff}`; browse the checked-out files for context as needed.\n\
         {lens_section}\
         - Review the {subject} for correctness, completeness (tests included where the \
           change is code), and fit with the repository's conventions.\n\
         - Do NOT modify, commit, or push anything; the review file below is your only \
           deliverable.\n\
         - Write your review to `{review}` as JSON:\n\
           `{{\"verdict\": \"clean\" | \"findings\", \"review\": \"<Markdown summary>\", \
           \"findings\": [{{\"path\": \"src/x.rs\", \"line\": 42, \"lens\": \"correctness\", \
           \"body\": \"<what must change>\"}}]}}`\n\
           - \"clean\": nothing must change before this can be published (pure nitpicks do not \
             block; mention them in `review` and leave `findings` empty).\n\
           - \"findings\": something must change. Each entry must anchor to a line that appears \
             on the NEW side of the diff and may name the `lens` it came from; put cross-cutting \
             remarks that fit no single line in `review` only.\n\
         - A completed review is a success regardless of verdict; report \"failure\"/\"needs_human\" \
           only when you cannot review at all.\
         {lang_section}",
        number = run.issue_number,
        round = round,
        title = cp.issue_title,
        diff = DIFF_FILE,
        review = REVIEW_FILE,
        lens_section = lens_instruction(kind, lenses),
        lang_section = flow::language_instruction(language),
    )
    // The completion contract is appended by prepare_turn.
}

fn fix_prompt(findings: &[Finding], language: Option<&str>) -> String {
    let list = if findings.is_empty() {
        "(no line-anchored findings — see the review summary from your last turn)".to_string()
    } else {
        findings
            .iter()
            .map(|f| format!("- `{}:{}` — {}", f.path, f.line, f.body))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "Your self-review found issues in your own diff. Address each finding, then commit \
         your fixes.\n\n\
         # Findings\n{list}\n\n\
         # Instructions\n\
         - Fix each finding you agree with; if a finding is wrong, leave the code and move on \
           (the next review round will re-check).\n\
         - Run the relevant tests/checks yourself.\n\
         - COMMIT all your work to the current branch with clear messages. Leave the working \
           tree clean.\n\
         - Do NOT push and do NOT create a pull request; meguri handles both.\
         {lang_section}",
        list = list,
        lang_section = flow::language_instruction(language),
    )
    // The completion contract is appended by prepare_turn.
}

/// Parse and validate the review file. The Err text feeds a corrective prompt.
fn read_review(worktree: &Path) -> std::result::Result<ImplReviewFile, String> {
    let raw = std::fs::read_to_string(worktree.join(REVIEW_FILE)).map_err(|_| {
        format!("- review file `{REVIEW_FILE}` does not exist (write it as instructed)")
    })?;
    let review: ImplReviewFile = serde_json::from_str(raw.trim()).map_err(|e| {
        format!(
            "- review file `{REVIEW_FILE}` is not valid JSON ({e}); expected \
             {{\"verdict\": \"clean\" | \"findings\", \"review\": \"<Markdown>\", \
             \"findings\": [{{\"path\": ..., \"line\": ..., \"body\": ...}}]}}"
        )
    })?;
    if review.verdict == ReviewVerdict::Findings && review.review.trim().is_empty() {
        return Err(format!(
            "- verdict is \"findings\" but `review` in `{REVIEW_FILE}` is empty; \
             summarize every finding"
        ));
    }
    if review.verdict == ReviewVerdict::Clean && !review.findings.is_empty() {
        return Err(format!(
            "- verdict is \"clean\" but `findings` in `{REVIEW_FILE}` is not empty; \
             a clean review carries no findings — move the remarks into `review` \
             or change the verdict"
        ));
    }
    for f in &review.findings {
        if f.path.trim().is_empty() || f.line == 0 || f.body.trim().is_empty() {
            return Err(format!(
                "- every `findings` entry in `{REVIEW_FILE}` needs a non-empty \
                 `path`, a `line` >= 1 on the NEW side of the diff, and a \
                 non-empty `body`"
            ));
        }
    }
    Ok(review)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_run() -> RunRecord {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        let run = store
            .create_run_for_loop("proj", "worker", 7, "Add caching")
            .unwrap();
        let mut run = store.get_run(&run.id).unwrap().unwrap();
        run.issue_title = Some("Add caching".into());
        run
    }

    fn cp_with_title() -> Checkpoint {
        Checkpoint {
            issue_title: "Add caching".into(),
            ..Default::default()
        }
    }

    #[test]
    fn review_file_parses_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".meguri")).unwrap();
        let path = dir.path().join(REVIEW_FILE);

        let err = read_review(dir.path()).unwrap_err();
        assert!(err.contains("does not exist"), "{err}");

        std::fs::write(&path, "not json").unwrap();
        assert!(
            read_review(dir.path())
                .unwrap_err()
                .contains("not valid JSON")
        );

        std::fs::write(&path, r#"{"verdict":"findings","review":"  "}"#).unwrap();
        assert!(read_review(dir.path()).unwrap_err().contains("empty"));

        // Clean must carry no findings.
        std::fs::write(
            &path,
            r#"{"verdict":"clean","review":"ok","findings":[{"path":"a.rs","line":1,"body":"x"}]}"#,
        )
        .unwrap();
        assert!(read_review(dir.path()).unwrap_err().contains("clean"));

        // Findings must be fully anchored.
        std::fs::write(
            &path,
            r#"{"verdict":"findings","review":"r","findings":[{"path":"","line":1,"body":"x"}]}"#,
        )
        .unwrap();
        assert!(read_review(dir.path()).unwrap_err().contains("non-empty"));
        std::fs::write(
            &path,
            r#"{"verdict":"findings","review":"r","findings":[{"path":"a.rs","line":0,"body":"x"}]}"#,
        )
        .unwrap();
        assert!(read_review(dir.path()).is_err());

        std::fs::write(
            &path,
            r#"{"verdict":"findings","review":"- bug","findings":[{"path":"src/a.rs","line":42,"body":"off by one"}]}"#,
        )
        .unwrap();
        let review = read_review(dir.path()).unwrap();
        assert_eq!(review.verdict, ReviewVerdict::Findings);
        assert_eq!(review.findings.len(), 1);
        assert_eq!(review.findings[0].line, 42);

        std::fs::write(&path, r#"{"verdict":"clean"}"#).unwrap();
        assert_eq!(
            read_review(dir.path()).unwrap().verdict,
            ReviewVerdict::Clean
        );
    }

    #[test]
    fn review_prompt_demands_anchored_findings_not_changes() {
        let run = fake_run();
        let lenses = super::super::flow::Kind::Impl;
        let prompt = review_prompt(
            &run,
            &cp_with_title(),
            lenses,
            &["correctness".to_string(), "security".to_string()],
            None,
        );
        assert!(prompt.contains("# Issue: Add caching"));
        assert!(prompt.contains(DIFF_FILE));
        assert!(prompt.contains(REVIEW_FILE));
        assert!(prompt.contains("Do NOT modify"));
        assert!(prompt.contains("NEW side of the diff"));
        assert!(prompt.contains("self-review round 1"));
        // The configured lenses are named in the review instruction (ADR 0008).
        assert!(prompt.contains("`correctness`"));
        assert!(prompt.contains("`security`"));
        assert!(!prompt.contains("# Output language"));
    }

    #[test]
    fn plan_review_prompt_reframes_lenses_for_a_document() {
        let run = fake_run();
        let prompt = review_prompt(
            &run,
            &cp_with_title(),
            super::super::flow::Kind::Plan,
            &["tests".to_string()],
            None,
        );
        assert!(prompt.contains("spec/ADR"));
        assert!(prompt.contains("design document"));
    }

    #[test]
    fn fix_prompt_lists_findings() {
        let findings = vec![Finding {
            path: "src/a.rs".into(),
            line: 7,
            body: "handle the None case".into(),
            lens: Some("correctness".into()),
        }];
        let prompt = fix_prompt(&findings, Some("日本語"));
        assert!(prompt.contains("`src/a.rs:7`"));
        assert!(prompt.contains("handle the None case"));
        assert!(prompt.contains("Do NOT push"));
        assert!(prompt.contains("# Output language"));

        // No anchored findings still yields a usable prompt.
        let prompt = fix_prompt(&[], None);
        assert!(prompt.contains("no line-anchored findings"));
    }
}
