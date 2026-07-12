# ADR 0004: private + Free の制約下では automerge のゲートを Renovate 側マージに置く

## ステータス

採用(issue #79。spec レビューを clean で通過し、実装済み)

## コンテキスト

supply-chain 連作((1/4)〜(4/4))の要は「依存更新の automerge を、必須チェック緑を
前提とすることで安全にする」ことだった。当初の想定は branch protection の required
status checks をゲートにすることだが、`kkato1030/meguri` は個人アカウント所有の
private リポジトリ(Free プラン)であり、branch protection / rulesets も
secret scanning + push protection も提供されない(API は 403 を返す)。
解禁の道は「public 化」(不可逆・履歴のシークレット監査が前提のオーナー判断)か
「Pro 課金」(branch protection のみ解禁で secret scanning は依然不可)しかない。

一方 Renovate の automerge には 2 経路ある: GitHub ネイティブ auto-merge を使う
`platformAutomerge`(branch protection が前提)と、Renovate 自身によるマージ
(branch のチェックがすべて緑になるまで待つ。`ignoreTests` デフォルト false)。
CI は全 PR で `test` と `cargo-deny` を走らせているため、後者は required status
checks の実質的な代替として機能する。

## 決定

1. **リポジトリは当面 private + Free のままとし、automerge のゲートは
   「チェック緑を待つ Renovate 側マージ」に置く。** `renovate.json5` に
   `platformAutomerge: false` を明示し、経路をこの 1 本に固定する — 将来
   `allow_auto_merge` が有効化されても、required checks なしの GitHub
   auto-merge というゲートなし経路が静かに生えないようにする。
2. **Dependabot は alerts(通知)のみの役割とし、security updates(PR 作成)は
   無効化する。** 脆弱性 PR の作成は Renovate の `vulnerabilityAlerts` に一本化し、
   同一脆弱性に対する二重 PR を避ける。
3. **branch protection / secret scanning + push protection は「public / Pro
   移行時に有効化するもの」として手順書(`docs/ops/github-settings.md`)に
   チェックリスト化して残す。** required checks は CI の job 名 `test` と
   `cargo-deny`。

## 帰結

- automerge は CI(`test` + `cargo-deny`)が緑になるまでマージされない、という
  連作の本来の目的はコストゼロで達成される。
- サーバ側の強制は存在しないため、人間による main への直 push・force-push・
  赤 PR の手動マージは防げない。単独メンテナの private リポジトリのリスクとして
  受容し、公開時に branch protection で塞ぐ。
- 鍵の誤コミットを push 時点でブロックする手段はこの形態では存在しない。
  緩和はローカルフック等の運用に留まり、根本解決は public 化(無料で解禁)に伴う。
- リポジトリを public 化する場合は、全履歴のシークレット監査を前提作業とした上で、
  手順書のチェックリストに従って本 ADR の決定を branch protection ベースに差し替える
  (そのとき本 ADR は superseded とする)。
