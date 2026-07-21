# ADR 0016: operator の介入面は identity への 3 動詞(run / why / attach)に確定し、役割別動詞は採らない

- Status: accepted
- Date: 2026-07-21
- Issue: #224(ADR 0012 スライス4 に合流。旧 #197)

> 番号について: 本リポジトリの ADR 番号は slug と Issue 番号で区別する運用であり、
> `0016-decompose-through-spec-review-gate-then-materialize.md`(#134)と番号を共有する。

## Context

ADR 0012 で meguri は level-triggered reconciler に再構成された。そこでの中心的な発見は
**「loop(役割)はコード上の1級構造物ではなく、観測から `next_step` が選ぶ arm として実行時に
現れる軌道である」**ことだった。planner / worker / reviewer / fixer は「呼び出す関数」ではなく、
issue/PR の観測された level に応じて reconciler が選ぶ枝になった。

すると operator(人間)の介入面をどう設計するかという問いが残る。素朴には、内部の役割に対応する
動詞 — `meguri plan #N` / `impl #N` / `review #N` / `fix #N` — を並べたくなる。実際 meguri が
loop を1級構造物として持っていた時代の発想はこれに近い。しかしこれは ADR 0012 が捨てたばかりの
**edge-triggered な役割思考**そのものだ。「この issue を今 plan せよ」と命じることは、reconciler が
観測から導くべき「次の一手」を人間が横から固定することであり、level-triggered の利点(冪等・
所有の一意性・観測からの再導出)を破る。

## Decision

operator が identity(issue / PR / run)に対して持つ介入動詞を、役割非依存の **3 つ**に確定する。

- **`run <id>`** — その identity の仕事を今すぐ1回起こす(cadence ゲートを迂回)。どの役割の
  run になるかは reconciler の観測が決める。operator は「動け」とだけ言い、役割は指定しない。
- **`why <id>`** — その identity について reconciler が今どう判断しているかを説明する。観測した
  Snapshot と `next_step` が返した Step、そしてその理由文字列を読み取り面に出す。「なぜ進まない
  のか」への答えは、level-triggered reconciler では観測の副産物として既にそこにある。
- **`attach <id>`** — その identity の生きたペインに端末を接続する。エージェントの画面は
  meguri が読み取って判定するものではないが(overview 参照)、人間はいつでも覗ける。

### identity と入力文法

**3 動詞は共通の identity 集合**を閉じて受ける: issue 番号 / PR 番号 / run id / local task id
(repo / project は含めない)。3 動詞は同じ型付き相互排他フラグ(`--issue` / `--pr` / `--run` /
`--task` のちょうど1つ)で identity を一意に指す — 位置引数だと issue 番号・PR 番号・run id が
同値のとき曖昧になるため。各動詞はその identity を canonical 化して扱う:
- `why` — **fresh observation** を回して Step + 理由を出す **読み取り専用**(forge 書き込みも run
  生成もしない)。
- `run` — 役割を指定せず、その identity を観測して reconciler が選んだ arm を dispatch(worker 固定
  ではない)。
- `attach` — その identity の live ペインに接続する。

**`run` / `why` は identity の種別を保つ**: open な PR を持つ identity は PR 側の reconciler が所有
するので、`--pr` / `--run`(PR 系 run)を canonical issue に潰して issue 側判断に流してはならない。
所有する側(PR / issue / local)の Snapshot と Step を対象にする。`attach` はペイン解決なので
canonical issue に畳んでよい(ペインは所有 decider に依らず identity で決まる)。

具体的な CLI 文法・例・実在 id 形式・後方互換・検証は spec の決定9に置く(spec は使い捨てなので、
恒久的な「3 動詞・共通の 4 identity・役割非指定」の決定のみここに残す)。

役割別動詞(`plan` / `impl` / `review` / `fix`)は **採らない**。理由は上記のとおり、役割は観測から
選ばれる arm であって operator が名指しで起動する対象ではないからだ。人間が望ましい状態を宣言
したいときは、spec 軸のラベル(`meguri:plan` / `meguri:ready` / `meguri:hold` / `meguri:needs-human`,
ADR 0012 決定5)を書き込む。これは「今この役割を実行せよ」という命令ではなく「こうあってほしい」
という宣言であり、reconciler がそれを観測して次の一手を導く。命令ではなく宣言、というこの非対称が
level-triggered の設計思想である。

## Consequences

- `run` / `attach` は既に存在する(`src/cli.rs`)。`why` を新設する。3 動詞のうち 2 つが既存で
  あり、確定作業の主眼は「役割別動詞を足さないと決めること」と `why` の追加にある。
- `why` は reconciler と自然に噛み合う。observe → `next_step` → Step+理由 という経路が既にあり、
  `why` はそれを人間向けに表示するだけの読み取り専用コマンドになる(書き込みを一切しない)。
- operator の語彙が役割から切り離されるため、将来 arm が増減しても介入面は変わらない。新しい
  トリガを足すことは `next_step` に arm を1本足すことであり(ADR 0012)、operator 面には波及
  しない。
- 旧 #197 はこの ADR に合流し、独立 issue にはしない。
