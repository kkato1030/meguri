//! Role-based agent routing (issue #64, routing 1/3; roles re-scoped to a
//! 6-way "kind of work" grouping in issue #167 — ADR 0003 revision).
//!
//! `[routing.roles]` steers 6 coarse roles ([`KNOWN_ROLES`]), not the
//! finer-grained internal loop kinds (`runs.loop_kind`): several loop kinds
//! share a role's cost/quality profile ([`routing_role_for_loop`] is the
//! mapping), so we route the role — not an estimated issue difficulty, and
//! not the loop kind directly — to a launch profile. Two rules are
//! load-bearing (ADR 0003):
//!
//! - **Explicit always beats auto.** A role listed in `[routing.roles]` uses
//!   exactly that profile; a missing profile, a failed CLI detection, or an
//!   unknown role name is a *startup* error (`bail!`), never a silent
//!   fallback.
//! - **`[routing]` is the switch.** Without a `[routing]` section every role
//!   runs the `default` profile (`[agent]`) with no detection — byte-for-byte
//!   the historical behavior. Defining `[agents.profiles.*]` alone changes
//!   nothing; profiles are inert until `[routing]` references them.
//!
//! The recommendation table below is a 2026-07 snapshot baked into the
//! binary; `GENERATED_AT` dates it so a freshness check (routing 2/3) can
//! flag it when models turn over.

use std::collections::HashMap;

use anyhow::{Result, bail};

use crate::config::{AgentProfile, Config, RoutingMode};

/// When the recommendation table below was authored. Routing 2/3 turns this
/// into a machine freshness check; for now it is documentation with teeth.
pub const GENERATED_AT: &str = "2026-07-12";

/// Age past which doctor warns the recommendation table may be stale. The
/// table is a "2026-07 snapshot"; models turn over on roughly this cadence.
pub const TABLE_STALE_DAYS: i64 = 90;

/// Age in days of the recommendation table (`GENERATED_AT`) relative to
/// `now_ts` (an RFC3339 UTC stamp, e.g. from `store::now`). None if either
/// date is malformed. Clamped at 0 for a future/skewed clock.
pub fn table_age_days_at(now_ts: &str) -> Option<i64> {
    let generated = format!("{GENERATED_AT}T00:00:00Z");
    let g = crate::store::parse_ts(&generated)?;
    let n = crate::store::parse_ts(now_ts)?;
    Some((n.saturating_sub(g) / 86_400) as i64)
}

/// Age in days of the recommendation table as of now.
pub fn table_age_days() -> Option<i64> {
    table_age_days_at(&crate::store::now())
}

/// The major version number in a CLI `--version` line: the integer at the
/// first digit run. "gh version 2.40.1" → 2, "v1.2.3" → 1, "codex 0.5" → 0,
/// a line with no digits → None. Used to flag a CLI major-version drift.
pub fn major_version(version_line: &str) -> Option<u64> {
    let start = version_line.find(|c: char| c.is_ascii_digit())?;
    let rest = &version_line[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .map(|e| start + e)
        .unwrap_or(version_line.len());
    version_line[start..end].parse().ok()
}

/// The result of doctor's live-launch probe: does the profile's model alias
/// still resolve? The three cases carry different doctor severities (ADR
/// 0007) — a bad model is actionable (❌); a transport/auth failure is not
/// routing's fault (⚠️).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The alias resolved and the CLI produced a reply.
    Ok,
    /// The CLI ran but rejected the model — alias retired/renamed.
    ModelInvalid,
    /// Network / auth / spawn failure — indistinguishable from a bad model
    /// only by fault, not by routing: don't fail doctor on it.
    Unavailable,
}

