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
- `Snapshot` に fixer 症状を足す。大半は既に `PrObservation` にあるが、**2 つは bulk observe の
  拡張が要る**(§1.5 で確定 — 当初の「forge に新フィールド不要」は撤回する):
  - `conflicting`(`merge.mergeable == Conflicting || status == Dirty`)—— 既存フィールドで足りる。
  - `ci_failure`(`rollup` から meguri 自身の status を除いた `state() == Failure`)—— 既存で足りる。
  - `awaits_fixer_thread`(`review_threads` に `thread_awaits_fixer` が1つでもある)—— **要拡張**。
    bulk observe の `ReviewThread.comments` は今 常に空(`gh.rs:477` が `Vec::new()`)なので
    `thread_awaits_fixer` の「最終 comment が fixer の返信マーカーでない」判定ができない(§1.5)。
  - claim marker の観測値 —— **要拡張**(§1.5 / §7)。
  - arm ごとの予算(`succeeded_run_count` を build 時に store から読む。fixer の `opted_in` と同型の
    ローカルな追加読み。forge I/O ではない)。
- S1 の 2 つの placeholder `Skip` を `Step::Agent(Arm::ConflictResolver)` /
  `Step::Agent(Arm::CiFixer)` に置換。arm regime 側(未 arm)ではなく watch regime 側にある
  この分岐が、まさに「fixer 家族に委譲する」判断だった。加えて未解決スレッド →
  `Step::Agent(Arm::Fixer)` を足す。

### 1.5. bulk observe の拡張(f2 / f3 の決定)—— informer-cache は1クエリのまま

ADR 0012 決定3 は「loop ごとの個別 API 叩きをやめ、resync ごとに一括クエリ」を義務づける。よって
不足フィールドは **per-PR の追加読みを足すのではなく `observe_open_prs` の一括クエリを広げて**埋める
(individual read への後退は ADR 違反)。`PrObservation` の以下 2 点を拡張する:

- **f2(Fixer arm の発火)**: 各 `ReviewThread` に **最終 comment だけ**を載せる
  (GraphQL `comments(last:1)` の author login + body)。これで engine 側が既存の
  `thread_awaits_fixer`(未解決 かつ 最終 comment が `FIXER_REPLY_MARKER` でない)を計算できる。
  全 comment は要らない(最終1件で十分)ので cost 増は限定的で、既存 `observe_cost` に載って可観測。
  → **代替案 B(per-PR `list_review_threads` を残す)は却下**: 個別 read の復活で ADR 決定3 に反する。
- **f3(claim の真正性)**: 各 PR 会話 comment に **`viewerDidAuthor`(自分が書いたか)と node `id`** を
  足す(`PrComment` を拡張)。claim マーカーは **meguri 自身が書いた comment のときだけ信頼**し、
  release 時にその `id` で編集して無効化する(§7)。
- **f6(100 件超 PR での担保)**: 今の observe は comment window(`last:100`)が切れると
  **REST `all_pr_comments` へ fallback**する(`gh.rs:1583-1585`)。しかし REST 結果には
  `viewerDidAuthor` が無く、id も GraphQL node id と別形式。このままだと 100 件超の PR で
  **自分の claim を第三者扱いして no-steal を失い、tombstone 編集の id も狂う**。よって
  **決定: overflow の fallback を REST から GraphQL cursor pagination に変える**
  (`comments(first:100, after:$cursor)` を最終ページまで辿り、各ページで `body createdAt
  viewerDidAuthor id` を同じ形で取る)。これで 100 件超でも真正性判定と id 編集が保たれる。
  → **代替案(REST fallback を残し REST 側で著者判定)は却下**: REST は `viewerDidAuthor` を返さず、
  bot トークンの自著判定を著者 login 比較で代替すると別 instance との区別も崩れる。metadata を
  落とさない GraphQL pagination が唯一整合する。arm-marker(§7 の claim / S1 の automerge)の
  idempotency も同じ pagination の恩恵を受ける。

この 3 点で「forge に新フィールド不要」は撤回。ただし追加は **bulk observe 1 クエリ + その overflow
pagination の中**に閉じ、per-PR の個別 read(旧 `list_review_threads` 等)は増やさない
(ローカルな `succeeded_run_count` 読みを除く)。gh / fake も追随。

### 2. 3 fixer は arm recipe として残し、`discover()` だけ空にする(f1 の決定)

`fixer.rs` / `ci_fixer.rs` / `conflict_resolver.rs` は**削除しない**。重い agent フロー
(`Flavor` 実装 + `run_fixer` / `run_ci_fixer` / `run_conflict_resolver`)は arm の実体
(recipe)として残す。

