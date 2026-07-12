# issue-92 spec — issue / pane / claude session を一つの寿命に統一する

meguri の設計の核は「1 issue = 1 pane = 1 live claude session、close まで維持」(#13)のはずだが、実装を実測すると鍵が割れ(PR 番号鍵の loop が別 pane を作る)、resume は配線が切れて一度も動いておらず、loop ごとの寿命はコードに散っている。この spec は issue #92 の 3 軸 — **(a) issue 番号で全部を束ねる (b) どこで落ちても resume できる (c) loop 別寿命の明文化** — を、どう実装するかに収束させる。

土台となる設計判断(issue が寿命の単位、pane 鍵は `(project, issue, lane)`、resume の真実は `panes.agent_session_id`)は ADR 0004 に切り出した。この spec は使い捨ての実装計画である。

## 1. canonical issue 解決 — `Target.issue_number` は常に GitHub issue 番号

PR を対象にする loop(reviewer / fixer / conflict_resolver / **ci_fixer**)が `Target.issue_number` に PR 番号を入れているのが鍵割れの根本原因。issue 本文の表には ci_fixer がない(#74/#80 で issue 起票後に追加されたため)が、fixer と同じく issue branch を編集する loop なので **本 spec は ci_fixer も統一対象に含める**。

解決順序:

1. **branch 復元** — `gitops::issue_from_branch(pr.head_branch)`(`meguri/<issue>-…`)。fixer / ci_fixer / conflict_resolver は `MEGURI_BRANCH_PREFIX` を要求済みなので、この経路で常に決まる。
2. **PR 本文の `Closes #N`** — reviewer のみ必要な fallback(人間が自分の PR に `meguri:spec-reviewing` を貼るケース)。既に取得済みの PR body を正規表現で読むだけで、新しい forge API は増やさない。
3. **どちらも失敗** — reviewer は PR 番号をそのまま鍵に使う(現状と同じ劣化モード)。event で `canonical_issue.unresolved` を emit して観測可能にする。

実装:

- `src/engine/mod.rs` に共通ヘルパー `canonical_issue(pr: &PullRequest) -> Option<i64>`(上の 1→2 の順で解決)と、**鍵そのものを返す `canonical_key(pr: &PullRequest) -> i64`**(`canonical_issue(pr).unwrap_or(pr.number)` — 劣化モードを含めた最終的な鍵)を置く。各 loop の `discover` は `Target { issue_number: canonical_key(&pr), … }` を返す。
- run が issue 番号鍵になるので、`prepare_work` は PR を再解決する必要がある。再解決の照合は discover と**同じ式 `canonical_key(&pr) == run.issue_number`** で行い、構成上ズレないようにする(`issue_from_branch` 単独での照合にすると、`Closes #N` fallback で解決した PR と劣化モードの PR を再発見できず退行する):
  - fixer / ci_fixer / conflict_resolver — `open_pr_for_issue(deps, issue)`: `list_open_prs` から `canonical_key` 照合で引く(spec_worker の `spec_ready_pr` と同型。これらの loop は meguri branch 必須なので実質 branch 復元で決まる)。
  - reviewer — 照合対象を `list_prs_with_label(spec-reviewing)` にして同じ式で照合。fallback 解決の PR(非 meguri branch + `Closes #N`)も劣化モードの PR(鍵 = PR 番号)も同じ式で再発見でき、新しい forge API は不要。
  - 同一鍵に複数の open PR がヒットしたら skip(benign race 扱い、escalate しない)。
- PR 番号は checkpoint に載せて以降の forge 呼び出しに使う。fixer は `cp.pr_number` を既に持つ。conflict_resolver / ci_fixer も `cp.pr_number` を埋める。reviewer は `ReviewCheckpoint` に `pr_number: i64` を追加し、`pr_diff` / `pr_comments` / `comment_pr` / label 操作 / `escalate_on_pr` を全部 `cp.pr_number` 経由にする(`run.issue_number` を PR 番号として使う箇所を一掃)。
- run 失敗時の escalation で `cp.pr_number` がまだ埋まっていない場合(prepare_work 自体の失敗など)は、canonical issue(`run.issue_number`)へ **issue API で** escalate する。GitHub の issue コメント / ラベル API は PR 番号にも効くので、劣化モード(鍵 = PR 番号)でも通知は届く。

副作用として受け入れるもの:

- `runs.issue_number` の意味が変わるため、`succeeded_run_count` / `issue_has_succeeded_run` の既存カウント(PR 番号鍵)は新しい鍵では見えなくなる。in-flight の PR が再発見されうるが、reviewer の head-sha marker・fixer の thread reply marker・conflict_resolver の mergeable 判定が二重作業を防ぐ(Authority は forge 側にある)。runs テーブルのデータ移行はしない。
- conflict_resolver の `MAX_RESOLVE_RUNS` は「PR ごと」から「issue ごと」になるが、meguri の PR は 1 issue = 1 branch = 1 PR なので実質同じ。

## 2. pane 鍵を `(project, issue, role)` へ

### migration `0005_pane_role.sql`

SQLite は PK を ALTER できないのでテーブルを作り直す: 新 `panes` を PK `(project_id, issue_number, role)`・`role TEXT NOT NULL DEFAULT 'author'` で作成し、既存行を `role = 'author'` で移送して rename。旧 fixer/reviewer が PR 番号で作った行はそのまま author 行として残るが、dead-pane スイープが順次回収するので実害はない。

role の値は `'author'` と `'review'` の 2 つ。cleaner は lane モデル外(現状維持)だが、行としては author を使う — レポート issue は cleaner しか触らないので衝突しない。lane を 3 つ以上に一般化するのは将来(#54 系)。

### store API — `src/store/panes.rs`

`get_pane` / `upsert_pane` / `save_pane_session` / `mark_pane_reclaimed` に `role: &str`(または `Lane` enum)を追加。`panes_for_issue` は role を含めて返し、`list_panes` はそのまま(行に role が増えるだけ)。session id のクリア用に `save_pane_session` を `Option<&str>` 化するか `clear_pane_session` を足す(resume 即死時に必要、§3)。

### lane の決定 — `src/engine/mod.rs`

`loop_kind → role` の対応は静的: `reviewer → review`、それ以外 → `author`。`run.loop_kind` から引ける関数を 1 つ置き、`ensure_pane` / `finish_pane` / `release_pane` / reaper がそれを使う。

author lane の共有で fixer / ci_fixer / conflict_resolver が同一 issue を並行して掴む混線は新たな手当て不要: 各 loop は discover と prepare の両方で `meguri:working` ラベルを確認してから claim するため(fixer の `pr_is_fixable` + claim 前チェックほか各 loop 同様)、同じ PR を触る仕事は forge 上で直列化されており、同一 pane に複数 run の turn が混ざることはない。

### reviewer の worktree を issue 単位に固定

現状の review worktree は `review-<pr>-<run.id>` で run ごとに移動するため、`ensure_pane` の「worktree moved」分岐が毎回 pane を殺し、round を跨いだ session 継続が構造的に不可能。ディレクトリを **`review-<issue>` に固定**し、`gitops::create_review_worktree` を「既存なら `fetch` + `reset --hard <head_sha>`(+ `clean -fd`)で新しい head に付け替える」よう拡張する。同一 issue の並行 review は `runs_active_target` unique index が既に防いでいる。

### CLI — `meguri attach <issue> [--review]`

`resolve_attach_pane`(`src/app.rs`)を issue 起点に: 既定は author lane、`--review` で review lane。run id を渡した場合は run の loop_kind から lane を導いてその pane を引く。`panes_for_issue` の単一マッチ判定は `(project, role)` 単位に更新。

## 3. session id は③(ファイル走査)を主経路に、resume は panes から読む

- **書く側** — `record_agent_session`(`src/engine/flow.rs`)を変更: turn 完了ごとに `agent_session::latest_session_id(session_root, worktree)` を主経路として引き、**`panes.agent_session_id`(issue + role 鍵)へ upsert** する。ファイル走査が空振りしたときのみ result file の自己申告 → mux(herdr)の順で fallback。`runs.agent_session_id` への書き込みは観測用に残してよいが、resume はもう読まない。
- **読む側** — `ensure_pane` の resume 分岐を `runs.agent_session_id` → **`panes.agent_session_id`** 参照に切替。`upsert_pane` は reclaim/respawn を跨いで session id を保持する既存挙動のまま。
- **即死フォールバック** — resume spawn した pane が `RESUME_PROBE` 内に死んだら panes 側の id をクリアして fresh spawn(既存挙動の移植)。`pane.resume_failed` event はそのまま。
- **reaper** — `release_pane_record` の走査は最後の保険としてそのまま残す(turn 経路が主、reclaim 前が保険)。

これで「idle 中に pane が死んでも、次にどの lane の loop が触れた瞬間に同じ session へ `--resume` で戻れる」が全 loop で成立する。worker → fixer のような branch 継続はそもそも同一 pane を adopt し、planner(新 branch)→ worker(別 branch)のような worktree 移動時も、保存済み session id による resume で文脈が継がれる。

## 4. 罠の掃除

- **`keep_pane` の値検証** — `Config::load` 時に `"until-issue-closed"` / `"never"` 以外をエラーで弾く(`src/config.rs`)。`on-failure` セマンティクスの実装はしない(必要になったら別 issue)。doc コメントの「Any other value is treated as …」も削除。
- **reaper の回収 reason** — `PaneCandidate` に reason を持たせ、dead-pane 起因は `"pane-dead"`、issue close 起因は `"issue-closed"` を `pane.reclaimed` event に emit(`plan_panes` で判定済みの情報を `reclaim_panes` まで運ぶだけ)。

## 5. loop 別寿命の明文化

以下の表を README(en/ja)の該当節と各 loop のモジュール doc コメントに反映する(ci_fixer 行を追加した以外は issue 記載の表のまま):

| loop | trigger | 鍵 | worktree | 正常終了 | pane 後始末 |
|---|---|---|---|---|---|
| planner (author) | `meguri:plan` issue | issue | 新 branch | spec PR 作成 → `spec-reviewing` | keep |
| reviewer (review) | `spec-reviewing` PR / head 未レビュー | **issue + `review`** | read-only detached(`review-<issue>` 固定) | clean → `spec-ready` / findings → 据置 | keep(独立) |
| spec_worker (author) | `spec-ready` PR | issue(branch 復元) | 既存 branch を継ぐ | 実装 → PR 更新 | keep・author pane を継ぐ |
| worker (author) | `meguri:ready` issue | issue | 新 branch | PR `Closes #N` | keep |
| fixer (author) | PR の未解決スレッド | **issue へ統一** | PR head に attach | スレッドに再 review 依頼返信 | **author pane を継ぐ** |
| ci_fixer (author) | meguri PR の CI 赤 | **issue へ統一** | PR head に attach | fix push(≤3 round) | **author pane を継ぐ** |
| conflict_resolver (author) | PR が Conflicting(≤3) | **issue へ統一** | PR head に attach | base merge & 解消 → push | **author pane を継ぐ** |
| cleaner (standalone) | レポート issue + 既定 branch 前進 | レポート issue | read-only detached | レポート issue 再生成 | 自前回収 |

「1 issue = 1 pane」と書いてある箇所(`src/store/panes.rs` / `src/store/migrations` / `src/engine/flow.rs` / `src/engine/scheduler.rs` / `src/app.rs` / README×2)は「1 issue = author lane 1 pane + review lane 1 pane」に言い換える。

## 触るファイル

- `src/engine/mod.rs` — `canonical_issue` / `open_pr_for_issue` / `loop_kind → role` の共通化
- `src/engine/reviewer.rs` / `fixer.rs` / `ci_fixer.rs` / `conflict_resolver.rs` — discover を issue 鍵に、`cp.pr_number` 経由の forge 呼び出しへ
- `src/store/migrations/0005_pane_role.sql` / `src/store/panes.rs` — PK に `role`、API 拡張
- `src/engine/flow.rs` — `ensure_pane`(role 鍵 + panes 参照 resume)、`record_agent_session`(③主経路で panes へ)、`finish_pane`(role)
- `src/engine/reaper.rs` — role 対応、reason の実態化
- `src/gitops.rs` — `create_review_worktree` の head 付け替え対応
- `src/config.rs` — `keep_pane` 検証
- `src/app.rs` / `src/cli.rs` — `meguri attach <issue> [--review]`
- `README.md` / `README.ja.md` / 各 loop の doc コメント
- テスト: `tests/pane_lifecycle_test.rs` / `resume_test.rs` / `reaper_test.rs` / `reviewer_test.rs` / `fixer_test.rs` / `ci_fixer_test.rs` / `conflict_resolver_test.rs`

## 受け入れ基準

1. **鍵の統一** — worker が issue N で成功した後、同じ issue の PR に findings が付いて fixer が走ると、fixer は worker と**同一の pane** を adopt する(spawn しない)。conflict_resolver / ci_fixer も同様。統合テストで pane id の一致を検証。
2. **reviewer の独立と束ね** — reviewer は `(project, issue, review)` 鍵の別 pane を持ち、同じ PR の第 2 round でも同一 pane / 同一 worktree(head は新 sha に更新済み)を使う。
3. **resume の成立** — turn 完了後に `panes.agent_session_id` がファイル走査由来の id で埋まる。pane を kill した後の次の run は `--resume <id>` 付きで spawn する(`resume_test.rs` に fake session transcript を置いて spawn コマンドを検証)。resume 即死時は id がクリアされ fresh spawn に落ちる。
4. **attach** — `meguri attach <issue>` が author pane、`meguri attach <issue> --review` が review pane に繋がる。
5. **keep_pane 検証** — `keep_pane = "on-failure"` など未知値で `Config::load` がエラーになる。
6. **reaper reason** — dead-pane 回収の `pane.reclaimed` event が `"pane-dead"`、issue close 回収が `"issue-closed"` を運ぶ。
7. `cargo nextest run` が全 green(migration の後方互換テスト含む: 0004 時代の panes 行が role='author' で読めること)。

## スコープ外 / 別 issue

- reviewer の human gate(`spec.auto_approve`、findings/clean の park + `awaiting_human` + notify)→ **#83**(この土台の上に乗る)
- 実装 diff への AI レビューループ新設 → #84
- lane の 3 つ以上への一般化、リモート DB マルチホスト → 将来(#54 系)
- `keep_pane = "on-failure"` の実装(本 spec は未知値を弾くまで)
