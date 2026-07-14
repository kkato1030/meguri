# spec: issue #176 — escalation を集約し、人間対応が要る全経路に `needs-human` を貼る + autonomy モード + self-review disposition

> このファイルは実装が landing したら削除される使い捨ての足場。恒久的な設計判断は
> ADR 0012 に、実装は本体コードに残る。

## spec 深度: design(設計spec)

**理由:** 不確実性も影響範囲も大きい。escalation は約10サイトに分散しており、
新モジュール(`escalation.rs`)・新 config 軸(`Autonomy`)・self-review verdict の
2値→3値コントラクト変更・auto-merge 既定挙動の変更を伴う。1箇所の判断ミスが複数ループの
「人を呼ぶ/呼ばない」に波及する。よって architecture / alternatives / migration /
observability / test strategy を含む design tier で書く。

## 何を作るか(ADR 0012 の要約)

1. **self-review = 自動修復層(層1)**: verdict を `clean`/`fixable`/`needs_human` の3値に。
   `fixable`→自動修復継続、`needs_human`→即 escalate、`max_rounds` 未収束→escalate。モード非依存。
2. **guard = 人間ゲート層(層2)**: findings(plan/impl 一律)→ `needs-human`。guard-fixer は作らない。
   discover の needs-human skip を plan 側にも足して impl と対称化。
3. **escalation 集約**: `src/engine/escalation.rs` に人間ゲートの全分岐を寄せ、
   `needs-human` を貼る唯一の経路にする。conflict_resolver 枯渇・self-review 未収束もここへ。
4. **autonomy モード**: `Autonomy { Attended(既定), Full }`。唯一の実役割は auto-merge の
   arm ゲート(Full のみ arm)。escalation はモード非依存。

## Acceptance criteria

- [ ] self-review の verdict が `clean`/`fixable`/`needs_human` の3値を受理し、レビュー
      プロンプトが3値と分類基準(機械的に直せる=fixable / 判断が要る=needs_human)を説明する。
- [ ] self-review が `needs_human` を受けたら round を消費せず即 escalate する。
- [ ] self-review が `max_rounds` 到達で未収束のとき、footer 公開ではなく escalate する。
- [ ] guard settle が findings(plan/impl とも)で `needs-human` を貼る(`escalation.rs` 経由)。
- [ ] guard discover/candidate が `needs-human` の付いた **plan** PR を skip する(impl と対称)。
- [ ] conflict_resolver が `MAX_RESOLVE_RUNS` 枯渇時に、**今も `CONFLICTING` の PR に限って** escalate する(競合解消済みの PR には貼らない)。
- [ ] 人間ゲートの全 escalate が `escalation.rs` の中央ヘルパを通る(issue/local/PR の3宛先)。
- [ ] `Autonomy` config が global 既定 + project override(wholesale)で読め、
      `Config::autonomy_for(project)` が返す。hot-reload 対象。
- [ ] auto-merger が `autonomy == Full` のときだけ arm する。Attended では green でも arm しない。
- [ ] 既存の escalate サイト(validate/agent needs_human/spec_worker/ci_fixer/watchdog)が
      引き続き `needs-human` を貼る(集約でリグレッションしていない)。

## 触るファイル

| ファイル | 変更 |
|---|---|
| `src/engine/escalation.rs` **(新規)** | 中央ヘルパ。`escalate_task(deps, key, reason)`(task_source 経由)と `escalate_pr(deps, pr, reason)`(ラベル+コメント統一)。既存の散在コードをここへ寄せる |
| `src/engine/mod.rs` | `mod escalation;` 登録 |
| `src/engine/impl_reviewer.rs` | `ReviewVerdict` を3値化。`ImplReviewFile`/`read_review`/`review_prompt` を3値対応。`needs_human`→即 escalate、未収束→`mark_unconverged` を escalate に置換 |
| `src/engine/guard.rs` | `settle` の findings を `escalation.rs` 経由で `needs-human`。`candidate_kind` の Plan 分岐に needs-human skip を追加 |
| `src/engine/conflict_resolver.rs` | `MAX_RESOLVE_RUNS` 枯渇時に escalate。ただし **PR が今も `CONFLICTING` であることを確認した後**に限る(現行の budget skip は mergeable 判定より前にあるので、その地点で貼ると既に競合解消済みの PR にも `needs-human` を付けてしまう。判定順を budget→mergeable から mergeable→budget に入れ替えるか、budget 超過を検知したら追加で mergeable を確認してから escalate する)。1度だけ貼るための冪等性(既に `needs-human` なら skip)にも注意 |
| `src/engine/flow.rs` | `escalate_on_forge` 等の既存 escalate を `escalation.rs` へ委譲。`Checkpoint` に必要なら未収束フラグ調整 |
| `src/engine/auto_merger.rs` | arm 条件に `autonomy == Full` を追加。guard-failed escalate は `escalation.rs` 経由へ |
| `src/config.rs` | `Autonomy` enum・`Config.autonomy`・`ProjectConfig.autonomy`・`autonomy_for`。doctor 用の任意 warn |
| `src/main.rs`(doctor) | 任意: `auto_merge.enabled && autonomy=Attended` の不整合を warn |
| `.claude/rules/overview.md` | 実装 landing 時に「人間対応が要る全経路は `escalation.rs` 経由で必ず `needs-human`」の不変条件を1行追記(この spec では書かない) |

## Architecture への影響

- **新しい依存の向き**: 各ループ → `escalation.rs` → `task_source` / `forge`。ループが
  `forge` を直接叩いて `needs-human` を貼る現状の多点分散を、単方向の集約に変える。
