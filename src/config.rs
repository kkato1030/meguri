use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Root of all meguri state: `~/.meguri`, or `$MEGURI_HOME` if set.
pub fn meguri_home() -> PathBuf {
    normalize_meguri_home(
        std::env::var("MEGURI_HOME").ok(),
        std::env::current_dir().ok().as_deref(),
        dirs::home_dir().as_deref(),
    )
}

/// Pure core of [`meguri_home`], factored out so its normalization is testable
/// without mutating process env. An absolute `$MEGURI_HOME` passes through; a
/// relative one is joined onto `cwd` (issue #235 f2: `preflight_dir()` /
/// `deny_settings_path()` are contracted to be absolute, but a relative
/// `MEGURI_HOME` — e.g. `state` — used to pass straight through. The daemon
/// then wrote `deny.json` relative to *its own* cwd, while `run_preflight`
/// changes the child's cwd to the worktree before passing the same relative
/// `--settings <path>` argv, so the prime looks for the file under the
/// worktree instead and fails — permanently, since a failure is claim-once and
/// never retried). An unset value falls back to `home/.meguri`.
fn normalize_meguri_home(
    env_value: Option<String>,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> PathBuf {
    if let Some(value) = env_value {
        let path = PathBuf::from(value);
        if path.is_absolute() {
            return path;
        }
        return match cwd {
            Some(cwd) => cwd.join(path),
            None => path,
        };
    }
    home.expect("cannot resolve home directory").join(".meguri")
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

/// Pre-flight state directory: `~/.meguri/preflight`. Holds the meguri-owned
/// deny-all `--settings` file the prime runs under and the per-identity "done"
/// markers (issue #235). Deliberately under [`meguri_home`], NOT inside a
/// worktree cwd, so an ephemeral cwd's teardown never wipes the marker
/// and re-triggers a redundant prime.
pub fn preflight_dir() -> PathBuf {
    meguri_home().join("preflight")
}

/// The config-dir a `claude` launch (pane or prime) actually resolves to: an
/// explicit `$CLAUDE_CONFIG_DIR` (normalized to absolute against the daemon's
/// cwd if it was relative), else the CLI's own `~/.claude` default. Resolving
/// to a single absolute path lets the prime and the pane be handed the exact
/// same `CLAUDE_CONFIG_DIR` (issue #235 f1): tmux/herdr spawn the pane through
/// a long-lived server whose captured environment can differ from the daemon's,
/// so a relative or unset value would let the prime write folder trust to a
/// different dir than the pane reads.
pub fn effective_config_dir() -> PathBuf {
    normalize_config_dir(
        std::env::var("CLAUDE_CONFIG_DIR").ok(),
        std::env::current_dir().ok().as_deref(),
        dirs::home_dir().as_deref(),
    )
}

/// Pure core of [`effective_config_dir`], factored out so its normalization is
/// testable without mutating process env. An absolute `$CLAUDE_CONFIG_DIR`
/// passes through; a relative one is joined onto `cwd`; an unset value falls
/// back to `home/.claude`.
fn normalize_config_dir(
    env_value: Option<String>,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> PathBuf {
    if let Some(dir) = env_value {
        let p = PathBuf::from(dir);
        if p.is_absolute() {
            return p;
        }
        if let Some(cwd) = cwd {
            return cwd.join(p);
        }
        return p;
    }
    home.map(|h| h.to_path_buf())
        .unwrap_or_default()
        .join(".claude")
}

/// Minimal `config.toml` written by `meguri init`. Loading fills every
/// omitted section/key from the serde defaults, so the template only carries
/// the projects stub plus commented override examples.
pub const INIT_TEMPLATE: &str = r#"# meguri config — override したい項目だけ書けば、残りは既定値が使われます。
# 既定値一覧は README を参照。

# プロジェクトは下の例のコメントを外して手書きします。init 直後は 0 件です。
# [[projects]]
# id = "myproj"
# repo_path = "/abs/path/to/clone"  # meguri が worktree を切る元の clone(必須)
# repo_slug = "owner/repo"          # github mode で必須
# default_branch = "main"
# check_command = "cargo test"      # 設定すると success 申告時に独立検証で実行される
# profile = "claude"                # [agents.profiles.*] の名前。省略で default([agent])
# mode = "local"      # ラベル/GitHub を使わず手元で回す(repo_slug は不要、成果物は
#                     # ローカルブランチ)。`meguri add "タスク"` で投入。

# [projects.worktree_setup]                  # worktree 準備のたびに(再利用時も)実行する汎用フック
# commands = ["apm install --frozen"]        # 任意コマンド列
# exclude = [".claude/rules", "AGENTS.md"]   # 生成物を .git/info/exclude に追記(.meguri/ は常に追記される)
# required = false                           # true にすると失敗時に run が失敗扱いになる(既定は warn で続行)
# timeout_secs = 300

# [prompts]                            # preamble: turn プロンプト冒頭に埋め込む恒常規律
# all = "ops/agents/guardrails.md"     # 値は repo 相対パス(絶対パス/`..` は不可)
# worker = "ops/agents/worker.md"
# [projects.prompts]                   # per-project override(キー単位で [prompts] を上書き)
# worker = "ops/agents/worker.md"      # 常時読み込みで足りるなら CLAUDE.md を使い、これは使わない

# 既定を上書きしたい時だけ、必要なセクション/キーを書く:
# [scheduler]
# max_concurrent_runs = 3
# intake_interval_secs = 120           # GitHub ラベル読み取り(intake)の周期。キューの権威は sqlite
#
# [limits]
# idle_grace_secs = 120
#
# [agent]                              # default profile
# args = ["--permission-mode", "acceptEdits"]  # yolo をやめて確認ダイアログ運用にする例
#
# [agents.profiles.claude-opus]        # 追加の名前付き profile(projects.profile で選択)
# command = "claude"
# args = ["--model", "opus", "--dangerously-skip-permissions"]
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
    /// The `default` profile: the CLI launched when a project pins no other
    /// profile. Keeps the historical `[agent]` section shape and semantics.
    #[serde(default)]
    pub agent: AgentProfile,
    /// Named launch profiles (`[agents.profiles.<name>]`), selectable per
    /// project via `projects.profile` — see [`crate::profile`].
    #[serde(default)]
    pub agents: Option<AgentsConfig>,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub pr: PrConfig,
    /// Top-level preamble map (`[prompts]`): key (`worker`, or the shared
    /// `all`) → repo-relative path to a file whose contents are injected into
    /// the turn prompt. Per-project `[projects.prompts]` overrides it per key.
    /// See [`Config::preambles_for`].
    #[serde(default)]
    pub prompts: HashMap<String, String>,
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
    /// Base mux label. Each project's panes live in a per-project workspace
    /// derived from it — `<session>:<project>` (herdr) / `<session>-<project>`
    /// (tmux) — while the bare `<session>` is the cross-project `meguri top`
    /// view. Fixed for the daemon's lifetime (see `ConfigReloader`).
    #[serde(default = "default_session")]
    pub session: String,
    /// Pane lifetime policy: "until-issue-closed" (default — the reaper
    /// reclaims the pane when the issue closes on the forge) | "never"
    /// (kill the pane as soon as its run ends; high-throughput operation).
    /// Any other value is rejected at config load.
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

/// A launch profile: the bundle of "how to start (and resume) one agent CLI".
/// The `default` profile lives in `[agent]`; named ones in
/// `[agents.profiles.<name>]`. Both share this shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
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
    /// Complete argv for the launch-time pre-flight prime (issue #235): a
    /// headless one-shot run in the worktree cwd just before the interactive
    /// pane spawns, so the CLI records folder trust for that path and the real
    /// pane no longer stalls at the first-run trust prompt (meguri never reads
    /// the screen to answer it).
    ///
    /// Resolution (see [`crate::profile::effective_preflight_args`]): a
    /// non-empty value is used verbatim (a complete argv — a host opt-in that
    /// bypasses the safe default and is warned about if it carries yolo); an
    /// explicit empty `[]` disables the prime; absence falls back to a
    /// known-CLI default that keeps the pane's model but adds a meguri-owned
    /// all-tool deny (`--settings` + `--strict-mcp-config`) so the prime turn
    /// executes no tool even against a permissive inherited config. On a
    /// `claude` older than [`crate::profile::PREFLIGHT_MIN_CLAUDE_VERSION`] (or
    /// any non-`claude` command) the default resolves empty — the prime is
    /// skipped and the pane launches as before.
    #[serde(default)]
    pub preflight: Option<Vec<String>>,
    /// Transcript size (bytes) beyond which a saved session is NOT resumed
    /// (issue #245): an oversized transcript is the signature of a session at
    /// (or past) its context limit, where `--resume` only produces API 400s.
    /// The gate clears the session and falls back to a fresh spawn with full
    /// re-injection — prompts are self-contained, so nothing is lost. Per
    /// profile because context windows differ per model. `0` disables the
    /// gate. Default 5 MiB.
    #[serde(default = "default_resume_transcript_limit_bytes")]
    pub resume_transcript_limit_bytes: u64,
}

impl Default for AgentProfile {
    fn default() -> Self {
        Self {
            command: default_agent_command(),
            args: default_agent_args(),
            resume_args: default_agent_resume_args(),
            herdr_agent_hint: None,
            session_dir: None,
            preflight: None,
            resume_transcript_limit_bytes: default_resume_transcript_limit_bytes(),
        }
    }
}

fn default_resume_transcript_limit_bytes() -> u64 {
    5 * 1024 * 1024
}

/// `[agents]`: the named-profile registry.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentsConfig {
    /// `[agents.profiles.<name>]`. A user entry named the same as a builtin
    /// (`claude-opus` / `claude-sonnet` / `codex`) overrides that builtin.
    #[serde(default)]
    pub profiles: HashMap<String, AgentProfile>,
}

fn default_agent_command() -> String {
    "claude".into()
}

fn default_agent_args() -> Vec<String> {
    // Yolo by default; see AgentProfile::args for the rationale and opt-out.
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
    /// Attach a sanitized pane tail to the agent_quiet needs-human escalation
    /// (issue #245). The tail is diagnosis-only — never used to judge turn
    /// success — and always passes `sanitize_pane_tail` (ANSI/control strip,
    /// credential masking, fence-escape-proof code block) before leaving the
    /// local trust boundary. `false` keeps the raw tail in local events only.
    ///
    /// Lives under `[limits]` rather than `[escalation]`: that section is the
    /// profile-escalation chain table whose flattened role map would swallow
    /// (and choke on) a boolean key.
    #[serde(default = "default_escalation_pane_tail")]
    pub escalation_pane_tail: bool,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            idle_grace_secs: default_idle_grace(),
            nudge_limit: default_nudge_limit(),
            max_turn_runtime_secs: default_max_turn_runtime(),
            result_grace_secs: default_result_grace(),
            validate_turns: default_validate_turns(),
            escalation_pane_tail: default_escalation_pane_tail(),
        }
    }
}

