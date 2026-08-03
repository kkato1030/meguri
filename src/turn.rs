//! 完了コントラクト — orchestrator とエージェントの唯一の契約。
//!
//! orchestrator は worktree の `.meguri/prompt-<turn_id>.md` に指示を書き、
//! エージェントは作業を終えたら `.meguri/result.json` を書く:
//!
//! ```json
//! {"turn_id": "...", "status": "success" | "failure" | "needs_human", "summary": "..."}
//! ```
//!
//! 画面のパースはしない。`turn_id` が一致しない result は古いターンの残骸と
//! して無視する。`.meguri/` は実行時の制御ファイル置き場で、コミットさせない
//! (worktree 作成時に `.git/info/exclude` へ足す — [`crate::gitops`])。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct TurnResult {
    pub turn_id: String,
    pub status: String,
    #[serde(default)]
    pub summary: String,
}

/// prompt を書き出し、そのパスを返す。契約(result.json の書き方)は prompt
/// 自身に埋め込む — エージェント側に事前設定を要求しない。
pub fn write_prompt(
    worktree: &Path,
    turn_id: &str,
    task: &str,
    check_command: Option<&str>,
) -> Result<PathBuf> {
    let dir = worktree.join(".meguri");
    std::fs::create_dir_all(&dir).context("`.meguri/` の作成")?;
    let check_note = match check_command {
        Some(c) => format!(
            "完了前に `{c}` が通ることを確認してください。orchestrator も独立に同じ検証を行います。\n"
        ),
        None => String::new(),
    };
    let body = format!(
        r#"# タスク

{task}

# 完了の作法

- この worktree の中だけで作業し、変更はすべて commit してください(未 commit の変更が残っていると成功として扱われません)。
- {check_note}作業を終えたら、最後に `.meguri/result.json` を次の形式で書いてください:

```json
{{"turn_id": "{turn_id}", "status": "success", "summary": "何をしたかの 1〜3 行の要約(日本語)"}}
```

- 完遂できないと判断したら `status` を `"failure"` に、人間の判断が必要なら `"needs_human"` にして、`summary` に理由を書いてください。
- `.meguri/` 配下のファイルは commit しないでください。
"#
    );
    let path = dir.join(format!("prompt-{turn_id}.md"));
    std::fs::write(&path, body).with_context(|| format!("{} の書き出し", path.display()))?;
    Ok(path)
}

/// result.json を読む。まだ無ければ `None`。壊れた JSON や turn_id 不一致
/// (古いターンの残骸)も `None` — 待ちを続ける。
pub fn read_result(worktree: &Path, turn_id: &str) -> Result<Option<TurnResult>> {
    let path = worktree.join(".meguri").join("result.json");
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("{} の読み込み", path.display())),
    };
    match serde_json::from_str::<TurnResult>(&raw) {
        Ok(result) if result.turn_id == turn_id => Ok(Some(result)),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_roundtrip_and_stale_turn_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let wt = dir.path();
        assert!(read_result(wt, "t-1").unwrap().is_none(), "無ければ None");

        std::fs::create_dir_all(wt.join(".meguri")).unwrap();
        std::fs::write(
            wt.join(".meguri/result.json"),
            r#"{"turn_id": "t-0", "status": "success", "summary": "old"}"#,
        )
        .unwrap();
        assert!(
            read_result(wt, "t-1").unwrap().is_none(),
            "古い turn_id は無視"
        );

        std::fs::write(
            wt.join(".meguri/result.json"),
            r#"{"turn_id": "t-1", "status": "needs_human", "summary": "判断が要る"}"#,
        )
        .unwrap();
        let r = read_result(wt, "t-1").unwrap().unwrap();
        assert_eq!(r.status, "needs_human");
    }

    #[test]
    fn prompt_carries_task_and_contract() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_prompt(dir.path(), "t-9", "READMEを直す", Some("cargo test")).unwrap();
        let body = std::fs::read_to_string(path).unwrap();
        assert!(body.contains("READMEを直す"));
        assert!(body.contains(r#""turn_id": "t-9""#));
        assert!(body.contains("cargo test"));
    }
}
