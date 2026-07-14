# issue #188: `spec_fixer` — guard(Plan) findings で park した spec PR を再駆動する

> このファイルは使い捨ての足場。実装が landしたら削除する（ADR 0001）。恒久的な設計判断は
> ADR 0012 に、対称化の背景は ADR 0008 にある。

## ゴール

guard(Plan) が findings を出して `meguri:spec-reviewing` のまま park した spec PR を、
人手なしで「修正 turn → push → 再 guard」に入れる。impl 側の `ci_fixer`（赤 CI）と対称な
plan 側の fixer 系ループ `spec_fixer` を追加する。

## スペック深度: normal

新しいスケジューラループを 1 つ足す変更だが、雛形（`ci_fixer` / `fixer`）が既にあり、
永続スキーマ・公開契約・不可逆操作には触れない（ラウンド計数は既存の `succeeded_run_count`
を再利用、新テーブルなし）。設計判断の芯（findings → 次 push で再 guard）は ADR 0008 で
確定済みで、本 issue はその「駆動主体の欠落」を埋める実装。よって normal。veto ルール
（永続状態・スキーマ・公開契約）に該当しないため migration/rollback セクションは不要。

## 何を作るか

### 1. 新ループ `src/engine/spec_fixer.rs`（fixer 系の雛形）

`ci_fixer.rs` を素直に写す。PR head に attach し、新 PR は作らない。

- `KIND = "spec-fixer"`、`MAX_SPEC_FIX_RUNS: i64 = 3`。
- **discover**: `deps.open_prs`（per-tick 共有キャッシュ）で open PR を走査し、
  - `meguri:spec-reviewing` を持つ PR に限る（= guard(Plan) の対象）。
  - 触れる状態か: hold / working / needs-human でないこと。head が `meguri/` であること。
    （`pr_is_touchable` はそのままでは spec-ready を見るだけなので、spec-reviewing 用の
    軽い判定で足りる。既存ガードの再利用可否は実装時に判断。）
  - 現在の head の `commit_status(head_sha, "meguri/guard-review")` が `Failure` のもの。
    `None`（未 guard、= 直近 push 済み）や `Pending` は拾わない。
  - `succeeded_run_count(project, "spec-fixer", issue) >= MAX_SPEC_FIX_RUNS` なら
    その場で needs-human に escalate して skip（`ci_fixer::escalate_budget_exhausted` と同型）。
  - target は canonical issue でキーする（planner author lane と同じ pane に載せるため）。
- **prepare_work**: canonical issue から PR を再解決し、`meguri:working` で claim。
  discover〜claim 間の状態変化（status が消えた/緑になった等）は benign race として skip。
  guard `<details>` の findings を checkpoint に取り込む（下記 2）。
- **prepare_worktree**: `flow::attach_pr_worktree`（PR head に attach、新ブランチは切らない）。
- **execute_prompt**: 「PR #N の spec/ADR を、guard の findings に沿って直す」。findings を
  本文から抜いて注入。push 禁止・ブランチ切替禁止（meguri が push する）。planner と同じく
  spec は使い捨て・durable value は ADR / ドメイン文書へ、の原則を再掲。
- **verify_work**: commit されていれば良い（真の判定は次の再 guard）。`verify_base` は
  PR ブランチの pushed tip（`ci_fixer` と同じ）。
- **settle_labels**: `meguri:working` を外し、best-effort の PR コメントを 1 本残す。
  ラベル遷移は不要（`spec-reviewing` のまま。新 head を guard が拾って再レビューする）。
- **sets_subject = false**（修正は PR タイトルを揺らさない、#136 と同じ）。
- **escalate**: 失敗時は PR に needs-human ラベル + コメント（`ci_fixer` と同型）。

### 2. findings の取り出し（`src/engine/guard.rs`）

guard は findings を PR 本文の `<!-- meguri:guard-review -->` 折り畳みに書く。
`spec_fixer` がここを読めるよう、`GUARD_BODY_MARKER` を pub にするか、本文から guard
ブロックを抜く小さなヘルパを `guard.rs` に足して再利用する。抜き出しが面倒なら本文全体を
プロンプトに渡すのも可（findings は本文内にある）。`GUARD_STATUS` は既に pub。

