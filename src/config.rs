//! `MEGURI_HOME/config.toml`。いまは content の言語だけ。将来の project 単位設定の置き場。
//!
//! `lang` は **自然言語名の文字列**(唯一の消費者は planning プロンプトの LLM なので、
//! `en`/`ja` のようなコードでなく "English" / "日本語" / "Japanese" をそのまま渡す)。

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct Config {
    /// Outcome の statement 等、**エージェント/ユーザーが書く中身**の言語(自然言語名)。
    #[serde(default = "default_lang")]
    pub lang: String,
    /// pane で起動するエージェント CLI(そのまま shell に打ち込む 1 行)。
    #[serde(default = "default_agent")]
    pub agent: String,
}

fn default_lang() -> String {
    "English".to_string()
}

fn default_agent() -> String {
    // planning ではエージェントが proposal.json を書くだけなので権限確認を省く。
    "claude --dangerously-skip-permissions".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Config { lang: default_lang(), agent: default_agent() }
    }
}

/// config.toml を読む。無ければ既定。
pub fn load() -> Result<Config> {
    let path = crate::db::meguri_home()?.join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(s) => toml::from_str(&s).with_context(|| format!("invalid config.toml: {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}
