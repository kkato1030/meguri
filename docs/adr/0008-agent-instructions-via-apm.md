# ADR 0008: エージェント向け静的指示は microsoft/apm をソースにし、コンパイル成果物はコミットしない

- Status: accepted
- Date: 2026-07-13
- Issue: #137

## Context

meguri リポジトリには AI エージェント向けの静的指示ファイル(`CLAUDE.md` / `.claude/rules/` /
`AGENTS.md`)が一切なかった。meguri 自身が多数の issue を並行して worker/planner/fixer などの
ループに投げる以上、各エージェントに「このリポジトリ固有の知識」(完了コントラクト・
モジュール構成・ADR/spec の運用など)を静的に渡す土台が要る。

一方で、その土台を素朴に「`CLAUDE.md` を直接手書きして commit する」で作ると、meguri の実運用
（1 issue = 1 worktree = 1 branch が多数並行する)と相性が悪い。指示を 1 行変えるたびに、
その時点で走っている全 worktree/PR 側で `.claude/rules/` 等の再生成 diff が発生し、
merge のたびに conflict とレビューノイズを生む。

[microsoft/apm](https://github.com/microsoft/apm)(Agent Package Manager)は、この
「ソース(`.apm/instructions/*.instructions.md` + `apm.yml`)」と「ターゲット別コンパイル成果物
(`CLAUDE.md` / `.claude/rules/` / `AGENTS.md` / `.codex/` など)」を分離する道具として存在する。
`apm install` / `apm compile` を実行すると、ソースから Claude Code / Codex ネイティブ形式が
再構築できる。

## Decision

1. **apm をソースの唯一の真実(single source of truth)にする。** `apm.yml`(`targets: [claude,
   codex]` — `src/routing.rs` の built-in 推奨テーブルがルーティング先として使う2つの CLI)と
   `.apm/instructions/*.instructions.md` をコミットする。`apm.lock.yaml` も固定のためコミットする
   (apm は v0.x で破壊的変更が起きうるため、ロックなしでは `apm install` の再現性が保証できない)。

2. **コンパイル成果物はコミットしない。** 実際に `apm install && apm compile` を実行して検証した
   ところ、この apm.yml / instructions からは以下が生成される:
   - `.claude/rules/{overview,rust,docs}.md`(`apm install` が `.apm/instructions/` の内容を
     Claude Code のネイティブ配置に展開したもの)
   - `AGENTS.md`(ルート、`overview.instructions.md` の内容)と `src/AGENTS.md`
     (`rust.instructions.md` の `applyTo: src/**/*.rs` に基づく配置。Codex はスコープ付き
     instructions を単一の `AGENTS.md`/`src/AGENTS.md` に畳み込む形で読む)
   - `apm_modules/`(依存キャッシュ。現状 apm 依存は0件だが `apm install` が作る)
   - `CLAUDE.md` は生成されない(`.claude/rules/` が既にあるので Claude Code はそちらを直接
     読み、apm 側が重複コンテキストとして自動的にスキップする)。`.agents/` も、skill
     パッケージを依存に持たない現状の構成では生成されない。
   - これらすべて(`CLAUDE.md` / `AGENTS.md` / `.claude/rules/` / `.codex/` / `apm_modules/` /
     `.agents/`)を `.gitignore` に追加し、worktree ローカルの生成物として扱う。
   - 生成する仕組みは worktree 準備時のフック(#138 の `worktree_setup`)に持たせる。この issue
     (#137)ではソースの整備までを範囲とし、フック自体は実装しない。

3. **静的指示はリポジトリ固有の知識に限定し、ロール別の振る舞いは書かない。** worker は push
   しない・reviewer は read-only、といった役割ごとの制約は実行時プロンプト(`src/turn/prompts.rs`、
   各 `src/engine/*.rs` のプロンプト生成)がすでに担っている。静的ファイルに同じ内容を書くと
   二重管理になり、どちらかが更新漏れでドリフトする。したがって `.apm/instructions/` には
   「エージェントの役割によらず常に真」なリポジトリの知識だけを書く:
   - `overview.instructions.md`(`applyTo` なし → `AGENTS.md` に畳み込まれる): 全体像・完了
     コントラクト・fake forge/mux を使ったテストパターン・チェックコマンドの回し方・成果物の
     言語
   - `rust.instructions.md`(`applyTo: src/**/*.rs`): エラーハンドリングの流儀・
     `gitops.rs`/trait 抽象化などモジュール構成の意図
   - `docs.instructions.md`(`applyTo: docs/**`): ADR は恒久・spec は使い捨てという運用、
     2軸ラベルモデル(ADR 0005)への参照

## Consequences

- `apm.yml` / `apm.lock.yaml` / `.apm/` の3つだけが git 管理下に入り、`.gitignore` に追加した
  6つのパスは worktree ごとに `apm install && apm compile` で再構築される。指示を1行直しても、
  並行中の他ブランチに再生成 diff が波及しない。
- 現時点では `worktree_setup` フック(#138)が未実装なので、`apm install && apm compile` は
  開発者が手動で実行する必要がある(README に手順を記載)。#138 が入るまでは、meguri の
  ループ自身が起動する worktree にも `CLAUDE.md`/`.claude/rules/`/`AGENTS.md` は存在しない —
  #138 のスコープ。
- apm は v0.x で API/生成物の形が変わりうる。挙動が変わった場合は `apm.lock.yaml` の
  `apm_version` で検出できるが、`apm.yml` の書式自体が壊れる変更が来た場合は
  この ADR の前提(生成される成果物の一覧)を見直す必要がある。
