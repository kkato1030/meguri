# loop モデル横断 overview — 設計者向けの「loop の地図」

## この doc の位置づけ

meguri の loop についての説明は今まで2系統に散らばっていた。

- **README.md / README.ja.md** — 全 loop を横断で説明する唯一の場所だが、**利用者向け**(使い方・設定)。
- **docs/adr/** — loop に触れる決定は複数あるが、それぞれ**1決定を縦に掘るだけ**で、全 loop がどう繋がるかの横断の地図は無い。

「なぜこの loop がこの位置にあるか」という縦の理由は ADR に、「全 loop がどう繋がるか」という横の地図は README にあるが、**設計者がまず読む「loop の地図」**はどちらでもない。この doc がそれを埋める。

- **README** = 利用者向けの使い方(引き続き正)。
- **ADR** = 個別決定とその理由(引き続き正)。
- **この doc** = 設計者向けの構造の地図。事実の二重管理を避けるため、詳細は README / ADR を参照し、この doc 自体はパイプライン図・優先度・ライフサイクル表・ADR 索引に徹する。

> **この doc は現行モデルを正として書いている。** #132(spec/impl ループの対称化、ADR 0008 相当)が in-flight で、着地すると loop モデルが大きく動く。詳細は「[6. 注意: #132 / ADR 0008 は in-flight](#6-注意-132--adr-0008-は-in-flight)」を参照。

## 1. パイプライン全体図

入口は2つ — `meguri:plan`(spec 先行)と `meguri:ready`(直行)。**この2つは実装 diff の担保が非対称**: 直行(`meguri:ready`)経路は worker の内部 self-review(ADR 0006)を経てから PR を開くが、spec 先行経路は spec PR が(planner によって)既に open な状態で `spec_worker` が実装 commit を積むだけで完了し、`SpecWorkerFlavor` は `Flavor::self_reviews()` を override していない(既定 `false`)ため worker と同じ内部 self-review フェーズを通らない。`spec_reviewer` がレビューするのは `meguri:spec-reviewing` の spec PR head(=spec の内容)だけで、discovery は `meguri:spec-reviewing` ラベルの付いた PR に限られるため、`spec_worker` が実装 commit を積んで PR がそのラベルを離れた後は再び走らない — spec 先行経路には実装 diff に対する worker 相当の内部/GitHub レビューが無く、PR 公開後の人間・外部 bot のレビューと fixer 系ループだけがそれを担う。両経路とも最終的に同じ fixer/ci_fixer/conflict_resolver → auto-merge → merge-watch の後工程に合流する。cleaner だけはこのパイプラインの外で独立に回る。

```
GitHub issue(未トリアージ、無ラベル)
        │
        │  人間がラベルを選ぶ ── 2つの入口 ──
        ├──────────────────────────────┐
        ▼                               ▼
  meguri:plan(spec 先行、opt-in)    meguri:ready(直行)
        │                               │
        ▼                               │
  planner (author)                      │
    調査 → spec PR を open               │
    issue: plan→speccing                │
    PR: meguri:spec-reviewing           │
        │                               │
        ▼                               │
  spec_reviewer (review, 独立pane)       │
    clean → PR: spec-ready              │
    findings → PRコメント、次の push待ち   │
    (同じ head は1回だけレビュー)          │
        │                               │
        ▼                               ▼
  spec_worker (author, 同一PRを継続)    worker (author)
    実装 commit を同じ branch/PR に積む    issue: ready→implementing
    spec を削除(disposable、ADR 0001)          │
    issue: speccing→implementing              ▼
    → そのまま完了(PR は既に open 済み)  self-review(内部ループ、ADR 0006、worker のみ)
    ※ SpecWorkerFlavor は self_reviews() execute → validate → self-review ⇄ fix
      を override しないため、右側の      (ラウンド上限まで、forge には一切触れない)
      内部 self-review フェーズは通らない        │
        │                                       ▼
        │                                PR open(`Closes #N`)
        │                                       │
        └───────────────────┬───────────────────┘
                             ▼
        (spec 先行経路はここで合流 — PR は既に open 済み)
   ┌─────────────────────────┼──────────────────────┐
   ▼                          ▼                       ▼
 fixer                    ci_fixer             conflict_resolver
 (author,                 (author,               (author,
  継続)                     継続)                   継続)
 人間/外部botの              CI red →               CONFLICTING →
 未解決スレッド               fix push              base merge・解消 push
 に返信                     (≤3 round)             (≤3 round)
   └─────────────────────────┴──────────────────────┘
                 │
                 │  ここから先は run/pane を持たない「帯域外 sweep」(§2)
                 ▼
     auto_merger.sweep (opt-in, ADR 0003)
     条件が揃った PR に GitHub-native auto-merge を arm
                 │
                 ▼
     merge_watch.sweep (ADR 0007)
     conflict/red CI は fixer 系に委譲(no-op)。
     どのループも拾わない stall だけ meguri:needs-human で escalate
                 │
                 ▼
        GitHub が branch protection + required checks で
        マージを確定 → issue close(Closes #N)


cleaner (standalone) ── パイプラインの外を独立に回る
  default branch head を定期巡回し、乖離を単一の
  meguri:clean-report issue に報告するだけ(read-only)
```

補足:

- **local モード**(GitHub なし)は spec 先行経路を持たない。`planner`(および spec_reviewer / spec_worker)がまだ無く(issue #54 Phase 3)、`PlannerLoop::discover` は `deps.forge.is_none()` なら空を返すため、ローカルの `plan` task は queued のまま dormant になる。worker が `needs_plan` を返しても、local task は forge 越しの planner 委譲ができないため人間へエスカレーション(`NeedsHuman`)される。つまり local モードで実際に回るのはこの図の `meguri:ready` 直行経路(worker → self-review → 完了)だけ。discovery の入口自体も GitHub ラベルではなく meguri のローカル task queue になり(`TaskSource` 抽象、[ADR 0003(tasksource-task-moves-run-pins)](../adr/0003-tasksource-task-moves-run-pins.md))、成果物も PR ではなく検証済みローカルブランチになる。詳細は README の「Local mode」を参照。
- fixer / ci_fixer / conflict_resolver は互いに排他ではなく、同じ PR に対して並行して起こりうる(スレッド対応・CI 修正・conflict 解消は独立事象)。図は簡略化のため並列に描いているが、実際の駆動順は §2 のディスパッチ優先度に従う。

## 2. ディスパッチ優先度

現行の `default_loops()`(`src/engine/mod.rs`)の**登録順そのものが優先度**である。プリエンプションは無く、`Loop` trait に `priority()` のような機構も無い — 並び順そのものが仕様([ADR 0001-scheduler-priority-wip-first](../adr/0001-scheduler-priority-wip-first.md))。

```
conflict_resolver → ci_fixer → fixer → spec_worker → spec_reviewer → worker → planner → cleaner
```

これは**パイプラインの逆順**(merge に近い側から先取り)であり、背後の原則は一つだけ:**新規着手より仕掛かりの完了を優先する(WIP を減らす)**。同一ループ内は issue/PR 番号の昇順(FIFO) — 古い仕掛かり品ほどコンフリクトのリスクが溜まるため、先に生まれたものを先に完了させる。複数プロジェクト構成ではループ→プロジェクトの順で走査するため、優先度がプロジェクト順より強く効く。

### 帯域外(out-of-band)sweep

`default_loops()` の**外**で、`scheduler.rs` の poll tick から直接呼ばれる軽量 API 掃引が3つある。いずれも `Loop` trait を実装せず、run レコードも pane も持たないため、上のディスパッチ優先度リストには現れない:

| sweep | 役割 | ADR |
|---|---|---|
| `reaper::sweep` | close された issue の pane・worktree・マージ済みローカルブランチを回収 | [0004-issue-lane-pane-session-lifetime](../adr/0004-issue-lane-pane-session-lifetime.md) |
| `auto_merger::sweep` | 条件が揃った PR に GitHub-native auto-merge を arm(opt-in) | [0003-auto-merge-github-native-arm-only](../adr/0003-auto-merge-github-native-arm-only.md) |
| `merge_watch::sweep` | arm 済み PR のドリフト検出。conflict/red CI は fixer 系ループに委譲(no-op)、拾われない stall だけ escalate | [0007-merge-watch-defers-to-fixer-loops-and-backstops-drift](../adr/0007-merge-watch-defers-to-fixer-loops-and-backstops-drift.md) |

3つは実行順に固定されている(reaper → auto_merger → merge_watch)。新しく arm した PR を同じ tick 内で merge_watch が一度観測できるよう、auto_merger の後に merge_watch が続く。

## 3. loop 別ライフサイクル

README の「ループ別の寿命の一覧」を、設計視点([ADR 0004-issue-lane-pane-session-lifetime](../adr/0004-issue-lane-pane-session-lifetime.md)の lane モデル)で再構成したもの。事実の一次情報は README とコード(`src/engine/*.rs` の各 loop 冒頭コメント)にあり、ここは表として横断しやすくしたものに過ぎない。

| loop | lane(role) | trigger | 鍵 | worktree | 正常終了 | pane 後始末 |
|---|---|---|---|---|---|---|
| planner | author | `meguri:plan` issue | issue | 新 branch | spec PR 作成、issue: `plan`→`speccing` | 維持(author pane) |
| spec_reviewer | review(独立) | `spec-reviewing` PR、head 未レビュー | issue + `review` lane | read-only detached、`review-<issue>` 固定 | clean → PR: `spec-ready` / findings → PR コメント、次の push 待ち | 維持(独立 pane) |
| spec_worker | author(継続) | `spec-ready` PR | issue(branch から復元) | 既存 branch を継ぐ | 実装 commit → 同一 PR、issue: `speccing`→`implementing` | 維持、author pane を継続 |
| worker | author | `meguri:ready` issue | issue | 新 branch | self-review(内部)→ PR `Closes #N`、issue: `ready`→`implementing` | 維持(author pane) |
| fixer | author(継続) | PR の未解決スレッド(人間/外部bot) | issue(branch から復元) | PR head に attach | スレッドに返信、再レビュー待ち | 維持、author pane を継続 |
| ci_fixer | author(継続) | meguri PR の CI red | issue(branch から復元) | PR head に attach | fix push(≤3 round)、超過は `needs-human` | 維持、author pane を継続 |
| conflict_resolver | author(継続) | meguri PR が `CONFLICTING`(≤3) | issue(branch から復元) | PR head に attach | base merge・解消 push、解消不能は `needs-human` | 維持、author pane を継続 |
| cleaner | standalone(lane モデル外) | レポート issue + default branch 前進(`clean.interval_hours`) | レポート issue | read-only detached | 単一レポート issue を再生成 | 自前回収 |

補足:

- **author lane** は同じ branch を編集する loop 全員(planner → worker/spec_worker → fixer/ci_fixer/conflict_resolver)が同一 pane・同一 claude session を共有し、文脈を継ぐ。**review lane** は spec_reviewer 専用の独立 pane(別 session)。**standalone** は cleaner のみで lane モデルの対象外。
- pane・worktree はいずれも issue が寿命の単位で、issue が close されると `reaper::sweep` が回収する(watch 実行中はポーリングのたびに、一発実行では `meguri prune`)。
- 表に無い `auto_merger.sweep` / `merge_watch.sweep` は `Loop` trait を実装しない軽量 API 掃引のため、pane も worktree も持たない(§2 参照)。

## 4. 横断原則

個々の loop の実装を読む前に踏まえておくべき、全 loop 共通の原則。

- **Authority(forge が唯一の永続状態)** — looper 由来の原則。GitHub のラベル・コメントが durable な workflow state であり、ローカル sqlite(`~/.meguri/meguri.sqlite`)は run 実行の進行管理にしか使わない。meguri をいつ kill しても forge から復旧できるのはこの原則のおかげ。ADR 0006 の内部ループ化(「内部ループでも forge には一切触れない」)や ADR 0007 の merge-watch(「専用マーカーを持たず forge から毎回導出する」)は、いずれもこの原則の直接の帰結。詳細は README の「[The completion contract](../../README.md#the-completion-contract)」および ADR 0006 / 0007 を参照。
- **ラベル二軸(phase × ball)** — [ADR 0005-issue-labels-two-axis-phase-and-ball](../adr/0005-issue-labels-two-axis-phase-and-ball.md)。フェーズ軸(`plan`/`speccing`/`ready`/`implementing`)が issue の位置を、ボール軸(`working`/`needs-human`/`hold`)が誰の番かをフェーズに重ねて示す。無ラベル issue = 未トリアージという一義性はここから生まれ、discovery が読む唯一の入力でもある。
- **role routing** — [ADR 0003-role-based-agent-routing](../adr/0003-role-based-agent-routing.md)。エージェント振り分けは issue の難易度推定ではなく `loop_kind`(役割)を軸にする。明示設定(`[routing.roles]`)は常に auto に勝ち、プロファイル未定義や CLI 未検出は起動時に大きな音を立てて落ちる(静かなフォールバックはしない)。
- **AI 実装レビュー = 内部ループ** — [ADR 0006-ai-implementation-review-is-an-internal-loop](../adr/0006-ai-implementation-review-is-an-internal-loop.md)。実装 diff の self-review は worker の run の worktree 内(`validate` と `open-pr` の間)で完結し、forge には一切触れない。GitHub 上のレビュー transport は人間・外部 bot 専用に残る(fixer が拾うのは人間/外部 bot のスレッドだけ)。
- **auto-merge = arm-only** — [ADR 0003-auto-merge-github-native-arm-only](../adr/0003-auto-merge-github-native-arm-only.md)。meguri は「マージして安全か」を自前で判定せず、条件の揃った PR に GitHub-native auto-merge(`gh pr merge --auto`)を arm するだけで、最終判断は GitHub(branch protection + required checks)に委ねる。opt-in(`[pr.auto_merge].enabled` + `meguri:automerge` ラベル)で、fail-fast(リポジトリが条件を honor できなければ起動時に拒否)。
- **merge-watch = ドリフト検出であってマージ権威ではない** — [ADR 0007-merge-watch-defers-to-fixer-loops-and-backstops-drift](../adr/0007-merge-watch-defers-to-fixer-loops-and-backstops-drift.md)。conflict / red CI は conflict_resolver / ci_fixer が arm と無関係にすでに拾っているため、merge-watch はそれらに介入せず no-op にする(`needs-human` を貼れば fixer 系ループ自身を締め出しデッドロックする)。merge-watch が固有に escalate するのは「どのループも拾わないまま放置された arm 済み PR」だけ。

## 5. ADR 索引(loop に関係するもの)

縦の理由(個別決定とその背景)への入口。1決定1ファイルの原則どおり、詳細は各 ADR を参照。

| ADR | 一行要約 |
|---|---|
| [0001-scheduler-priority-wip-first](../adr/0001-scheduler-priority-wip-first.md) | ディスパッチ優先度はパイプラインの逆順に固定。新規着手より仕掛かりの完了を優先する。 |
| [0001-specs-are-disposable-scaffolding](../adr/0001-specs-are-disposable-scaffolding.md) | spec は使い捨ての足場。実装時に刈り、残す価値のある決定は ADR / ドメイン文書へ振り分ける。 |
| [0003-role-based-agent-routing](../adr/0003-role-based-agent-routing.md) | エージェント振り分けは役割ベース。明示は auto に勝ち、失敗は起動時に大きな音を立てる。 |
| [0003-auto-merge-github-native-arm-only](../adr/0003-auto-merge-github-native-arm-only.md) | 自動マージは GitHub-native auto-merge への arm が基本。安全判定は GitHub に委ねる。 |
| [0003-cleaner-read-only-single-report-issue](../adr/0003-cleaner-read-only-single-report-issue.md) | hygiene ループは read-only detector から始め、書き込み境界を単一レポート issue に限定する。 |
| [0004-issue-lane-pane-session-lifetime](../adr/0004-issue-lane-pane-session-lifetime.md) | 寿命の単位は issue。pane は `(project, issue, lane)` で鍵り、author/review lane を分ける。 |
| [0005-issue-labels-two-axis-phase-and-ball](../adr/0005-issue-labels-two-axis-phase-and-ball.md) | issue ラベルは「フェーズ × ボールの所在」の2軸。無ラベル = 未トリアージが一義になる。 |
| [0006-ai-implementation-review-is-an-internal-loop](../adr/0006-ai-implementation-review-is-an-internal-loop.md) | AI 実装レビューは内部ループ。GitHub は人間・外部レビューにだけ残す。 |
| [0007-merge-watch-defers-to-fixer-loops-and-backstops-drift](../adr/0007-merge-watch-defers-to-fixer-loops-and-backstops-drift.md) | merge-watch は fixer 系ループに委譲し、どのループも拾わない stall だけを backstop する。 |
| 0008-symmetric-plan-impl-review-loop(**in-flight**、#132 / PR #140、未マージ) | plan/impl ループの対称化: 内部 self-review は必須(多角視点)、GitHub guard レビューは任意。着地待ち — §6 参照。 |

## 6. 注意: #132 / ADR 0008 は in-flight

**#132(spec/impl ループの対称化)で loop モデルが大きく動く。** issue #132 の spec は spec PR [#140](https://github.com/kkato1030/meguri/pull/140) として進行中で、着地予定のファイル名は `docs/adr/0008-symmetric-plan-impl-review-loop.md` — ただし本 doc 作成時点ではまだ `main` にマージされていない(`docs/adr/0008-agent-instructions-via-apm.md` が既に 0008 を使っているため、着地時の採番は別番号にずれる可能性がある。#132 のリンクは着地後に確認して直すこと)。

着地すると想定される変化:

- `spec_reviewer` が `guard(kind)` へ一般化される(`kind = Plan | Impl` パラメータ化)。spec_reviewer は guard の Plan 特化版に格下げされる。
- 内部 self-review(多角視点)が spec 側にも必須化される(現行は impl 側のみ必須、spec 側は GitHub 上の spec_reviewer が必須ループ)。
- `plan_delivery = separate | combined` の project config が増え、spec PR と impl PR が1本にまとまる `combined` モードが選べるようになる。
- guard レビューの出力先が commit status + PR 本文 `<details>` になり、auto-merge の arm 条件に `guard-review success` が加わる。

**本 doc は #132 着地前の現行モデルを正として書いている。** #132 の実装が着地したら、本 doc(特に §1 パイプライン図・§3 ライフサイクル表)を追随して更新する — #132 自体の文書フェーズ(spec に記載の Done の目安「新 ADR で本設計を記録」)でこの doc も一緒に更新するのが望ましい。
