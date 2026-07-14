# ADR 0009: ネイティブ auto-merge が使えないリポジトリでは meguri 自身がマージする(orchestrator 側マージモード) — ADR 0003 の arm-only 原則を mode で二分する

- Status: accepted
- Date: 2026-07-13
- Issue: #157
- Amends: ADR 0003(auto-merge は GitHub ネイティブ auto-merge への arm が基本)

## Context

ADR 0003 は「meguri は『マージして安全か』を自前で判定せず、GitHub ネイティブ auto-merge に arm するだけ。最終判定(branch protection + required checks)は GitHub に委ねる」と決めた。この原則は **サーバ側にゲートが実在すること** を暗黙の前提にしている。

しかし **private + Free プランのリポジトリでは "Allow auto-merge" 設定そのものが有効化できない**(API で PATCH しても黙って `false` のまま戻る)。branch protection / rulesets も提供されない(API は 403)。この形態では ADR 0003 の基本経路(`enable_auto_merge` = `gh pr merge --auto`)は成立しようがなく、`[pr.auto_merge].enabled = true` は `meguri doctor` / `meguri watch` の fail-fast(`auto_merge_allowed = false`)で必ず落ちる。結果、Free plan の private リポジトリでは automerge を一切使えない。

これは meguri 自身が ADR 0004 で直面したのと同一の制約である。ADR 0004 は automerge のゲートを「チェック緑を待つ Renovate 側マージ」に置くことで回避したが、meguri のパイプラインが生成する通常の PR にはその受け皿がない。具体的な被害者は ai-revenue-engine(private + Free、コンテンツ生産の完全自動運用)で、現在は launchd + `gh pr merge` の外部スイープで暫定回避しているが、適格判定ロジックが meguri と二重管理になっている。

論点は「ネイティブ auto-merge が使えない環境で、meguri は適格 PR を自分でマージすべきか」。ADR 0003 の arm-only 原則を破ることになるため、破り方を明示的に決める必要がある。

## Decision

1. **`[pr.auto_merge]` に `mode`(`"native" | "orchestrator"`)を導入し、arm-only 原則を mode で二分する。** 既定は `"native"`(ADR 0003 の現行動作、後方互換)。
   - **native モード(既定・推奨)**: ADR 0003 のまま。サーバ側強制(branch protection + required checks)が常に強い唯一の権威であり、ネイティブが使える環境ではこれを崩さない。
   - **orchestrator モード**: ネイティブ auto-merge が使えないリポジトリ向けのフォールバック。sweep の適格判定(`meguri/` ブランチ・`Closes #N.` リンク・ブロックラベルなし・opt-in・未解決スレッドなし)を通過し、**GitHub が `MERGEABLE`(コンフリクトなし)を返す PR を、設定 strategy で meguri 自身が直接マージする**(`gh pr merge --squash --match-head-commit` 相当)。arm もマーカーコメントによる冪等性担保も不要 — 即マージなので、マージされた PR は `Closes #N.` で issue ごと閉じて `list_open_prs` から外れ、冪等性は forge の state が担保する。

2. **orchestrator モードは「meguri 自身の PR 前検証(`check_command` + self-review, ADR 0006)が唯一のゲートである」ことを明示的に受容するモードである。** ADR 0003 が GitHub の required checks に委ねていた「マージして安全か」の判定は、Free/private では成立しない。orchestrator モードでは GitHub の `MERGEABLE` は **コンフリクトの不在** しか意味せず、CI 緑や approval を意味しない(そもそも required checks が存在しない)。したがってゲートは meguri がマージ前に走らせる `check_command` と self-review ループだけになる。この受容が orchestrator モードの本質であり、native モードとの決定的な差である。

3. **`--match-head-commit <head_sha>` は orchestrator モードでも維持する。** meguri が適格性を確認した head と実際にマージする head の原子的一致を GitHub 側で保証し、確認とマージの間の push という TOCTOU を弾く(ADR 0003 決定 2 の一般化)。これは既存の「arm しに行った時点で既に mergeable なら GitHub の判定でそのまま `merge_pr` する」経路(ADR 0003 決定 1 の唯一の例外)の一般化でもある — orchestrator モードはその例外経路を常態にしたものと理解できる。

4. **fail-fast は mode 対応にする。** orchestrator モードでは "Allow auto-merge" 設定・branch protection を要求しない(そもそも使えない前提のモード)。ただし **設定 strategy がリポジトリで許可されているか**(`allow_squash_merge` 等)は依然として検証する — `gh pr merge --squash` は squash 不許可のリポジトリで失敗するため、これは orchestrator モードでも実在する前提。native モードの fail-fast は現行どおり(auto-merge 許可・strategy 許可・(要求時)protection の三点)。

5. **`require_branch_protection = true` と orchestrator モードの併用は config validate で弾く。** orchestrator は protection が無い前提のモードであり、両立しない。既定値が `true` であるため、orchestrator モードの利用者には `require_branch_protection = false` の明示を要求する。これは「サーバ側ゲートは無く、meguri の検証だけがゲートである」という受容(決定 2)を config 上で一行の明示的な承認として残すためであり、meguri の「設定と現実の不一致は起動時に人間へ返す」方針(ADR 0003 決定 4)と一貫する。

## Consequences

- Free/private リポジトリでも automerge が使えるようになる。ai-revenue-engine の外部スイープ(launchd + `gh pr merge`)は撤去でき、適格判定ロジックが meguri に一本化される。
- orchestrator モードの自動マージの安全性は **meguri の PR 前検証(`check_command` + self-review)の質に等しい**。サーバ側の強制は存在しないため、人間による main への直 push・赤い PR の存在は防げない — 単独オーナーの private リポジトリのリスクとして受容する(ADR 0004 の帰結と同型)。`meguri doctor` は orchestrator モード時に「サーバ側ゲートなし・meguri の検証のみがゲート」であることを注意表示し、この受容を運用者に想起させる。
- ネイティブが使える環境では native が推奨のまま。orchestrator は「使えない環境のフォールバック」という位置付けを崩さない — サーバ側強制は常に orchestrator 側検証より強い。
- `UNKNOWN`(mergeability 算出中)は次回 sweep に持ち越し、`CONFLICTING` は conflict-resolver ループに委ねる(native と同じ、ADR 0007)。
- orchestrator モードは即マージなので arm マーカーを残さない。merge-watch(auto-merge 2/3, ADR 0007)は armed marker を持つ PR だけを watch するため、orchestrator モードでは実質 no-op になる — 即マージにドリフト窓が無いので、これは正しい挙動。
- マージ後のブランチ・worktree・pane の回収は既存の issue-close 起点の回収(`Closes #N.` によるマージ→issue クローズ)がそのまま働く。orchestrator 固有の後始末は不要。
