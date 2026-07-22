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
   なお stale なら新規は棄却・既存 id の再リストは open 保持 + fixer 対象外にして、needs-human に落とさない。
2. **#231 の実ケースを fixture 化**(下記テスト戦略)。旧 head の引用を **同じ id で再主張**する経路も塞ぐ。
3. reviewer ロールのターンが保存済み session id を resume に使わない。author/fixer 系は従来どおり resume。
4. `defect` finding は **新規・再リスト双方**を照合対象にする(`decision` 型のみ免除)。
5. stale 率が `meguri stats review` に単一定義で出る(`anchor_checked` イベントから)。分母 M=0 は N/A 表示。
6. 旧 checkpoint は serde default で読め、`anchor_verification = false` のとき checkpoint は
   byte-for-byte 不変(追加フィールドは `Option` + `skip_serializing_if`)。
7. anchor の `path` が worktree 外(絶対パス・`..`・symlink 越え)を指す finding は stale に倒し、
   worktree 外のファイルを読まない・プロンプトへ流さない。
8. anchor confirmation の overrule findings も照合される(照合を素通りする経路を残さない)。
9. `anchor_verified=Some(false)` の open entry は cap→final-fix publish 時に PR `<details>` に表示される。
10. `anchor_verified=Some(false)` の entry は rollback 用 pending に写らず、旧 binary が actionable 扱いしない。

## 触るファイル

- `src/engine/self_review.rs` — `Finding` に `quote: Option<String>`、`LedgerEntry` に
  `anchor_verified: Option<bool>` を追加(ともに `#[serde(default, skip_serializing_if = "Option::is_none")]`)。
  照合対象は **すべての `defect` finding(新規 + 再リスト)**。sequential 経路(`review_turn`)の verify 段に、
  tree/id とは**別カウンタ**の anchor stale リトライ(1回)を追加。差し戻し後もなお stale なら
  **新規は台帳から棄却・再リストは `Open` + `anchor_verified=Some(false)` で保持し fixer 対象集合から除外**。
  `update_ledger_from_review` は「listed だが anchor 失敗」を第3状態として扱い、omission 解消に落とさない。
  `round1_parallel_review` は **merge 前に reviewer 別に照合**し stale を棄却してから union-merge(f8)。
  `anchor_confirm` の overrule findings(`merge_reviews` の `extra`)も **merge 前に anchor turn 内で照合**し、
  専用の anchor index で `anchor_checked` を emit(f11)。fix file 検証の母集団を actionable set
  (open ∧ anchor_verified≠Some(false))にする。`mirror_open_to_pending` は `anchor_verified=Some(false)` の
  entry を pending に写さない(f13。rollback で旧 binary が actionable 扱いしないため)。
  `self_review.anchor_checked` イベント新設(照合を走らせた reviewer ターンにつき1回)。
  review プロンプト(`review_prompt` 系)に `quote` 必須・再リストも再照合・照合ルールを明記。
- `src/gitops.rs` — HEAD の tracked blob を repo-relative path で読むヘルパ(`git show HEAD:<path>` 相当)と、
  worktree 内拘束の path 正規化(絶対/`..`/symlink 越えを弾く)。git 操作は gitops に集約する規約に従う。
- `src/engine/flow.rs` — `Lane` に `reuse_session: bool`。`author_lane`(role で分岐)/
  `self_review_lane_for`(常に false)で設定。reviewer ターンは spawn 前に lane の生存 pane を release/kill
  してから resume 引数なしで素の spawn を行う(`ensure_pane` の adopt を回避)。`spawn_direct_process` も
  `reuse_session == false` のとき session id を `--resume` に読まない。session id の**保存**は継続。
  `self_review_details_with_outcome`(PR body の `<details>` renderer)に、`anchor_verified=Some(false)` の
  open entry を「anchor 未照合の open」として1行出す描画を追加(f12。現行は round ごとの件数しか出さない)。
- `src/config.rs` — `[review]` に `anchor_verification`(既定 true)のトグル。rollback レバー。
- `src/store/stats.rs` — `self_review.anchor_checked` の payload を合計し
  stale 率 = Σ`stale_count` / Σ`findings_total` を集計。**Σ`findings_total`=0 は N/A 表示(ゼロ除算回避)**。
  CLI 表示を追加。
- テスト: `src/engine/self_review.rs` / `src/engine/flow.rs` / `src/store/stats.rs` の unit、
  `tests/*.rs` の統合(fake_agent.sh)。

## 主要な決定(A-or-B を先に潰す)

1. **anchor の形**: `Finding` に `quote: Option<String>` を足す。`defect` は必須(空/欠落は照合失敗扱い
   ではなく **contract 違反 → 既存 corrective-turn で1回差し戻し**、再注入で quote を書かせる)。
   `decision` は任意。`line` は照合条件に含めず位置ヒントに留める(古い行番号で正しい引用を落とさない)。