/// Production live-launch probe: fire a one-shot, ~1-token turn to check the
/// profile's model alias is still valid. Only the `claude` CLI has a known
/// probe form today; other commands report `Unavailable` (⚠️, non-fatal)
/// rather than a false `ModelInvalid`. Injected as a closure in doctor so
/// tests exercise the classification without spawning a real CLI.
pub fn probe_profile(profile: &AgentProfile) -> ProbeOutcome {
    // Match the CLI by basename so an absolute path to a `claude` binary (or a
    // test fake) still counts.
    let base = std::path::Path::new(&profile.command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&profile.command);
    if base != "claude" {
        return ProbeOutcome::Unavailable;
    }
    // Reuse the profile's own args (which carry `--model <alias>`) plus a
    // trivial one-shot prompt.
    let mut args = profile.args.clone();
    args.push("-p".into());
    args.push("reply: ok".into());
    match std::process::Command::new(&profile.command)
        .args(&args)
        .output()
    {
        Ok(out) if out.status.success() => ProbeOutcome::Ok,
        Ok(out) => {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
            .to_lowercase();
            // A model-rejection message names the model and an invalidity;
            // everything else (auth, rate limit, network) is Unavailable.
            let model_rejected = text.contains("model")
                && (text.contains("invalid")
                    || text.contains("unknown")
                    || text.contains("not found")
                    || text.contains("does not exist")
                    || text.contains("no such"));
            if model_rejected {
                ProbeOutcome::ModelInvalid
            } else {
                ProbeOutcome::Unavailable
            }
        }
        Err(_) => ProbeOutcome::Unavailable,
    }
}

/// The reserved profile name for the historical `[agent]` section. Users
/// steer a role back to it with `<role> = "default"`; it is never detected.
pub const DEFAULT_PROFILE: &str = "default";

/// Roles routing knows about: the 6 *kinds of work* a user answers "which
/// model should do this?" for, independent from the finer-grained internal
/// loop kinds (`runs.loop_kind`) — see [`routing_role_for_loop`] for that
/// mapping (ADR 0003 revision, issue #167). `self-reviewer` and `pr-reviewer`
/// are not loop kinds at all but the profiles of the internal self-review
/// turn (ADR 0006/0008) and the advisory guard-loop review on a published PR
/// (ADR 0008); both are shared across the plan and impl kinds (spec and impl
/// are managed by the same model). Explicit entries for anything outside this
/// set are a startup error.
pub const KNOWN_ROLES: &[&str] = &[
    "planner",
    "worker",
    "fixer",
    "self-reviewer",
    "pr-reviewer",
    "cleaner",
];

/// Deprecated routing role keys → their current name (ADR 0003 revision,
/// issue #167 — folds in the ADR 0008 review-role rename too). A config
/// still using an old key resolves as if it named the new one: the review
/// pair `reviewer`/`spec-reviewer`/`guard` → `pr-reviewer`, `impl-reviewer`/
/// `self-review` → `self-reviewer`, the worker pair `spec-worker` → `worker`,
/// and the fixer family `conflict-resolver`/`ci-fixer` → `fixer`.
const DEPRECATED_ROLE_ALIASES: &[(&str, &str)] = &[
    ("reviewer", "pr-reviewer"),
    ("spec-reviewer", "pr-reviewer"),
    ("guard", "pr-reviewer"),
    ("impl-reviewer", "self-reviewer"),
    ("self-review", "self-reviewer"),
    ("spec-worker", "worker"),
    ("conflict-resolver", "fixer"),
    ("ci-fixer", "fixer"),
];

/// Map a loop kind (`runs.loop_kind`) to its routing role — the fine↔coarse
/// bridge this ADR revision introduces (same pattern as `role_for_loop` for
/// pane lanes, `src/engine/mod.rs`). Internal loop kinds stay
/// fine-grained (budget/stats stay observable per loop); routing only cares
/// about the 6-role grouping. `self-reviewer` has no loop kind of its own —
/// it is resolved directly by name where the internal self-review turn runs
/// (`impl_review_lane`).
pub fn routing_role_for_loop(loop_kind: &str) -> &'static str {
    match loop_kind {
        "planner" => "planner",
        "fixer" | "ci-fixer" | "conflict-resolver" => "fixer",
        "guard" => "pr-reviewer",
        "cleaner" => "cleaner",
        // "worker" | "spec-worker", and anything unrecognized.
        _ => "worker",
    }
}

