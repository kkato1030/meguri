//! Agent profile resolution (kernel-pruning-plan Phase 6): the reserved
//! `default` profile (`[agent]`), named profiles (`[agents.profiles.<name>]`
//! plus a tiny builtin set), and a single per-project override
//! (`[[projects]] profile = "<name>"`). Role-based routing is dormant
//! (docs/adr/STATUS.md). The launch-argv helpers the pre-flight prime uses
//! live here too.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};

use crate::config::{AgentProfile, Config, ProjectConfig};

/// Resolve the profile name a project's runs launch under: the project's
/// `profile` override, else `default`. Loud when the named profile does not
/// exist.
pub fn resolve(cfg: &Config, project: &ProjectConfig) -> Result<String> {
    let name = project
        .profile
        .clone()
        .unwrap_or_else(|| DEFAULT_PROFILE.to_string());
    profile_by_name(cfg, &name)
        .with_context(|| format!("project {:?} pins profile {name:?}", project.id))?;
    Ok(name)
}

/// Startup validation: every project override must name a resolvable profile,
/// and no user profile may claim the reserved `default` name.
pub fn validate(cfg: &Config) -> Result<()> {
    if let Some(agents) = &cfg.agents
        && agents.profiles.contains_key(DEFAULT_PROFILE)
    {
        bail!(
            "[agents.profiles.default] is reserved — configure the default \
             profile via the top-level [agent] section"
        );
    }
    for p in &cfg.projects {
        if let Some(name) = &p.profile {
            profile_by_name(cfg, name)
                .with_context(|| format!("project {:?} pins profile {name:?}", p.id))?;
        }
    }
    Ok(())
}

/// The reserved profile name for the historical `[agent]` section. Users
/// steer a role back to it with `<role> = "default"`; it is never detected.
pub const DEFAULT_PROFILE: &str = "default";

/// The no-op prompt handed to the pre-flight prime (issue #235): a headless
/// one-shot whose only intended effect is that the CLI enters the worktree
/// directory once (recording folder trust). The prompt asks for no work; the
/// all-tool deny (D1 [prime 仕様]) is what actually guarantees no tool runs
/// even if a hostile `CLAUDE.md` tries to hijack the turn.
pub const PREFLIGHT_NOOP_PROMPT: &str = "reply ok and make no changes";

/// The lowest `claude` version the pre-flight prime is enabled for (issue
/// #235). The prime's safety rests on a meguri-owned `--settings` deny file
/// plus `--strict-mcp-config`; older CLIs that lack those flags (or the
/// `permissions.deny` schema) cannot be made provably tool-free, so the prime
/// is skipped there and the pane launches as before. The floor is deliberately
/// conservative: `--settings` / `--strict-mcp-config` are 1.x-era flags, so a
/// major of `1` gates out the pre-1.0 line. The exact floor is confirmed by
/// `tests/preflight_injection_test.rs` (issue #235 f1), a real-`claude`
/// all-surface injection test gated behind `MEGURI_TEST_CLAUDE=1`; if the
/// floor ever needs to move, that test is where it gets re-confirmed.
pub const PREFLIGHT_MIN_CLAUDE_VERSION: (u64, u64, u64) = (1, 0, 0);

/// Parse a `major.minor.patch` triple out of a `--version` line, e.g.
/// "claude 1.2.3 (…)" → (1, 2, 3), "v2.0" → (2, 0, 0). Missing components
/// default to 0; a line with no leading version number is `None`. Coarser
/// than a full semver parser on purpose — only the numeric prefix is compared.
pub fn parse_version_triple(version_line: &str) -> Option<(u64, u64, u64)> {
    let start = version_line.find(|c: char| c.is_ascii_digit())?;
    let rest = &version_line[start..];
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let mut parts = rest[..end].split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next().flatten()?;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// Extract the model selector from a profile's launch `args` so the pre-flight
/// prime uses the *same* model as the pane it precedes (issue #235 f2). Handles
/// both the split form (`--model opus` / `-m opus`) and the joined form
/// (`--model=opus` / `-m=opus`); returns the argv fragment to reproduce it, or
/// empty when the profile pins no model (the prime then uses the CLI default,
/// exactly as the pane would). Deliberately reads `args` — not
/// `effective_headless_args`, which drops the model when `headless_args` is
/// unset and would revive the f2 hang on custom profiles.
pub fn model_flag_from_args(args: &[String]) -> Vec<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--model" || a == "-m" {
            if let Some(v) = it.next() {
                return vec![a.clone(), v.clone()];
            }
            return Vec::new();
        }
        if a.starts_with("--model=") || a.starts_with("-m=") {
            return vec![a.clone()];
        }
    }
    Vec::new()
}

