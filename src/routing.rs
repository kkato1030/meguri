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

/// Roles routing knows about: the *kinds of work* a user answers "which
/// model should do this?" for, independent from the finer-grained internal
/// loop kinds (`runs.loop_kind`) — see [`routing_role_for_loop`] for that
/// mapping (ADR 0003 revision, issue #167). `self-reviewer` is not a loop
/// kind at all but the profile of the internal self-review turn (ADR
/// 0006/0008); `pr-reviewer` is both a role and the `pr-reviewer` loop's own
/// `runs.loop_kind` (the advisory external review on a published PR, ADR
/// 0008). Both are shared across the plan and impl kinds (spec and impl are
/// managed by the same model). `refiner` is likewise not a loop kind but the
/// profile of `meguri add`'s one-shot refine (ADR 0006), routed like any
/// other role so a cheap model can be steered to it. Explicit entries for
/// anything outside this set are a startup error.
pub const KNOWN_ROLES: &[&str] = &[
    "planner",
    "worker",
    "fixer",
    "self-reviewer",
    "pr-reviewer",
    "cleaner",
    "refiner",
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
/// bridge this ADR revision introduces (same pattern as `lane_for_loop` for
/// pane lanes, `src/engine/mod.rs`). Internal loop kinds stay
/// fine-grained (budget/stats stay observable per loop); routing only cares
/// about the 6-role grouping. `self-reviewer` has no loop kind of its own —
/// it is resolved directly by name where the internal self-review turn runs
/// (`self_review_lane`).
pub fn routing_role_for_loop(loop_kind: &str) -> &'static str {
    match loop_kind {
        "planner" => "planner",
        "fixer" | "ci-fixer" | "conflict-resolver" => "fixer",
        "pr-reviewer" => "pr-reviewer",
        "cleaner" => "cleaner",
        // "worker" | "spec-worker", and anything unrecognized.
        _ => "worker",
    }
}

/// Map a (possibly deprecated) config role key to its canonical name. Shared
/// with `crate::launch` (same 6-role vocabulary) and public so `config` can
/// canonicalize `[prompts]` keys the same way (issue #149).
pub fn canonical_role(role: &str) -> &str {
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

/// Known CLIs that support a headless one-shot mode, and the argv that enters
/// it, keyed by the profile `command`'s base name. Used to fill in
/// [`effective_headless_args`] when a profile leaves `headless_args` unset, so
/// a zero-config `meguri init` (whose `default` command is `claude`) still
/// refines. Kept deliberately tiny: only exact base-name matches, never a
/// guess at an unknown CLI's flags.
fn known_headless_args(command: &str) -> Option<Vec<String>> {
    let base = std::path::Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(command);
    match base {
        "claude" => Some(vec!["-p".to_string()]),
        _ => None,
    }
}

/// The argv that actually launches a profile's headless one-shot refine, or
/// `None` when the profile has no headless mode (refine is then skipped with a
/// one-line warning — never a silent fallback). Resolution, in order:
///
/// 1. explicit non-empty `headless_args` → used verbatim (a complete argv);
/// 2. explicit empty `[]` → `None`: the opt-out sentinel (TOML can't write
///    `None`, and an empty argv is a valid-looking-but-broken launch);
/// 3. unset + a known headless CLI `command` → that CLI's default argv;
/// 4. unset + an unknown `command` → `None` (unsupported).
pub fn effective_headless_args(profile: &AgentProfile) -> Option<Vec<String>> {
    match &profile.headless_args {
        Some(args) if !args.is_empty() => Some(args.clone()),
        Some(_) => None,
        None => known_headless_args(&profile.command),
    }
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
            // Headless refine keeps the model but never yolo (read-only).
            headless_args: Some(vec!["-p".into(), "--model".into(), "opus".into()]),
            direct_args: vec!["-p".into()],
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
            headless_args: Some(vec!["-p".into(), "--model".into(), "sonnet".into()]),
            direct_args: vec!["-p".into()],
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
            headless_args: None,
            // codex's non-interactive one-shot form is the `exec` subcommand
            // (mirrors `resume_args` also being a bare subcommand).
            direct_args: vec!["exec".into()],
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
        // self-review turn and the advisory pr-reviewer loop's review key off
        // this.
        "self-reviewer" | "pr-reviewer" => &["codex", "claude-opus", DEFAULT_PROFILE],
        // The bulk of consumption (worker, incl. the spec-triggered worker)
        // and the narrow-scope fix-up work (fixer, incl. ci-fixer / conflict
        // resolution) both land on Sonnet — close to Opus on coding at
        // roughly half the quota/price.
        "worker" | "fixer" => &["claude-sonnet", DEFAULT_PROFILE],
        // One-shot title/body tidy-up (`meguri add`): the cheapest capable
        // model is plenty; tilt the chain to the cheap side (ADR 0006).
        "refiner" => &["claude-sonnet", DEFAULT_PROFILE],
        // cleaner (read-only hygiene sweep) and anything unrecognized.
        _ => &[DEFAULT_PROFILE],
    }
}

