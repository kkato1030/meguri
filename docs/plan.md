# meguri 開発計画

## 1. 概要

meguri は、**Intent を実行可能な Outcome Graph に変換し、人間と AI による実行・判断を通して Desired State への到達を管理する Delivery Control Plane** を目指す。

初期は権威・実行・永続化をローカルに置く(local-first)。ただしこれは**現時点の設計の重心であって、恒久的な制約ではない**。実行系は将来リモート runtime へ拡張しうる(§8)し、権威や永続化の所在も要求が現れれば見直しうる。local-first は今の一手であり、meguri の定義ではない。

AI Agent 自体やコード実行環境を独自実装するのではなく、Claude Code / Codex 等の既存 Agent、herdr / tmux、GitHub 等を組み合わせる(ACP は検討の末いったん不採用 — §7 / §23 Q5)。

meguri が所有するのは、主に以下とする。

* Intent
* Outcome Graph(到達したい状態(Outcome)のグラフ。ノード = Outcome・Work = それを満たす手段。§4/§5、§23 Q1)
* Desired State(= トップレベルの Outcome 群)
* Work の実行状態
* Outcome 間の依存関係
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
Outcome Graph の提案
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

GitHub が存在しなくても Outcome Graph 自体は成立する。

### 3.3 Planning と Execution を分離する

```text
Planning Plane

Human ↔ AI Agent(pane で直接対話)
              ↓ proposal.json(構造化契約)
            meguri
              ↓
Intent / Desired State / Outcome Graph


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

### 3.5 実行系から持ち込む不変条件

以下は旧 meguri で実際に事故ってから獲得した原則である。コードは捨てても、これらは最初から設計に織り込む。

**偏りについて明示しておく**: 旧 meguri(v1/v2)は実行系に振り切った実装(issue 駆動の autonomous loop)だったため、実費を払って得た傷はほぼ実行・連携レイヤに集中している。以下の 8 項目も 1〜6 は実行/連携、7〜8 のみが構造(ドメインモデル / reconciler)である。**Planning Plane は旧実装がまともに作らなかった領域で、継承すべき不変条件がまだ無い** — v0.1 が最高リスクなのはそこが新規 territory だからで、計画系の不変条件はこれから自前で発見していく。なお 2(trust-but-verify)・3(ブロック≠失敗)・8(level-triggered)は実行系の顔をしているが本質は横断スタンスで、計画系にも効く(Graph を valid と信じず再導出する、人間の介入をエラー扱いしない、等)。

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

> **グラフの土台は Outcome graph に確定した(§23 Q1、B 案)。** ノードは「到達したい状態(Outcome)」であり、辺は Outcome 間の requires/enables。**Work はノードではなく、Outcome を「未充足 → 充足」に反転させる手段**として Outcome にぶら下がる。これにより、旧構造で別々だった **Desired State / Acceptance Criteria / グラフ**が 1 つに畳まれる。ただし機構は dumb に保つ(1 Outcome 1 アプローチ、OR 自動探索なし、自動 planner なし)。

## Intent

ユーザーが実現したいこと。グラフの根。

例:

```yaml
intent:
  id: intent-001
  title: "認証基盤を production-ready にする"
  description: |
    OAuth ベースの認証基盤を整備し、
    本番利用可能な状態にする。
