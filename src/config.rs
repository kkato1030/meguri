use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Root of all meguri state: `~/.meguri`.
pub fn meguri_home() -> PathBuf {
    if let Ok(home) = std::env::var("MEGURI_HOME") {
        return PathBuf::from(home);
    }
    dirs::home_dir()
        .expect("cannot resolve home directory")
        .join(".meguri")
}

pub fn config_path() -> PathBuf {
    meguri_home().join("config.toml")
}

pub fn db_path() -> PathBuf {
    meguri_home().join("meguri.sqlite")
}

pub fn worktrees_root() -> PathBuf {
    meguri_home().join("worktrees")
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    /// Language for agent-authored deliverables (PR descriptions, summaries,
    /// specs). Free-form, passed verbatim into the prompt (e.g. "日本語",
    /// "Japanese"). None leaves the agent to its default (usually English).
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub mux: MuxConfig,
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub pr: PrConfig,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrConfig {
    /// Open pull requests as drafts (a human promotes them when ready).
    #[serde(default = "default_pr_draft")]
    pub draft: bool,
}

impl Default for PrConfig {
    fn default() -> Self {
        Self {
            draft: default_pr_draft(),
        }
    }
}

fn default_pr_draft() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuxConfig {
    /// "auto" | "herdr" | "tmux"
    #[serde(default = "default_mux_kind")]
    pub kind: String,
    /// mux session name that holds all meguri panes
    #[serde(default = "default_session")]
    pub session: String,
    /// Pane lifetime policy: "until-issue-closed" (default — the reaper
    /// reclaims the pane when the issue closes on the forge) | "never"
    /// (kill the pane as soon as its run ends; high-throughput operation).
    /// Any other value is treated as "until-issue-closed".
    #[serde(default = "default_keep_pane")]
    pub keep_pane: String,
}

impl Default for MuxConfig {
    fn default() -> Self {
        Self {
            kind: default_mux_kind(),
            session: default_session(),
            keep_pane: default_keep_pane(),
        }
    }
}

fn default_mux_kind() -> String {
    "auto".into()
}
fn default_session() -> String {
    "meguri".into()
}
fn default_keep_pane() -> String {
    "until-issue-closed".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Interactive agent CLI launched inside the pane.
    #[serde(default = "default_agent_command")]
    pub command: String,
    /// Extra args placed before the initial prompt argument.
    ///
    /// Defaults to yolo mode (`--dangerously-skip-permissions`): each run
    /// works in an isolated git worktree, and an autonomous loop stalls if the
    /// agent stops to ask permission for every `git`/`cargo` command. Users who
    /// want a per-command gate can set `args = ["--permission-mode",
    /// "acceptEdits"]` and answer dialogs by attaching to the pane.
    #[serde(default = "default_agent_args")]
    pub args: Vec<String>,
    /// herdr agent name hint (HERDR_AGENT) when detection needs help.
    #[serde(default)]
    pub herdr_agent_hint: Option<String>,
    /// Where the agent keeps its native session transcripts (default:
    /// `$CLAUDE_CONFIG_DIR` or `~/.claude`). The reaper reads it to save a
    /// resumable session id before closing a pane.
    #[serde(default)]
    pub session_dir: Option<PathBuf>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            command: default_agent_command(),
            args: default_agent_args(),
            herdr_agent_hint: None,
            session_dir: None,
        }
    }
}

fn default_agent_command() -> String {
    "claude".into()
}

fn default_agent_args() -> Vec<String> {
    // Yolo by default; see AgentConfig::args for the rationale and opt-out.
    vec!["--dangerously-skip-permissions".into()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    /// Seconds of mux-idle without a result file before nudging the agent.
    #[serde(default = "default_idle_grace")]
    pub idle_grace_secs: u64,
    /// Nudges per turn before escalating to awaiting_human.
    #[serde(default = "default_nudge_limit")]
    pub nudge_limit: u32,
    /// Wall-clock budget per turn while the agent is working (secs).
    #[serde(default = "default_max_turn_runtime")]
    pub max_turn_runtime_secs: u64,
    /// Seconds to keep waiting for Working->Idle after the result file appears.
    #[serde(default = "default_result_grace")]
    pub result_grace_secs: u64,
    /// Max validate-fix turns before escalating.
    #[serde(default = "default_validate_turns")]
    pub validate_turns: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            idle_grace_secs: default_idle_grace(),
            nudge_limit: default_nudge_limit(),
            max_turn_runtime_secs: default_max_turn_runtime(),
            result_grace_secs: default_result_grace(),
            validate_turns: default_validate_turns(),
        }
    }
}

