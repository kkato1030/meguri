# 刈り込み計画: ミニマム核への削減と権威反転

Status: draft(人間レビュー待ち)
Date: 2026-07-29

## 背景と診断

meguri 自身の開発を meguri で行った結果、intent 生成と実装の両方が安くなり、
「turn の成功は検証するが feature の存在価値は検証しない」ループが
プロダクト表面積を無検証に増幅した。現状 src 約 56,000 行・テスト約 20,000 行・
ADR 64 本に対し、直近 2 週間で収束・速度の診断ラウンドを 3 回要している。
段階的な削減ではなく、いったんミニマムな核まで絞り込み、規律に基づいて
再成長させる。

あわせて、GitHub GraphQL レートリミットを構造的にはらむ
「状態権威が GitHub にある」設計を反転する。**sqlite が真実、GitHub は投影**。

## 核の定義

meguri の発明は次の一文に凝縮される:

> 生きた pane でエージェントを走らせ、完了申告を信用せず独立検証し、
> タスクを PR に収束させる。

これを実現する最小集合が核。それ以外はすべて「核の運用中に観測された失敗への
対処」として、失敗が再観測されたときに再導入する。

### 核に残すもの

| 構成要素 | 理由 |
|---|---|
| `turn/`(完了コントラクト: prompt-<id>.md / result.json / turn_id 照合 / nudge) | 発明そのもの |
| `gitops.rs`(worktree 管理 + 独立検証: clean / ahead / check_command) | 成功検証の耐力壁 |
| `mux/`(tmux + herdr + fake) | 生きた pane という差別化。herdr が主用、tmux は統合テスト |
| `forge/`(縮小 Forge trait + gh + fake) | PR 作成・コメント等の**投影書き込み**に限定 |
| `engine/flow.rs`(縮小 step machine: prepare → worktree → execute → validate → open_pr) | checkpoint resume を含むターン駆動の本体 |
| WorkerFlavor(唯一のループ) | issue→PR の実行主体 |
| 縮小 task reconciler(純粋 decider パターンは維持) | level-triggered の判断核 |
| `engine/scheduler.rs`(縮小 tick) | dispatch と crash recovery(redispatch) |
| `engine/reaper.rs`(縮小) | pane/worktree 回収。session id 保存 → `--resume` |
| `agent_session.rs` | resume 用ネイティブセッション id 解決(ADR 0029) |
| 縮小 `escalation` | needs_human 化 + インフラ障害の分類(ADR 0028)。profile ladder は削除 |
| `tasks.rs` の `TaskSource` + `LocalTaskSource` | 権威反転の土台。**local が default になる** |
| `store/`(runs / turns / events / panes / tasks) | 状態権威。schema は v1 に畳み直す |
| `events.rs` | 再成長規律の証拠チャネル |
| `preflight.rs` | folder-trust / bypass 初回ゲートは pane 起動の本質的弱点で、
  複数回実観測済み(#232/#234/#235)。証拠ゲート基準を既に満たす |
| CLI: `init` / `doctor`(縮小) / `add` / `watch`(foreground) / `run` / `attach` / `ps` / `tasks` / `pause` / `resume` / `stop` / `prune` | 最小の操作面 |

完了コントラクトは `success | failure | needs_human` の 3 値に縮小する
(`needs_plan` / `decompose` は plan 層とともに休眠)。

### 削るもの(→ 休眠 ADR)

各機構は削除するが、対応する ADR は**失敗モード・ライブラリ**として保存する。
再導入条件は「その動機となった失敗がミニマム構成で再観測されること」。

| 削る機構 | 主なコード | 休眠 ADR |
|---|---|---|
| plan/spec パイプライン | planner / spec_worker / spec_fixer / plan_handoff / decompose_materializer | 0008, 0010, 0013, 0014, 0016(decompose) |
| レビュー機構一式 | self_review / pr_reviewer / findings ledger / review lane | 0004, 0006, 0011(combined), 0022, 0023, 0025, 0026(cost×catch) |
| fixer 一族 | fixer / ci_fixer / conflict_resolver / backoff | 0007, 0021 |
| auto-merge とマージ tail 監視 | arm / update-branch / MERGE_TAIL_OBSERVE_QUERY | 0003, 0009(auto-merge) |
| routing / escalation / drift | routing.rs / escalation chain / canary / routing_drift | 0003(routing), 0007(freshness), 0011(roles), 0013 |
| triage / cleaner | triage.rs / cleaner.rs | 0003(cleaner), 0006(triage), 0015, 0017 |
| schedule / cron / cadence / not-before | schedule.rs / cron.rs / cadence.rs | 0009(schedules), 0011(throttles), 0026(schedules) |
| collab advisor | collab.rs | 0006(collab), 0017(collab) |
| notify | notify/ | 0020 |
| daemon / launchd | daemon/(flock は watch に残す) | 0001(daemon) |
| managed clone / add-project / workspace | ensure_bare_clone ほか / workspaces | 0018, 0019, 0009(cross-repo) |
| 二層 config(repo 側 meguri.toml) | RepoManifest ほか | 0011(two-layer), 0015(repo reads), 0026 |
| agent skills 配布 / refine / gate probe | agent_skills.rs / refine.rs / gate.rs | 0009(skills), 0006(intake), — |
| launch mode(direct) | launch.rs(pane のみに) | 0012(launch) |
| sweep_health / body-edit 再注目 | sweep_health.rs / reconcile_body_edits.rs | 0009(body-edit) |

この表は例示であり、全 64 本の正式な kernel / dormant 分類は Phase 0 の
`docs/adr/STATUS.md` で確定する。特に ADR 0027(claim identity /
profile preflight)・0028(インフラ障害は needs_human ではない)・
0029(resume は会話可能セッションのみ)は削減後も生きる **kernel** に分類する。

規模の見立て: 単純なファイル単位合計では約 30,000 行だが、flow.rs /
issue_reconciler.rs / config.rs / gh.rs / store の内部からの削減が主体のため、
最終的な src は **20,000 行前後**を見込む(当初見立て 12〜15k は
「ループ本体ファイルが薄く、実体は共有基盤に分散している」事実により上方修正)。

## 権威反転(GitHub 依存の降格)

現行の github mode は「ラベルが唯一の真実源、DB ミラーなし」。これを反転する。

- **タスクのライフサイクル(queue / claim / needs_human / done)は sqlite が権威。**
  issue 番号は `tasks.origin = github:<N>` の外部参照に過ぎない。
- **GitHub への書き込み = 投影**(低頻度・有界): PR 作成、needs_human 時のコメント、
  完了時のコメント、人間向けラベル更新。書き込みはレート圧の主因ではない。
- **GitHub からの読み取り = edge シグナル拾いのみ**:
  - intake sweep: `meguri:ready` ラベルの issue を低頻度(既定 120s 目安)で列挙し、
    未 import(origin 一意)ならローカル task を生成する。これが唯一の定常 read。
  - マージ検出は API ではなく **git protocol**: fetch した default branch に対する
    `is_ancestor` で判定する(git 通信にレートリミットはない)。
  - CI rollup / review thread の観測はレビュー・fixer 機構とともに休眠。
- 完全ローカル(GitHub なし)でも全機能が回る。forge は optional のまま。

これにより毎 tick の GitHub read は構造的にほぼゼロになり、レートリミット問題は
「クエリ最適化」ではなく「権威の所在」で解決される。

## 実行計画

原則: **in-place の削減**(書き直しではない)。各フェーズで
`cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
`cargo nextest run` / `cargo test --doc` を通し、統合テストを同フェーズで刈り込む。
1 フェーズ = 1 PR、**人間レビュー必須**(再成長規律 5 の実践)。
コンパイラを削減装置として使う: enum variant(`Arm` / `Step` / `Flavor` 実装)を
消し、エラーを潰し切る。

### Phase 0: 凍結と規律の敷設

- **運転停止と排水**: launchd daemon を停止・uninstall する
  (plist が `~/.local/bin` の手動コピーを指す既知の問題があるため、
  `launchctl` 側の残骸まで確認する)。in-flight の run を完了させるか park し、
  `meguri prune` で pane / worktree を回収、`meguri:working` 等の残留ラベルを
  掃除する。**刈り込み作業自体は meguri のループでは行わない**(対話的に行う。
  再成長規律 5 の先行適用)
- 現 main に `pre-prune` タグ。参照用に凍結
- 本ドキュメントのレビュー・確定
- `docs/adr/STATUS.md` を新設し、全 ADR を `kernel` / `dormant` に分類
  (ADR 本文は編集しない)
- 既存 sqlite DB は互換維持しない(runs は ephemeral、pane は再生成可能)。
  schema を v1 に畳み直し、旧 DB は破棄する旨を明記

### Phase 1: 周辺系の枝落とし(~10.5k 行)

依存の葉から。notify / collab / cadence / cron / engine::schedule / refine /
agent_skills / gate / daemon / triage / cleaner / routing_drift /
reconcile_body_edits / sweep_health。
CLI から daemon / schedules / stats / top / agent-skills / why を削除。
config の対応セクション削除、doctor 縮小。

注: decompose_materializer / plan_handoff はここでは削らない。仕事を送る側の
planner より先に消すと「materialize されない decompose 提案」「handoff されない
merged spec PR」という壊れた中間状態を main に作るため、Phase 2 で planner と
同時に削る。

### Phase 2: plan/spec パイプライン(~4k 行)

planner / spec_worker / spec_fixer / decompose_materializer / plan_handoff を
一括で削る(送り手と受け手を同時に消し、壊れた中間状態を作らない)。
flow.rs の `Kind::Plan` 分岐、`on_needs_plan` / `on_decompose`。
完了コントラクトを 3 値に縮小(turn/prompts.rs とプロンプト文言の更新)。
`meguri:plan` / `speccing` / `spec-*` 系ラベル廃止、`meguri add --plan` 削除。

### Phase 3: レビュー機構(~5k 行)

self_review / pr_reviewer。flow.rs の STEP_SELF_REVIEW と ledger 系
checkpoint フィールド。escalation.rs を「needs_human を上げる + 投影コメント」
だけに縮小(profile escalation ladder 削除)。panes.lane は author のみに。

### Phase 4: fixer 一族と auto-merge(~5k 行)

fixer / ci_fixer / conflict_resolver。issue_reconciler.rs の PR 側
Snapshot / next_step から fixer 系 `Arm` と auto-merge 系 `Op`
(ArmAutoMerge / MergePr / UpdateBranch)を削除。forge から auto-merge・
update-branch・マージ tail GraphQL(gh.rs の観測クエリ群)を削除。
pr_is_touchable / reconciler_backoff 削除。

**ただし「merged/closed の観測 → reaper::finalize」の薄い経路だけは残す。**
これを先に消すと、Phase 5 の git ベース検出が入るまで pane/worktree の回収と
task 完了の連鎖が止まる。Snapshot の完全解体は Phase 5 で置き換えと同時に行う。

### Phase 5: 権威反転(唯一コードが増えるフェーズ、正味 ±0)

- `LocalTaskSource` を全モードの権威に。`LabelTaskSource` を廃止し、
  薄い `GithubIntake`(ラベル → ローカル task import、origin 一意で冪等)に置換
- マージ検出を git fetch + `is_ancestor` に切替。検出時に task を done へ、
  投影として issue コメント・close(best-effort)
- needs_human / 完了の投影書き込みを整理

### Phase 6: routing / config / store の残渣掃除(~6.5k 行)

- routing.rs / launch.rs 削除。profile は「default + 名前付き profile、
  project ごとの override 1 段」だけ config に残す(異種モデル運用は維持)
- config.rs を核の表面積まで畳む(目安 4,374 → ~1,200 行)。
  二層 config 廃止に伴い、各リポジトリの `meguri.toml` にある `check_command` 等は
  host `config.toml` の `[[projects]]` へ移す(移行メモを残す)
- store schema v1 化(runs / turns / events / panes / tasks +
  schema_migrations のみ)。migrations 履歴を初期化
- app.rs / main.rs / doctor の整理
- `.claude/rules/overview.md` を核の記述に書き換え

### 統合テスト

`tests/` は各フェーズで対応シナリオを削除し、核シナリオを残す:
ブロックダイアログ処理・虚偽申告の訂正・validation feedback・crash recovery。
Phase 5 で新規追加: intake import の冪等性、git ベースのマージ検出 → task close。

## 再成長の規律

刈り込み後の膨らませ方のルール。これ自体が今回の失敗の再発防止である。

1. **ADR は失敗モード・ライブラリ。** dormant な機構は、その動機となった失敗が
   ミニマム構成で再観測されたときに初めて再導入候補になる
2. **証拠ゲート。** 新機構は event log か実体験で **2 回観測された問題**にしか
   紐づけない。もっともらしい改善案は却下
3. **出生時に削除条件を宣言。** 再導入 ADR には「このメトリクスがこうなったら
   消す」を書いてから入れる
4. **直列に膨らませる。** 機構は一度に一つ。実運用で人間が価値を確認してから次
5. **dogfooding の役割限定。** worker turn の実行には使い続けてよいが、
   meguri 自身への機構追加の判断・マージは人間ゲート必須

## 未確定の判断事項(Phase 5 着手前に人間が決める)

権威反転は「人間の操作面」を変える。以下は計画として未確定であり、
Phase 5 の設計時に明示的に決める:

1. **`meguri add` の投影挙動**: github プロジェクトへの add は
   (a) ローカル task のみ作る、(b) 投影として GitHub issue も起票する、のどちらか。
   現行は issue 即起票。共有面としての issue を残すなら (b) だが、書き込みは増える
2. **GitHub 側の制御ラベルの扱い**: 権威反転後、`meguri:hold` / `needs-human` 等の
   GitHub 側操作は一次的な制御手段ではなくなる。intake sweep が同じ list 呼び出しで
   hold 系も拾って同期するか、制御は CLI(`meguri pause` 等)一本に寄せるか。
   後者はスマホから issue 操作で止められる現行の運用性を失う
3. **needs_human の通知経路**: notify を削るため、foreground watch のログと
   投影コメント(GitHub 通知)だけになる。無人運転を始める際に不足するようなら、
   notify の再導入は証拠ゲートを通す(ADR 0020 dormant)

## 明示的な判断事項(確定済み)

- 書き直しではなく in-place 削減(統合テストと失敗由来のコードパスを保存するため)
- preflight は残す(初回ゲート問題は複数回実観測済みで、証拠ゲートを満たす)
- gate.rs(doctor の PTY probe)は削る(診断専用)
- mux は tmux / herdr 両方残す
- auto-merge は削る。マージは人間が行い、検出は git protocol で行う
- DB は非互換リセット(個人ツールであり runs は ephemeral)
