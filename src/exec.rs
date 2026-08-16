//! v0.2 execution の実装プロンプト(完了契約、§9)。
//!
//! spawn 済みの Work(worktree を持つ)に対して、エージェントへ渡す指示を組み立てる。
//! planning の proposal.json と同じ「耐久チャネルで構造化結果を受け取る・画面は読まない」
//! 型(§8): エージェントは worktree で実装 → commit → `.meguri/result.json` を書く。

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::store::{Outcome, Verify};

/// 実装エージェントに渡す完了契約プロンプト。`result_path` に result.json を書かせる。
pub fn impl_prompt(o: &Outcome, worktree: &Path, result_path: &Path, lang: &str) -> String {
    let mut s = String::new();
    s.push_str("You are implementing one Work in an isolated git worktree (your current directory).\n\n");

    s.push_str(&format!("# Goal (Outcome o{})\n{}\n", o.id, o.statement));
    if !o.description.trim().is_empty() {
        s.push_str(&format!("\n{}\n", o.description));
    }
    s.push('\n');

    s.push_str("# Definition of done\n");
    match &o.verify {
        Verify::Command(cmd) => s.push_str(&format!("- The check passes: `{cmd}`\n")),
        Verify::Human => s.push_str("- A human will review it; make a clean, focused change.\n"),
        Verify::Rollup => s.push_str("- (milestone)\n"),
    }
    s.push('\n');

    s.push_str("# Steps\n");
    s.push_str(&format!("- Work only in this worktree: {}\n", worktree.display()));
    s.push_str("- Implement the change, focused on the goal above.\n");
    s.push_str(&format!("- Write commit messages and any prose in {lang}.\n"));
    if let Verify::Command(cmd) = &o.verify {
        s.push_str(&format!("- Run the check and make it pass: `{cmd}`\n"));
    }
    s.push_str("- Commit your work (git add -A && git commit). Do NOT commit the .meguri/ directory.\n\n");

    s.push_str("# When done, write the result file\n");
    s.push_str(&format!("Write JSON to: {}\n", result_path.display()));
    s.push_str("{ \"status\": \"success\" | \"failure\" | \"needs_human\", \"summary\": \"one line\" }\n");
    s.push_str("meguri watches this file to judge completion — it does not read your screen.\n");
    s
}

/// エージェントが `.meguri/result.json` に書く完了報告(§8 の耐久シグナル)。
#[derive(Debug, Deserialize)]
pub struct WorkResult {
    /// "success" | "failure" | "needs_human"。
    pub status: String,
    #[serde(default)]
    pub summary: String,
}

/// result.json を読む。まだ無ければ `Ok(None)`(= 未完了)。壊れていれば `Err`。
///
/// 書き込み途中の部分 JSON では `Err` になり得る。呼び側(o16 のポーリング)は
/// それを「まだ完成していない」として扱い、待ち続ける(§8: 耐久シグナルのみを見る)。
pub fn read_result(path: &Path) -> Result<Option<WorkResult>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    let r: WorkResult = serde_json::from_str(&text)
        .with_context(|| format!("{} is not valid result JSON", path.display()))?;
    Ok(Some(r))
}

/// 報告 status から Work の次状態を決める。meguri 側の独立検証(o17-o20)はまだ無いので、
/// `success` は「エージェント報告済み・meguri 未検証」= `reported` に置く(検証で verified 化するのは後の増分)。
pub fn state_for(status: &str) -> &'static str {
    match status {
        "failure" => "failed",
        "needs_human" => "needs_human",
        // "success" と未知の status はどちらも「報告あり・未検証」に倒す。
        _ => "reported",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_missing_then_present_result() {
        let dir = std::env::temp_dir().join(format!("meguri-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("result.json");
        let _ = std::fs::remove_file(&p);

        assert!(read_result(&p).unwrap().is_none()); // まだ無い

        std::fs::write(&p, r#"{"status":"success","summary":"did it"}"#).unwrap();
        let r = read_result(&p).unwrap().unwrap();
        assert_eq!(r.status, "success");
        assert_eq!(r.summary, "did it");

        // 壊れた JSON は Err(部分書き込みと区別しない)。
        std::fs::write(&p, "{ partial").unwrap();
        assert!(read_result(&p).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn maps_status_to_state() {
        assert_eq!(state_for("success"), "reported");
        assert_eq!(state_for("failure"), "failed");
        assert_eq!(state_for("needs_human"), "needs_human");
        assert_eq!(state_for("weird"), "reported");
    }
}
