# ADR 0010: spec の深さは issue の性質(不確実性 × 影響範囲)で適応させる — 新サブシステムは作らず planner プロンプトの拡張で実現する

- Status: proposed
- Date: 2026-07-13
- Issue: #133

## コンテキスト

現状の planner は全 issue に同じ深さの spec(受け入れ条件・触るファイル・決定事項)を書く。だが issue には二種類ある:

- **期待動作を明文化すれば実装に入れるもの** — 局所的で、間違えても被害が小さい。
- **技術的選択・移行・後方互換の整理が要るもの** — 永続状態や公開 contract に触れ、間違えたときの被害が広い。

後者に前者と同じ軽い spec を書くと、設計判断が spec review をすり抜ける(レビュアは「明文化された期待動作」だけを見て承認し、選ばれなかった代替案や移行の穴を問わない)。逆にすべてに重い spec を強制すると、小さな issue が過剰に重くなり、spec 先行フローの安さ(ADR 0001:「planner の spec は下流を steer するが安い」)が崩れる。深さと issue の性質がねじれている。

これは AI-DLC 的な「Adaptive Elaboration」(問題の複雑度に応じて仕様化・設計・分解ステップを適応させる)の検討から抽出した論点である。検討の中で、複雑度を数値化して契約にする方向(6 軸スコア出力・YAML ルールエンジン・`ElaborationPlan` 型・`meguri:elaborate` 専用 phase)も俎上に載ったが、いずれも棄却された(下記「棄却した代替案」)。

## 決定

**spec の深さを、実装工数ではなく「不確実性 × 影響範囲」で決める。** フローは既存の `meguri:plan` → `meguri:speccing` → spec review → 実装のまま。変わるのは planner が書く spec の必須セクションだけであり、新しいサブシステム・phase・ラベル・承認機構は一切作らない。

### 1. 深さは 2 段。normal と design(design 相当)

- **normal spec**(現状どおり): 受け入れ条件・触るファイル・key decision。
- **design spec**(深い): 上に加えて必須セクションを持つ — architecture impact / 代替案と決定 / migration・rollback(永続状態に影響がある場合)/ observability / test strategy。

多段化はしない。2 段で始め、必要が実証されてから増やす。

### 2. 判定軸は「不確実性 × 影響範囲」— 工数ではない

LLM は工数見積もりが苦手だが、「何が未確定か」「間違えたときの被害範囲」の列挙は得意である。だから深さの判定を、planner が **どうせ spec を書くために行う repo 調査の in-context 判断** に委ねる。planner のプロンプトに、この 2 軸で列挙して深さを選ぶよう明記する。

### 3. veto 軸 — 単一の総合判断に畳まない

総合判断が「軽くてよい」に傾いても、以下のいずれかを検出したら該当セクション(migration / rollback)を **無条件で必須** にする:

- 永続状態(state)・schema・公開 contract への影響
- 不可逆な運用リスク

veto は総合スコアの一項ではなく **ハードなフロア** である。状態の安全性は、他の評価がどうであれ優先される。

### 4. 実現は planner プロンプトの拡張。新サブシステムは作らない

深さの決定ロジックはコード側に持たない。planner の execute プロンプトに「2 段の定義・判定軸・veto ルール・design spec の必須セクション・深さ判断の理由を 1〜2 文残すこと」を書き込むだけにする。これは既存の型(triage=判定、planner=elaboration、spec PR=承認ゲート)の拡張であって、新しい型ではない。

### 5. depth ヒントの尊重(前方互換)

`spec_depth: normal | design` というヒントを planner が尊重する。供給源は:

- 人間が issue 本文に明示指定
- 将来 triage v1(#87)が判定して付ける提案コメント / hidden marker

いまは triage が存在しないので、ヒントは「issue 本文に人間が書いた `spec_depth:` 行」に限られ、その行はすでにプロンプトへ本文ごと差し込まれる。よって planner エージェントはヒントを読める。triage v1 が着地したら、triage コメント / marker をプロンプトに渡す小さな追補で自動供給に接続する(本 ADR のスコープ外の follow-up)。

**ヒントは深さを上げられるが、veto フロアより下げられない。** 人間が `design` を指定すれば深い spec を強制できる。しかし `normal` を指定しても、veto(state/contract 影響)が deep を要求するならそちらが勝つ。

### 6. design spec も使い捨て(ADR 0001 は不変)

深い spec も disposable scaffolding のままである。実装時に、architecture impact / 代替案と決定は ADR へ、長期的なドメイン規則はドメイン文書へ振り分けられ、残りは実装コードに蒸留され、spec 自体は spec-worker が削除する。**design spec は永続の設計文書ではない** — レビューを深く収束させるための、より重い足場にすぎない。

### 7. 承認は既存の spec PR レビューが担う

深い spec も、承認は既存の spec PR レビュー(reviewer ループ、または人間)が行う。新しい approval 機構・ラベル・phase は作らない(ADR 0005:「phase は増やさない」)。深さの判断理由は spec 本文(または PR 説明)に 1〜2 文残し、レビュアがその判断自体を吟味できるようにする。

## 棄却した代替案

- **6 軸数値スコアの出力契約化** — LLM の弱点(数値見積もり)に依存し、契約化するとブレを機械が信じてしまう。「何が未確定か」の列挙(得意分野)に寄せる方が頑健。
- **YAML ルールエンジン / `ElaborationPlan` 型** — 深さの決定をコードに持たせると、新しいサブシステムと契約・テスト・保守が生える。planner はどうせ repo を調査するので、in-context 判断で足りる。
- **`meguri:elaborate` 専用 phase** — ADR 0005 の 2 軸ラベルモデルは「phase を増やさない」ことを不変条件にしている。深さは spec の中身の違いであって、フローの状態が増えるわけではない。
- **深さの多段化** — 2 段で始める。3 段以上は必要が実証されてから。

## 帰結

- 永続状態や公開 contract に触れる issue の spec PR には、migration / rollback を含む深いセクション構成が現れ、設計判断が review の俎上に載る。
- 局所的で明確な issue の spec は現状どおり軽いまま(回帰なし)。planner の安さは小さな issue では維持される。
- 深さの判断は planner の in-context 判断に閉じるため、コードの変更は planner プロンプト(と、その内容を保証するテスト)に限られる。オーケストレータの状態機械・ラベル・phase は無変更。
- 深さの根拠が spec 本文 / PR に残るので、深さの判定自体をレビューできる。誤って軽くした場合は review が是正経路になる。
- #134(decompose)と本 ADR は「深い spec」と「分解提案」が planner の出力型のバリエーションであるという同じ整理に立つ。将来これらを統一的に扱う余地を残す。