- **層の分離**: self-review(自動修復)と guard(人間ゲート)は既に別コンポーネントだが、
  escalation の観点で「どちらが自動でどちらが人を呼ぶか」を明文化する。guard-fixer を
  作らない判断で、AI↔AI ピンポンを再導入しない(ADR 0006 の不可逆な後退を防ぐ)。
- **autonomy の作用点は1つ**: auto_merger の arm 条件のみ。escalation ロジックに
  モード分岐を持ち込まない(分岐が1箇所に閉じ、テストと推論が単純になる)。

## 検討した代替案 / 決定

- **guard-fixer を作り full-auto では fixable を自動修復(却下)**: ADR 0006 が消した
  AI↔AI ピンポンを復活させる。自動修復は self-review が既に担う。→ ADR 0012 §2。
- **escalation を各ループに残しヘルパ関数だけ共有(却下)**: 「貼り忘れ」の構造的余地が
  残る。P1 を保証するには「唯一の経路」にする必要がある。
- **autonomy を escalation にも効かせる(却下)**: P1/P4(必ず/枯渇でも人を呼ぶ)は
  環境非依存の不変条件。モードで escalate を消すと不変条件が崩れる。

## Migration & rollback

- **config は加算的**: `autonomy` は既定 Attended で、未設定の config はそのまま読める。
  DB スキーマ変更なし。`Checkpoint`(sqlite `checkpoint_json`)にフィールドを足す場合も
  `#[serde(default)]` で後方互換。永続状態のマイグレーションは不要。
- **verdict コントラクト**: `.meguri/self-review.json` の `verdict` が2値→3値。
  永続データではなく毎ターン再生成される成果物なので保存済みデータの移行は無い。
  旧プロンプトを見た agent が `findings` を書く可能性への保険として、`read_review` は
  `findings` を `fixable` の別名として受理する(下位互換)ことを検討。
- **auto-merge 既定挙動の変化(要注意)**: `autonomy` 既定 Attended により、現在
  `auto_merge.enabled=true` の環境は arm されなくなる。ロールバック手順 = 各環境が
  `autonomy = "full"` を明示する(global か project)。影響は「明示的に auto-merge
  有効化済み」の環境に限られる(既定は元々 `enabled=false`, ADR 0003)。doctor の warn で通知。
- **rollback**: 本 issue は既存ループに閉じた変更で外部状態を作らない。revert すれば
  挙動は完全に戻る(残留するマイグレーション成果物が無い)。

## Observability

- 既存 event を維持しつつ、集約点で一貫した event を出す:
  - `escalation.raised`(宛先種別 issue/local/pr・サイト名・reason)を `escalation.rs` から発火。
  - self-review: `self_review.needs_human`(即 escalate)・`self_review.unconverged`(escalate へ変更)。
  - guard: `guard.escalated`(plan/impl・findings)。
  - conflict_resolver: `conflict_resolver.exhausted`。
  - auto_merger: arm 抑止時に `automerge.attended_hold`(Attended で green だが arm しない旨)。
- これにより「どのサイトが何回人を呼んだか」を event から集計でき、集約のリグレッション
  (貼り忘れ)を運用で検知できる。

## Test strategy

- **各 escalation サイトが `needs-human` を貼る**(FakeForge / FakeMux の記録に対して):
  self-review `needs_human` / self-review 未収束 / guard(plan) findings / guard(impl) findings /
  conflict_resolver 枯渇。既存サイト(validate 枯渇・agent needs_human・spec_worker・
  ci_fixer 枯渇)の回帰も1本ずつ。
- **conflict_resolver の枯渇 escalate は CONFLICTING 限定**: budget 超過かつ今も
  `CONFLICTING` の PR は escalate される / budget 超過だが競合解消済み(mergeable)の PR は
  escalate されない、の両方を FakeForge の mergeable 応答を出し分けて検証する。
- **verdict 3値**: `read_review` が `clean`/`fixable`/`needs_human` を受理・検証する
  ユニットテスト(既存 `review_file_parses_and_validates` を拡張)。`findings` エイリアス
  受理の是非をテストで固定する。
- **自動修復の継続**: `fixable` は round を1つ消費して修復ループを続ける / `needs_human` は
  round を消費せず即 escalate する、をチェックポイントで検証。
- **autonomy 分岐**: Attended では green + guard clean でも arm しない / Full では arm する
  (auto_merger のユニット + FakeForge)。`autonomy_for` の override/wholesale を config テストで。
- **guard discover skip**: `needs-human` 付き plan PR が candidate から外れる(impl と対称)。
- **統合テスト**: 既存 `tests/*.rs`(実 tmux・実 git worktree)に、self-review の
  needs_human 分類が実 fake_agent 経由で `needs-human` に至る通しを1本足すか検討。

## 決定ポイント(レビューで確定させたい)

1. **guard を full-auto でも人間ゲートに留める**(本 issue の推奨)で確定してよいか。
   代替は guard-fixer だが ADR 0006 のピンポン再導入トレードオフがある(ADR 0012 §2)。
2. **autonomy の実役割を「auto-merge arm ゲート」1点に絞り、escalation を完全にモード非依存に
   する**解釈で良いか。issue の「escalate-on-枯渇 を必須化するか」は、P4(枯渇でも人を呼ぶ)を
   全モードで満たす=モード非依存、と読んだ。
3. **auto-merge 既定挙動の変化を許容するか**。既定 Attended で既存の auto_merge 有効環境は
   `autonomy=full` 明示が要る。許容せず「後方互換のため auto_merge.enabled=true なら
   実質 full 扱い」に倒す選択肢もあるが、autonomy の意味が濁る。
4. **verdict の `findings`→`fixable` エイリアス受理**を後方互換として入れるか、
   3値をハードに要求するか。
