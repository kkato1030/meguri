---
description: meguri v2 の全体像・完了コントラクト・書き直しの規律
---

- meguri v2 はフルスクラッチ書き直し中である。目的は理解の再取得: **1 増分 =
  1 PR = 人間が全行読めるサイズ**を守り、増分は直列に積む。順序と各増分が参照
  する失敗カタログは `docs/design/v2-roadmap.md` が正。
- 中核概念(v1 から持ち越した不変条件): 完了コントラクト(worktree の
  `.meguri/prompt-<turn_id>.md` → エージェントが `.meguri/result.json` を書く。
  画面は読まない)、trust-but-verify(clean tree / base より ahead /
  `check_command` の 3 点独立検証)、生きた tmux pane(いつでも attach 可能、
  ブロック ≠ 失敗)。
- `.meguri/` 配下は実行時の制御ファイルであり、リポジトリにコミットしない。
- v1 の設計判断・失敗の記録は `docs/adr/`(台帳: `docs/adr/STATUS.md`)。新しい
  機構を足す前に、同じ問題を扱った ADR を読む。
- 変更後は commit 前に `make check`(= `cargo fmt --check` / `cargo clippy
  --all-targets -- -D warnings` / `cargo nextest run` / `cargo test --doc`)を通す。
- 人間向けの成果物(summary・PR 本文・設計文書)は日本語で書く。コード識別子・
  commit メッセージは英語慣習に従う。
