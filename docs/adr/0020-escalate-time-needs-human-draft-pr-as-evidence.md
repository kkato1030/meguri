# ADR 0020: self-review エスカレート時、commit 済みなら needs-human draft PR を証拠物件として publish する

- Status: proposed
- Date: 2026-07-15
- Issue: #209
- 関連: ADR 0006(AI 実装レビューの内部ループ化)・ADR 0008(spec/impl 対称ループ)・ADR 0012(未収束・needs_human は人間が判断)

## Context

ADR 0012 で、self-review が収束しない run(`needs_human` verdict、`max_rounds` 到達、
review/fix ターンの失敗)は publish せず `meguri:needs-human` へエスカレートすると決めた。
だがこのとき書きかけの成果物(spec / 実装 diff)は **ローカル worktree にしか無く、
forge から見えない**。人間に届くのは「収束しなかった」というコメントだけで、判断材料の
本体は pane/worktree を覗きに行かないと読めない。「エスカレート = 行き止まり」になっている。

#88(triage auto)で `meguri:plan` の自動投入が増えると、この行き止まりの発生頻度も上がる。

## Decision

### 1. エスカレート時にだけ publish する(fallback)

self-review がエスカレートする時点で、**branch が base より commit で進んでいれば、
push して `meguri:needs-human` ラベル付きの draft PR を開く**。進んでいなければ従来どおり
コメントのみ。可視性の問題だけを解き、happy path には何も持ち込まない。

- happy path は不変。ADR 0006/0008 の「self-review は forge に一切触れない」は維持され、
  forge に触れるのは **エスカレート時のみ**。CI・通知コストの増加は無い。
- draft なので GitHub 側でマージがブロックされ、人間の merge ゲート(ADR 0004)は壊れない。
- 生まれた瞬間から `meguri:needs-human` が付くので `pr_is_touchable`(`src/engine/mod.rs`)が
  fixer 系ループから除外する。未完成ブランチを別ループが claim する競合は起きず、新しい
  ガードは要らない。この「生まれた瞬間から」は文字どおりで、**ラベルは PR 作成と同一の
  forge 呼び出しで付ける**(下記 §5)。作成後に別呼び出しで貼る設計では、その隙間に
  ラベルなしの未収束 draft が観測され、競合の窓が開く。
- 人間の回復パスが既存の状態機械に乗る。中身が良ければ手で直して ready + `meguri:spec-ready`
  に倒せば spec-worker の takeover に合流でき、捨てるなら PR を閉じるだけ。

### 2. PR の意味論を二種類に明確に分ける

- 通常 PR = **検証済みの成果物**(self-review clean・commit ahead・check 通過)。
- needs-human draft = **未収束の証拠物件**。draft + `meguri:needs-human` の組で厳密に区別する。

証拠物件は **グリーン保証を持たない**。fix ターン途中でエスカレートした場合、publish される
tree は check が通っていない可能性がある。draft の本文にこの性質を明記し、通常 PR 本文の
「self-review clean」表現(`compose_pr_body`)とは別の composer で組む。

### 3. `pr.created` は通常 delivery 専用に保つ

「PR 作成 = 成功」と数えるダッシュボード/stats が濁らないよう、証拠 draft の publish は
`pr.created` を emit せず、別イベント(`self_review.escalated_draft`)で記録する。run の終了
ステータスは従来どおり Failed(needs-human)のままで、draft を開いても成功にはならない。

### 4. 却下: 常に draft PR 先行

self-review の**前に** draft PR + push まで行うモデルは採らない。

- `pr_is_touchable` は PR のラベルでしか除外せず draft かどうかは見ない。self-review 中の
  push で CI が赤くなると ci_fixer が未完成ブランチを claim し author lane と二重書き込みする。
  防ぐには `meguri:working` の付け外し運用が必須で、push とラベル付与の間のクラッシュという
  新しい整合性問題を抱える。
- ADR 0006/0008 の「self-review は forge に触れない」が happy path でも崩れる。
- run ごとの PR 作成・push ごとの CI 消費・読みかけ draft の diff 書き換え・放棄 run の掃除と、
  常時コストがかかる。

エスカレート時 publish は、これらを happy path に持ち込まずに可視性だけを解決する。

### 5. 証拠 draft は「作成と同時にラベル付き」で生まれる(不変条件)

`pr_is_touchable` が draft を判定材料にせず `meguri:needs-human` ラベルだけで除外する以上、
証拠 draft は **ラベルなしで観測されてはならない**。よって needs-human ラベルは PR 作成と
不可分にする —— 作成後の別呼び出しではなく、作成 API に載せて同一 forge 呼び出しで付ける
(`gh pr create --label`。既存の `create_issue(title, body, labels)` と同型)。

- 作成が成功した瞬間には必ずラベルが載っており、「作成後・ラベル前」というラベルなし draft の
  窓が meguri 制御下に存在しない。プロセスが作成直後に落ちても、PR は既にラベル付きで
  fixer / ci_fixer / conflict_resolver から除外されている。
- 作成が失敗すれば meguri は「delivered な draft」を記録せずコメントのみへ落ちる。
- 却下: 「作成後にラベルを貼り、失敗したら PR を閉じる」はクローズ呼び出し自体が失敗しうる
  ため窓を消せない。「draft を untouchable 条件に足す」は `pr.draft = true` 運用の通常
  draft PR まで巻き込むため過剰。作成時ラベルが最小かつ窓ゼロ。

## Consequences

- 「エスカレート = 行き止まり」が「エスカレート = 判断材料付きの分岐点」になる。
- planner(spec)と worker(実装 diff)の両方に自然に効く。self-review は flow 共通だからだ。
- push・PR 作成が失敗しても best-effort でコメントのみに落ち、run の終了挙動は変わらない。
- `Forge::create_pr` が `labels` を取る形に広がる(§5)。通常 delivery の `open_pr` は空配列を
  渡すだけで挙動不変。
- 未検証 tree が draft として forge に出うる。証拠物件でありグリーン保証はない、という
  意味論を draft 本文とこの ADR で明文化することが前提になる。
