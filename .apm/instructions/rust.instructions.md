---
description: Rust のエラーハンドリングとモジュール構成の流儀
applyTo: "src/**/*.rs"
---

- エラーハンドリングは `anyhow::Result` + `.context("...")` / `bail!("...")` で統一する
  （`src/gitops.rs` 参照）。ライブラリ独自のエラー型は導入しない。
- `unwrap()` / `expect()` はテストコード（`#[cfg(test)]`）以外では使わない。失敗しうる箇所は
  `Result` を伝播させるか `Context` で理由を付けて `bail!` する。
- git 操作は `src/gitops.rs` に集約する。他のモジュールから `git` プロセスを直接起動しない —
  worktree 操作・branch 命名・clean tree 検証など、git に触れるロジックはすべてここを経由する。
- forge（GitHub）・mux（tmux/herdr）は `src/forge/mod.rs` の `Forge` トレイト・
  `src/mux/mod.rs` の `Mux`/`AgentState` を通じて抽象化する。具体的な実装
  （`src/forge/gh.rs` / `src/forge/fake.rs`、`src/mux/tmux.rs` / `src/mux/herdr.rs` /
  `src/mux/fake.rs`）を呼び出し側から直接 match しない。新しい振る舞いはまずトレイトに足す。
- issue ラベル（`meguri:*`）は権威反転後のモデルに従う: `ready`/`hold` は人間の
  エッジ入力（intake が読む）、`working`/`implementing`/`needs-human` は sqlite の
  task 状態の best-effort 投影。ラベル定数は `src/forge/mod.rs` の `LABEL_*` にまとまっている。
- ループ実装は `src/engine/` にループ1つ = ファイル1つで置く（現在の核は `worker.rs` のみ）。
  完了コントラクトのプロンプト生成・result.json の読み書きは `src/turn/` が担当し、
  各ループはそこを通す。