fn default_escalation_pane_tail() -> bool {
    true
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
    /// How often the GitHub intake sweep runs (issue listings → task rows,
    /// 権威反転). Deliberately slower than the poll: the queue authority is
    /// sqlite, so the label read is a low-frequency edge signal — the human's
    /// label edits reach meguri within this window.
    #[serde(default = "default_intake_interval")]
    pub intake_interval_secs: u64,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            max_concurrent_runs: default_max_concurrent(),
            intake_interval_secs: default_intake_interval(),
        }
    }
}

fn default_intake_interval() -> u64 {
    120
}

fn default_poll_interval() -> u64 {
    60
}
fn default_max_concurrent() -> u32 {
    2
}
/// How a project coordinates work: through GitHub labels (the default), or
/// entirely locally against a sqlite `tasks` table. `silent` (issue #54
/// Phase 2 — read issues but never write labels/comments) is not implemented
/// yet, so it is deliberately not a variant here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProjectMode {
    /// Current behavior: labels are the queue/claim/escalation.
    #[default]
    Github,
    /// No GitHub at all: `meguri add` queues local tasks; state is local.
    Local,
}

impl ProjectMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Local => "local",
        }
    }
}

/// The shape of a run's deliverable. `patch` (issue #54 Phase 2) is accepted
/// by the config but not yet implemented by the flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Deliver {
    /// Push the branch and open a pull request (github default).
    Pr,
    /// Leave the verified commits on a local branch; no push, no PR.
    Branch,
    /// `git format-patch` into `.meguri/out/` (Phase 2).
    Patch,
}

