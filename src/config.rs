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
#
# [notifications]
# macos = true                       # awaiting_human を macOS 通知で知らせる
# webhook_url = "https://example.com/hook"  # JSON POST 先(省略で無効)
# throttle_secs = 60                 # 同一 run の連続通知の最短間隔(秒)
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
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub pr: PrConfig,
    #[serde(default)]
    pub clean: CleanConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
}

/// Settings for the impl-reviewer loop (AI review of implementation PRs).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewConfig {
    /// Kill switch: false silences the impl-reviewer loop entirely
    /// (e.g. when an external review bot already covers implementation PRs).
    #[serde(default = "default_impl_review_enabled")]
    pub impl_enabled: bool,
    /// Max impl-review rounds per PR, counted as head markers in the PR's
    /// comments — the cap that keeps the AI review→fix ping-pong finite.
    /// Once reached, the loop quietly leaves the PR to the humans.
    #[serde(default = "default_impl_max_rounds")]
    pub impl_max_rounds: u32,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            impl_enabled: default_impl_review_enabled(),
            impl_max_rounds: default_impl_max_rounds(),
        }
    }
}

fn default_impl_review_enabled() -> bool {
    true
}
fn default_impl_max_rounds() -> u32 {
    3
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
    /// Args that resume a previous native session; the session id follows
    /// them (`{command} {args} {resume_args} <session-id> <trigger>`).
    /// Defaults to Claude Code's `--resume`.
    #[serde(default = "default_agent_resume_args")]
    pub resume_args: Vec<String>,
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
            resume_args: default_agent_resume_args(),
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

fn default_agent_resume_args() -> Vec<String> {
    vec!["--resume".into()]
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

/// Restart policy for the OS-supervised watch (maps to launchd `KeepAlive`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Start at load only; never resurrect.
    Never,
    /// Restart only after a non-zero exit (default).
    OnFailure,
    /// Restart whenever the process exits.
    Always,
}

impl RestartPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::OnFailure => "on-failure",
            Self::Always => "always",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonConfig {
    #[serde(default = "default_restart_policy")]
    pub restart_policy: RestartPolicy,
    /// Minimum seconds between supervisor restarts (launchd `ThrottleInterval`).
    #[serde(default = "default_throttle_secs")]
    pub throttle_secs: u64,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            restart_policy: default_restart_policy(),
            throttle_secs: default_throttle_secs(),
        }
    }
}

fn default_restart_policy() -> RestartPolicy {
    RestartPolicy::OnFailure
}
fn default_throttle_secs() -> u64 {
    10
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

/// awaiting_human escalations paged to a human (issue #7).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationsConfig {
    /// macOS notification via `osascript` (no-op on other platforms).
    #[serde(default = "default_notifications_macos")]
    pub macos: bool,
    /// URL POSTed a JSON payload (run id / issue / reason / attach command).
    /// None disables the webhook.
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Minimum seconds between notifications for the same run.
    #[serde(default = "default_notifications_throttle")]
    pub throttle_secs: u64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            macos: default_notifications_macos(),
            webhook_url: None,
            throttle_secs: default_notifications_throttle(),
        }
    }
}

