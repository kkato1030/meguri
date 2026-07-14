use std::collections::HashMap;
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
# mode = "local"      # ラベル/GitHub を使わず手元で回す(repo_slug は不要、成果物はローカルブランチ)。
                      # `meguri add "タスク"` で投入。詳細は README を参照。

# [projects.worktree_setup]                  # worktree 準備のたびに(再利用時も)実行する汎用フック
# commands = ["apm install --frozen"]        # 例: agent 指示ファイルの再生成。apm 専用ではなく任意コマンド列
# exclude = [".claude/rules", "AGENTS.md"]   # 生成物を .git/info/exclude に追記(.meguri/ は常に追記される)
# required = false                           # true にすると失敗時に run が失敗扱いになる(既定は warn で続行)
# timeout_secs = 300

# [[workspaces]]                       # 関連 project の静的グルーピング(cross-repo 分解のスコープ + 表示単位)
# id = "shop"                          # decompose の起票範囲・cross-repo blocker の解決範囲・ps/top のまとめ方にだけ効く
# projects = ["shop-api", "shop-web"]  # 実行系(worktree/pane/branch)には一切現れない。詳細は README / ADR 0009

# [prompts]                            # ロール別 preamble: turn プロンプト冒頭に埋め込む恒常規律(issue #149)
# all = "ops/agents/guardrails.md"     # 全ロール共通。値は repo 相対パス(絶対パス/`..` は不可)
# worker = "ops/agents/worker.md"      # キーは routing のロール名(worker/planner/fixer/self-reviewer/pr-reviewer/cleaner)
# [projects.prompts]                   # per-project override(キー単位で [prompts] を上書き)。ADR 0012
# planner = "ops/agents/planner.md"    # 常時読み込みで足りるなら CLAUDE.md を使い、これは使わない(過剰採用を避ける)

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
    /// The `default` profile: the CLI launched when no routing steers a role
    /// elsewhere. Keeps the historical `[agent]` section shape and semantics.
    #[serde(default)]
    pub agent: AgentProfile,
    /// Named launch profiles (`[agents.profiles.<name>]`). Inert until a
    /// `[routing]` section references them — see [`crate::routing`].
    #[serde(default)]
    pub agents: Option<AgentsConfig>,
    /// Role→profile routing (`[routing]`). Absent = legacy behavior: every
    /// loop runs the `default` profile, no detection.
    #[serde(default)]
    pub routing: Option<RoutingConfig>,
    /// Role→launch-mode overrides (`[launch]`, issue #169). Always active
    /// (no legacy/off state) — a role with no entry here still resolves
    /// through the built-in recommendation table.
    #[serde(default)]
    pub launch: LaunchConfig,
    /// Outcome-based routing drift thresholds (`[drift]`). Deliberately a
    /// top-level section, NOT nested under `[routing]`: `[routing]`'s mere
    /// presence switches role routing on (see [`routing`]), so a
    /// `[routing.drift]` table would silently activate routing for a user who
    /// only wanted to tune drift. Drift detection is independent of routing
    /// being active — legacy runs all use `default`, so `(role, default)`
    /// regressions are still caught.
    #[serde(default)]
    pub drift: DriftConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub daemon: DaemonConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub pr: PrConfig,
    #[serde(default)]
    pub clean: CleanConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub reconcile: ReconcileConfig,
    /// Top-level role→preamble map (`[prompts]`, issue #149): role name (or
    /// the shared `all` key) → repo-relative path to a file whose contents are
    /// injected into the turn prompt. Per-project `[projects.prompts]` overrides
    /// it per canonical key. See [`Config::preambles_for`] and ADR 0012.
    #[serde(default)]
    pub prompts: HashMap<String, String>,
    #[serde(default)]
    pub projects: Vec<ProjectConfig>,
    /// Static groupings of related projects (issue #154). Purely declarative —
    /// no runtime state, never touches run/turn — and used for exactly three
    /// things: the decompose issue-filing scope, the cross-repo blocker
    /// resolution scope, and `ps`/`top` display grouping. Opt-in: a config
    /// without `[[workspaces]]` behaves exactly as before. See ADR 0009.
    #[serde(default)]
    pub workspaces: Vec<WorkspaceConfig>,
}

/// Settings for the internal self-review phase (ADR 0006/0008): the review→fix
/// loop that runs before the PR opens, now symmetric across plan and impl
/// (ADR 0008). The old `impl_enabled` / `impl_max_rounds` keys (the forge-based
/// impl-reviewer loop, ADR 0004) are still accepted as serde aliases so
/// existing configs keep working.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewConfig {
    /// Kill switch: false skips the internal self-review phase entirely
    /// (e.g. when an external review bot already covers PRs).
    #[serde(default = "default_self_review_enabled", alias = "impl_enabled")]
    pub enabled: bool,
    /// Max self-review rounds per run — the cap that keeps the internal
    /// review→fix loop finite. Once reached, the PR is published as-is (the
    /// human merge gate is the backstop, ADR 0006).
    #[serde(default = "default_self_review_max_rounds", alias = "impl_max_rounds")]
    pub max_rounds: u32,
    /// The review lenses the self-review turn applies each round (ADR 0008):
    /// one review turn considers every configured perspective. Defaults to
    /// `correctness / tests / simplicity / security`; add or drop to taste.
    #[serde(default = "default_review_lenses")]
    pub lenses: Vec<String>,
    /// External GitHub guard review, enabled per kind (ADR 0008 §1/§3).
    #[serde(default)]
    pub guard: GuardConfig,
}

impl Default for ReviewConfig {
    fn default() -> Self {
        Self {
            enabled: default_self_review_enabled(),
            max_rounds: default_self_review_max_rounds(),
            lenses: default_review_lenses(),
            guard: GuardConfig::default(),
        }
    }
}