```

## Outcome(グラフのノード)

到達したい状態。Intent の **Desired State はトップレベルの Outcome 群**であり、粗い Outcome(マイルストーン)は子 Outcome の充足の合成として表す。

Outcome は以下を持つ。

* **statement**: 到達状態の宣言(「〜されている」)
* **requires**: 前提となる他 Outcome(辺)
* **verify**: この状態を満たせたかの確かめ方(下記)。省略した Outcome は **まとめ節点(マイルストーン)**

### verify(確かめ方)と satisfied(達成)を分ける

同じ Outcome について、時間軸の違う 2 つを別に持つ。

* **verify** — ある作業ブランチが狙った状態を満たしているか。**Work の完了時**に評価する(§9.1)。Human Gate に上げてよいかの門番。
* **satisfied** — Outcome が今ほんとうに達成されているか。**保存しない・導出値**。

verify の種類は 3 つ(**v0.1 はこの 3 つに絞る**):

| verify | 確かめ方 | satisfied(達成)の定義 |
|---|---|---|
| `command` | シェルコマンドが exit 0(テスト・ビルド・検査) | 担当 Work が verify を通り、かつ**マージされた** |
| `human` | 人が「達成」と表明 | 人が表明したら達成。**計画やり直しまで維持(sticky)** |
| (省略=まとめ節点) | 自分では確かめない | **子(requires)が全て satisfied** になったら達成 |

`command` は権威 tree のコミットでキャッシュし、**コードが変われば自動で再評価**(先祖返りで達成が自動的に外れる)。`human` は sticky。どの Outcome も verify を必ず持つ(既定 `human`)— 計画対話で「達成をどう確かめる?」を毎回問わせ、受け入れ基準を鋭くするため。

> **verify の芯**(§23 Q6、北極星): この 3 種は「**誰が verify するか**」の射影 —— `command`=computer Actor / `human`=human Actor / `rollup`=Actor でなく**構造**(meguri がグラフから導出)。4 種目の `ci`(GitHub Actions Actor、adapter 供給)は p3 で足す。verify は execute(Work の executor、§23 Q2)と同じ Actor 台帳を引く。

**v0.1 では作らない(が塞がない)確かめ方**: 「本番で実際に動いている(URL が応答する等の runtime 観測)」「外部ステータス(CI 緑等)」。これらは**コードが変わらなくても状態が変わる**ので、コミットでのキャッシュに乗らず別の観測サイクルが要る。delivery を名乗る以上ロードマップには残す(§23 Q4)。

例:

```yaml
outcome:
  id: outcome-042
  statement: "OAuth callback で不正な state が弾かれる"
  verify:
    kind: command          # command | human | (省略=まとめ節点)
    command: "cargo test state_validation"
  requires:
    - outcome-018          # "OAuth プロバイダ設定が存在する"
# satisfied は導出:
#   command → 担当 Work が verify 通過 + マージ済み。tree のコミット変化で自動再評価
#   human   → 人の達成表明が有効な間(計画やり直しまで sticky)
#   まとめ  → requires が全て satisfied
```

Outcome は AI が自動決定するのではなく、Human + AI の対話によって合意する(§7)。

## Work(Outcome を満たす手段)

自律実行可能な bounded work unit。**Outcome に紐づき、その Outcome を充足させるために起こす**。Work はノードではなく手段なので、1 つの Outcome に対して(失敗時など)複数の Work を付け替えられる — Outcome の identity は試みをまたいで安定する。

Work は以下を持つ。

* **serves**: 満たそうとしている Outcome(1 つ)
* **objective**: 何をするか
* **executor**: 実装フェーズを誰がやるか(`ai` 既定 | `human`。§23 Q2)
* **state**: 実行状態(§6 の Work Lifecycle)

Acceptance Criteria は Work ではなく **Outcome の predicate** が担う(完了の基準は「Outcome が充足したか」であって「Work が終わったと申告したか」ではない — trust-but-verify)。

例:

```yaml
work:
  id: work-311
  serves: outcome-042
  objective: |
    OAuth callback 時に state parameter を検証する処理を実装する。
  executor: ai
  state: planned
```

---

# 5. Outcome Graph

Outcome を DAG として管理する(ノード = Outcome、辺 = requires)。Work はノードではなくノードにぶら下がる手段。

```text
             ┌── O2 ── O5
Intent → O1 ─┤
             └── O3 ── O4
```

DAG と各 Outcome の充足状態から機械的に以下を導出する(いずれも**保存しない**)。

* satisfied / unsatisfied(Outcome ごと、predicate で判定)
* ready(unsatisfied かつ requires が全て satisfied → Work を起こせる)
* blocked(unsatisfied かつ未充足の requires がある)
* critical path / downstream impact

例:

```text
Satisfied
  O1

Working(Work 実行中)
  O2

Ready(導出: 未充足・前提充足)
  O3

Blocked(導出: 前提が未充足)
  O4 ← O3
  O5 ← O2
