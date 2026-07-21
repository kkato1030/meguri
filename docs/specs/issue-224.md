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
    spec PR が merged な `speccing` issue → `Op(Handoff)`(separate delivery、plan_handoff の
    吸収)、body 変化した `implementing` issue → `reconcile_body_edits` の signal。
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
| closed / merged issue で local resource が残る | `Op(Finalize)`(決定4、terminal 所有) |

issue 側は open PR の有無で PR 側と排他になる。手動ラベル drift(人間が1つの issue に `plan` と
`spec-ready` を同時に貼る等)への耐性は、**issue 側 `next_step_issue` 自身の全域性**で担保する:
phase ラベルの優先順を閉じて定める(`hold`/`needs-human` > `plan` > `ready` > `speccing` >
`implementing`)ので、複数ラベルが付いても **ちょうど1つの arm** に落ちる。drift は「どの1本か」を
変えるだけで、二重 enqueue は生じない。`issue_busy`(既存 `issue_has_active_author_run`)は
author lane を跨いで直列化する二重の安全網。

却下した代替: 単一 Snapshot への統一。PR 前段は PR フィールドが全て空になり部分的で、property
test の意味も薄れる。ADR 0012 自身が Issue Kind を「issue/PR identity で鍵る」と両建てで書くことと
整合する。

### `speccing` handoff の観測(f3)

`observe_open_issues` は PR 状態を持たず、`observe_open_prs` は open PR しか返さない(merged PR は
そこから消える)ので、`speccing` issue の「spec PR が merged」を判定する契約を明示する。

- **分岐**: (A) `speccing` issue ごとに linked spec-PR branch の PR 状態を追加取得する / (B) merged
  PR を含む別 bulk observation を持つ。
- **決定**: **(A)**。現 `plan_handoff::process_issue` がまさに「planner が記録した spec-PR branch を
  引き当て、state == merged を確認する」targeted read をしており、`speccing` issue は少数なので
  per-issue の追加取得で十分。`IssueSnapshot` に `spec_pr_state: Option<PrState>`(open / merged /
  closed-unmerged)を持たせ、`next_step_issue` が **merged → `Op(Handoff)`(→ ready)** /
  **open → `Wait("spec PR in review")`** / **closed-unmerged → `Skip`(人手領域、handoff しない)**
  を返す。open/merged/closed-unmerged の3状態 handoff テストを追加する。

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
  spec 軸への書き込み(4 handshake の1メンバ、ADR 0017)である点を doc に明記する。
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
  - **頻度**: 毎 resync。現 reaper は毎 tick 走る。Finalize は Issue Kind reconciler の
    **毎-resync パス**(`reclaim_stale_claims` と同列)として呼び、この頻度を保つ。
  - **decide の純関数入力**: candidate が持つ資源集合と terminal フラグ。判定は自明(terminal かつ
    資源が残る → 回収)なので `next_step` は `Step::Op(Finalize)` を返す。
  - **所有境界**: terminal に達した issue identity は Issue Kind が所有し、その唯一の arm が
    Finalize(ADR 0012「issue が terminal に達したときの `Op(Finalize)`」)。open PR / open issue
    の decider とは「issue が terminal か」で排他。
  - **回帰テスト**: closed issue に live な pane/worktree が残る状態 → Finalize → 回収される、を
    追加。
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
- **決定**: **(B)**。単一契約は以下。
  - **observe**: `RepoSnapshot.clone_health: CloneHealth`(`Absent` / `Present` / `Unreadable`)を
    managed-clone パスの観測(既存 `clone_health`)から作る。
  - **decide**: `next_step_repo` は `clone_health == Absent`(かつ managed clone)なら
    `Op(EnsureClone)` を返す。`Present` なら repo は当該 tick で ready で、cleaner / triage / drift
    の判定へ進む。`Unreadable` は not-ready(下記のとおり除外)。
  - **act**: `Op(EnsureClone)` の act は bare clone を実体化(現 `ensure_project_clone` 本体)し、
    成否を返す。
  - **readiness 契約(単一)**: scheduler は tick 先頭で `repo_reconciler::reconcile_ready(deps)` を
    呼ぶ。これは `next_step_repo` の **EnsureClone 部分のみ**を評価・act し、`ready:
    HashSet<project_id>` を返す(EnsureClone 成功後 `Present`、または失敗/`Unreadable` で除外)。
    cleaner / triage / drift(slot や書き込みを伴う)は project ごとの通常ブロックで、しかも
    **ready な project にのみ**回す — 他 Kind と同じゲート。
  - **失敗時の遮断**: EnsureClone に失敗した project はその tick の `ready` から外れ、redispatch /
    新規 enqueue / 全 Kind の処理が当該 tick で止まる(現 `ensure_projects_ready` の除外と同一)。
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