/// `[projects.worktree_setup]` (agent 指示基盤 2/3, issue #138): a generic
/// post-worktree-preparation hook. meguri stays agnostic to what runs here
/// (ADR 0003) — a project might regenerate agent instructions
/// (`apm install --frozen`, see README), fetch dependencies, or warm a build
/// cache. Commands run with the worktree as `cwd`, in order, every time the
/// worktree is prepared (created, attached, or re-pointed) — not just the
/// first time, since `attach_worktree` / `create_review_worktree` can wipe
/// untracked files on reuse — so write them idempotently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeSetupConfig {
    /// Shell commands (`sh -c`), run in order; a later command does not run
    /// after an earlier one fails.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Extra paths appended to `.git/info/exclude`, alongside the always-on
    /// `.meguri/` — keeps the commands' untracked output out of the agent's
    /// diffs and out of the clean-tree verification.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Failure policy: false (default) logs a warning, emits
    /// `worktree_setup.failed`, and lets the run continue; true escalates a
    /// failing command to a run failure.
    #[serde(default)]
    pub required: bool,
    /// Per-command timeout in seconds; commands may fetch over the network.
    #[serde(default = "default_worktree_setup_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for WorktreeSetupConfig {
    fn default() -> Self {
        Self {
            commands: Vec::new(),
            exclude: Vec::new(),
            required: false,
            timeout_secs: default_worktree_setup_timeout_secs(),
        }
    }
}