```

「ready な Outcome に Work を起こす」が実行の入口(§9)。「Outcome が satisfied か」が完了の基準(§9.1)。

**依存の解禁は satisfied(= マージ済み)基準【案A】**: 下流の着手は上流がマージされるまで待つ。単純・安全だが、鎖状の依存は「人間のマージ待ち」で直列化する。効くのは長い鎖のみ(横に広い形は上流マージで一斉に ready)なので、当面は**計画対話で鎖を短く・横に広く保つ**ことで緩和する。throughput が実測で問題になったら、上流が verify を通った時点で下流を解禁する【案B(stacked)】を将来レバーとして入れる(§23 Q4)。なお進みの本当の律速は人間レビューのゲートであり、案B は実装の並行化にしか効かない。

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

# 7. Planning 対話の実現(pane + 構造化契約)

Planning Plane でやること:

* Intent の解釈 / Desired State(= トップ Outcome 群)の合意
* 不明点についての Human-AI 対話
* Outcome への分解・Outcome Graph の提案 / 再計画

**実現手段は pane + 構造化ファイル契約を第一とする(§23 Q5、ACP は不採用)**。
人間が **生の Claude Code / Codex と pane(herdr/tmux)で対話**し、合意した
グラフをエージェントが `.meguri/proposal.json`(meguri のスキーマ)に書く。
meguri は **画面を読まず、その proposal ファイルだけを読む** —— 実行系の
完了契約(result.json)と同じ「耐久チャネルで構造化結果を受け取る」型(§8)。

UX イメージ:

```text
Human(pane 内で Claude/Codex に直接):
「認証をちゃんとしたい」

Agent:
「今回の 'ちゃんと' を以下として定義してよいですか?」
- OAuth login / session persistence / logout / expiration / E2E

Human:「password login は対象外で」

Agent:「了解。Outcome Graph を .meguri/proposal.json に書きます」
        → meguri がファイルを読み、現行グラフと diff して人間に提示
```

AI が直接 state を確定するのではなく、常に:

```text
Current Graph → Agent Proposal(proposal.json)→ Graph Diff → Human Approval
```

refine は「人間が同じ pane で対話を続ける → エージェントが proposal.json を
書き直す → meguri が再収穫」。meguri は会話のキーストロークを仲介せず、
**文脈を投入し・構造化提案を収穫し・diff/承認を持つ**ことだけを担う。

**なぜ ACP でないか**(p0 の実測を経た判断、§23 Q5): ACP は動くが未成熟
(アダプタ依存・版 churn・相手ごとにバラつく)で、planning に ACP・execution に
pane を使うと**エージェント境界が 2 つ**になる。pane + 契約なら planning と
execution が**同じ 1 つの型**を共有し、一級・最新・全エージェント対応
(Codex/Cursor もネイティブ headless なら動く)。ACP を再検討するトリガーは、
meguri が**自前のチャット UI をホストする**か **人間なしで自律 planning を回す**
必要が出た時、または ACP がベンダー純正・1.0 まで成熟した時。p0 のスパイクは
`archive/p0-acp-spike` ブランチに残す。

---

# 8. エージェント境界(planning と execution で共有する 1 つの型)

meguri とエージェントの接点は **1 つの抽象**に統一する:

> **文脈 + プロンプトを送り、耐久チャネル(ファイル / stdout-JSON)で
> 構造化結果を受け取る。画面は読まない。**

* planning: 提案を `.meguri/proposal.json` で受け取る(§7)
* execution: 結果を `.meguri/result.json` で受け取る(§9)

同じ「完了契約」の型なので、pane 起動・完了契約・trust-but-verify の機構が
両方で使い回せる。エージェント起動レシピの差(program / args / 除去する環境変数)
だけが相手ごとに変わる(p0 で実測: claude はネイティブ ACP 無しで CLAUDECODE の
unset が要る等 —— `archive/p0-acp-spike` ブランチ参照)。

> **Actor × Runtime**(§23 Q6、北極星): この境界の向こうにいるのは **Actor**(具体エージェントや human/ci)で、その届き方が **Runtime**(下記 Herdr/Tmux=ローカル pane / Remote=Web・cloud)。`Claude Code (CLI)` と `Claude Code Web` は Actor はほぼ同系で Runtime だけ違う。cloud Actor の耐久チャネルは多くが PR(GitHub adapter で観測)になるが、契約の形は不変。

初期実装では pane 供給に **herdr を優先**する。

```text
meguri
  ↓
Runtime(pane 供給 + 完了契約)
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
* serving Outcome(statement = 達成すべき状態、verify = 達成の確かめ方)
* 対象 Work(objective)
* 関連する前提 Outcome(requires)とその状態
* Repository context
* Coding / project instructions
* 完了の作法(result ファイル形式・コミット作法・verify があることの予告)

## 9.1 独立検証(trust-but-verify)= Outcome の verify

