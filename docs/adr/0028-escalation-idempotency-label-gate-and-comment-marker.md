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

**claim 解放とラベル付けの順序**。成功経路では **needs-human を先に付けてから
`remove_pr_label(working)`** を best-effort で外す。逆（working を先に外す）にすると、外してから
needs-human を付けるまでの間 PR が「未 claim かつ未 escalate」に見え、別ループがそれを掴んで
新しい run を走らせた直後に元の escalation が needs-human を付け、**動いている run の PR を
人間待ちで止める**競合が起きる。needs-human を先に付ければこの窓は生じない（間に読んだ
ループは needs-human を見て掴まない）。

なお「動いている run のいる PR を escalate しない」第一の保証は、reconciler の `issue_busy`
（run-liveness）ゲート — `next_step` は live run のいる issue を Skip する（`issue_reconciler.rs`）。
上の順序はそれを狭めないための二次的な担保である。

失敗経路、すなわち `add_pr_label(needs-human)` が失敗したときだけ claim を best-effort で外す。
この経路では needs-human が付かないので上の競合は起きず、外す狙いは別にある: `pr_is_touchable`
（`src/engine/mod.rs`）は `working` を `needs-human` より先に見て「claim 済み」で skip するため、
`working` を残すと discovery ベースの再試行が塞がれる。claim を落としておけば次 sweep /
discovery がクリーンに再エスカレーションできる。`remove_pr_label` 自身が失敗して `working` が
残る二重失敗は、run-liveness ベースの stale-claim 回収（ADR 0027）が担う（escalate する run は
terminal へ向かうので claim は stale）。

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

**全ページを読み、読み切れなければ defer する**。dedup の読みは全コメントを対象にする必要が
ある — 100 件超の PR で古いページに自著マーカーがあるのに部分結果だけで判定すると、マーカーを
見落として重複投稿する。既存の全ページ GraphQL helper（`paginate_pr_comments`、
`viewerDidAuthor` を各ページで埋め、`MAX_COMMENT_PAGES` 到達や非前進カーソルで
「読み切っていない」を返す）を使い、**読み切れていない（`complete=false`）ときは判定せず
`Deferred`** にする。これは reconciler が `!comments_complete` の PR を `human_stop`（park）に
倒すのと同じ方針（過度にチャットの多い PR は API コストの上限で読みを止め、park する）。
読み（get_pr / 全ページ読み）と label 書き込みは、read-after-write と同じく、いずれかが
失敗・不完全なら escalation 全体を defer し、ラベルとコメントを常に結合させる。

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
コメントだけ**を信頼する。偽造コメントでは escalation は抑止されない。`viewerDidAuthor` は
上記の全ページ GraphQL helper が各コメントに埋める（単発の `gh pr view --json comments` は
これを返さないため、そちらは使わない）。

read（`get_pr` / 全ページコメント読み）が forge エラーまたは不完全で読めないときは、書き込みと
同様に **escalation 全体を defer**（return）する。読み → ラベル → コメントの順にして、mutate
する前にしか defer しないので、defer はいつでもクリーンに再試行できる。ラベルとコメントは
常に結合する。

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
  `escalation.deferred` を emit する。ログの WARN 止まりにしない。caller 固有イベント
  （`reconciler.budget_exhausted` 等）と通知は、実際にコメントが載った `Posted` の時だけ
  emit する（no-op 発火を成功として記録しない）。

## Limitations（原子性）

live read（marker の有無）→ comment 投稿は**原子的でない**。2 つの sweep が同時に
「marker 無し」を読めば、両方が投稿しうる。したがって「head×reason ごとに高々 1 件」は
**単一 instance の直列化された escalation を前提**にした保証である。この前提は既存構造で満たされる:

- reconciler の sweep は単一ループで PR を 1 つずつ処理する。
- fixer 家族のターン escalation は DB の家族横断 active-run インデックス（ADR 0027）で
  PR 単位に排他される。
- そもそも既存の arm マーカー dedup（`head_already_armed` → `arm()` の comment）も
  read-then-write で非原子であり、同じ単一ループ前提に乗っている。本 ADR は新しい弱点を
  持ち込まず、既存の前提を踏襲するだけである。

クロス instance（Phase-4 マルチホスト）の同時 escalation は二重投稿しうるが、冪等な
`add_pr_label` により両者とも `needs-human` へ収束するため害は bounded。forge 側に comment の
CAS 相当が無い以上、真の原子性はこの層では作らない。マルチホストで厳密な単一化が必要になれば、
ADR 0027 の instance 排他をエスカレーションにも広げる別 ADR で対処する。
