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
   なお stale なら新規は棄却・既存 id の再リストは `Waived`(anchor 失敗)にして、needs-human に落とさない。
2. **#231 の実ケースを fixture 化**(下記テスト戦略)。旧 head の引用を **同じ id で再主張**する経路も塞ぐ。
3. reviewer ロールのターンが保存済み session id を resume に使わない。author/fixer 系は従来どおり resume。
4. `defect` finding は **新規・再リスト双方**を照合対象にする(`decision` 型のみ免除)。
5. stale 率が `meguri stats review` に単一定義で出る(`anchor_checked` イベントのみを母集団に、terminal
   event の有無に依存しない)。分母 M=0 は N/A 表示。表示整形は `src/app.rs` の `cmd_stats_review`。
6. 旧 checkpoint は serde default で読め、`anchor_verification = false` のとき checkpoint は
   byte-for-byte 不変(追加フィールドは `Option` + `skip_serializing_if`)。
7. anchor の `path` が worktree 外(絶対パス・`..`・symlink 越え)を指す finding は stale に倒し、
   worktree 外のファイルを読まない・プロンプトへ流さない。
8. anchor confirmation の overrule findings も照合される(照合を素通りする経路を残さない)。
9. `anchor_verified=Some(false)` の entry は publish 時(clean / cap→final-fix いずれでも)PR `<details>` に表示。
10. `anchor_verified=Some(false)` の entry は `status=Waived` なので、**#247 以降・以前を問わず ledger-aware な
    binary は fixer に渡さない**(rollback しても ping-pong を再発しない)。
11. system-waive(`anchor_verified=Some(false)`)は author-waive と区別され、`waive_rate` 等「作者が拒否」を
    意味する consumer から除外される(ADR 0028 が 0022/0026 を精緻化)。
12. 「初回 stale → 差し戻し後 clean」でも stale 率が 0% にならない(全 attempt 通算で stale を1回計上)。

## 触るファイル

- `src/engine/self_review.rs` — `Finding` に `quote: Option<String>`、`LedgerEntry` に
  `anchor_verified: Option<bool>` を追加(ともに `#[serde(default, skip_serializing_if = "Option::is_none")]`)。
  照合対象は **すべての `defect` finding(新規 + 再リスト)**。sequential 経路(`review_turn`)の verify 段に、
  tree/id とは**別カウンタ**の anchor stale リトライ(1回)を追加。差し戻し後もなお stale なら
  **新規は台帳から棄却・再リストは `status=Waived` + `anchor_verified=Some(false)` + システム `waive_reason`** に落とす。
  Waived は `fix_turn` が拾う `status==Open` 集合から自然に外れるので、fixer 対象外・rollback 安全が status 一本で
  担保される(actionable set の特別扱いは不要)。この waive は omission 経路ではなく **anchor 失敗を理由に明示設定**。
  `round1_parallel_review` は **merge 前に reviewer 別に照合**し stale を棄却してから union-merge(f8)。
  `anchor_confirm` の overrule findings(`merge_reviews` の `extra`)も **merge 前に anchor turn 内で照合**し、
  専用の anchor index で `anchor_checked` を emit(f11)。`mirror_open_to_pending` は `status==Open` のみ写すので
  Waived 化した stale 再リストは自動的に pending から外れる(旧 binary への漏れなし)。
  `self_review.anchor_checked` イベント新設(照合を走らせた reviewer ターンにつき1回)。
  review プロンプト(`review_prompt` 系)に `quote` 必須・再リストも再照合・照合ルールを明記。
- `src/gitops.rs` — HEAD の tracked blob を repo-relative path で読むヘルパ(`git show HEAD:<path>` 相当)と、
  worktree 内拘束の path 正規化(絶対/`..`/symlink 越えを弾く)。git 操作は gitops に集約する規約に従う。