Agent が success を申告したときだけ、meguri が独立に検証する。これは **serving Outcome の `verify` を、その Work のブランチで評価すること**に等しい(§4):

1. working tree が clean(未 commit の変更なし)
2. base から commit が進んでいる(何もせず success を弾く)
3. **Outcome の `verify` が通る**:
   - `verify.kind = command` → そのコマンドをブランチで実行し exit 0
   - `verify.kind = human` → 機械検証は無い。そのまま Human Gate(§13)の人間判断が verify を兼ねる

検証落ちは fix turn として Agent に差し戻す(回数上限付き)。上限超過は `failed`。

**verify を通っただけでは Outcome は satisfied ではない**。satisfied になるのは、この後 Human Gate を経て **マージされた**とき(§4 の satisfied 定義)。verify(ブランチが良さそう)と satisfied(実際に届いた)は別の事実。

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

meguri の中核。**level-triggered** で動く: **Outcome の identity だけを受け取り**、毎回全状態(Graph・充足判定・実行状態・Artifact・projection)を読み直して次の一手を決める。ループはコード上に存在せず、reconcile + requeue の合成として現れる。Outcome graph 化(§23 Q1)により、これは **K8s controller と同型**になる — 「この Outcome は満たされているか? 未充足かつ前提が満たされているなら、それを満たす Work が存在/実行中であることを保証する」。

```text
reconcile(outcome):
  Observe(この Outcome と前提の状態を読み直す)
     ↓
  satisfied?(§4 の種類ごとの定義で判定)── Yes → Done(下流 Outcome の ready 導出が変わる)
     │ No
     ↓
  requires が全て satisfied?(導出)── No → Wait(blocked。何もしない)
     │ Yes
     ↓
  この Outcome を満たす Work がある?
     ├─ ない → capacity があれば Work を起こす(§9)
     └─ ある → 実行を観測。Artifact 出たら Human Gate(§13)へ
     ↺
```

初期段階では高度な AI scheduling は行わない。

まずは、

```text
outcome.satisfied = false
AND all requires satisfied = true
AND その Outcome に走行中の Work が無い
```

なら Work を起こす候補とする。「下流を unlock する」処理は存在しない — Outcome が satisfied になれば下流の ready 判定が導出で変わるだけ。

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

Outcome が充足するたびに、単純に次の Outcome に進むだけではなく、Graph 自体を再評価できるようにする。Outcome graph 化により、この再評価は「**どの Outcome が今 satisfied か、そしてまだこの Outcome 群を欲しいか**」の問い直しになる(目的は安定・手段は使い捨て、なので再計画が明快)。

```text
Outcome Satisfied
      ↓
Current Reality Changed
      ↓
各 Outcome の predicate を再評価 + Outcome 群がまだ欲しいか人間が判断(MVP では手動 trigger)
      ↓
Outcome Graph still valid?
  ┌──────┴──────┐
 Yes            No
  ↓              ↓
Next ready Outcome  Re-plan(Outcome の追加/削除/requires の張り替え)
                 ↓
             Graph Diff
                 ↓
            Human Approval
```

初期 MVP では手動 trigger でよい。再計画の提案は Planning Plane(pane 対話 + proposal.json)が担い、確定は常に Human Approval を通す。

---

# 15. 永続化

meguri は「実行・判断の履歴」を所有すると言った以上、ストレージを持つ。

* 初期は **sqlite 一択**とする(単一ファイル、トランザクション、ローカル完結)。永続化の所在(ローカル / リモート)は現時点の選択であり、将来リモート runtime やチーム利用の要求が現れれば見直しうる(§1)。
* 保存するのは事実のみ: Intent / Outcome(statement / predicate / requires)/ Work(serves / executor / 実行状態)/ Artifact / 履歴イベント / Agent session id。
* 導出値(Outcome の satisfied / ready / blocked / critical path)は保存しない。
* クラッシュ耐性の契約: 「実行中 turn の途中進捗のみ喪失可」。それ以外はプロセス再起動で復元できること。

---

# 16. UI / Interaction

**最初は CLI のみ**とする。Local Web UI は planning 仮説(§17 v0.1)が生き残ってから作る。

* Graph の可視化は当面 **Mermaid 出力**(`meguri graph --mermaid`)で足りる。
* Work 選択時の詳細(Objective / Acceptance / Dependencies / Session / Artifact / Logs)も CLI で表示する。

