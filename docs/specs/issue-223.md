# issue-223 spec — workqueue 導入 + fixer 3 兄弟を Issue Kind の arm に畳む(ADR 0012 スライス3/5)

ADR 0012(level-triggered reconciler)の**スライス3**。S1(#221)で `merge_tail` が
「observe → next_step → act」の型を敷き、`next_step` の中に **意図的な空席**を2つ残した:

```rust
if m.mergeable == Conflicting || m.status == Dirty { return Step::Skip("conflict-resolver owns it"); }
if m.status == Blocked && s.rollup_failure          { return Step::Skip("ci-fixer owns it"); }
```

このスライスの芯は一行で書ける。**この2つの `Skip` を本物の `Step::Agent` の arm に変え、
未解決レビュースレッドの arm を1本足して、`fixer` / `ci_fixer` / `conflict_resolver` の
3 loop を「1つの `next_step` が所有する arm」へ畳む。** そのうえで dispatch を
**workqueue + resync** に載せ替え、ADR 0014(signal binding / step policy)と
ADR 0015(claim identity / no-steal)の芯を同じ PR で land する。

## なぜ design spec か(深さの理由)

未決事項が多く(所有 arm の優先順・claim の担体・backoff の永続先・policy フィルタの形)、
波及も広い(公開型 `Step`/`Snapshot` の拡張・dispatch 経路の差し替え・claim 権威の移動)。
さらに **永続 sqlite 状態(backoff テーブル)と claim 権威という「契約」を触る**ため、veto ルールで
migration & rollback セクションが必須。よって normal ではなく design tier で書く。

> 番号の訂正: issue は合流 ADR を「0014 / 0015」と呼ぶが、その番号は既存の別 ADR
> (`0014-plan-review-...` / `0015-repo-side-reads-...`)が占めている。ADR は「次の空き番号」規約
> (`.claude/rules/docs.md`)なので、本スライスの実装時に書く 2 本は **ADR 0026(signal binding /
> step policy)** と **ADR 0027(claim identity / no-steal)** になる。backoff 用 migration の空き番号は
> **0016**。

## これは1本の spec か(分解しない判断)

分解提案にはしない。ADR 0012 は「各スライス = 独立に review・rollback できる 1 PR」を rollback の
単位と定め、0014 / 0015 を**このスライスに合流させよ**と明示している。3 者は実際に相互依存する:
arm は `next_step` そのもの、workqueue はその arm を配る器、claim marker(0015)は workqueue の
排他、signal binding(0014)は `next_step` が入力を読む担体。バラすと中途半端な中間状態が生まれる。
受け入れの芯も「3 arm + signal binding + step policy + claim marker の property test が**揃って**緑」を
要求する。よって1本の design spec とし、レビュー容易性は「実装をコミット順に薄く切る」(§実装順)で担保する。

---

## アーキテクチャ影響(architecture impact)

### 1. `merge_tail.rs` → `issue_reconciler.rs` に成長させる(新設ではない)

S1 の `src/engine/merge_tail.rs` を `src/engine/issue_reconciler.rs` に**改名**し、その中の
`Snapshot` / `next_step` / `Step` を拡張する。新しい第2の `next_step` を作らない —— それでは
「ちょうど1つの所有 arm」が2つの関数に割れ、ADR が殺したはずの所有の曖昧さが戻る。**PR 側の
Issue Kind は1つの `next_step` が全状態を所有する**、が不変条件。

- `Step` に `Agent(Arm)` を追加。`Arm ∈ { Fixer, CiFixer, ConflictResolver }`。
  既存の `Op` / `Wait` / `Skip` はそのまま。
- `Snapshot` に fixer 症状を足す —— **すべて既に `PrObservation` にある**(forge に新フィールド不要):
  - `conflicting`(`merge.mergeable == Conflicting || status == Dirty`)
  - `ci_failure`(`rollup` から meguri 自身の status を除いた `state() == Failure`)
  - `awaits_fixer_thread`(`review_threads` に `thread_awaits_fixer` が1つでもある)
  - arm ごとの予算(`succeeded_run_count` を build 時に store から読む。fixer の `opted_in` と同型の
    「唯一の追加 I/O」)と claim marker の観測値。
- S1 の 2 つの placeholder `Skip` を `Step::Agent(Arm::ConflictResolver)` /
  `Step::Agent(Arm::CiFixer)` に置換。arm regime 側(未 arm)ではなく watch regime 側にある
  この分岐が、まさに「fixer 家族に委譲する」判断だった。加えて未解決スレッド →
  `Step::Agent(Arm::Fixer)` を足す。

### 2. 3 fixer loop は「recipe」だけ残し、`Loop`/`discover` を外す

`fixer.rs` / `ci_fixer.rs` / `conflict_resolver.rs` は**削除しない**。重い agent フロー
(`Flavor` 実装 + `run_fixer` / `run_ci_fixer` / `run_conflict_resolver`)は arm の実体
(recipe)として残す。外すのは `impl Loop` と `discover` だけ —— discover は `issue_reconciler` の
resync が担う。`act(Step::Agent(arm))` が対応する `run_*` を run に載せる。予算定数
(`MAX_CI_FIX_RUNS` / `MAX_RESOLVE_RUNS`)は arm の判断(§4 の escalate 条件)へ移す。

### 3. dispatch = workqueue + resync

ADR 0012 決定6の 3 キューを、**新しい器を作らず既存基盤へ写像**する(最小差分):

| 概念キュー | 実体 | 正しさの源 |
|---|---|---|
| **activeQ**(優先度 = マージ近接) | `queued` run 群を**優先度順**に dispatch | resync が毎 tick 再構築 |
| **backoffQ**(指数バックオフ) | sqlite テーブル `reconciler_backoff`(§migration) | 発火時刻・attempt(forge から復元不可) |
| **parked**(人間待ち) | `Wait(_)` = run を作らない + forge の `hold`/`needs-human` | forge ラベル(spec 軸の権威) |

resync = poll 相乗りの `issue_reconciler::sweep`(S1 の `merge_tail::sweep` を継ぐ)。毎 tick:

1. **observe**: `observe_open_prs`(S1 の `observe_merge_tail` を改名)一括クエリ。cost を emit。
2. **decide**: PR ごとに `Snapshot` を作り `next_step`。
3. **act**:
   - `Op` / `Wait` / `Skip` は S1 同様その場で処理(Op は API 操作、Wait/Skip はログのみ)。
   - `Agent(arm)` は **workqueue へ enqueue**: (a) backoff テーブルを見てまだ見えなければ skip、
     (b) claim marker が**他 instance**のものなら skip(no-steal)、(c) `queued` run を作る
     (active-run unique index が二重を弾く)。

`default_loops()` から 3 fixer を外す。planner / worker などは S4 まで従来の `Loop` のまま**併存**。
dispatch の解決は `run.loop_kind` 経由なので、3 fixer の `run_*` は `self.loops` に登録を残す
(discover が空になっただけ)—— これで scheduler の weight / redispatch / slot 予算機構をそのまま使える。

### 4. arm の判断と escalate/backoff の対応(ADR 0007 supersede の完成)

ADR 0007 の分類が全部行き先を持つ(これが S1 の Skip を埋める意味):

| 観測 | Step | キュー |
|---|---|---|
| Conflicting | `Agent(ConflictResolver)`(予算内)/ `Op(Escalate)`(予算超過で再衝突) | active / parked |
| Blocked かつ required check 失敗 | `Agent(CiFixer)`(予算内)/ `Op(Escalate)`(予算超過で赤) | active / parked |
| 未解決スレッド(人/外部 bot) | `Agent(Fixer)` | active |
| Behind(arm 済み) | `Op(UpdateBranch)`(S1) | — |
| Blocked・非 Behind・stale | `Op(Escalate)`(S1 の Stuck backstop) | parked |
| transient(`merge == None` 等)/ 一時的に未整定 | `Wait`→ 次 resync、明示遅延なら backoff | backoff |
| `hold` / `needs-human` | `Wait`/`Skip`(human stop) | parked |

予算(`MAX_*_RUNS`)超過は「これ以上自動で回さず人間へ」= parked への昇格。conflict_resolver の
#176 の順序修正(まだ conflicting か**先に**確認してから予算判断)は arm の中でも維持する。

### 5. 優先度関数 = マージ近接(ADR 0001 の移設先)

ADR 0001 の「登録順が優先度」を activeQ の優先度関数へ移す。`default_loops()` の登録順
(conflict > ci > fixer =「merge に近い順」)が既にマージ近接そのものなので、優先度キーは:

1. arm クラス順(ConflictResolver=0 < CiFixer=1 < Fixer=2 < 新規 worker 系。conflict は merge の
   最後の障害物なので最優先)、
2. 同クラス内は issue 番号昇順(FIFO)。

これを `queued` run の dispatch 順に効かせる(現状の `list_runs` 順 → 優先度キー順に変更)。
純粋・全順序・決定的であること(property test で担保)。

### 6. signal binding / step policy(ADR 0026 の芯 = 部分導入)

2 つを部分導入する。「seam を入れる」のが本スライスの成果物で、両担体を実装するのではない。

- **signal carrier seam**: `Snapshot` を作るとき、spec 軸(phase / `hold` / `needs-human`)と status 軸を
  読む口を `SignalCarrier` トレイト越しにする。本スライスは **`Labels` 担体1つだけ**を実装
  (= 今日の挙動を seam の裏に写すのみ)。`Markers` 担体は enum の空席として宣言するが未実装。
  既定束縛 = `Labels`。property は「`Labels` 担体経由の `Snapshot` == 直読みの baseline」
  (seam が挙動保存)。
- **step policy allow-filter**: `next_step` が返した生の `Step` を、純関数
  `apply_policy(step, &Policy) -> Step` に通す。不許可 arm の `Agent` は `Wait(PolicyDisabled)` に
  なる。これが散らばった per-loop kill switch(`review.impl_enabled` 等)を**一枚の後段フィルタ**に
  統一する。config `[reconciler.policy]` の allow-set が担体。property は「無効 arm は決して
  `Agent` を返さず常に `Wait(PolicyDisabled)`、かつ所有の全域性は保たれる」。

### 7. claim identity / no-steal(ADR 0027 の芯)

- claim の真実を **instance 名入りマーカーコメント**にする:
  `<!-- meguri:claim instance=<id> run=<run_id> -->`。head 非依存(1 work-item の claim は
  head 移動をまたぐ)。`observe_open_prs` が拾う `comments` から読む。
- `meguri:working` は**表示用射影に格下げ**。人間向けに従来どおり付け外しするが、権威ではない。
  `pr_is_touchable` の「working あり = claim 済み skip」判定を、**マーカー**判定へ差し替える。
- dispatch の claim 排他 = no-steal: enqueue 前に、観測コメントに**他 instance**の claim マーカーが
  あれば skip。自分 or 無しなら進む。release / complete でマーカーを消す。
- instance id: github 側は net-new(今日は label のみ・owner 無し)。既定 = `mux.session`
  (既定 `"meguri"`)、`[reconciler] instance = "..."` で上書き可。

> 現実的射程の注記: 今日は単一 instance 運用なので真の並行 claim は起きない。no-steal は
> Phase-4 の複数ホスト(`tasks.claimed_by` の github 版)への**前準備**であり、単機では
> 「マーカーが claim の権威、label は表示」という権威分離の意味だけが効く。

---

## 検討した代替案と決定(alternatives & decision)

- **A: `next_step` を2本(merge 用と fixer 用)に分ける** → 却下。所有が2関数に割れ、
  「ちょうど1つの arm」property が書けない。ADR が殺した曖昧さの再来。
- **B: workqueue を run テーブルと別の永続キューとして新設** → 却下。weight / redispatch /
  slot 予算 / crash recovery を二重持ちになる。activeQ = 優先度順の `queued` run、backoff = 小さな
  enqueue ゲート、parked = Wait、で既存基盤に写像すれば差分が最小で、resync の再構築性(正しさ)も自然。
- **C: parked も sqlite に持つ(ADR 0012 決定2 の字面どおり)** → **精緻化して却下**。parked の
  membership は `Wait` verdict であり、forge の `hold`/`needs-human` から毎 resync 再導出できる。
  タイマーも持たない。永続化すると forge ラベルと食い違う**第2の権威**が生まれる。**backoff の
  発火時刻・attempt だけが forge 非復元**なので、sqlite に置くのは backoff のみとする。これは
  ADR 0012 決定6 の一段の精緻化として ADR 0027(または実装時の短い追記)に記録する。
- **D: claim を label のまま atomic 化** → 却下。label には owner を載せられず no-steal を表現できない。
  0015 の要求(claim の真実 = instance 名入り)を満たすにはマーカーが要る。

---

## migration & rollback(必須 — 永続状態と契約を触る)

**追加のみ・破壊なし。**

- **schema**: migration `0016_reconciler_backoff.sql` を新設。
  `reconciler_backoff(project_id TEXT, item_key INTEGER, arm TEXT, attempt INTEGER,
  next_visible_at TEXT, PRIMARY KEY(project_id, item_key, arm))`。`schedules.rs` に倣った
  アクセサ `src/store/reconciler.rs`(read / advance / clear)。既存テーブルは無改変。
- **claim marker**: 追加コメント。`working` label は射影として付け外しを続けるので、旧バイナリ・
  人間の目には後方互換。
- **前進(forward)**: activeQ / parked は**再導出**なので状態移行データは不要。初回 resync が
  observe から全部組み直す(level-triggered の利点)。backoff テーブルは空から始まって自然に埋まる。
- **rollback**(この PR を revert): 3 loop の `discover` が戻り、`reconciler_backoff` は**孤児化
  するだけ**(誰も読まない・害なし)。claim マーカーは無害なコメントとして残る(cleaner が拾うか
  無視)。`working` label は付いたままなので旧 `pr_is_touchable` がそのまま claim として尊重 ——
  **二重 claim は起きない**(単一 instance 前提。§7 の射程注記どおり)。
- **段階順序**(ADR 0012 のスライス順の趣旨):forge 権威(spec 軸ラベル)には触れない。触るのは
  status 軸の射影化(`working`)と、local な実行進行(backoff)のみ。

## observability

- 既存 `merge_tail.observe_cost` を `reconciler.observe_cost` に継ぐ(requests / graphql_cost / prs)。
- 新規 emit: `reconciler.enqueued`(arm, issue)/ `reconciler.parked`(reason)/
  `reconciler.backoff_scheduled`(arm, attempt, next_visible_at)/ `reconciler.policy_disabled`(arm)/
  `pr.claimed`(instance を含める)/ `reconciler.claim_skipped`(他 instance 検出 = no-steal)。
- activeQ 深さ・backoff 深さは resync ごとに1本のゲージ emit(dispatch の可観測性)。

## test strategy(受け入れの芯はここ)

純関数 `next_step` + `apply_policy` + 優先度キーへの property test が中核。

1. **所有の全域性(芯1・芯2)**: PR 側状態空間(armed × mergeStateStatus × mergeable ×
   conflicting × ci_failure × threads × 予算 × claim × policy)を網羅し、`next_step` が常に
   ちょうど1つの `Step` を返す/所有の欠落も二重所有も無いことを assert。BEHIND は
   `Op(UpdateBranch)`、Conflicting は `Agent(ConflictResolver)`、赤 CI は `Agent(CiFixer)`、
   未解決スレッドは `Agent(Fixer)` が**必ず**所有。S1 の `ownership_is_total_no_gap_no_double` を拡張。
2. **signal binding(芯3)**: `Labels` 担体経由で作った `Snapshot` が直読み baseline と全ラベル集合で
   一致(seam が挙動保存)。
3. **step policy(芯3)**: 全 snapshot × policy で、無効 arm は `Agent` を返さず `Wait(PolicyDisabled)`。
   フィルタ後も所有の全域性が保たれる。
4. **claim / no-steal(芯3)**: 他 instance のマーカーがある観測は決して dispatch されない(skip)、
   自分 or 無しは dispatch 可。FakeForge にマーカー入りコメントを積んで連結検証。
5. **backoff**: `RequeueAfter` が `next_visible_at` を指数(attempt 反映)で置く、due 前は不可視、
   sqlite なので restart をまたいで生存。
6. **優先度順 dispatch**: `queued` run が conflict > ci > fixer、同クラスは issue 番号昇順で出る。
7. **非回帰**: 既存 `tests/*fixer*` の discovery テストは reconciler の arm 判定へ書き換え。
   `scheduler_test.rs` / `merge_tail` の property は破壊しない。統合テスト
   (`tests/fixtures/fake_agent.sh`)で fixer arm が実 tmux / 実 worktree で回ることを確認。

---

## 触るファイル

- `src/engine/merge_tail.rs` → `src/engine/issue_reconciler.rs`(改名 + `Snapshot`/`Step` 拡張、
  placeholder Skip → Agent arm、Fixer arm 追加、`SignalCarrier` seam、`apply_policy`、
  claim マーカー排他、優先度キー)
- `src/engine/fixer.rs` / `ci_fixer.rs` / `conflict_resolver.rs`(`impl Loop`/`discover` 撤去、
  `Flavor` + `run_*` は arm recipe として残す、予算定数を arm 判断へ)
- `src/engine/mod.rs`(`default_loops()` から 3 fixer を外す・dispatch 解決用の登録は残す、
  `pr_is_touchable` の claim 判定を marker へ)
- `src/engine/scheduler.rs`(`queued` run を優先度順に dispatch、resync sweep 呼び出しを継ぐ)
- `src/store/migrations/0016_reconciler_backoff.sql`(新規)+ `src/store/reconciler.rs`(アクセサ新規)
- `src/config.rs`(`[reconciler]` = `ReconcilerConfig`: step policy allow-set、backoff base/cap、
  carrier 束縛(既定 labels)、instance 名)
- `src/forge/mod.rs`(`observe_merge_tail` → `observe_open_prs` 改名。新フィールド不要。gh/fake も追随)
- `README.md` / `README.ja.md`(dispatch = workqueue + resync、fixer は arm、claim marker の一段)
- `tests/`(property test 拡張、claim no-steal / backoff の FakeForge 連結、既存 fixer テスト書換)
- 実装時に新規 ADR: **0026**(signal binding / step policy)/ **0027**(claim identity / no-steal、
  parked 非永続の精緻化を含む)

## 受け入れ基準(acceptance criteria)

1. `fixer` / `ci_fixer` / `conflict_resolver` が `issue_reconciler::next_step` の `Agent` arm 化され、
   3 loop の `discover`/`impl Loop` が消え、`default_loops()` から外れている。
2. **「全状態にちょうど1つの所有 arm」property test が緑**。BEHIND を含め、所有の欠落・二重所有を
   property test が検出する(S1 の property を fixer arm まで拡張)。
3. Conflicting → `Agent(ConflictResolver)`、赤 CI(required)→ `Agent(CiFixer)`、未解決スレッド →
   `Agent(Fixer)`。予算超過はいずれも parked(needs-human)へ昇格。
4. dispatch が workqueue + resync で動く: `queued` run が優先度順(マージ近接)に出る、backoff が
   sqlite で restart をまたいで生存、parked は run を作らない。
5. signal binding: `Labels` 担体 seam が入り、seam 経由の `Snapshot` が baseline と一致する
   property test が緑。
6. step policy: `apply_policy` が入り、無効 arm が `Wait(PolicyDisabled)` になる property test が緑。
7. claim marker: claim の権威が instance 名入りマーカーに移り、他 instance の claim を dispatch しない
   (no-steal)property test が緑。`meguri:working` は表示射影として付け外しされる。
8. 既存テスト(特に `merge_tail` property / `scheduler_test.rs` / 統合テスト)が全て緑。
   `cargo fmt` / `clippy -D warnings` / `nextest` / `test --doc` が通る。

## スコープ外(S4 以降)

- planner / worker / spec_worker / guard / pr_reviewer と cleaner / triage / routing_drift の Issue/Repo
  Kind 吸収、旧 `Loop` trait の撤去、body-edit `reconcile` → `reconcile_body_edits` 退避、
  `reaper` → `Op(Finalize)`、`ensure_project_clone` → `Op(EnsureClone)`(全て S4)。
- `SignalCarrier` の `Markers` 担体の実装(seam のみ本スライス、担体の中身は将来)。
- config 键粒度の整理(ADR 0013、S5)。
- 真の複数 instance 並行(no-steal は前準備。Phase-4 で活きる)。

## 実装順(1 PR 内の薄いコミット列 — レビュー容易性の担保)

1. `merge_tail.rs` → `issue_reconciler.rs` 改名 + `observe_open_prs` 改名(挙動不変・機械的)。
2. `Step::Agent(Arm)` 追加、placeholder Skip → Agent arm、Fixer arm、予算移設 + **所有 property 拡張**
   (芯1/2 がここで緑)。
3. workqueue 写像: 優先度順 dispatch + `reconciler_backoff` migration/アクセサ + backoff ゲート。
4. `SignalCarrier` seam + `apply_policy`(芯3 の signal binding / step policy)。
5. claim マーカー + no-steal(芯3 の claim)、`working` 射影化、`pr_is_touchable` 差し替え。
6. `default_loops()` 整理、README、既存テスト書換。