/// Whether an argv carries a yolo / skip-permissions flag — the marker of an
/// unsafe pre-flight override (issue #235 f13) or an unsafe posture in general.
pub fn args_carry_yolo(args: &[String]) -> bool {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--dangerously-skip-permissions" || a == "--yolo" {
            return true;
        }
        // Both the joined (`--permission-mode=bypassPermissions`) and the split
        // (`--permission-mode bypassPermissions`) forms of the bypass mode.
        if a == "--permission-mode=bypassPermissions" {
            return true;
        }
        if a == "--permission-mode" && it.next().map(String::as_str) == Some("bypassPermissions") {
            return true;
        }
    }
    false
}

/// Resolve the argv for a profile's launch-time pre-flight prime, or an empty
/// vec meaning "skip the prime" (issue #235 D1/D2).
///
/// An explicit non-empty `preflight` is used verbatim — a host opt-in, warned
/// about at load if it carries yolo ([`warn_unsafe_preflight_overrides`]). An
/// explicit `[]` is the opt-out sentinel (skip). Absent, a `claude` command at
/// or above [`PREFLIGHT_MIN_CLAUDE_VERSION`] gets the safe default (the pane's
/// `--model` if any, `--strict-mcp-config`, `--settings <deny.json>`, `-p
/// <no-op>` — no yolo; folder trust needs none and the deny file makes the turn
/// tool-free). Absent on an older/unknown `claude`, or on any other command,
/// resolves to empty (skip).
pub fn effective_preflight_args(
    profile: &AgentProfile,
    claude_version: Option<(u64, u64, u64)>,
    deny_settings_path: &std::path::Path,
) -> Vec<String> {
    if let Some(explicit) = &profile.preflight {
        return explicit.clone();
    }
    let base = std::path::Path::new(&profile.command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&profile.command);
    if base != "claude" {
        return Vec::new();
    }
    match claude_version {
        Some(v) if v >= PREFLIGHT_MIN_CLAUDE_VERSION => {}
        _ => return Vec::new(),
    }
    let mut argv = model_flag_from_args(&profile.args);
    argv.push("--strict-mcp-config".to_string());
    argv.push("--settings".to_string());
    argv.push(deny_settings_path.to_string_lossy().into_owned());
    argv.push("-p".to_string());
    argv.push(PREFLIGHT_NOOP_PROMPT.to_string());
    argv
}

/// Log a warning for every profile whose explicit `preflight` override carries
/// a yolo flag (issue #235 f13). The default prime is safe; an explicit
/// override is a host opt-in that bypasses the all-tool deny, so a yolo one
/// lets a hostile `CLAUDE.md` drive tools before the pane starts. Host config
/// is inside the trust boundary (ADR 0011), so this warns rather than blocks.
pub fn warn_unsafe_preflight_overrides(cfg: &Config) {
    let check = |name: &str, profile: &AgentProfile| {
        if let Some(pf) = &profile.preflight
            && args_carry_yolo(pf)
        {
            tracing::warn!(
                profile = name,
                "[preflight] override carries a yolo/skip-permissions flag — the pre-flight \
                 prime will run tool-enabled and is injection-unsafe (a hostile CLAUDE.md can \
                 drive tools before the pane starts). This is your explicit opt-in."
            );
        }
    };
    check("default", &cfg.agent);
    if let Some(agents) = &cfg.agents {
        for (name, profile) in &agents.profiles {
            check(name, profile);
        }
    }
}

/// The built-in profiles, resolvable by name with no other config. A user
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
            preflight: None,
            resume_transcript_limit_bytes: AgentProfile::default().resume_transcript_limit_bytes,
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
            preflight: None,
            resume_transcript_limit_bytes: AgentProfile::default().resume_transcript_limit_bytes,
        },
    );
    m.insert(
        "codex".to_string(),
        AgentProfile {
            command: "codex".into(),
            args: vec!["--yolo".into()],
            resume_args: vec!["resume".into()],
            // codex's non-interactive one-shot form is the `exec` subcommand
            // (mirrors `resume_args` also being a bare subcommand).
            herdr_agent_hint: None,
            session_dir: None,
            preflight: None,
            resume_transcript_limit_bytes: AgentProfile::default().resume_transcript_limit_bytes,
        },
    );
    m
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
