//! Launch mode: per-role choice of *how* a turn is launched (issue #169,
//! ADR 0012) — `pane` (the historical live mux pane, attach-able, nudged by
//! the turn engine) or `direct` (a plain subprocess for one turn, no pane,
//! no attach, no nudging). Orthogonal to `[routing]`'s profile axis (`crate::
//! routing`): routing decides *which model*, launch decides *how it runs*.
//! Both key off the same 6-role vocabulary ([`crate::routing::KNOWN_ROLES`]).
//!
//! Unlike routing, there is no legacy/off state: a role with no
//! `[launch.roles]` entry always resolves through [`recommended_mode`] — the
//! built-in table below. An explicit entry always wins over it (same rule as
//! routing's explicit-beats-auto).

use anyhow::{Result, bail};

use crate::config::{Config, LaunchMode};
use crate::routing::{self, KNOWN_ROLES};

/// The recommended launch mode for each of the 6 routing roles ("kind of
/// work"), decided by whether anything needs the turn's execution vehicle to
/// outlive the turn — i.e. whether a human or a later turn needs to attach:
///
/// - `planner` / `worker` / `fixer`: the author lane's conversation
///   continues across turns and a stuck run needs a human to attach
///   (ADR 0004's core).
/// - `pr-reviewer`: re-review rounds keep context, and attach is valuable
///   (the first candidate to move to `direct` if throughput matters more).
/// - `self-reviewer`: an internal loop (ADR 0006) — no human ever attaches,
///   and its transcript never leaves the worktree.
/// - `cleaner`: a read-only sweep that already self-reclaims its pane the
///   moment it finishes (D9) — nothing to attach to.
///
/// Unknown roles default to `Pane`, the historically safe behavior.
pub fn recommended_mode(role: &str) -> LaunchMode {
    match routing::canonical_role(role) {
        "self-reviewer" | "cleaner" => LaunchMode::Direct,
        _ => LaunchMode::Pane,
    }
}

/// Resolve a role's launch mode: an explicit `[launch.roles]` entry (honoring
/// the same deprecated role aliases routing accepts) always wins; otherwise
/// [`recommended_mode`].
pub fn resolve(cfg: &Config, role: &str) -> LaunchMode {
    if let Some(mode) = role_override(cfg, role) {
        return mode;
    }
    recommended_mode(role)
}

fn role_override(cfg: &Config, role: &str) -> Option<LaunchMode> {
    if let Some(mode) = cfg.launch.roles.get(role) {
        return Some(*mode);
    }
    cfg.launch
        .roles
        .iter()
        .find(|(old, _)| routing::canonical_role(old) == role)
        .map(|(_, mode)| *mode)
}

/// Startup validation: every `[launch.roles]` key must name one of the 6
/// known roles (or a deprecated alias) — a typo is a loud config error, not a
/// silently-ignored entry. Mode values are validated by serde at parse time
/// (`"pane"` / `"direct"` only), so there is nothing else to check here.
pub fn validate(cfg: &Config) -> Result<()> {
    for role in cfg.launch.roles.keys() {
        if !KNOWN_ROLES.contains(&routing::canonical_role(role)) {
            bail!(
                "[launch.roles] has unknown role {role:?} — valid roles: {}",
                KNOWN_ROLES.join(", "),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(toml_str: &str) -> Config {
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn no_launch_section_uses_the_recommended_table() {
        let cfg = Config::default();
        assert_eq!(resolve(&cfg, "planner"), LaunchMode::Pane);
        assert_eq!(resolve(&cfg, "worker"), LaunchMode::Pane);
        assert_eq!(resolve(&cfg, "fixer"), LaunchMode::Pane);
        assert_eq!(resolve(&cfg, "pr-reviewer"), LaunchMode::Pane);
        assert_eq!(resolve(&cfg, "self-reviewer"), LaunchMode::Direct);
        assert_eq!(resolve(&cfg, "cleaner"), LaunchMode::Direct);
    }

    #[test]
    fn explicit_role_beats_auto() {
        let cfg = cfg_from(
            r#"
[launch.roles]
pr-reviewer = "direct"
self-reviewer = "pane"
"#,
        );
        assert_eq!(resolve(&cfg, "pr-reviewer"), LaunchMode::Direct);
        assert_eq!(resolve(&cfg, "self-reviewer"), LaunchMode::Pane);
        // Untouched roles still fall through to the recommended table.
        assert_eq!(resolve(&cfg, "worker"), LaunchMode::Pane);
        assert_eq!(resolve(&cfg, "cleaner"), LaunchMode::Direct);
    }

    #[test]
    fn deprecated_role_keys_steer_the_renamed_roles() {
        let cfg = cfg_from(
            r#"
[launch.roles]
guard = "direct"
spec-worker = "direct"
"#,
        );
        assert_eq!(resolve(&cfg, "pr-reviewer"), LaunchMode::Direct);
        assert_eq!(resolve(&cfg, "worker"), LaunchMode::Direct);
        validate(&cfg).unwrap();
    }

    #[test]
    fn validate_rejects_unknown_role() {
        let cfg = cfg_from(
            r#"
[launch.roles]
nonsense = "direct"
"#,
        );
        let err = validate(&cfg).unwrap_err().to_string();
        assert!(err.contains("unknown role"), "{err}");
    }

    #[test]
    fn validate_accepts_no_launch_section() {
        validate(&Config::default()).unwrap();
    }

    #[test]
    fn invalid_mode_value_is_a_parse_error() {
        let err = toml::from_str::<Config>("[launch.roles]\nworker = \"floating\"\n").unwrap_err();
        assert!(format!("{err}").contains("unknown variant"), "{err}");
    }
}