fn default_self_review_enabled() -> bool {
    true
}
fn default_self_review_max_rounds() -> u32 {
    3
}
fn default_review_lenses() -> Vec<String> {
    ["correctness", "tests", "simplicity", "security"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// `[review.guard]` — the optional external GitHub guard review, toggled
/// independently for the plan (spec/ADR) and impl kinds (ADR 0008). Plan guard
/// defaults on (it is today's mandatory `spec_reviewer`), impl guard defaults
/// off (opt-in; external-bot compatible). Its output is a `meguri/guard-review`
/// commit status + a folded PR-body `<details>` — never inline threads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuardConfig {
    /// Guard the plan (spec/ADR) PR — the reviewed-spec gate (default on).
    #[serde(default = "default_true")]
    pub plan: bool,
    /// Guard the implementation PR (default off).
    #[serde(default, rename = "impl")]
    pub impl_enabled: bool,
}

impl Default for GuardConfig {
    fn default() -> Self {
        Self {
            plan: default_true(),
            impl_enabled: false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Settings for the reconcile loop (issue #142): detecting that a once-shipped
/// issue's body was edited and treating it as a re-attention signal.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ReconcileConfig {
    /// Kill switch: false restores the pre-#142 behavior — a succeeded run
    /// suppresses the issue permanently regardless of later body edits (half A
    /// and half B both go inert).
    #[serde(default = "default_reconcile_body_edits")]
    pub body_edits: bool,
    /// Whether the poll sweep (half B) posts the "本文が更新されました" signal
    /// comment on a changed-body issue. False emits the `issue.body_changed`
    /// event only (no forge write).
    #[serde(default = "default_reconcile_signal_comment")]
    pub signal_comment: bool,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        Self {
            body_edits: default_reconcile_body_edits(),
            signal_comment: default_reconcile_signal_comment(),
        }
    }
}

fn default_reconcile_body_edits() -> bool {
    true
}
fn default_reconcile_signal_comment() -> bool {
    true
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
    /// GitHub-native auto-merge (auto-merge 1/3, issue #41).
    #[serde(default)]
    pub auto_merge: AutoMergeConfig,
}

impl Default for PrConfig {
    fn default() -> Self {
        Self {
            draft: default_pr_draft(),
            auto_merge: AutoMergeConfig::default(),
        }
    }
}

fn default_pr_draft() -> bool {
    true
}

/// How a PR opts into auto-merge: `label` requires the `meguri:automerge`
/// label (on the issue or the PR), `all` arms every eligible meguri PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoMergeOptIn {
    Label,
    All,
}

/// How auto-merge finalizes an eligible PR (ADR 0003 / 0009). `native` arms
/// GitHub-native auto-merge and lets GitHub (branch protection + required
/// checks) decide. `orchestrator` is the fallback for repos where native
/// auto-merge can't be enabled (private + Free): meguri merges the PR itself
/// once GitHub reports it MERGEABLE, accepting its own pre-PR verification
/// (`check_command` + self-review) as the only gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoMergeMode {
    Native,
    Orchestrator,
}

/// `[pr.auto_merge]` — opt-in GitHub-native auto-merge. meguri never decides
/// "safe to merge"; it arms auto-merge on eligible PRs and GitHub (branch
/// protection + required checks) decides (ADR 0003).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoMergeConfig {
    /// Master switch; off by default.
    #[serde(default = "default_auto_merge_enabled")]
    pub enabled: bool,
    /// How eligible PRs are finalized: arm GitHub-native auto-merge (`native`,
    /// the default) or merge them directly (`orchestrator`, the private+Free
    /// fallback).
    #[serde(default = "default_auto_merge_mode")]
    pub mode: AutoMergeMode,
    /// Merge strategy to arm with (no fallback if the repo forbids it).
    #[serde(default = "default_merge_strategy")]
    pub strategy: crate::forge::MergeStrategy,
    /// Refuse to arm unless the base has required-checks branch protection.
    #[serde(default = "default_require_branch_protection")]
    pub require_branch_protection: bool,
    /// Which PRs are eligible (label opt-in vs all meguri PRs).
    #[serde(default = "default_auto_merge_opt_in")]
    pub opt_in: AutoMergeOptIn,
}

impl Default for AutoMergeConfig {
    fn default() -> Self {
        Self {
            enabled: default_auto_merge_enabled(),
            mode: default_auto_merge_mode(),
            strategy: default_merge_strategy(),
            require_branch_protection: default_require_branch_protection(),
            opt_in: default_auto_merge_opt_in(),
        }
    }
}

fn default_auto_merge_enabled() -> bool {
    false
}
fn default_auto_merge_mode() -> AutoMergeMode {
    AutoMergeMode::Native
}
fn default_merge_strategy() -> crate::forge::MergeStrategy {
    crate::forge::MergeStrategy::Squash
}
fn default_require_branch_protection() -> bool {
    true
}
fn default_auto_merge_opt_in() -> AutoMergeOptIn {
    AutoMergeOptIn::Label
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
    /// Extra args that make the launch non-interactive, for `direct` launch
    /// mode (issue #169): the full command line is `{command} {args}
    /// {direct_args} [{resume_args} <session-id>] <trigger>`, run as a plain
    /// subprocess instead of inside a mux pane. Defaults to Claude Code's
    /// `-p` (headless one-shot).
    #[serde(default = "default_agent_direct_args")]
    pub direct_args: Vec<String>,
    /// herdr agent name hint (HERDR_AGENT) when detection needs help.
    #[serde(default)]
    pub herdr_agent_hint: Option<String>,
    /// Where the agent keeps its native session transcripts (default:
    /// `$CLAUDE_CONFIG_DIR` or `~/.claude`). The reaper reads it to save a
    /// resumable session id before closing a pane.
    #[serde(default)]
    pub session_dir: Option<PathBuf>,
}

