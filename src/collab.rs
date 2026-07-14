//! The collab advisor layer (issue #111, ADR 0006 collab-advisor).
//!
//! During a worker run, the plan author's role (planner) is re-embodied as an
//! *advisor* pane on the same issue, and the worker may consult it over
//! [agmsg](https://github.com/fujibee/agmsg) — "am I drifting / does this meet
//! the spec?". Three invariants (ADR 0006) shape this module:
//!
//! - **Opt-in switch, loud startup check.** Absent `[collab]` (or `mode =
//!   "off"`) is byte-for-byte the historical behavior. `mode = "advisor"` pairs
//!   with a startup agmsg detection that `bail!`s if the skill is missing —
//!   never a silent fallback (mirrors `routing::validate`, ADR 0003).
//! - **Consultation is advice, not a completion condition.** meguri never reads
//!   / waits on / validates agmsg. It only *detects* the CLI once, at startup;
//!   the agents themselves invoke agmsg. Run success stays governed by
//!   `result.json` + git verification alone.
//! - **The protocol lives in the prompts.** meguri seeds both peers with the
//!   team name, the counterpart's agmsg id, and the exact scripts to call; it
//!   does not exec agmsg beyond the startup `--version` probe.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::{CollabMode, Config};
use crate::store::RunRecord;
use crate::tasks::TaskKey;

/// Home-relative path to the agmsg skill's runtime scripts. The agmsg runtime
/// is this bundle of bash scripts (the `agmsg` npm binary on PATH is only a
/// bootstrapper); the prompts drive these same scripts, so detection looks at
/// what is actually used.
pub const AGMSG_SCRIPTS_SUBPATH: &str = ".agents/skills/agmsg/scripts";
/// The two scripts the peers call. Left as `~`-relative because they are
/// embedded in agent-facing prompts, which run through a shell that expands
/// `~`. meguri's own detection uses the absolute path below instead.
pub const AGMSG_SEND: &str = "~/.agents/skills/agmsg/scripts/send.sh";
pub const AGMSG_INBOX: &str = "~/.agents/skills/agmsg/scripts/inbox.sh";

/// The default advisor role — the model that wrote the spec.
pub const DEFAULT_ADVISOR_ROLE: &str = "planner";

/// Whether the advisor layer is active (`[collab] mode = "advisor"`). Absent
/// section or `mode = "off"` → false (feature off).
pub fn advisor_active(cfg: &Config) -> bool {
    matches!(
        cfg.collab.as_ref().map(|c| c.mode),
        Some(CollabMode::Advisor)
    )
}

/// The routing role the advisor borrows its profile from (default `planner`).
pub fn advisor_role(cfg: &Config) -> &str {
    cfg.collab
        .as_ref()
        .map(|c| c.advisor_role.as_str())
        .unwrap_or(DEFAULT_ADVISOR_ROLE)
}

/// Which loop kinds get an advisor (issue #111 v1: worker and spec-worker
/// only). A pure function of `loop_kind` so both flow (spawn/reap gating) and
/// the scheduler (slot weighting) share one source of truth — the scheduler
/// only holds `RunRecord.loop_kind`, never the flow's `Flavor`.
pub fn supports_advisor_loop_kind(loop_kind: &str) -> bool {
    loop_kind == crate::engine::worker::KIND || loop_kind == crate::engine::spec_worker::KIND
}

/// Whether this run will actually get an advisor spawned: collab active, an
/// advisor-eligible loop kind, AND a github issue. The issue check is
/// load-bearing — advisor addressing is issue-scoped (`team_name`), so a local
/// task gets no advisor. flow's `ensure_advisor` and the scheduler's slot
/// weighting must agree on exactly this predicate, or a run books a slot for an
/// advisor it never receives.
pub fn run_gets_advisor(cfg: &Config, run: &RunRecord) -> bool {
    advisor_active(cfg)
        && supports_advisor_loop_kind(&run.loop_kind)
        && matches!(run.task_key(), TaskKey::Issue(_))
}

/// The absolute path to the agmsg version script, built from the home
/// directory. `detect_command` runs `Command::new(path).arg("--version")`
/// without a shell, so `~` would not expand — this must be absolute. `None`
/// when the home directory cannot be resolved.
pub fn agmsg_version_script() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(AGMSG_SCRIPTS_SUBPATH).join("version.sh"))
}

