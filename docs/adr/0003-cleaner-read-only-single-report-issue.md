# ADR 0003: hygiene ループは read-only から始め、書き込み境界を単一レポート issue に限定する

(採番メモ: 0001 は issue #25 の spec PR で、0002 は `meguri serve` で採番済みのため 0003 を使う)

## ステータス

採用(issue #44、cleaner v0)

## コンテキスト

AI ループが常時コードを生成する環境では、個々の diff が正しくても総体として乖離が蓄積する
(重複、spec の陳腐化、残骸ブランチ、放置 TODO)。PR 単位のゲート(reviewer)では原理的に
捕捉できず、集約視点の掃引圧力が別に要る。最終形は detector → fixer の自動 hygiene
パイプラインだが、自動修正には冪等性・confidence 階層・bot ループ防止といった重い機構が要る。
信頼のない自動修正ほど厄介なものはない。まず観測だけを自動化し、判断は人間に残す —
autofix 系プロダクト(Meta SCARF、autofix.ci)が揃って通った道でもある。

## 決定

1. **hygiene 系の新ループは read-only detector として導入する。** リポジトリへの作用は
   「検出結果の報告」のみで、修正への昇格は信頼を積んだ後の別 issue とする。
2. **書き込み境界は 1 project = 1 本のレポート issue の作成・更新に限定する**
   (Renovate の Dependency Dashboard 方式)。push・ブランチ操作・他 issue / PR への
   ラベルやコメントは一切しない。境界を守るため、他ループが使う `meguri:working` claim
   すら行わない(重複防止は DB の一意制約と head マーカーで足りる)。
3. **レポート issue の本文は履歴ではなくスナップショットとして毎回完全上書きする。**
   走査済み head と巡回時刻はマーカー(`<!-- meguri:clean head=... scanned=... -->`)として
   本文に埋める — 何を走査したかの真実は forge 側に置く(Authority 原則)。

## 帰結

- 検出が誤っていても壊れるものが何もないため、エスカレーション(`meguri:needs-human`)や
  bot ループ防止機構なしで安全に常駐できる。失敗は静かにスキップして次回巡回に委ねられる。
- 人間の操作面が 1 本の issue に集約される: 採用は通常 issue 化して既存フローへ、
  誤検知は config の無視リストへ、停止は `meguri:hold` 1 枚。
- issue 本文が唯一の永続状態なので、ホストをまたいでも再起動しても同一 head の再走査が起きない。
- 将来 detector → fixer に昇格させる場合も、この issue が findings の供給源になる。
  昇格の判断(confidence 階層、冪等 PR 管理)はそのときの ADR で改めて行う。