## Status View(CLI)

例:

```text
Intent: Authentication production-ready

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

「Intent → Outcome Graph」に価値があるかを検証する。**server も Web UI も作らない**。

分割increments:

* **p0: ACP spike(済)** — Claude/Gemini/Codex/Cursor で ACP 往復を実測。結論: ACP は動くが未成熟で、**pane + 構造化ファイル契約の方が筋が良い**と判断(§7 / §23 Q5)。捨てコードは main に入れず `archive/p0-acp-spike` ブランチに退避(PR は不採用で close)
* **p1: データモデル + 永続化 + CLI(済)** — Intent / Outcome(statement/verify/requires)/ Work の CRUD、sqlite、satisfied・ready・blocked 導出、Mermaid 出力。実装は `src/`(architecture.md 参照)
* **p2: Planning 対話** — 2 枚に分割:
  * **p2.1 契約(済)** — pane なしで契約を確立: `plan prompt`(Intent+現グラフ+スキーマ)→ エージェントが `proposal.json` を書く → `plan diff`(検証)→ `plan apply`(承認で additive 反映、ref→id 配線)。実装は `src/plan.rs`
  * **p2.2 pane 自動化(済)** — mux 層(§8: tmux/herdr backend + auto 選択、`src/mux.rs`)を導入し、`meguri plan run` が pane にエージェントを起動 → 猶予後にプロンプト注入 → `proposal.json` の出現検知 → diff → 反映まで一気通貫。config に `agent`。proposal は Intent 別パス(`proposals/i<N>.json`)

### 完了条件

「曖昧な Intent を入力し、AI と対話しながら、納得できる Outcome Graph を作れる」。

---

## v0.2 — Local Execution

Outcome Graph を実際の AI coding に接続する。

* [x] `ready` 導出からの Work 選択(o14: `meguri run <o>`)
* [x] HerdrRuntime(interface 化は 2 実装目まで待つ) — mux 層(tmux/herdr、`src/mux.rs`)
* [x] workspace / worktree 作成(repo bare clone + 隔離 worktree、exclude — o13/o14、§9.3。preflight は未)
* [x] Claude Code / Codex 起動(o15: worktree の pane に config の agent を起動)
* [x] Work instruction injection(完了コントラクト込み)(o15: `src/exec.rs` の impl_prompt を注入、state=running)
* [ ] result 待機(耐久シグナルのみ)(o16: `.meguri/result.json` のポーリング)
* [ ] meguri 側の独立検証 + fix turn 差し戻し(上限付き)
* [ ] 沈黙 nudge / タイムアウト / pane 死亡の失敗経路
* [ ] Artifact registration
* [ ] `meguri accept` / `meguri rework`(ローカル Human Gate)

### 完了条件

「ready な Outcome に Work を起こすと、AI が独立 workspace で実装し、検証済みの git change が Artifact として登録され、ローカル accept で Outcome が satisfied になり次の Outcome が ready になる」。

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
Outcome Graph
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
Outcome Graph
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

3. pane で Claude / Codex と対話する(合意は proposal.json へ)

4. Desired State(= トップレベルの Outcome 群)を決める

5. Outcome Graph を生成する

6. Graph を Human が approve する

7. 依存の無い Outcome が ready(導出)になる

8. meguri が herdr workspace / pane を作る

9. その Outcome を満たす Work を起こし、Agent に渡す

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

17. Work を done、Outcome を satisfied にする

18. 依存していた次の Outcome が ready(導出)になり、
    次の Work execution を開始する
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

* Intent から納得できる Outcome Graph まで作れるか
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
Outcome Graph
  ↓
AI Execution
  ↓
Human Judgment
  ↓
Desired State
```

のサイクル全体について、人間が **作業のオペレーションではなく Intent と Judgment に集中できているか**を評価する。

---

# 22. meguri の定義

> **meguri is a delivery control plane that turns intent into an executable outcome graph and coordinates humans and AI agents toward the desired state.**

日本語では、

> **Intent を実行可能な Outcome Graph に変換し、人間と AI Agent による実行・判断を通じて Desired State への到達を管理する Delivery Control Plane。**

とする。

定義の核は「Intent → Outcome Graph → 実行・判断 → Desired State」のサイクルの管理であって、その実装がローカルかリモートかではない。初期は local-first で始めるが(§1)、それは現時点の重心であり、この定義には含めない。

---