**f1 の決定 —— `impl Loop` は残し、`discover()` を空(`Ok(Vec::new())`)にするだけ**にする。
理由: `Scheduler::dispatch` は `run.loop_kind` に一致する `Loop` を `self.loops` から引いて
`drive()` を呼ぶ。3 fixer を `default_loops()` から抜くと、reconciler が作った run が
「unknown loop」として捨てられる。よって **`default_loops()` の登録は残す**(dispatch の解決簿として)。
discovery だけを reconciler の resync に移すため、3 fixer の `discover()` は空に落とす —— これで
discovery の権威は reconciler に一本化されつつ、weight / redispatch / slot 予算 / crash recovery の
既存機構がそのまま効く。`act(Step::Agent(arm))` は対応する `run_*` を `queued` run として作り、
scheduler が `loop_kind` 経由で `drive()` に配る。予算定数(`MAX_CI_FIX_RUNS` / `MAX_RESOLVE_RUNS`)は
arm の判断(§4 の escalate 条件)へ移す。

> discovery と recipe の登録簿を物理分離する案(finding の対案)も検討したが、旧 `Loop` trait は
> S4 で丸ごと撤去される。このスライスの間だけ「空 discover」で橋渡しするのが最小差分で、S4 の
> 撤去時に自然に消える。

### 3. dispatch = workqueue + resync

ADR 0012 決定6の 3 キューを、**新しい器を作らず既存基盤へ写像**する(最小差分):

| 概念キュー | 実体 | 正しさの源 |
|---|---|---|
| **activeQ**(優先度 = マージ近接) | `queued` run 群を**優先度順**に dispatch | resync が毎 tick 再構築 |
| **backoffQ**(指数バックオフ) | sqlite テーブル `reconciler_backoff`(§migration) | `next_visible_at`・`scheduled_attempt`(forge から復元不可) |
| **parked**(人間待ち) | `Wait(_)` = run を作らない + forge の `hold`/`needs-human` | forge ラベル(spec 軸の権威) |

resync = poll 相乗りの `issue_reconciler::sweep`(S1 の `merge_tail::sweep` を継ぐ)。毎 tick:

1. **observe**: `observe_open_prs`(S1 の `observe_merge_tail` を改名)一括クエリ。cost を emit。
2. **decide**: PR ごとに `Snapshot` を作り `next_step`。
3. **act**:
   - `Op` / `Wait` / `Skip` は S1 同様その場で処理(Op は API 操作、Wait/Skip はログのみ)。
   - `Agent(arm)` は **workqueue へ enqueue**: (a) backoff テーブルを見て `next_visible_at` が
     未来なら skip(§4.5)、(b) 自著 claim marker の `run_id` が **active** なら skip
     (no-steal / 家族排他。terminal なら stale として reclaim、§7)、(c) `queued` run を作る
     (**家族横断 `runs_active_fixer_family` インデックス**が同 PR の 2 本目を atomic に弾く、§7)。

3 fixer の `discover()` は空になるが `default_loops()` の登録は残る(§2 の f1 決定)。planner / worker
などは S4 まで従来の `Loop` のまま**併存**。dispatch の解決は `run.loop_kind` 経由なので、これで
scheduler の weight / redispatch / slot 予算機構をそのまま使える。

### 4. arm の判断と escalate/backoff の対応(ADR 0007 supersede の完成)

ADR 0007 の分類が全部行き先を持つ(これが S1 の Skip を埋める意味):

| 観測 | Step | キュー |
|---|---|---|
| Conflicting | `Agent(ConflictResolver)`(予算内)/ `Op(Escalate)`(予算超過で再衝突) | active / parked |
| Blocked かつ required check 失敗 | `Agent(CiFixer)`(予算内)/ `Op(Escalate)`(予算超過で赤) | active / parked |
| 未解決スレッド(人/外部 bot) | `Agent(Fixer)` | active |
| Behind(arm 済み) | `Op(UpdateBranch)`(S1) | — |
| Blocked・非 Behind・stale | `Op(Escalate)`(S1 の Stuck backstop) | parked |
| transient(`merge == None` 等)/ 一時的に未整定 | `Wait` → 次 resync | —(backoff は §4.5) |
| `hold` / `needs-human` | `Wait`/`Skip`(human stop) | parked |

予算(`MAX_*_RUNS`)超過は「これ以上自動で回さず人間へ」= parked への昇格。conflict_resolver の
#176 の順序修正(まだ conflicting か**先に**確認してから予算判断)は arm の中でも維持する。