/// Map a (possibly deprecated) config role key to its canonical name.
fn canonical_role(role: &str) -> &str {
    DEPRECATED_ROLE_ALIASES
        .iter()
        .find(|(old, _)| *old == role)
        .map(|(_, new)| *new)
        .unwrap_or(role)
}

/// Look up a canonical role in a user's `[routing.roles]` map, honoring the
/// deprecated aliases (so a config still keyed on `reviewer = …` steers the
/// same profile as `pr-reviewer = …`).
fn role_override<'a>(roles: &'a HashMap<String, String>, role: &str) -> Option<&'a String> {
    if let Some(name) = roles.get(role) {
        return Some(name);
    }
    DEPRECATED_ROLE_ALIASES
        .iter()
        .filter(|(_, new)| *new == role)
        .find_map(|(old, _)| roles.get(*old))
}

/// The built-in profiles baked in alongside the recommendation table, so
/// `[routing] mode = "auto"` works with no other config. A user
/// `[agents.profiles.<same-name>]` overrides the builtin.
pub fn builtin_profiles() -> HashMap<String, AgentProfile> {
    let mut m = HashMap::new();
    m.insert(
        "claude-opus".to_string(),
        AgentProfile {
            command: "claude".into(),
            args: vec![
                "--dangerously-skip-permissions".into(),
                "--model".into(),
                "opus".into(),
            ],
            resume_args: vec!["--resume".into()],
            herdr_agent_hint: None,
            session_dir: None,
        },
    );
    m.insert(
        "claude-sonnet".to_string(),
        AgentProfile {
            command: "claude".into(),
            args: vec![
                "--dangerously-skip-permissions".into(),
                "--model".into(),
                "sonnet".into(),
            ],
            resume_args: vec!["--resume".into()],
            herdr_agent_hint: None,
            session_dir: None,
        },
    );
    m.insert(
        "codex".to_string(),
        AgentProfile {
            command: "codex".into(),
            args: vec!["--yolo".into()],
            resume_args: vec!["resume".into()],
            herdr_agent_hint: None,
            session_dir: None,
        },
    );
    m
}

/// The recommended profile chain for a role, tried in order and filtered by
/// detection; the terminal `default` is never detected, so a chain always
/// resolves. Unknown roles fall straight to `default`.
pub fn recommended_chain(role: &str) -> &'static [&'static str] {
    match role {
        // Small consumption, top leverage: best spec = fewest downstream turns.
        "planner" => &["claude-opus", DEFAULT_PROFILE],
        // Cross-vendor on purpose: reviewing with the author's model shares its
        // blind spots (and spares the Claude quota). Both the internal
        // self-review turn and the advisory pr-reviewer (guard loop) review
        // key off this.
        "self-reviewer" | "pr-reviewer" => &["codex", "claude-opus", DEFAULT_PROFILE],
        // The bulk of consumption (worker, incl. the spec-triggered worker)
        // and the narrow-scope fix-up work (fixer, incl. ci-fixer / conflict
        // resolution) both land on Sonnet — close to Opus on coding at
        // roughly half the quota/price.
        "worker" | "fixer" => &["claude-sonnet", DEFAULT_PROFILE],
        // cleaner (read-only hygiene sweep) and anything unrecognized.
        _ => &[DEFAULT_PROFILE],
    }
}

