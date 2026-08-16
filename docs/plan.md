# meguri 開発計画

## 1. 概要

meguri は、**Intent を実行可能な Work Graph に変換し、人間と AI による実行・判断を通して Goal への到達を管理する Delivery Control Plane** を目指す。

初期は権威・実行・永続化をローカルに置く(local-first)。ただしこれは**現時点の設計の重心であって、恒久的な制約ではない**。実行系は将来リモート runtime へ拡張しうる(§8)し、権威や永続化の所在も要求が現れれば見直しうる。local-first は今の一手であり、meguri の定義ではない。

AI Agent 自体やコード実行環境を独自実装するのではなく、Claude Code / Codex 等の既存 Agent、ACP、herdr / tmux、GitHub 等を組み合わせる。

meguri が所有するのは、主に以下とする。

* Intent
* Desired State
* Work Graph
* Work の実行状態
* Work 間の依存関係
* Human Gate
* 実行・判断の履歴
* 「現在どこまで進んでいるか」
* 「次に何をすべきか」の判断

GitHub Issues / PR 等は source of truth ではなく、meguri の state の **projection / integration surface** として扱う。

本計画はゼロからの新規開発である。ただし旧 meguri(v1: issue 駆動 autonomous loop / v2: 増分書き直し)で実費を払って得た教訓は、コードではなく**設計原則として持ち込む**(§3.5)。旧実装の履歴と ADR 群(失敗カタログ)はリモートの archive ブランチ(`archive/v1-main` / `archive/v2` / `archive/v2-preflight`)に保存されており、各増分の設計前に該当 ADR を参照する。

---

## 2. 基本ワークフロー

```text
Intent
  ↓
Human + AI による対話
  ↓
Desired State
  ↓
Work Graph の提案
  ↓
Human Approval
  ↓
Ready Work の選択
  ↓
AI Agent に実行を委譲
  ↓
実装 / Test / Commit
  ↓
meguri による独立検証
  ↓
PR 作成
  ↓
Human Review
  ├─ Rework
  └─ Merge
       ↓
Work Complete
       ↓
Graph Reconciliation
       ↓
Next Ready Work
       ↺
```

現在行っている開発プロセスを、できる限りそのままソフトウェアとして表現する。

このワークフローは happy path である。失敗経路(検証落ち・沈黙・タイムアウト・pane 死亡)は §9 で本編として扱う。**失敗経路こそが実行系の本体である** — これは旧実装の歴史全体から得た結論。

---

## 3. 設計原則

### 3.1 Agent を作らない

Claude Code / Codex 等を Worker として利用する。

meguri は「どうコードを書くか」ではなく、

* 何を実現するか
* 何を次に実行するか
* 何が完了したか
* どこで人間の判断が必要か

を管理する。

### 3.2 GitHub を Source of Truth にしない

meguri 内部に独立した Work identity を持つ。

```text
meguri Work
    │
    ├─ GitHub Issue
    ├─ GitHub PR
    ├─ Agent Session
    ├─ Commit
    └─ その他 Artifact
```

GitHub が存在しなくても Work Graph 自体は成立する。

### 3.3 Planning と Execution を分離する

```text
Planning Plane

Human
  ↕ ACP
AI Agent
  ↕
meguri
  ↓
Intent / Desired State / Work Graph


Execution Plane

meguri Reconciler
  ↓
herdr / tmux
  ↓
Claude / Codex
  ↓
git changes
```

### 3.4 Human Judgment を残す

自動化するのは、

* Work の選択
* Agent 起動
* 実装
* Test
* Commit
* PR 作成
* 状態追跡

まで。

少なくとも初期段階では、PR の採用・Merge は人間が判断する。

### 3.5 持ち込む不変条件

以下は旧 meguri で実際に事故ってから獲得した原則である。コードは捨てても、これらは最初から設計に織り込む。