### 4.5. backoff のライフサイクル(f4 の決定 — 誰が作り、誰が消すか)

finding f4 のとおり、当初案は backoff テーブルを「読む」だけで**書く契約が無く**、`RequeueAfter` も
`Step` に存在しなかった。ここで確定する。

- **`next_step` は純粋・`Step` のみ。`RequeueAfter` は導入しない**(壁時計依存を pure 関数に持ち込まない)。
  backoff の「時間」は `next_step` の外、resync 側で扱う。
- **作る = resync 側、観測駆動。「成功済み run 数」を高水位マークにして 1 ラウンド 1 回だけ進める
  (finding 1 の決定)**。run 完了の瞬間に書かない —— push 直後の CI は普通 `Pending` で「症状が残る」か
  まだ分からないからだ。判定は**後の resync** が赤を観測して初めて確定する。ただし同じ成功 run を毎
  tick 数えて `attempt` を膨らませてはいけない。そこで **`succeeded_run_count`(runs 表に既に永続。
  arm×issue の成功ラウンド数)を唯一の attempt 源にし、backoff 行にはそれを「どの成功数まで間隔を
  引いたか」の高水位マーク `scheduled_attempt` として持つ**。resync ごと、症状が残る PR×arm について:
  - `n = succeeded_run_count(project, arm, issue)` を読む。
  - **`n > scheduled_attempt`(前回スケジュール後に新しい成功ラウンドが1本増えた)ときだけ** 1 回進める:
    `next_visible_at = now + min(cap, base * 2^n)`、`scheduled_attempt = n` を書く(config `[reconciler]`
    の base / cap)。
  - `n == scheduled_attempt`(この成功ラウンドは既に間隔化済み)なら `next_visible_at` は**触らない**
    —— 毎 tick 押し戻して無限延期する事故を防ぐ。
  こうして各成功ラウンドはちょうど 1 回だけ `attempt` を進め、`n=0`(まだ 1 度も直していない初回赤)は
  行が無い=即 enqueue、以降のラウンドだけ `2^n` で間隔が開く。
  - **`Interrupted`(pane 死・中断)は backoff の対象にしない(f7)**。`RunStatus::Interrupted` は
    終端結果ではなく、既存の `redispatch_interrupted` が毎 tick チェックポイントからそのまま再開する
    (crash recovery、#183)。backoff を被せると新規 run 向けゲートも通らず「pane 死が毎 tick 再試行」に
    なる。回復は**従来どおり redispatch に委ね**、backoff は触らない(crash-loop 抑制は本スライス対象外)。
  - **transient(observe の `merge==None` 等)も persistent backoff にしない**。`next_step` が
    `Wait`/`Skip` を返し、次 resync が `poll_interval` 間隔で再観測して追いつく(poll が pacing)。
  - **genuine な失敗(agent が直せず escalate)は backoff ではなく parked(needs-human)昇格**。
- **enqueue ゲート = 読む側**。resync の act(c) 前に backoff を引き、`next_visible_at` が未来の
  PR×arm はこの resync では **run を作らない**(activeQ に入れない)。
- **消す = resync 側、症状の「積極的な解消」を観測したときだけ(f5 の決定)**。毎 resync、その
  PR×arm が **positive に解決**した —— conflict なら `Mergeable`(`Unknown` は不可)、ci なら rollup
  `Success`(`Pending` は不可)、fixer なら awaiting スレッド無し —— ときに限り
  `store.clear_backoff(project, item_key, arm)` で行を削除する。
  - **`head 前進` は clear 条件にしない(f5)**。ci-fixer 等の成功 push それ自体が head 前進なので、
    head を消去条件にすると arm 自身の1押しで即 clear され、指数バックオフが育たず ping-pong を
    抑えられない。加えて現 schema には比較元の head SHA が無く、arm 自身の push と外部前進を
    区別できない。よって **head SHA は持たず、head 前進も clear に使わない**。
  - **transient(`Pending` / `Unknown` / `merge==None`)は「解消」ではない**。ここで clear すると
    CI 再実行中の谷間で行が消え `scheduled_attempt` がリセットされ、やはりバックオフが育たない。
    positive 解決の信号(緑 / mergeable / スレッド無し)だけを clear のトリガにする。
  - 結果、`2^n` の間隔は赤→赤のラウンドをまたいで**単調に開く**(`n = succeeded_run_count` が単調)。
    PR がこの arm を本当に通過したときだけ行が消えて 0 に戻る。row は PR が閉じ/merge されれば孤児化
    (finalize/reaper が回収、S4)。
- 予算超過(`MAX_*_RUNS`)は backoff ではなく **parked(needs-human)への昇格**で、別物 —— バックオフが
  間隔を広げ、budget がラウンド総数を打ち切る、の二段構え(どちらも `succeeded_run_count` を読む)。

つまり **backoff は「成功ラウンドの観測が高水位マークで 1 回ずつ間隔を開け、positive 解決の観測だけが
消す」**。これで同じ成功 run を二度数えず、`next_visible_at` が arm 自身の push で誤って reset されず、
症状が本当に解けたときだけ
clear される —— 受け入れ基準で検証可能になる。

### 5. 優先度関数 = マージ近接(ADR 0001 の移設先)—— 全 loop を含む rank(finding 4 の決定)

`queued` run の dispatch 順を「現状の `list_runs` 順」から**優先度キー順**へ変える。これは reconciler が
enqueue する 3 fixer arm だけでなく、default_loops 経由で作られる**全 loop の queued run にも効く
グローバルな並べ替え**なので、優先度キーは全 loop_kind に定義しなければならない(でないと spec_fixer 等の
既存の相対順が壊れる)。ADR 0001 の「登録順が優先度」を、この明示的な rank に移設する:

- **優先度キー = `dispatch_rank(loop_kind)`**(小さいほど先)。値は今日の `default_loops()` の並び順
  そのもの —— `conflict-resolver=0 < ci-fixer=1 < fixer=2 < spec-fixer=3 < spec-worker=4 <
  pr-reviewer=5 < worker=6 < planner=7 < cleaner=8 < triage=9`。reconciler の 3 fixer arm は自分の
  `loop_kind` の rank(0/1/2)を使う。これで「merge に近い順」= マージ近接がそのまま再現される。
- 同 rank 内は **issue 番号昇順(FIFO)**。`dispatch_rank` は純粋・全順序・決定的(property test で担保)。

**`spec_fixer` の置き場所(finding 4)**: `spec_fixer` は plan 側の fixer(plan review が留めた spec PR を
unpark する。ADR 0013/0014)で、本スライスが畳む impl 側 3 兄弟(fixer/ci_fixer/conflict_resolver)
とは別物。よって **S3 では従来の `Loop` のまま据え置き**(discover も無変更)、rank は今日と同じ **3**
(fixer の直後)を与えて既存の相対優先を保つ。arm 化・reconciler 吸収は **S4**(planner/worker/
spec_worker と一緒に Issue Kind へ)。本スライスは spec_fixer の挙動も順位も変えない —— 変わるのは
「順位が登録順の副作用から `dispatch_rank` という明示キーになった」ことだけ(既存の順序テストは緑のまま)。

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

### 7. claim identity / no-steal(ADR 0027 の芯)—— f3 の決定

finding f3 の急所: PR コメントは(公開リポジトリでは)**誰でも書ける**。claim の権威を「本文だけの
コメント」に置くと、label より弱い権限の第三者が偽マーカーを投稿し、no-steal を悪用して自動処理を
止められる。しかも `PrComment` に author が無い。ここを次のように塞ぐ。

- **信頼するのは自分が書いた claim マーカーだけ**。§1.5 の拡張で各 comment に `viewerDidAuthor` を
  載せ、`viewerDidAuthor == true`(= meguri 自身が投稿)の claim マーカーしか読まない。第三者の
  偽マーカーは `viewerDidAuthor == false` なので **無視される** —— 偽装しても no-steal を凍結できない
  (むしろ何も止まらない)。これが f3 の悪用ベクタを消す核心。
- マーカー書式は `<!-- meguri:claim instance=<id> run=<run_id> -->`。**claim は PR(work-item)単位で、
  arm 非依存**(1 PR に fixer 家族の active claim は最大1本)。head 非依存(claim は head 移動をまたぐ)。

- **排他の権威 = sqlite の「fixer 家族を横断する」active-run 部分ユニークインデックス(finding 2 の決定)**。
  現行 index は `runs(project_id, loop_kind, issue_number)` 単位(`0007_tasks.sql:74-76`)なので arm を
  またいで排他しない —— Fixer 実行中に CI が赤へ変わると CiFixer は別 `loop_kind` で enqueue でき、
  今 `meguri:working` が担う「PR に fixer 家族は同時 1 本」(`mod.rs:293-319`)が失われる。よって
  migration 0016 に **家族横断の部分ユニークインデックス**を足す:
  ```sql
  CREATE UNIQUE INDEX runs_active_fixer_family
    ON runs(project_id, issue_number)
    WHERE loop_kind IN ('conflict-resolver','ci-fixer','fixer')
      AND status IN ('queued','running','interrupted') AND issue_number IS NOT NULL;
  ```
  これが atomic な単一 instance 権威。claim マーカーはその forge 射影で、Phase-4 の共有 DB で
  クロスホスト権威に昇格する。`meguri:working` は人間向け表示射影(付け外しは続けるが権威ではない)。

- **enqueue の排他判定 = マーカーの run が「まだ生きているか」で決める(finding 3 の決定)**。単なる
  マーカー存在ではなく、`run_id` を runs 表で引いて状態を見る。これが no-steal と stale 回収を同時に解く:
  - `run_id` が **active**(`queued`/`running`/`interrupted`)→ **skip**。PR は fixer 家族の誰かが処理中
    (Fixer 実行中は CiFixer も止まる = 家族横断排他が forge 側にも見える)。他 instance の active run
    なら no-steal。
  - `run_id` が **terminal**(succeeded/failed/stopped)/ 見つからない → マーカーは **stale**。無視して
    **reclaim**(自分の新 run で上書き)。tombstone 編集に失敗して古いマーカーが残っても、instance 名を
    変えた再起動や将来の別 instance が引っかかって永久停止する事故は起きない —— 生存判定が run を見るからだ。
- **release の契約と再試行**: run 終端時に claim comment を node `id`(§1.5)で tombstone 編集する。
  best-effort だが、**correctness は tombstone の成否に依存しない**(上の生存判定が本線)。編集失敗は
  `reconciler.claim_release_failed` を emit し、次に同 PR を処理する resync が「run terminal ⇒ stale」
  として回収・上書きする。期限タイマーは設けない(run 生存が唯一の真実)。
- instance id: github 側は net-new(今日は label のみ・owner 無し)。既定 = `mux.session`
  (既定 `"meguri"`)、`[reconciler] instance = "..."` で上書き可。

> 現実的射程の注記: 今日は単一 instance 運用なので、家族横断インデックスが実効の排他、マーカーはその
> 射影。no-steal(他 instance の active run で skip)は Phase-4 の複数ホスト共有 DB で活きる前準備。

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
  `next_visible_at`・`scheduled_attempt` だけが forge 非復元**なので、sqlite に置くのは backoff のみと
  する。これは
  ADR 0012 決定6 の一段の精緻化として ADR 0027(または実装時の短い追記)に記録する。
- **D: claim を label のまま atomic 化** → 却下。label には owner を載せられず no-steal を表現できない。
  0015 の要求(claim の真実 = instance 名入り)を満たすにはマーカーが要る。

---

## migration & rollback(必須 — 永続状態と契約を触る)

**追加のみ・破壊なし。**

- **schema**: migration `0016_reconciler_backoff.sql` を新設。
  `reconciler_backoff(project_id TEXT, item_key INTEGER, arm TEXT, scheduled_attempt INTEGER,
  next_visible_at TEXT, PRIMARY KEY(project_id, item_key, arm))`。`scheduled_attempt` は
  「どの `succeeded_run_count` まで間隔を引いたか」の高水位マーク(finding 1)。**head SHA 列は
  持たない**(f5: head 前進を clear 条件にしないため不要)。`schedules.rs` に倣ったアクセサ
  `src/store/reconciler.rs`(read / advance / clear)。
- **同 migration に fixer 家族横断の部分ユニークインデックス `runs_active_fixer_family` を足す
  (finding 2、§7)**。`runs(project_id, issue_number) WHERE loop_kind IN
  ('conflict-resolver','ci-fixer','fixer') AND status IN ('queued','running','interrupted')`。
  既存の `runs_active_issue`(loop 単位)はそのまま残る(併存・後方互換)。ALTER なし・追加のみ。
- **claim marker**: 追加コメント + release 時の**自分のコメント編集**(tombstone 化、§7)。
  correctness は tombstone 成否に依存せず、生存判定(run 状態)が本線。`working` label は射影として
  付け外しを続けるので、旧バイナリ・人間の目には後方互換。
- **forge observe の拡張(§1.5)**: `observe_open_prs` の GraphQL に、各スレッドの最終 comment
  (`comments(last:1)` の author+body)と、各 PR 会話 comment の `viewerDidAuthor` + node `id` を
  足す。**さらに comment window overflow の fallback を REST `all_pr_comments` から GraphQL cursor
  pagination へ変える(f6)**—— REST は `viewerDidAuthor` を落とすので 100 件超の PR で claim の
  真正性が壊れる。schema ではなくクエリ/ページングの変更なので migration は不要・後方互換
  (gh/fake の read が広がるだけ)。
- **前進(forward)**: activeQ / parked は**再導出**なので状態移行データは不要。初回 resync が
  observe から全部組み直す(level-triggered の利点)。backoff テーブルは空から始まって自然に埋まる。
- **rollback**(この PR を revert): 3 loop の `discover` が戻り、`reconciler_backoff` は**孤児化
  するだけ**(誰も読まない・害なし)。claim マーカーは無害なコメントとして残る(cleaner が拾うか
  無視)。`working` label は本スライスでも射影として付け外しを続けているので、旧 `pr_is_touchable` が
  そのまま claim として尊重し、**二重 claim は起きない**(単一 instance 前提)。
  - **`runs_active_fixer_family` インデックスの残置**: migration は前進のみ(revert されない)なので、
    rollback 後もこの家族横断インデックスは残る。旧コードは fixer 家族の同時実行を `working` label の
    discovery-time skip で既に直列化しているため、このインデックスに**依存もしないが、破られもしない**。
    稀に旧コードが同 PR に family 2 本目の run を作ろうとした場合、インデックスが弾いて run 作成が
    `Err` → 既存の「誰かと競合した、skip」経路に穏当に落ちる(害なし)。
- **段階順序**(ADR 0012 のスライス順の趣旨):forge 権威(spec 軸ラベル)には触れない。触るのは
  status 軸の射影化(`working`)と、local な実行進行(backoff)のみ。

## observability

- 既存 `merge_tail.observe_cost` を `reconciler.observe_cost` に継ぐ(requests / graphql_cost / prs)。
- 新規 emit: `reconciler.enqueued`(arm, issue)/ `reconciler.parked`(reason)/
  `reconciler.backoff_scheduled`(arm, scheduled_attempt, next_visible_at)/
  `reconciler.policy_disabled`(arm)/ `pr.claimed`(instance, run を含める)/
  `reconciler.claim_skipped`(reason = active-run-of-other / same-instance-busy = no-steal / 家族排他)/
  `reconciler.claim_reclaimed`(stale marker の回収)/ `reconciler.claim_release_failed`(tombstone 失敗)。
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
4. **claim / no-steal / 家族排他 / stale 回収(芯3・f3・finding 2・finding 3)**:
   (a) *第三者が書いた*(`viewerDidAuthor==false`)偽マーカーは**無視され dispatch は進む**(凍結不能)。
   (b) マーカー無し / 自著だが run が terminal → dispatch 可。
   (c) 自著マーカーの `run_id` が **active** → skip(no-steal / 家族排他)。**別 arm でも**同 PR に
   fixer 家族 run が active なら enqueue されない(finding 2 —— 家族横断インデックスと生存判定の両輪)。
   (d) **stale 回収(finding 3)**: 自著マーカーの run が terminal なら、tombstone 編集が失敗して古い
   マーカーが残っていても reclaim され dispatch が進む(instance 名変更後の再起動でも永久停止しない)。
   (e) release は node `id` で tombstone 編集し、失敗時は `claim_release_failed` を emit する。
   (f) **DB レベル**: 同 PR(canonical issue)に fixer 家族 run を 2 本目 create しようとすると
   `runs_active_fixer_family` が弾く(store 単体テスト)。engine 側は FakeForge + in-memory store で連結検証。
5. **comment pagination の真正性(f6 / f8)**: **これは FakeForge では検証にならない**(FakeForge は
   全 comment を1度に返し、`last:100` clip も cursor pagination も通らないので、pagination が
   `viewerDidAuthor`/`id` を落としても緑になる)。よって **GhForge のページ畳み込みを純関数
   `fold_comment_pages(&[Value]) -> Vec<PrComment>` に切り出し、`pageInfo{hasNextPage,endCursor}` を
   持つ複数ページの scripted JSON(2 ページ目は `endCursor` を辿って初めて届く)を食わせて**、
   全ページで `viewerDidAuthor`/`id` が保たれること・2 ページ目に置いた claim マーカーが自著判定と
   node id で拾えることを assert する(GhForge-level test)。
6. **backoff(f4 / f5 / f7 / finding 1)**: (a) 症状が残る PR×arm で、`succeeded_run_count` が
   `scheduled_attempt` を超えたときだけ `next_visible_at = now + min(cap, base * 2^n)` を置き
   `scheduled_attempt = n` に更新する(**式は §4.5 と同一**)。(b) **同じ成功 run を毎 tick 二度数えない**:
   `n == scheduled_attempt` の resync では `next_visible_at` を変えない(無限延期しないことを assert)。
   (c) `n=0`(初回赤・成功 run 無し)は行が無く即 enqueue、以降のラウンドだけ間隔が開く。(d) due 前の
   PR×arm は enqueue されない。(e) **arm 自身の成功 push(= head 前進)/ transient(`Pending`/`Unknown`)
   では clear されず**、positive 解決(緑 / mergeable / スレッド無し)を観測した resync でだけ
   `clear_backoff`(f5)。(f) sqlite なので restart をまたいで生存。(g) **`Interrupted` は `advance_backoff`
   を呼ばず `redispatch_interrupted` が再開**(f7)。`next_step` に `RequeueAfter` は無い。
7. **thread 観測(f2)**: bulk observe が各スレッドの最終 comment を載せ、`thread_awaits_fixer` が
   計算できる(最終が fixer 返信マーカーなら Fixer arm は発火しない)ことを FakeForge で検証。
8. **優先度順 dispatch(finding 4)**: `dispatch_rank` が全 loop_kind に対し
   conflict=0 < ci=1 < fixer=2 < **spec-fixer=3** < spec-worker=4 < pr-reviewer=5 < worker=6 <
   planner=7 < cleaner=8 < triage=9 を返す(純粋・全順序)。`queued` run がこの rank 順・同 rank は
   issue 番号昇順で dispatch され、**`spec_fixer` の相対順(fixer の直後 / worker より前)が今日と一致**
   することを assert(既存の `spec_fixer_sits_in_the_fixer_family_above_new_work` を rank ベースへ移植)。
9. **非回帰**: 既存 `tests/*fixer*` の discovery テストは reconciler の arm 判定へ書き換え。
   `scheduler_test.rs` / `issue_reconciler`(旧 merge_tail)の property は破壊しない。`spec_fixer` は
   S3 で無変更(挙動・順位とも)。統合テスト(`tests/fixtures/fake_agent.sh`)で fixer arm が実 tmux /
   実 worktree で回ることを確認。

---

## 触るファイル

- `src/engine/merge_tail.rs` → `src/engine/issue_reconciler.rs`(改名 + `Snapshot`/`Step` 拡張、
  placeholder Skip → Agent arm、Fixer arm 追加、`SignalCarrier` seam、`apply_policy`、
  claim マーカー排他、優先度キー)
- `src/engine/fixer.rs` / `ci_fixer.rs` / `conflict_resolver.rs`(**`impl Loop` は残し `discover()` を
  空にする**(f1)、`Flavor` + `run_*` は arm recipe として残す、予算定数を arm 判断へ)
- `src/engine/mod.rs`(3 fixer は `default_loops()` に**登録を残す**が discover は空(f1)、
  `pr_is_touchable` の claim 判定を marker + run 生存(finding 3)へ、`dispatch_rank(loop_kind)` を定義
  (finding 4、`spec_fixer=3` を含む全 loop)。`spec_fixer` は無変更)
- `src/engine/scheduler.rs`(`queued` run を `dispatch_rank` 順に dispatch、backoff enqueue ゲート、
  resync sweep 呼び出しを継ぐ)
- `src/store/migrations/0016_reconciler_backoff.sql`(新規: `reconciler_backoff` +
  `runs_active_fixer_family` 家族横断インデックス(finding 2))+ `src/store/reconciler.rs`
  (アクセサ新規: `advance_backoff`(高水位マーク方式)/ `clear_backoff` / read)+ 家族横断 active-run の
  存在判定 helper)
