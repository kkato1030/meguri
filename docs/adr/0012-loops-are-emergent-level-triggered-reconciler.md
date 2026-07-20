# ADR 0012: loop はコード上の構造物ではなく実行時の軌道 — level-triggered reconciler(3 Kind + 純関数 next_step + workqueue)へ再構成する

- Status: proposed
- Date: 2026-07-20
- Issue: #198

> 番号について: 本リポジトリの ADR 番号は一意ではなく、slug と Issue 番号で区別する運用に
> なっている(既に `0012-*` は複数ある)。本 ADR は Issue #198 が正と宣言した
> `0012-loops-are-emergent-level-triggered-reconciler.md` を採用する。

## Context

meguri は今 15 個以上の loop / sweep 実装を抱えている。`src/engine/` の 10 個の `Loop`
実装(worker / planner / pr_reviewer / spec_fixer / spec_worker / fixer / ci_fixer /
conflict_resolver / cleaner / triage)と、`scheduler.rs` の poll tick から直接呼ばれる
帯域外 sweep(scheduler_fire / reaper / auto_merger / merge_watch / plan_handoff /
decompose_materializer / routing_drift / reconcile)である。どれも「トリガを1つ増やすたびに
loop を1つ増やす」流儀で育ってきた。

この流儀の限界が **BEHIND 問題**で露呈した。auto-merge を arm した後に base branch が進むと、
その PR は「マージ可能だが古い」状態で誰にも直されずに留まる。conflict でも red CI でもないので
conflict_resolver も ci_fixer も拾わない。これを直すのに「16 個目の loop(base 更新 loop)」を
足すのは、同じ構造的欠陥をもう一段深くするだけだ。

根本原因は、**loop がコード上の1級構造物になっていること**にある。各 loop が自分のトリガ条件
(discover)と駆動(drive)を個別に持ち、状態遷移がコード全体に散らばっているため、「ある状態の
issue/PR を今どの loop が見るべきか」がグローバルには決まらない。トリガの組み合わせ(arm 済み ×
base 進行、のような)が増えるたびに、それを見る loop を新設するしかない。

looper / Kubernetes controller と同じ **level-triggered reconciler** に立ち返る。制御ループは
「エッジ(イベント)に反応する分岐」ではなく「観測した現状(level)を望ましい状態へ寄せる冪等な
関数」であるべきだ。loop はコード上の構造物ではなく、reconcile と requeue の合成として**実行時に
現れる軌道**になる。

## Decision

meguri を **level-triggered reconciler(observe → decide → act + requeue)**へ再構成する。

### 1. 1 reconciler = 1 Kind(single-owner)

reconciler は Kind ごとに1つだけ置き、その Kind の identity を単独で所有する。現行の全 loop /
sweep が3 Kind のどれに吸収されるかを**漏れなく**割り当てる(移行後に「旧 `Loop` trait 撤去 /
全 Kind が reconciler 経由」を成立させるには、行き先の無い loop/sweep が1つも残ってはならない):