- `src/engine/flow.rs` — `Lane` に `reuse_session: bool`。`author_lane`(role で分岐)/
  `self_review_lane_for`(常に false)で設定。reviewer ターンは spawn 前に lane の生存 pane を release/kill
  してから resume 引数なしで素の spawn を行う(`ensure_pane` の adopt を回避)。`spawn_direct_process` も
  `reuse_session == false` のとき session id を `--resume` に読まない。session id の**保存**は継続。
  `self_review_details_with_outcome`(PR body の `<details>` renderer)に、`anchor_verified=Some(false)` の
  entry を「anchor 未照合(照合失敗)」として1行出す描画を追加(f12。現行は round ごとの件数しか出さない)。
- `src/config.rs` — `[review]` に `anchor_verification`(既定 true)のトグル。rollback レバー。
- `src/store/stats.rs` — **既存 `review_stats`(terminal event 依存・phases=0 で母集団から drop)に相乗り
  させない**。`self_review.anchor_checked` **だけ**を読む独立ロールアップを新設(新 struct `AnchorStatRow`
  相当。key は既存と同じ project/loop_kind/authoring profile)。terminal event が無い run でも `anchor_checked`
  を数え、Σ`stale_count` / Σ`findings_total` と check 件数(coverage)を集計。**Σ`findings_total`=0 は
  N/A(ゼロ除算回避)**。`reviewer_index` は payload に残すが Phase1 の CLI ロールアップは authoring group 単位。
- `src/app.rs` — `cmd_stats_review`(整形の実体はここ、stats.rs ではない)に anchor 統計セクションを追加:
  列は `CHECKS`(照合ターン数)/`FINDINGS`(Σfindings_total)/`STALE`(Σstale_count)/`STALE%`(M=0 は `N/A`)。
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
   - **再リスト**(既存 id)→ entry を **`status=Waived`** に落とし `anchor_verified=Some(false)` + システム
     `waive_reason`(「anchor 照合失敗: 現 head に該当引用なし」)を付ける。`Open` のままにはしない — 理由は
     rollback 安全性(f-259-1): `Open` だと `self_review_ledger` を正として読む #247 前(ADR 0022 以降)の
     ledger-aware binary が `fix_turn` にそのまま渡し ping-pong を再発させる。`Waived` はどの ledger-aware
     binary も非 actionable と解釈する。omission 経路ではなく anchor 失敗を理由に**明示 waive**するので f4 とも
     衝突しない。Waived は open 数から外れるので phase は普通に収束でき needs-human に落ちない。publish 時に
     PR `<details>` へ「anchor 未照合(照合失敗)」として出す(透明性、鍵は `anchor_verified=Some(false)`)。
4. **stale の扱い(round 1 parallel + anchor overrule)**: **merge 前に reviewer 別に照合**する(f8)。
   union-merge 後だと reviewer 境界が消え `reviewer_index` 帰属と `findings_total`/`stale_count` の分割が
   できない。各 `self-review#N` が自分の findings を照合して stale を棄却してから union-merge(全 finding が
   新規なので棄却で済む)。verified な他 reviewer の finding は影響を受けない。reviewer 別 corrective-turn retry
   は持たない。**anchor confirmation の overrule findings(`merge_reviews` の `extra`)も同様に merge 前に
   anchor turn 内で照合**し、専用の anchor index で `anchor_checked` を emit(f11。ここを外すと overrule 経路が
   照合を素通りする)。
