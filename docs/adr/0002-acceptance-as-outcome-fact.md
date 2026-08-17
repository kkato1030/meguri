# ADR 0002: 受理は Outcome 側の耐久事実・複数 artifact / 複数リポは将来に開く

- **Status**: Accepted
- **Date**: 2026-08-17
- **関連**: ADR [0001](0001-delivery-and-projection-layers.md)、plan.md §4/§5(satisfied 導出)、architecture.md「導出のルール」、o28(棚卸し)

## Context(なぜ考えるのか)

satisfied は保存せず事実から導出する(§5)。command Outcome の唯一の根拠は当初
「accept 済みの **Work** が存在するか」だった。だが dogfood でこれが脆いと露呈した:

- **Work を掃除すると満たしが退行する**。finished Work / worktree / pane を片付けると
  `works.state='accepted'` が消え、Outcome が satisfied → ready に戻る。掃除とグラフの
  真実が結合していた。
- Outcome は **複数 Work を持てる**(リトライ・並行試行、`serves_id` に unique 無し)。
  「どの試行で満たされたか」が可変な Work 行の生存に依存するのは弱い。

さらに問い: **1 Outcome を満たすのに複数 artifact(特に複数リポ)** が要るパターンは?
現状 Intent は 1 repo に紐づき(ADR 0001)、Outcome の Work は全てその 1 repo で走り、
satisfaction = 1 accepted work。**複数リポにまたがる Outcome は表現できない**。

## Decision(何を決めたか)

### 1. 受理を「Outcome に貼る耐久事実」にする(今実装)

- 新テーブル `acceptances(id, outcome_id, work_id?, repo_id?, artifact_sha?)`。
  **satisfied の根拠はこの行**。`works.state='accepted'` は運用状態として残すが根拠ではない。
- `work_id` は情報用で **FK を張らない** → Work を掃除しても受理は残る(退行しない)。
- `derive` の `accepted` 集合は `acceptances` から引く(Work state ではなく)。
- 後方互換: 既存の `works.state='accepted'` を起動時に一度だけ `acceptances` へ backfill(冪等)。
- Outcome を消せば受理も消える(`remove_outcome`/`remove_intent` で明示削除)。

### 2. 複数 artifact / 複数リポは「席だけ用意して開く」(今は作らない、§20)

- `acceptances` は Outcome ごと **0..N 行**を許し、`repo_id` / `artifact_sha` を各行に持つ。
  → スキーマ上は **複数 artifact・複数リポの受理を表現できる**。
- ただし当面の satisfied 条件は **「受理行が 1 つ以上」**(現状踏襲)。AND 満たし
  (「必要な artifact が全部揃って初めて満たす」)や cross-repo Work・cross-Intent 辺は
  **本 ADR では決めない**。2 実例目が現れてから結晶させる。

## Consequences(結果として何が起きるか)

- **良くなること**
  - 掃除(work rm / worktree・pane 撤去)が **グラフを退行させない**。運用と真実が分離。
  - 複数 Work があっても「どの artifact で満たしたか」が Outcome 側に一意に残る。
  - `repo_id` を各受理に持つので、複数リポ対応・v0.3 の PR projection に繋げやすい。
- **割り切り / 未**
  - satisfied は当面「受理 1 つ以上」。**AND 満たし・cross-repo Outcome は未実装**(下記 open)。
  - Intent→1 repo(ADR 0001)は据え置き。1 Outcome が複数リポの Work を持つには
    Intent-repo 境界と requires のスコープ(実質 Intent 内)の再設計が要る。

## Alternatives considered(検討して採らなかった案)

- **`works.state='accepted'` のままにする** — 掃除で退行する。却下(本 ADR の動機)。
- **Outcome に単一 `accepted_artifact_sha` 列を足す** — 複数 artifact / 複数リポを塞ぐ。
  集合テーブルにして将来を開く方を採る。
- **今すぐ複数 artifact の AND 満たし・cross-repo を実装** — premature(§20)。実例が
  現れてから。スキーマの席だけ用意して判断を遅延する。

## Open questions(将来 ADR にする論点)

- 1 Outcome が **複数 artifact を AND で**要求する満たし条件の表現(必要数 / 必須集合)。
- **cross-repo Outcome**: 1 Outcome の Work が複数リポで走る。Intent→1 repo 境界
  (ADR 0001)と requires の Intent 内スコープをどう緩めるか。分解(per-repo 子 Outcome +
  rollup)で足りるか、真に 1 Outcome : N repo が要るか。
