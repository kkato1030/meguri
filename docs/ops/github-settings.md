# GitHub リポジトリ設定 — 現状・検証・移行チェックリスト

issue #79((4/4) GitHub プラットフォーム設定)の成果物。このリポジトリは
**個人アカウント所有の private リポジトリ(Free プラン)** であり、branch protection /
rulesets / secret scanning + push protection は提供されない。そのため依存更新
automerge のゲートは **「チェック緑を待つ Renovate 側マージ」** に置いている(ADR 0004)。
このドキュメントは、その前提となる設定の現状と検証コマンド、ゲートが依存する不変条件、
そして public / Pro へ移行したときに有効化すべきものを記録する。

前提: `gh` CLI が `kkato1030/meguri` に対する権限で認証済みであること。

## 現状の設定(あるべき状態)

| 設定 | 状態 | 理由 |
|---|---|---|
| Dependabot alerts | **有効** | 無料の脆弱性通知源。Renovate の `vulnerabilityAlerts` の供給源でもある |
| Dependabot security updates | **無効** | 脆弱性 PR の作成は Renovate `vulnerabilityAlerts` に一本化(二重 PR 防止) |
| リポジトリ設定 Allow auto-merge | **無効** | GitHub ネイティブ auto-merge は required checks なしでは即マージの穴になるため使わない |
| Renovate `platformAutomerge` | **false**(`renovate.json5`) | automerge 経路を「チェック緑を待つ Renovate 側マージ」1 本に固定 |
| Branch protection / rulesets | 設定不可(Free + private) | 移行時チェックリスト参照 |
| Secret scanning + push protection | 設定不可(個人 private) | 移行時チェックリスト参照 |

## 検証コマンド

```console
# Dependabot alerts が有効か(HTTP/2.0 204 なら有効)
$ gh api repos/kkato1030/meguri/vulnerability-alerts -i | head -1
HTTP/2.0 204 No Content

# Dependabot security updates が無効か(enabled: false であること)
$ gh api repos/kkato1030/meguri/automated-security-fixes -q '{enabled: .enabled}'
{"enabled":false}

# Allow auto-merge が無効か(false であること)
$ gh api repos/kkato1030/meguri -q .allow_auto_merge
false
```

あるべき状態から外れていた場合の復旧:

```console
$ gh api -X PUT repos/kkato1030/meguri/vulnerability-alerts          # alerts 有効化
$ gh api -X DELETE repos/kkato1030/meguri/automated-security-fixes   # security updates 無効化
$ gh api -X PATCH repos/kkato1030/meguri -F allow_auto_merge=false   # auto-merge 無効化
```

## automerge ゲートが依存する不変条件

Renovate 側マージのゲートは「**すべての PR で CI(`test` + `cargo-deny`)が必ず走る**」
ことに依存している。Renovate は PR 上にチェックが 1 つも存在しないと待たずにマージする。
したがって:

- **`.github/workflows/ci.yml` の `pull_request` トリガーを絞ってはならない**
  (`paths` / `branches` フィルタの追加や、job への `if` による全体スキップは、
  このゲートを静かに消す)。
- `renovate.json5` の `platformAutomerge: false` を外す・リポジトリ設定で
  Allow auto-merge を有効化する場合は、branch protection の required checks が
  先に設定済みであること(= 移行チェックリスト完了後)。

任意の追加ハードニング: branch protection の "up-to-date before merge" 相当が
ないため、「main から遅れているがチェックは緑」の PR が automerge され得る。
気になる場合は `renovate.json5` に `rebaseWhen: 'behind-base-branch'` を足すと
Renovate が常に base に追随してから automerge する(CI 実行回数は増える。
週次バッチ + 単独メンテナの現運用ではリスクが小さいため既定では入れていない)。

## automerge ゲートの観測手順(事後確認)

automerge 対象(cargo minor/patch グループ、または GitHub Actions の
digest/patch/minor)の Renovate PR が次に来たときに確認する:

1. PR が open された直後、checks 完了前にマージされていないこと
   (`gh pr view <PR#> --json state,statusCheckRollup`)。
2. `test` / `cargo-deny` の両方が成功した後の Renovate 実行(週次 or
   Dependency Dashboard からの手動トリガー)でマージされること。
3. 逆に checks が赤の automerge 対象 PR が open のまま残ること(機会があれば)。

## public / Pro 移行時チェックリスト

リポジトリを public 化する(推奨の最終形。ただし**全履歴のシークレット監査が前提**、
オーナー判断)か GitHub Pro に移行したら、以下を有効化して automerge のゲートを
サーバ側強制に差し替える:

- [ ] **Branch protection(main)**(または ruleset。public 化 / Pro で解禁):
  - Require a pull request before merging
  - Require status checks to pass — required checks は CI の job 名 **`test`** と **`cargo-deny`**
  - Require branches to be up to date before merging
  - Force push / deletion の禁止(デフォルト維持)
- [ ] **Secret scanning + push protection**(public 化で無料解禁。Pro では不可):
  `gh api -X PATCH repos/kkato1030/meguri -f security_and_analysis[secret_scanning][status]=enabled -f security_and_analysis[secret_scanning_push_protection][status]=enabled`
- [ ] リポジトリ設定 **Allow auto-merge を有効化**し、`renovate.json5` の
  `platformAutomerge: false` を削除(required checks がゲートになるため
  GitHub ネイティブ auto-merge が安全に使える)
- [ ] **OpenSSF Scorecard workflow** の追加(public のみ結果公開可)
- [ ] ADR 0004 を superseded にし、branch protection ベースの決定を新 ADR に記録

## 経緯

- 設定変更の実施と本ドキュメントの追加: issue #79
- 決定の記録: `docs/adr/0004-automerge-gate-renovate-side-on-free-private.md`
- 関連: (1/4) #76 Renovate 導入 / (2/4) #77 cargo-deny / (3/4) #78 CI ハードニング