- `run` / `attach` は既存(`src/cli.rs`)。役割別動詞は元々存在しないので「足さないと決める」だけ。
- **`why <id>` を新設**(読み取り専用)。f6 で入力文法・出力・観測方式・read-only 検証を固定する:
  - **入力文法**: `meguri why <id> [--project <P>]`。`<id>` は `attach` と同じ解決順(issue 番号 →
    run id)に、PR を明示する `--pr <N>` と、local mode の `TaskKey::Local` id を加える。`--project`
    は複数 project 構成の曖昧さ解消(`run` / `attach` と同じ)。対応 identity 集合 = {issue 番号,
    PR 番号, run id, local task id}。
  - **例**: `meguri why 224`(issue) / `meguri why --pr 231` / `meguri why run_abc123` /
    `meguri why 42 --project foo`。
  - **観測方式**: **fresh observation**(informer cache ではなく1回きりの observe を今回す)。
    `why` は「今どうなっているか」を答えるので、古い cache を映さない。
  - **出力**: 解決した identity、所有する Kind / decider、`next_step` が返した Step、その理由文字列を
    人間可読で出す(issue/PR は Snapshot の主要フィールドも)。
  - **read-only 検証**: CLI テストで `why` が forge への書き込みも run 生成もしないことを assert
    (FakeForge が mutating 呼び出しを記録せず、`runs` に新行が増えない)。
  - 3 動詞のうち最も切り離しやすい部品ではあるが、reconciler との噛み合いが良く安価なため本スライス
    で入れる。

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
- `src/cli.rs` / `src/main.rs` / `src/app.rs` — `Why` サブコマンドを追加。
- `docs/adr/0016-operator-surface-run-why-attach.md`(本 PR 同梱)。

## 受け入れ基準

1. `Loop` trait / `default_loops` / `Scheduler.loops` / `Scheduler::discover` が消えている
   (grep で0件)。
2. scheduler tick から独立 sweep 呼び出し(`reaper::sweep` / `plan_handoff::sweep` /
   `decompose_materializer::sweep` / `routing_drift::sweep` / `reconcile::sweep`)が消え、
   全て reconciler(issue / repo)の act / arm 経由になっている。
3. `ensure_project_clone` の scheduler 固有経路が消え、`Op(EnsureClone)` として表現されている。
   tick の ready ゲート順序は不変(clone 未実体化の project は当該 tick で除外)。
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
7. **f1**: local mode(forge 無し)で `TaskKey::Local` の `ready` 相当タスクが `next_step_local` →
   `Agent(Worker)` で enqueue・drive され、`Loop` trait 撤去後も回帰しない回帰テストがある。
8. **f3**: `speccing` issue の spec PR が open / merged / closed-unmerged の3状態で、それぞれ
   `Wait` / `Op(Handoff)`(→ ready)/ `Skip` になる handoff テストがある。
9. **f4**: closed issue に live な pane/worktree が残る状態から `Op(Finalize)` が発火し、pane /
   worktree / merged branch が回収される回帰テストがある。
10. operator 面が `run` / `why` / `attach` の 3 動詞に確定。`meguri why <id>` が identity の
   Step + 理由を read-only で表示し(**f6**: forge 書き込みも run 生成もしないことを CLI テストで
   assert)、対応 identity 集合 {issue, PR, run, local task} を解決する。役割別動詞は追加されて
   いない。ADR 0016 が land。
11. 統合テスト(`tests/*.rs`、実 tmux + FakeForge/FakeMux)で plan→spec→ready→impl→merge の
   通し、虚偽申告訂正、crash recovery が `Loop` trait 撤去後も回帰しない。
12. `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo nextest run` /
   `cargo test --doc` が通る。

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
  (既存 PR 側 property test と同型)。**結合 property test**(f2): 2 decider が同一 issue に
  二重の enqueue を出さないこと(受け入れ6、drift 込み)。
- **挙動保存**: 各 loop の既存ユニット/挙動テストを、enqueue 経路を reconciler に差し替えても
  緑のまま通す(planner は `meguri:plan` で発火、worker の needs_plan ping-pong ガード、
  spec_fixer の budget escalation、cleaner/triage の scan cadence、等)。
- **新規テスト**: local mode worker(f1、受け入れ7)/ spec-PR handoff の3状態(f3、受け入れ8)/
  closed-issue の Finalize 回収(f4、受け入れ9)/ `why` の read-only(f6、受け入れ10)。
- **統合**(`tests/*.rs`): `Loop` trait 撤去後の通し(受け入れ11)。
- **回帰ガード**: 受け入れ1–4 を満たす grep/compile ベースのアサーション(sweep 呼び出し・
  `Loop`・`default_loops` が消えたことの機械的確認)。
