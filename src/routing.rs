//! Role-based agent routing (issue #64, routing 1/3).
//!
//! Each loop has a role (`runs.loop_kind`) whose cost/quality profile is
//! stable, so we route a role — not an estimated issue difficulty — to a
//! launch profile. Two rules are load-bearing (ADR 0003):
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

/// The reserved profile name for the historical `[agent]` section. Users
/// steer a role back to it with `<role> = "default"`; it is never detected.
pub const DEFAULT_PROFILE: &str = "default";

/// Loop kinds routing knows about (= `runs.loop_kind`). Explicit entries for
/// anything outside this set are a startup error.
pub const KNOWN_ROLES: &[&str] = &[
    "planner",
    "reviewer",
    "worker",
    "spec-worker",
    "fixer",
    "conflict-resolver",
];

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
        // blind spots (and spares the Claude quota).
        "reviewer" => &["codex", "claude-opus", DEFAULT_PROFILE],
        // The bulk of consumption; Sonnet lands close to Opus on coding at
        // roughly half the quota/price.
        "worker" | "spec-worker" => &["claude-sonnet", DEFAULT_PROFILE],
        // Narrow scope, small diffs — Sonnet is plenty.
        "fixer" | "conflict-resolver" => &["claude-sonnet", DEFAULT_PROFILE],
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

    if let Some(name) = routing.roles.get(role) {
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
        if !KNOWN_ROLES.contains(&role.as_str()) {
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
        assert_eq!(resolve(&cfg, "reviewer", &never).unwrap(), DEFAULT_PROFILE);
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
        // codex present → reviewer uses codex.
        assert_eq!(
            resolve(&cfg, "reviewer", &only(&["codex", "claude"])).unwrap(),
            "codex"
        );
        // codex absent → reviewer falls to claude-opus.
        assert_eq!(
            resolve(&cfg, "reviewer", &only(&["claude"])).unwrap(),
            "claude-opus"
        );
        // neither present → reviewer falls to default.
        assert_eq!(
            resolve(&cfg, "reviewer", &only(&[])).unwrap(),
            DEFAULT_PROFILE
        );
    }

    #[test]
    fn auto_worker_prefers_sonnet_then_default() {
        let cfg = cfg_from("[routing]\nmode = \"auto\"\n");
        assert_eq!(
            resolve(&cfg, "worker", &only(&["claude"])).unwrap(),
            "claude-sonnet"
        );
        assert_eq!(
            resolve(&cfg, "worker", &only(&[])).unwrap(),
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
reviewer = "codex"
"#,
        );
        // Listed role uses its explicit profile; unlisted roles go to default
        // with no detection (chain is off in manual).
        assert_eq!(
            resolve(&cfg, "reviewer", &only(&["codex"])).unwrap(),
            "codex"
        );
        let never = |_: &str| panic!("manual unlisted roles must not detect");
        assert_eq!(resolve(&cfg, "worker", &never).unwrap(), DEFAULT_PROFILE);
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