# 23. 設計上の未決事項(open design questions)

以下のうち Q1 は**確定済み**(本文 §4〜§14 に反映済み)。Q2/Q3 は Q1 の上に乗る形で方針まで固まっており、スキーマ詳細は v0.1 p1 実装時に確定する。扉を閉じないため経緯を記録しておく。

## Q1. グラフの土台 — Work DAG か Outcome graph か 【決定: B(Outcome graph)/ 2026-08-16】

**決定**: ノード = Outcome(到達したい状態)、辺 = requires/enables、**Work は Outcome を「未充足→充足」に反転させる手段**。本文 §4(Domain Model)/ §5(Outcome Graph)/ §12(reconciler)/ §14(再計画)に反映済み。

決め手だった B の利点: (1) 別々だった「Desired State」「Acceptance Criteria」「グラフ」が 1 構造に畳まれる(ノード = 状態、充足述語 = 旧 Acceptance、Work = 手段)。(2) §12 の reconciler が K8s controller と同型になる。(3) §14 の再計画が「どの Outcome が今満たされているか」の再評価になり明快。(4) **Work でない到達点**(マイルストーン・不変条件・能力)が特別扱いなしにノードとして載る。(5) OR(複数手段のどれかで達成)を表現しうる。

制約(確定の条件): **機構は dumb に保つ** — 1 Outcome 1 アプローチ、OR 自動探索なし、分解は pane 対話での提案 → 人間承認、自動 planner は作らない(§18 Non-Goal「複雑な AI scheduler」を守る)。B を選んでも day-1 の挙動は Work DAG とほぼ同じで、違いはスキーマ(ノードと Work を分ける)に集約される。

**ノード名を「Outcome」に確定**(検討中は Goal と仮称、2026-08-16)。Outcome ノードは骨格が旧 meguri の Issue reconciler(statement + acceptance + 依存 + reconcile(id))と同型になった — これは level-triggered な「望む変化の単位」への収束で、良い兆候。ただし旧 Issue が溶かしていた「durable な目的」と「使い捨ての手段」を分離した点が改善であり、**名前はこの分離を保つ**必要がある。ゆえに:
- **Work にはしない**(「Work」は手段側の呼称。ノードを Work にすると end/means がまた 1 語に溶ける)。
- **Issue にはしない**(内部ノードを Issue と呼ぶと GitHub の source-of-truth identity を核に引き込む = §3.2 違反。旧 meguri は issue 番号 identity で事故った)。ノードは Issue に**投影される**が Issue **ではない**。
- **Outcome** を採る: 「outcome ⇄ output」の対比が Work(= output を生む)と綺麗に対になり、end/means の分離が名前に埋め込まれる。粒度にも中立。

先行研究: GORE(KAOS / i*)、Impact Mapping、OKR ツリー、HTN / AND-OR グラフ、K8s desired-state。

## Q2. 人間が実行する Work(executor)

Work は Outcome を満たす手段(§4)で、実行系(§9)は AI 前提になっている。しかし本番 DNS 切替・デザイン決定・API キー発行など、**AI に委譲できない手段**が存在する。§4 に `executor` フィールドは記載済み(既定 `ai`)で、ここは実行フローの詳細が未確定。

- **案**: 実装フェーズに `executor`(既定 `ai` | `human`)を持たせる。`human` のとき §9 の実行フローは「人間に提示 → 完了報告(`meguri accept` 相当)を待つ」だけに退化する。DAG・ready 導出・critical path は executor を問わず同じに効き、**人間 Work → AI Work の依存が1本の graph に乗る**。
- **効用**: §21 の「今どこで詰まっているか」が人間タスクまで含めて説明できる(meguri の価値提案の中核)。
- **defer**: `external` actor(CI / デプロイ / 承認 bot)への一般化は v0.x では過剰。

## Q3. multi-actor ライフサイクル(phase ごとの actor)

「AI が実装し、人間がレビューする」は Work 単位の actor ではなく **phase 単位の actor** の話であり、**既に §6 のライフサイクルに埋まっている**:

```
running        実装      → 既定 AI(Q2 で human もありうる)
verifying      独立検証   → meguri 自身
awaiting_human レビュー   → 人間
accepted       採用判断   → 人間
```

つまり「AI 実装 + 人間レビュー」は **meguri の既定フローそのもの**で、追加構造を要しない。§13 Human Gate がこのレビュー phase。