- `src/config.rs`(`[reconciler]` = `ReconcilerConfig`: step policy allow-set、backoff base/cap、
  carrier 束縛(既定 labels)、instance 名)
- `src/forge/mod.rs` / `gh.rs` / `fake.rs`(`observe_merge_tail` → `observe_open_prs` 改名 +
  §1.5 の observe 拡張: `ReviewThread` の最終 comment、`PrComment` の `viewerDidAuthor` + `id`、
  **comment overflow の fallback を REST → GraphQL cursor pagination に変更(f6)**——
  ページ畳み込みは純関数 `fold_comment_pages(&[Value]) -> Vec<PrComment>` に切り出し
  GhForge-level test(f8)可能にする、claim comment 編集用の write)
- `README.md` / `README.ja.md`(dispatch = workqueue + resync、fixer は arm、claim marker の一段)
- `tests/` + `src/forge/gh.rs`(next_step の property test 拡張、claim no-steal / backoff の
  FakeForge 連結、**`fold_comment_pages` の複数ページ scripted JSON test(f8)**、既存 fixer テスト書換)
- 実装時に新規 ADR: **0026**(signal binding / step policy)/ **0027**(claim identity / no-steal ——
  **claim の権威 = 家族横断 active-run インデックス、マーカーはその forge 射影で生存判定つき
  (finding 2 / 3)**、parked 非永続 + backoff の高水位マーク方式(finding 1)の精緻化を含む)。
  spec は使い捨てなので、これら恒久的な設計判断は実装時に ADR へ振り分ける。

