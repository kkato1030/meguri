# issue-79 spec — supply-chain (4/4): GitHub プラットフォーム設定

issue は「Dependabot alerts / secret scanning + push protection / branch protection を有効化せよ」と言っている。ところが調査してみると、このリポジトリの形態ではそのうち 2 つが**原理的に有効化できない**。この spec の仕事は、機能の列挙ではなく「この制約下で issue の本来の目的 — automerge を安全にする — をどう達成するか」に収束させることだ。

## 調査で判明した前提(2026-07 時点、`gh api` で確認済み)

- `kkato1030/meguri` は **User 所有の private リポジトリ、Free プラン**。
- **Branch protection / rulesets**: API が 403 — "Upgrade to GitHub Pro or make this repository public"。**使えない。**
- **Secret scanning + push protection**: 個人アカウントの private リポジトリには提供されない(public なら無料、private は組織 + GitHub Secret Protection のみ)。**使えない。**
- **Dependabot alerts**: すでに有効(`GET /repos/{repo}/vulnerability-alerts` → 204)。
- **Dependabot security updates**: 有効になっている(`{"enabled":true}`)。issue の方針「security updates の PR 作成は Renovate に任せる」と重複しており、Renovate の `vulnerabilityAlerts`(`renovate.json5:37`)と二重に PR が立つ。
- リポジトリ設定 `allow_auto_merge` は false — GitHub ネイティブ auto-merge はそもそも使えない状態にある。

つまり元 issue の受け入れ条件 4 項のうち 2 項(secret scanning、branch protection)は満たせない。決定が要る。

## 決定: 3 つの選択肢と推奨

| 案 | 内容 | 得られるもの | コスト / 前提 |
|---|---|---|---|
| A | リポジトリを public 化 | branch protection・secret scanning + push protection・Scorecard、すべて無料で解禁 | 公開は事実上不可逆。**全履歴のシークレット監査が前提**。オーナーの意思決定 |
| B | GitHub Pro($4/月) | private のまま branch protection のみ解禁 | secret scanning は依然使えない。中途半端 |
| C | 現状維持(private + Free) | automerge ゲートを Renovate 側で担保(下記) | コストゼロ。人間の main 直 push は防げない |

**推奨は C。** この連作((1/4)〜(4/4))の目的は「automerge を安全にする」ことで、それは C で達成できる。A は MIT ライセンスで README も公開向けに整っているこのプロジェクトにとって自然な最終形だが、履歴監査という別の前提作業を伴うオーナー判断であり、この issue に同梱すべきではない(スコープ外の別 issue とする)。この決定は spec より長生きするので ADR 0004 に置いた(本 PR に同梱、ステータスは提案)。

## 案 C の成立根拠 — branch protection なしで automerge がなぜ安全か

Renovate の automerge には 2 経路ある: GitHub ネイティブ auto-merge を使う `platformAutomerge`(デフォルト true)と、Renovate 自身によるマージ。前者は branch protection が前提の機能で、いまの設定では動かない — が、「動かないから放置」は脆い。後者の **Renovate 自身のマージは、branch 上のチェックがすべて緑になるまで待つ**(`ignoreTests` デフォルト false)。CI は全 PR で `test` と `cargo-deny` の 2 job を走らせるので、required status checks が設定できなくても実質同じゲートが機能する。

よって `renovate.json5` に **`platformAutomerge: false` を明示**し、automerge の経路を「チェック緑を待つ Renovate 側マージ」の 1 本に固定する。将来誰かが `allow_auto_merge` を有効にしても、ゲートなしの経路が静かに生えることはない。

残る穴は branch protection との差分そのもの: 人間による main への直 push・force-push・赤 PR の手動マージは防げない。単独メンテナの private リポジトリではこれを受容し、公開(または Pro)時に branch protection で塞ぐ — その際の設定内容を手順書に残しておく。

## 実装内容(この branch の続きでやること)

1. **GitHub 設定変更**(`gh api`、手順とともに手順書へ記録):
   - Dependabot **security updates を無効化**(`DELETE /repos/{repo}/automated-security-fixes`)— 脆弱性 PR の作成を Renovate の `vulnerabilityAlerts` に一本化する。alerts 自体は無料の通知源として維持。
   - Dependabot alerts が有効であることの検証コマンド(204 確認)。
2. **`renovate.json5`**: `platformAutomerge: false` を追加。冒頭コメント(`renovate.json5:5`)の「branch protection の必須チェック…((4/4) で設定)」という記述を実態(Renovate 側ゲート)に合わせて更新。
3. **`docs/ops/github-settings.md`(新規)**: 現状の設定と検証コマンド、「public / Pro に移行したら有効化するもの」チェックリスト — branch protection(required checks は CI の job 名 `test` と `cargo-deny`、PR 必須、up-to-date 要求)、secret scanning + push protection、OpenSSF Scorecard。
4. **ADR 0004**(本 PR に同梱済み)。
5. **optional(spec レビューで採否)**: gitleaks を CI に追加し、secret scanning の「検出」側だけ代替する。push 時点のブロックにはならない(push 後検出)し、CI ハードニングの (3/4) #78 に寄せる手もあるので、既定では見送りに倒す。

## 受け入れ条件(元 issue から改訂)

- [ ] Dependabot alerts 有効(手順書の検証コマンドで 204 を確認)
- [ ] Dependabot security updates 無効(Renovate と PR が二重に立たない)
- [ ] `renovate.json5` に `platformAutomerge: false` があり、`renovate-config-validator` を通る
- [ ] automerge 対象 PR がチェック緑まで実際にマージされないこと — 次回の Renovate automerge PR で観測して確認(観測手順を手順書に記載)
- [ ] `docs/ops/github-settings.md` に、現状不可の 2 項(secret scanning + push protection / branch protection)が「public / Pro 移行時チェックリスト」として明記されている

## 触るファイル

- `renovate.json5` — `platformAutomerge: false`、コメント更新
- `docs/ops/github-settings.md` — 新規、手順書 + 移行時チェックリスト
- `docs/adr/0004-automerge-gate-renovate-side-on-free-private.md` — 決定の記録(本 PR に同梱)
- (optional)`.github/workflows/ci.yml` — gitleaks job(既定では見送り)

## スコープ外

- **リポジトリ公開の判断と全履歴のシークレット監査**(案 A)— オーナー判断の別 issue。
- **OpenSSF Scorecard** — private では結果公開ができず価値が薄い。公開時チェックリストへ。
- **(3/4) #78 の CI ハードニング**(SHA pin / harden-runner / zizmor)— 別 issue で進行中。