5. **anchor_verified は `status` を補助する表示/統計フラグ**(f14 + f-259-1): `LedgerEntry.anchor_verified:
   Option<bool>`(`skip_serializing_if = "Option::is_none"`)。3値 — `None`(照合なし: 無効時・decision 免除)/
   `Some(true)`(通過)/`Some(false)`(再リストが照合失敗 → 同時に `status=Waived`)。**actionability は
   `status` 一本**が担う(Waived は fixer が拾う `Open` 集合から自然に外れる)。`anchor_verified` はそれ自体で
   actionability を変えず、(a) 表示(`<details>` の鍵)、(b) stats(stale 識別)に効く。境界:
   **open/fixed/waived の遷移は `status`、`anchor_verified` は「その waive が anchor 失敗由来か」の由来 + 表示/統計**。
   これで f14 の二枚舌を解き、actionability を status に集約して rollback 安全も同時に満たす。fix file 検証や
   pending mirror の特別扱いは不要(Waived が既に非 Open)。stale 率は台帳ではなくイベントから導出(§observability)。
   **`Waived` は今後 author-waive(ADR 0022、`anchor_verified=None`)と system-waive(anchor 失敗、
   `anchor_verified=Some(false)`)の2種を含む(f-259b-1)。ADR 0028 が 0022/0026 の `waived` 意味論を精緻化する。
   「作者が拒否した」を意味する全 consumer は `anchor_verified=Some(false)` を除外**すること。ADR 0026 の
   `waive_rate`(本 issue では未実装・将来 phase)は author-waive のみを数える。system-waive は `Fixed` に
   しないので ADR 0026 の捕捉(numerator=fixed)は汚さない。詳細は ADR 0028 §2/§4。
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
- **stale 第3状態の rollback(f13 + f-259-1)**: stale 再リストは `status=Waived` にするので、rollback 安全は
  status 一本で担保される。(1) **ledger-aware な #247 前(ADR 0022 以降)binary** は `self_review_ledger` を正として
  読み、`fix_turn` が拾うのは `status==Open` のみ → Waived の stale entry は渡らず ping-pong を再発しない
  (未知の `anchor_verified` を serde で無視しても安全)。(2) **pre-#212 binary** は `self_review_pending` を読むが、
  `mirror_open_to_pending` は `status==Open` だけ写すので Waived は元から入らない。以前の設計は entry を Open のまま
  残し pending 除外だけで守ろうとしたが、それでは (1) の ledger-aware binary を守れない(pr-review 指摘)ため、
  status を Waived に落とす方式へ変更した。
- **rollback 時の統計劣化は許容(f-259b-1)**: 旧 binary が `status` だけを見ると system-waive を author-waive と
  数え得る(`anchor_verified` を読めない)。だがこれは実行時契約(ping-pong 再発なし・tree 検証)には触れず、
  `waive_rate` 等の統計が run 完了までわずかに過大になるだけの一時的劣化。numerator=fixed(ADR 0026)は
  system-waive を含まないので捕捉数は汚れない。新 binary は `anchor_verified` で正しく除外する。
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
  **1回だけ** emit(イベントは二重に出さない)。**発火単位を照合単位に一致**させ、parallel は各 `self-review#N`
  が merge 前に照合するので reviewer ごと・`reviewer_index` 付き、sequential は round ごと。
  payload = `{ round, reviewer_index, findings_total, stale_count }`。
- **カウントはそのターンの全 attempt を通算する(f-259b-2)**: `findings_total` は照合した `defect` finding の
  延べ数(初回 + anchor 差し戻し後の再試行を合算)、`stale_count` は失敗の延べ数(新規棄却 + 再リスト Waived 化)。
  こうしないと「初回 stale → 再試行 clean」が最終試行だけ数えられ `stale_count=0`(0%)になり、stale 出力が
  定義から漏れる。通算なら findings_total=2, stale_count=1(= stale を1回計上)。各 attempt の stale ⊆ findings
  なので `stale_count ≤ findings_total`、率は [0,1]。差し戻しの無い parallel/anchor_confirm は attempt 1回分。
- **stale 率 = Σ`stale_count` / Σ`findings_total`**。**集計は `anchor_checked` イベントのみを母集団にし、
  terminal event に依存しない(f-259-2)。** 既存 `review_stats` は terminal event だけを読み phases=0 の
  グループを落とすため、anchor 照合後に pane 停止などで terminal が出なかった run の `anchor_checked` が
  coverage から消える。これを避け、anchor 統計は `src/store/stats.rs` に**独立ロールアップ**(`AnchorStatRow` 相当)
  として新設し、`src/app.rs` の `cmd_stats_review` に別セクションで出す。**Σ`findings_total`=0 は ゼロ除算を避け
  `N/A` 表示**(0.0% ではない)。`findings_total=0` でもイベントは emit し coverage(照合ターン数 CHECKS)を数える。
- **透明性(f12)**: `anchor_verified=Some(false)` の entry(= Waived)は、PR body の self-review `<details>` に
  「anchor 未照合(照合失敗)」として1行出す(`self_review_details_with_outcome` を拡張、鍵は status ではなく
  `anchor_verified`)。publish 時(clean / cap→final-fix いずれでも)に出るので、human merge gate が
  「照合できなかった指摘がある」ことを見られる。