1. **耐久シグナルのみで判断する。** 成否の判断材料は result ファイル・pane の生死・git の状態だけ。エージェントの画面は読まない。
2. **Trust-but-verify。** Agent の自己申告を信じない。完了 = meguri 側の独立検証(working tree が clean / base から commit が進んでいる / check_command 通過)が揃うこと。
3. **ブロック ≠ 失敗。** どの結末でも pane を残し、人間が attach して引き取れる。人間の介入をエラーとして扱わない。
4. **完了コントラクトは prompt に埋め込む。** result ファイルの形式・コミット作法は prompt 自身に書く。Agent 側の事前設定を要求しない。
5. **pane / worktree / branch は Work の識別子でキーする。** Issue 番号等の外部 ID に依存させない。
6. **forge read に予算を持つ。** 旧実装では GitHub GraphQL 5000/hr を daemon が食い切る運用事故が実際に起きた。ポーリング設計は必ず予算から逆算する。
7. **`ready` / `blocked` は保存しない。** 保存するのは事実(planned / running / done / failed …)だけで、実行可能性は Graph から毎回導出する(§6)。
8. **Reconciler は level-triggered。** identity だけを受け取り、毎回全状態を読み直して次の一手を決める(§12)。イベントの取りこぼしが状態の矛盾にならない構造にする。

---

# 4. Domain Model

## Intent

ユーザーが実現したいこと。

例:

```yaml
intent:
  id: intent-001
  title: "認証基盤を production-ready にする"
  description: |
    OAuth ベースの認証基盤を整備し、
    本番利用可能な状態にする。
```

## Desired State

Intent が達成されたと判断できる状態。

例:

```yaml
desired_state:
  - OAuth login が成立する
  - session が永続化される
  - logout が可能
  - expired session が適切に処理される
  - E2E test が存在する
```

Desired State は AI が自動決定するのではなく、Human + AI の対話によって合意する。

Desired State の充足判定は、MVP では機械化しない。**手動 trigger + 人間判断**とする(§14)。

## Work

自律実行可能な bounded work unit。

Work は以下を満たすことを目指す。

* Objective が明確
* Acceptance Criteria が存在する
* 完了を独立して検証できる
* Agent が一定時間自律実行できる程度に閉じている
* 他 Work との依存関係を表現できる

例:

```yaml
work:
  id: work-042
  title: "OAuth state validation を実装する"

  objective: |
    OAuth callback 時に state parameter を検証する。

  acceptance:
    - 不正な state を拒否する
    - 正常な callback が成功する
    - integration test が存在する

  depends_on:
    - work-018

  state: planned
```

---

# 5. Work Graph

Work は DAG として管理する。

```text
             ┌── W2 ── W5
Intent → W1 ─┤
             └── W3 ── W4
```

DAG から機械的に以下を導出する。

* ready
* blocked
* critical path
* downstream impact
* 次に実行可能な Work

例:

```text
Completed
  W1

Running
  W2

Ready(導出)
  W3

Blocked(導出)
  W4 ← W3
  W5 ← W2
```

---

# 6. Work Lifecycle

保存する状態と導出する状態を分ける。

**保存する状態(事実)**:

```text
planned
   ↓
starting
   ↓
running
   ↓
verifying
   ↓
awaiting_human
   ↓
accepted
   ↓
done
```

例外系:

```text
running
 ├─ blocked   (Agent が判断待ちで停止。pane は生きている)
 └─ failed

verifying
 └─ (検証落ち) → running   fix turn として差し戻し(上限付き、§9)
                  上限超過 → failed

failed
 └─ 人間行き。人間の裁定で
     - retry(新しい実行として running へ)
     - 仕様を見直して re-plan(§14)
     - discard(Work を閉じる)

awaiting_human
 └─ rework
      ↓
    running
```

**導出する状態(保存しない)**:

* `ready` = `planned` かつ全依存が `done`
* `blocked (graph)` = `planned` かつ未完了の依存がある

状態を必要以上に細分化しない。導出値を保存しないことで、遷移イベントの取りこぼしが矛盾として固着しない。

rework / retry のセッション意味論: **会話可能な Agent セッションが残っている場合のみ resume し、それ以外は新規セッションで開始する**(旧 ADR 0029 の結論)。

---

# 7. ACP の利用

ACP は主に **Planning Plane** で使用する。

主な用途:

* Intent の解釈
* Desired State の定義
* 不明点についての Human-AI 対話
* Work への分解
* Work Graph の提案
* Graph の再計画