2. **照合ロジック**: 対象は **すべての `defect` finding(新規 + 既存 id の再リスト)**。再リストを免除すると
   旧 head の引用を同じ id で再主張する #231 の型が残るため、再リストにも現 head の quote を要求する(f9)。
   `path` を repo-relative に正規化し **worktree 内に拘束**(絶対・`..`・symlink 越えは stale)。読む対象は
   working tree ではなく **clean な HEAD の tracked blob**(gitops 経由)とし、その中で `quote` の
   **substring 逐語一致**を見る。ファイルが無い/読めない/照合失敗はすべて stale。`decision` は免除。
3. **stale の扱い(sequential)**: 単一 reviewer / round 2+ で stale があれば **1回だけ差し戻し**
   (「修正で消えた finding は drop、残る concern は現 head で引用し直せ」)。retry 状態は tree/id の
   `corrective_turns` とは **別カウンタ**にし、終端は needs-human ではなく **下記の振り分けに固定**
   (tree/id は従来どおり2回目で NeedsHuman、anchor は昇格しない)。差し戻し後もなお stale:
   - **新規**(台帳に無い)→ 台帳に入れず **棄却**(閉じる対象が無く omission 誤読も起きない)。
   - **再リスト**(既存 id)→ entry を `Open` のまま保持し `anchor_verified=Some(false)`、**fixer 対象集合から除外**。
     omission 自動解消させず(第3状態)、fix turn を回さないので `fix_attempts` が伸びず ping-pong→needs-human
     に至らない。残れば max_rounds → cap→final-fix publish に落ち(needs-human でない)、PR `<details>` に
     「anchor 未照合の open」として出る(透明性)。
   fix file 検証の「open には disposition 必須」は **actionable set**(open ∧ anchor_verified≠Some(false))を母集団にする。
4. **stale の扱い(round 1 parallel + anchor overrule)**: **merge 前に reviewer 別に照合**する(f8)。
   union-merge 後だと reviewer 境界が消え `reviewer_index` 帰属と `findings_total`/`stale_count` の分割が
   できない。各 `self-review#N` が自分の findings を照合して stale を棄却してから union-merge(全 finding が
   新規なので棄却で済む)。verified な他 reviewer の finding は影響を受けない。reviewer 別 corrective-turn retry
   は持たない。**anchor confirmation の overrule findings(`merge_reviews` の `extra`)も同様に merge 前に
   anchor turn 内で照合**し、専用の anchor index で `anchor_checked` を emit(f11。ここを外すと overrule 経路が
   照合を素通りする)。
5. **anchor_verified は status と直交する制御フラグ**(f14): `LedgerEntry.anchor_verified: Option<bool>`
   (`skip_serializing_if = "Option::is_none"`)。3値 — `None`(照合なし: 無効時・decision 免除)/
   `Some(true)`(通過)/`Some(false)`(再リストが差し戻し後もなお stale)。**収束軸は `status` のまま**
   (収束 = open 数ゼロ)で `anchor_verified` は status を動かさない。ただし `Some(false)` は entry を
   **非 actionable** にし、(a) fixer 対象集合、(b) fix file の disposition 必須検証、から除く制御状態として使う
   — 単なる監査フラグではない。open/fixed の遷移は `status` だけが担い、`anchor_verified` は actionability と
   表示を制御する、と境界を定める。stale 率は台帳ではなくイベントから導出(§observability)。
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
  の追加フィールド。既存 checkpoint(in-flight run)は None で読める。`None` の entry は照合が走っていない
  = 従来どおりの actionable な open として扱われるので、新 binary が旧 checkpoint を読んでも挙動は変わらない。
- **stale 第3状態の rollback(f13)**: `anchor_verified=Some(false)` の open entry は `mirror_open_to_pending` で
  pending(`Vec<Finding>`、旧 binary 用の後方互換スナップショット)に **写さない**。写すと `Finding` に
  印が無いため、#247 前の binary が stale entry を通常の actionable finding として fixer に渡し、元の
  ping-pong を再発させてしまう。非 actionable な第3状態は pending から落とす(= rollback 時は消える)。
  これらは元々 fixer を回さない entry なので、消えても ping-pong を生まない。新 binary 側は台帳の
  `anchor_verified` を正として扱うので pending の欠落に影響されない。
- **byte-for-byte 不変の範囲**: 常時 serialize される裸の `bool` だと単一 reviewer 経路の checkpoint が
  変わってしまう。`Option` + `skip_serializing_if` にすることで、`anchor_verification = false`(照合を
  走らせない)なら両フィールドは None のまま **serialize されず byte-for-byte 不変**。照合が走る経路では
  結果を記録した新表現になる(不変を主張するのは無効時に限る、と受入基準6に明記)。
- **後方 rollback**: #247 を revert しても、増えたフィールドは serde default で無視され checkpoint は読める。
  `[review].anchor_verification = false` で A の挙動だけを即時に殺せる(コード revert 不要のレバー)。