## 受け入れ基準(acceptance criteria)

1. `fixer` / `ci_fixer` / `conflict_resolver` の discovery が `issue_reconciler::next_step` の `Agent`
   arm に一本化され、3 loop の `discover()` が空になっている。**`impl Loop` と `default_loops()` 登録は
   dispatch 解決のため残る**(f1)—— reconciler が作った run が unknown loop で捨てられないこと。
2. **「全状態にちょうど1つの所有 arm」property test が緑**。BEHIND を含め、所有の欠落・二重所有を
   property test が検出する(S1 の property を fixer arm まで拡張)。
3. Conflicting → `Agent(ConflictResolver)`、赤 CI(required)→ `Agent(CiFixer)`、未解決スレッド →
   `Agent(Fixer)`。予算超過はいずれも parked(needs-human)へ昇格。bulk observe が各スレッドの最終
   comment を載せ `thread_awaits_fixer` を計算できる(f2)。
4. dispatch が workqueue + resync で動く: `queued` run が `dispatch_rank` 順(マージ近接、`spec_fixer` の
   相対順は不変 —— finding 4)に出る、parked は run を作らない。**backoff(f4 / f5 / f7 / finding 1)**:
   症状が残る PR×arm で `succeeded_run_count` が高水位 `scheduled_attempt` を超えたときだけ
   `next_visible_at = now + min(cap, base * 2^n)` を **1 ラウンド 1 回** 置き(同じ成功 run を二度数えない)、
   due 前は enqueue されず、restart をまたいで生存する。**arm 自身の成功 push(head 前進)や transient
   (`Pending`/`Unknown`)では clear されず**、positive 解決を観測したときだけ `clear_backoff` される
   (head SHA は持たない)。**`Interrupted` は backoff を作らず `redispatch_interrupted` が再開**(f7)。