/// The escalation chain for a role: the ordered profiles (weakest → strongest)
/// a stuck run climbs (routing 3/3, issue #66). Distinct from
/// [`recommended_chain`], which is a *detection fallback* ending in `default`
/// (the weakening direction); escalation goes the strengthening direction.
/// `worker` / `fixer` climb sonnet → opus by default; `planner` and the
/// reviewer roles are already at the top, so they have no chain (never
/// escalate). A `[escalation]` role entry overrides the default (an empty
/// chain disables escalation for that role).
pub fn escalation_chain(cfg: &Config, role: &str) -> Vec<String> {
    let canonical = canonical_role(role);
    if let Some(chain) = escalation_override(&cfg.escalation.roles, canonical) {
        return chain.clone();
    }
    default_escalation_chain(canonical)
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// The built-in escalation chain for a role (before any `[escalation]`
/// override). Only the implementation-heavy roles lean cheap-then-strong.
fn default_escalation_chain(role: &str) -> &'static [&'static str] {
    match role {
        "worker" | "fixer" => &["claude-sonnet", "claude-opus"],
        _ => &[],
    }
}

/// Look up a role's escalation-chain override, honoring the deprecated role
/// aliases the same way [`role_override`] does for `[routing.roles]`.
fn escalation_override<'a>(
    roles: &'a HashMap<String, Vec<String>>,
    role: &str,
) -> Option<&'a Vec<String>> {
    if let Some(chain) = roles.get(role) {
        return Some(chain);
    }
    DEPRECATED_ROLE_ALIASES
        .iter()
        .filter(|(_, new)| *new == role)
        .find_map(|(old, _)| roles.get(*old))
}

/// The next stronger profile to escalate a run to, or None when it should not
/// escalate. The decision is anchored on where the run's *current* pinned
/// profile sits in the role's escalation chain (issue #66):
///
/// - a profile not in the chain (e.g. `default` from a manual/fallback pin, or
///   an explicit off-chain `[routing.roles]` pick) never escalates — routing
///   1/3's contract is preserved;
/// - from a chain entry that isn't the last, walk strictly upward to the first
///   candidate that both exists and is detected, skipping unusable ones (the
///   same skip flavor as [`resolve`], but only in the stronger direction);
/// - the chain's last entry, or no usable stronger entry, yields None.
pub fn next_escalation(
    cfg: &Config,
    role: &str,
    current_profile: &str,
    detect: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let chain = escalation_chain(cfg, role);
    let pos = chain.iter().position(|p| p == current_profile)?;
    for candidate in &chain[pos + 1..] {
        let Ok(profile) = profile_by_name(cfg, candidate) else {
            continue;
        };
        if detect(&profile.command) {
            return Some(candidate.clone());
        }
    }
    None
}