UX イメージ:

```text
Human:
「認証をちゃんとしたい」

Agent:
「今回の 'ちゃんと' を以下として定義してよいですか?」

- OAuth login
- session persistence
- logout
- expiration handling
- E2E testing

Human:
「今回は password login は対象外で」

Agent:
「了解しました。Work Graph を提案します」
```

AI が直接 state を確定するのではなく、

```text
Current Graph
      ↓
Agent Proposal
      ↓
Graph Diff
      ↓
Human Approval
```

を基本とする。

**統合リスクの扱い**: ACP over Claude Code / Codex で planning 対話に必要な往復が実際に通るかは、v0.1 の最初に spike で検証する(§16)。詰まった場合の退路は headless 実行 + ファイル渡し(proposal を構造化ファイルで受け取り、対話は通常のチャットで行う)。

---

# 8. Execution Runtime

初期実装では **herdr を優先**する。

```text
meguri
  ↓
ExecutionRuntime
  ↓
HerdrRuntime
  ↓
workspace / pane
  ↓
Claude Code / Codex
```

将来的には interface を通して、

```text
ExecutionRuntime
 ├─ HerdrRuntime
 ├─ TmuxRuntime
 └─ RemoteRuntime
```

へ拡張できるようにする。

ただし v0.x では HerdrRuntime のみ実装する。抽象(trait 等)は 2 つ目の実装が実際に現れた増分で導入する。

---

# 9. Work 実行

`ready` な Work を開始する場合:

```text
Work Ready
   ↓
Actor Selection
   ↓
workspace / worktree 作成
   ↓
herdr pane 作成
   ↓
Agent 起動
   ↓
Work Context 投入
   ↓
implementation / test / commit
   ↓
result 申告
   ↓
meguri 側の独立検証
   ├─ pass → Artifact Produced
   └─ fail → fix turn として差し戻し(上限付き)
```

Agent に渡す Context には最低限以下を含める。

* Intent
* Desired State
* 対象 Work
* Acceptance Criteria
* 関連 Work
* Repository context
* Coding / project instructions
* 完了の作法(result ファイル形式・コミット作法・検証があることの予告)

## 9.1 独立検証(trust-but-verify)

Agent が success を申告したときだけ、meguri が独立に検証する:

1. working tree が clean(未 commit の変更なし)
2. base から commit が進んでいる(何もせず success を弾く)
3. check_command が通る(設定時。meguri 自身が実行)

検証落ちは fix turn として Agent に差し戻す(回数上限付き)。上限超過は `failed`。

## 9.2 失敗経路

* **沈黙**: 一定時間 result が出ない → nudge を打つ(回数上限付き)。上限超過はタイムアウト。
* **タイムアウト**: Work を `failed` として記録するが、**pane は殺さない**。人間が attach して引き取れる。
* **pane 死亡**: result 未提出のまま pane が死んだら `failed`。
* **needs_human 申告**: Agent 自身が人間の判断が必要と申告したら `blocked`。pane を残して人間に引き継ぐ。

## 9.3 並列実行の既知の罠

* 同一 repo への並行 `git worktree add` は `.git/config` の lock で衝突する(旧実装で CI が実際に検出)。repo 単位の排他を最初から入れる。
* worktree 個別の `info/exclude` は git が読まない(common dir 共有)。exclude は共有側 `.git/info/exclude` に書く。
* claude は fresh worktree で folder-trust ダイアログを出して run が始まらないことがある。pane 起動前の preflight(権限を一切与えない headless 1 発で trust を記録)を用意する。

---

# 10. Artifact Model

Agent の成果を GitHub PR そのものとして扱わない。

meguri の内部では Artifact を持つ。

```yaml
artifact:
  id: artifact-123
  type: git-change

  work: work-042

  repository: /repos/example
  base_revision: abc123
  head_revision: def456

  branch: meguri/work-042
```

GitHub Adapter が有効な場合:

```text
GitChangeArtifact
       ↓
GitHub Adapter
       ↓
Pull Request
```

と projection する。

---

# 11. GitHub Integration