fn default_notifications_macos() -> bool {
    true
}
fn default_notifications_throttle() -> u64 {
    60
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
        Self::parse(&raw, path)
    }

    fn parse(raw: &str, path: &Path) -> Result<Self> {
        let cfg: Config =
            toml::from_str(raw).with_context(|| format!("invalid config at {}", path.display()))?;
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

/// Hot reload for a long-lived process (`meguri watch`): re-reads the config
/// file when its content changes and swaps it in atomically — `apply` builds
/// everything derived from the candidate config, and only when it succeeds
/// does the candidate become `current`. A bad edit (unreadable file, invalid
/// TOML, no projects) never kills the process: it is rejected with a warning
/// and the last good config stays in effect.
///
/// Settings bound to the process lifetime are exempt from reload and pinned
/// to their startup values: `mux.kind` / `mux.session` (the daemon already
/// holds panes in that session) and the `[daemon]` section (consumed at
/// start/install time by the OS supervisor). A change to them logs a
/// restart-required warning instead.
pub struct ConfigReloader {
    path: PathBuf,
    /// Raw content of the last load attempt (good or rejected), so each edit
    /// is parsed — and warned about — once, not on every poll. `None` means
    /// the last attempt could not even be read.
    last_seen: Option<String>,
    current: Config,
}

impl ConfigReloader {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path).with_context(|| {
            format!(
                "cannot read config at {} (run `meguri init`)",
                path.display()
            )
        })?;
        let current = Config::parse(&raw, path)?;
        Ok(Self {
            path: path.to_path_buf(),
            last_seen: Some(raw),
            current,
        })
    }

    /// The config currently in effect (the last one that loaded and applied).
    pub fn current(&self) -> &Config {
        &self.current
    }

    /// Reload if the file changed since the last attempt. `apply` receives
    /// (current, candidate) and rebuilds whatever depends on the config; an
    /// `Err` keeps `current` untouched and retries on the next poll (apply
    /// failures are environmental, unlike parse errors which are final for
    /// that content). Returns `None` when nothing changed or the reload was
    /// rejected.
    pub fn poll<T>(&mut self, apply: impl FnOnce(&Config, &Config) -> Result<T>) -> Option<T> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) => {
                if self.last_seen.take().is_some() {
                    tracing::warn!(
                        "cannot read {}: {e} — keeping the last good config",
                        self.path.display()
                    );
                }
                return None;
            }
        };
        if self.last_seen.as_deref() == Some(raw.as_str()) {
            return None;
        }

        let mut next = match Config::parse(&raw, &self.path) {
            Ok(next) => next,
            Err(e) => {
                self.last_seen = Some(raw);
                tracing::warn!("config reload rejected: {e:#} — keeping the last good config");
                return None;
            }
        };
        if next.projects.is_empty() {
            self.last_seen = Some(raw);
            tracing::warn!(
                "config reload rejected: no projects configured — keeping the last good config"
            );
            return None;
        }

        // Pin the process-bound settings so `current` always reflects what is
        // actually in effect.
        if next.mux.kind != self.current.mux.kind || next.mux.session != self.current.mux.session {
            tracing::warn!(
                "mux.kind / mux.session are fixed for the daemon's lifetime — \
                 restart `meguri watch` to apply them"
            );
            next.mux.kind = self.current.mux.kind.clone();
            next.mux.session = self.current.mux.session.clone();
        }
        if next.daemon != self.current.daemon {
            tracing::warn!(
                "[daemon] settings apply at start/install time — \
                 restart (or `meguri daemon install`) to apply them"
            );
            next.daemon = self.current.daemon.clone();
        }

        match apply(&self.current, &next) {
            Ok(applied) => {
                tracing::info!("config reloaded from {}", self.path.display());
                self.last_seen = Some(raw);
                self.current = next;
                Some(applied)
            }
            Err(e) => {
                tracing::warn!(
                    "config reload failed to apply: {e:#} — keeping the last good config"
                );
                None
            }
        }
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
        assert_eq!(back.daemon.restart_policy, RestartPolicy::OnFailure);
        assert_eq!(back.daemon.throttle_secs, 10);
        assert!(back.pr.draft);
        assert_eq!(back.server.port, 8607);
        assert_eq!(back.server.bind, "127.0.0.1");
        assert!(back.notifications.macos);
        assert_eq!(back.notifications.webhook_url, None);
        assert_eq!(back.notifications.throttle_secs, 60);
        assert!(back.review.impl_enabled);
        assert_eq!(back.review.impl_max_rounds, 3);
    }

    #[test]
    fn review_section_overrides_defaults() {
        let raw = r#"
[review]
impl_enabled = false
impl_max_rounds = 1
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.review.impl_enabled);
        assert_eq!(cfg.review.impl_max_rounds, 1);
    }

    #[test]
    fn notifications_defaults_apply_without_section() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.notifications.macos);
        assert_eq!(cfg.notifications.webhook_url, None);
        assert_eq!(cfg.notifications.throttle_secs, 60);
    }

    #[test]
    fn notifications_section_overrides_defaults() {
        let raw = r#"
[notifications]
macos = false
webhook_url = "https://example.com/hook"
throttle_secs = 10
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.notifications.macos);
        assert_eq!(
            cfg.notifications.webhook_url.as_deref(),
            Some("https://example.com/hook")
        );
        assert_eq!(cfg.notifications.throttle_secs, 10);
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
    fn daemon_config_parses_kebab_case_policy() {
        let cfg: Config =
            toml::from_str("[daemon]\nrestart_policy = \"on-failure\"\nthrottle_secs = 30\n")
                .unwrap();
        assert_eq!(cfg.daemon.restart_policy, RestartPolicy::OnFailure);
        assert_eq!(cfg.daemon.throttle_secs, 30);
        let cfg: Config = toml::from_str("[daemon]\nrestart_policy = \"always\"\n").unwrap();
        assert_eq!(cfg.daemon.restart_policy, RestartPolicy::Always);
        assert_eq!(cfg.daemon.throttle_secs, 10);
        let cfg: Config = toml::from_str("[daemon]\nrestart_policy = \"never\"\n").unwrap();
        assert_eq!(cfg.daemon.restart_policy, RestartPolicy::Never);
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
        // resume_args keeps its Claude Code default unless overridden.
        assert_eq!(cfg.agent.resume_args, vec!["--resume"]);
    }

    #[test]
    fn agent_resume_args_can_be_overridden() {
        let raw = r#"
[agent]
resume_args = ["resume", "--session"]
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert_eq!(cfg.agent.resume_args, vec!["resume", "--session"]);
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

    /// Minimal valid config with one project and the given extra lines.
    fn write_config(path: &Path, extra: &str) {
        let raw = format!(
            "{extra}\n[[projects]]\nid = \"demo\"\nrepo_path = \"/tmp/demo\"\nrepo_slug = \"me/demo\"\n"
        );
        std::fs::write(path, raw).unwrap();
    }

    #[test]
    fn reloader_ignores_unchanged_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "language = \"A\"");
        let mut r = ConfigReloader::load(&path).unwrap();

        let mut applied = false;
        let out = r.poll(|_, _| -> Result<()> {
            applied = true;
            Ok(())
        });
        assert!(out.is_none());
        assert!(!applied, "apply must not run when the file is unchanged");
    }

    #[test]
    fn reloader_applies_changed_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "language = \"A\"");
        let mut r = ConfigReloader::load(&path).unwrap();

        write_config(&path, "language = \"B\"");
        let got = r.poll(|prev, next| {
            assert_eq!(prev.language.as_deref(), Some("A"));
            Ok(next.language.clone())
        });
        assert_eq!(got, Some(Some("B".to_string())));
        assert_eq!(r.current().language.as_deref(), Some("B"));
    }

    #[test]
    fn reloader_rejects_invalid_toml_and_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "language = \"A\"");
        let mut r = ConfigReloader::load(&path).unwrap();

        std::fs::write(&path, "language = not valid toml").unwrap();
        let mut applied = false;
        assert!(
            r.poll(|_, _| -> Result<()> {
                applied = true;
                Ok(())
            })
            .is_none()
        );
        assert!(!applied);
        assert_eq!(r.current().language.as_deref(), Some("A"));
        // Same bad content again: still rejected, still on the last good config.
        assert!(r.poll(|_, _| -> Result<()> { Ok(()) }).is_none());

        // Fixing the file resumes reloading.
        write_config(&path, "language = \"C\"");
        assert!(r.poll(|_, _| -> Result<()> { Ok(()) }).is_some());
        assert_eq!(r.current().language.as_deref(), Some("C"));
    }

    #[test]
    fn reloader_rejects_empty_projects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "");
        let mut r = ConfigReloader::load(&path).unwrap();

        std::fs::write(&path, "language = \"B\"\n").unwrap();
        assert!(r.poll(|_, _| -> Result<()> { Ok(()) }).is_none());
        assert!(!r.current().projects.is_empty());
    }

    #[test]
    fn reloader_survives_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "language = \"A\"");
        let mut r = ConfigReloader::load(&path).unwrap();

        std::fs::remove_file(&path).unwrap();
        assert!(r.poll(|_, _| -> Result<()> { Ok(()) }).is_none());
        assert_eq!(r.current().language.as_deref(), Some("A"));

        write_config(&path, "language = \"B\"");
        assert!(r.poll(|_, _| -> Result<()> { Ok(()) }).is_some());
        assert_eq!(r.current().language.as_deref(), Some("B"));
    }

    #[test]
    fn reloader_pins_process_bound_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "");
        let mut r = ConfigReloader::load(&path).unwrap();

        write_config(
            &path,
            "language = \"B\"\n[mux]\nsession = \"other\"\nkind = \"tmux\"\n[daemon]\nthrottle_secs = 99",
        );
        let got = r.poll(|_, next| Ok(next.clone())).unwrap();
        // The reloadable change went through…
        assert_eq!(got.language.as_deref(), Some("B"));
        // …but process-bound settings keep their startup values.
        assert_eq!(got.mux.session, "meguri");
        assert_eq!(got.mux.kind, "auto");
        assert_eq!(got.daemon.throttle_secs, 10);
        assert_eq!(r.current().mux.session, "meguri");
    }

    #[test]
    fn reloader_keeps_current_and_retries_when_apply_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "language = \"A\"");
        let mut r = ConfigReloader::load(&path).unwrap();

        write_config(&path, "language = \"B\"");
        assert!(
            r.poll(|_, _| -> Result<()> { anyhow::bail!("transient") })
                .is_none()
        );
        assert_eq!(r.current().language.as_deref(), Some("A"));

        // Unlike a parse error, an apply failure retries on the next poll.
        assert!(r.poll(|_, _| -> Result<()> { Ok(()) }).is_some());
        assert_eq!(r.current().language.as_deref(), Some("B"));
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