- **制約(失敗カタログ由来)**: レビューを独立した graph ノードに割らない。レビューは bounded outcome ではなく phase。旧 meguri はレビューを分離可能な escalation 単位として扱い、**ping-pong 型 escalation が throughput の主因**になった(archive の review-convergence 診断)。旧実装の最終形も「レビューは reconciler 内の role/step(pr-reviewer)、別 issue ではない」。
- **defer**: レビュー phase の actor 可変化(`human` | `ai-reviewer`、異種モデル相互レビュー = 旧 6-role routing)は後回し。v0.x は人間レビュー固定(§13)。

Q2/Q3 は確定した Q1(Outcome graph)の上に乗る = 「Outcome を満たす手段(Work)の内訳」として phase/executor を持つ。方針は固まっており、残るは v0.1 p1 でのスキーマと実行フローの詳細確定。

## Q4. 達成の確かめ方と依存の解禁 【一部決定 / 一部将来レバー・2026-08-16】

**決定(本文 §4/§5/§9.1 に反映済み)**:
- **verify(検証)と satisfied(達成)を分ける**。verify = 作業ブランチが狙い通りか(Work 完了時に評価)、satisfied = 実際に達成(導出値)。
- **verify は v0.1 で 3 種類**: `command`(コマンド exit 0)/ `human`(人が表明・sticky)/ 省略=まとめ節点(子が全部 satisfied)。既定 `human`。
- **satisfied の定義**: command → 担当 Work が verify 通過 + マージ済み(コミット変化で自動再評価)/ human → 人の表明が有効な間 / まとめ → requires 充足。
- **依存の解禁は satisfied 基準【案A】**(下流は上流マージ後に着手)。

**将来レバー(v0.1 では作らないが塞がない)**:
- **案B(stacked 実行)**: 上流が verify を通った時点で下流を解禁(上流ブランチ基点で積む)。鎖状依存の直列化を緩めるが、上流がレビューで覆ると下流は作り直し。旧 meguri は「土台が動く」痛みを実測済み。throughput が実測で問題化したら投入。実装は worktree を上流ブランチ基点にする仕組みが v0.2 に増える。
- **ci(GitHub Actions)= 4 種目の verify**: adapter が供給する Actor(§23 Q6)。`command` と同じ SHA 固定なので扱いやすく、**PR が要る = p3(GitHub)で入れる**のが自然(v0.4 でなく p3 に寄せる)。
- **runtime 観測**: 「本番で実際に動いている(URL 応答等)」。コードが変わらなくても状態が変わる(time-varying)ので、コミットキャッシュでなく別の観測サイクルが要る。ci より後。**delivery を名乗る以上ロードマップに残す**。

**より深い律速の所在**: 進みの本当のボトルネックは依存の張り方ではなく**人間レビューのゲート**。案B は実装の並行化にしか効かない。レビューの捌き方(pr-reviewer の役割・自動化の範囲)は v0.4 以降の独立論点(Q3 の defer とも接続、旧 review-convergence 診断を参照)。

## Q5. エージェント統合 = ACP か 契約ベースか 【決定: 契約ベース / 2026-08-16】

**決定(本文 §7/§8 に反映済み)**: planning の対話も、ACP ではなく **pane + 構造化ファイル契約**で実現する。エージェント境界は「文脈を送る → 耐久チャネル(ファイル/stdout-JSON)で構造化結果を受け取る・画面は読まない」の 1 抽象に統一し、planning(`proposal.json`)と execution(`result.json`)で共有する。

