//! `~/.meguri/config.toml` の読み込み。
//!
//! v0 は「上書きしたい項目だけ書く」方針の最小形: `[[projects]]` と `[agent]`
//! だけがあり、他はすべて既定値。プロジェクトは自分で維持している clone を
//! `repo_path` で指す(meguri は clone を所有しない)。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub projects: Vec<Project>,
    #[serde(default)]
    pub agent: Agent,
    #[serde(default)]
    pub limits: Limits,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub id: String,
    /// meguri が worktree を切る元の clone(必須・絶対パス)。
    pub repo_path: PathBuf,
    #[serde(default = "default_branch")]
    pub default_branch: String,
    /// 設定すると success 申告の独立検証で orchestrator 自身が実行する。
    pub check_command: Option<String>,
}

/// pane で起動するエージェント CLI。既定は claude + yolo:
/// エージェントは隔離された worktree の中で走り、コマンドごとに許可を求めると
/// 自律ループが止まるため。ゲートしたい場合は args を差し替え、pane に attach
/// してダイアログに答える。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    #[serde(default = "default_agent_command")]
    pub command: String,
    #[serde(default = "default_agent_args")]
    pub args: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// エージェント CLI の起動から prompt 投入までの猶予(秒)。
    #[serde(default = "default_spawn_grace")]
    pub spawn_grace_secs: u64,
    /// result.json を待つ上限(秒)。使い切っても pane は殺さない。
    #[serde(default = "default_max_turn_runtime")]
    pub max_turn_runtime_secs: u64,
}

impl Config {
    /// `--project` の解決: 明示が最優先、無指定は「1 件だけ設定済み」のときに
    /// 限りその 1 件(複数あるのに無指定は明示的なエラー)。
    pub fn project(&self, id: Option<&str>) -> Result<&Project> {
        match id {
            Some(id) => self
                .projects
                .iter()
                .find(|p| p.id == id)
                .with_context(|| format!("プロジェクト {id:?} は config.toml にありません")),
            None => match self.projects.as_slice() {
                [] => bail!(
                    "config.toml にプロジェクトがありません({} に [[projects]] を書いてください)",
                    config_path().display()
                ),
                [only] => Ok(only),
                _ => {
                    bail!("複数プロジェクトが設定されています — --project <id> で指定してください")
                }
            },
        }
    }
}

pub fn load() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        // 設定ファイルが無いのは初回の正常系: 既定値だけの Config を返し、
        // プロジェクト未設定のエラーは使う段で出す。
        return Ok(Config::default());
    }
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("{} の読み込み", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("{} のパース", path.display()))
}

pub fn meguri_home() -> PathBuf {
    if let Ok(home) = std::env::var("MEGURI_HOME") {
        return PathBuf::from(home);
    }
    home_dir().join(".meguri")
}

pub fn config_path() -> PathBuf {
    meguri_home().join("config.toml")
}

pub fn worktrees_root() -> PathBuf {
    meguri_home().join("worktrees")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| Path::new(".").into())
}

fn default_branch() -> String {
    "main".into()
}
fn default_agent_command() -> String {
    "claude".into()
}
fn default_agent_args() -> Vec<String> {
    vec!["--dangerously-skip-permissions".into()]
}
fn default_spawn_grace() -> u64 {
    8
}
fn default_max_turn_runtime() -> u64 {
    2700
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            command: default_agent_command(),
            args: default_agent_args(),
        }
    }
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            spawn_grace_secs: default_spawn_grace(),
            max_turn_runtime_secs: default_max_turn_runtime(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_fills_defaults() {
        let cfg: Config =
            toml::from_str("[[projects]]\nid = \"demo\"\nrepo_path = \"/tmp/demo\"\n").unwrap();
        let p = cfg.project(None).unwrap();
        assert_eq!(p.default_branch, "main");
        assert_eq!(cfg.agent.command, "claude");
        assert_eq!(cfg.limits.max_turn_runtime_secs, 2700);
    }

    #[test]
    fn unknown_keys_are_rejected_loudly() {
        // 打ち間違いを黙って無視すると「設定したのに効かない」で溶ける。
        assert!(toml::from_str::<Config>("[agnet]\ncommand = \"x\"\n").is_err());
    }

    #[test]
    fn multiple_projects_require_explicit_id() {
        let cfg: Config = toml::from_str(
            "[[projects]]\nid = \"a\"\nrepo_path = \"/tmp/a\"\n\
             [[projects]]\nid = \"b\"\nrepo_path = \"/tmp/b\"\n",
        )
        .unwrap();
        assert!(cfg.project(None).is_err());
        assert_eq!(cfg.project(Some("b")).unwrap().id, "b");
    }
}
