# ADR 0004: private + Free の制約下では automerge のゲートを Renovate 側マージに置く

## ステータス

採用(issue #79。spec レビューを clean で通過し、実装済み)。public 化完了(issue #116)
時点で superseded — 移行判断は追補(issue #114)を参照。

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

## 追補(issue #114、2026-07-12)

OSS 公開連作(issue #113〜#119)により、この ADR が前提としていた「public 化は不可逆な
オーナー判断であり本 issue のスコープ外」という状況が動いた。public 化そのものは
(4/7) #116(公開作業チェックリスト)で実施することが決まっている。本 ADR の決定
(Renovate 側マージをゲートにする)は **#116 が実行され、private + Free という前提が
崩れるまでは有効なまま** であり、この追補はその後に適用される移行判断を先出しで
記録するものである(#116 の実行時点で本 ADR は superseded とする)。

**移行判断:** public 化が完了した時点で、automerge のゲートを Renovate 側マージから
**GitHub 側の branch protection required status checks** に移す。

- required checks は CI(`.github/workflows/ci.yml`)の job 名 **`test`・`cargo-deny`・
  `zizmor`** の 3 つとする。ADR 0004 制定時点(issue #79)は `zizmor` job がまだ存在せず
  `test` / `cargo-deny` の 2 つだったが、その後 (3/4) #78 の CI ハードニングで
  `zizmor`(ワークフロー静的解析)job が追加済みであり、required checks の対象として扱う。
- required checks が server 側で強制されることで、GitHub ネイティブ auto-merge
  (`platformAutomerge`)が required checks なしの「即マージの穴」ではなくなる。
  リポジトリ設定 Allow auto-merge を有効化し、`renovate.json5` の
  `platformAutomerge: false` を削除して Renovate 側マージ待機と二重にゲートを持つ
  運用をやめてよい。
- Dependabot security updates を無効化したまま Renovate の `vulnerabilityAlerts` に
  一本化する運用、および branch protection でカバーされない領域(force-push 禁止・
  PR 必須・up-to-date 要求)は変更しない。branch protection がこれらも合わせて
  server 側強制に置き換える。
- secret scanning + push protection は public 化で無料解禁されるため同時に有効化する
  (automerge ゲートとは独立した決定だが、同じ #116 のタイミングで行う)。

具体的な手順(`gh api` コマンド・チェックリスト)は `docs/ops/github-settings.md` に記録する。
本 ADR 自体を「Renovate 側マージが唯一のゲート」から「branch protection が唯一のゲート」に
書き換える(=本 ADR を superseded とし新 ADR に差し替える)のは、判断ではなく実行の話なので
#116 の作業に含める。
