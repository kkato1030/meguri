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

## 決定1: Issue Kind は「2 観測 × 2 純関数 next_step」を1モジュールに畳む

Issue Kind は issue identity(ラベル駆動)と PR identity(PR 状態駆動)の両方を所有する。今の
`issue_reconciler.rs`(S1–S3)は **PR 側のみ**を観測している(`observe_open_prs`)。planner /
worker は PR がまだ無い段階の issue ラベルで駆動されるので、PR 観測だけでは半分しか見えない。

- **分岐**: 単一の Snapshot に issue も PR も詰めるか、観測を2本に分けるか。
- **決定**: **観測を2本、純関数 `next_step` を2本**にし、enqueue / claim / backoff 機構は共有する
  (1 Kind = 1 モジュール `issue_reconciler`)。
  - `observe_open_issues` → `next_step_issue(IssueSnapshot) -> Step`。PR 前の issue ライフ
    サイクルを所有: `meguri:plan` → `Agent(Planner)`、`meguri:ready` → `Agent(Worker)`、
    separate delivery で spec PR が merged な `speccing` issue → `Op` で `ready` へ(plan_handoff
    の吸収)、body 変化した `implementing` issue → `reconcile_body_edits` の signal。
  - `observe_open_prs` → `next_step`(既存を拡張)。PR ライフサイクルを所有: fixer 家族(S3)+
    spec 段階 arm(spec_worker / pr_reviewer / spec_fixer)+ 承認済み分解提案の materialize。
- **二重所有の回避**: 2つの decider は **phase ラベルで排他**(`plan`/`ready` は PR 前 = issue 側、
  `spec-reviewing`/`spec-ready` = PR 側)、かつ `issue_busy`(既存の
  `issue_has_active_author_run`)で **直列化**される。author lane の run が生きている issue は
  両側とも Skip なので、同時に二つの arm が同じ identity を触ることはない。property test で
  各 decider の全域性(ちょうど1所有 arm)を守る。

却下した代替: 単一 Snapshot への統一。PR 前段は PR フィールドが全て空になり、部分的で扱いづらく、
property test の意味も薄れる。ADR 0012 自身が Issue Kind を「issue/PR identity で鍵る」と両建てで
書いていることとも整合する。

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

- **reaper → `Op(Finalize)`**: reaper のペイン/worktree/merged branch 回収ロジック本体は関数を
  保ったまま `Op(Finalize)` の act から呼ぶ。Finalize の観測は **ローカルの pane/worktree 登録**を
  issue の terminal 性と突き合わせる現 reaper の discover をそのまま使う(closed issue は
  `observe_open_issues` に出ないため、Finalize は別パスで terminal を検出する)。芯は「reaper が
  scheduler tick の独立 sweep 呼び出しから消え、Issue Kind の act 経路で回収される」こと。
- **`reconcile.rs` → `reconcile_body_edits.rs`**: 機械的改名(module 名 + `sweep`)。新しい
  `reconcile(id)` 契約(ADR 0012 決定2)との名前衝突を解消する。挙動は不変(`implementing` issue
  の body 変化 → signal comment、agent は起こさない)。scheduler の独立 sweep 呼び出しをやめ、
  Issue Kind reconciler の毎-resync act(`reclaim_stale_claims` と同列)として呼ぶ。
- **decompose_materializer → PR 側 act**: PR 側観測が「spec-ready かつ decompose 提案」の PR を
  見たら materialize の act(現 sweep 本体)を回す。`spec_worker_owns`(combined && spec-ready)
  との区別は `is_decompose_proposal` で行い、提案は spec_worker より先に materialize 枝へ振る。

## 決定5: plan_handoff を issue 側の Op に畳む

separate delivery で spec PR が merged になった `speccing` issue を `ready` に進める現 sweep を、
`next_step_issue` の枝(`Op`、ラベル遷移のみ)にする。挙動不変。

## 決定6: `ensure_project_clone` は `Op(EnsureClone)` にしつつ tick の ready ゲートを保つ

最も慎重を要す一点。現状 `ensure_projects_ready()` は tick の**最初**に走り、`repo_path` に触れる
全下流(discover / redispatch / 各 sweep)をゲートする `ready` 集合を作る。