impl Default for AgentProfile {
    fn default() -> Self {
        Self {
            command: default_agent_command(),
            args: default_agent_args(),
            resume_args: default_agent_resume_args(),
            direct_args: default_agent_direct_args(),
            herdr_agent_hint: None,
            session_dir: None,
        }
    }
}

/// `[agents]`: the named-profile registry. Its own section so `[routing]` can
/// stay a sibling of `[agents]` rather than nesting under it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentsConfig {
    /// `[agents.profiles.<name>]`. A user entry named the same as a builtin
    /// (`claude-opus` / `claude-sonnet` / `codex`) overrides that builtin.
    #[serde(default)]
    pub profiles: HashMap<String, AgentProfile>,
}

/// How `[routing]` steers a loop's role to a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RoutingMode {
    /// Roles absent from `[routing.roles]` resolve through the built-in
    /// recommendation table, filtered by CLI detection.
    #[default]
    Auto,
    /// Roles absent from `[routing.roles]` resolve to `default`; the
    /// recommendation table is off.
    Manual,
}

/// How a role's turns are launched (issue #169, ADR 0012): `pane` keeps the
/// historical live mux pane (a human can attach; the turn engine nudges a
/// quiet agent); `direct` spawns the agent CLI as a plain subprocess for one
/// turn and reads its exit + the result file — no pane, no attach, no
/// nudging. Orthogonal to `[routing]`'s profile axis: this only decides
/// *how* the chosen profile is launched.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    Pane,
    Direct,
}

impl LaunchMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pane => "pane",
            Self::Direct => "direct",
        }
    }
}

/// `[launch]`: role→launch-mode resolution (issue #169). Unlike `[routing]`,
/// there is no legacy/off state — a role with no explicit entry always
/// resolves through the built-in recommendation table
/// ([`crate::launch::recommended_mode`]); an explicit `[launch.roles]` entry
/// always wins over it. See `crate::launch`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LaunchConfig {
    #[serde(default)]
    pub roles: HashMap<String, LaunchMode>,
}

/// `[routing]`: role→profile resolution. Present = routing is active (auto or
/// manual); absent = legacy (every role runs `default`, no detection).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RoutingConfig {
    #[serde(default)]
    pub mode: RoutingMode,
    /// Explicit per-role overrides. Keys are the 6 routing roles (`planner`,
    /// `worker`, `fixer`, `self-reviewer`, `pr-reviewer`, `cleaner`) — a
    /// "kind of work" grouping, independent from the finer-grained internal
    /// loop kinds (`runs.loop_kind`); see `crate::routing::KNOWN_ROLES`.
    /// Values are profile names. An explicit entry always beats auto.
    #[serde(default)]
    pub roles: HashMap<String, String>,
}

/// `[drift]`: outcome-based routing drift thresholds (routing 2/3, issue #65).
/// The scheduler compares the most recent `window` runs of each
/// `(role, profile)` against the preceding `window` and flags a regression
/// when the success rate drops by `success_rate_drop_pt` points OR the mean
/// turn count rises by `turns_increase_pct` percent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftConfig {
    /// Success-rate drop (in percentage points, 0–100) that trips a warning.
    #[serde(default = "default_success_rate_drop_pt")]
    pub success_rate_drop_pt: f64,
    /// Mean-turn-count increase (in percent) that trips a warning.
    #[serde(default = "default_turns_increase_pct")]
    pub turns_increase_pct: f64,
    /// Runs per comparison window (recent vs. preceding).
    #[serde(default = "default_drift_window")]
    pub window: usize,
}

impl Default for DriftConfig {
    fn default() -> Self {
        Self {
            success_rate_drop_pt: default_success_rate_drop_pt(),
            turns_increase_pct: default_turns_increase_pct(),
            window: default_drift_window(),
        }
    }
}

fn default_success_rate_drop_pt() -> f64 {
    20.0
}
fn default_turns_increase_pct() -> f64 {
    50.0
}
fn default_drift_window() -> usize {
    20
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

fn default_agent_direct_args() -> Vec<String> {
    vec!["-p".into()]
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

/// How a plan-first issue is delivered (ADR 0008). Deliberately a separate
/// key from [`ProjectMode`] (github/local): the two are orthogonal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlanDelivery {
    /// Two PRs: the spec/ADR PR is reviewed and merged on its own, then the
    /// issue flips `speccing → ready` and the worker implements in a separate
    /// PR. The spec PR uses a non-closing `Refs #N` reference (default).
    #[default]
    Separate,
    /// One PR: the spec-worker takes over the spec PR's branch and stacks the
    /// implementation on it (the #98 morph shape); spec and impl merge once.
    Combined,
}

impl PlanDelivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Separate => "separate",
            Self::Combined => "combined",
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

/// Which loop a fired schedule targets. `ready` enqueues worker work
/// (`meguri:ready` / task kind `work`); `plan` enqueues planner work
/// (`meguri:plan`) and is github-only (local mode has no planner yet, so the
/// task would never be consumed — rejected by [`Config::validate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleKind {
    #[default]
    Ready,
    Plan,
}

impl ScheduleKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Plan => "plan",
        }
    }
}

