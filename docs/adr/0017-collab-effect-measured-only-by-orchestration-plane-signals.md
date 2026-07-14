# ADR 0017: 協働の効き目は統率面の durable 信号でのみ測る(協働面は不可視だから)

- Status: accepted
- Date: 2026-07-14
- Issue: #121(collab 基盤 — 測定を第一級に)
- 関連: collab-advisor #111 / ADR 0006(協働面を「meguri は読まない」と決めた当の判断)、routing 測定 #65 / ADR 0002(sqlite 直読みの stats)、最小主義 ADR 0001(Rule of Three)

## 文脈

ADR 0006 で協働面(agmsg)を **advisory・meguri は読まない・完了契約に影響しない**と定義した。速度は安全性から来る設計だ — meguri が判断に使わないからこそ、再コンパイルも検証もなしに協働プロトコルを差し替えられる。

その代償が #121 のセルフレビューで露わになった。**協働面は不可視ゆえ、協働面からは効果を測れない。** advisor の助言が効いたかを協働面(相談ログ)から読めば、それは meguri の「durable signals only」不変条件を破る。結果、「クイックにテスト」のうち速いのは *回すこと* だけで、*評価* の手段はゼロだった。編成を切り替えても、良し悪しを比べる物差しが無い。**測定の欠落こそが真の欠落**だった。

## 決定

**協働(編成)の効き目は、統率面に既に durable に落ちている信号でのみ測る。協働面の中身は測定に一切使わない。** 具体には:

- advisor が付く run にだけ、その面 `advisor` を durable に刻む(`runs.collab_mode`)。刻むのは **意図した面**であって spawn の当たり外れではない(spawn は best-effort、ADR 0006)。付かない run は書かず NULL のままにし、集計側で `off` と読む — feature off なら書き込みが一切起きず、inert 規律(ADR 0006)を保つ。
- 面ごとに、統率面の既存 durable 信号(成功率・平均 turns・平均所要時間)を比較する(`meguri stats collab`、`stats routing` と同じ sqlite 直読み)。ただし比較は **routing を固定した上で行う** — group key に `agent_profile` と `routing_arm`(#66 で explore/escalated を本線に混ぜないために足された列)を残し、`collab_mode` はそこに1軸足すだけ。これがないと collab を有効にした時期の model/routing の変化が `advisor` 行に混ざり、物差しがずれる。それでも観察データであって無作為化実験ではないので(issue 難易度など残る交絡)、これは「routing を固定した durable 信号の比較」であって因果の証明ではない、と正直に位置づける。
- 語彙は今日実在する軸(`collab` = off/advisor)に留め、**"ensemble" は名乗らない**。編成パターンが advisor 1つだけの段階で framework 語を持ち込まない(ADR 0001 の Rule of Three、#121 §②)。

## 帰結

- 「テスト」を名乗る条件が満たされる:同じ `(役割, profile, arm)` の `off` 行と `advisor` 行を並べ、routing を固定したまま durable 信号の分布で編成を比較できる。回すだけでなく評価できて初めて "テスト"。
- 協働面の不変条件は無傷。meguri は依然 agmsg を読まない・待たない・検証しない。測定は run 行の観測用メタデータ(routing_arm と同性質)だけを読む派生ビューで、完了契約・検証・scheduler に食い込まない。
- 測定は最小に留める。v1 の信号は `runs` 単独にある run ローカルなものだけ。run をまたぐ信号(fixer ping-pong 往復数・impl-review findings 数・CI 失敗回数)は `runs` に無く、後続の拡張とする — 骨格を先に通し、信号は後から足せる形にする。
- ensemble への一般化は封印を継続。2例目の実編成(judge-panel / adversarial 等)が現れ共通形が見えるまで、測定軸は `collab` のまま据え置く。その時に別 issue で `ensemble` へ広げる。
- `[collab]` 未指定なら `collab_mode` への書き込みは一切発生せず、行データは現状とバイト単位同一で、inert 規律(ADR 0006)を破らない。
