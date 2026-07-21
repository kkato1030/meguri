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
4. `decision` 型 finding、および既存 id の再リストは anchor 照合の対象外(新規 finding のみ照合)。
5. stale 率が `meguri stats review` に単一定義で出る(§observability の `anchor_checked` イベントから)。
6. 旧 checkpoint は serde default で読め、`anchor_verification = false` のとき checkpoint は
   byte-for-byte 不変(追加フィールドは `Option` + `skip_serializing_if`)。
7. anchor の `path` が worktree 外(絶対パス・`..`・symlink 越え)を指す finding は stale に倒し、
   worktree 外のファイルを読まない・プロンプトへ流さない。

## 触るファイル

- `src/engine/self_review.rs` — `Finding` に `quote: Option<String>`、`LedgerEntry` に
  `anchor_verified: Option<bool>` を追加(ともに `#[serde(default, skip_serializing_if = "Option::is_none")]`)。
  sequential 経路(`review_turn`)の verify 段に、tree/id とは**別カウンタ**の anchor stale リトライ(1回)を追加。
  `round1_parallel_review` は union-merge 後に新規 finding をまとめて照合し stale を棄却(sequential 差し戻しはしない)。
  照合は **新規 finding のみ**を対象にし、`update_ledger_from_review` に渡す再リスト集合は stale で削らない
  (drop == 解消 の誤読を防ぐ)。`self_review.anchor_checked` イベント新設(ターンにつき1回)。
  review プロンプト(`review_prompt` 系)に `quote` 必須と照合ルールを明記。
- `src/gitops.rs` — HEAD の tracked blob を repo-relative path で読むヘルパ(`git show HEAD:<path>` 相当)と、
  worktree 内拘束の path 正規化(絶対/`..`/symlink 越えを弾く)。git 操作は gitops に集約する規約に従う。
- `src/engine/flow.rs` — `Lane` に `reuse_session: bool`。`author_lane`(role で分岐)/
  `self_review_lane_for`(常に false)で設定。reviewer ターンは spawn 前に lane の生存 pane を release/kill
  してから resume 引数なしで素の spawn を行う(`ensure_pane` の adopt を回避)。`spawn_direct_process` も
  `reuse_session == false` のとき session id を `--resume` に読まない。session id の**保存**は継続。
- `src/config.rs` — `[review]` に `anchor_verification`(既定 true)のトグル。rollback レバー。
- `src/store/stats.rs` — `self_review.anchor_checked` の payload を合計し
  stale 率 = Σ`stale_discarded` / Σ`findings_total` を集計。CLI 表示を追加。
- テスト: `src/engine/self_review.rs` / `src/engine/flow.rs` の unit、`tests/*.rs` の統合(fake_agent.sh)。

## 主要な決定(A-or-B を先に潰す)

1. **anchor の形**: `Finding` に `quote: Option<String>` を足す。`defect` は必須(空/欠落は照合失敗扱い
   ではなく **contract 違反 → 既存 corrective-turn で1回差し戻し**、再注入で quote を書かせる)。
   `decision` は任意。`line` は照合条件に含めず位置ヒントに留める(古い行番号で正しい引用を落とさない)。
2. **照合ロジック**: 対象は **新規 finding のみ**(`id` の無い finding。既存 id の再リストは照合しない —
   初出時に照合済みで、fix でコードが動けば quote が正当に消える。継続/解消は ping-pong が裁く)。
   `path` を repo-relative に正規化し **worktree 内に拘束**(絶対・`..`・symlink 越えは stale)。読む対象は
   working tree ではなく **clean な HEAD の tracked blob**(gitops 経由)とし、その中で `quote` の
   **substring 逐語一致**を見る。ファイルが無い/読めない/照合失敗はすべて stale。
3. **stale の扱い(sequential)**: 単一 reviewer / round 2+ で新規 stale があれば **1回だけ差し戻し**。
   retry 状態は tree/id の `corrective_turns` とは **別カウンタ**にし、終端は needs-human ではなく
   **棄却に固定**(tree/id は従来どおり2回目で NeedsHuman、anchor は昇格しない)。棄却は新規 finding のみに
   適用し、`update_ledger_from_review` へ渡す再リスト集合を stale で削らない(drop == 解消 の誤読を防ぐ)。
   verified finding だけ台帳へ。
4. **stale の扱い(round 1 parallel)**: reviewer 別の corrective-turn retry は持たない(各 reviewer は
   fresh session で head を読んでおり round 1 の stale は稀。retry 単位が非決定的になるのを避ける)。
   union-merge 後の新規 finding をまとめて照合し **stale を棄却**、verified な他 reviewer の finding は保持。
5. **anchor_verified**: `LedgerEntry.anchor_verified: Option<bool>`(`skip_serializing_if = "Option::is_none"`)。
   照合が走ったら `Some(true)`、無効時・decision 免除は `None`(serialize されない)→ byte-for-byte 不変。
   監査/表示専用で open/fixed 判定には使わない。stale 率は台帳ではなくイベントから導出(§observability)。
