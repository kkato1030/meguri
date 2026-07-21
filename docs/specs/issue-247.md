# spec: issue #247 — blocking finding の anchor 機械照合と reviewer turn の fresh session 既定化

> 使い捨ての足場(ADR 0001)。恒久的な設計判断は **ADR 0028** に、実装完了時にこの spec は消す。

## なぜこの深さ(design tier)か

持続状態(checkpoint の `Finding`/`LedgerEntry` JSON)と reviewer の出力契約という public contract に
触れ、session lifecycle という広い波及面を持つ。未決定も多い(anchor の形・stale の扱い・対象ロープ)。
よって design tier + 移行/rollback 必須(veto rule 該当: schema/contract 変更)。

## ゴール

「存在しない引用を持つ blocking finding が偽の不収束で needs-human に落ちる」経路(設計書 §3-B、#231)を
2つの独立した機構で閉じる:

- **A. anchor 機械照合** — 内部 self-review(ADR 0022 台帳)の `defect` finding に現物引用を必須化し、
  台帳へ畳む前に現 head と照合。stale は1回差し戻し、なお stale なら棄却。
- **B. reviewer fresh session 既定** — reviewer ロール(self-reviewer / pr-reviewer)は resume せず
  毎ターン fresh spawn。旧 head の記憶が現物に勝つ構造原因を絶つ。fixer 系(author lane)は resume 継続。

A と B は関連するが独立にレビュー/rollback 可能。ただし #247 は両者をまとめて1 PR で入れる
(#231 fixture が両者を貫くため。分割は過剰分割)。

## 受け入れ基準

1. 存在しない引用を持つ `defect` finding が needs-human に到達しない。差し戻し1回 → クリーンなら通過、
   なお stale なら該当 finding を棄却して verified な finding だけで phase 継続。
2. **#231 の実ケースを fixture 化**(下記テスト戦略の 2 本)。
3. reviewer ロールのターンが保存済み session id を resume に使わない。author/fixer 系は従来どおり resume。
4. `decision` 型 finding は anchor 照合の対象外(`quote` 任意、照合 skip)。
5. stale 率が `meguri stats review` に出る。
6. 単一 reviewer 経路の checkpoint は byte-for-byte 不変(追加フィールドは `#[serde(default)]`)。

## 触るファイル

- `src/engine/self_review.rs` — `Finding` に `quote`、`LedgerEntry` に `anchor_verified` を追加。
  `review_turn` / `round1_parallel_review` / `anchor_confirm` の verify 段に anchor 照合 + stale 差し戻しを追加。
  `update_ledger_from_review` は verified finding だけ畳む。`self_review.anchor_stale` イベント新設。
  review プロンプト(`review_prompt` 系)に `quote` 必須と照合ルールを明記。
- `src/engine/flow.rs` — `Lane` に `reuse_session: bool`。`author_lane`(role で分岐)/
  `self_review_lane_for`(常に false)で設定。`ensure_pane` / `spawn_direct_process` は
  `reuse_session == false` のとき保存済み session id を resume に読まない(保存はする)。
- `src/config.rs` — `[review]` に `anchor_verification`(既定 true)のトグル。rollback レバー。
- `src/store/stats.rs` — `self_review.anchor_stale` から stale 率(件数 / review ターン数)を集計。
- テスト: `src/engine/self_review.rs` の unit、`tests/*.rs` の統合(fake_agent.sh)を2本追加。

## 主要な決定(A-or-B を先に潰す)

1. **anchor の形**: `Finding` に `quote: Option<String>` を足す。`defect` は必須(空/欠落は照合失敗扱い
   ではなく **contract 違反 → 既存 corrective-turn で1回差し戻し**、再注入で quote を書かせる)。
   `decision` は任意。`line` は照合条件に含めず位置ヒントに留める(古い行番号で正しい引用を落とさない)。
2. **照合ロジック**: `quote` が現 head の worktree 上 `path` ファイルに **substring 逐語一致**するか。
   ファイルが無い/読めない場合も stale 扱い。照合は review ターンの verify 段(tree clean・id 検証の隣)で行う。
3. **stale の扱い**: stale が1つでもあれば1回差し戻し(`self_review.anchor_stale` emit)。差し戻し後も
   stale なら **台帳に入れず棄却**(needs-human に落とさない)。verified finding だけ `update_ledger_from_review`。
   stale による差し戻しは既存の tree/id 差し戻しと独立に高々1回(理由ごとに1回)。
4. **anchor_verified**: `LedgerEntry.anchor_verified: bool`。照合を通った/免除された(decision)finding は true。
   stale は台帳に入らないので、**stale 率は台帳ではなくイベントから**導出(既存の「emit = stats source」流儀)。
5. **fresh session の対象**: reviewer ロール = `self-reviewer`(self-review / self-review#N / self-review-anchor
   lane)と `pr-reviewer`(pr-review lane)。author lane(worker/planner/spec-worker + 相乗りする
   fixer/spec-fixer/ci-fixer)は resume 継続。判定は `Lane.reuse_session` に集約し、ロープ名の直 match を避ける。
6. **pr-reviewer は anchor 照合をやらない**(ADR 0028 スコープ)。pr-reviewer は prose findings 契約のままで、
   #247 では **fresh session だけ**効かせる。構造 anchor は将来の follow-up。#231 の実インシデント(pr-reviewer
   resume で stale 再主張)は **B の fresh session** が直接閉じる。
7. **config トグル**: `[review].anchor_verification`(既定 true)。false で A を無効化(rollback)。
   B(fresh session)は lifecycle 既定でトグルを設けない(rollback はコード revert)。

## 移行 / rollback(veto rule: schema/contract 変更のため必須)

- **前方移行**: `quote` / `anchor_verified` は `#[serde(default)]` の追加フィールド。既存 checkpoint
  (in-flight run)はデフォルト値で読める。`anchor_verified` の default は `false` だが、ledger は
  照合済み finding しか持たないため、既存エントリが `false` でも fixer/収束ロジックには影響しない
  (`anchor_verified` は監査/表示専用で、open/fixed の判定には使わない)。
- **後方 rollback**: #247 を revq しても、増えたフィールドは serde default で無視され checkpoint は読める。
  `[review].anchor_verification = false` で A の挙動だけを即時に殺せる(コード revert 不要のレバー)。
- **fresh session の rollback**: session id の**保存**は続けるので、B を revert すれば次ターンから
  再び resume を読むだけ。棄てたのは resume の「読み取り」であって保存データではない。DB migration 無し。

## observability

- `self_review.anchor_stale`(棄却/差し戻し。round・finding 数・stale 件数を payload に)。
- `meguri stats review` に stale 率(stale finding 数 / review ターン数)を追加(ADR 0026 CATCH)。
- fresh session は既存の `pane.resume_failed` とは別で、resume を試みない経路なのでイベント追加は不要
  (`direct.spawned` / spawn の `resumed:false` で観測できる)。

## テスト戦略

- **unit(self_review.rs)**: (a) quote が現物に在る → verified で台帳へ。(b) quote 不在 → 1回差し戻し。
  (c) 差し戻し後もう一度 stale → 棄却され台帳に入らず、verified 分だけ残る。(d) `decision` は quote 無しで通る。
  (e) `Lane.reuse_session` が role で正しく分岐する。
- **統合1(#231 再現・pr-reviewer / B)**: fake_agent が resume 前提で旧 head の stale finding を再主張する
  シナリオを組み、fresh session 既定で pr-reviewer が現 head を読み直し clean → spec-ready 昇格、
  needs-human に落ちないことを検証。
- **統合2(内部 self-review / A)**: 1周目で「現物に無い引用」の finding を出す fake_agent →
  meguri が stale として1回差し戻し → 2周目クリーン → 収束・publish、needs-human ゼロ。
- 既存の self-review / fresh でない author lane resume のテストが緑のままであること(回帰なし)。

## 実装しないこと

- pr-reviewer の finding 構造化・anchor 照合(将来 follow-up、ADR 0028 スコープ外)。
- P1/P2/P4/P5/P6 系(設計書 #241 の別 issue)。
