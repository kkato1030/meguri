# ADR 0022: self-review に findings 台帳と kind(defect/decision)を導入し、escalation を「回数」から「挙動」へ変える

- Status: proposed
- Date: 2026-07-15
- Issue: #212(親: #211)
- 関連: ADR 0006(AI 実装レビューの内部ループ化)・ADR 0008(spec/impl 対称化・多レンズ review)・ADR 0011(combined も self-review を通す)・ADR 0012(escalation 集約・2層モデル)・ADR 0021(escalate 時 needs-human draft)

## Context

self-review の cap 落ち(フェーズの約3割が needs-human で終わる)を調べると、主因は
仕組みの側にあった。reviewer が**毎ラウンド全 diff をゼロから再レビューする**構造なので、
前回直した箇所とは別の新規指摘が毎回湧く。収束するかどうかが「指摘が尽きるか」という
運任せになり、「何をもって収束とするか」という意味論が無い。

さらに ADR 0012(#176)は cap 到達を「footer 付きで公開」から「escalate」へ変えた。
これは未収束 diff を無防備に公開する穴を塞いだ正しい判断だが、**残りが軽微な blocking
だけでも人間を呼ぶ**。実測では cap 落ち26件のうち10件前後が「あと1件、機械的に直せる
指摘が残っただけ」で escalate していた。人間ゲートの価値が薄まる。

「収束の意味論」を作り、escalation を回数ではなく**中身の挙動**で決め直す。

## Decision

### 1. finding に kind と id を持たせ、severity は導入しない

`Finding` に `kind: defect | decision` と安定した `id` を足す。

- **severity は作らない。** findings は定義上すべて blocking である。非ブロッキングな
  所感は従来どおり `review` 散文に流す(reviewer が既に守れている挙動の公式化)。
  この不変条件を `fixable ⇔ findings 非空` の**双方向**強制で守る:
  `clean`/`needs_human` は findings 空、`fixable` は findings 非空。
- **`decision` 型** = 「A か B か決めて spec に明記せよ」型の指摘(実 findings の15〜20%)。
  defect(コードのバグ・欠落)とは扱いが違う。decision は fix turn で**決定して記録する**
  ものであり、再審の対象にしない(後述 §3)。

### 2. 台帳(ledger)— 最新ラウンドの上書きをやめ、finding 単位で状態を積む

checkpoint の `self_review_pending`(最新ラウンドで丸ごと上書き)を、finding 単位の
**累積台帳**に置き換える。各エントリは status(`open` / `fixed` / `waived`)と
「これまで何回 fix turn を経たか」を持つ。

- fix turn の作者は finding ごとに「**直した(fixed)**」か「**同意しない(waived・理由必須)**」を
  申告する。decision 型は「決定して spec に記録した」を fixed として申告し、決定内容を台帳に残す。
- 台帳は checkpoint に永続する。crash resume 後も status・fix 回数・waive 理由・決定内容が
  維持され、レビューがゼロからやり直しにならない。

### 3. round 2+ の役割を「フル再レビュー」から「解消確認 + 新規のみ」へ変える

round 2 以降の review turn には、前ラウンドまでの台帳(waive 理由・decision の決定内容込み)と、
base との全 diff に加えて**前回レビュー時 HEAD からの増分 diff** を渡す。役割を変える:

- **前回指摘の解消確認**(直ったか)+ **blocking 級の新規のみ追加可**。「気になった所を
  ゼロから並べ直す」ことは禁止。これが「新規指摘が毎回湧く」構造を止める。
- **decision finding は「決定が記録されたか」だけを確認**する。記録されていれば解消。
  A/B どちらが正しかったかの**再審は禁止**。決定に異議があるなら再審ではなく escalate(§4-3)。

### 4. escalation を挙動で決める(ADR 0012 の cap 行を置き換える)

needs-human に値するのは次の3つだけとする:

1. **reviewer の `needs_human` verdict**(ADR 0012 どおり、即 escalate)。
2. **本当の ping-pong** — 同一 finding が fix を**2回**経てもなお open。作者と reviewer が
   噛み合っていない証拠なので人間が裁く。
3. **記録済み decision への異議** — reviewer が記録済みの決定を人間が覆すべきと判断した場合。
   これは reviewer の `needs_human` verdict として表明する(§3 の再審禁止の唯一の例外)。

これらに**該当せず** cap に達した場合(残っているのは軽微な blocking だけ)は、
ADR 0012 の「cap 到達=escalate」を**本 ADR で置き換える**:

- **最終 fix turn + validate を実行して publish** する(fix で終わる勘定に直す)。
- 最終 fix は再レビューしない。ただし `check_command` + tree 検証(clean・base より進んでいる)は
  通す。green 保証は残る。
- PR footer に「**最終ラウンドの fix は未再レビュー**」を1行記録し、human merge gate に委ねる。

ping-pong と decision 異議は検知した時点で即 escalate するので、cap に到達した時点で
残っているのは構造上「軽微な blocking」だけになる。cap 落ち26件中、残り1件前後で escalate
していた10件程度がこの経路で救える見込み。

### 5. planner の round 1 に「未決定を出し切る」観点を足す

cap 落ちは planner に集中(64%)しており、後半ラウンドで decision 型が新規に湧くのが主因。
planner の初回 spec 作成プロンプトに、**未決定事項(A か B か)を初回に洗い出して spec に
出し切る**観点(`decisions` 相当のレンズ)を足す。後半で decision が湧くのを前倒しで潰す。

## Consequences

- **収束が「指摘が尽きる」から「台帳の open が捌ける」に変わる。** 収束の意味論が
  運任せから構造へ移る。
- **人間ゲートが本当に人間が要る所へ絞られる。** cap 到達=一律 escalate をやめ、
  ping-pong・decision 異議・reviewer verdict の3つに集約する。ADR 0012 の
  「§5 の表: self-review / `max_rounds` 到達・未収束 → escalate」の行は本 ADR が置き換える
  (残り軽微なら最終 fix→publish)。ADR 0012 の他の行(guard・conflict_resolver・
  needs_human verdict)は不変。
- **verdict/artifact コントラクトが広がる。** 毎ターン生成される `.meguri/self-review.json` の
  finding に `kind`/`id` が増え、fix turn は新たに per-finding の申告ファイルを書く。DB スキーマの
  変更ではないが、checkpoint(run step の永続 JSON)に台帳フィールドが増える(移行は spec §
  移行/rollback を参照)。
- **「最終 fix は未再レビュー」を footer に明記する透明性コスト。** 未再レビューの1コミットが
  PR に載ることを人間に隠さない。merge gate がそれを見て判断する前提。
- **ADR 0021 の needs-human draft は残る。** ping-pong・decision 異議・reviewer verdict で
  escalate する経路では、従来どおり証拠 draft が公開される。挙動化で「救われた」ケースは
  そもそも escalate しないので draft も出ない(通常 PR として公開される)。