- **Issue Kind**(issue/PR identity で鍵る) — planner / worker / spec_worker / spec_fixer /
  pr_reviewer / fixer / ci_fixer / conflict_resolver / self-review / plan_handoff /
  auto_merger / merge_watch。加えて、poll tick から回る次の3 sweep も issue identity 駆動
  なので Issue Kind が吸収する:
  - `reaper::sweep`(close 済み issue の pane / worktree / merged branch の回収)→
    issue が terminal に達したときの `Op(Finalize)`。
  - `decompose_materializer::sweep`(承認済み分解提案 PR を子 issue + 依存へ materialize)→
    spec-ready の分解提案という identity に対する act。
  - 既存 `reconcile::sweep`(#142、body-edit 再注意)→ implementing issue の観測から出す
    signal(名前衝突のため `reconcile_body_edits` へ退避、後述)。
- **Repo Kind**(repo 単位の検出器 — 観測は repo 全体、書き込みは `Op`) — cleaner / triage /
  routing_drift。加えて、scheduler tick の最上段で declared-but-missing な managed clone を
  実体化する `ensure_project_clone`(#195、ADR 0018 が既に level-triggered reconcile step と
  呼んでいる)も repo scope の冪等 ensure なので Repo Kind が所有し、`Op(EnsureClone)` として
  表す。**Repo Kind は read-only ではない**点に注意する:
  - triage auto(ADR 0017)は閾値超え推薦を `meguri:ready` / `meguri:plan` へ昇格する。これは
    **spec 軸への書き込み**(決定 5)なので、triage-auto は spec 軸の 4 handshake 列挙メンバの
    1つとして扱う。検出は repo scope だが、昇格の act は当該 issue identity への `Op` である
    (ADR 0017 と矛盾しない)。
  - cleaner(ADR 0003)は 1 本のレポート issue を更新する。これも observe は repo 全体・書き込みは
    そのレポート issue への `Op`。read-only の原則(レポート以外は書かない)は不変。
  - routing_drift はドリフト検出。書き込みが要る場合は同様に `Op` として明示する。
- **Schedule Kind**(cron 起票) — scheduler_fire。

これで現行の10 `Loop` 実装・poll-tick sweep(scheduler_fire / reaper / auto_merger /
merge_watch / plan_handoff / decompose_materializer / routing_drift / reconcile)・および
tick 最上段の bootstrap reconcile(`ensure_project_clone`)はすべていずれかの Kind に属す。
対象外にする loop/sweep/reconcile step は無い。`ensure_project_clone` は「消える 8 sweep」では
なく Repo Kind の `Op` へ畳まれる(scheduler 固有の reconcile 経路を移行後に残さない)。

新しいトリガは Kind でなく `next_step` の **arm(分岐)**として増設する。BEHIND のような新条件は
loop を新設せず、`next_step` に arm を1本足すだけで済む。

### 2. reconcile 契約

```
reconcile(id) -> Verdict { Done | Requeue | RequeueAfter(Duration) | Escalate(Reason) }
```

reconcile は **identity のみ**を受け取る。ペイロードや前回の判断は持ち込まない。判断は毎回
観測から導出する。これが level-triggered の核心である。

**永続状態の境界**(Authority 原則の精密化)。この「毎回観測から導出」が forge だけで完結するのは
Issue / Repo Kind の **workflow state**(ラベル・コメント・PR 状態)に限る。ここは forge が唯一の
権威で、meguri をいつ kill しても forge から復元できる。一方 Schedule Kind の最終発火時刻
(`schedule_state`)や workqueue の backoff / parked は forge から復元できない **ローカルな実行
進行**であり、これは sqlite に置く(cleaner の interval や `runs.cadence_label` と同種の、
既存でも認めている Authority 原則の例外)。したがって reconcile が Snapshot を作る際、
workflow state は forge から、実行進行(発火時刻・backoff)はローカルから読む。前者を落としても
forge から再導出でき、後者は「次いつ見るか」の最適化情報にすぎず、失っても resync が正しさを
取り戻す。

### 3. observe → decide → act

- **observe** は informer cache 型。resync ごとに一括クエリして snapshot を作る(loop ごとの
  個別 API 叩きをやめる)。
- **decide** は純関数 `next_step(Snapshot) -> Step`。壁時計にも I/O にも依存しない。同じ
  snapshot なら常に同じ Step を返す。
- **act** は Step を実行する副作用境界。

`next_step` が純関数であることで、「全状態にちょうど1つの所有 arm がある(所有の欠落も二重所有も
ない)」を**網羅 property test** で保証できる。BEHIND はこの property の穴として捉えられる —
「arm 済み × base 進行」に所有 arm が無かった。

### 4. Step 二相

```
Step = Agent(Role, Recipe)          // agent を起動して仕事をさせる
     | Op(UpdateBranch | ArmAutoMerge | MergePr | Finalize | Escalate)  // meguri 自身の API 操作
     | Wait(Reason)                  // 所有 arm が「今は動かない」と決めた状態(人間待ち等)
```

agent 起動(重い)と meguri 自身の軽い API 操作(Op)を型で分ける。**BEHIND の解は
`Op(UpdateBranch)` + arm 1本** — agent を起こさず、base を取り込んで再 arm するだけで閉じる。

`Wait` は「所有 arm が無い」状態**ではない**。人間待ちや parked も、その状態を所有する arm が
「今は何もしない」と明示的に決めた結果 `Wait(Reason)` を返している。つまり全状態は常にちょうど
1つの arm に所有され(決定 3 の invariant)、その arm の返り値が `Agent` / `Op` / `Wait` の
いずれかになる。`Wait` は「所有の欠落」ではなく「所有 arm が意図的に静止を選んだ」ことを表す。

### 5. ラベル 2 軸を spec / status に再解釈(ADR 0005 amend)

ADR 0005 の phase × ball 2 軸を、reconciler の語彙で **spec 軸 / status 軸**として再解釈する。
既存のラベル文字列(`meguri:*`)は変えないので、どのラベルがどちらの軸かを閉じて定める:

- **spec 軸**(望ましい状態の宣言 = 入力) — 人間や上流が「こうあってほしい」と書き込む面。
  **ここは観測から再構築してはならない**。人間の意思決定を含むため、forge に書かれた値そのものが
  権威になる。書き込みは **4 handshake の列挙制**にし、誰がどの遷移を書けるかを閉じた集合で
  管理する。spec 軸に属す既存ラベル:
  - phase トリガ: `meguri:plan` / `meguri:ready`(人間が起票を宣言する)。
  - **人間制御(ball 軸の一部)**: `meguri:hold`(停止の宣言)/ `meguri:needs-human`
    (人間の介入が要るという宣言)。**人間待ち・停止の権威はここ(forge の human-written
    ラベル)にある**。reconciler はこれを observe して `Wait(Reason)` を返すが、この意思決定を
    status から作り直すことは決してしない。
- **status 軸**(観測された現状 = 出力) — reconciler が観測(PR 状態 / CI / mergeable /
  run 履歴)から**常に再構築できることを義務**とする。status を一次ソースにしない。落ちても
  再導出できる。status 軸に属すのは進捗の観測値: `meguri:working`(run が走っている)、および
  phase の中間表示(`meguri:speccing` / `meguri:implementing` — PR の有無と run 履歴から
  復元可能)。

**再構築義務が効く範囲を過大評価しない**。「落ちても forge から再導出できる」のは status 軸の
進捗ラベルに限る。`hold` / `needs-human` はそもそも観測から作れない human 宣言なので status
軸に置かず、spec 軸の入力として forge を権威にする。これにより rollback / kill 復旧の安全性は
「status = 再導出、spec = 人間の書いた値が権威」と正しく二分され、人間の停止・エスカレーションを
機械が勝手に上書きすることはない。

### 6. dispatch = workqueue + resync

dispatch は workqueue と定期 resync の合成にする:

- **activeQ**(優先度 = マージ近接) / **backoffQ**(指数バックオフ) / **parked**(人間待ち)の
  3 キュー。
- **イベントは最適化、resync が正しさ**。イベントを取りこぼしても、resync が snapshot を作り
  直して必ず追いつく。現行の「登録順が優先度」(ADR 0001)は activeQ の優先度関数へ移す。

### 7. 移行は 5 スライスの縦切り(big-bang なし)

一度に全部を差し替えない。動くまま縦に薄く切って移していく:

1. **merge tail**(Op のみ、API コスト実測) — auto_merger / merge_watch を Op に載せ替え、
   BEHIND を `Op(UpdateBranch)` で閉じる。observe の一括クエリの API コストをここで実測する。
2. **Schedule Kind + repo-side config**(#165) — scheduler_fire を Schedule Kind に。
3. **queue + fixer 家族** — workqueue を導入し、fixer / ci_fixer / conflict_resolver を
   Issue Kind の arm に畳む。
4. **planner / worker / spec_worker / guard + Repo Kind** — 残りの重い agent 起動系と
   cleaner / triage / routing_drift を吸収。ここで旧 `Loop` trait を撤去する。
5. **config 键粒度**(ADR 0013) — 設定の粒度を新構造に合わせて整える。

各スライスは独立に review・rollback できる PR として起票する。

## Consequences

### 正の帰結

- 新トリガが loop 新設でなく arm 1本の追加で済む。BEHIND を含め、トリガの組み合わせ爆発が
  `next_step` の中に閉じる。
- 「全状態にちょうど1つの所有 arm」が property test で機械的に守られる。所有の欠落(BEHIND
  類)も二重所有(競合)も回帰で検出できる。
- observe が informer cache 化することで、loop ごとの重複クエリが1回の一括クエリに畳まれ、
  API コストが観測可能・制御可能になる。

### 負の帰結・リスク

- **公開 contract の変更**: `reconcile` / `Verdict` / `Step` / `Snapshot` は新しい公開型。
  既存の `Loop` trait を段階的に撤去する。移行中は新旧が併存する。
- **名前衝突**: `reconcile` は既に body-edit 再注意 sweep(#142)の名前で使われている。新しい
  reconcile 契約と紛らわしい。**方針は旧 sweep を `reconcile_body_edits` へ退避すること**で
  確定(#198 の spec で決定)。機械的な改名の適用はスライス 4 が行う。
- **移行中の二重権威リスク**: status を観測から再構築する義務(決定 5)が満たされる前に spec/
  status の再解釈を入れると、旧ラベル運用と新 status が食い違いうる。スライス順(merge tail →
  ... → guard)はこのリスクを最小化するために「forge 権威に触れない Op から始める」ようにした。

### supersede / 不変移設

- **supersede: ADR 0007(merge-watch)** — 全分類に行き先ができる。Conflict / RedCI は fixer
  recipe、Stuck は Escalate、HumanDisabled は Wait、Transient は backoff。merge-watch の
  「委譲か escalate か」の場合分けは `next_step` の arm に吸収される。
- **不変のまま移設**(判断は変えず、reconciler の語彙に載せ替えるだけ):
  - ADR 0003(auto-merge arm-only) — arm は `Op(ArmAutoMerge)`。
  - ADR 0009(orchestrator merge) — orchestrator の直接マージは `Op(MergePr)`。
  - ADR 0006 / 0008(内部 self-review) — self-review は Issue Kind 内の Step。
  - ADR 0001(scheduler-priority WIP-first) — 「登録順が優先度」は activeQ の**優先度関数**
    (マージ近接)へ移る。WIP 優先の原則自体は不変。
  - ADR 0005(labels two-axis) — phase × ball を spec / status に再解釈(amend であって
    reject ではない)。

### 承認後の動き

本 ADR 承認後、移行スライス 1〜5 を個別 issue として起票する。切り方と依存順も本 issue #198 の
決定物なので、起票 payload(kind・blocked_by・受け入れの芯)をここに残す:

1. **merge tail(Op のみ)** — kind: plan / blocked_by: なし。auto_merger / merge_watch を `Op`
   に載せ替え、BEHIND を `Op(UpdateBranch)` + 再 arm で閉じる。observe 一括クエリの API コストを
   実測。芯: BEHIND が回帰テストで閉じ、API コスト実測値が記録される。
2. **Schedule Kind + repo-side config(#165)** — kind: plan / blocked_by: [1]。scheduler_fire を
   Schedule Kind に。芯: cron 起票が Schedule Kind 経由で動き、既存の消化ループと非回帰。
3. **queue + fixer 家族** — kind: plan / blocked_by: [1]。workqueue(activeQ / backoffQ /
   parked)を導入し、fixer / ci_fixer / conflict_resolver を Issue Kind の arm に畳む。芯:
   3 fixer が arm 化され、「ちょうど1つの所有 arm」property test が緑。
4. **planner / worker / spec_worker / guard + Repo Kind** — kind: plan / blocked_by: [2, 3]。
   残りの重い agent 起動系と cleaner / triage / routing_drift を吸収し、旧 `Loop` trait を撤去。
   `reaper`(→ `Op(Finalize)`)・`decompose_materializer`(→ 分解提案への act)・body-edit
   `reconcile`(→ `reconcile_body_edits` へ退避)・tick 最上段の `ensure_project_clone`
   (→ Repo Kind の `Op(EnsureClone)`)もここで畳む。芯: default_loops・全 poll-tick sweep・
   bootstrap reconcile が消え、全 Kind が reconciler 経由。
5. **config 键粒度(ADR 0013)** — kind: plan / blocked_by: [4]。設定粒度を新構造に整える。芯:
   config が新構造に追随し hot reload 非回帰。

ADR 0013 / 0014 / 0015 / 0016(#197)の実装は上記スライスに合流する(独立起票しない)。