/// The agmsg team for an issue. Scoped by project because the agmsg SQLite
/// floor is shared across projects on one host, so a bare issue number could
/// collide.
pub fn team_name(project_id: &str, issue: i64) -> String {
    format!("meguri-{project_id}-{issue}")
}

/// Startup validation (loud, early — the `meguri watch` / `meguri run` entry).
/// A no-op unless `mode = "advisor"`; then the `advisor_role` must be a known
/// routing role and the agmsg skill must be detectable. `detect` is injected
/// (production `routing::detect_command`, tests a fake) exactly like
/// `routing::validate`.
pub fn validate(cfg: &Config, detect: &dyn Fn(&str) -> bool) -> Result<()> {
    if !advisor_active(cfg) {
        return Ok(());
    }
    let role = advisor_role(cfg);
    if !crate::routing::KNOWN_ROLES.contains(&crate::routing::canonical_role(role)) {
        bail!(
            "[collab] advisor_role = {role:?} is not a known routing role — valid roles: {}",
            crate::routing::KNOWN_ROLES.join(", "),
        );
    }
    let script = agmsg_version_script()
        .context("[collab] mode = \"advisor\" but the home directory could not be resolved")?;
    let script = script.to_string_lossy();
    if !detect(&script) {
        bail!(
            "[collab] mode = \"advisor\" but the agmsg skill was not found \
             (detection `{script} --version` failed) — install agmsg to \
             ~/{AGMSG_SCRIPTS_SUBPATH} or remove [collab]"
        );
    }
    Ok(())
}

/// The block appended to the worker's execute prompt when an advisor is live
/// (issue #111 §4). English to match the surrounding worker prompt; the
/// polling cadence is prose, not a protocol constant.
pub fn worker_consult_block(team: &str) -> String {
    format!(
        "# A plan-author advisor is available\n\
         The advisor who wrote this issue's spec is reachable over agmsg on team `{team}` \
         as `advisor` (they can see the full spec). If you are unsure whether your \
         implementation approach satisfies the spec's requirements — or is drifting from \
         them — consult them:\n\
         - send a question: `{AGMSG_SEND} {team} worker advisor \"<question>\"`\n\
         - poll for replies: `{AGMSG_INBOX} {team} worker` (about every 30s)\n\
         If no reply comes after a few minutes, proceed without it. Consulting is optional \
         and never a completion condition. Use it for requirement/drift questions, not code \
         review.\n\n"
    )
}

