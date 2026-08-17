# Architecture Decision Records (ADR)

このディレクトリは meguri の**設計上の決定**を1件1ファイルで残す。

- **ADR = 決定の記録**。「なぜこう決めたか」を後から辿れるようにするのが目的。実装の
  現在地を映す [architecture.md](../architecture.md)、未来の計画を書く [plan.md](../plan.md)
  とは役割が違う —— ADR は**その中間の「分岐点での判断」**を凍結する。
- 旧実装(v1/v2)の ADR 群は失敗カタログとしてリモートの archive ブランチに保存されている。
  v3(現行)は 0001 から番号を振り直す。

## 書くとき

1. [TEMPLATE.md](TEMPLATE.md) を `NNNN-kebab-title.md` にコピー(4 桁ゼロ埋め・連番)。
2. 中身を埋める。**content は日本語**(architecture.md / plan.md と同じ)。
3. 下の索引に 1 行足す。
4. 決定が別の決定を置き換えるときは、古い方の Status を `Superseded by NNNN` にし、
   本文冒頭にリンクを張る(消さない —— 経緯を残す)。

## Status の意味

- `Proposed` — 提案中(まだ動いていない / 合意前)。
- `Accepted` — 採用。実装がこれに従う。
- `Superseded by NNNN` — 後続 ADR に置き換えられた(記録として残す)。
- `Deprecated` — もう当てはまらないが、置き換え先が無い。

## 索引

| # | タイトル | Status |
|---|---|---|
| [0001](0001-delivery-and-projection-layers.md) | 配送と projection のレイヤー分離・delivery target は repo 単位 | Accepted |
| [0002](0002-acceptance-as-outcome-fact.md) | 受理は Outcome 側の耐久事実・複数 artifact / 複数リポは将来に開く | Accepted |
