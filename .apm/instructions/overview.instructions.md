---
description: meguri の全体像・完了コントラクト・テスト/チェックの回し方
---

- meguri は GitHub issue（または local mode のタスク）を受け取り、AI コーディングエージェントを
  `herdr`/`tmux` の生きたペインで動かすオーケストレーターである。エージェントの画面を読み取って
  成否を判定することはしない。
- 各ターンは worktree に `.meguri/prompt-<turn_id>.md` を書き、エージェントはそれを実行して
  `.meguri/result.json` を書くことで完了を通知する（完了コントラクト）:
  `{"turn_id": "...", "status": "success" | "failure" | "needs_human", "summary": "...", "pr_body": "..."}`。
- `status: "success"` の申告は独立に検証される（`src/gitops.rs` / `src/turn/prompts.rs`）:
  git tree が clean であること、base branch より commit が進んでいること、`check_command`
  （設定されていれば）が通ること。この3つが揃わない限り成功として扱わない。
- `.meguri/` 配下（`prompt-*.md` / `result.json`）は実行時に生成される制御ファイルであり、
  リポジトリにコミットしない。
- forge（GitHub）・mux（tmux/herdr）に依存するテストは実サービスを叩かず、
  `src/forge/fake.rs` の `FakeForge` / `src/mux/fake.rs` の `FakeMux` を使う。両方とも
  呼び出しを記録するだけのインメモリ実装で、アサーションはその記録に対して行う。
- 統合テスト（`tests/*.rs`）はスクリプト化された疑似エージェント TUI
  （`tests/fixtures/fake_agent.sh`）を実 tmux・実 git worktree・ローカルの bare origin に
  対して動かし、ブロックダイアログ処理・虚偽申告の訂正・validation feedback・crash recovery
  まで通しで検証する。
- 変更後は commit 前に `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
  `cargo nextest run`（または `cargo test`）/ `cargo test --doc` を通す。CI と同じ並びである。
- summary・PR 本文・ADR・spec などエージェントが人間向けに書く成果物は日本語で書く
  （`language = "日本語"` がデフォルト）。コード識別子・commit メッセージは既存の英語慣習に従う。
