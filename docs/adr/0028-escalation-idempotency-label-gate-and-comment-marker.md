# ADR 0028: エスカレーションの冪等性 — label 書き込みゲート + head×reason コメントマーカー

- Status: accepted
- Date: 2026-07-22
- Issue: #246（設計書 `docs/design/needs-human-friction-and-delivery-speed.md` §3-C / §P2）

## Context

meguri のエスカレーション（PR を `meguri:needs-human` に park する経路）は
**level-triggered** である。reconciler の sweep は毎 poll で全 PR を観測し直し、
「予算を使い切ってなお症状が残る」PR を毎回 `Op::Escalate` に落とす。同じことを止める
唯一の仕掛けは `meguri:needs-human` ラベルで、これが付くと次 sweep で `human_stop = true`
になり `next_step` が `Skip` する、という前提だった。

この前提が 2 通りに崩れる。

1. **書き込み失敗**。`escalate_pr` / `escalate_budget_exhausted` は
   `let _ = add_pr_label(...)` で結果を捨てていた。ラベル書き込みが失敗しても
   コメントは投稿され、次 sweep はラベル不在（`human_stop = false`）を見て再発火する。
2. **stale 読み**。ラベルは付いたが、次 sweep の bulk-observe キャッシュがまだそれを
   反映していないと、やはり `human_stop = false` と誤読して再発火する。

実際に PR #231 へ同一文面の needs-human コメントが 3 回付いた（07-21 00:50 / 01:37 /
03:06）。イベント上ラベルを外した actor は存在せず、「書いたつもりが書けていなかった」が
最有力だった。

## Decision

**エスカレーションのコメント投稿を、2 段の冪等ゲートで囲う。**

### ゲート1: label 書き込みを捨てない（read-after-write）

`add_pr_label(needs-human)` の `Result` を評価する。**失敗したらコメントも通知も出さず
そのまま return し、次 sweep に委ねる**。ラベル（＝ 耐久的な「escalated」記録）と
人間向けコメントを分離させない — ラベルが付かない限りコメントは 1 件も出ない。
これで「書けていないのにコメントだけ出る」故障モードが消える。

### ゲート2: head×reason のコメントマーカーで dedup

arm マーカー（`ARMED_MARKER_PREFIX`、ADR 0003）・claim マーカー（`CLAIM_MARKER_PREFIX`、
ADR 0027）と同じ「コメント自身をマーカーにする」イディオムを踏襲する。エスカレーション
コメントの先頭に隠しマーカーを埋める:

```
<!-- meguri:needs-human reason=<slug> head=<sha> -->
```

コメント投稿前に、対象 PR の**自分が書いた**コメント（`viewer_did_author`）を読み、
同一 head・同一 reason のマーカーが既にあれば**コメントも通知も skip** する。
これでラベルの stale 読みが起きても、コメントは head×reason ごとに高々 1 件になる。

- **head でキーする理由**: 新しい head が push されたら状況は変わりうる（同じ reason でも
  再エスカレーションが正当）。head をキーに含めることで、arm マーカーと同型に「現 head では
  一度だけ、head が動けば再評価」になる。head は **stale になりうる観測キャッシュではなく
  `get_pr` の現物**から取る（stale 読みが dedup キーを狂わせないため）。
- **reason でキーする理由**: 同一 head でも conflict 予算切れ・CI 予算切れ・stuck・
  pr-review 失敗は別の人間向け事情である。reason を含めることで、別種の escalation が
  互いを誤って抑止しない。

### 権威と担体

第三者が公開リポジトリでマーカー入りコメントを偽造して escalation を抑止する可能性がある
（ADR 0027 の no-steal と同じ injection 面）。dedup 判定は **`viewer_did_author` が真の
コメントだけ**を信頼する。偽造コメントでは escalation は抑止されない。

read（`get_pr` / `pr_comments_meta`）が forge エラーで読めないときは、書き込みと同様に
**escalation 全体を defer**（return）する。ラベルとコメントを常に結合させ、次 sweep が
クリーンに再試行できるようにする。

## Consequences

- 「PR head × reason ごとに escalation コメントは高々 1 件」という不変条件が、ローカル
  状態なしに forge の観測可能な状態（ラベル + マーカー）だけで保証される。ラベル書き込み
  失敗にも観測キャッシュの stale にも耐える。
- 全 escalation 経路が単一のガード済みプリミティブ（`escalation` モジュール）を通る。
  reconciler の予算切れ / stuck / pr-review 失敗、fixer 家族のターン escalation、
  spec_fixer の予算切れがいずれも同じゲートを共有する。
- `human_stop` ラベルゲート（`next_step`）はこれまで通り第一の抑止として残る。マーカー
  dedup はラベルが効かなかったときの二重の安全網であり、置き換えではない。
- 人間が `needs-human` を外しても head を動かさず症状が残る場合、reconciler は再評価して
  再度 escalate に到達する。同一 head×reason のマーカーが残っているためコメントと通知は
  再発火せず、ラベルだけ付け直す（＝「まだ人間が要る」を静かに再表明する）。再通知を望むなら
  新しい head を push する。これは意図した挙動である。
- 観測性: 抑止された発火は `escalation.deduped`、read/label 失敗による延期は
  `escalation.deferred` を emit する。ログの WARN 止まりにしない。
