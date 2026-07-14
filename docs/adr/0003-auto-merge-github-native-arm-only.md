# ADR 0003: 自動マージは GitHub ネイティブ auto-merge への arm が基本 — meguri は「マージして安全か」を判定せず、GitHub が承認済み(clean)のマージのみ確定する

- Status: accepted(orchestrator モードによる補完あり — 下記追補 / ADR 0009 参照)
- Date: 2026-07-12
- Issue: #41

## Context

meguri のパイプライン(plan → spec review → 実装 → review → fix)は PR を「マージ可能」まで運ぶが、マージそのものは人間に任せてきた(forge にマージ系 API は 0 行)。オプトインの自動マージを導入するにあたり、「いつマージして安全か」を誰が判定するかが論点。選択肢は (a) meguri が CI 結果・approval・スレッド状態を自前で集計して `gh pr merge` を直接叩く、(b) 条件の揃った PR に GitHub ネイティブの auto-merge を arm し、最終判定を GitHub(branch protection + required checks)に委ねる。

looper は同じ問題を ADR-0005 で (b) と決めており、meguri も同じ前提(forge のラベル・コメントが唯一の真実 — "Authority" 原則)に立っている。

## Decision

1. **meguri は「マージして安全か」を自前で判定しない。** マージを実行する権威(= branch protection + required checks を満たすか)は GitHub にあり、meguri は CI 結果や approval を自前で再判定しない。判定を二重化すると、GitHub 側の設定変更(required checks の追加等)に meguri の判定が置いていかれ、古い判定でマージする事故が起きる — 権威は一箇所に置く。基本形は `enable_auto_merge`(= `gh pr merge --auto`)で arm するだけ。**唯一の例外**は、arm しようとした時点で GitHub が既に「マージ可能(clean status)」と判定済みで auto-merge の予約が成立しない場合で、このときだけ meguri が `merge_pr`(= `gh pr merge --match-head-commit`、`--auto` なし)でマージを確定する。これは meguri が安全性を判定しているのではなく、GitHub が既に下した判定(clean = branch protection の要求をすべて充足)に従って、GitHub 自身が「ブロックが解けたら」実行するはずだったマージを代わりに実行するだけ — 権威は依然 GitHub にある。`--match-head-commit` により確認した head 以外はマージしない。
2. **arm は `--match-head-commit <head_sha>` で head に固定する。** meguri が条件を確認した head と GitHub が arm する head が原子的に一致することを保証し、確認と arm の間の push という TOCTOU を GitHub 側で弾く。
3. **arm の記録は PR 上のマーカーコメント(`<!-- meguri:automerge armed head=<sha> -->`)。** ローカル状態ではなく forge に置く("Authority")。同一 head を二度 arm しない冪等性と、人間が auto-merge を解除した head に再 arm しない上書き尊重を、この 1 つの仕組みで賄う。push で head が変われば条件を再判定する。
4. **fail-fast: 静かな劣化を許さない。** `enabled = true` なのにリポジトリが auto-merge 不許可・strategy 不許可・(要求時)required checks 付き protection なしの場合、`meguri watch` 起動時と `meguri doctor` で拒否する。strategy の fallback(squash がダメなら merge、のような)もしない — 設定と現実の不一致は実行時に黙って吸収せず、起動時に人間へ返す。

## Consequences

- 自動マージの安全性は **リポジトリの branch protection の質に等しい**。required checks が薄いリポジトリでは薄い保証のままマージされる。だからこそ `require_branch_protection = true` がデフォルトで、オプトイン(ラベル or config)を二段にしている。
- meguri 側の実装は「条件が揃ったら arm、あとは待つ」だけになり、CI ポーリングやマージリトライのループを持たない。実装・テストの表面積が小さい。
- arm 後に条件が崩れた場合(新しい review thread 等)の解除は GitHub は自動でやらない。ドリフト検出・解除は後段の merge-watch(別 issue)で扱う — この ADR は「meguri がマージ実行者にならない」ことだけを固定する。
- classic branch protection API で判定するため rulesets 運用は検出できない。加えて protection の存在確認(`branches/{base}/protection/required_status_checks`)は **admin 権限のトークンを要する** — write のみのトークンでは protection 実在下でも 403 になり判定できない。どちらの場合も `require_branch_protection = false` が逃げ道だが、その場合 protection の存在確認は人間の責任になる。403 を「protection なし」に倒すと保証の薄いリポジトリと区別できず fail-fast の意味が失われるため、403 はエラーとして人間に返す。

## 追補(issue #157、2026-07-13 — orchestrator モード)

本 ADR の基本形(arm-only、判定を GitHub に委ねる)は **サーバ側にゲートが実在すること** を暗黙の前提にしている。**private + Free プランのリポジトリでは "Allow auto-merge" 設定自体が有効化できず**(API で PATCH しても黙って false のまま)、branch protection も無い(403)ため、この前提が崩れ、`enabled = true` は fail-fast で必ず落ちる。

この環境向けのフォールバックとして **orchestrator モード**(`[pr.auto_merge].mode = "orchestrator"`)を追加した。適格判定は本 ADR の sweep と共通のまま、GitHub が `MERGEABLE` を返す PR を meguri 自身が直接マージする(arm しない)。本 ADR の「arm しに行った時点で既に mergeable なら `merge_pr` で確定する」唯一の例外を常態化したものであり、**「meguri 自身の PR 前検証(`check_command` + self-review, ADR 0006)が唯一のゲートになる」ことを明示的に受容するモード**である。native モード(既定)は本 ADR のまま。決定の詳細と帰結は **ADR 0009** を参照。