fn default_worktree_setup_timeout_secs() -> u64 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub id: String,
    /// Absolute path to the clone meguri operates on.
    pub repo_path: PathBuf,
    /// "owner/repo" on GitHub. Optional: required unless `mode = "local"`.
    #[serde(default)]
    pub repo_slug: Option<String>,
    /// Coordination mode (see [`ProjectMode`]).
    #[serde(default)]
    pub mode: ProjectMode,
    /// Deliverable shape (see [`Deliver`]). Defaults by mode: `pr` for
    /// github, `branch` for local — resolved via [`Config::deliver_for`].
    #[serde(default)]
    pub deliver: Option<Deliver>,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    /// Per-project deliverable language; overrides the top-level `language`.
    #[serde(default)]
    pub language: Option<String>,
    /// Command the orchestrator runs in the worktree to validate agent work.
    #[serde(default)]
    pub check_command: Option<String>,
    /// The agent profile this project's runs launch under (a name from
    /// `[agents.profiles]`, a builtin, or the reserved `default`). One level
    /// of override.
    #[serde(default)]
    pub profile: Option<String>,
    /// Override for the worktree parent directory (default: ~/.meguri/worktrees).
    #[serde(default)]
    pub worktree_root: Option<PathBuf>,
    /// Per-project PR settings; overrides the global `[pr]` section.
    #[serde(default)]
    pub pr: Option<PrConfig>,
    /// Post-worktree-preparation hook (see [`WorktreeSetupConfig`]).
    #[serde(default)]
    pub worktree_setup: WorktreeSetupConfig,
    /// Per-project preamble overrides (`[projects.prompts]`). Same shape as
    /// the top-level `[prompts]`; a per-project entry overrides the top-level
    /// one for the same key. See [`Config::preambles_for`].
    #[serde(default)]
    pub prompts: HashMap<String, String>,
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
        cfg.validate()
            .with_context(|| format!("invalid config at {}", path.display()))?;
        Ok(cfg)
    }

    /// Reject config that would otherwise fail confusingly at run time:
    /// - a `keep_pane` value that used to no-op silently (issue #92).
    /// - a non-local project without a `repo_slug` (nothing to talk to on
    ///   GitHub), and a local project asking to `deliver = "pr"` (no push
    ///   target) (issue #54).
    fn validate(&self) -> Result<()> {
        match self.mux.keep_pane.as_str() {
            "until-issue-closed" | "never" => {}
            other => anyhow::bail!(
                "mux.keep_pane = {other:?} is not supported (use \"until-issue-closed\" or \"never\")"
            ),
        }
        for p in &self.projects {
            // `id` becomes a filesystem path element (worktree paths), so it
            // must be a single safe path component — reject empty, `/`, `\`,
            // `.`, `..`, and any multi-component or absolute value before it
            // can escape the tree.
            validate_project_id(&p.id)?;
            if p.mode != ProjectMode::Local && p.repo_slug.is_none() {
                anyhow::bail!(
                    "project {:?} has mode = {:?} but no repo_slug (required unless mode = \"local\")",
                    p.id,
                    p.mode.as_str()
                );
            }
            if p.mode == ProjectMode::Local && p.deliver == Some(Deliver::Pr) {
                anyhow::bail!(
                    "project {:?} is mode = \"local\" but deliver = \"pr\" (local has no push target)",
                    p.id
                );
            }
        }
        // Prompt-map invariants span top-level and per-project maps — validate
        // them once here.
        self.validate_prompts()?;
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

    /// Effective deliverable shape for a project. An explicit `deliver` wins;
    /// otherwise the default is mode-dependent — `branch` for local (its only
    /// Phase 1 value), `pr` for github. Splitting the default by mode avoids
    /// the "default pr + local forbids pr" trap that would force every local
    /// project to spell out `deliver`.
    pub fn deliver_for(&self, project: &ProjectConfig) -> Deliver {
        project.deliver.unwrap_or(match project.mode {
            ProjectMode::Local => Deliver::Branch,
            ProjectMode::Github => Deliver::Pr,
        })
    }

    /// The preamble paths to inject for a role, in injection order: the shared
    /// `all` entry first, then the role-specific one (`worker` is the only
    /// role key today — see [`KNOWN_PROMPT_KEYS`]). A per-project entry
    /// overrides the top-level one for the same key. Returns `(key, rel_path)`
    /// for whichever of `all`/`role` are configured.
    pub fn preambles_for(&self, project: &ProjectConfig, role: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for key in ["all", role] {
            if let Some(rel) = preamble_in_map(&project.prompts, key)
                .or_else(|| preamble_in_map(&self.prompts, key))
            {
                out.push((key.to_string(), rel));
            }
        }
        out
    }

    /// Preamble config invariants: every key is `all` or a known prompt key
    /// ([`KNOWN_PROMPT_KEYS`]), no key is set twice within one map, and every
    /// path is safely repo-relative.
    fn validate_prompts(&self) -> Result<()> {
        check_prompt_map(&self.prompts, "[prompts]")?;
        for p in &self.projects {
            let label = format!("project {:?} [projects.prompts]", p.id);
            check_prompt_map(&p.prompts, &label)?;
        }
        Ok(())
    }
}