- **fresh session の rollback**: session id の**保存**は続けるので、B を revert すれば次ターンから
  再び resume を読むだけ。棄てたのは resume の「読み取り」であって保存データではない。DB migration 無し。

## observability

- **単一イベント `self_review.anchor_checked`**(f6/f8 の決定): 照合を走らせた reviewer ターンにつき
  **1回だけ** emit(差し戻し中間状態では出さない → 二重計上しない)。**発火単位を照合単位に一致**させ、
  parallel は各 `self-review#N` が merge 前に照合するので reviewer ごと・`reviewer_index` 付き、sequential は
  round ごと。payload = `{ round, reviewer_index, findings_total, stale_count }`。`stale_count` は照合失敗数
  (新規棄却ぶん + 再リスト open 保持ぶんの両方)。
- **stale 率 = Σ`stale_count` / Σ`findings_total`**。`meguri stats review` はこの1イベントを合計して出す。
  母集団(照合ターン数)も併記。**Σ`findings_total`=0(全 clean・有効化直後など)は ゼロ除算を避け
  `N/A(照合 finding 0件)` と表示**(0.0% ではない)。`findings_total=0` でもイベントは emit(coverage を数える)。
  CLI 表示は「anchor stale: X.X%(失敗 N / 照合 M)」、M=0 は「anchor stale: N/A(照合 0件)」。
  terminal phase 依存の既存 `review_stats` 母集団とは別に、この専用イベントを分子・分母の唯一のソースにする。
- **透明性(f12)**: `anchor_verified=Some(false)` の open entry は、PR body の self-review `<details>` に
  「anchor 未照合の open finding」として1行出す(`self_review_details_with_outcome` を拡張)。cap→final-fix
  publish 時にこの entry が残るので、human merge gate が「照合できなかった指摘がある」ことを見られる。
- fresh session は既存の `pane.resume_failed` とは別で、resume を試みない経路なのでイベント追加は不要
  (`direct.spawned` / spawn の `resumed:false`、および reviewer lane の pane release で観測できる)。

## テスト戦略

- **unit(self_review.rs)**: (a) quote が HEAD blob に在る → verified で台帳へ。(b) 新規 quote 不在 → 1回差し戻し。
  (c) 差し戻し後もう一度 stale(新規) → 棄却され台帳に入らず、verified 分だけ残る。(d) `decision` は quote 無しで通る。
  (e) **既存 id の再リストが現 head で照合失敗 → 差し戻し後もなお stale なら open 保持 + `anchor_verified=Some(false)`
  + fixer 対象外**(omission で解消されず、ping-pong→needs-human にも至らない)。(e2) 再リストが現 head の quote で
  照合を通れば open 継続、fix でコードが消え reviewer が drop すれば解消。
  (f) round 1 parallel: **merge 前に reviewer 別照合** — 1 reviewer が stale、他が verified → stale だけ棄却、
  verified は union に入り `reviewer_index` が保たれる。(g) `path` が絶対/`..`/symlink で worktree 外 → stale、外を読まない。
  (l) **anchor overrule findings も照合される(f11)** — `anchor_confirm` の overrule に stale finding を混ぜ、
  merge 前に棄却され union に残らないこと、anchor index で `anchor_checked` が出ること。
  (m) **pending mirror 除外(f13)** — `anchor_verified=Some(false)` の open entry が `mirror_open_to_pending` の
  結果 `self_review_pending` に含まれないこと(open で actionable な entry は従来どおり含まれること)。
- **unit(stats.rs)**: (h) Σ`findings_total`>0 で率が出る。(i) **Σ`findings_total`=0 → ゼロ除算せず N/A 表示**。
- **unit(flow.rs)**: (j) `Lane.reuse_session` が role で正しく分岐。(k) reviewer lane は spawn 前に生存 pane を
  release し、resume 引数なしで spawn する(pane・direct 両モードで、前ターン session に接続しないこと)。
  (n) **`<details>` 描画(f12)** — `anchor_verified=Some(false)` の open entry を持つ checkpoint で
  `self_review_details_with_outcome` が「anchor 未照合の open」行を出すこと。
- **統合1(#231 再現・pr-reviewer / B)**: fake_agent が resume 前提で旧 head の stale finding を再主張する
  シナリオを組み、fresh session 既定で pr-reviewer が現 head を読み直し clean → spec-ready 昇格、
  needs-human に落ちないことを検証。
- **統合2(内部 self-review / A)**: 1周目で「現物に無い引用」の finding を出す fake_agent →
  meguri が stale として1回差し戻し → 2周目クリーン → 収束・publish、needs-human ゼロ。
- 既存の self-review / fresh でない author lane resume のテストが緑のままであること(回帰なし)。

## 実装しないこと

- pr-reviewer の finding 構造化・anchor 照合(将来 follow-up、ADR 0028 スコープ外)。
- P1/P2/P4/P5/P6 系(設計書 #241 の別 issue)。
