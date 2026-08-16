//! v0.2 execution の実装プロンプト(完了契約、§9)。
//!
//! spawn 済みの Work(worktree を持つ)に対して、エージェントへ渡す指示を組み立てる。
//! planning の proposal.json と同じ「耐久チャネルで構造化結果を受け取る・画面は読まない」
//! 型(§8): エージェントは worktree で実装 → commit → `.meguri/result.json` を書く。

use std::path::Path;

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
