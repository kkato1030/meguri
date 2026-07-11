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

/// Minimal `config.toml` written by `meguri init`. Loading fills every
/// omitted section/key from the serde defaults, so the template only carries
/// the projects stub plus commented override examples.
pub const INIT_TEMPLATE: &str = r#"# meguri config — override したい項目だけ書けば、残りは既定値が使われます。
# 既定値一覧は README を参照。

[[projects]]
id = "myproj"
repo_path = "/abs/path/to/clone"
repo_slug = "owner/repo"
# default_branch = "main"
# check_command = "cargo test"

# 既定を上書きしたい時だけ、必要なセクション/キーを書く:
# [scheduler]
# max_concurrent_runs = 3
#
# [limits]
# idle_grace_secs = 120
#
# [agent]
# args = ["--permission-mode", "acceptEdits"]  # yolo をやめて確認ダイアログ運用にする例
"#;

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
    pub server: ServerConfig,
    #[serde(default)]
    pub pr: PrConfig,
    #[serde(default)]
    pub clean: CleanConfig,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
}

/// Settings for the cleaner loop (read-only repository sweeps).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanConfig {
    /// Minimum hours between sweeps; a moved head alone does not trigger one.
    #[serde(default = "default_clean_interval_hours")]
    pub interval_hours: u64,
    /// Remote branches whose last commit is older than this many days are
    /// reported as stale (merged branches are reported regardless of age).
    #[serde(default = "default_stale_branch_days")]
    pub stale_branch_days: u64,
    /// False-positive silencer: findings whose file/note (or branch name /
    /// `#N` reference) contains any of these substrings are dropped from the
    /// report at render time.
    #[serde(default)]
    pub ignore: Vec<String>,
}

impl Default for CleanConfig {
    fn default() -> Self {
        Self {
            interval_hours: default_clean_interval_hours(),
            stale_branch_days: default_stale_branch_days(),
            ignore: Vec::new(),
        }
    }
}

fn default_clean_interval_hours() -> u64 {
    24
}
fn default_stale_branch_days() -> u64 {
    30
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
    /// "on-failure" | "always" | "never"
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
    "on-failure".into()
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
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            command: default_agent_command(),
            args: default_agent_args(),
            herdr_agent_hint: None,
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
pub struct ServerConfig {
    /// `meguri serve` listen port.
    #[serde(default = "default_server_port")]
    pub port: u16,
    /// Bind address. Loopback by default — the dashboard has no auth; serve
    /// warns (but proceeds) on anything else.
    #[serde(default = "default_server_bind")]
    pub bind: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            port: default_server_port(),
            bind: default_server_bind(),
        }
    }
}

fn default_server_port() -> u16 {
    8607
}
fn default_server_bind() -> String {
    "127.0.0.1".into()
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
    /// Per-project cleaner settings; overrides the global `[clean]` section
    /// (the ignore list in particular is inherently project-specific).
    #[serde(default)]
    pub clean: Option<CleanConfig>,
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

    /// Effective cleaner settings for a project (project override wins).
    pub fn clean_for<'a>(&'a self, project: &'a ProjectConfig) -> &'a CleanConfig {
        project.clean.as_ref().unwrap_or(&self.clean)
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
        assert_eq!(back.limits.idle_grace_secs, 90);
        assert_eq!(back.scheduler.max_concurrent_runs, 2);
        assert!(back.pr.draft);
        assert_eq!(back.server.port, 8607);
        assert_eq!(back.server.bind, "127.0.0.1");
    }

    #[test]
    fn server_defaults_apply_without_section() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.server.port, 8607);
        assert_eq!(cfg.server.bind, "127.0.0.1");
    }

    #[test]
    fn server_section_overrides_defaults() {
        let cfg: Config = toml::from_str("[server]\nport = 9000\nbind = \"0.0.0.0\"\n").unwrap();
        assert_eq!(cfg.server.port, 9000);
        assert_eq!(cfg.server.bind, "0.0.0.0");
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
    fn init_template_is_minimal_and_loads_with_defaults() {
        // Only the projects stub is active; every other section stays commented.
        let active_tables: Vec<&str> = INIT_TEMPLATE
            .lines()
            .filter(|l| l.trim_start().starts_with('['))
            .collect();
        assert_eq!(active_tables, vec!["[[projects]]"]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, INIT_TEMPLATE).unwrap();
        let cfg = Config::load_from(&path).unwrap();

        let p = cfg.project("myproj").unwrap();
        assert_eq!(p.repo_slug, "owner/repo");
        assert_eq!(p.default_branch, "main");
        assert_eq!(p.check_command, None);

        // Omitted sections/keys fall back to the serde defaults.
        assert_eq!(cfg.language, None);
        assert_eq!(cfg.mux.kind, "auto");
        assert_eq!(cfg.agent.args, vec!["--dangerously-skip-permissions"]);
        assert_eq!(cfg.limits.idle_grace_secs, 90);
        assert_eq!(cfg.scheduler.max_concurrent_runs, 2);
        assert!(cfg.pr.draft);
    }

    #[test]
    fn clean_defaults_apply_without_section() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.clean.interval_hours, 24);
        assert_eq!(cfg.clean.stale_branch_days, 30);
        assert!(cfg.clean.ignore.is_empty());
    }

    #[test]
    fn clean_project_override_wins() {
        let raw = r#"
[clean]
interval_hours = 12

[[projects]]
id = "demo"
repo_path = "/tmp/demo"
repo_slug = "me/demo"

[[projects]]
id = "quiet"
repo_path = "/tmp/quiet"
repo_slug = "me/quiet"

[projects.clean]
interval_hours = 48
ignore = ["docs/legacy"]
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let demo = cfg.project("demo").unwrap();
        assert_eq!(cfg.clean_for(demo).interval_hours, 12);
        assert_eq!(cfg.clean_for(demo).stale_branch_days, 30);

        let quiet = cfg.project("quiet").unwrap();
        assert_eq!(cfg.clean_for(quiet).interval_hours, 48);
        // The override replaces the whole section; omitted keys fall back to
        // the built-in defaults, not the global section.
        assert_eq!(cfg.clean_for(quiet).stale_branch_days, 30);
        assert_eq!(cfg.clean_for(quiet).ignore, vec!["docs/legacy"]);
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
