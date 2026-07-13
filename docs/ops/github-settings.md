# GitHub リポジトリ設定 — public 化後の目標状態・移行手順・検証

このリポジトリは OSS 公開連作(issue #113〜#119)により public 化した。
public 化は (4/7) #116(公開作業チェックリスト)で実施済み。本ドキュメントは
**public 化後の GitHub リポジトリ設定を目標状態として** 記述し、#116 で実行した
`gh api` コマンドと事後の検証コマンドを手順化する。automerge のゲートを
Renovate 側マージから GitHub 側の branch protection required status checks へ
移す判断そのものは `docs/adr/0004-automerge-gate-renovate-side-on-free-private.md`
の追補(issue #114)に記録した。

**#116 の実行により、このリポジトリは public + branch protection 有効の状態に
移行済み。** automerge のゲートは ADR 0004 追補どおり、branch protection の
required status checks(`test`・`cargo-deny`・`zizmor`)がサーバ側で強制する形に
切り替わっている。移行前(private + Free)の設定は歴史的経緯として末尾の
[「public 化前(移行前)の設定」](#public-化前移行前の設定) に残す。

前提: `gh` CLI が `kkato1030/meguri` に対する権限で認証済みであること。

## public 化後の目標状態(現状)

| 設定 | 状態 | 理由 |
|---|---|---|
| リポジトリ visibility | **public** | OSS 公開連作の目的そのもの(#116) |
| Branch protection(main) | **有効** | required status checks で automerge / 人間 merge 双方をサーバ側で強制する |
| Branch protection: required status checks | **`test`・`cargo-deny`・`zizmor`** | `.github/workflows/ci.yml` の job 名。全 PR で必ず走る 3 job(ADR 0004 追補) |
| (任意) `meguri/guard-review` を required check に追加 | **既定では未設定** | ADR 0008 の guard レビューは既定で advisory(赤チェックは出すが human マージは止めない)。human 側も厳密ゲート化したい場合のみ、この context を required に足す。ただし付けるなら該当 kind の guard を必ず ON にすること(`review.guard.impl` 等)— guard が status を出さないと required check が永久 pending になりマージがハングする |
| Branch protection: Require a pull request before merging | **有効** | main への直 push を禁止する |
| Branch protection: Require branches to be up to date before merging | **有効** | 「main から遅れているがチェックは緑」の PR が merge されるのを防ぐ |
| Branch protection: Force push / deletion | **禁止(デフォルト維持)** | 履歴改変・main 削除の防止 |
| Secret scanning + push protection | **有効** | public 化で無料解禁。鍵の誤コミットを push 時点でブロックする |
| リポジトリ設定 Allow auto-merge | **有効** | required checks がサーバ側で強制されるため、GitHub ネイティブ auto-merge が安全に使える |
| Renovate `platformAutomerge` | **`renovate.json5` から `platformAutomerge: false` を削除済み**(true 相当) | branch protection が唯一のゲートになったため、Renovate 側の待機ロジックへの依存が不要になった |
| Private vulnerability reporting | **有効**(#115) | public 化で無料解禁。脆弱性報告を非公開 issue で受け付けられる |
| Dependabot alerts / security updates | 現行のまま(alerts 有効・security updates 無効) | public 化と無関係。Renovate `vulnerabilityAlerts` への一本化は継続 |

## 移行手順(#116 で実行済み)

```console
# 1. public 化
#    --visibility を使う場合 --accept-visibility-change-consequences が必須(gh CLI)
$ gh repo edit kkato1030/meguri --visibility public --accept-visibility-change-consequences

# 2. secret scanning + push protection を有効化(public で無料解禁)
#    zsh では `[` `]` がグロブ展開されるため、フィールド名は必ずクォートする。
$ gh api -X PATCH repos/kkato1030/meguri \
    -f 'security_and_analysis[secret_scanning][status]=enabled' \
    -f 'security_and_analysis[secret_scanning_push_protection][status]=enabled'

# 3. private vulnerability reporting を有効化(#115、public で無料解禁)
$ gh api -X PUT repos/kkato1030/meguri/private-vulnerability-reporting

# 4. branch protection(main)を required status checks 付きで設定
#    required_status_checks[strict] は boolean 必須フィールドなので -F(typed)で送る。
#    required_pull_request_reviews は null にすると PR 必須自体が無効化されるため、
#    「PR は必須・承認数は要求しない」を表す required_approving_review_count=0 の
#    object を渡す(null ではない)。
$ gh api -X PUT repos/kkato1030/meguri/branches/main/protection \
    -F 'required_status_checks[strict]=true' \
    -f 'required_status_checks[checks][][context]=test' \
    -f 'required_status_checks[checks][][context]=cargo-deny' \
    -f 'required_status_checks[checks][][context]=zizmor' \
    -F 'required_pull_request_reviews[required_approving_review_count]=0' \
    -F 'enforce_admins=true' \
    -F 'restrictions=null'

# 5. リポジトリ設定 Allow auto-merge を有効化
$ gh api -X PATCH repos/kkato1030/meguri -F 'allow_auto_merge=true'
```

`renovate.json5` の `platformAutomerge: false` の削除、および冒頭コメント
(private + Free 前提の記述)の更新は、この branch protection 設定が完了した
**後** に行った(順序を守らないと required checks なしで native auto-merge が
armed される穴が生まれるため)。

## public 化後の検証コマンド

```console
# visibility が public か
$ gh api repos/kkato1030/meguri -q .visibility
public

# secret scanning / push protection が有効か
$ gh api repos/kkato1030/meguri -q '.security_and_analysis'
{"secret_scanning":{"status":"enabled"},"secret_scanning_push_protection":{"status":"enabled"},...}

# branch protection の required checks が想定どおりか
$ gh api repos/kkato1030/meguri/branches/main/protection/required_status_checks -q .contexts
["test", "cargo-deny", "zizmor"]

# Allow auto-merge が有効か
$ gh api repos/kkato1030/meguri -q .allow_auto_merge
true
```

## automerge ゲートが依存する不変条件(public 化後)

required status checks は **CI(`.github/workflows/ci.yml`)の job 名と完全一致**する
文字列で branch protection に登録される。したがって:

- **`.github/workflows/ci.yml` の `test`・`cargo-deny`・`zizmor` の job 名を
  リネームする場合は、branch protection の required status checks 設定も
  同時に更新する。** 片方だけ変更すると、リネーム後の job は「新しい未知の
  check」として扱われ、旧 job 名は「二度と報告されない required check」として
  merge を永久にブロックする。
- **`.github/workflows/ci.yml` の `pull_request` トリガーを絞ってはならない**
  (`paths` / `branches` フィルタの追加や、job への `if` による全体スキップは、
  該当 PR で required check が永遠に pending のままになり merge 不能になる)。
- branch protection の "up-to-date before merge" が有効な間は、Renovate の
  automerge も base の追随を待つ。CI 実行回数が増える点は許容する
  (単独メンテナ + 週次バッチの現運用ではコストが小さい)。

## public 化前(移行前)の設定

以下は #116 実行前(private + Free 時代)の設定の記録。ADR 0004 本文の決定に対応する。

| 設定 | 状態 | 理由 |
|---|---|---|
| リポジトリ visibility | private | OSS 公開連作(#113〜#119)前の初期状態 |
| Dependabot alerts | 有効 | 無料の脆弱性通知源。Renovate の `vulnerabilityAlerts` の供給源でもある |
| Dependabot security updates | 無効 | 脆弱性 PR の作成は Renovate `vulnerabilityAlerts` に一本化(二重 PR 防止) |
| リポジトリ設定 Allow auto-merge | 無効 | GitHub ネイティブ auto-merge は required checks なしでは即マージの穴になるため使わない |
| Renovate `platformAutomerge` | false(`renovate.json5`) | automerge 経路を「チェック緑を待つ Renovate 側マージ」1 本に固定 |
| Branch protection / rulesets | 設定不可(Free + private) | 上記「移行手順」で設定 |
| Secret scanning + push protection | 設定不可(個人 private) | 上記「移行手順」で設定 |
| Private vulnerability reporting | 設定不可(個人 private) | 上記「移行手順」で設定(#115) |

移行前の automerge ゲートは「**すべての PR で CI(`test` + `cargo-deny`)が必ず走る**」
ことに依存していた(`zizmor` は issue #79 当時は存在せず、ADR 0004 の required checks
一覧には含まれていない)。Renovate は PR 上にチェックが 1 つも存在しないと待たずに
マージするため、`.github/workflows/ci.yml` の `pull_request` トリガーを絞ってはならない
という不変条件は移行後も同様に成り立つ。

## 経緯

- 設定変更の実施と本ドキュメントの追加: issue #79
- 決定の記録: `docs/adr/0004-automerge-gate-renovate-side-on-free-private.md`
  (public 化後の移行判断は同 ADR の追補、issue #114)
- 関連: (1/4) #76 Renovate 導入 / (2/4) #77 cargo-deny / (3/4) #78 CI ハードニング
  (`zizmor` job の追加を含む)
- OSS 公開連作: (1/7) #113 LICENSE / (2/7) #114 本ドキュメントの public 前提化 /
  (3/7) #115 Private vulnerability reporting / (4/7) #116 公開作業チェックリスト
  (本ドキュメントの手順を実行し、public 化を完了した)