GitHub は外部 projection として扱う。実態が双方向同期に膨らむと所有権の衝突(人間が GitHub 側で編集したら?)が必ず起きるため、**所有権を明示的に列挙して固定する**。

## 11.1 meguri → GitHub(outbound projection)

meguri が所有し、GitHub 側へ一方的に投影するもの:

```text
WorkCreated
  → GitHub Issue 作成

WorkStarted
  → label: in-progress

ArtifactProduced
  → PR 作成 / Link

AwaitingHuman
  → review request

WorkCompleted
  → Issue close
```

GitHub 側でこれらが書き換えられても meguri の state は変わらない(次の projection で上書きされうる)。

## 11.2 GitHub → meguri(inbound、最小限)

初期に受ける inbound イベントは以下**のみ**とする:

```text
GitHub PR merged
       ↓
ArtifactAccepted → WorkCompleted

GitHub PR closed(unmerged)
       ↓
人間の裁定として Work を rework / failed へ
```

Issue 本文の編集・ラベル操作・コメント等は inbound として扱わない。拡張するときは、そのフィールドの所有権を meguri から GitHub へ移す判断として明示的に行う。

## 11.3 read 予算

merge 検知はポーリングになる。**API read の予算(req/hr)を設定として持ち、そこからポーリング周期を逆算する**(不変条件 §3.5-6)。

---

# 12. Reconciler

meguri の中核。**level-triggered** で動く: Work の identity だけを受け取り、毎回全状態(Graph・実行状態・Artifact・projection)を読み直して次の一手を決める。ループはコード上に存在せず、reconcile + requeue の合成として現れる。

```text
Observe(全状態を読み直す)
     ↓
Ready Work exists?(導出)
     ↓
Execution capacity available?
     ↓
Start Work
     ↓
Observe execution
     ↓
Artifact produced?
     ↓
Request human judgment
     ↓
Accepted?
     ↓
Complete Work
     ↓
Unlock downstream Work(導出が変わるだけ)
     ↺
```

初期段階では高度な AI scheduling は行わない。

まずは、

```text
state = planned
AND all dependencies = done
```

なら実行候補とする。

---

# 13. Human Gate

AI が Work を完了しても、自動 Merge は行わない。

```text
Implementation
      ↓
Test
      ↓
meguri 検証
      ↓
PR
      ↓
CI
      ↓
awaiting_human
      ↓
Human Judgment
 ┌────┴────┐
 ↓         ↓
Rework    Accept
            ↓
          Merge
            ↓
           Done
```

Human Judgment は「Acceptance Criteria を満たしたか」だけでなく、

* 良い設計か
* 複雑性は妥当か
* この方向へプロダクトを進めたいか
* 将来的な保守性は良いか

といった価値判断を担う。

**GitHub 不在時の Gate**: v0.2(GitHub 連携前)では、`meguri accept <work>` / `meguri rework <work>` のローカルコマンドで Human Gate を閉じる。GitHub 連携後は PR merge / close がこれの projection になる。

---

# 14. Graph Reconciliation

Work が完了するたびに、単純に次の Work に進むだけではなく、Graph 自体を再評価できるようにする。

```text
Work Completed
      ↓
Current Reality Changed
      ↓
Desired State と比較(人間が判断。MVP では手動 trigger)
      ↓
Graph still valid?
  ┌──────┴──────┐
 Yes            No
  ↓              ↓
Next Work     Re-plan
                 ↓
             Graph Diff
                 ↓
            Human Approval
```

初期 MVP では手動 trigger でよい。再計画の提案は Planning Plane(ACP 対話)が担い、確定は常に Human Approval を通す。

---

# 15. 永続化

meguri は「実行・判断の履歴」を所有すると言った以上、ストレージを持つ。

* 初期は **sqlite 一択**とする(単一ファイル、トランザクション、ローカル完結)。永続化の所在(ローカル / リモート)は現時点の選択であり、将来リモート runtime やチーム利用の要求が現れれば見直しうる(§1)。
* 保存するのは事実のみ: Intent / Desired State / Work(保存状態のみ)/ 依存 / Artifact / 履歴イベント / Agent session id。
* 導出値(ready / blocked / critical path)は保存しない。
* クラッシュ耐性の契約: 「実行中 turn の途中進捗のみ喪失可」。それ以外はプロセス再起動で復元できること。