/// Look up a profile by name, merging (user profiles win) builtin profiles and
/// the reserved `default` (= `[agent]`). Err if the name is defined nowhere.
pub fn profile_by_name(cfg: &Config, name: &str) -> Result<AgentProfile> {
    if name == DEFAULT_PROFILE {
        return Ok(cfg.agent.clone());
    }
    if let Some(profile) = cfg
        .agents
        .as_ref()
        .and_then(|a| a.profiles.get(name))
        .cloned()
    {
        return Ok(profile);
    }
    if let Some(profile) = builtin_profiles().remove(name) {
        return Ok(profile);
    }
    bail!(
        "agent profile {name:?} is not defined — add [agents.profiles.{name}] \
         or use a built-in ({}), or \"default\"",
        builtin_profiles()
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// Whether `name` names a resolvable profile (user, builtin, or `default`).
fn profile_exists(cfg: &Config, name: &str) -> bool {
    profile_by_name(cfg, name).is_ok()
}

/// Resolve a role to a profile name, given a CLI detector. This is the
/// spawn-time lazy resolution pinned into `runs.agent_profile`:
///
/// 1. no `[routing]` → `default` (legacy, no detection);
/// 2. explicit `[routing.roles]` entry → that name verbatim (existence and
///    detection are enforced up front by [`validate`]);
/// 3. auto → first chain entry that both exists and detects; `manual` skips
///    the chain and yields `default`.
pub fn resolve(cfg: &Config, role: &str, detect: &dyn Fn(&str) -> bool) -> Result<String> {
    let Some(routing) = &cfg.routing else {
        return Ok(DEFAULT_PROFILE.to_string());
    };

    if let Some(name) = role_override(&routing.roles, role) {
        return Ok(name.clone());
    }

    if routing.mode == RoutingMode::Manual {
        return Ok(DEFAULT_PROFILE.to_string());
    }

    for candidate in recommended_chain(role) {
        if *candidate == DEFAULT_PROFILE {
            return Ok(DEFAULT_PROFILE.to_string());
        }
        let profile = match profile_by_name(cfg, candidate) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if detect(&profile.command) {
            return Ok((*candidate).to_string());
        }
    }
    // Chains always end in `default`, so this is unreachable; keep it safe.
    Ok(DEFAULT_PROFILE.to_string())
}

/// Startup validation for the explicit part of routing: the loud, early error
/// surface (`meguri watch` / `meguri run` entry). Checks the reserved
/// `default` name, unknown roles, missing profiles, and — for non-`default`
/// explicit picks — that the CLI actually detects.
pub fn validate(cfg: &Config, detect: &dyn Fn(&str) -> bool) -> Result<()> {
    if let Some(agents) = &cfg.agents
        && agents.profiles.contains_key(DEFAULT_PROFILE)
    {
        bail!(
            "[agents.profiles.default] is reserved — the `default` profile is \
             the [agent] section; configure it there"
        );
    }

    let Some(routing) = &cfg.routing else {
        return Ok(());
    };

    for (role, profile_name) in &routing.roles {
        if !KNOWN_ROLES.contains(&canonical_role(role)) {
            bail!(
                "[routing.roles] has unknown role {role:?} — valid roles: {}",
                KNOWN_ROLES.join(", "),
            );
        }
        if !profile_exists(cfg, profile_name) {
            bail!(
                "[routing.roles] {role} = {profile_name:?}, but that profile is \
                 not defined"
            );
        }
        // An explicit "default" is never detected (like the auto chain terminal).
        if profile_name != DEFAULT_PROFILE {
            let profile = profile_by_name(cfg, profile_name)?;
            if !detect(&profile.command) {
                bail!(
                    "[routing.roles] {role} = {profile_name:?}, but its command \
                     `{}` is not available (detection `{} --version` failed)",
                    profile.command,
                    profile.command,
                );
            }
        }
    }
    Ok(())
}

/// Production CLI detector: `command --version` exits 0. Mirrors doctor's
/// `run_capture`; injected as a closure in tests so the fallback chain runs
/// without spawning subprocesses.
pub fn detect_command(command: &str) -> bool {
    std::process::Command::new(command)
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(toml_str: &str) -> Config {
        toml::from_str(toml_str).unwrap()
    }

    /// A detector that only "finds" the named commands.
    fn only(available: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |cmd: &str| available.contains(&cmd)
    }

    #[test]
    fn legacy_config_resolves_every_role_to_default_without_detection() {
        let cfg = Config::default();
        // A detector that would panic if consulted — legacy must not detect.
        let never = |_: &str| panic!("detection must not run in legacy mode");
        for role in KNOWN_ROLES {
            assert_eq!(resolve(&cfg, role, &never).unwrap(), DEFAULT_PROFILE);
        }
    }

    #[test]
    fn profiles_only_without_routing_stays_legacy_and_inert() {
        // `[agents.profiles]` defined but no `[routing]`: still legacy.
        let cfg = cfg_from(
            r#"
[agents.profiles.codex]
command = "codex"
args = ["--yolo"]
"#,
        );
        let never = |_: &str| panic!("detection must not run when profiles are inert");
        assert_eq!(
            resolve(&cfg, "pr-reviewer", &never).unwrap(),
            DEFAULT_PROFILE
        );
        assert_eq!(resolve(&cfg, "worker", &never).unwrap(), DEFAULT_PROFILE);
    }

    #[test]
    fn explicit_role_beats_auto() {
        let cfg = cfg_from(
            r#"
[routing]
mode = "auto"

[routing.roles]
worker = "claude-opus"
"#,
        );
        // Even though auto would pick claude-sonnet for worker, explicit wins;
        // and detection is not consulted for the explicit pick in resolve().
        assert_eq!(
            resolve(&cfg, "worker", &only(&["claude"])).unwrap(),
            "claude-opus"
        );
    }

    #[test]
    fn auto_falls_back_along_the_chain() {
        let cfg = cfg_from("[routing]\nmode = \"auto\"\n");
        // codex present → pr-reviewer uses codex.
        assert_eq!(
            resolve(&cfg, "pr-reviewer", &only(&["codex", "claude"])).unwrap(),
            "codex"
        );
        // codex absent → pr-reviewer falls to claude-opus.
        assert_eq!(
            resolve(&cfg, "pr-reviewer", &only(&["claude"])).unwrap(),
            "claude-opus"
        );
        // neither present → pr-reviewer falls to default.
        assert_eq!(
            resolve(&cfg, "pr-reviewer", &only(&[])).unwrap(),
            DEFAULT_PROFILE
        );
        // The internal self-reviewer role shares the cross-vendor chain.
        assert_eq!(
            resolve(&cfg, "self-reviewer", &only(&["codex", "claude"])).unwrap(),
            "codex"
        );
    }

    #[test]
    fn auto_worker_and_fixer_prefer_sonnet_then_default() {
        let cfg = cfg_from("[routing]\nmode = \"auto\"\n");
        assert_eq!(
            resolve(&cfg, "worker", &only(&["claude"])).unwrap(),
            "claude-sonnet"
        );
        assert_eq!(
            resolve(&cfg, "worker", &only(&[])).unwrap(),
            DEFAULT_PROFILE
        );
        assert_eq!(
            resolve(&cfg, "fixer", &only(&["claude"])).unwrap(),
            "claude-sonnet"
        );
    }

    #[test]
    fn auto_cleaner_stays_on_default() {
        // cleaner has no model lean — it always lands on the default profile,
        // same as before it was registered (issue #167 fixed the omission,
        // not the outcome).
        let cfg = cfg_from("[routing]\nmode = \"auto\"\n");
        assert_eq!(
            resolve(&cfg, "cleaner", &only(&["codex", "claude"])).unwrap(),
            DEFAULT_PROFILE
        );
    }

    #[test]
    fn manual_mode_sends_unlisted_roles_to_default() {
        let cfg = cfg_from(
            r#"
[routing]
mode = "manual"

[routing.roles]
pr-reviewer = "codex"
"#,
        );
        // Listed role uses its explicit profile; unlisted roles go to default
        // with no detection (chain is off in manual).
        assert_eq!(
            resolve(&cfg, "pr-reviewer", &only(&["codex"])).unwrap(),
            "codex"
        );
        let never = |_: &str| panic!("manual unlisted roles must not detect");
        assert_eq!(resolve(&cfg, "worker", &never).unwrap(), DEFAULT_PROFILE);
    }

    #[test]
    fn deprecated_role_keys_steer_the_renamed_roles() {
        // Configs still using pre-#167 keys resolve to the current 6 roles:
        // `reviewer` / `spec-reviewer` / `guard` → `pr-reviewer`,
        // `impl-reviewer` / `self-review` → `self-reviewer`, `spec-worker` →
        // `worker`, `conflict-resolver` / `ci-fixer` → `fixer`. validate()
        // accepts them all.
        let cfg = cfg_from(
            r#"
[routing]
mode = "manual"

[routing.roles]
reviewer = "codex"
impl-reviewer = "claude-opus"
spec-worker = "claude-sonnet"
ci-fixer = "claude-opus"
"#,
        );
        assert_eq!(
            resolve(&cfg, "pr-reviewer", &only(&["codex"])).unwrap(),
            "codex"
        );
        assert_eq!(
            resolve(&cfg, "self-reviewer", &only(&["claude"])).unwrap(),
            "claude-opus"
        );
        assert_eq!(
            resolve(&cfg, "worker", &only(&["claude"])).unwrap(),
            "claude-sonnet"
        );
        assert_eq!(
            resolve(&cfg, "fixer", &only(&["claude"])).unwrap(),
            "claude-opus"
        );
        validate(&cfg, &only(&["codex", "claude"])).unwrap();

        // The current `spec-reviewer` key and the old `guard`/`conflict-resolver`
        // keys also still steer their new roles.
        let cfg2 = cfg_from(
            r#"
[routing]
mode = "manual"

[routing.roles]
spec-reviewer = "codex"
guard = "codex"
conflict-resolver = "codex"
"#,
        );
        assert_eq!(
            resolve(&cfg2, "pr-reviewer", &only(&["codex"])).unwrap(),
            "codex"
        );
        assert_eq!(resolve(&cfg2, "fixer", &only(&["codex"])).unwrap(), "codex");
        validate(&cfg2, &only(&["codex"])).unwrap();
    }

    #[test]
    fn routing_role_for_loop_groups_loop_kinds_by_kind_of_work() {
        // The 6-role grouping (issue #167): fixer's family, worker's family,
        // and the previously-unregistered ci-fixer / cleaner all resolve.
        assert_eq!(routing_role_for_loop("planner"), "planner");
        assert_eq!(routing_role_for_loop("worker"), "worker");
        assert_eq!(routing_role_for_loop("spec-worker"), "worker");
        assert_eq!(routing_role_for_loop("fixer"), "fixer");
        assert_eq!(routing_role_for_loop("ci-fixer"), "fixer");
        assert_eq!(routing_role_for_loop("conflict-resolver"), "fixer");
        assert_eq!(routing_role_for_loop("guard"), "pr-reviewer");
        assert_eq!(routing_role_for_loop("cleaner"), "cleaner");
    }

    #[test]
    fn ci_fixer_and_cleaner_join_their_family_auto_chain() {
        // Before #167, ci-fixer/cleaner were missing from KNOWN_ROLES and
        // recommended_chain, so auto routing silently dropped them to
        // `default` while their siblings rode the family chain, and an
        // explicit `[routing.roles] ci-fixer = ...` failed at startup. Now
        // both loop kinds resolve through their role's chain like the rest
        // of the family.
        let cfg = cfg_from("[routing]\nmode = \"auto\"\n");
        for kind in ["fixer", "ci-fixer", "conflict-resolver"] {
            assert_eq!(
                resolve(&cfg, routing_role_for_loop(kind), &only(&["claude"])).unwrap(),
                "claude-sonnet",
                "loop: {kind}"
            );
        }
        validate(
            &cfg_from("[routing]\nmode = \"manual\"\n[routing.roles]\nfixer = \"claude-sonnet\"\ncleaner = \"claude-sonnet\"\n"),
            &only(&["claude"]),
        )
        .unwrap();
    }

    #[test]
    fn explicit_default_is_allowed_and_undetected() {
        let cfg = cfg_from(
            r#"
[routing]
mode = "auto"

[routing.roles]
worker = "default"
"#,
        );
        assert_eq!(
            resolve(&cfg, "worker", &only(&["claude"])).unwrap(),
            DEFAULT_PROFILE
        );
        // validate() must not reject or detect an explicit "default".
        let never = |_: &str| panic!("explicit default must not be detected");
        validate(&cfg, &never).unwrap();
    }

    #[test]
    fn validate_rejects_unknown_role() {
        let cfg = cfg_from(
            r#"
[routing]
[routing.roles]
nonsense = "codex"
"#,
        );
        let err = validate(&cfg, &only(&["codex"])).unwrap_err().to_string();
        assert!(err.contains("unknown role"), "{err}");
    }

    #[test]
    fn validate_rejects_missing_profile() {
        let cfg = cfg_from(
            r#"
[routing]
[routing.roles]
worker = "ghost"
"#,
        );
        let err = validate(&cfg, &only(&["claude"])).unwrap_err().to_string();
        assert!(err.contains("not defined"), "{err}");
    }

    #[test]
    fn validate_rejects_undetected_explicit_profile() {
        let cfg = cfg_from(
            r#"
[routing]
[routing.roles]
reviewer = "codex"
"#,
        );
        // codex is a builtin (profile exists) but the CLI isn't installed.
        let err = validate(&cfg, &only(&["claude"])).unwrap_err().to_string();
        assert!(err.contains("not available"), "{err}");
        // With codex present, it passes.
        validate(&cfg, &only(&["codex"])).unwrap();
    }

    #[test]
    fn validate_rejects_reserved_default_profile_name() {
        let cfg = cfg_from(
            r#"
[agents.profiles.default]
command = "claude"
"#,
        );
        let err = validate(&cfg, &only(&["claude"])).unwrap_err().to_string();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn legacy_config_validates_without_detection() {
        // No routing, no profiles: validate is a no-op.
        let never = |_: &str| panic!("legacy validate must not detect");
        validate(&Config::default(), &never).unwrap();
    }

    #[test]
    fn user_profile_overrides_builtin() {
        let cfg = cfg_from(
            r#"
[agents.profiles.codex]
command = "my-codex"
args = ["--foo"]
"#,
        );
        let p = profile_by_name(&cfg, "codex").unwrap();
        assert_eq!(p.command, "my-codex");
        assert_eq!(p.args, vec!["--foo"]);
    }

    #[test]
    fn table_age_days_counts_from_generated_at() {
        // GENERATED_AT is 2026-07-12. 90 days later crosses the stale line.
        assert_eq!(table_age_days_at("2026-07-12T00:00:00Z"), Some(0));
        assert_eq!(table_age_days_at("2026-07-13T00:00:00Z"), Some(1));
        assert_eq!(table_age_days_at("2026-10-11T00:00:00Z"), Some(91));
        assert!(table_age_days_at("2026-10-11T00:00:00Z").unwrap() > TABLE_STALE_DAYS);
        // A date before the table's own date clamps to 0, never negative.
        assert_eq!(table_age_days_at("2026-01-01T00:00:00Z"), Some(0));
        assert_eq!(table_age_days_at("garbage"), None);
    }

    #[test]
    fn major_version_extracts_leading_integer() {
        assert_eq!(major_version("gh version 2.40.1 (2024-01-01)"), Some(2));
        assert_eq!(major_version("git version 2.39.2"), Some(2));
        assert_eq!(major_version("v1.2.3"), Some(1));
        assert_eq!(major_version("codex 0.5.0"), Some(0));
        assert_eq!(major_version("claude 13.0.1"), Some(13));
        assert_eq!(major_version("reply: ok"), None);
    }

    #[test]
    fn probe_non_claude_is_unavailable_not_a_false_negative() {
        // We can't drive an arbitrary CLI's one-shot form, so report a
        // non-fatal Unavailable rather than a false ModelInvalid.
        let codex = builtin_profiles().remove("codex").unwrap();
        assert_eq!(probe_profile(&codex), ProbeOutcome::Unavailable);
    }

    #[test]
    fn default_profile_is_the_agent_section() {
        let cfg = cfg_from(
            r#"
[agent]
command = "claude"
args = ["--permission-mode", "acceptEdits"]
"#,
        );
        let p = profile_by_name(&cfg, DEFAULT_PROFILE).unwrap();
        assert_eq!(p.args, vec!["--permission-mode", "acceptEdits"]);
    }
}
