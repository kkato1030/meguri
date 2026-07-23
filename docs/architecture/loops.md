# loop モデル横断 overview — 設計者向けの「loop の地図」

## この doc の位置づけ

meguri の loop についての説明は今まで2系統に散らばっていた。

- **README.md / README.ja.md** — 全 loop を横断で説明する唯一の場所だが、**利用者向け**(使い方・設定)。
- **docs/adr/** — loop に触れる決定は複数あるが、それぞれ**1決定を縦に掘るだけ**で、全 loop がどう繋がるかの横断の地図は無い。

「なぜこの loop がこの位置にあるか」という縦の理由は ADR に、「全 loop がどう繋がるか」という横の地図は README にあるが、**設計者がまず読む「loop の地図」**はどちらでもない。この doc がそれを埋める。

- **README** = 利用者向けの使い方(引き続き正)。
- **ADR** = 個別決定とその理由(引き続き正)。
- **この doc** = 設計者向けの構造の地図。事実の二重管理を避けるため、詳細は README / ADR を参照し、この doc 自体はパイプライン図・優先度・ライフサイクル表・ADR 索引に徹する。

## 1. パイプライン全体図

入口は2つ — `meguri:plan`(spec 先行、opt-in)と `meguri:ready`(直行)。ADR 0008 でこの2つの
実装 diff の担保は対称になった: どちらの経路も、実装 diff を積む loop(直行の worker、または
spec 先行で実装を担う worker/spec_worker)は forge に触れない内部 self-review(独立 lane、
ADR 0006)を通ってから、その diff を PR として世に出す — ただし出し方は loop によって違う。
worker(直行/separate)は self-review の後に**新規 PR を open** するが、combined の
spec_worker は新規 PR を作らず、self-review の後に**既存の spec PR を通常の実装 PR として
進める**(`meguri:spec-ready` ラベルを外すだけ)。加えて opt-in の `pr_reviewer`(kind = Impl)が
外部 GitHub レビューを任意で載せられる([ADR 0008](../adr/0008-symmetric-plan-impl-review-loop.md))。
spec 先行経路はさらに
`plan_delivery` 設定(`separate` | `combined`)で二手に分かれる: **separate**(既定)は spec PR と
実装 PR を別々に完結させ、`plan_handoff.sweep` が両者を橋渡しする。**combined** は
`spec_worker` が spec PR の branch を引き継ぎ、spec + 実装を1本の PR にまとめる。plan
レビュー(`pr_reviewer`, kind=Plan)の findings は「次の push 待ち」で放置されない —
`spec_fixer`(issue #188 / [ADR 0013](../adr/0013-spec-fixer-drives-plan-review-findings.md))が
planner と同じ author lane で findings を spec/ADR に反映して push し、`pr_reviewer` が新しい
head を再レビューする(head sha で収束判定、≤3 round、超過は `needs-human`)。両経路とも
最終的に同じ fixer/ci_fixer/conflict_resolver → auto-merge → merge-watch の後工程に合流する。
cleaner だけはこのパイプラインの外で独立に回る。

入口から「実装 diff を含む PR が1本 open」になるまで:

```
GitHub issue(未トリアージ、無ラベル)
   │
   │  人間がラベルを選ぶ ── 2つの入口
   ├────────────────────────────────┐
   ▼                                 ▼
meguri:plan(spec 先行、opt-in)   meguri:ready(直行)
   │                                 │
   ▼                                 │
planner (author lane)                │
  調査 → self-review                  │
  (self-review lane、ADR 0006/0008)   │
  → spec PR を open                  │
  issue: plan→speccing               │
  PR: meguri:spec-reviewing          │
   │                                 │
   ▼                                 │
pr_reviewer, kind=Plan               │
(pr-review lane、既定 on)             │
  clean → PRラベル:                   │
    spec-reviewing→spec-ready        │
  findings → PR本文 <details> に追記   │
  (同じ head は1回だけレビュー)         │
   │                                 │
   ▼ findings                        │
spec_fixer(author lane、             │
planner の pane/session を継続、       │
issue #188 / ADR 0013)               │
  findings を読み spec/ADR を修正      │
  → push(≤3 round、超過は             │
    needs-human)                    │
   │                                 │
   └─(新 head を pr_reviewer が       │
      再レビュー、head sha で dedup、  │
      clean になるまで ⇄ 繰り返し)     │
   │                                 │
   ▼ clean → plan_delivery で分岐     │
   ├──────────────┐                  │
   ▼ separate(既定) ▼ combined        │
plan_handoff.sweep  spec_worker      │
(帯域外、ADR 0008)   (author lane、    │
  spec PR merge 済み  同一 branch/PR   │
  の speccing issue   を継続)          │
  を検知               → 実装 commit  │
  → issue:            を同一 PR に積む │
    speccing→ready    validate →     │
  (以降は worker が    self-review    │
   新PRで直行と合流)   (self-review    │
   │                  lane、spec+実装  │
   │                  統合 diff、      │
   │                  ADR 0011)⇄fix   │
   │                  issue: speccing │
   │                  →implementing   │
   │                  (このPRが issue  │
   │                   を Closes —    │
   │                   spec先行で唯一  │
   │                   PR1本のまま     │
   │                   完了するケース) │
   ▼                     │           │
worker (author lane)      │           │
  新 branch                │           │
  self-review               │           │
  (self-review lane、        │           │
   ADR 0006)                 │           │
  → PR open(`Closes #N`)      │           │
  issue: ready→implementing    │           │
   │                            │         │
   └──────────────┬─────────────┘         │
                  │                       │
                  └───────────┬───────────┘
                              ▼
           実装 diff を含む PR が1本 open(Closes #N)
```

```
           実装 diff を含む PR(Closes #N)
                  │
                  ▼
   pr_reviewer, kind=Impl(pr-review lane、opt-in、既定 off)
     対象: meguri/ branch PR、needs-human でない、spec-ready
     でない(spec_worker の縄張りは除外)、CI green
     head 未レビューなら任意でレビュー(ラベル遷移なし)
                  │
   ┌──────────────┼──────────────────┐
   ▼              ▼                   ▼
 fixer         ci_fixer          conflict_resolver
 (author,      (author,            (author,
  継続)          継続)                継続)
 人間/外部botの   CI red →            CONFLICTING →
 未解決スレッド    fix push           base merge・解消 push
 に返信          (≤3 round)          (≤3 round)
   └──────────────┴──────────────────┘
                  │
                  │  ここから先は run/pane を持たない「帯域外 sweep」(§2)
                  ▼
     merge_tail.sweep (ADR 0012 slice 1, #221) — observe → next_step → act
     一括 observe から純関数 next_step が PR ごとに 1 つの Op を選ぶ:
       native: 適格 PR に GitHub-native auto-merge を arm (ADR 0003)
       orchestrator: 同じ適格条件で meguri 自身が直接 merge (ADR 0009)
       BEHIND(arm 済み × base 進行): Op(UpdateBranch) で base を取り込み
         head を進める → 次 observe で未 arm と判定され自然に再 arm
       conflict/red CI は fixer 系に委譲(no-op)。
       どのループも拾わない stall だけ meguri:needs-human で escalate
                  │
                  ▼
        native: GitHub が branch protection + required checks で
        マージを確定 / orchestrator: meguri が MERGEABLE 報告時に
        merge_pr → いずれも issue close(Closes #N)


cleaner (standalone) ── パイプラインの外を独立に回る
  default branch head を定期巡回し、乖離を単一の
  meguri:clean-report issue に報告するだけ(read-only)
```

補足:

- **local モード**(GitHub なし)は spec 先行経路を持たない。`planner`(および `pr_reviewer` /
  `spec_worker`)がまだ無く(issue #54 Phase 3)、`PlannerLoop::discover` は
  `deps.forge.is_none()` なら空を返すため、ローカルの `plan` task は queued のまま dormant に
  なる。worker が `needs_plan` を返しても、local task は forge 越しの planner 委譲ができないため
  人間へエスカレーション(`NeedsHuman`)される。つまり local モードで実際に回るのはこの図の
  `meguri:ready` 直行経路(worker → self-review → 完了)だけ。discovery の入口自体も GitHub
  ラベルではなく meguri のローカル task queue になり(`TaskSource` 抽象、
  [ADR 0003(tasksource-task-moves-run-pins)](../adr/0003-tasksource-task-moves-run-pins.md))、
  成果物も PR ではなく検証済みローカルブランチになる。詳細は README の「Local mode」を参照。
- fixer / ci_fixer / conflict_resolver は互いに排他ではなく、同じ PR に対して並行して起こりうる
  (スレッド対応・CI 修正・conflict 解消は独立事象)。図は簡略化のため並列に描いているが、実際の
  駆動順は §2 のディスパッチ優先度に従う。
- `plan_delivery = combined` の PR は spec-worker が引き取るまで `meguri:spec-ready` ラベルを
  帯びたままなので、上の pr_reviewer(kind=Impl)の対象条件から自然に除外される — 二重レビューは
  起きない。

## 2. ディスパッチ優先度と enqueue の所有

ADR 0012 スライス4([0012-loops-are-emergent-level-triggered-reconciler](../adr/0012-loops-are-emergent-level-triggered-reconciler.md))で旧 `Loop` trait と `default_loops()` は撤去された。loop はコード上の構造物ではなく、reconciler が生む実行時の軌道である。

- **enqueue は reconciler が所有する**。毎 poll、observe → 純関数 decide → act(run の enqueue、または agent を起こさない軽量 `Op`):
  - **Issue Kind**(`issue_reconciler.rs`): PR 側 `next_step`(fixer 家族 + spec 段階 arm + merge tail + `Op(Finalize)` などの毎 resync act)、issue 側 `next_step_issue`(planner / worker / `Op(Handoff)`)、local 側 `next_step_local`(worker)。
  - **Repo Kind**(`repo_reconciler.rs`): `Op(EnsureClone)`(tick 先頭の readiness 契約)> cleaner > triage、毎 resync の routing-drift 再計算。
  - **Schedule Kind**(`schedule.rs`): cron スケジュールの評価と起票。
- **dispatch は recipe テーブル**(決定8): `runs.loop_kind` → `run_recipe` の純 match。workqueue の順序は `dispatch_rank`(merge に近い側から先取り):

```
conflict_resolver → ci_fixer → fixer → spec_fixer → spec_worker → pr_reviewer → worker → planner → cleaner → triage
```

同一 kind 内は issue/PR 番号の昇順(FIFO)。背後の原則は不変で一つだけ: **新規着手より仕掛かりの完了を優先する(WIP を減らす)**([ADR 0001-scheduler-priority-wip-first](../adr/0001-scheduler-priority-wip-first.md))。

旧「帯域外 sweep」はすべて reconciler の act / arm に畳まれた: reaper は Issue Kind の `Op(Finalize)`(open な meguri PR を持つ identity の資源は PR 側が保持)、plan_handoff は issue 側の `Op(Handoff)`、decompose materialize と body-edit signal は毎 resync の act、routing_drift は Repo Kind の毎 resync 再計算。scheduler tick に残る独立呼び出しは Schedule Kind(`schedule::sweep`)のみ。operator は identity への 3 動詞 `run` / `why` / `attach` で介入する([ADR 0016](../adr/0016-operator-surface-run-why-attach.md))。

## 3. loop 別ライフサイクル

README の「ループ別の寿命の一覧」を、設計視点([ADR 0004-issue-lane-pane-session-lifetime](../adr/0004-issue-lane-pane-session-lifetime.md)の lane モデル)で再構成したもの。事実の一次情報は README とコード(`src/engine/*.rs` の各 loop 冒頭コメント)にあり、ここは表として横断しやすくしたものに過ぎない。

| loop | lane | trigger | 鍵 | worktree | 正常終了 | pane 後始末 |
|---|---|---|---|---|---|---|
| planner | author(+ self-review) | `meguri:plan` issue | issue | 新 branch | self-review(self-review lane、ADR 0008/0011)→ spec PR 作成、issue: `plan`→`speccing` | 維持(author pane) |
| pr_reviewer | pr-review(独立) | Plan: `spec-reviewing` PR(既定 on)/ Impl: 実装 PR(opt-in)、head 未レビュー | issue + `pr-review` lane | read-only detached、`pr-reviewer-<issue>` 固定 | `meguri/pr-review` commit status + PR 本文 `<details>` 要約(Plan の clean は PR: `spec-ready`)、次の push 待ち | 維持(独立 pane) |
| spec_fixer | author(継続) | `spec-reviewing` PR の head の `meguri/pr-review` が failure | issue(branch から復元) | PR head に attach | spec 修正 push(≤3 round)、超過は `needs-human`。pr_reviewer が新 head を再レビュー | 維持、author pane を継続 |
| spec_worker | author(継続、+ self-review) | `spec-ready` PR(`plan_delivery = combined` 限定 — separate では discover が空) | issue(branch から復元) | 既存 branch を継ぐ | 実装 commit を同一 PR に統合 → validate/self-review(self-review lane、spec+実装の統合 diff、ADR 0011)、issue: `speccing`→`implementing` | 維持、author pane を継続 |
| worker | author(+ self-review) | `meguri:ready` issue | issue | 新 branch | self-review(self-review lane、ADR 0006)→ PR `Closes #N`、issue: `ready`→`implementing` | 維持(author pane) |
| fixer | author(継続) | PR の未解決スレッド(人間/外部bot) | issue(branch から復元) | PR head に attach | スレッドに返信、再レビュー待ち | 維持、author pane を継続 |
| ci_fixer | author(継続) | meguri PR の CI red | issue(branch から復元) | PR head に attach | fix push(≤3 round)、超過は `needs-human` | 維持、author pane を継続 |
| conflict_resolver | author(継続) | meguri PR が `CONFLICTING`(≤3) | issue(branch から復元) | PR head に attach | base merge・解消 push、解消不能は `needs-human` | 維持、author pane を継続 |
| cleaner | standalone(lane モデル外) | レポート issue + default branch 前進(`clean.interval_hours`) | レポート issue | read-only detached | 単一レポート issue を再生成 | 自前回収 |

補足:

- **author lane** は同じ branch を編集する loop 全員(planner → spec_fixer → worker/spec_worker → fixer/ci_fixer/conflict_resolver)が同一 pane・同一 claude session を共有し、文脈を継ぐ。spec_fixer は run を PR の canonical issue で鍵るため、spec を書いた planner と同じ author pane・同一 session で修正が走り、planning の文脈を保つ(issue #92 の lane モデルどおり)。**self-review lane** は self-review が必須の3 loop(planner / worker / spec_worker、表の「+ self-review」)だけが使う、同じ issue に紐づく別の実行体(プロファイル `self-reviewer`)——author が積んだ diff を独立した目でレビューし、fix 指示を author lane へ戻す内部往復専用([ADR 0006](../adr/0006-ai-implementation-review-is-an-internal-loop.md) / [ADR 0008](../adr/0008-symmetric-plan-impl-review-loop.md) / [ADR 0011](../adr/0011-combined-impl-diff-self-review.md))。lane = pane とは限らない — launch mode は role 単位で pane/direct を選べ、self-reviewer の既定は `direct`(pane を張らない、[ADR 0012](../adr/0012-launch-mode-role-pane-or-direct-keep-pane-subordinate.md))。**pr-review lane** は pr_reviewer 専用の独立 pane(別 session)。**standalone** は cleaner のみで lane モデルの対象外。
- pane・worktree はいずれも issue が寿命の単位で、issue が terminal に達すると Issue Kind の `Op(Finalize)` が回収する(watch 実行中は resync のたびに、一発実行では `meguri prune`)。open な meguri PR が残る identity の資源は PR 側が保持する(finding 4b)。
- merge tail / handoff は Issue Kind reconciler の `Op` のため、pane も worktree も持たない(§2 参照)。

## 4. 横断原則

個々の loop の実装を読む前に踏まえておくべき、全 loop 共通の原則。

- **Authority(forge が唯一の永続状態)** — looper 由来の原則。GitHub のラベル・コメントが durable な workflow state であり、ローカル sqlite(`~/.meguri/meguri.sqlite`)は run 実行の進行管理にしか使わない。meguri をいつ kill しても forge から復旧できるのはこの原則のおかげ。ADR 0006 の内部ループ化(「内部ループでも forge には一切触れない」)や ADR 0007 の merge-watch(「専用マーカーを持たず forge から毎回導出する」)は、いずれもこの原則の直接の帰結。詳細は README の「[The completion contract](../../README.md#the-completion-contract)」および ADR 0006 / 0007 を参照。
- **ラベル二軸(phase × ball)** — [ADR 0005-issue-labels-two-axis-phase-and-ball](../adr/0005-issue-labels-two-axis-phase-and-ball.md)。フェーズ軸(`plan`/`speccing`/`ready`/`implementing`)が issue の位置を、ボール軸(`working`/`needs-human`/`hold`)が誰の番かをフェーズに重ねて示す。無ラベル issue = 未トリアージという一義性はここから生まれ、discovery が読む唯一の入力でもある。
- **discovery の時刻ゲート(not-before / cadence)** — [ADR 0011-discovery-throttles-not-before-and-cadence](../adr/0011-discovery-throttles-not-before-and-cadence.md)。`LabelTaskSource` / `LocalTaskSource` の discover は、claim より前・dependencies と同じ層で2つの調速ゲートを通す。**not-before**(issue 本文の `<!-- meguri:not-before <TS> -->` マーカー、または local task の `not_before` フィールド)が未通過の issue/task と、**cadence**(config `[[projects.cadence]]` のラベル→窓あたり上限)の窓が埋まっているラベルの issue は、ラベルもコメントも足さずサイレントにスキップする(GitHub-native dependencies のブロックと同じ流儀)。ゲート順は not-before → dependencies → cadence で、共有残枠を消費するのは dependencies を通過した actionable な候補だけ。消化実績は forge ではなく sqlite の run 履歴(`runs.cadence_label`)で数える(Authority 原則の帰結。`schedule_state` と同じ「実行の記録はローカル」)。サイレントスキップは forge に痕跡を残さないため、`meguri tasks` が理由付きで可視化する。
- **role routing** — [ADR 0003-role-based-agent-routing](../adr/0003-role-based-agent-routing.md)。エージェント振り分けは issue の難易度推定ではなく `loop_kind`(役割)を軸にする。明示設定(`[routing.roles]`)は常に auto に勝ち、プロファイル未定義や CLI 未検出は起動時に大きな音を立てて落ちる(静かなフォールバックはしない)。
- **AI 実装レビュー = 内部ループ** — [ADR 0006-ai-implementation-review-is-an-internal-loop](../adr/0006-ai-implementation-review-is-an-internal-loop.md)。実装 diff の self-review は worker の run の worktree 内(`validate` と `open-pr` の間)で完結し、forge には一切触れない。GitHub 上のレビュー transport は人間・外部 bot 専用に残る(fixer が拾うのは人間/外部 bot のスレッドだけ)。
- **auto-merge = 適格判定は共通、マージ権威は mode で二分** — 既定の `native`([ADR 0003-auto-merge-github-native-arm-only](../adr/0003-auto-merge-github-native-arm-only.md))では meguri は「マージして安全か」を自前で判定せず、条件の揃った PR に GitHub-native auto-merge(`gh pr merge --auto`)を arm するだけで、最終判断は GitHub(branch protection + required checks)に委ねる。native が使えない private+Free リポジトリ向けの `orchestrator`([ADR 0009-auto-merge-orchestrator-side-merge-on-free-private](../adr/0009-auto-merge-orchestrator-side-merge-on-free-private.md))では、同一の適格条件を通った PR を GitHub が `MERGEABLE` と報告した時点で meguri 自身が `merge_pr` する — サーバ側ゲートが無いぶん、pre-PR 検証(`check_command` + self-review)を唯一のゲートとして明示的に引き受ける。いずれも opt-in(`[pr.auto_merge].enabled` + `meguri:automerge` ラベル)で、fail-fast(リポジトリが条件を honor できなければ起動時に拒否。orchestrator は `require_branch_protection = false` が必須)。
- **merge-watch = ドリフト検出であってマージ権威ではない** — [ADR 0007-merge-watch-defers-to-fixer-loops-and-backstops-drift](../adr/0007-merge-watch-defers-to-fixer-loops-and-backstops-drift.md)。conflict / red CI は conflict_resolver / ci_fixer が arm と無関係にすでに拾っているため、merge-watch はそれらに介入せず no-op にする(`needs-human` を貼れば fixer 系ループ自身を締め出しデッドロックする)。merge-watch が固有に escalate するのは「どのループも拾わないまま放置された arm 済み PR」だけ。

## 語彙: role / loop kind / lane(issue #168)

内部命名は3層に分かれ、"role" という語は routing 専用にする(issue #167/#168):

- **role**(設定の粗粒度 = 仕事の種類)— `[routing.roles]` が振り分ける6分類
  ([ADR 0003 改訂](../adr/0003-role-based-agent-routing.md)): planner /
  worker / fixer / self-reviewer / pr-reviewer / cleaner。
- **loop kind**(内部実行単位)— `runs.loop_kind` に入る値。role より細かい
  (例: `worker` と `spec-worker` は同じ role "worker" だが loop kind は別)。
- **lane**(pane・session の独立単位)— `(project, issue, lane)` で鍵る pane
  の区画([ADR 0004](../adr/0004-issue-lane-pane-session-lifetime.md))。
  `LANE_AUTHOR` / `LANE_PR_REVIEW` / `LANE_SELF_REVIEW` の3種。

**命名規約: loop 名の qualifier は常にトリガー(入力)であり、成果物ではない。**

- `fixer` — 未解決レビュースレッドがトリガー。
- `ci_fixer`(loop kind `ci-fixer`)— 赤 CI がトリガー。
- `conflict_resolver` — CONFLICTING 状態がトリガー。
- `spec_worker`(loop kind `spec-worker`)— spec-ready PR がトリガー(spec と
  いう「成果物」ではなく「入力」)。同じ読みで `review-fixer` のような複合名も
  「レビュー(スレッド)に反応する fixer」と読める(現状そのものの loop kind は
  存在しないが、命名する際はこの型に従う)。

この規約により、"spec-worker" は「spec を作る worker」ではなく「spec-ready
PR に反応する worker」と正しく読める。

issue #168 は #167 が routing role 層で確定した語彙(`self-reviewer` /
`pr-reviewer`)に内部命名を追随させた: loop kind `guard` → `pr-reviewer`、
commit status `meguri/guard-review` → `meguri/pr-review`、lane `review` →
`pr-review` / `impl-review` → `self-review`、`ROLE_*` 定数 → `LANE_*`、
`impl_reviewer.rs` → `self_review.rs`、`handoff.rs` → `plan_handoff.rs`。
fixer 家族(`review-fixer` / `conflict-fixer` 等)の内部 rename、および
`worker` / `spec-worker` の rename は対象外(この節の命名規約を満たしている
ため、rename する動機が薄い)。

## 5. ADR 索引(loop に関係するもの)

縦の理由(個別決定とその背景)への入口。1決定1ファイルの原則どおり、詳細は各 ADR を参照。

| ADR | 一行要約 |
|---|---|
| [0001-scheduler-priority-wip-first](../adr/0001-scheduler-priority-wip-first.md) | ディスパッチ優先度はパイプラインの逆順に固定。新規着手より仕掛かりの完了を優先する。 |
| [0001-specs-are-disposable-scaffolding](../adr/0001-specs-are-disposable-scaffolding.md) | spec は使い捨ての足場。実装時に刈り、残す価値のある決定は ADR / ドメイン文書へ振り分ける。 |
| [0003-role-based-agent-routing](../adr/0003-role-based-agent-routing.md) | エージェント振り分けは役割ベース。明示は auto に勝ち、失敗は起動時に大きな音を立てる。 |
| [0003-auto-merge-github-native-arm-only](../adr/0003-auto-merge-github-native-arm-only.md) | 自動マージは GitHub-native auto-merge への arm が基本。安全判定は GitHub に委ねる。 |
| [0003-cleaner-read-only-single-report-issue](../adr/0003-cleaner-read-only-single-report-issue.md) | hygiene ループは read-only detector から始め、書き込み境界を単一レポート issue に限定する。 |
| [0003-tasksource-task-moves-run-pins](../adr/0003-tasksource-task-moves-run-pins.md) | discovery の入口を `TaskSource` 抽象で統一し、GitHub ラベルとローカル task queue を同じ枠で扱う。 |
| [0004-issue-lane-pane-session-lifetime](../adr/0004-issue-lane-pane-session-lifetime.md) | 寿命の単位は issue。pane は `(project, issue, lane)` で鍵り、author/review lane を分ける。 |
| [0005-issue-labels-two-axis-phase-and-ball](../adr/0005-issue-labels-two-axis-phase-and-ball.md) | issue ラベルは「フェーズ × ボールの所在」の2軸。無ラベル = 未トリアージが一義になる。 |
| [0006-ai-implementation-review-is-an-internal-loop](../adr/0006-ai-implementation-review-is-an-internal-loop.md) | AI 実装レビューは内部ループ。GitHub は人間・外部レビューにだけ残す。 |
| [0007-merge-watch-defers-to-fixer-loops-and-backstops-drift](../adr/0007-merge-watch-defers-to-fixer-loops-and-backstops-drift.md) | merge-watch は fixer 系ループに委譲し、どのループも拾わない stall だけを backstop する。 |
| [0008-symmetric-plan-impl-review-loop](../adr/0008-symmetric-plan-impl-review-loop.md) | plan/impl レビューループの対称化: 内部 self-review は必須(多角視点)、外部 GitHub レビュー(pr-reviewer)は任意。 |
| [0009-schedules-enqueue-only-not-a-cron-replacement](../adr/0009-schedules-enqueue-only-not-a-cron-replacement.md) | 時刻駆動スケジュールはキューへの起票だけに限定。任意コマンドの定期実行はスコープ外。 |
| [0009-auto-merge-orchestrator-side-merge-on-free-private](../adr/0009-auto-merge-orchestrator-side-merge-on-free-private.md) | ネイティブ auto-merge が使えない private+Free リポジトリ向けに、meguri 自身がマージする orchestrator モードを追加(ADR 0003 を mode で二分)。 |
| [0011-discovery-throttles-not-before-and-cadence](../adr/0011-discovery-throttles-not-before-and-cadence.md) | discovery に not-before(時刻ゲート)と cadence(ラベル別の窓あたり上限)の2つの調速ゲートを追加。 |
| [0011-combined-impl-diff-self-review](../adr/0011-combined-impl-diff-self-review.md) | `plan_delivery = combined` では spec+実装の統合 diff に対して1回だけ self-review する。 |
| [0013-spec-fixer-drives-plan-review-findings](../adr/0013-spec-fixer-drives-plan-review-findings.md) | plan レビューの findings は spec_fixer が planner の author lane で駆動する。収束は head sha、≤3 round で needs-human。 |