fn default_idle_grace() -> u64 {
    90
}
fn default_nudge_limit() -> u32 {
    2
}
fn default_max_turn_runtime() -> u64 {
    45 * 60
}
fn default_result_grace() -> u64 {
    60
}
fn default_validate_turns() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent_runs: u32,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            max_concurrent_runs: default_max_concurrent(),
        }
    }
}

fn default_poll_interval() -> u64 {
    60
}
fn default_max_concurrent() -> u32 {
    2
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub id: String,
    /// Absolute path to the primary clone.
    pub repo_path: PathBuf,
    /// "owner/repo" on GitHub.
    pub repo_slug: String,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    /// Per-project deliverable language; overrides the top-level `language`.
    #[serde(default)]
    pub language: Option<String>,
    /// Command the orchestrator runs in the worktree to validate agent work.
    #[serde(default)]
    pub check_command: Option<String>,
    /// Override for the worktree parent directory (default: ~/.meguri/worktrees).
    #[serde(default)]
    pub worktree_root: Option<PathBuf>,
    /// Per-project PR settings; overrides the global `[pr]` section.
    #[serde(default)]
    pub pr: Option<PrConfig>,
}

fn default_branch() -> String {
    "main".into()
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read config at {} (run `meguri init`)",
                path.display()
            )
        })?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("invalid config at {}", path.display()))?;
        Ok(cfg)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let raw = toml::to_string_pretty(self)?;
        std::fs::write(path, raw)?;
        Ok(())
    }

    pub fn project(&self, id: &str) -> Option<&ProjectConfig> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// Effective PR settings for a project (project override wins).
    pub fn pr_for<'a>(&'a self, project: &'a ProjectConfig) -> &'a PrConfig {
        project.pr.as_ref().unwrap_or(&self.pr)
    }

    /// Effective deliverable language for a project (project override wins).
    pub fn language_for<'a>(&'a self, project: &'a ProjectConfig) -> Option<&'a str> {
        project.language.as_deref().or(self.language.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip() {
        let cfg = Config::default();
        let raw = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&raw).unwrap();
        assert_eq!(back.mux.kind, "auto");
        assert_eq!(back.mux.keep_pane, "until-issue-closed");
        assert_eq!(back.limits.idle_grace_secs, 90);
        assert_eq!(back.scheduler.max_concurrent_runs, 2);
        assert!(back.pr.draft);
    }

    #[test]
    fn default_agent_is_yolo() {
        // Autonomous loops must not stall on per-command permission prompts;
        // the agent runs in an isolated worktree, so yolo is the default.
        assert_eq!(
            Config::default().agent.args,
            vec!["--dangerously-skip-permissions".to_string()]
        );
    }

    #[test]
    fn agent_args_can_be_overridden_to_gated() {
        let raw = r#"
[agent]
command = "claude"
args = ["--permission-mode", "acceptEdits"]
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.agent.args, vec!["--permission-mode", "acceptEdits"]);
    }

    #[test]
    fn pr_draft_defaults_true() {
        assert!(Config::default().pr.draft);
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.pr.draft);
    }

    #[test]
    fn pr_draft_can_be_disabled_globally() {
        let cfg: Config = toml::from_str("[pr]\ndraft = false\n").unwrap();
        assert!(!cfg.pr.draft);
    }

    #[test]
    fn pr_draft_project_override_wins() {
        let raw = r#"
[[projects]]
id = "demo"
repo_path = "/tmp/demo"
repo_slug = "me/demo"

[projects.pr]
draft = false
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.pr.draft, "global default stays true");
        let p = cfg.project("demo").unwrap();
        assert!(!cfg.pr_for(p).draft);
    }

    #[test]
    fn language_defaults_to_none() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.language, None);
    }

    #[test]
    fn language_project_override_wins() {
        let raw = r#"
language = "日本語"

[[projects]]
id = "demo"
repo_path = "/tmp/demo"
repo_slug = "me/demo"

[[projects]]
id = "en"
repo_path = "/tmp/en"
repo_slug = "me/en"
language = "English"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let demo = cfg.project("demo").unwrap();
        assert_eq!(cfg.language_for(demo), Some("日本語"));
        let en = cfg.project("en").unwrap();
        assert_eq!(cfg.language_for(en), Some("English"));
    }

    #[test]
    fn parses_project() {
        let raw = r#"
[[projects]]
id = "demo"
repo_path = "/tmp/demo"
repo_slug = "me/demo"
check_command = "cargo test"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let p = cfg.project("demo").unwrap();
        assert_eq!(p.default_branch, "main");
        assert_eq!(p.check_command.as_deref(), Some("cargo test"));
    }
}