/// One `[[projects.schedules]]` entry (issue #146): a cron definition that
/// periodically enqueues an issue (github mode) or local task (local mode).
/// Firing only puts one item on the queue — the existing worker/planner loops
/// consume it (ADR 0009). The last-fired time is *not* here; it lives in
/// sqlite (`schedule_state`) so a hot-reload edit to the definition never
/// loses it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfig {
    /// Unique within the project; the sqlite state key and the body marker id.
    pub name: String,
    /// Standard 5-field cron expression (minute hour day-of-month month
    /// day-of-week), interpreted as UTC. Parsed by [`crate::cron`].
    pub cron: String,
    /// Worker-bound (`ready`, default) or planner-bound (`plan`, github-only).
    #[serde(default)]
    pub kind: ScheduleKind,
    /// Title template. The only variable is `{{date}}` (the fire date,
    /// `YYYY-MM-DD` UTC).
    pub title: String,
    /// Repo-relative path to a file whose contents become the body. Mutually
    /// exclusive with `body`; exactly one is required.
    #[serde(default)]
    pub body_file: Option<String>,
    /// Inline body. Mutually exclusive with `body_file`.
    #[serde(default)]
    pub body: Option<String>,
    /// When false (default), skip firing if the schedule's last-created
    /// issue/task is still open; true fires every occurrence.
    #[serde(default)]
    pub allow_overlap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub id: String,
    /// Absolute path to the primary clone.
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
    /// How plan-first issues are delivered (see [`PlanDelivery`], ADR 0008);
    /// defaults to `separate` (two PRs).
    #[serde(default)]
    pub plan_delivery: PlanDelivery,
    /// Per-project self-review / guard settings; overrides the global
    /// `[review]` section wholesale (like `pr` / `clean`).
    #[serde(default)]
    pub review: Option<ReviewConfig>,
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
    /// Post-worktree-preparation hook (see [`WorktreeSetupConfig`]).
    #[serde(default)]
    pub worktree_setup: WorktreeSetupConfig,
    /// Cron schedules that periodically enqueue issues/tasks (issue #146).
    #[serde(default)]
    pub schedules: Vec<ScheduleConfig>,
    /// Per-project role→preamble overrides (`[projects.prompts]`, issue #149).
    /// Same shape as the top-level `[prompts]`; a per-project entry overrides
    /// the top-level one for the same canonical role key. See
    /// [`Config::preambles_for`] and ADR 0012.
    #[serde(default)]
    pub prompts: HashMap<String, String>,
}

fn default_branch() -> String {
    "main".into()
}

