# issue-224 spec — planner/worker/spec_worker/guard + Repo Kind 吸収、旧 Loop trait 撤去

ADR 0012 移行のスライス4。決定は一行で書ける。**残りの重い agent 起動系(planner / worker /
spec_worker / pr_reviewer / spec_fixer / plan_handoff)と Repo Kind(cleaner / triage /
routing_drift)を reconciler の arm / Op に畳み、poll-tick sweep(reaper / decompose_materializer
/ body-edit reconcile)と bootstrap の `ensure_project_clone` も畳み、その上で旧 `Loop` trait を
撤去して全 Kind を reconciler 経由にする。**

正は [ADR 0012](../adr/0012-loops-are-emergent-level-triggered-reconciler.md) §承認後の動き 4。
ここは決定を作る場ではなく、ADR 0012 が既に決めた設計を **どう実装で実現するか**を収束させる場で
ある。以下、割れうる分岐(A か B か)はこの spec ですべて倒す。

## スコープと深さ

- スコープ = ADR 0012 §承認後の動き **項目4(スライス4)のみ**。項目5(config 键粒度 / ADR
  0013)は別 issue(blocked_by [4])であり本 spec には含めない。issue タイトルの「S4/5」は
  「5スライス中の4」の意で、`unblocks: S5` と整合する。
- 深さ = **design spec**。理由: blast radius が広い(中核 trait `Loop` を撤去し dispatch を
  組み替え、全 Kind を reconciler 化する)。**veto 該当**: 公開 contract(`Loop` trait、CLI 面)
  に触れるため、後述の「移行とロールバック」節は必須。ただし後述のとおり **sqlite スキーマ変更は
  無い**(S3 の workqueue / backoff、既存の drift / schedule_state、forge 上の scan マーカーで
  足りる)。

## 決定0: 1本の spec にする(分解提案にしない)

最初に倒すべき分岐。本 issue を子 issue に再分解しない。理由は三つ。