---

# 16. UI / Interaction

**最初は CLI のみ**とする。Local Web UI は planning 仮説(§17 v0.1)が生き残ってから作る。

* Graph の可視化は当面 **Mermaid 出力**(`meguri graph --mermaid`)で足りる。
* Work 選択時の詳細(Objective / Acceptance / Dependencies / Session / Artifact / Logs)も CLI で表示する。

## Status View(CLI)

例:

```text
Goal: Authentication production-ready

Progress
  Done:             4
  Running:          2
  Ready:            1
  Blocked:          3
  Awaiting Human:   1

Critical Path
  W12 → W18 → W23

Next Recommended Work
  W18: OAuth state validation

Reason
  - dependencies resolved
  - on critical path
```

Web UI(Intent View / Graph View / Status View)は v0.4 以降の増分として、必要が確認されてから導入する。server + Web UI は依存の跳躍(async runtime / HTTP / frontend)であり、それが解く問題が現れてから払う。

---

# 17. MVP Scope

各増分は「1 増分 = 1 PR = レビューできるサイズ」で積む(§20 開発規律)。

## v0.1 — Planning

「Intent → Work Graph」に価値があるかを検証する。**server も Web UI も作らない**。

分割increments:

* **p0: ACP spike** — Claude Code / Codex と ACP で対話往復が通ることだけを確認する捨てコード。通らなければ退路(headless + ファイル渡し)へ切り替え、以降の設計を調整
* **p1: データモデル + 永続化 + CLI** — Intent / Desired State / Work / 依存 DAG の CRUD、sqlite、ready 導出、Mermaid 出力
* **p2: Planning 対話** — ACP 経由の対話 → Work Graph proposal → Graph Diff 表示 → Human approval で確定

### 完了条件

「曖昧な Intent を入力し、AI と対話しながら、納得できる Work Graph を作れる」。

---

## v0.2 — Local Execution

Work Graph を実際の AI coding に接続する。

* [ ] `ready` 導出からの Work 選択
* [ ] HerdrRuntime(interface 化は 2 実装目まで待つ)
* [ ] workspace / worktree 作成(repo 排他、exclude、preflight — §9.3)
* [ ] Claude Code / Codex 起動
* [ ] Work instruction injection(完了コントラクト込み)
* [ ] result 待機(耐久シグナルのみ)
* [ ] meguri 側の独立検証 + fix turn 差し戻し(上限付き)
* [ ] 沈黙 nudge / タイムアウト / pane 死亡の失敗経路
* [ ] Artifact registration
* [ ] `meguri accept` / `meguri rework`(ローカル Human Gate)

### 完了条件

「Graph 上の Work を選択すると、AI が独立 workspace で実装し、検証済みの git change が Artifact として登録され、ローカル accept で次の Work が ready になる」。

**この時点で 18 ステップ(§19)のローカル版(GitHub 抜き)が一周する。**

---

## v0.3 — GitHub Projection

* [ ] GitHub Adapter
* [ ] Work → Issue projection(outbound)
* [ ] Artifact → PR projection(outbound)
* [ ] PR merged / closed の検知(inbound はこれのみ、read 予算から周期を逆算)
* [ ] Work completion
* [ ] downstream Work unlock

### 完了条件

以下が end-to-end で通る。

```text
Intent
 ↓
Work Graph
 ↓
Ready Work
 ↓
AI Implementation
 ↓
meguri 検証
 ↓
PR
 ↓
Human Merge
 ↓
Work Done
 ↓
Next Work Ready
```

---

## v0.4 — Delivery Visibility

* [ ] Graph progress
* [ ] blocked detection
* [ ] critical path
* [ ] next work recommendation
* [ ] workload / running agents
* [ ] progress summary
* [ ] (必要が確認されたら)Local Web UI

### 完了条件

meguri が、

* 今どこまで進んでいるか
* 何で詰まっているか
* 次に何をすべきか

を説明できる。

---