/// The alternative ("explore") profile a canary run is diverted to instead of
/// the mainline pick (issue #66): the next entry after the *auto* pick in the
/// role's [`recommended_chain`] that exists, is detected, and differs from the
/// mainline. None (no divert) when the mainline is already the chain's tail or
/// nothing after it is usable — the run then stays on the mainline. `default`
/// is a legitimate alternative here (it answers "is routing better than the
/// bare `[agent]`?") and is returned without detection, mirroring [`resolve`].
///
/// Explore only ever canaries the *auto recommendation*. It is a no-op when the
/// role's profile isn't the auto pick — legacy (no `[routing]`), manual mode, or
/// an explicit `[routing.roles]` entry — because ADR 0003 promises an explicit
/// pick is honored verbatim: a user who pinned `worker = "claude-sonnet"` gets
/// exactly that, never a silently-diverted next-in-chain.
pub fn explore_alternative(
    cfg: &Config,
    role: &str,
    detect: &dyn Fn(&str) -> bool,
) -> Option<String> {
    let routing = cfg.routing.as_ref()?;
    // Manual mode has no auto recommendation to canary; an explicit override is
    // the user's deliberate choice and must not be diverted.
    if routing.mode == RoutingMode::Manual || role_override(&routing.roles, role).is_some() {
        return None;
    }
    // With those ruled out, `resolve` returns the auto chain pick = the mainline.
    let mainline = resolve(cfg, role, detect).ok()?;
    let chain = recommended_chain(role);
    let pos = chain.iter().position(|c| *c == mainline)?;
    for candidate in &chain[pos + 1..] {
        if *candidate == DEFAULT_PROFILE {
            // `default` always resolves and, since the mainline sits earlier in
            // the chain, necessarily differs from it.
            return Some(DEFAULT_PROFILE.to_string());
        }
        let Ok(profile) = profile_by_name(cfg, candidate) else {
            continue;
        };
        if detect(&profile.command) {
            return Some((*candidate).to_string());
        }
    }
    None
}

/// Whether a run targeting `number` (issue or task) falls in the explore
/// fraction, decided deterministically so the same target always lands the same
/// way and tests are reproducible (issue #66). Uses an explicit FNV-1a hash —
/// NOT `std`'s `DefaultHasher`, whose output isn't stable across toolchains.
pub fn is_explore(number: i64, ratio: f64) -> bool {
    if ratio <= 0.0 {
        return false;
    }
    if ratio >= 1.0 {
        return true;
    }
    (fnv1a_u64(number as u64) % 10_000) < (ratio * 10_000.0) as u64
}

