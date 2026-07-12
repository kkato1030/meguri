# issue-108 spec — AI 実装レビューを「内部ループ」にする + 命名の対称化(spec_reviewer / impl_reviewer)

## 一行

AI 実装レビューを、forge を transport/state に使う**外部ループ**(現 `impl-reviewer` `Loop`)から、
worker run の worktree 内で review→fix を回し **forge に一切触らない内部ループ**へ作り替える。
あわせて `reviewer` → `spec_reviewer`、`impl_reviewer` を内部ループ実装へ再分類して命名を対称化する。

設計判断(なぜこの形か)は **ADR 0006**(本 PR 同梱、ADR 0004 を置換)に置いた。この spec は
「どこを・どう触るか」に絞る使い捨ての足場。

## 決定した設計(実装の骨子)

### A. self-review フェーズを共有フローに挿す(`engine/flow.rs`)

- `STEP_SELF_REVIEW`(文字列 `"self-review"`)を `STEP_VALIDATE` と `STEP_OPEN_PR` の間に追加。
  `drive()` のステップ連鎖と `save_step` の遷移をこの順に更新する。
- `Flavor` に `fn self_reviews(&self) -> bool { false }` を追加。`WorkerFlavor` だけ `true`。
  false のフレーバは self-review をスキップして従来どおり `open-pr` へ。
- `Checkpoint` に内部ループの state を追加(**local only、forge には出さない**):
  - `self_review_rounds: u32`(消費ラウンド数)
  - `self_review_pending: Vec<Finding>`(review turn が出し fix turn がまだ潰していない findings)
  - `self_review_unconverged: bool`(上限到達で未収束のまま publish した印 — フッタ 1 行の根拠)
- 収束条件と上限: 各ラウンドで review turn → verdict。`clean` なら抜ける。`findings` なら
  fix turn → commit → `validate` を再実行 → 次ラウンド。`rounds >= max_rounds` に達したら
  `self_review_unconverged = true` を立てて **block せず** `open-pr` へ進む(0006: レビュー済み
  PR が人間ゲート前で開くのは正常)。ラウンド 0(diff 皆無)や初回 clean は即通過。
- 中断・再開は既存ステップと同型: 各 turn 後に `save_step(STEP_SELF_REVIEW, cp)` して
  ラウンドカウンタと pending を checkpoint に残す。resume は checkpoint から続きを回す。
- フッタ: `self_review_unconverged` のとき `compose_pr_body`(`engine/flow.rs`)が PR 本文末尾に
  「🔁 self-review: N ラウンドで未収束のまま公開」相当の **1 行**だけ足す。往復トランスクリプトは
  載せない。加えて run イベント(例 `self_review.unconverged`)を emit。

### B. `impl_reviewer.rs` を内部ループ実装に作り替える

- `ImplReviewerLoop` と `impl super::Loop`、`discover` / `run_impl_reviewer` / `drive` の
  スケジュール機構を撤去。`default_loops()`(`engine/mod.rs`)から `ImplReviewerLoop` を外す。
- forge 投稿系を**全削除**: `create_pr_review` 呼び出し、`marker_comment` / `review_body` /
  `fallback_comment`、head-sha マーカー(`impl_review_marker` / `head_already_reviewed` /
  `review_rounds`)、`settle` / `escalate_on_pr` / `prepare_work`(PR claim)/ CI・thread 判定。
- 残す・再構成する中核: review turn(ローカル diff を読み `{verdict, findings[]}` を書く)と
  fix turn の駆動、findings のパース/検証(`read_review` 相当、NEW 側 anchor 要件)、ラウンド機構。
  これらを flow の self-review フェーズから呼べる関数として公開する
  (例 `pub(crate) async fn review_turn(...) -> ReviewOutcome` / `fix_turn(...)`)。
- review turn が読む diff は `git diff <base>...HEAD` のローカル生成(`gitops`)に切り替える
  (base = `Flavor::verify_base` = 既定ブランチ)。`forge.pr_diff` は使わない。findings は
  checkpoint 経由でメモリ内を渡り、GitHub には出ない。

### C. `reviewer.rs` → `spec_reviewer.rs` に改名

- ファイル・モジュール名を `spec_reviewer` に。`engine/mod.rs` の `pub mod reviewer;` を追随。
- `KIND` を `"reviewer"` → `"spec-reviewer"` に。`role_for_loop`(`engine/mod.rs`)の
  `loop_kind == reviewer::KIND` 判定を追随(ROLE_REVIEW は不変)。`ReviewerLoop` /
  `run_reviewer` の名前も `SpecReviewer*` に揃える。`default_loops()` の登録を差し替え。
- 呼び出し元(`impl_reviewer.rs` のテストが参照する `reviewer::review_marker` 等)を追随。
  内部ループ化で impl 側のマーカー衝突テストは不要になるので併せて整理。

### D. routing の対称化(`routing.rs`)

- `KNOWN_ROLES`: `"reviewer"` → `"spec-reviewer"` に置換し、`"impl-reviewer"` を**追加**
  (内部ループの review turn 用に role を残す)。
- `recommended_chain`: `"reviewer"` の枝を `"spec-reviewer"` に改名。`"impl-reviewer"` にも
  クロスベンダ寄りの枝(例 `["codex", "claude-opus", DEFAULT_PROFILE]`)を与える。
- **deprecated alias**: 旧 config キー `reviewer` を `spec-reviewer` として吸収する。
  `[routing.roles] reviewer = ...` を書いた既存 config が壊れないよう、resolve/validate の
  role 突き合わせでエイリアスを正規化する。

### E. config `[review]` の読み替え(`config.rs`)