- **分岐**: Repo Kind reconciler の中に EnsureClone を埋めると tick 順序が崩れうる。
- **決定**: EnsureClone は **Repo Kind の Op でありつつ、tick 先頭で走って ready ゲートも兼ねる**。
  具体的には scheduler が `repo_reconciler::ensure_ready(deps)`(EnsureClone の act 経路)を
  他の何よりも先に呼び、現 `ensure_projects_ready` と同じ `ready: HashSet` を返す。cleaner /
  triage の arm(slot を要す)は通常の enqueue フローに、EnsureClone(純ゲート)は tick 先頭に、
  と分けることで **順序不変**を担保する。芯は「scheduler 固有の bootstrap reconcile 経路が消え、
  clone 実体化が Repo Kind の Op として表現される」こと。

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
- **`why <id>` を新設**(読み取り専用): その identity の観測を1回回し、`next_step` が返した Step と
  理由文字列を表示する。reconciler の副産物(理由文字列は既に全枝にある)を人間向けに出すだけで、
  書き込みは一切しない。3 動詞のうち最も切り離しやすい部品なので、review で重すぎると判断されたら
  ここだけ follow-up に退避できる余地を残す(が、reconciler との噛み合いが良く安価なため本スライス
  で入れる)。

## 決定10: step policy を新 arm に広げる

S3 の `StepPolicyConfig`(conflict_resolver / ci_fixer / fixer の bool + `apply_policy`)を、新 arm
(planner / worker / spec_worker / pr_reviewer / spec_fixer / cleaner / triage)にも広げ、ADR 0026 の
「無効 arm の `Agent` を `Wait(PolicyDisabled)` に落とす」統一 kill switch を維持する。ただし
triage の `mode`(off/report/advise/auto)や pr_reviewer impl 側の opt-in など **既存の config 意味は
snapshot の trigger 条件として温存**する(bool へ潰さない — config 键粒度の再設計は S5 / ADR 0013)。

## 変更箇所

- `src/engine/issue_reconciler.rs` — `observe_open_issues` 系の追加、`IssueSnapshot` +
  `next_step_issue`、`Arm` に SpecWorker/PrReviewer/SpecFixer 追加、PR 側 Snapshot 拡張、
  `Op` に Finalize/EnsureClone は決定4/6 に沿って(EnsureClone は repo 側)、reaper 回収 act、
  decompose materialize act、`reconcile_body_edits` の毎-resync 呼び出し、plan_handoff の issue Op。
  肥大化するなら `issue_reconciler/{mod,pr,issue}.rs` へ分割してよい(1 Kind = 1 モジュール群)。
- `src/engine/repo_reconciler.rs`(新規) — `RepoSnapshot` / `next_step_repo` / arm(cleaner,
  triage)/ Op(routing_drift, ensure_clone)、`ensure_ready`。
- `src/engine/reconcile.rs` → `reconcile_body_edits.rs`(改名)。
- `src/engine/mod.rs` — `Loop` trait / `default_loops` / `Target` / `OpenPrCache` 撤去、
  `run_recipe` 追加、`ensure_project_clone` は repo_reconciler へ移設。
- `src/engine/scheduler.rs` — `loops` フィールド撤去、tick の sweep ブロックを
  reconciler 呼び出し(issue / repo / schedule)に整理、`discover` 撤去、`dispatch` を
  `run_recipe` に、`ensure_projects_ready` を `repo_reconciler::ensure_ready` に。
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
6. issue 側 `next_step_issue` と repo 側 `next_step_repo` に「ちょうど1つの所有 arm(所有の欠落も
   二重所有も無い)」の網羅 property test がある。
7. operator 面が `run` / `why` / `attach` の 3 動詞に確定。`meguri why <id>` が identity の
   Step + 理由を読み取り専用で表示する。役割別動詞は追加されていない。ADR 0016 が land。
8. 統合テスト(`tests/*.rs`、実 tmux + FakeForge/FakeMux)で plan→spec→ready→impl→merge の
   通し、虚偽申告訂正、crash recovery が `Loop` trait 撤去後も回帰しない。
9. `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo nextest run` /
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

- **純関数 property test**: `next_step_issue` / `next_step_repo` の全域性(既存 PR 側 property
  test と同型 — 観測状態空間を列挙し、ちょうど1 Step、症状ごとの所有 arm を assert)。
- **挙動保存**: 各 loop の既存ユニット/挙動テストを、enqueue 経路を reconciler に差し替えても
  緑のまま通す(planner は `meguri:plan` で発火、worker の needs_plan ping-pong ガード、
  spec_fixer の budget escalation、cleaner/triage の scan cadence、等)。
- **統合**(`tests/*.rs`): `Loop` trait 撤去後の通し(受け入れ8)。
- **回帰ガード**: 受け入れ1–4 を満たす grep/compile ベースのアサーション(sweep 呼び出し・
  `Loop`・`default_loops` が消えたことの機械的確認)。