/// Find the preamble path for key `want` in one map.
fn preamble_in_map(map: &HashMap<String, String>, want: &str) -> Option<String> {
    map.iter()
        .find(|(k, _)| canonical_preamble_key(k) == want)
        .map(|(_, v)| v.clone())
}

/// Preamble map keys: the shared `all` plus the worker role.
pub const KNOWN_PROMPT_KEYS: &[&str] = &["worker"];

fn canonical_preamble_key(key: &str) -> &str {
    key
}

/// Validate one preamble map's keys and path values (see
/// [`Config::validate_prompts`]).
fn check_prompt_map(map: &HashMap<String, String>, label: &str) -> Result<()> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (key, rel) in map {
        let canon = canonical_preamble_key(key);
        if canon != "all" && !KNOWN_PROMPT_KEYS.contains(&canon) {
            anyhow::bail!(
                "{label} has unknown role key {key:?} — valid keys: all, {}",
                KNOWN_PROMPT_KEYS.join(", ")
            );
        }
        if !seen.insert(canon.to_string()) {
            anyhow::bail!(
                "{label} sets role {canon:?} more than once (an alias and its \
                 canonical name both map to it) — keep one"
            );
        }
        validate_repo_relative(rel).with_context(|| format!("{label} key {key:?}"))?;
    }
    Ok(())
}

/// Reject a project `id` that is not a single safe path component. The `id`
/// becomes a filesystem path element (the worktree paths), so `../x`, `a/b`,
/// a leading `/`, `.`, `..`, or an empty string must fail loudly at load time
/// rather than silently placing a worktree outside its root. Same "interpret
/// as a path and reject dangerous components" stance as
/// [`validate_repo_relative`].
pub fn validate_project_id(id: &str) -> Result<()> {
    if id.is_empty() {
        anyhow::bail!("project id must not be empty");
    }
    // On Unix `Path::components()` treats `\` as an ordinary character, so `a\b`
    // would pass the single-component check below — yet it is a separator on
    // Windows and in many tools. Reject it explicitly so an id is portable and
    // can never gain a separator on another platform.
    if id.contains('\\') {
        anyhow::bail!("project id {id:?} must not contain `\\`");
    }
    let mut components = Path::new(id).components();
    match (components.next(), components.next()) {
        // Exactly one plain component (e.g. `myproj`) — the only safe shape.
        // The `to_str` equality also rejects a trailing slash (`a/` normalizes
        // to one `Normal("a")` component whose string no longer equals `a/`).
        (Some(std::path::Component::Normal(c)), None) if c.to_str() == Some(id) => Ok(()),
        _ => anyhow::bail!(
            "project id {id:?} must be a single path component \
             (no `/`, `\\`, `.`, `..`, or leading `/`)"
        ),
    }
}

/// Reject a configured preamble path that could escape the repo lexically: an
/// absolute path, or one containing a `..` component. This is the first of two
/// gates; the second, [`resolve_preamble_within`], follows symlinks
/// at read time. Preamble contents are embedded into the agent prompt, so a
/// path outside the tree would leak secrets to the agent.
pub fn validate_repo_relative(rel: &str) -> Result<()> {
    let path = Path::new(rel);
    if path.is_absolute() {
        anyhow::bail!("preamble path {rel:?} must be repo-relative, not absolute");
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!("preamble path {rel:?} must not contain `..`");
    }
    // A trailing slash would make `git ls-tree` list a directory's children
    // instead of the entry itself, letting a directory pass as a regular file
    // in the default-branch read (ADR 0015).
    if rel.ends_with('/') {
        anyhow::bail!("path {rel:?} must not end with `/`");
    }
    Ok(())
}

/// Outcome of resolving a configured preamble path against a root directory.
#[derive(Debug)]
pub enum PreambleResolution {
    /// Resolved inside `root`; carries the file contents.
    Content(String),
    /// The path does not exist (or could not be canonicalized/read).
    Missing,
    /// The real path — reached through a symlink — lies outside `root`.
    Escapes,
}