/// `[[workspaces]]` — a static grouping of related projects (issue #154).
///
/// ```toml
/// [[workspaces]]
/// id = "shop"
/// projects = ["shop-api", "shop-web", "shop-infra"]
/// ```
///
/// A workspace never appears in the execution path (worktree / pane / branch /
/// verification are unchanged) and carries no runtime state. It only bounds
/// where a decomposition may file cross-repo child issues, which sibling repos
/// discovery is willing to resolve blockers in, and how `ps` / `top` group
/// their rows. See ADR 0009.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub id: String,
    /// Member project ids. Each must reference a defined `[[projects]]` entry,
    /// and a project may belong to at most one workspace (both enforced by
    /// [`Config::validate`]).
    #[serde(default)]
    pub projects: Vec<String>,
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
            // orchestrator auto-merge assumes no branch protection (ADR 0009):
            // it is the fallback for repos where server-side gates don't exist.
            // Pairing it with `require_branch_protection = true` is a
            // contradiction — reject it so the operator explicitly opts out and
            // acknowledges meguri's own verification is the only gate.
            let am = &self.pr_for(p).auto_merge;
            if am.mode == AutoMergeMode::Orchestrator && am.require_branch_protection {
                anyhow::bail!(
                    "project {:?} has auto_merge.mode = \"orchestrator\" but \
                     require_branch_protection = true — orchestrator mode has no \
                     server-side gate, so set require_branch_protection = false to \
                     acknowledge meguri's own verification is the only gate",
                    p.id
                );
            }
            self.validate_schedules(p)?;
        }
        self.validate_prompts()?;
        Ok(())
    }

    /// Reject schedule definitions that would break `watch` startup / hot
    /// reload or silently never fire: a bad cron expression, a duplicate
    /// `name`, both/neither of `body`/`body_file`, or a local-mode `plan`
    /// schedule (no local planner — the task would never be consumed).
    fn validate_schedules(&self, p: &ProjectConfig) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for s in &p.schedules {
            if !seen.insert(s.name.as_str()) {
                anyhow::bail!(
                    "project {:?} has duplicate schedule name {:?}",
                    p.id,
                    s.name
                );
            }
            if let Err(e) = crate::cron::Cron::parse(&s.cron) {
                anyhow::bail!(
                    "project {:?} schedule {:?} has invalid cron {:?}: {e}",
                    p.id,
                    s.name,
                    s.cron
                );
            }
            match (&s.body, &s.body_file) {
                (Some(_), Some(_)) => anyhow::bail!(
                    "project {:?} schedule {:?} sets both `body` and `body_file` (mutually exclusive)",
                    p.id,
                    s.name
                ),
                (None, None) => anyhow::bail!(
                    "project {:?} schedule {:?} sets neither `body` nor `body_file` (one is required)",
                    p.id,
                    s.name
                ),
                _ => {}
            }
            if p.mode == ProjectMode::Local && s.kind == ScheduleKind::Plan {
                anyhow::bail!(
                    "project {:?} is mode = \"local\" but schedule {:?} has kind = \"plan\" \
                     (local mode has no planner, so the task would never be consumed)",
                    p.id,
                    s.name
                );
            }
        }
        self.validate_workspaces()?;
        Ok(())
    }

    /// Workspace invariants (issue #154): every referenced project is defined,
    /// no project belongs to two workspaces, and workspace ids are unique and
    /// non-empty. Hard-fail like the other structural checks so a typo surfaces
    /// at load time (and `meguri doctor`) rather than as a silent no-op scope.
    fn validate_workspaces(&self) -> Result<()> {
        let mut seen_ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut owner: HashMap<&str, &str> = HashMap::new();
        for ws in &self.workspaces {
            if !seen_ids.insert(ws.id.as_str()) {
                anyhow::bail!("workspace id {:?} is defined more than once", ws.id);
            }
            if ws.projects.is_empty() {
                anyhow::bail!("workspace {:?} has no projects", ws.id);
            }
            for pid in &ws.projects {
                if self.project(pid).is_none() {
                    anyhow::bail!(
                        "workspace {:?} references undefined project {:?}",
                        ws.id,
                        pid
                    );
                }
                if let Some(other) = owner.insert(pid.as_str(), ws.id.as_str()) {
                    anyhow::bail!(
                        "project {:?} belongs to both workspace {:?} and {:?} \
                         (a project may join at most one workspace)",
                        pid,
                        other,
                        ws.id
                    );
                }
            }
        }
        Ok(())
    }

    pub fn project(&self, id: &str) -> Option<&ProjectConfig> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// The workspace with this id, if any.
    pub fn workspace(&self, id: &str) -> Option<&WorkspaceConfig> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    /// The workspace a project belongs to, if any. `None` for a project that
    /// joined no workspace (the opt-out default).
    pub fn workspace_of(&self, project_id: &str) -> Option<&WorkspaceConfig> {
        self.workspaces
            .iter()
            .find(|w| w.projects.iter().any(|p| p == project_id))
    }

    /// The other projects sharing `project_id`'s workspace (self excluded),
    /// resolved to their [`ProjectConfig`]. Empty when the project joined no
    /// workspace. Drives both the decompose issue-filing scope and the
    /// cross-repo blocker resolution scope (issue #154).
    pub fn workspace_siblings(&self, project_id: &str) -> Vec<&ProjectConfig> {
        let Some(ws) = self.workspace_of(project_id) else {
            return Vec::new();
        };
        ws.projects
            .iter()
            .filter(|p| p.as_str() != project_id)
            .filter_map(|p| self.project(p))
            .collect()
    }

    /// Effective PR settings for a project (project override wins).
    pub fn pr_for<'a>(&'a self, project: &'a ProjectConfig) -> &'a PrConfig {
        project.pr.as_ref().unwrap_or(&self.pr)
    }

    /// Effective self-review / guard settings for a project (project override
    /// wins wholesale, like `pr_for`).
    pub fn review_for<'a>(&'a self, project: &'a ProjectConfig) -> &'a ReviewConfig {
        project.review.as_ref().unwrap_or(&self.review)
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

    /// Effective cleaner settings for a project (project override wins).
    pub fn clean_for<'a>(&'a self, project: &'a ProjectConfig) -> &'a CleanConfig {
        project.clean.as_ref().unwrap_or(&self.clean)
    }

    /// The preamble paths to inject for a role, in injection order: the shared
    /// `all` entry first, then the role-specific one (issue #149, ADR 0012).
    /// `role` must be canonical (a `KNOWN_ROLES` value); keys in the maps are
    /// matched after canonicalization, so a deprecated alias such as
    /// `spec-reviewer` still resolves for the `pr-reviewer` turn. A per-project
    /// entry overrides the top-level one for the same key. Returns
    /// `(key, rel_path)` for whichever of `all`/`role` are configured.
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

    /// Every preamble path that could be injected for `project`, as
    /// `(canonical_key, rel_path)`, with per-project entries overriding
    /// top-level ones by canonical key. Used by `meguri doctor` to check that
    /// each configured path actually resolves inside the project's clone.
    pub fn effective_prompts(&self, project: &ProjectConfig) -> Vec<(String, String)> {
        let mut merged: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for (k, v) in self.prompts.iter().chain(project.prompts.iter()) {
            merged.insert(canonical_preamble_key(k).to_string(), v.clone());
        }
        merged.into_iter().collect()
    }

    /// Preamble config invariants (issue #149): every key is `all` or a known
    /// routing role (aliases allowed), no alias+canonical pair collides on the
    /// same role within one map, and every path is safely repo-relative.
    fn validate_prompts(&self) -> Result<()> {
        check_prompt_map(&self.prompts, "[prompts]")?;
        for p in &self.projects {
            let label = format!("project {:?} [projects.prompts]", p.id);
            check_prompt_map(&p.prompts, &label)?;
        }
        Ok(())
    }
}

/// Find the preamble path for canonical key `want` in one map, matching each
/// entry's key by its canonical form (`all` matches literally).
fn preamble_in_map(map: &HashMap<String, String>, want: &str) -> Option<String> {
    map.iter()
        .find(|(k, _)| canonical_preamble_key(k) == want)
        .map(|(_, v)| v.clone())
}

/// Canonicalize a preamble map key: `all` stays literal, everything else goes
/// through the routing role aliases (`spec-reviewer` → `pr-reviewer`, …).
fn canonical_preamble_key(key: &str) -> &str {
    if key == "all" {
        "all"
    } else {
        crate::routing::canonical_role(key)
    }
}

/// Validate one preamble map's keys and path values (see
/// [`Config::validate_prompts`]).
fn check_prompt_map(map: &HashMap<String, String>, label: &str) -> Result<()> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (key, rel) in map {
        let canon = canonical_preamble_key(key);
        if canon != "all" && !crate::routing::KNOWN_ROLES.contains(&canon) {
            anyhow::bail!(
                "{label} has unknown role key {key:?} — valid keys: all, {}",
                crate::routing::KNOWN_ROLES.join(", ")
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

/// Reject a configured preamble path that could escape the repo lexically: an
/// absolute path, or one containing a `..` component. This is the first of two
/// gates (ADR 0012); the second, [`resolve_preamble_within`], follows symlinks
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
        // `[launch.roles]` typos are a loud startup error (`crate::launch::
        // validate`, issue #169) — a hot reload must reject the same way
        // instead of silently applying an ignored override.
        if let Err(e) = crate::launch::validate(&next) {
            self.last_seen = Some(raw);
            tracing::warn!("config reload rejected: {e:#} — keeping the last good config");
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
        assert!(back.notifications.macos);
        assert_eq!(back.notifications.webhook_url, None);
        assert_eq!(back.notifications.throttle_secs, 60);
        assert!(back.review.enabled);
        assert_eq!(back.review.max_rounds, 3);
    }

    #[test]
    fn review_section_overrides_defaults() {
        let raw = r#"
[review]
enabled = false
max_rounds = 1
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.review.enabled);
        assert_eq!(cfg.review.max_rounds, 1);
    }

    #[test]
    fn review_section_accepts_deprecated_impl_keys() {
        // The ADR-0004 key names still load (serde aliases) so existing
        // configs keep working after the ADR-0006 rename.
        let raw = r#"
[review]
impl_enabled = false
impl_max_rounds = 5
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.review.enabled);
        assert_eq!(cfg.review.max_rounds, 5);
    }

    #[test]
    fn reconcile_defaults_are_on_and_overridable() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.reconcile.body_edits);
        assert!(cfg.reconcile.signal_comment);

        let raw = r#"