/// FNV-1a over the 8 little-endian bytes of `n`. Small, explicit, and stable.
fn fnv1a_u64(n: u64) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in n.to_le_bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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

    // `[escalation]` chain overrides get the same loud, up-front surface: an
    // unknown role name, an undefined profile, or a `default` entry (which has
    // no defined "strength" and would muddy the in-chain position check) is a
    // startup error, not a silent no-op (routing 3/3, issue #66). Checked
    // independently of `[routing]` because `[escalation]` is a top-level
    // section — a typo should be loud even in a legacy config (where escalation
    // stays inert). Detection is NOT required: an escalation target that isn't
    // installed is skipped at escalation time; only the chain's shape is checked.
    for (role, chain) in &cfg.escalation.roles {
        if !KNOWN_ROLES.contains(&canonical_role(role)) {
            bail!(
                "[escalation] has unknown role {role:?} — valid roles: {}",
                KNOWN_ROLES.join(", "),
            );
        }
        for profile_name in chain {
            if profile_name == DEFAULT_PROFILE {
                bail!(
                    "[escalation] {role} chain lists {DEFAULT_PROFILE:?}, but the \
                     default profile cannot be an escalation target (it has no \
                     defined strength) — escalation climbs toward stronger models"
                );
            }
            if !profile_exists(cfg, profile_name) {
                bail!(
                    "[escalation] {role} chain lists {profile_name:?}, but that \
                     profile is not defined"
                );
            }
        }
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
        assert_eq!(routing_role_for_loop("pr-reviewer"), "pr-reviewer");
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
    fn builtin_claude_headless_args_keep_model_and_drop_yolo() {
        // Structural guarantee (spec 論点1/論点4): the headless argv carries the
        // routed model but never the yolo flag, so refine stays read-only.
        for name in ["claude-opus", "claude-sonnet"] {
            let p = profile_by_name(&Config::default(), name).unwrap();
            let argv = effective_headless_args(&p).unwrap();
            assert!(argv.contains(&"--model".to_string()), "{name}: keeps model");
            assert!(
                !argv.contains(&"--dangerously-skip-permissions".to_string()),
                "{name}: no yolo in headless"
            );
        }
    }

    #[test]
    fn effective_headless_args_resolution_rules() {
        let base = |command: &str, headless: Option<Vec<String>>| AgentProfile {
            command: command.into(),
            args: vec![],
            resume_args: vec![],
            headless_args: headless,
            direct_args: vec![],
            herdr_agent_hint: None,
            session_dir: None,
        };
        // Rule 3: unset + known CLI (default profile is `claude`) → its default.
        assert_eq!(
            effective_headless_args(&AgentProfile::default()),
            Some(vec!["-p".to_string()])
        );
        assert_eq!(
            effective_headless_args(&base("/usr/local/bin/claude", None)),
            Some(vec!["-p".to_string()]),
            "base-name match ignores the directory"
        );
        // Rule 1: explicit non-empty wins over inheritance.
        assert_eq!(
            effective_headless_args(&base("claude", Some(vec!["-p".into(), "--model".into(), "haiku".into()]))),
            Some(vec!["-p".to_string(), "--model".to_string(), "haiku".to_string()])
        );
        // Rule 2: explicit empty = opt-out sentinel.
        assert_eq!(effective_headless_args(&base("claude", Some(vec![]))), None);
        // Rule 4: unset + unknown CLI = unsupported.
        assert_eq!(effective_headless_args(&base("my-llm", None)), None);
    }

    #[test]
    fn refiner_is_a_known_role_routed_to_the_cheap_chain() {
        assert!(KNOWN_ROLES.contains(&"refiner"));
        let cfg = cfg_from("[routing]\nmode = \"auto\"\n");
        assert_eq!(
            resolve(&cfg, "refiner", &only(&["claude"])).unwrap(),
            "claude-sonnet"
        );
        assert_eq!(
            resolve(&cfg, "refiner", &only(&[])).unwrap(),
            DEFAULT_PROFILE
        );
        // Legacy (no [routing]) sends refiner to default like every role.
        assert_eq!(
            resolve(&Config::default(), "refiner", &|_: &str| true).unwrap(),
            DEFAULT_PROFILE
        );
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

    // --- routing 3/3 (issue #66): escalation + explore --------------------

    #[test]
    fn escalation_chain_defaults_and_overrides() {
        let cfg = Config::default();
        // Implementation roles lean cheap→strong; the rest have no chain.
        assert_eq!(
            escalation_chain(&cfg, "worker"),
            vec!["claude-sonnet", "claude-opus"]
        );
        assert_eq!(
            escalation_chain(&cfg, "fixer"),
            vec!["claude-sonnet", "claude-opus"]
        );
        assert!(escalation_chain(&cfg, "planner").is_empty());
        assert!(escalation_chain(&cfg, "cleaner").is_empty());

        // A `[escalation]` role entry replaces the default; an empty chain
        // disables escalation for that role.
        let cfg = cfg_from(
            r#"
[escalation]
worker = ["claude-sonnet", "claude-opus", "codex"]
fixer = []
"#,
        );
        assert_eq!(
            escalation_chain(&cfg, "worker"),
            vec!["claude-sonnet", "claude-opus", "codex"]
        );
        assert!(escalation_chain(&cfg, "fixer").is_empty());
        // A deprecated key still steers its renamed role.
        let cfg = cfg_from("[escalation]\nspec-worker = [\"claude-opus\"]\n");
        assert_eq!(escalation_chain(&cfg, "worker"), vec!["claude-opus"]);
    }

    #[test]
    fn next_escalation_climbs_only_from_within_the_chain() {
        let cfg = Config::default();
        let detect = only(&["claude"]);
        // From a mid-chain entry → the next stronger one.
        assert_eq!(
            next_escalation(&cfg, "worker", "claude-sonnet", &detect).as_deref(),
            Some("claude-opus")
        );
        // From the chain tail → no escalation.
        assert_eq!(
            next_escalation(&cfg, "worker", "claude-opus", &detect),
            None
        );
        // `default` (manual / detection-fallback pin) is off-chain → never
        // escalates: routing 1/3's contract is preserved.
        assert_eq!(
            next_escalation(&cfg, "worker", DEFAULT_PROFILE, &detect),
            None
        );
        // An explicit off-chain pick (e.g. `worker = "codex"`) → no escalation.
        assert_eq!(next_escalation(&cfg, "worker", "codex", &detect), None);
        // A role with no chain never escalates.
        assert_eq!(
            next_escalation(&cfg, "planner", "claude-opus", &detect),
            None
        );
    }

    #[test]
    fn next_escalation_skips_undetected_candidates_upward() {
        // A mid-chain entry that isn't installed is skipped toward the stronger
        // end, same flavor as auto `resolve`, but only in the stronger direction.
        let cfg = cfg_from(
            r#"
[escalation]
worker = ["claude-sonnet", "codex", "claude-opus"]
"#,
        );
        // codex CLI absent → skip it, land on claude-opus.
        assert_eq!(
            next_escalation(&cfg, "worker", "claude-sonnet", &only(&["claude"])).as_deref(),
            Some("claude-opus")
        );
        // Nothing stronger is detected → no escalation.
        assert_eq!(
            next_escalation(&cfg, "worker", "claude-sonnet", &only(&[])),
            None
        );
    }

    #[test]
    fn validate_checks_escalation_chain_shape() {
        // Unknown role name.
        let err = validate(
            &cfg_from("[escalation]\nnonsense = [\"claude-opus\"]\n"),
            &only(&["claude"]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown role"), "{err}");

        // `default` is not a legal escalation target.
        let err = validate(
            &cfg_from("[escalation]\nworker = [\"claude-sonnet\", \"default\"]\n"),
            &only(&["claude"]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("default profile cannot be"), "{err}");

        // Undefined profile.
        let err = validate(
            &cfg_from("[escalation]\nworker = [\"ghost\"]\n"),
            &only(&["claude"]),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not defined"), "{err}");

        // A well-formed chain passes — even with no `[routing]` (legacy), and
        // without the CLI installed (detection isn't required for the shape check).
        validate(
            &cfg_from("[escalation]\nworker = [\"claude-sonnet\", \"claude-opus\"]\n"),
            &only(&[]),
        )
        .unwrap();
    }

    #[test]
    fn is_explore_is_deterministic_and_respects_the_bounds() {
        // ratio 0 (the default) never explores; ratio ≥ 1 always does.
        for n in 0..50 {
            assert!(!is_explore(n, 0.0), "n={n}");
            assert!(is_explore(n, 1.0), "n={n}");
        }
        // Same target, same verdict every call (reproducible).
        for n in 0..50 {
            assert_eq!(is_explore(n, 0.3), is_explore(n, 0.3), "n={n}");
        }
        // Roughly the requested fraction over a spread of targets (loose bound;
        // this is a determinism test, not a statistics one).
        let hits = (0..1000).filter(|&n| is_explore(n, 0.2)).count();
        assert!((100..300).contains(&hits), "hits={hits}");
    }

    #[test]
    fn explore_alternative_picks_the_next_chain_candidate() {
        let cfg = cfg_from("[routing]\nmode = \"auto\"\n");
        // Reviewer roles have a non-default alternative: mainline codex → the
        // next chain entry claude-opus.
        assert_eq!(
            explore_alternative(&cfg, "pr-reviewer", &only(&["codex", "claude"])).as_deref(),
            Some("claude-opus")
        );
        // Worker's mainline is claude-sonnet; the next candidate is `default`,
        // a legitimate "routing vs bare [agent]" comparison.
        assert_eq!(
            explore_alternative(&cfg, "worker", &only(&["claude"])).as_deref(),
            Some(DEFAULT_PROFILE)
        );
        // When the mainline is already the chain tail (`default`), there is no
        // alternative → no divert.
        assert_eq!(
            explore_alternative(&cfg, "cleaner", &only(&["claude"])),
            None
        );
    }

    #[test]
    fn explore_is_a_noop_for_explicit_and_manual_roles() {
        // An explicit `[routing.roles]` pick is honored verbatim (ADR 0003) —
        // even a chain member like claude-sonnet is never diverted.
        let cfg = cfg_from(
            r#"
[routing]
mode = "auto"

[routing.roles]
worker = "claude-sonnet"
"#,
        );
        assert_eq!(
            explore_alternative(&cfg, "worker", &only(&["claude"])),
            None
        );
        // A deprecated alias for the same role also counts as explicit.
        let cfg = cfg_from(
            r#"
[routing]
mode = "auto"

[routing.roles]
spec-worker = "claude-sonnet"
"#,
        );
        assert_eq!(
            explore_alternative(&cfg, "worker", &only(&["claude"])),
            None
        );

        // Manual mode has no auto recommendation to canary — even an explicit
        // chain member stays put.
        let cfg = cfg_from(
            r#"
[routing]
mode = "manual"

[routing.roles]
pr-reviewer = "codex"
"#,
        );
        assert_eq!(
            explore_alternative(&cfg, "pr-reviewer", &only(&["codex", "claude"])),
            None
        );

        // Legacy (no `[routing]`) never explores.
        assert_eq!(
            explore_alternative(&Config::default(), "worker", &only(&["claude"])),
            None
        );
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