5. signal binding: `Labels` 担体 seam が入り、seam 経由の `Snapshot` が baseline と一致する
   property test が緑。
6. step policy: `apply_policy` が入り、無効 arm が `Wait(PolicyDisabled)` になる property test が緑。
7. claim / 家族排他 / stale 回収(f3 / f6 / f8 / finding 2 / finding 3):
   - 第三者の偽マーカーは無視して dispatch を止めない(凍結不能)。自著マーカーは `run_id` を引き、
     **active なら skip(別 arm でも同 PR に fixer 家族 run が active なら enqueue しない)**、terminal なら
     stale として reclaim(tombstone 失敗・instance 名変更でも永久停止しない)—— property/連結 test が緑。
   - **DB レベルの家族横断排他**: `runs_active_fixer_family` が同 PR の 2 本目 family run を弾く
     (store 単体 test)。`meguri:working` は表示射影として付け外しされる。
   - **100 件超 comment の pagination は `fold_comment_pages` の GhForge-level test で担保**(FakeForge 不可)
     —— `pageInfo`/`endCursor` を辿り全ページで `viewerDidAuthor`/`id` が保たれ、2 ページ目の claim
     マーカーが拾えること(f8)。
8. **`dispatch_rank` が全 loop を順序づけ、`spec_fixer` の相対順が今日と一致**する(finding 4)。
   `spec_fixer` は S3 で挙動・順位とも無変更。
9. 既存テスト(特に `issue_reconciler`(旧 `merge_tail`)property / `scheduler_test.rs` / 統合テスト)が
   全て緑。`cargo fmt` / `clippy -D warnings` / `nextest` / `test --doc` が通る。

## スコープ外(S4 以降)

- planner / worker / spec_worker / **spec_fixer** / guard / pr_reviewer と cleaner / triage /
  routing_drift の Issue/Repo Kind 吸収、旧 `Loop` trait の撤去、body-edit `reconcile` →
  `reconcile_body_edits` 退避、`reaper` → `Op(Finalize)`、`ensure_project_clone` →
  `Op(EnsureClone)`(全て S4)。`spec_fixer` は本スライスでは `dispatch_rank=3` の旧 `Loop` のまま。
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