[reconcile]
body_edits = false
signal_comment = false
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(!cfg.reconcile.body_edits);
        assert!(!cfg.reconcile.signal_comment);
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
    fn auto_merge_defaults_are_conservative() {
        let cfg: Config = toml::from_str("").unwrap();
        let am = &cfg.pr.auto_merge;
        assert!(!am.enabled);
        assert_eq!(am.mode, AutoMergeMode::Native);
        assert_eq!(am.strategy, crate::forge::MergeStrategy::Squash);
        assert!(am.require_branch_protection);
        assert_eq!(am.opt_in, AutoMergeOptIn::Label);
    }

    #[test]
    fn auto_merge_parses_overrides() {
        let raw = r#"
[pr.auto_merge]
enabled = true
mode = "orchestrator"
strategy = "rebase"
require_branch_protection = false
opt_in = "all"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let am = &cfg.pr.auto_merge;
        assert!(am.enabled);
        assert_eq!(am.mode, AutoMergeMode::Orchestrator);
        assert_eq!(am.strategy, crate::forge::MergeStrategy::Rebase);
        assert!(!am.require_branch_protection);
        assert_eq!(am.opt_in, AutoMergeOptIn::All);
    }

    #[test]
    fn orchestrator_mode_rejects_required_branch_protection() {
        // orchestrator + the default require_branch_protection = true is a
        // contradiction and must fail validation (ADR 0009).
        let raw = r#"
[[projects]]
id = "demo"
repo_path = "/tmp/demo"
repo_slug = "me/demo"

[projects.pr.auto_merge]
enabled = true
mode = "orchestrator"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("orchestrator"), "{err}");
        assert!(err.contains("require_branch_protection"), "{err}");
    }

    #[test]
    fn orchestrator_mode_with_protection_off_validates() {
        let raw = r#"
[[projects]]
id = "demo"
repo_path = "/tmp/demo"
repo_slug = "me/demo"

[projects.pr.auto_merge]
enabled = true
mode = "orchestrator"
require_branch_protection = false
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        cfg.validate().unwrap();
        let p = cfg.project("demo").unwrap();
        assert_eq!(cfg.pr_for(p).auto_merge.mode, AutoMergeMode::Orchestrator);
    }

    #[test]
    fn auto_merge_rejects_unknown_strategy_at_load() {
        let err =
            toml::from_str::<Config>("[pr.auto_merge]\nstrategy = \"fast-forward\"\n").unwrap_err();
        assert!(err.to_string().contains("strategy"), "{err}");
    }

    #[test]
    fn auto_merge_project_override_wins_whole_section() {
        // pr_for takes the project's [pr] section wholesale; a project that
        // sets only draft gets the default auto_merge, not the global one.
        let raw = r#"
[pr.auto_merge]
enabled = true

[[projects]]
id = "demo"
repo_path = "/tmp/demo"
repo_slug = "me/demo"

[projects.pr]
draft = false
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.pr.auto_merge.enabled, "global stays enabled");
        let p = cfg.project("demo").unwrap();
        assert!(
            !cfg.pr_for(p).auto_merge.enabled,
            "project [pr] wins wholesale, so auto_merge falls back to default (disabled)"
        );
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
        assert_eq!(p.repo_slug.as_deref(), Some("owner/repo"));
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
    fn agents_and_routing_default_to_none() {
        // Back-compat: an empty config (and any config without the new
        // sections) leaves both `agents` and `routing` absent — the legacy
        // "everything runs [agent]" path.
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.agents.is_none());
        assert!(cfg.routing.is_none());
    }

    #[test]
    fn drift_defaults_and_does_not_activate_routing() {
        // Defaults hold with no `[drift]` section.
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.drift.success_rate_drop_pt, 20.0);
        assert_eq!(cfg.drift.turns_increase_pct, 50.0);
        assert_eq!(cfg.drift.window, 20);

        // A `[drift]` section tunes the thresholds but is top-level: it must
        // NOT imply `[routing]` (which would silently switch role routing on).
        let cfg: Config = toml::from_str(
            r#"
[drift]
success_rate_drop_pt = 10.0
turns_increase_pct = 25.0
window = 50
"#,
        )
        .unwrap();
        assert_eq!(cfg.drift.success_rate_drop_pt, 10.0);
        assert_eq!(cfg.drift.turns_increase_pct, 25.0);
        assert_eq!(cfg.drift.window, 50);
        assert!(
            cfg.routing.is_none(),
            "[drift] must stay legacy for routing"
        );
    }

    #[test]
    fn parses_profiles_and_routing() {
        let raw = r#"
[agents.profiles.claude-opus]
command = "claude"
args = ["--dangerously-skip-permissions", "--model", "opus"]
resume_args = ["--resume"]

[agents.profiles.codex]
command = "codex"
args = ["--yolo"]
resume_args = ["resume"]

[routing]
mode = "auto"

[routing.roles]
reviewer = "codex"
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        let profiles = &cfg.agents.as_ref().unwrap().profiles;
        assert_eq!(profiles["claude-opus"].command, "claude");
        assert_eq!(
            profiles["claude-opus"].args,
            vec!["--dangerously-skip-permissions", "--model", "opus"]
        );
        assert_eq!(profiles["codex"].resume_args, vec!["resume"]);
        let routing = cfg.routing.as_ref().unwrap();
        assert_eq!(routing.mode, RoutingMode::Auto);
        assert_eq!(routing.roles["reviewer"], "codex");
    }

    #[test]
    fn routing_mode_defaults_to_auto_when_section_present() {
        let cfg: Config = toml::from_str("[routing]\n").unwrap();
        assert_eq!(cfg.routing.as_ref().unwrap().mode, RoutingMode::Auto);
        let cfg: Config = toml::from_str("[routing]\nmode = \"manual\"\n").unwrap();
        assert_eq!(cfg.routing.as_ref().unwrap().mode, RoutingMode::Manual);
    }

    #[test]
    fn profiles_without_routing_still_parse() {
        // `[agents.profiles]` alone is legal; it stays inert (no routing).
        let raw = r#"
[agents.profiles.codex]
command = "codex"
args = ["--yolo"]
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        assert!(cfg.agents.is_some());
        assert!(cfg.routing.is_none());
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

    #[test]
    fn schedules_parse_as_array_of_project_tables() {
        let raw = r#"
[[projects]]
id = "demo"
repo_path = "/tmp/demo"
repo_slug = "me/demo"

[[projects.schedules]]
name = "daily-tidy"
cron = "0 9 * * *"
title = "Daily tidy {{date}}"
body = "do the thing"

[[projects.schedules]]
name = "weekly-plan"
cron = "0 9 * * 1"
kind = "plan"
title = "Weekly plan"
body_file = "ops/plan.md"
allow_overlap = true
"#;
        let cfg: Config = toml::from_str(raw).unwrap();
        cfg.validate().unwrap();
        let schedules = &cfg.project("demo").unwrap().schedules;
        assert_eq!(schedules.len(), 2);
        assert_eq!(schedules[0].name, "daily-tidy");
        assert_eq!(schedules[0].kind, ScheduleKind::Ready); // default
        assert_eq!(schedules[0].body.as_deref(), Some("do the thing"));
        assert_eq!(schedules[1].kind, ScheduleKind::Plan);
        assert_eq!(schedules[1].body_file.as_deref(), Some("ops/plan.md"));
        assert!(schedules[1].allow_overlap);
    }

    #[test]
    fn schedule_with_invalid_cron_is_rejected() {
        let raw = "[[projects]]\nid = \"d\"\nrepo_path = \"/tmp/d\"\nrepo_slug = \"me/d\"\n\
                   [[projects.schedules]]\nname = \"s\"\ncron = \"* * *\"\n\
                   title = \"t\"\nbody = \"b\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("cron"), "{err}");
    }

    #[test]
    fn schedule_with_duplicate_name_is_rejected() {
        let raw = "[[projects]]\nid = \"d\"\nrepo_path = \"/tmp/d\"\nrepo_slug = \"me/d\"\n\
                   [[projects.schedules]]\nname = \"dup\"\ncron = \"0 9 * * *\"\ntitle = \"t\"\nbody = \"b\"\n\
                   [[projects.schedules]]\nname = \"dup\"\ncron = \"0 9 * * *\"\ntitle = \"t\"\nbody = \"b\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate schedule name"), "{err}");
    }

    #[test]
    fn schedule_with_both_body_and_body_file_is_rejected() {
        let raw = "[[projects]]\nid = \"d\"\nrepo_path = \"/tmp/d\"\nrepo_slug = \"me/d\"\n\
                   [[projects.schedules]]\nname = \"s\"\ncron = \"0 9 * * *\"\ntitle = \"t\"\n\
                   body = \"b\"\nbody_file = \"f.md\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("mutually exclusive"), "{err}");
    }

    #[test]
    fn schedule_with_neither_body_nor_body_file_is_rejected() {
        let raw = "[[projects]]\nid = \"d\"\nrepo_path = \"/tmp/d\"\nrepo_slug = \"me/d\"\n\
                   [[projects.schedules]]\nname = \"s\"\ncron = \"0 9 * * *\"\ntitle = \"t\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("neither"), "{err}");
    }

    #[test]
    fn local_mode_plan_schedule_is_rejected() {
        // local mode has no planner; a plan schedule would enqueue a task that
        // never gets consumed (spec §doctor / ADR 0009).
        let raw = "[[projects]]\nid = \"l\"\nrepo_path = \"/tmp/l\"\nmode = \"local\"\n\
                   [[projects.schedules]]\nname = \"s\"\ncron = \"0 9 * * *\"\n\
                   kind = \"plan\"\ntitle = \"t\"\nbody = \"b\"\n";
        let cfg: Config = toml::from_str(raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("no planner"), "{err}");
    }

    #[test]
    fn local_mode_ready_schedule_is_accepted() {
        let raw = "[[projects]]\nid = \"l\"\nrepo_path = \"/tmp/l\"\nmode = \"local\"\n\
                   [[projects.schedules]]\nname = \"s\"\ncron = \"0 9 * * *\"\ntitle = \"t\"\nbody = \"b\"\n";
        let cfg = Config::parse(raw, Path::new("cfg")).unwrap();
        assert_eq!(cfg.project("l").unwrap().schedules.len(), 1);
    }

    /// Minimal valid config carrying the given projects plus the extra lines.
    fn config_with_projects(ids: &[&str], extra: &str) -> String {
        let mut raw = String::new();
        for id in ids {
            raw.push_str(&format!(
                "[[projects]]\nid = \"{id}\"\nrepo_path = \"/tmp/{id}\"\nrepo_slug = \"me/{id}\"\n\n"
            ));
        }
        raw.push_str(extra);
        raw
    }

    #[test]
    fn workspaces_default_to_empty() {
        // Opt-in: a config without [[workspaces]] has none, and behavior is
        // unchanged (acceptance criterion 5).
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.workspaces.is_empty());
        assert!(cfg.workspace_of("anything").is_none());
        assert!(cfg.workspace_siblings("anything").is_empty());
    }

    #[test]
    fn parses_workspace_and_resolves_membership() {
        let raw = config_with_projects(
            &["shop-api", "shop-web", "shop-infra", "loner"],
            "[[workspaces]]\nid = \"shop\"\nprojects = [\"shop-api\", \"shop-web\", \"shop-infra\"]\n",
        );
        let cfg: Config = toml::from_str(&raw).unwrap();
        cfg.validate().unwrap();

        assert_eq!(cfg.workspace("shop").unwrap().projects.len(), 3);
        assert_eq!(
            cfg.workspace_of("shop-web").map(|w| w.id.as_str()),
            Some("shop")
        );
        assert!(cfg.workspace_of("loner").is_none());

        let siblings: Vec<&str> = cfg
            .workspace_siblings("shop-api")
            .iter()
            .map(|p| p.id.as_str())
            .collect();
        assert_eq!(siblings, vec!["shop-web", "shop-infra"]);
        assert!(cfg.workspace_siblings("loner").is_empty());
    }

    #[test]
    fn workspace_referencing_undefined_project_is_rejected() {
        let raw = config_with_projects(
            &["shop-api"],
            "[[workspaces]]\nid = \"shop\"\nprojects = [\"shop-api\", \"ghost\"]\n",
        );
        let cfg: Config = toml::from_str(&raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("undefined project") && err.contains("ghost"),
            "{err}"
        );
    }

    #[test]
    fn project_in_two_workspaces_is_rejected() {
        let raw = config_with_projects(
            &["a", "b"],
            "[[workspaces]]\nid = \"w1\"\nprojects = [\"a\", \"b\"]\n\
             [[workspaces]]\nid = \"w2\"\nprojects = [\"b\"]\n",
        );
        let cfg: Config = toml::from_str(&raw).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("at most one workspace"), "{err}");
    }

    #[test]
    fn duplicate_workspace_id_and_empty_projects_are_rejected() {
        let dup = config_with_projects(
            &["a"],
            "[[workspaces]]\nid = \"w\"\nprojects = [\"a\"]\n\
             [[workspaces]]\nid = \"w\"\nprojects = [\"a\"]\n",
        );
        let cfg: Config = toml::from_str(&dup).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("more than once"), "{err}");

        let empty = config_with_projects(&["a"], "[[workspaces]]\nid = \"w\"\nprojects = []\n");
        let cfg: Config = toml::from_str(&empty).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("no projects"), "{err}");
    }

    const A_PROJECT: &str =
        "[[projects]]\nid = \"p\"\nrepo_path = \"/tmp/p\"\nrepo_slug = \"me/p\"\n";

    #[test]
    fn prompts_parse_and_key_validation() {
        // Known roles, deprecated aliases, and `all` all load and validate.
        let raw = format!(
            "[prompts]\nall = \"a.md\"\nworker = \"w.md\"\nspec-reviewer = \"r.md\"\n{A_PROJECT}"
        );
        let cfg = Config::parse(&raw, Path::new("t.toml")).unwrap();
        assert_eq!(cfg.prompts.get("all").map(String::as_str), Some("a.md"));

        // An unknown role key is rejected at load.
        let bad = format!("[prompts]\nnonsense = \"x.md\"\n{A_PROJECT}");
        let err = Config::parse(&bad, Path::new("t.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("unknown role key"), "{err:#}");
    }

    #[test]
    fn prompts_alias_and_canonical_collision_rejected() {
        let bad =
            format!("[prompts]\npr-reviewer = \"a.md\"\nspec-reviewer = \"b.md\"\n{A_PROJECT}");
        let err = Config::parse(&bad, Path::new("t.toml")).unwrap_err();
        assert!(format!("{err:#}").contains("more than once"), "{err:#}");
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
            cfg.preambles_for(p, "planner"),
            vec![("all".to_string(), "all.md".to_string())]
        );
    }

    #[test]
    fn preambles_for_resolves_aliases_and_top_project_override() {
        // Old name at top-level, canonical name per-project: project wins, and
        // the alias still resolves for the canonical role.
        let raw = format!(
            "[prompts]\nspec-reviewer = \"top-review.md\"\n\
             {A_PROJECT}[projects.prompts]\npr-reviewer = \"proj-review.md\"\n"
        );
        let cfg = Config::parse(&raw, Path::new("t.toml")).unwrap();
        let p = cfg.project("p").unwrap();
        assert_eq!(
            cfg.preambles_for(p, "pr-reviewer"),
            vec![("pr-reviewer".to_string(), "proj-review.md".to_string())]
        );

        // Old name only, at top-level, resolves for the canonical role turn.
        let raw2 = format!("[prompts]\nspec-reviewer = \"review.md\"\n{A_PROJECT}");
        let cfg2 = Config::parse(&raw2, Path::new("t.toml")).unwrap();
        let p2 = cfg2.project("p").unwrap();
        assert_eq!(
            cfg2.preambles_for(p2, "pr-reviewer"),
            vec![("pr-reviewer".to_string(), "review.md".to_string())]
        );

        // A role with nothing configured returns nothing.
        assert!(cfg2.preambles_for(p2, "cleaner").is_empty());
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
