# 0028. エスカレーションは冪等である — label の read-after-write と comment marker

- Status: Accepted
- Date: 2026-07-21
- 関連: issue #246 / `docs/design/needs-human-friction-and-delivery-speed.md` §3-C・§P2 /
  ADR 0012（集約エスカレーション）/ ADR 0003・0009（arm marker のイディオム）

## 背景

meguri のループは level-triggered である（ADR 0012）。「症状が消えるまで毎 sweep 再評価
する」設計なので、エスカレーション（`meguri:needs-human` を付けて人間にボールを渡す操作）は
**同じ症状に対して何度でも再発火しうる**。再発火をどう抑えるかが本 ADR の主題である。

現状の抑止は「ラベルが付いたはず」という仮定に預けられていた。`escalate_pr` /
`spec_fixer::escalate_budget_exhausted` などは

```rust
let _ = forge.add_pr_label(pr, LABEL_NEEDS_HUMAN).await;  // 結果を捨てる
let _ = forge.pr_comment(pr, comment).await;              // 仮定の上でコメント
```

と書き込み結果を捨て、次 sweep の touchability 判定（needs-human が付いていれば skip）が
抑止してくれることに依存していた。しかしこの仮定は2通りで崩れる:

1. **書き込み失敗**: `add_pr_label` が失敗しても後続のコメントは出る。ラベルが付いて
   いないので次 sweep は同じ PR を再発見し、また escalate する。
2. **stale 読み**: ラベル書き込みは成功したが、次 sweep が読む informer キャッシュ
   （`deps.open_prs`）がまだ古く、ラベル無しに見える。同じく再発見・再 escalate。

実害: PR #231 に同一文面の needs-human コメントが 00:50 / 01:37 / 03:06 と3回付いた。
イベント上ラベルを外した actor はいない — 「書いたつもりが書けていなかった」が最有力。

## 決定

エスカレーションを**2重の冪等機構**で守る。どちらも既存イディオムの再利用である。

### 1. label の read-after-write が comment を gate する

`add_pr_label(needs-human)` の結果を捨てない。**失敗したらコメントも出さず、その sweep の
エスカレーションを丸ごと諦める**（イベントも通知も出さない）。ラベルこそが level-triggered
ループの抑止装置なので、抑止装置が立っていない以上、次 sweep に委ねるのが正しい。

- 失敗 → 何も残らない → 次 sweep が再試行する。いつかラベルが立てば、そこで初めて
  コメントが1件出る。「1回失敗して次に成功」でコメントは高々1件。
- これは `gitops.rs` の「成功申告は検証してから信じる」と同じ精神を、ラベル書き込みに
  適用したものである。

### 2. comment を head-keyed marker として dedup する

ラベル書き込みが成功しても stale 読みで再発火しうる。そこで、コメント投稿の前に
**現 head / 同一 reason の既存エスカレーションコメントがあれば skip する**。判定は
informer キャッシュではなく forge の live read（`pr_comments`）で行う — これがキャッシュ
遅延に勝つ肝である。

marker は arm marker（ADR 0003 の `<!-- meguri:automerge armed head=… -->`）と同型の、
コメント本文末尾に埋める hidden HTML comment とする:

```
<!-- meguri:escalated head=<sha> reason=<key> -->
```

- **head-keyed**: 新しい push で head が動けば marker も変わり、再評価される（新しい head は
  新しい問題かもしれない — level-triggered の意図に合う）。
- **reason-keyed**: 同じ head でも別種の理由（budget 枯渇 / stuck / review 失敗）は
  別々に1回ずつ出せる。issue #246 の「同一 reason」要件そのもの。

### 適用範囲と非適用

- PR エスカレーションの唯一の choke point である `escalation::escalate_pr` に両機構を実装し、
  そこを通す全 caller（ci-fixer / fixer / conflict-resolver / spec-worker / pr-reviewer /
  reconciler の budget・stuck・review-failed）が恩恵を受ける。
- `spec_fixer::escalate_budget_exhausted` は `escalate_pr` を通らず label+comment を手書き
  していた（issue が名指しした故障箇所）。同じ write primitive を共有して両機構を得る。
- head を安価に持たない turn 完了時の escalate 経路（flavor の `escalate`）は read-after-write
  のみ適用する。これらは「ターンが1回失敗したら1回」発火するもので level-triggered な
  再ポーリングではないため、ラベルの read-after-write だけで二重発火は防げる。
- issue-native の `escalate_issue` にも read-after-write を適用する。head を持たないため
  comment marker dedup は対象外（本 ADR は PR エスカレーションの再発を扱う）。

## 検討した代替案

- **エスカレーション済みを DB に持つ**: `escalated(pr, head, reason)` を events から集計して
  抑止する案。forge をソースオブトゥルースにする既存方針（ラベル・コメントが状態）と二重帳簿に
  なり、DB と forge の齟齬という新しい stale を生む。arm marker と同じく「コメントを marker に
  する」方が系全体で一貫する。不採用。
- **ラベルの有無だけを live read して判定**: dedup を「needs-human ラベルが既にあるか」で
  行う案。ラベルは「ボールの所在」1軸しか表さず、reason を区別できない（別種の理由を
  1回ずつ出せない）。また head が動いても同じラベルで抑止され続け、再 push 後の再評価を
  潰す。marker の方が表現力で優る。不採用。
- **書き込みをリトライする**: 失敗時にその場で数回リトライ。一時故障には効くが、恒久故障で
  sweep をブロックする。level-triggered なので「諦めて次 sweep」が自然でシンプル。不採用。

## 帰結

- エスカレーションの発火回数（`escalation.raised` 等）が「人間を実際に呼んだ回数」と一致し、
  ADR 0026 の human-roundtrips 指標が信頼できる数になる。
- 新しい不変条件を1つ持つ:「同一 head / 同一 reason の PR エスカレーションコメントは高々1件」。
  回帰は FakeForge のラベル書き込みを故意に失敗させる単体テストで検出する。
- forge write の順序が「label → (dedup 判定) → working 解除 → comment」に固定される。
  label が最初に立つので、部分失敗しても「ラベルだけ立ってコメント無し」に倒れる（安全側）。