/// The seed prompt for the advisor pane (issue #111 §4): the planner reborn,
/// grounded in the spec, told to answer only on requirement/drift and to write
/// nothing.
pub fn advisor_seed_prompt(issue: i64, team: &str, spec_material: &str) -> String {
    format!(
        "You are the planner who wrote the spec for GitHub issue #{issue}. Below is that \
         spec in full.\n\n{spec_material}\n\n\
         A worker is now implementing it and will consult you over agmsg on team `{team}`, \
         addressed to `advisor`. Keep watch by running `{AGMSG_INBOX} {team} advisor` every \
         30-60 seconds — this is your only job; do not do any other work. When a question \
         arrives, answer concisely and ONLY from the standpoint of requirement-fulfilment \
         and drift, then reply with `{AGMSG_SEND} {team} advisor worker \"<answer>\"`. If the \
         worker is drifting, point to the exact part of the spec it conflicts with. Do NOT \
         write code, do NOT commit, do NOT create files — your working directory is empty and \
         there is nowhere to write."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(toml_str: &str) -> Config {
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn absent_section_is_off() {
        let cfg = cfg_from("");
        assert!(cfg.collab.is_none());
        assert!(!advisor_active(&cfg));
        // A detector that would panic if consulted — off must not detect.
        let never = |_: &str| panic!("off must not detect");
        validate(&cfg, &never).unwrap();
    }

    #[test]
    fn explicit_off_is_inert() {
        let cfg = cfg_from("[collab]\nmode = \"off\"\n");
        assert!(cfg.collab.is_some());
        assert!(!advisor_active(&cfg));
        assert_eq!(advisor_role(&cfg), "planner");
        let never = |_: &str| panic!("off must not detect");
        validate(&cfg, &never).unwrap();
    }

    #[test]
    fn advisor_defaults_role_to_planner() {
        let cfg = cfg_from("[collab]\nmode = \"advisor\"\n");
        assert!(advisor_active(&cfg));
        assert_eq!(advisor_role(&cfg), "planner");
    }

    #[test]
    fn advisor_with_agmsg_detected_passes_and_gets_the_absolute_path() {
        let cfg = cfg_from("[collab]\nmode = \"advisor\"\n");
        let want = agmsg_version_script()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        // The detector must receive an absolute path (no unexpanded `~`).
        let detect = |cmd: &str| {
            assert!(!cmd.contains('~'), "detection path must be absolute: {cmd}");
            assert_eq!(cmd, want);
            true
        };
        validate(&cfg, &detect).unwrap();
    }

    #[test]
    fn advisor_without_agmsg_bails() {
        let cfg = cfg_from("[collab]\nmode = \"advisor\"\n");
        let err = validate(&cfg, &|_| false).unwrap_err().to_string();
        assert!(err.contains("agmsg"), "{err}");
    }

    #[test]
    fn advisor_with_unknown_role_bails() {
        let cfg = cfg_from("[collab]\nmode = \"advisor\"\nadvisor_role = \"nonsense\"\n");
        // Detection would pass, but the role check fails first.
        let err = validate(&cfg, &|_| true).unwrap_err().to_string();
        assert!(err.contains("advisor_role"), "{err}");
    }

    #[test]
    fn supports_only_worker_and_spec_worker() {
        assert!(supports_advisor_loop_kind(crate::engine::worker::KIND));
        assert!(supports_advisor_loop_kind(crate::engine::spec_worker::KIND));
        assert!(!supports_advisor_loop_kind("planner"));
        assert!(!supports_advisor_loop_kind("fixer"));
        assert!(!supports_advisor_loop_kind("pr-reviewer"));
    }

    #[test]
    fn run_gets_advisor_requires_issue_eligible_loop_and_collab_on() {
        use crate::store::Store;
        let store = Store::open_in_memory().unwrap();
        // Distinct issue numbers: only one active run is allowed per
        // (project, loop, issue).
        let issue_run = |kind: &str, issue: i64| {
            let r = store.create_run_for_loop("proj", kind, issue, "t").unwrap();
            store.get_run(&r.id).unwrap().unwrap()
        };
        let worker_run = issue_run(crate::engine::worker::KIND, 7);
        let spec_worker_run = issue_run(crate::engine::spec_worker::KIND, 8);
        let planner_run = issue_run("planner", 9);
        // A *local* task worker run — no issue lane, so no advisor.
        let local_run = {
            let r = store
                .create_run_for_task("proj", crate::engine::worker::KIND, 42, "t")
                .unwrap();
            store.get_run(&r.id).unwrap().unwrap()
        };

        let on = cfg_from("[collab]\nmode = \"advisor\"\n");
        let off = cfg_from("");

        // On + issue + eligible loop → yes.
        assert!(run_gets_advisor(&on, &worker_run));
        assert!(run_gets_advisor(&on, &spec_worker_run));
        // On but a local task → no (the bug this predicate fixes: no advisor is
        // ever spawned for a local run, so it must not book the extra slot).
        assert!(!run_gets_advisor(&on, &local_run));
        // On but an ineligible loop → no.
        assert!(!run_gets_advisor(&on, &planner_run));
        // Off → no.
        assert!(!run_gets_advisor(&off, &worker_run));
    }

    #[test]
    fn team_name_is_project_scoped() {
        assert_eq!(team_name("proj", 111), "meguri-proj-111");
    }

    #[test]
    fn prompts_carry_team_id_and_scripts() {
        let team = team_name("proj", 7);
        let worker = worker_consult_block(&team);
        assert!(worker.contains(&team));
        assert!(worker.contains("advisor"));
        assert!(worker.contains(AGMSG_SEND));
        assert!(worker.contains(AGMSG_INBOX));

        let seed = advisor_seed_prompt(7, &team, "SPEC BODY");
        assert!(seed.contains("SPEC BODY"));
        assert!(seed.contains(&team));
        assert!(seed.contains("#7"));
        assert!(seed.contains("Do NOT write code"));
    }
}
