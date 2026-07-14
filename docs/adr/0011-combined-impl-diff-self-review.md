# ADR 0011: combined モードの実装 diff にも内部 self-review を通す — 「PR を開く前に 1 回」を「公開状態を進める前に 1 回」と読み替え、spec-worker を self-review 側に立てる

- Status: proposed
- Date: 2026-07-14
- Issue: #171
- 関連: ADR 0006(AI 実装レビューの内部ループ化)・ADR 0008(spec/impl 対称化)

## Context

ADR 0008 は「必須の内部 self-review + 任意の GitHub guard」を spec と impl の両側に
立てた。だが `Flavor::self_reviews()` を true にしたのは **planner と worker だけ**で、
`spec_worker` は既定の false のまま残った。

`spec_worker` は combined モードで spec PR の branch を takeover し、実装 commit を
積む(#98 の morph 型)。ADR 0008 §2 の「self-review は PR を開く前に 1 回きり」を
文字どおり取ると、PR を新規に開かない spec_worker は self-review を素通りする。結果、
combined 経路の実装 diff は:

- 内部 self-review = なし(`self_reviews()` = false)
- guard(Impl) = 既定 OFF(ADR 0008、外部 bot 互換のため opt-in)

の二重で無防備になり、**内部レビューを一度も通らずに公開されうる**。一方、separate 経路
(worker が新 PR)も ready 直行経路も self-review を必ず通る。同じ「実装 diff の公開」なのに
combined だけ品質下限が抜けている — ADR 0008 が閉じたはずの非対称が、morph 経路にだけ
残っていた(finding)。

## Decision

**選択肢 (a) を採る: spec_worker に必須の内部 self-review を持たせる。**

`SpecWorkerFlavor::self_reviews()` を true にする。kind は既定の `Impl` のまま
(実装 diff を code レンズで見る)。

### 「PR を開く前」を「公開状態を進める前」と読み替える

ADR 0008 §2 の原則は「1 diff につき 1 回、公開の直前に内部レビューを通す」ことにある。
combined の spec PR は、spec commit だけの段階では**まだ実装として未完成**で、spec-worker が
実装 commit を積んで初めて中身が確定する。したがって「PR を開く」瞬間ではなく
「PR の中身を実装として進める」瞬間こそが、combined における "公開直前" である。
spec-worker の takeover をこの読み替えの対象に含めるのは、原則の文言ではなく趣旨に沿う。

これで内部 self-review は「実装 diff を公開する経路すべて」で必須になり、
ready(worker)・separate(worker)・combined(spec-worker)の 3 経路が対称に揃う。

### レビュー対象は「combined 差分」= ADR + 実装コード

self-review は flavor の `verify_base` ではなく常に **default branch** との差分
(`git diff main...HEAD`)を読む(`impl_reviewer.rs`)。combined branch では planner が
spec を追加し、spec-worker が実装完了時にそれを削除する。self-review が走る時点
(execute → validate の後)では spec は既に削除済みなので、レビュー対象は
**ADR + 実装コード**という、まさに公開される中身そのものになる。二重の base 指定や
特別扱いは要らない。

### なぜ (b)/(c) ではないか

- **(b) combined で guard(Impl) を既定 ON にする** — 却下。guard は外部・任意・advisory で、
  ADR 0008 が「外部 bot 互換のため既定 OFF」と定めたもの。モードで既定を裏返すと挙動が
  読みづらく、下限を forge 往復に依存させる。しかも guard は required 化しない限り止めない
  ので、内部 self-review のような**確実な下限**にならない。
- **(c) 現状を意図として明文化する** — 却下。ADR 0008 の主題そのものが「実装 diff の品質下限を
  対称化する」ことだった。combined だけ下限を抜くのは 0008 の趣旨と矛盾する。

## Consequences

- combined の実装ターンは execute → validate → **self-review** → open-pr を通る。worker と
  同じ内部 review→fix ループが spec-worker の worktree で回り、forge は一切触らない。
- combined PR 本文の `<details>` に実装 self-review のラウンド要約が載る
  (`settle_presentation` が本文を再構成する既存経路に相乗り)。planner 段階の spec
  self-review 要約は、takeover 時の本文書き換えで実装側の要約に置き換わる。
- スキーマ・マイグレーションは不要。`Checkpoint` の `self_review_*` は全 flavor 共通で
  既に存在し、`STEP_SELF_REVIEW` も全 flavor で通る。変更は override 1 つ。
- ロールバックは容易: override を戻すか、`review.enabled = false`(内部 self-review の
  kill switch)で無効化できる。永続状態や公開契約には触れない。
- guard(Impl) は従来どおり opt-in のまま。combined でも「内部は必須・外部は任意」という
  ADR 0008 の役割分担を崩さない。