/// Read a repo-relative preamble path against `root`, following symlinks, and
/// return its contents only if the real file stays within `root` (ADR 0012).
/// The second containment gate behind [`validate_repo_relative`]: a repo-internal
/// symlink pointing outside the tree passes the lexical check but is caught here.
/// Missing paths and containment failures never error — the caller treats both
/// as "skip this preamble, keep going".
pub fn resolve_preamble_within(root: &Path, rel: &str) -> PreambleResolution {
    let canon_root = match std::fs::canonicalize(root) {
        Ok(p) => p,
        Err(_) => return PreambleResolution::Missing,
    };
    let canon_full = match std::fs::canonicalize(root.join(rel)) {
        Ok(p) => p,
        Err(_) => return PreambleResolution::Missing,
    };
    if !canon_full.starts_with(&canon_root) {
        return PreambleResolution::Escapes;
    }
    match std::fs::read_to_string(&canon_full) {
        Ok(s) => PreambleResolution::Content(s),
        Err(_) => PreambleResolution::Missing,
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
    fn effective_config_dir_normalization() {
        // Absolute passes through.
        assert_eq!(
            normalize_config_dir(
                Some("/abs/cfg".into()),
                Some(Path::new("/cwd")),
                Some(Path::new("/home/u"))
            ),
            PathBuf::from("/abs/cfg")
        );
        // Relative is joined onto cwd (so prime and the mux-server-spawned pane
        // resolve the same absolute dir — issue #235 f1).
        assert_eq!(
            normalize_config_dir(
                Some("rel/cfg".into()),
                Some(Path::new("/cwd")),
                Some(Path::new("/home/u"))
            ),
            PathBuf::from("/cwd/rel/cfg")
        );
        // Unset falls back to ~/.claude.
        assert_eq!(
            normalize_config_dir(None, Some(Path::new("/cwd")), Some(Path::new("/home/u"))),
            PathBuf::from("/home/u/.claude")
        );
    }

    #[test]
    fn meguri_home_normalization() {
        // Absolute passes through.
        assert_eq!(
            normalize_meguri_home(
                Some("/abs/state".into()),
                Some(Path::new("/daemon/cwd")),
                Some(Path::new("/home/u"))
            ),
            PathBuf::from("/abs/state")
        );
        // Relative is joined onto cwd, so `preflight_dir()`/`deny_settings_path()`
        // stay absolute even under a relative `MEGURI_HOME` (issue #235 f2) — the
        // prime writes `deny.json` and passes `--settings <path>` from the same
        // resolved root regardless of which cwd the subprocess later runs under.
        assert_eq!(
            normalize_meguri_home(
                Some("state".into()),
                Some(Path::new("/daemon/cwd")),
                Some(Path::new("/home/u"))
            ),
            PathBuf::from("/daemon/cwd/state")
        );
        assert!(
            normalize_meguri_home(
                Some("state".into()),
                Some(Path::new("/daemon/cwd")),
                Some(Path::new("/home/u"))
            )
            .is_absolute()
        );
        // Unset falls back to ~/.meguri.
        assert_eq!(
            normalize_meguri_home(
                None,
                Some(Path::new("/daemon/cwd")),
                Some(Path::new("/home/u"))
            ),
            PathBuf::from("/home/u/.meguri")
        );
    }

    #[test]
    fn preflight_field_roundtrips_and_defaults_none() {
        assert_eq!(AgentProfile::default().preflight, None);
        let cfg: Config =
            toml::from_str("[agent]\ncommand = \"claude\"\npreflight = [\"-p\", \"ok\"]\n")
                .unwrap();
        assert_eq!(
            cfg.agent.preflight,
            Some(vec!["-p".to_string(), "ok".to_string()])
        );
        // An explicit empty array is preserved (the opt-out sentinel).
        let off: Config =
            toml::from_str("[agent]\ncommand = \"claude\"\npreflight = []\n").unwrap();
        assert_eq!(off.agent.preflight, Some(vec![]));
    }

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
    fn unknown_keep_pane_is_rejected_at_load() {
        let path = Path::new("test.toml");
        for value in ["until-issue-closed", "never"] {
            let raw = format!("[mux]\nkeep_pane = \"{value}\"\n");
            assert!(Config::parse(&raw, path).is_ok(), "value: {value}");
        }
        // "on-failure" used to silently behave like the default (issue #92);
        // now it fails loudly instead of no-opping.
        let err = Config::parse("[mux]\nkeep_pane = \"on-failure\"\n", path).unwrap_err();
        assert!(format!("{err:#}").contains("keep_pane"), "{err:#}");
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
        // No table is active — the projects stub is commented too: a fresh
        // `meguri init` config has zero live projects.
        let active_tables: Vec<&str> = INIT_TEMPLATE
            .lines()
            .filter(|l| l.trim_start().starts_with('['))
            .collect();
        assert!(
            active_tables.is_empty(),
            "no live table expected, got {active_tables:?}"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, INIT_TEMPLATE).unwrap();
        let cfg = Config::load_from(&path).unwrap();

        assert!(cfg.projects.is_empty(), "fresh init has no live projects");

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
    fn reloader_rejects_unknown_launch_role() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_config(&path, "");
        let mut r = ConfigReloader::load(&path).unwrap();

        std::fs::write(
            &path,
            "language = \"B\"\n[launch.roles]\nnonsense = \"direct\"\n",
        )
        .unwrap();
        let mut applied = false;
        assert!(
            r.poll(|_, _| -> Result<()> {
                applied = true;
                Ok(())
            })
            .is_none()
        );
        assert!(!applied, "an unknown launch role must reject before apply");
        assert_ne!(r.current().language.as_deref(), Some("B"));
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

    #[test]
    fn mode_defaults_to_github_and_parses_local() {
        let cfg: Config = toml::from_str(
            "[[projects]]\nid = \"g\"\nrepo_path = \"/tmp/g\"\nrepo_slug = \"me/g\"\n",
        )
        .unwrap();
        assert_eq!(cfg.project("g").unwrap().mode, ProjectMode::Github);

        let cfg: Config =
            toml::from_str("[[projects]]\nid = \"l\"\nrepo_path = \"/tmp/l\"\nmode = \"local\"\n")
                .unwrap();
        assert_eq!(cfg.project("l").unwrap().mode, ProjectMode::Local);
    }

    #[test]
    fn deliver_default_is_mode_dependent() {
        // github without an explicit deliver → pr; local → branch.
        let cfg: Config = toml::from_str(
            "[[projects]]\nid = \"g\"\nrepo_path = \"/tmp/g\"\nrepo_slug = \"me/g\"\n\
             [[projects]]\nid = \"l\"\nrepo_path = \"/tmp/l\"\nmode = \"local\"\n",
        )
        .unwrap();
        assert_eq!(cfg.deliver_for(cfg.project("g").unwrap()), Deliver::Pr);
        assert_eq!(cfg.deliver_for(cfg.project("l").unwrap()), Deliver::Branch);
    }

    #[test]
    fn local_project_loads_without_repo_slug() {
        // Acceptance criterion 1: a local project needs no repo_slug.
        let raw = "[[projects]]\nid = \"l\"\nrepo_path = \"/tmp/l\"\nmode = \"local\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.project("l").unwrap().repo_slug, None);
    }

    #[test]
    fn non_local_without_repo_slug_is_rejected() {
        let raw = "[[projects]]\nid = \"g\"\nrepo_path = \"/tmp/g\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("repo_slug"), "{err}");
    }

    #[test]
    fn dangerous_project_ids_are_rejected() {
        // An id that isn't a single safe path component would let a worktree
        // path escape the worktree root.
        for bad in ["../x", "a/b", "a\\b", "/x", ".", "..", "", "a/"] {
            let raw = format!(
                "[[projects]]\nid = {bad:?}\nrepo_path = \"/tmp/g\"\nrepo_slug = \"me/g\"\n"
            );
            let cfg: Config = toml::from_str(&raw).unwrap();
            assert!(cfg.validate().is_err(), "id {bad:?} should be rejected");
        }
        // A normal id passes.
        let raw =
            "[[projects]]\nid = \"my-proj_1\"\nrepo_path = \"/tmp/g\"\nrepo_slug = \"me/g\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn local_with_deliver_pr_is_rejected() {
        // Acceptance criterion 1: local + pr has no push target.
        let raw = "[[projects]]\nid = \"l\"\nrepo_path = \"/tmp/l\"\n\
                   mode = \"local\"\ndeliver = \"pr\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("deliver"), "{err}");
    }

    #[test]
    fn local_with_deliver_branch_is_accepted() {
        let raw = "[[projects]]\nid = \"l\"\nrepo_path = \"/tmp/l\"\n\
                   mode = \"local\"\ndeliver = \"branch\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.deliver_for(cfg.project("l").unwrap()), Deliver::Branch);
    }

    #[test]
    fn worktree_setup_defaults_to_empty_and_optional() {
        let raw =
            "[[projects]]\nid = \"demo\"\nrepo_path = \"/tmp/demo\"\nrepo_slug = \"me/demo\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        let ws = &cfg.project("demo").unwrap().worktree_setup;
        assert!(ws.commands.is_empty());
        assert!(ws.exclude.is_empty());
        assert!(!ws.required);
        assert_eq!(ws.timeout_secs, 300);
    }

    #[test]
    fn worktree_setup_parses_project_table() {
        let raw = r#"
[[projects]]
id = "demo"
repo_path = "/tmp/demo"
repo_slug = "me/demo"

[projects.worktree_setup]
commands = ["apm install --frozen", "apm compile"]
exclude = [".claude/rules", "AGENTS.md"]
required = true
timeout_secs = 60
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let ws = &cfg.project("demo").unwrap().worktree_setup;
        assert_eq!(ws.commands, vec!["apm install --frozen", "apm compile"]);
        assert_eq!(ws.exclude, vec![".claude/rules", "AGENTS.md"]);
        assert!(ws.required);
        assert_eq!(ws.timeout_secs, 60);
    }

    // ---- prompt preambles ----

    const A_PROJECT: &str =
        "[[projects]]\nid = \"p\"\nrepo_path = \"/tmp/p\"\nrepo_slug = \"me/p\"\n";

    #[test]
    fn prompts_parse_and_key_validation() {
        // Known keys and `all` all load and validate.
        let raw = format!("[prompts]\nall = \"a.md\"\nworker = \"w.md\"\n{A_PROJECT}");
        let cfg = Config::parse(&raw, Path::new("t.toml")).unwrap();
        assert_eq!(cfg.prompts.get("all").map(String::as_str), Some("a.md"));

        // An unknown role key is rejected at load.
        let bad = format!("[prompts]\nnonsense = \"x.md\"\n{A_PROJECT}");
        let err = Config::parse(&bad, Path::new("t.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("unknown role key"), "{err:#}");
    }

    #[test]
    fn prompts_absolute_and_parent_paths_rejected() {
        let abs = format!("[prompts]\nworker = \"/etc/passwd\"\n{A_PROJECT}");
        let err = Config::parse(&abs, Path::new("t.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("repo-relative"), "{err:#}");

        let parent = format!("[prompts]\nworker = \"../../secret\"\n{A_PROJECT}");
        let err = Config::parse(&parent, Path::new("t.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("`..`"), "{err:#}");
    }

    #[test]
    fn validate_repo_relative_helper() {
        assert!(validate_repo_relative("ops/agents/worker.md").is_ok());
        assert!(validate_repo_relative("/etc/passwd").is_err());
        assert!(validate_repo_relative("../escape").is_err());
        assert!(validate_repo_relative("a/../../escape").is_err());
        // A trailing slash would make the default-branch read list a
        // directory's children (ADR 0015).
        assert!(validate_repo_relative("ops/").is_err());
    }

    #[test]
    fn preambles_for_composition_and_override() {
        let raw = format!(
            "[prompts]\nall = \"all.md\"\nworker = \"top-worker.md\"\n\
             {A_PROJECT}[projects.prompts]\nworker = \"proj-worker.md\"\n"
        );
        let cfg = Config::parse(&raw, Path::new("t.toml")).unwrap();
        let p = cfg.project("p").unwrap();

        // both `all` and the role, in that order; per-project role wins.
        assert_eq!(
            cfg.preambles_for(p, "worker"),
            vec![
                ("all".to_string(), "all.md".to_string()),
                ("worker".to_string(), "proj-worker.md".to_string()),
            ]
        );
        // a role with no entry falls back to just `all`.
        assert_eq!(
            cfg.preambles_for(p, "fixer"),
            vec![("all".to_string(), "all.md".to_string())]
        );
    }

    #[test]
    fn preambles_for_project_override_wins() {
        let raw = format!(
            "[prompts]\nworker = \"top.md\"\n{A_PROJECT}[projects.prompts]\nworker = \"proj.md\"\n"
        );
        let cfg = Config::parse(&raw, Path::new("t.toml")).unwrap();
        let p = cfg.project("p").unwrap();
        assert_eq!(
            cfg.preambles_for(p, "worker"),
            vec![("worker".to_string(), "proj.md".to_string())]
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_preamble_within_containment() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();

        // (c) a real file inside root → read.
        std::fs::write(root.path().join("ok.md"), "inside").unwrap();
        assert!(matches!(
            resolve_preamble_within(root.path(), "ok.md"),
            PreambleResolution::Content(s) if s == "inside"
        ));

        // (d) a missing path → Missing.
        assert!(matches!(
            resolve_preamble_within(root.path(), "nope.md"),
            PreambleResolution::Missing
        ));

        // (b) a symlink pointing inside root → read.
        std::fs::write(root.path().join("target.md"), "linked-inside").unwrap();
        symlink(
            root.path().join("target.md"),
            root.path().join("in-link.md"),
        )
        .unwrap();
        assert!(matches!(
            resolve_preamble_within(root.path(), "in-link.md"),
            PreambleResolution::Content(s) if s == "linked-inside"
        ));

        // (a) a symlink escaping root (the exfiltration case) → Escapes.
        let secret = outside.path().join("secret.md");
        std::fs::write(&secret, "secret").unwrap();
        symlink(&secret, root.path().join("escape.md")).unwrap();
        assert!(matches!(
            resolve_preamble_within(root.path(), "escape.md"),
            PreambleResolution::Escapes
        ));
    }
}