6. **fresh session の対象**: reviewer ロール = `self-reviewer`(self-review / self-review#N / self-review-anchor
   lane)と `pr-reviewer`(pr-review lane)。author lane(worker/planner/spec-worker + 相乗りする
   fixer/spec-fixer/ci-fixer)は resume 継続。判定は `Lane.reuse_session` に集約し、ロープ名の直 match を避ける。
   **session id を読まないだけでなく、spawn 前に生存 pane を畳む**(§触るファイル `flow.rs`)。
7. **pr-reviewer は anchor 照合をやらない**(ADR 0028 スコープ)。pr-reviewer は prose findings 契約のままで、
   #247 では **fresh session だけ**効かせる。構造 anchor は将来の follow-up。#231 の実インシデント(pr-reviewer
   resume で stale 再主張)は **B の fresh session** が直接閉じる。
8. **config トグル**: `[review].anchor_verification`(既定 true)。false で A を無効化(rollback)。
   B(fresh session)は lifecycle 既定でトグルを設けない(rollback はコード revert)。

## 移行 / rollback(veto rule: schema/contract 変更のため必須)

- **前方移行**: `quote` / `anchor_verified` は `Option` + `#[serde(default, skip_serializing_if = "Option::is_none")]`
  の追加フィールド。既存 checkpoint(in-flight run)は None で読める。`anchor_verified` は監査/表示専用で
  open/fixed 判定には使わないので、None のエントリが fixer/収束ロジックに影響しない。
- **byte-for-byte 不変の範囲**: 常時 serialize される裸の `bool` だと単一 reviewer 経路の checkpoint が
  変わってしまう。`Option` + `skip_serializing_if` にすることで、`anchor_verification = false`(照合を
  走らせない)なら両フィールドは None のまま **serialize されず byte-for-byte 不変**。照合が走る経路では
  結果を記録した新表現になる(不変を主張するのは無効時に限る、と受入基準6に明記)。
- **後方 rollback**: #247 を revert しても、増えたフィールドは serde default で無視され checkpoint は読める。
  `[review].anchor_verification = false` で A の挙動だけを即時に殺せる(コード revert 不要のレバー)。
- **fresh session の rollback**: session id の**保存**は続けるので、B を revert すれば次ターンから
  再び resume を読むだけ。棄てたのは resume の「読み取り」であって保存データではない。DB migration 無し。

## observability

- **単一イベント `self_review.anchor_checked`**(f6 の決定): reviewer ターンが照合を終えた時点で
  **ターンにつき1回だけ** emit(差し戻し中間状態では出さない → 二重計上しない。parallel は reviewer ごと、
  sequential は round ごと)。payload = `{ round, reviewer_index, findings_total, stale_discarded }`。
- **stale 率 = Σ`stale_discarded` / Σ`findings_total`**(照合した新規 finding のうち逐語照合に失敗した割合)。
  `meguri stats review` はこの1イベントを合計して出す。母集団(照合ターン数)も併記。CLI 表示は
  「anchor stale: X.X%(棄却 N / 照合 M)」。terminal phase 依存の既存 `review_stats` 母集団とは別に、
  この専用イベントを分子・分母の唯一のソースにする。
- fresh session は既存の `pane.resume_failed` とは別で、resume を試みない経路なのでイベント追加は不要
  (`direct.spawned` / spawn の `resumed:false`、および reviewer lane の pane release で観測できる)。

## テスト戦略

- **unit(self_review.rs)**: (a) quote が HEAD blob に在る → verified で台帳へ。(b) quote 不在 → 1回差し戻し。
  (c) 差し戻し後もう一度 stale → 棄却され台帳に入らず、verified 分だけ残る。(d) `decision` は quote 無しで通る。
  (e) 既存 id の再リストは照合されず、stale でも open のまま(fold で誤って解消されない)。
  (f) round 1 parallel: 1 reviewer が stale 新規、他 reviewer が verified → stale だけ棄却、verified は残り union に入る。
  (g) `path` が絶対/`..`/symlink で worktree 外 → stale、外のファイルを読まない。
- **unit(flow.rs)**: (h) `Lane.reuse_session` が role で正しく分岐。(i) reviewer lane は spawn 前に生存 pane を
  release し、resume 引数なしで spawn する(pane・direct 両モードで、前ターン session に接続しないこと)。
- **統合1(#231 再現・pr-reviewer / B)**: fake_agent が resume 前提で旧 head の stale finding を再主張する
  シナリオを組み、fresh session 既定で pr-reviewer が現 head を読み直し clean → spec-ready 昇格、
  needs-human に落ちないことを検証。
- **統合2(内部 self-review / A)**: 1周目で「現物に無い引用」の finding を出す fake_agent →
  meguri が stale として1回差し戻し → 2周目クリーン → 収束・publish、needs-human ゼロ。
- 既存の self-review / fresh でない author lane resume のテストが緑のままであること(回帰なし)。

## 実装しないこと

- pr-reviewer の finding 構造化・anchor 照合(将来 follow-up、ADR 0028 スコープ外)。
- P1/P2/P4/P5/P6 系(設計書 #241 の別 issue)。