## v0.5 — Planning / Scrum Projection

必要性を確認してから実装する。

候補:

* Sprint Goal
* Sprint projection
* Kanban View
* Timeline
* Estimate
* Schedule simulation
* Human / AI capacity
* milestone

Scrum 自体を core domain にはしない。

```text
Work Graph
    ↓
Scrum Projection
```

として扱う。

---

# 18. 初期 Non-Goals

以下は最初から作らない。

* 独自 LLM
* 独自 Coding Agent
* Cloud sandbox infrastructure
* Agent IDE
* GitHub clone
* Jira clone
* Scrum management suite
* 完全自動 Merge
* 複雑な AI scheduler
* 大規模 distributed orchestration
* enterprise permission management
* multi-user collaboration

---

# 19. 最初に通す Vertical Slice

最初の実装では、機能を横に広げず次の一本を通す。

```text
1. meguri を起動する(CLI)

2. Intent を作る

3. ACP で Claude / Codex と対話する

4. Desired State を決める

5. Work Graph を生成する

6. Graph を Human が approve する

7. 依存関係のない Work が ready(導出)になる

8. meguri が herdr workspace / pane を作る

9. Agent に Work を渡す

10. Agent が
    - 実装
    - test
    - commit
    まで行う

11. meguri が独立検証する(落ちたら fix turn)

12. Artifact を登録する

13. GitHub に PR を作成する

14. Work が awaiting_human になる

15. Human が PR を確認・merge する

16. meguri が merge を検知する

17. Work を done にする

18. 依存していた次の Work が ready(導出)になり、
    次の Agent execution を開始する
```

**この 18 ステップが一度通れば、meguri は最小の Software Factory として成立したとみなす。**(ステップ 13〜16 を `meguri accept` に置き換えたローカル版は v0.2 の完了条件。)

---

# 20. 開発規律

* **1 増分 = 1 PR = レビューできるサイズ。** レビューを通らない速度で書かない。旧実装の失敗(dogfooding が検証ゲートを外し、無検証の表面積が増殖した)の直接の再発防止。
* **地図を常に正確に保つ。** `docs/architecture.md` に「いま動くものだけ」を書き、機能を変える PR は必ず同時に更新する。未来の計画は本書(plan.md)の仕事。
* **失敗カタログ参照。** 旧実装の `docs/adr/`(リモート archive ブランチ `archive/v1-main` 等)は失敗カタログ。各増分の設計前に該当 ADR を読む。解を写す義務はないが、記録された失敗は再発させない。
* **依存最小・抽象後払い。** 新しい crate はそれが解く問題が現れた増分で足す。trait 等の抽象は 2 つ目の実装が現れてから。

---

# 21. 成功指標

最初はプロダクト指標より、自分自身の開発フロー改善を見る。

### Planning

* Intent から納得できる Work Graph まで作れるか
* Issue 分解に必要な人間時間が減るか
* Acceptance Criteria が明確になるか

### Execution

* Work を人間が手動で Agent に渡す操作が減るか
* Agent が途中介入なしで PR まで到達できる割合
* Work の失敗 / blocked 状態を正しく検出できるか

### Delivery

* 「今どこまで進んでいるか」がすぐ分かるか
* 「次に何をすべきか」を考える時間が減るか
* 並行する AI Work を人間が把握できるか

最終的には、

```text
Intent
  ↓
Human-AI Planning
  ↓
Work Graph
  ↓
AI Execution
  ↓
Human Judgment
  ↓
Goal
```

のサイクル全体について、人間が **作業のオペレーションではなく Intent と Judgment に集中できているか**を評価する。

---

# 22. meguri の定義

> **meguri is a delivery control plane that turns intent into an executable work graph and coordinates humans and AI agents toward the desired state.**

日本語では、

> **Intent を実行可能な Work Graph に変換し、人間と AI Agent による実行・判断を通じて Desired State への到達を管理する Delivery Control Plane。**

とする。

定義の核は「Intent → Work Graph → 実行・判断 → Desired State」のサイクルの管理であって、その実装がローカルかリモートかではない。初期は local-first で始めるが(§1)、それは現時点の重心であり、この定義には含めない。