- `impl_enabled` → `enabled`、`impl_max_rounds` → `max_rounds` に改名し、self-review の
  「有効/無効」「ラウンド上限」として読む。`enabled = false` は「外部 bot がいるので自己
  レビューを切る」の意味(worker が self-review フェーズをスキップ)。
- 旧フィールド名は **serde alias** で受ける(`#[serde(alias = "impl_enabled")]` /
  `alias = "impl_max_rounds"`)ので既存 config は無改修で動く。既存テスト(`config.rs` の
  round-trip)を新名に更新。

### F. モデル分離(内部ループでも保つ)

review turn は routing role `impl-reviewer` の profile で解決し、fix turn は worker(author)の
profile で回す。profile は run 単位でピン留めされ、1 pane = 1 CLI プロセスなので、**review turn は
別 profile を使う以上、別 pane で走らせる**必要がある。採る形:

- review turn: HEAD の read-only sibling worktree(`gitops::create_review_worktree` 同型)を
  worker run の worktree の隣に作り、専用 lane(新 `ROLE_IMPL_REVIEW` 相当)の pane で
  `impl-reviewer` profile を使って走らせる。出力は `.meguri/review.json`(NEW 側 anchor 付き
  findings)。
- fix turn: worker の author pane・author worktree でそのまま commit。
- これを可能にするため、`flow::run_turn` に **lane role + profile を明示指定**できる薄い変種
  (または `ensure_pane`/`resolve_run_profile` の role パラメタ化)を足す。self-review フェーズが
  review turn だけこの経路を使う。

## 触るファイル

- `docs/adr/0006-ai-implementation-review-is-an-internal-loop.md` — 新規(本 PR 同梱、0004 置換)。
- `docs/adr/0004-ai-review-covers-implementation-diffs.md` — ステータスを「置換済み」に(本 PR で対応済)。
- `src/engine/flow.rs` — `STEP_SELF_REVIEW` 追加、`Flavor::self_reviews()` フック、`Checkpoint` に
  ラウンド/pending/unconverged、self-review 駆動、`compose_pr_body` のフッタ、lane+profile 明示の
  turn 変種。
- `src/engine/impl_reviewer.rs` — `Loop`/forge 投稿系を撤去、内部ループの review/fix turn +
  ラウンド機構として再構成。ローカル diff 生成へ。
- `src/engine/mod.rs` — `default_loops()` から `ImplReviewerLoop` を外す、`pub mod reviewer` →
  `spec_reviewer`、`role_for_loop` の追随。
- `src/engine/worker.rs` — `WorkerFlavor::self_reviews()` を `true` に。
- `src/engine/reviewer.rs` → `src/engine/spec_reviewer.rs` — 改名、`KIND`/型名の追随。
- `src/routing.rs` — `KNOWN_ROLES`/`recommended_chain` の `spec-reviewer`・`impl-reviewer`、
  旧 `reviewer` キーの alias 正規化。
- `src/config.rs` — `[review]` を `enabled`/`max_rounds` に(旧名は serde alias)。
- `src/engine/fixer.rs` — doc コメント更新(AI review はもう thread を作らない旨)。
- `src/store/panes.rs` — review turn 用の lane role 定数(必要なら)。
- `README.md` / `README.ja.md` — impl reviewer の説明(§ AI review、ループ表、`[review]` 設定、
  routing 例、ループ数「9 → 8」)を内部ループ版に更新。
- 関連テスト(`flow`/`impl_reviewer`/`spec_reviewer`/`routing`/`config`/`worker` の各 `mod tests`)。

## 受け入れ基準(issue より)

1. worker run が `execute → validate → self-review → open-pr` を通り、self-review で **forge を
   一切呼ばない**(fake forge の呼び出し記録がゼロ)。
2. findings ありのとき同一 run 内で review→fix が回り、clean or ラウンド上限で publish に進む
   (上限未収束でも block しない)。
3. 公開される PR に AI 往復の thread / comment が **載らない**(要約フッタ 1 行のみ許容)。
4. `fixer` は人間・外部 bot の thread では従来どおり発火する(回帰なし)。
5. `impl-reviewer` role の profile 設定が internal review turn に効く。
6. 命名が `spec_reviewer` / `impl_reviewer` で対称になり、旧 `reviewer` routing キーが alias として動く。
7. `[review]` の旧キー(`impl_enabled` / `impl_max_rounds`)を書いた既存 config が serde alias で
   無改修動作する。

## 主要な決定と論点

- **ADR 番号**: プロンプトは `0005-...` を示唆するが、`0005-issue-labels-two-axis` が既に存在する
  ため次の空き番号 **0006** を採番した(リポジトリは番号重複を許容するが、実ファイル衝突は避ける)。
- **`KIND` 改名の後方互換**: `runs.loop_kind` の永続値 `"reviewer"` → `"spec-reviewer"`。アップグレード
  時に in-flight の reviewer run があると scheduler が unknown loop として警告し得るが、discovery は
  冪等なので次周回で再生成される(run は短命・単一利用)。許容し、必要なら resume 時のみ旧値を
  マッピングする。
- **review turn の pane 分離**: モデル分離(受け入れ 5)を満たすには review turn を別 profile =
  別 pane で走らせる必要がある。§F の read-only sibling worktree + 専用 lane を採る。MVP でここまで
  やる(profile が「効く」ことが受け入れ基準のため punt 不可)。
- **フッタの置き場**: 未収束の記録は PR 本文フッタ 1 行 + run イベントのみ。inline thread も
  往復ログも載せない(0006 の「PR 会話は人間・外部レビュー専用」)。
- **`enabled = false` の意味変更**: 旧「impl-reviewer ループのキルスイッチ」→ 新「worker の
  self-review フェーズをスキップ」。外部レビュー bot 併用時の逃げ道という役目は同じ。