1. **ADR 0012(#198)が既に1段分解済み**。#198 は移行を5スライスに切り、本 issue はその
   スライス4である。分解は一段のみ(planner の分解規約)であり、スライス4をさらに割るのは
   #198 の分解の二段目にあたる。ADR 著者が意図して5スライスに畳んだ切り方を崩さない。
2. **このスライスの価値は不可分**。受け入れの芯は「`default_loops`・全 poll-tick sweep・
   bootstrap reconcile が消え、全 Kind が reconciler 経由」。`Loop` trait の撤去は実装者が
   ゼロになって初めて可能で、途中の子 PR はどれも芯を達成しない中間状態にしかならない。
3. **設計は ADR 0012 で固定済み**。残る分岐は構造的・機械的で、独立に review/rollback したい
   アーキ分岐ではない。

## 決定1: Issue Kind は「3 観測 × 3 純関数 decider」を1モジュールに畳む

Issue Kind は issue identity(ラベル駆動)と PR identity(PR 状態駆動)の両方を所有する。今の
`issue_reconciler.rs`(S1–S3)は **PR 側のみ**を観測している(`observe_open_prs`)。planner /
worker は PR がまだ無い段階の issue ラベルで駆動されるので、PR 観測だけでは半分しか見えない。

- **分岐**: 単一の Snapshot に issue も PR も詰めるか、観測を分けるか。
- **決定**: **観測を3本、純関数 decider を3本**にし、enqueue / claim / backoff 機構は共有する
  (1 Kind = 1 モジュール `issue_reconciler`)。3本目は local mode 用(f1)。
  - `observe_open_issues` → `next_step_issue(IssueSnapshot) -> Step`。PR 前 / 非 open-PR の issue
    ライフサイクルを所有: `meguri:plan` → `Agent(Planner)`、`meguri:ready` → `Agent(Worker)`、
    spec PR が merged な `speccing` issue → `Op(Handoff)`(separate delivery、plan_handoff の吸収)。
    **body-edit signal は `next_step_issue` の返す arm ではなく、毎-resync の signal act に分離する**
    (finding 3、下記「body-edit signal の所有」)。
  - `observe_open_prs` → `next_step`(既存を拡張)。open PR のライフサイクルを所有: fixer 家族
    (S3)+ spec 段階 arm(spec_worker / pr_reviewer / spec_fixer)+ 承認済み分解提案の materialize。
  - `observe_local_tasks` → `next_step_local(LocalSnapshot) -> Step`。**local mode の
    `TaskKey::Local` identity を所有**(f1)。observe は既存の `task_source.discover(TaskKind::Work)`
    (= local tasks store)を bulk 観測として使う。local mode は forge が無く planner も PR も
    持たない(`planner::discover` は forge 無しで空を返す不変)ので、この decider は
    `Agent(Worker)` のみを出す。enqueue は `create_run_for_task`、dedup は既存の
    unique (project, loop, task) run index。

### 所有境界の正規化(f1 / f2)

3 decider が同じ identity に二重に enqueue しないことを **証明可能**にするため、所有をひとつの
全域分割で定める。**鍵は「その identity に *open な meguri PR* があるか」**である。

| identity の状態 | 所有する decider |
|---|---|
| `TaskKey::Local`(local mode、forge 無し) | `next_step_local` のみ(PR も issue observe も存在しない) |
| open な meguri PR を持つ issue | **PR 側 `next_step` のみ**(issue 側は `Skip("owned by its open PR")`) |
| open な meguri PR を持たない open issue | **issue 側 `next_step_issue` のみ**(PR observe に現れない) |
| closed / merged issue で local resource が残る（open PR なし） | `Op(Finalize)`(決定4、terminal 所有) |

**単一 issue snapshot で phase を1本に畳む(finding 2)**。前案は planner/worker の観測を
`task_source.discover(Plan)` と `discover(Work)` の **2本の per-label discover** に委ねていたが、
現行実装は discover ごとに片方のラベルだけを引き、active-run の unique index も **loop_kind ごとに
別**(`runs_active_issue`)なので、`meguri:plan` と `meguri:ready` が同じ issue に付くと両 discover が
候補を返し planner と worker の run が同 tick に立つ。既存 `claim` も「どちらかの trigger label が
ある」しか見ず arm を再検証しない。これでは受け入れ6(二重 enqueue なし)を満たせない。よって:

- `observe_open_issues` は **各 open issue のラベル集合を1回まとめて観測**し、issue ごとに1つの
  `IssueSnapshot` を作る(per-label の2本 discover をやめる)。
- `next_step_issue` は phase 優先順を閉じて定め **ちょうど1つの arm** を選ぶ:
  `hold`/`needs-human` → `Wait` > `plan` → `Agent(Planner)` > `ready` → `Agent(Worker)` >
  `speccing` → handoff 判定 > `implementing` → `Skip`(進行中)。手動ラベル drift は「どの1本か」を
  変えるだけで、返る arm は常に1つ。
- enqueue は **issue-wide の予約**でガードする: 既存 `issue_has_active_author_run`(loop_kind に
  依らず author lane の active run を1本でも見たら true。pr-reviewer は別 lane で除外)を
  enqueue 直前に確認し、planner を立てたら同 issue の worker は立てない(その逆も)。unique index は
  loop_kind ごとなので **これがない と二重取りを防げない** — 契約として明記する。
- claim は **arm-tagged 再検証**にする: 選んだ arm の trigger label(`plan` なら `plan`、`ready` なら
  `ready`)と後述のゲートを **書き込み直前に再確認**してから `working` を貼る(discover→claim 間の
  drift は benign race として Skip)。

却下した代替: PR 前段まで単一 Snapshot 型に統一。PR フィールドが空になり部分的で扱いづらい。
ここで畳むのは **issue-side の phase だけ**(PR 側は別 decider のまま)であり、ADR 0012 が Issue Kind
を「issue/PR identity で鍵る」と両建てで書くことと整合する。

### body-edit signal の所有(finding 3)

`implementing` issue は通常 open な実装 PR を持つので、所有境界では PR 側が所有する。ここに
`reconcile_body_edits` を `next_step_issue` の arm として置くと、境界を守れば実行されず、実行すれば
single-owner に反する — どちらも破綻する。

- **決定**: body-edit signal は **arm ではなく、毎-resync の signal act** にする(PR 側の
  `reclaim_stale_claims` / `clear_resolved_backoffs` と同じ category)。single-arm 所有分割の外にあり、
  `next_step_*` の返り値ではない。`implementing` ラベルの issue を(現 `reconcile.rs` と同じく)
  ラベルから読み、body digest で dedup し、**signal comment を出すだけ**(agent を起こさず enqueue も
  しない)ので、二重 enqueue も single-owner 違反も原理的に起きない。実行箇所は Issue Kind の
  **1箇所**(毎 resync に1回)に固定し、PR 側では回さない。
- **受け入れ**: open な実装 PR がある `implementing` issue で body を変えても signal が **二重に
  出ない**(1 resync 1 回)テストを追加(受け入れ14)。

### `speccing` handoff の観測(f3)

`observe_open_issues` は PR 状態を持たず、`observe_open_prs` は open PR しか返さない(merged PR は
そこから消える)ので、`speccing` issue の「spec PR が merged」を判定する契約を明示する。

- **分岐**: (A) `speccing` issue ごとに linked spec-PR branch の PR 状態を追加取得する / (B) merged
  PR を含む別 bulk observation を持つ。
- **決定**: **(A)**。現 `plan_handoff::process_issue` がまさに「planner が記録した spec-PR branch を
  引き当て、state == merged を確認する」targeted read をしており、`speccing` issue は少数なので
  per-issue の追加取得で十分。
- **所有境界との整合(finding 3)**: handoff は本質的に **spec PR が merged**(= もう open でない)
  ときの遷移である。open な spec PR は `observe_open_prs` に現れ、所有境界では **PR 側**が所有する
  (spec review 進行中 = pr_reviewer / spec_fixer arm の領分)。よって:
  - spec PR が **open** の `speccing` issue → issue 側は `Skip("owned by its open PR")`(所有境界の
    とおり。前案の issue 側 `Wait("spec PR in review")` は **到達不能なので削除**)。待機は PR 側の
    spec 段階 arm がすでに担う。
  - spec PR が **merged**(open PR 無し)→ issue 側 `Op(Handoff)`(→ ready)。
  - spec PR が **closed-unmerged**(open PR 無し)→ issue 側 `Skip`(人手領域)。
  つまり `spec_pr_state` は **open PR を持たない `speccing` issue** について merged / closed-unmerged
  を区別するためだけに使う(open は所有境界が PR 側に振るので issue 側の判断に入れない)。
- **テスト**: (a) open spec PR → issue 側 `Skip`(PR 側所有、ownership property test と同一規則)/
  (b) merged → `Op(Handoff)` / (c) closed-unmerged → `Skip`。ownership property test と handoff test が
  **同じ規則**(open は PR 側、merged/closed は issue 側)を検証する。

### 既存 discovery の安全ゲートを漏れなく保持する(finding 3)

planner / worker の enqueue は「phase ラベルと `issue_busy`」だけでは不十分で、現
`LabelTaskSource::discover` / `claim`(`src/tasks.rs`)が適用している全ゲートを保たないと、blocked
issue・not-before 待ち・cadence 超過・body 未変更 issue を誤って enqueue してしまう。

前ラウンドの finding では「`task_source.discover`/`claim` を丸ごと再利用してゲートを保て」と求め
られたが、それは per-label discover であり finding 2 の二重取りを生む。両者を両立させる形に畳む:
**per-label discover は使わず、`task_source` が持つ *ゲート述語* を単一 issue snapshot に適用する。**

- **決定**: `next_step_issue` を「1 issue の全ラベル + ゲート入力」の純関数に保ち、選んだ arm に
  対して以下のゲートを効かせる(現 `LabelTaskSource` と **同じ判定関数**を呼ぶ。ラベル走査ループ
  だけ作り直さない):
  - `hold` / `working` skip。`working` は run-liveness の `issue_busy` とも二重化。
  - `already_shipped`(body digest、`implementing` 済みや同一 body の再処理抑止)。
  - not-before(`cadence::parse_not_before` / `not_before_wait`)。
  - `blocked_by` 依存(依存 issue 未 close はまだ動かさない)。
  - cadence 窓(`evaluate` が bucket を予約、`limit - consumed` を超えない)。選んだ 1 arm 分だけ
    予約するので、per-label 2 本のときのような二重予約は起きない。
  - **claim 直前の arm-tagged 再検証**(所有境界の項で既述): 選んだ arm の trigger label と上記
    ゲートを **書き込み直前に再確認**してから `working` を貼る。
  - **cadence run-stamp**: run は `create_run_for_loop_cadence` で予約 bucket を刻み、窓を
    正しく消費する(手動 `run` の二重消費防止と同型)。
- **受け入れ**: blocked issue / not-before 待ち / cadence 窓超過 / body 未変更 issue が enqueue
  **されない**、かつ `plan`+`ready` 併記 issue で **planner か worker のどちらか1本しか立たない**
  ことの回帰テストを追加(受け入れ6・13)。
- local decider(`observe_local_tasks`)も local task source の同じゲート述語を通す。

## 決定2: spec 段階の 3 loop は PR 側 next_step の arm にする

`Arm` を拡張: `SpecWorker` / `PrReviewer` / `SpecFixer` を追加。Snapshot に spec 段階のフィールド
(`spec-reviewing` / `spec-ready` の有無、`meguri/pr-review` commit status、combined delivery か、
spec-fix budget 到達か、decompose 提案か)を足す。

- 各 arm のトリガ(現状の discover 条件)を **snapshot のフィールドに移し、`next_step` の枝に**
  する。既存の budget(`spec_fixer::MAX_SPEC_FIX_RUNS = 3`)は fixer 家族と同じく「症状が残る
  まま予算切れ → `Op(Escalate)`」に載せる。
- **pr_reviewer の特殊性を保つ**: pr-reviewer は `pr-review` lane(author lane と別)で走り、
  head-SHA ごとの `meguri/pr-review` commit status で dedup する。fixer 家族の claim/backoff
  には乗せない。enqueue ゲートは「その issue に active な pr-reviewer run が無く、head にまだ
  pr-review status が無い」。`issue_busy` は元々 pr-reviewer を除外しているので、review lane は
  author work と並走できる(不変)。
- **spec_worker は combined delivery 限定**(不変)。`spec_worker_owns`(既存)で fixer 家族/
  merge tail が触れない不可侵は維持しつつ、spec-ready かつ非分解提案の PR に対して
  `Agent(SpecWorker)` を返す。
- 各 loop の `run_flow`(Flavor)/ `run_pr_reviewer` の recipe 本体は不変。arm は run を
  enqueue するだけで、dispatch は決定8の recipe テーブルが担う。

## 決定3: Repo Kind reconciler を新設する(`repo_reconciler.rs`)

`schedule.rs`(S2)/ `issue_reconciler.rs` に倣い新モジュールを作る。

- 観測は repo scope: report issue(clean / triage)、default-branch head、config interval、drift
  サンプル。純関数 `next_step_repo(RepoSnapshot) -> RepoStep`。
- **arm**(重い agent): `Cleaner` / `Triage`。scan due の判定(cleaner の `needs_scan`: head
  マーカー + interval、triage の `scan_due`: interval + 新規 issue / backlog / drift)を
  `next_step_repo` の純判定に移す。scan マーカーは report issue の body(forge)にあるので
  **cadence 用の新規 sqlite は不要**(ローカル進行の Authority 例外に当たらない)。triage-auto は
  spec 軸への書き込み(**ADR 0012 決定5 が定める 4 handshake の1メンバ**。triage-auto の挙動自体は
  ADR 0017)である点を doc に明記する。
- **Op**(軽い、agent 無し): `RoutingDrift`(純 sqlite の drift 再計算 = 現 `routing_drift::sweep`
  をそのまま act 本体に)/ `EnsureClone`(決定6)。
- **Repo Kind は read-only ではない**(ADR 0012 決定1): cleaner はレポート issue 1本、triage は
  提案/昇格ラベル、drift は event を書く。書き込みは全て `Op` / arm の act に閉じる。

## 決定4: reaper を `Op(Finalize)` に、body-edit reconcile を `reconcile_body_edits` に畳む

- **reaper → `Op(Finalize)`**(f4 で契約を具体化): reaper の pane/worktree/merged-branch 回収
  ロジック本体は関数を保ったまま `Op(Finalize)` の act から呼ぶ。closed issue は
  `observe_open_issues` に出ないので、Finalize 専用の観測を定める。
  - **observe**: `observe_terminal_resources(deps) -> Vec<TerminalCandidate>`。meguri の
    **ローカル資源登録**(mux pane・worktree ディレクトリ・`runs` 記録から引く issue→資源の対応)を
    列挙し、その issue が **open-issue 観測に現れない**(= closed/merged)ものを candidate とする。
    現 reaper の discover(pane/worktree を数え上げ issue の closed 性を突き合わせる)と同型。
  - **local / forge-less の除外(finding 4a)**: forge が無い local mode、および `TaskKey::Local` の
    identity は **terminal 観測に載せない**。local には issue lifecycle が無く、`deliver = "branch"`
    の成果物はブランチ + worktree そのものなので、「open issue が無い」を理由に回収してはならない。
    これは現 reaper の `deps.forge.is_none() → StateUnknown`(非回収)ガード(`reaper.rs`)を
    そのまま契約に写したもの。active run が所有する資源も従来どおり除外(`ActiveRun`)。
  - **open PR の除外(finding 4b)**: GitHub は issue を閉じたまま、その issue を参照する open な
    meguri PR を残せる(closed issue × open PR)。この資源は **PR 側が所有**(fixer / recipe が
    worktree の実装文脈を使う)ので、Finalize の候補から外す。契約: candidate の canonical issue に
    **open な meguri PR がある間は Finalize しない**(PR が terminal になって初めて Finalize 対象)。
    これは所有境界表の「open な meguri PR を持つ identity は PR 側のみ」を terminal 観測にも徹底する
    もの。active run が無い一瞬に worktree を奪って次の fixer の文脈を失う事故を防ぐ。
  - **頻度**: 毎 resync。現 reaper は毎 tick 走る。Finalize は Issue Kind reconciler の
    **毎-resync パス**(`reclaim_stale_claims` と同列)として呼び、この頻度を保つ。
  - **decide の純関数入力**: candidate が持つ資源集合と terminal フラグ。判定は自明(terminal かつ
    資源が残る → 回収)なので `next_step` は `Step::Op(Finalize)` を返す。
  - **所有境界**: terminal に達した issue identity は Issue Kind が所有し、その唯一の arm が
    Finalize(ADR 0012「issue が terminal に達したときの `Op(Finalize)`」)。open PR / open issue
    の decider とは「issue が terminal か」で排他。
  - **回帰テスト**: (a) closed issue に live な pane/worktree が残る状態 → Finalize → 回収される、
    (b) local mode の worktree/branch は open issue が無くても Finalize されない(既存の非回収保証)、
    (c) **closed issue × open な meguri PR** では Finalize されず PR 側が資源を保つ、の三つを追加。
- **`reconcile.rs` → `reconcile_body_edits.rs`**: 機械的改名(module 名 + `sweep`)。新しい
  `reconcile(id)` 契約(ADR 0012 決定2)との名前衝突を解消する。挙動は不変(`implementing` issue
  の body 変化 → signal comment、agent は起こさない)。scheduler の独立 sweep 呼び出しをやめ、
  Issue Kind reconciler の毎-resync act(`reclaim_stale_claims` と同列)として呼ぶ。
- **decompose_materializer → PR 側 act**: PR 側観測が「spec-ready かつ decompose 提案」の PR を
  見たら materialize の act(現 sweep 本体)を回す。`spec_worker_owns`(combined && spec-ready)
  との区別は `is_decompose_proposal` で行い、提案は spec_worker より先に materialize 枝へ振る。

## 決定5: plan_handoff を issue 側の `Op(Handoff)` に畳む

separate delivery で spec PR が merged になった `speccing` issue を `ready` に進める現 sweep を、
`next_step_issue` の枝 `Op(Handoff)`(ラベル遷移のみ)にする。トリガの観測は決定1の `spec_pr_state`
(f3)。挙動不変。

## 決定6: EnsureClone を Repo Kind の `Op` に統一し、bootstrap 専用経路は残さない(f5)

現状 `ensure_projects_ready()` が tick の**最初**に走り、`repo_path` に触れる全下流(discover /
redispatch / 各 sweep)をゲートする `ready` 集合を作る。f5 の指摘は「Op にしつつ scheduler が
`ensure_ready` を直接呼ぶ」構成が **通常の observe→decide→act と bootstrap gate を二重に残す**こと。
これを **ひとつの契約**に畳む。

- **分岐**: (A) EnsureClone を「特別な bootstrap ゲート」として scheduler に残す / (B) EnsureClone を
  Repo Kind reconcile の **通常の第一 Op** にし、scheduler は他 Kind より先にその Op を回すだけに
  する(順序は「clone が repo_path 仕事の前提」という依存であって、bespoke ゲートではない)。
- **決定**: **(B)**。既存の `gitops::CloneHealth`(`Healthy` / `Absent` / `Broken(String)`、
  f7)を **そのまま**使い、新しい状態名は導入しない。単一契約は以下。managed clone でない project
  (`repo_path` 明示)は clone 不要なので常に ready。
  - **observe**: `RepoSnapshot.clone_health: gitops::CloneHealth` を managed-clone パスの観測
    (既存 `gitops::clone_health`)から作る。3 状態を漏れなく持つ:
    - `Healthy` — clone 実体化済み。
    - `Absent` — 未 clone(または空ディレクトリ)。
    - `Broken(why)` — 読めない / bare でない / slug 不一致など。`why: String` は理由をそのまま保持。
  - **decide**: `next_step_repo` は clone_health で網羅 match する(property test も同型で網羅):
    - `Absent` → `Op(EnsureClone)`(clone を実体化させる)。
    - `Healthy` → clone 済みなので当該 tick は ready。cleaner / triage / drift の判定へ進む。
    - `Broken(why)` → `Op(EnsureClone)` を返す。現 `gitops::ensure_bare_clone` は `Broken` を
      `bail!` する(自動修復しない)ので、この act は失敗し project は not-ready になる。`why` は
      失敗理由として `repo.clone.failed` event と `meguri doctor` に載せる(下記)。**`meguri why` の
      対象ではない**: ADR 0016 の identity 集合は issue / PR / run / local task に閉じており、repo /
      project はそこに含めない。clone 失敗の人間向け面は従来どおり `doctor`(ADR 0018)。
  - **act**: `Op(EnsureClone)` の act は現 `ensure_project_clone` 本体(= `gitops::ensure_bare_clone`
    経由)。`Absent` は clone して `Healthy` へ、`Broken(why)` は `why` を付けて `Err` を返す
    (`repo.clone.failed` を毎失敗 tick emit、level-triggered)。`Healthy` は no-op。
  - **readiness 契約(単一)**: scheduler は tick 先頭で `repo_reconciler::reconcile_ready(deps)` を
    呼ぶ。これは `next_step_repo` の **EnsureClone 部分のみ**を評価・act し、`ready:
    HashSet<project_id>` を返す。**ready に入る条件は「act 後に `Healthy`」の一点**:
    - `Healthy`(元から / clone 成功後)→ ready。
    - `Absent` で clone 失敗 → not-ready(除外)。
    - `Broken(why)` → act が `Err(why)` を返し not-ready(除外)。
  - **失敗時の遮断**: ready から外れた project は redispatch / 新規 enqueue / 全 Kind の処理が当該
    tick で止まる(現 `ensure_projects_ready` の除外と同一)。`Broken` の理由 `why` は握り潰さず
    `repo.clone.failed` event に残し、`meguri doctor`(ADR 0018 の clone 面)が表示する。
  - 芯: scheduler 固有の bootstrap reconcile 経路は消え、clone 実体化は Repo Kind の第一 Op として
    表現される。scheduler が先に呼ぶのは「順序依存」であって二重ロジックではない。

## 決定7: 旧 `Loop` trait を撤去する

- `trait Loop`(mod.rs:449)、`default_loops()`(mod.rs:488)、`Scheduler.loops` フィールド、
  `Scheduler::discover`(loop.discover を回す機構)、`Target`、`OpenPrCache` の per-tick 共有を
  撤去する。10 個の `impl Loop` を落とす(recipe 本体 `run_*` は残す)。
- 各 loop の discover はどこにも残らない: fixer 家族 = S3 済み、planner/worker/spec_worker/
  pr_reviewer/spec_fixer/cleaner/triage = 本スライスで reconciler の arm 化。
- self_review は **もともと Loop ではない**(ADR 0006/0008 の内部フェーズ)。撤去された trait を
  参照していないことを確認するだけ(挙動変更なし)。

## 決定8: dispatch は `Loop` 解決から recipe テーブルに置き換える

`Scheduler::dispatch` は今 `loops.iter().find(kind).drive()` で run を recipe に繋ぐ。これを
**自由関数 `run_recipe(deps, run_id, loop_kind) -> Result<WorkerOutcome>`** に置き換え、
`loop_kind` を既存の recipe 入口へ match する:

| loop_kind | 入口(既存) |
|---|---|
| worker / planner / spec-worker / spec-fixer | `run_worker` / `run_planner` / `run_spec_worker` / `run_spec_fixer`(`run_flow`) |
| pr-reviewer | `run_pr_reviewer`(独自 step machine) |
| conflict-resolver / ci-fixer / fixer | 既存 recipe 入口 |
| cleaner / triage | `run_cleaner` / `run_triage` |

未知 kind は現状どおり warn + skip。`dispatch_rank`(文字列キー、mod.rs:469)と
`redispatch_interrupted`(workqueue activeQ、loop_kind 駆動)は **そのまま生き残る**。enqueue は
全て reconciler(`create_run_for_loop*`)に一本化される。

## 決定9: ADR 0016(operator surface)を land し `meguri why` を新設する

[ADR 0016](../adr/0016-operator-surface-run-why-attach.md)(本 PR 同梱、旧 #197 合流)。operator の
介入面を identity への 3 動詞 `run` / `why` / `attach` に確定し、役割別動詞 `plan`/`impl`/`review`/
`fix` は採らない。

### 3 動詞が共有する identity セレクタ(finding 1)

ADR 0016 は 3 動詞すべての identity を {issue / PR / run / local task} に閉じた。よって **3 動詞で
1つの型付き identity セレクタを共有**する。相互排他フラグを **ちょうど1つ**取る(clap `ArgGroup`、
required、複数不可):

`(--issue <N> | --pr <N> | --run <id> | --task <id>) [--project <P>]`

- 型付きなので issue 番号・PR 番号・run id・local task id が同値でも曖昧にならない(解決優先順は
  不要)。run id は実在形式 `run-<8hex>`(`src/store/runs.rs`)。`--project` は複数 project の
  曖昧さ解消のみ(identity ではない、決定6 finding 8 と整合)。
- **セレクタ種別は捨てない(finding 1)**: `--pr` / `--run` の identity を **issue に潰さない**。所有
  境界(決定1)は「open な meguri PR は PR 側 `next_step` が所有」なので、PR identity を canonical
  issue に畳んで `next_step_issue` に送ると、conflict の open PR に `run --pr` しても issue 側は
  `owned by its open PR` で止まり PR 側 arm も呼ばれない、という欠落が出る。よって **run / why は
  セレクタ種別を保って所有 decider へルーティングする**(下記各節)。`attach` だけはペイン解決に
  canonical issue を使ってよい(所有 decider に依らずペインは identity で決まる)。
- **後方互換**: 既存の `meguri run --issue N`(ADR 0011 の手動 run 契約)と `meguri attach <位置引数>`
  はそのまま受理し、新セレクタはそれを含む上位集合として足す(既存フローを壊さない)。

### `run` を役割非依存にし identity セレクタを取る(finding 1)

現 `meguri run --issue N`(`src/app.rs::cmd_run`、`Run { issue: i64 }`)は loop_kind を `"worker"` に
固定し `run_worker` を直接呼ぶ。ADR 0016 の「役割を指定せず reconciler が次の役割を決める」に反する
(worker 固定なので `plan` フェーズの issue にも worker を起こす)。

- **決定**: `run` は上記セレクタを受け、**所有 decider を選んで** fresh observe → decide → その
  arm を dispatch する(役割非指定)。ルーティング(finding 1):
  - `--pr N` → その PR を fresh observe し **PR 側 `next_step`** を回す。`Agent(arm)`(ConflictResolver
    / CiFixer / Fixer / SpecWorker / PrReviewer / SpecFixer)を dispatch。
  - `--run <id>` → その run の **保存 `loop_kind` を保持**する。既存 run を対象にするので、その run を
    resume/redispatch する(loop_kind を捨てて issue 側へ送らない)。
  - `--issue N` → 所有境界に従う: その issue に **open な meguri PR があれば PR 側**、無ければ
    **issue 側 `next_step_issue`**。
  - `--task <id>` → **`next_step_local`**(local project も入力経路を開ける。現 `cmd_run` の local
    bail をやめる)。
  - `Agent(arm)` はその arm の run を作って dispatch。`Op` / `Wait` / `Skip` はその Step と理由を
    表示して終わる(起こす agent が無い)。
- **手動 override が bypass するゲート(finding 2)**: 既存 `meguri run --issue N` は discovery の
  **`already_shipped` 抑止を迂回**し(`src/store/runs.rs`)、**cadence 窓も bypass**(ADR 0011)して
  run を作る。fresh observe をそのまま通すと、成功済み・本文未変更・cadence 窓満杯の issue で
  `Skip`/`Wait` になり **再実行できなくなる**。よって decider に **観測モード**を渡す:
  - `next_step_issue(snapshot, Mode)`。`Mode::ManualRun` では **discovery throttle** ゲート
    (`already_shipped` / cadence 窓)を **skip** し、phase が示す arm を返す。**not-before は
    skip しない** — ADR 0011 が手動 run に許す bypass は cadence 窓だけであり、not-before は
    fail-closed 契約(解析不能・未来時刻なら実行しない)として `LabelTaskSource::claim` の
    書き込み直前再検証にも刻まれている。解禁前の issue は手動でも実行しない。
  - `Mode::Reconcile`(watch の通常経路)では従来どおり全ゲートを適用。
  - **手動でも保持する安全ゲート**: `hold` / `needs-human`(人間の停止宣言、spec 軸)と `issue_busy`
    (二重起動防止)は ManualRun でも尊重する。cadence は **stamp して消費に数える**
    (`create_run_for_loop_cadence`、同日 `watch` の二重消費防止)のは不変 — bypass するのは「窓が
    満杯なら止める」判定だけで、消費計上は残す。
  - つまり **override = already_shipped + cadence 窓の迂回**、**保持 = 人間停止 + 二重起動 +
    not-before(fail-closed) + 消費計上**。

### `attach` に identity セレクタを足す(finding 1)

現 `meguri attach <run|issue> [--review]`(`Attach { run: String }`)は run id か issue 番号の
位置引数だけ。ADR 0016 の 4 identity に届かせるため、上記セレクタを追加で受理する(位置引数は
後方互換で残す)。

- **決定**: `attach` はセレクタの identity を **その live ペインに解決**する: issue → author lane
  ペイン(`--review` で review lane)、run → その run のペイン、`--pr N` → PR の canonical issue の
  ペイン、`--task id` → local task のペイン。解決規則は既存 `resolve_attach_pane`(`src/app.rs`)を
  PR / task フラグに広げる。書き込みは無い(現状どおり)。
- 変更箇所・受け入れ10 に `run` / `attach` のセレクタ対応を含める。

### `why` を新設(旧 f6)

読み取り専用の観測窓。上記の共有 identity セレクタを取る(旧案の位置引数 `<id>` + `--pr` は clap で
解析不能だった)。

- **入力**: 共有セレクタ `meguri why (--issue|--pr|--run|--task <id>) [--project <P>]`。
- **例**: `meguri why --issue 224` / `meguri why --pr 231` / `meguri why --run run-1a2b3c4d` /
  `meguri why --task 42 --project foo`。
- **所有 decider へルーティング(finding 1)**: `run` と同じ規則で、セレクタ種別と所有境界に従って
  **実際に所有する decider の Snapshot / Step を表示**する。`--pr` は PR 側 `next_step`、`--run` は
  その run の loop_kind の文脈、`--issue` は open PR の有無で PR 側 / issue 側、`--task` は local。
  issue に潰して常に issue 側判断を出す、ということはしない。
- **観測方式**: **fresh observation**(informer cache でなく1回きりの observe を今回す)。read-only。
- **出力**: 解決した identity、所有する Kind / decider、`next_step` が返した Step、その理由文字列を
  人間可読で出す(issue/PR は Snapshot の主要フィールドも)。
- **read-only 検証**: CLI テストで `why` が forge 書き込みも run 生成もしないことを assert
  (FakeForge が mutating 呼び出しを記録せず、`runs` に新行が増えない)。

## 決定10: step policy を新 arm に広げる

S3 の `StepPolicyConfig`(conflict_resolver / ci_fixer / fixer の bool + `apply_policy`)を、新 arm
(planner / worker / spec_worker / pr_reviewer / spec_fixer / cleaner / triage)にも広げ、ADR 0026 の
「無効 arm の `Agent` を `Wait(PolicyDisabled)` に落とす」統一 kill switch を維持する。ただし
triage の `mode`(off/report/advise/auto)や pr_reviewer impl 側の opt-in など **既存の config 意味は
snapshot の trigger 条件として温存**する(bool へ潰さない — config 键粒度の再設計は S5 / ADR 0013)。

## 変更箇所

- `src/engine/issue_reconciler.rs` — 観測3本(`observe_open_issues` / `observe_local_tasks`(f1) /
  `observe_terminal_resources`(f4))、`IssueSnapshot` + `next_step_issue`(`spec_pr_state` を含む,
  f3)、`LocalSnapshot` + `next_step_local`(f1)、`Arm` に SpecWorker/PrReviewer/SpecFixer 追加、
  PR 側 Snapshot 拡張、`Op` に Finalize / Handoff 追加、reaper 回収 act、decompose materialize act、
  `reconcile_body_edits` の毎-resync 呼び出し。肥大化するなら `issue_reconciler/{mod,pr,issue,
  local}.rs` へ分割してよい(1 Kind = 1 モジュール群)。
- `src/engine/repo_reconciler.rs`(新規) — `RepoSnapshot`(`clone_health` を含む)/ `next_step_repo`
  / arm(cleaner, triage)/ Op(routing_drift, ensure_clone)、`reconcile_ready`(決定6の単一
  readiness 契約 = EnsureClone Op を評価・act し `ready` を返す)。
- `src/engine/reconcile.rs` → `reconcile_body_edits.rs`(改名)。
- `src/engine/mod.rs` — `Loop` trait / `default_loops` / `Target` / `OpenPrCache` 撤去、
  `run_recipe` 追加、`ensure_project_clone` は repo_reconciler へ移設。
- `src/engine/scheduler.rs` — `loops` フィールド撤去、tick の sweep ブロックを
  reconciler 呼び出し(issue / repo / schedule)に整理、`discover` 撤去、`dispatch` を
  `run_recipe` に、`ensure_projects_ready` を `repo_reconciler::reconcile_ready` に。
- `src/engine/{planner,worker,spec_worker,pr_reviewer,spec_fixer,cleaner,triage}.rs` —
  `impl Loop` を落とし、discover を撤去、recipe 入口 `run_*` を残す。
- `src/engine/{reaper,decompose_materializer,plan_handoff,routing_drift}.rs` — sweep 本体を
  act として保ち、scheduler の独立呼び出しを撤去。
- `src/config.rs` — `StepPolicyConfig` に新 arm の bool を追加。
- `src/cli.rs` / `src/main.rs` / `src/app.rs` — 共有 identity セレクタ
  `(--issue|--pr|--run|--task)` を `Why`(新設)/ `Run` / `Attach` に足す(finding 1)。`cmd_run` /
  `cmd_why` は **所有 decider へルーティング**(`--pr` は PR 側、`--run` は保存 loop_kind を保持、
  `--issue` は open PR の有無、`--task` は local)し、issue に潰さない。`cmd_run` は worker 固定を
  やめ観測 arm を dispatch、`Mode::ManualRun` で discovery throttle(already_shipped / cadence 窓)を
  bypass しつつ hold/needs-human・not-before(ADR 0011 fail-closed)と cadence 消費計上は保持
  (finding 2)。
  `resolve_attach_pane` を PR / task フラグに拡張。既存の `run --issue` / `attach <位置引数>` は
  後方互換で維持。
- `src/tasks.rs` — planner / worker arm が使う **ゲート述語**(`already_shipped` / not-before /
  `blocked_by` / cadence `evaluate`)を単一 issue snapshot に効かせる接続。per-label discover は
  使わず、issue-wide 予約(`issue_has_active_author_run`)と arm-tagged claim で二重取りを防ぐ
  (finding 2 / 3)。
- `docs/adr/0016-operator-surface-run-why-attach.md`(本 PR 同梱)。

## 受け入れ基準

1. `Loop` trait / `default_loops` / `Scheduler.loops` / `Scheduler::discover` が消えている
   (grep で0件)。
2. scheduler tick から独立 sweep 呼び出し(`reaper::sweep` / `plan_handoff::sweep` /
   `decompose_materializer::sweep` / `routing_drift::sweep` / `reconcile::sweep`)が消え、
   全て reconciler(issue / repo)の act / arm 経由になっている。
3. `ensure_project_clone` の scheduler 固有経路が消え、`Op(EnsureClone)` として表現されている。
   tick の ready ゲート順序は不変(clone 未実体化の project は当該 tick で除外)。`next_step_repo` は
   既存 `gitops::CloneHealth`(`Healthy`/`Absent`/`Broken`)を網羅 match し、ready 条件は「act 後
   `Healthy`」、`Broken(why)` の理由は event に残る(f7)。
4. `reconcile` 名前衝突が解消(body-edit sweep は `reconcile_body_edits` に改名)。
5. planner(`meguri:plan`)/ worker(`meguri:ready`)/ spec_worker(combined の spec-ready PR)/
   pr_reviewer / spec_fixer / cleaner / triage が **全て reconciler の arm** から enqueue され、
   従来と同じラベル遷移・budget・escalation を保つ(既存の各 loop テストが緑)。
6. issue 側 `next_step_issue` / repo 側 `next_step_repo` / local 側 `next_step_local` それぞれに
   「ちょうど1つの所有 arm(所有の欠落も二重所有も無い)」の網羅 property test がある。**加えて
   f2**: issue phase(`plan`/`ready`/`speccing`/`implementing`/`hold`/`needs-human` の冪集合)× PR
   有無・状態(無 / open 各状態 / merged / closed-unmerged)を列挙し、`next_step_issue` と PR 側
   `next_step` のうち **enqueue を生む Step(Agent か mutate する Op)を返すのは高々1つ**で、それが
   所有境界表の owner と一致することを assert する結合 property test がある(手動ラベル drift 込み)。
   **加えて finding 2**: `meguri:plan` と `meguri:ready` を同時に持つ issue で、issue-wide 予約
   (`issue_has_active_author_run`)により **planner か worker のどちらか1本しか active run にならない**
   ことの回帰テストがある(単一 snapshot で phase を1本に畳む契約)。
7. **f1**: local mode(forge 無し)で `TaskKey::Local` の `ready` 相当タスクが `next_step_local` →
   `Agent(Worker)` で enqueue・drive され、`Loop` trait 撤去後も回帰しない回帰テストがある。
8. **f3 / finding 3(handoff の所有整合)**: `speccing` issue の spec PR が open / merged /
   closed-unmerged の3状態で、それぞれ **open → issue 側 `Skip`(PR 側所有)** / **merged →
   `Op(Handoff)`(→ ready)** / **closed-unmerged → `Skip`** になる handoff テストがある。ownership
   property test と同じ規則(open は PR 側、merged/closed は issue 側)を検証する。
9. **f4**: closed issue に live な pane/worktree が残る状態から `Op(Finalize)` が発火し、pane /
   worktree / merged branch が回収される回帰テストがある。
9b. **finding 4**: (a) local mode(forge 無し)/ `TaskKey::Local` の worktree・branch は open issue が
   無くても Finalize されない、(b) **closed issue × open な meguri PR** でも Finalize されず PR 側が
   資源を保つ、の回帰テストがある。
10. operator 面が `run` / `why` / `attach` の 3 動詞に確定し、**3 動詞が共有する型付き identity
   セレクタ `(--issue|--pr|--run|--task) [--project]`** を取る(finding 1)。`meguri why` は Step +
   理由を read-only で表示(**f6**: forge 書き込みも run 生成もしないことを CLI テストで assert)、
   `meguri run` は worker 固定をやめ fresh observe で選ばれた arm を dispatch、`meguri attach` は
   4 identity を live ペインに解決する。既存の `run --issue` / `attach <位置引数>` は後方互換で動く。
   役割別動詞は追加されていない。ADR 0016 が land。
11. 統合テスト(`tests/*.rs`、実 tmux + FakeForge/FakeMux)で plan→spec→ready→impl→merge の
   通し、虚偽申告訂正、crash recovery が `Loop` trait 撤去後も回帰しない。
12. `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo nextest run` /
   `cargo test --doc` が通る。
13. **finding 3(ゲート保持)**: 既存 discovery ゲート(hold/working / already_shipped(body digest)/
   not-before / `blocked_by` 依存 / cadence 窓 / claim 直前の arm-tagged 再検証 / cadence run-stamp)が
   保持され、blocked issue・not-before 待ち・cadence 窓超過・body 未変更 issue が **enqueue されない**
   回帰テストがある。
14. **finding 3(body-edit)**: `reconcile_body_edits` は arm ではなく毎-resync の signal act であり、
   open な実装 PR がある `implementing` issue で body を変えても signal が **1 resync に1回しか出ない**
   (二重実行しない)回帰テストがある。
15. **finding 1(PR identity のルーティング)**: `--pr` / `--run` の identity を issue に潰さず所有
   decider へ送る。回帰テスト: conflict の open PR に `meguri run --pr N` で **PR 側 `ConflictResolver`
   arm** が dispatch される / `meguri why --pr N` が **PR 側 Snapshot / Step** を表示する /
   `meguri run --run <fixer run>` が保存 `loop_kind` を保って resume する(issue 側へ流れない)。
16. **finding 2(手動 run の override)**: 成功済み・本文未変更・cadence 窓満杯の issue で
   `meguri run --issue N` が **`Skip`/`Wait` にならず arm を dispatch** する(`Mode::ManualRun` が
   discovery throttle を bypass)。ただし `hold`/`needs-human` の issue では **起動しない**(人間停止を
   尊重)、**not-before が未来時刻の issue でも起動しない**(ADR 0011 fail-closed の保持)、かつ
   cadence は **消費に計上**される、を回帰テストで固定する。

## 移行とロールバック(veto により必須)

- **永続状態 / スキーマ**: **新規 sqlite マイグレーションは無い**。workqueue / backoff(S3、
  migration 0016)、drift(`routing_drift_samples` / `record_drift`)、`schedule_state` は既存の
  まま。cleaner / triage の scan cadence は report issue の body マーカー(forge)にあり新規テーブル
  不要。`runs.loop_kind` は routing キーとして不変。
- **ロールバック**: スキーマ変更が無いため、ロールバックは **ブランチのコード revert のみ**でデータ
  移行を伴わない。`loop_kind` が安定キーなので、revert 時点で in-flight な run は新旧どちらの
  dispatch 経路(旧 `Loop::drive` / 新 `run_recipe`)からも同じ recipe に解決でき、run の取りこぼしは
  起きない。
- **移行中の二重権威リスク**: label の spec/status 再解釈(ADR 0012 決定5)は本スライスで
  **ラベル文字列を書き換えない**(概念的な再解釈は ADR 0005 amend で済んでいる)。よって label
  移行は無い。observe→再導出の義務は status 軸に限る(spec 軸 = 人間の書いた値が権威)ことを
  新 snapshot でも守る。

## observability

- 既存 `reconciler.*` event を再利用。`reconciler.enqueued`(arm=loop_kind)は新 arm を自動的に
  覆う。`reconciler.policy_disabled` を新 arm にも出す。issue 観測は `reconciler.observe_cost`、
  repo 観測は `repo_reconciler.observe_cost` を新設。Finalize / EnsureClone / materialize /
  body-edit は既存 event(`pane.reclaimed` / `worktree.reclaimed` / `repo.cloned` /
  `issue.body_changed` 等)を保つ。
- `meguri why` はこの観測を人間の読み取り面に接続する主要な UX。

## test strategy

- **純関数 property test**: `next_step_issue` / `next_step_repo` / `next_step_local` の全域性
  (既存 PR 側 property test と同型)。`next_step_repo` は `CloneHealth` の3状態
  (`Healthy` / `Absent` / `Broken`)を網羅し、readiness(act 後 `Healthy` のみ ready)を assert
  する(f7)。**結合 property test**(f2): 2 decider が同一 issue に二重の enqueue を出さないこと
  (受け入れ6、drift 込み)。
- **挙動保存**: 各 loop の既存ユニット/挙動テストを、enqueue 経路を reconciler に差し替えても
  緑のまま通す(planner は `meguri:plan` で発火、worker の needs_plan ping-pong ガード、
  spec_fixer の budget escalation、cleaner/triage の scan cadence、等)。
- **新規テスト**: local mode worker(f1、受け入れ7)/ spec-PR handoff(open→PR 側 Skip / merged→
  Handoff / closed→Skip、finding 3、受け入れ8)/ closed-issue の Finalize 回収(f4、受け入れ9)/
  非 Finalize: local 資源 + closed×open-PR(finding 4、受け入れ9b)/ `why` の read-only、`run` の
  観測 arm dispatch、`attach` の 4 identity 解決(受け入れ10)/ **PR identity のルーティング**
  (`run --pr`→PR arm、`why --pr`→PR Snapshot、`--run` が loop_kind 保持、finding 1、受け入れ15)/
  **手動 override**(shipped/cadence 満杯でも dispatch、hold は起動せず、finding 2、受け入れ16)/
  `plan`+`ready` 併記で1本しか立たない(受け入れ6)/ discovery ゲート保持(受け入れ13)/
  body-edit signal の非二重実行(受け入れ14)。
- **統合**(`tests/*.rs`): `Loop` trait 撤去後の通し(受け入れ11)。
- **回帰ガード**: 受け入れ1–4 を満たす grep/compile ベースのアサーション(sweep 呼び出し・
  `Loop`・`default_loops` が消えたことの機械的確認)。