**p0 の実測(`archive/p0-acp-spike` ブランチ、PR #282 は不採用で close)**: ACP は JSON-RPC over stdio で動く。Claude(adapter 経由)・Gemini(ネイティブ)で往復成立。ただし未成熟 —— アダプタは 0.x、プロトコルは v1→v2 の churn、Codex はアダプタ埋め込み core がモデルに追随できず生成不可、Cursor は第三者アダプタが返答を流さない。ベンダー純正 ACP は Gemini のみで、Claude/Codex は Zed のアダプタ頼み。

**契約ベースを採る理由**:
1. v1/v2 で証明済みの型(生きた pane + 完了契約 + 画面を読まない)の再利用。
2. **エージェント境界が 1 つに統一**(ACP を planning・pane を execution にすると 2 つになる)。表面積が半分。
3. 一級・最新・全エージェント対応(素の headless なら Codex の luna も Cursor も動く。アダプタ版 churn を回避)。
4. planning に必要なのは「多ターン + 構造化提案の収穫」だけで、ACP の richness(meguri がチャット UI をホスト / 人間なし自律ターン)は今は不要。

**ACP を再検討するトリガー**: meguri が自前のチャット UI をホストする / 人間なしで自律 planning を回す / ACP がベンダー純正・1.0 まで成熟する。その時に `archive/p0-acp-spike` ブランチから再開する。

**手段の段階**(§7): 第一は **pane(B)**(人間が生の pane で対話 → proposal.json 収穫)。人間が pane に attach せず meguri の CLI で完結させたくなったら **headless 仲介(C)**(`claude -p --resume` / `codex exec resume` を叩き proposal.json を受ける)を足す。B/C は同じファイル契約を共有するので B→C は追加であって作り直しではない。

## Q6. Actor モデル(北極星・2026-08-16)

**これは決定ではなく target model(北極星)**。実装は増分で結晶させる(下の timing)。executor(Q2/Q3)と verify(Q4)に散っていた「誰が」を、**1 つの Actor 概念**に畳む整理。

**Actor** = execute(変更を作る)/ verify(状態を確かめる)を行う主体。core が供給するもの(human)と、**adapter が供給するもの**(GitHub adapter → CI Actor + merge シグナル)がある。

- **executor も verify も同じ Actor 台帳**を引く。「誰が実装するか」「誰が確かめるか」が 1 つの語彙。
- **rollup だけは Actor ではない**(誰も確かめない・meguri がグラフから導く**構造**)。よって verify = **structural(rollup)** か **actor-attested(human / computer / ci / 将来 ai-reviewer)**。
- 能力(capability)は Actor ごと。全 AI を交換可能と仮定しない(p0 で Cursor adapter は返答すら返さなかった)。

| Actor | execute | verify | 供給元 |
|---|---|---|---|
| human | ✓ | ✓(判断) | core |
| ai(Claude/Codex…) | ✓ | ✓(ai-reviewer) | pane+契約(§8) |
| computer(ローカル) | △(機械的 op) | ✓(command) | core/ローカル |
| ci(GitHub Actions) | ✗ | ✓(pipeline) | GitHub adapter |

### Actor × Runtime(具体エージェントの表し方)

「AI」は粗すぎる。単位は具体エージェントだが、それは **2 軸の掛け算**で表す:

- **Actor(何者か)**: identity + capability + profile(モデル/速度/コスト)。
- **Runtime / Transport(どう届くか、§8)**: `Herdr`/`Tmux`(ローカル pane + ファイル契約)/ `Remote`(ブラウザ自動化 or cloud API)。

例: `Claude Code (CLI)` と `Claude Code Web` は **Actor はほぼ同系・Runtime だけ違う**(ローカル pane vs Web)。`Codex CLI`=ローカル、`Cursor Agent (Web)`=Remote。**耐久チャネルは Runtime で変わる**(ローカル=ファイル契約 / cloud=多くは PR を作る → GitHub adapter で観測)が、§8 の契約の形(文脈を送る→耐久チャネルで構造化結果)は不変。

### verify アクションと adapter

adapter は **Actor を供給し、Actor が verify/execute する**。verify の名前(例 `pass_ci`)は「その Actor に投げるチェックの種類」。CI は **最初の adapter 供給 Actor** で、`command` と同じく **SHA 固定**(コミットに対して走る)。だから time-varying な runtime 観測(本番で動いているか)より**先に入れやすい** → **ci は p3、runtime はその後**。

### timing(規律・§20 を守る)

- v0.x は **local-CLI(Herdr)Actor + verify 3 種(command/human/rollup)** のみ実装。
- 抽象(Actor trait / adapter-actor レジストリ / 複数 Runtime)は **2 つ目の実例が実際に現れた時**に結晶: **ci = 2 つ目の check Actor(p3)** / **Remote Runtime(Web エージェント)= その後**。今作ると premature(§20)。
- **単一の code trait を急がない**: Actor ごとに呼び方・観測が全然違う(shell / GitHub API / 人に聞く / pane 契約)。Actor は概念の芯として持ち、配管は Actor 別でよい。
- v1 前例: 旧 meguri は既に「agent = profile(モデル/ハーネス)+ launch」で異種モデルを扱っていた。Actor × Runtime はその一般化(発明ではない)。