### 3. 登録とルーティング

- `src/engine/mod.rs` の `default_loops()` に `SpecWorkerLoop`/`GuardLoop` 近傍で登録する。
  優先順位は「park したパイプラインを解くために worker/planner より前」を推奨（要決定）。
  `role_for_loop` は guard 以外を `ROLE_AUTHOR` に落とすので **spec-fixer は自動で author
  lane** になる（criterion 2 を満たす）— 追加変更は不要。
- `src/routing.rs` の `routing_role_for_loop` に `"spec-fixer"` の arm を足す。spec/ADR の
  文章を書く性質から **`"planner"` プロファイル**を推奨（代替: fixer 系なので `"fixer"`）。要決定。

### 4. テスト

- `spec_fixer.rs` 内の unit test: discover 条件（spec-reviewing かつ head status=failure の
  ときだけ拾う / None・Pending・green は拾わない / needs-human・working・hold は skip）、
  ラウンド上限で escalate、プロンプトが findings を載せ push を禁じる、sets_subject=false。
- `FakeForge` に guard-review status と spec-reviewing PR を仕込む形。既存 `ci_fixer` /
  `guard` のテストを参考にする。
- 余力があれば `tests/*.rs` に `fake_agent.sh` を使った通し（findings → 修正 push →
  新 head → 再 guard → clean で spec-ready）を 1 本。

## 主要な決定（レビューで詰める）

1. **ラウンド計数**: 既存 `succeeded_run_count(project, "spec-fixer", issue)` を再利用（≤3）。
   status 履歴や hidden marker や新テーブルは使わない。→ ADR 0012 で確定。
2. **収束/dedup**: head sha が dedup キー。push 後の新 head は guard status 未貼りなので
   spec_fixer は再発火しない。marker 不要。→ ADR 0012 で確定。
3. **ルーティングロール**: `spec-fixer` → `planner`（推奨）か `fixer`。**要決定。**
4. **`default_loops()` の優先順位**: どこに挿すか。**要決定**（推奨: plan 側 fixer として
   `spec_worker`/`guard` 近傍、`worker`/`planner` より前）。
5. **触れる状態の判定**: `pr_is_touchable` を再利用するか、spec-reviewing 用の軽い判定を
   spec_fixer 内に置くか。**要決定**（spec-reviewing PR は spec-ready を持たないので
   `skip_spec_ready` は無関係）。
6. **guard.plan が OFF のとき**: そもそも spec-reviewing PR も guard status も生まれないので
   discover は自然に空。明示 early-return を足すかは任意。

## 受け入れ基準

1. guard(Plan) が findings を出した spec PR が、人手なしで次 poll 以内に修正 turn → push →
   再 guard に入る。
2. 修正は planner と同じ author pane/session を継続する（canonical issue キー + ROLE_AUTHOR）。
3. ラウンド上限（≤3）超過で `meguri:needs-human` + 通知（#153 の awaiting_human と整合）。
4. combined/separate 両モードで動く（`spec-reviewing` は spec-ready 分岐より前なので
   delivery mode 非依存 — 確認済み）。

## 触るファイル

- `src/engine/spec_fixer.rs`（新規）
- `src/engine/mod.rs`（`default_loops()` に登録）
- `src/routing.rs`（`routing_role_for_loop` に `spec-fixer`）
- `src/engine/guard.rs`（`GUARD_BODY_MARKER` を pub / findings 抽出ヘルパ）
- `docs/adr/0012-spec-fixer-drives-plan-guard-findings.md`（本 issue で追加済み）
- `docs/specs/issue-188.md`（本ファイル、実装時に削除）

## 関連

- ADR 0006（guard は inline を出さない）/ ADR 0008（対称化: findings は次 push 待ち）/ ADR 0012（本件）
- #153（park の人間通知 — 補完関係）/ #183（同種「復帰経路の欠落」バグの先例）