- fresh session は既存の `pane.resume_failed` とは別で、resume を試みない経路なのでイベント追加は不要
  (`direct.spawned` / spawn の `resumed:false`、および reviewer lane の pane release で観測できる)。

## テスト戦略

- **unit(self_review.rs)**: (a) quote が HEAD blob に在る → verified で台帳へ。(b) 新規 quote 不在 → 1回差し戻し。
  (c) 差し戻し後もう一度 stale(新規) → 棄却され台帳に入らず、verified 分だけ残る。(d) `decision` は quote 無しで通る。
  (e) **既存 id の再リストが現 head で照合失敗 → 差し戻し後もなお stale なら `status=Waived` +
  `anchor_verified=Some(false)` + system waive_reason**(omission でなく明示 waive、ping-pong→needs-human に至らない)。
  (e2) 再リストが現 head の quote で照合を通れば open 継続、fix でコードが消え reviewer が drop すれば解消。
  (f) round 1 parallel: **merge 前に reviewer 別照合** — 1 reviewer が stale、他が verified → stale だけ棄却、
  verified は union に入り `reviewer_index` が保たれる。(g) `path` が絶対/`..`/symlink で worktree 外 → stale、外を読まない。
  (l) **anchor overrule findings も照合される(f11)** — `anchor_confirm` の overrule に stale finding を混ぜ、
  merge 前に棄却され union に残らないこと、anchor index で `anchor_checked` が出ること。
  (m) **rollback 安全(f-259-1)** — Waived 化した stale 再リストが `fix_turn` の `status==Open` 抽出に入らないこと、
  かつ `mirror_open_to_pending`(Open のみ)にも入らないこと。
  (q) **system-waive の由来区別(f-259b-1)** — system-waive(`anchor_verified=Some(false)`)と author-waive
  (`anchor_verified=None`)がともに `status=Waived` でも、由来で分けられること(将来 `waive_rate` が前者を除外できる形)。
- **unit(stats.rs)**: (h) Σ`findings_total`>0 で率が出る。(i) **Σ`findings_total`=0 → ゼロ除算せず N/A**。
  (o) **terminal event なしでも計上(f-259-2)** — `anchor_checked` は出たが terminal event が無い run でも、
  独立ロールアップの CHECKS/FINDINGS/STALE に反映される(既存 `review_stats` の phases=0 drop に飲まれない)。
  (r) **初回 stale → 再試行 clean(f-259b-2)** — 全 attempt 通算で `anchor_checked` が `findings_total=2,
  stale_count=1` になり、stale 率が 0% にならないこと。
- **unit(app.rs)**: (p) `cmd_stats_review` の anchor セクションが CHECKS/FINDINGS/STALE/STALE% を出し、M=0 で `N/A`。
- **unit(flow.rs)**: (j) `Lane.reuse_session` が role で正しく分岐。(k) reviewer lane は spawn 前に生存 pane を
  release し、resume 引数なしで spawn する(pane・direct 両モードで、前ターン session に接続しないこと)。
  (n) **`<details>` 描画(f12)** — `anchor_verified=Some(false)` の entry を持つ checkpoint で
  `self_review_details_with_outcome` が「anchor 未照合(照合失敗)」行を出すこと。
- **統合1(#231 再現・pr-reviewer / B)**: fake_agent が resume 前提で旧 head の stale finding を再主張する
  シナリオを組み、fresh session 既定で pr-reviewer が現 head を読み直し clean → spec-ready 昇格、
  needs-human に落ちないことを検証。
- **統合2(内部 self-review / A)**: 1周目で「現物に無い引用」の finding を出す fake_agent →
  meguri が stale として1回差し戻し → 2周目クリーン → 収束・publish、needs-human ゼロ。
- 既存の self-review / fresh でない author lane resume のテストが緑のままであること(回帰なし)。

## 実装しないこと

- pr-reviewer の finding 構造化・anchor 照合(将来 follow-up、ADR 0028 スコープ外)。
- P1/P2/P4/P5/P6 系(設計書 #241 の別 issue)。
