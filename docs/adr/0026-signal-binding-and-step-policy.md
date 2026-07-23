# ADR 0026: signal binding の seam と step policy の後段フィルタ

- Status: accepted
- Date: 2026-07-21
- Issue: #223(ADR 0012 スライス3 に合流。旧 #200)

## Context

ADR 0012 の Issue Kind reconciler は、観測(status 軸)から現状を導き、人間が書いた宣言
(spec 軸: `hold` / `needs-human` / phase)を入力として `next_step` を決める。ここには 2 つの
分離すべき関心がある。

1. **どの担体から spec/status 軸を読むか**。今日はすべて GitHub ラベル(ADR 0005)だが、将来は
   マーカーコメント等 別の担体に束ねたい場面がありうる。`next_step` の中に「ラベルを直読み」を
   埋め込むと、担体を差し替えるたびに決定ロジックを触ることになる。
2. **どの arm を今 動かしてよいか**。従来は loop ごとに個別のキルスイッチ(`review.impl_enabled`
   など)が散らばっていた。arm が増えるたびにスイッチが増え、「無効化されたら次に何を返すか」が
   loop ごとにバラバラだった。

## Decision

この 2 つを、reconciler の **seam**(担体)と **後段フィルタ**(政策)として分離する。スライス3 では
**部分導入**にとどめる —— seam を入れるのが成果物で、担体を複数実装するのではない。

### signal binding = `SignalCarrier` seam

`Snapshot` を作るとき、spec 軸(phase / `hold` / `needs-human`)を読む口を `SignalCarrier` トレイト
越しにする。本スライスは **`Labels` 担体1つだけ**を実装する —— 今日の「ラベル直読み」を seam の裏へ
そのまま写すだけで、挙動は不変。`Markers` 担体は将来の空席(未実装)。既定束縛は `Labels`。

> property: 全ラベル集合について「`Labels` 担体経由で作った signal == 直読み baseline」。seam は
> 挙動保存であることを機械的に担保する。

### step policy = `apply_policy` 後段フィルタ

`next_step` が返した生の `Step` を、純関数 `apply_policy(step, &StepPolicy) -> Step` に通す。不許可の
arm の `Agent(_)` は `Wait(PolicyDisabled)` になる(それ以外の `Step` は素通し)。これで散らばった
per-loop キルスイッチを **一枚の後段フィルタ**へ統一する。config は `[reconciler.policy]` の allow-set
(既定は全 arm 有効)。

> property: 全 snapshot × policy について、無効 arm は決して `Agent` を返さず常に
> `Wait(PolicyDisabled)`。かつ所有の全域性(ADR 0012 §3)はフィルタ後も保たれる —— `Wait` も
> ちょうど1つの所有だから。

## Consequences

- 担体の差し替えは `SignalCarrier` の実装追加だけで済み、`next_step` は触らない。
- arm の有効/無効が一箇所(policy)に集約され、「無効化時の挙動」が `Wait(PolicyDisabled)` に統一される。
- スライス3 の射程は「seam を入れる」まで。`Markers` 担体・spec 軸の 4 handshake 書き込み側は将来。
