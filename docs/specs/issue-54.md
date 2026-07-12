# issue-54 spec — label を使わないローカル/サイレントモード: TaskSource 抽象化

meguri のワークフロー状態は今、すべて GitHub ラベルの上に載っている。`meguri:ready` がキューで、`meguri:working` がロックで、`meguri:needs-human` がエスカレーションだ。よくできた仕組みだが、これは「リポジトリのラベルを自由に触れる」という前提の上でしか成立しない。触りたくない・触れないリポジトリで meguri を使うには、この調整レイヤーをラベルから引き剥がして差し替え可能にする必要がある。それが本 issue の核心で、将来のリモート DB マルチホスト対応もこの同じ差し替え口から入ってくる。

issue 本文に全体設計(Phase 1〜4)が既にあるので、この spec の仕事は (1) 設計を現行コードに正確に対応付けること、(2) 契約(トレイトとスキーマ)を確定させること、(3) **このブランチで実装する範囲を Phase 1 に確定させること**。長生きすべき語彙とアーキテクチャ決定は ADR 0003 に置いた。

## 現行コードの対応表: ラベルの 5 役割はどこにあるか

| 役割 | 実装箇所 |
|---|---|
| タスクキュー (discover) | `flow::discover_by_label` (`src/engine/flow.rs:190`)、worker/planner の `discover` |
| 排他制御 (claim) | `flow::claim_issue` (`flow.rs:463`) の `meguri:working` 付与 + DB 側 `runs_active_target` unique index |
| タスク本文 | `claim_issue` が `Checkpoint.issue_title/issue_body` に写す |
| エスカレーション | `flow::escalate_on_forge` (`flow.rs:430`) + `Flavor::release_claim` |
| 完了判定 | worker の `settle_labels`、reaper の `forge.issue_state` (`reaper.rs:128`) |

これらはすべて `Deps.forge: Arc<dyn Forge>` 経由で、`Forge` トレイト(`src/forge/mod.rs:135`)は 22 メソッドの一枚岩。ここから調整レイヤーだけを `TaskSource` として切り出す。

## `TaskSource` トレイト(契約の確定)

```rust
/// タスク調整レイヤー: discover / claim / release / escalate / complete。
/// claim はアトミック(他ホストと競合しても高々一者が勝つ)であることが契約。
/// 実装: LabelTaskSource(現行ラベル動作)/ LocalTaskSource(sqlite)。
#[async_trait]
pub trait TaskSource: Send + Sync {
    /// kind("work" | "plan")のアクション可能なタスクを列挙する。冪等。
    /// claim 可能状態(queued / needs_human)を返す。needs_human を含めるのは
    /// ラベル版の再エスカレーション動作(トリガーラベルが残り再 discover に載る)と同型にするため。
    async fn discover(&self, kind: TaskKind) -> Result<Vec<Task>>;
    /// 単一のアトミック操作としてタスクを claim する。None は良性の競合
    /// (他者が先に取った・対象でなくなった)で、run は Skipped で終わる。
    async fn claim(&self, key: &TaskKey, host: &str) -> Result<Option<Task>>;
    /// claim を手放す(`meguri stop` / needs-plan 降格)。
    async fn release(&self, key: &TaskKey) -> Result<()>;
    /// 人間に引き渡す。reason は耐久保存される(ラベル+コメント | status+reason)。
    async fn escalate(&self, key: &TaskKey, reason: &str) -> Result<()>;
    /// 成果物が出た。github: トリガー/working ラベル除去、local: status='done'。
    async fn complete(&self, key: &TaskKey) -> Result<()>;
}

/// task の同一性。github モードでは DB 行を作らず issue 番号がそのまま key
/// (ラベルが唯一の真実のまま)。local/silent では tasks 行の rowid。
pub enum TaskKey { Issue(i64), Local(i64) }

pub struct Task {
    pub key: TaskKey,
    pub kind: TaskKind,   // Work | Plan
    pub title: String,
    pub body: String,
    /// silent モード用: local タスクが指す issue 番号(origin github:<N>)。
    pub issue: Option<i64>,
}
```

シグネチャ上の要点:

- **claim-by-key であって claim-next ではない。** 現行の scheduler は「discover → target ごとに run 作成 → drive 先頭で claim」という構造で(`scheduler.rs:83-121`)、これを保つ。アトミック性は claim 単体が担保する: sqlite では `UPDATE tasks SET status='claimed', claimed_by=?, reason=NULL, ... WHERE id=? AND status IN ('queued','needs_human')` の affected rows 判定、Phase 4 の Postgres では同型の `UPDATE ... WHERE status IN ('queued','needs_human') OR lease_until < now() RETURNING *`。lease 失効条件を足すだけで契約は変わらない。
- **claim 可能条件に `needs_human` を含める(ラベル同型)。** ラベル版で再 claim が needs-human を解除できるのは、`escalate_on_forge`(`flow.rs:430`)が `meguri:working` だけ外してトリガーラベルを残し、エスカレーション済み issue が再 discover に載り `claim_issue` が needs-human を除去するからだ。local でこれと同型にするため、claim の WHERE は `status IN ('queued','needs_human')` とし、claim 成功時に `reason` をクリアする(受け入れ基準 5)。`claimed` なタスクは対象外なので、二重 claim は None のまま(受け入れ基準 3)。Phase 4 の lease 失効はこの条件の自然な拡張として乗る。
- `claim` が `host` を取るのは Phase 4 の布石(ローカルでは固定値)。`Option<Task>` の None は既存の `PreparedWork::Skip` に 1:1 で写る。
- `Forge` には issue 読み取り(silent 用)・PR 操作・レビュースレッド・blocked_by が残る。`LocalForge` で issue を偽装する案は採らない(issue 本文どおり)。

## `tasks` テーブルと runs の移行(migration 0004)

```sql
CREATE TABLE tasks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project_id TEXT NOT NULL,
  kind TEXT NOT NULL DEFAULT 'work',          -- work | plan
  title TEXT NOT NULL,
  body TEXT NOT NULL DEFAULT '',
  origin TEXT NOT NULL DEFAULT 'local',       -- 'local' | 'github:<N>'
  status TEXT NOT NULL DEFAULT 'queued',      -- queued|claimed|done|needs_human|cancelled
  reason TEXT,                                -- needs_human の理由
  claimed_by TEXT,                            -- host_id。Phase 1 では固定値
  lease_until TEXT,                           -- Phase 1 では NULL(無限)
  created_at TEXT NOT NULL
);
```

`claimed_by` / `lease_until` は Phase 1 では実質未使用だが、スキーマに最初から持たせて Phase 4 の手戻りを防ぐ(issue 本文どおり)。

**runs は issue の言う「`issue_number` を `task_id` に置き換え」を字義どおりにはやらない。** github モードは tasks 行を持たない(ミラー行はラベルとの二重状態になり Authority 原則に反する)ので、置き換えると github run の対象が表せなくなる。代わりに:

- `runs` に `task_id INTEGER` を追加し、`issue_number` を NULL 許容にする(sqlite なのでテーブル再作成 + データコピーの定石で)。github run は従来どおり issue_number、local run は task_id、silent run は両方を持つ。
- unique index は 2 本の部分インデックスに分ける: `runs_active_issue (project_id, loop_kind, issue_number) WHERE active AND issue_number IS NOT NULL` と `runs_active_task (project_id, loop_kind, task_id) WHERE active AND task_id IS NOT NULL`。既存データはそのまま前者に載る。ここで `active` は既存 `runs_active_target`(`0001_init.sql:33`)と同じ `status IN ('queued','running','interrupted')` の略記であり、migration 実装ではこの述語をそのまま両インデックスの `WHERE` に展開する(`active` という列やビューは無い)。

ブランチ名は origin で分ける: github 起源は従来の `meguri/<issue>-<slug>-<hash>` を維持(`gitops::issue_from_branch` や既存ブランチとの互換を壊さない)、local タスクは `meguri/t<task_id>-<slug>-<hash>`。`gitops` に `task_from_branch`(`t` プレフィックス判定)を足す。Phase 4 の再 claim 時は `meguri/t<id>-*` のリモートブランチ prefix 一致で引き継ぎを検出する(hash は run 由来なので一致ではなく prefix で見る)。

## config: `mode` と `deliver`

```toml
[[projects]]
id = "work"
repo_path = "/path/to/repo"
mode = "local"        # "github"(デフォルト) | "silent" | "local"
deliver = "branch"    # "pr"(デフォルト) | "branch" | "patch"
```

- `repo_slug` を `Option<String>` にする。ロード時検証: `mode != "local"` なら必須。`mode = "local"` では `deliver = "pr"` を設定エラーにする(push 先がない)。
- **deliver のデフォルトは mode で分ける。** `deliver` 省略時、`mode != "local"` は `"pr"`、`mode = "local"` は `"branch"`(Phase 1 の唯一の対応値)。こうしないと「デフォルト pr + local は pr 禁止」の組み合わせで local プロジェクトが deliver を明示しない限り必ず設定エラーになる。local で `deliver` を明示するのは patch を選ぶ時だけ(Phase 2)。
- **issue の `deliver = "draft-pr"` は採らない。** draft かどうかは既存の `[pr] draft`(グローバル + プロジェクト上書き、`config.rs:78`)が既に持っており、deliver 軸に重ねると同じ設定が二箇所になる。deliver は「成果物の形」だけを表す 3 値にする。
- `meguri doctor` は local モードのプロジェクトに gh 認証を要求しない。

## flow / engine への通し方

- `Deps` に `task_source: Arc<dyn TaskSource>` を足し、`forge` を `Option<Arc<dyn Forge>>` にする(local モードでは None)。app.rs の Deps 構築(`app.rs:25`)が mode で分岐する。
- `flow.rs` の付け替え: `claim_issue` → `task_source.claim`、`escalate_on_forge` → `task_source.escalate`、`Flavor::release_claim` の既定実装 → `task_source.release`、worker の `settle_labels` → `task_source.complete`。`Checkpoint` には従来どおり title/body が入るので execute 以降は無改修で流れる。
- worker/planner の `discover` は `discover_by_label` の代わりに `task_source.discover(kind)` を呼ぶ(LabelTaskSource の実装内容は現行 `discover_by_label` の移設。hold/working/succeeded-run/blocked_by のゲートごと持っていく)。
- **scheduler は `Target` に `TaskKey` を載せる。** scheduler は `targets.sort_by_key(|t| t.issue_number)` と `create_run_for_loop(&project.id, kind, target.issue_number, &title)` で `Target { issue_number, title }`(`engine/mod.rs:34`)に直接依存している(`scheduler.rs:94, 101-106`)。`Target` に `key: TaskKey` を足し、`Task` → `Target` 変換で運ぶ。run 作成は `key` で分岐: `TaskKey::Issue(n)` は従来の `create_run_for_loop`(issue_number)、`TaskKey::Local(id)` は新設 `create_run_for_task`(task_id)。sort は `issue_number` が Option になるため `sort_by_key(|t| t.key)`(Issue/Local を跨いだ安定順)に置き換える。drive 先頭の claim は `Target.key` をそのまま `task_source.claim(&key, host)` に渡す。これで discover → run 作成 → claim を通じて `TaskKey` が一貫して運ばれる(受け入れ基準 6 の scheduler_test を緑に保つ)。
- forge 前提のループ(fixer / reviewer / spec-worker / conflict-resolver)は `deps.forge` が None なら `discover` が空を返す。ループ登録(`default_loops`)は共通のまま。
- `STEP_OPEN_PR` は `deliver` で分岐する: `pr` は現行どおり、`branch` は push も PR もせず検証済みブランチを残して終わり、`patch` は `git format-patch <default>..HEAD` を `.meguri/out/` へ(Phase 2)。`WorkerOutcome::Succeeded { pr_url }` は成果物の所在を表す文字列に一般化する(branch なら ブランチ名、ps/logs の表示互換はここで吸収)。
- execute プロンプト: local タスクは「GitHub issue #N」ではなくタスク title/body を渡す。deliver が pr 以外のときは `pr_body_instruction` を省き、summary だけ要求する。

## CLI(Phase 1 で足すもの)

```
meguri add [--project <id>] [--plan] [--file <path>] [<title>]   # タスク投入
meguri tasks [--project <id>] [--all]                             # 一覧(needs_human は強調)
```

`--file` は markdown を body に読み込む(1 行目見出しを title に)。`meguri queue --issue N`(silent)、`review` / `accept` / `reject` は Phase 2/3。投入即ディスパッチはやらず、まず watch のポーリングに乗せる(poll_interval 以内に拾われれば十分。即時性は後で `meguri add` から直接 dispatch を蹴る拡張が素直に乗る)。

## このブランチの実装範囲 = Phase 1

**Phase 1**(このブランチ): `tasks` テーブル + migration、`TaskSource` 切り出し + `LabelTaskSource`/`LocalTaskSource`、config の `mode`/`deliver`、`meguri add`/`tasks`、worker ループの local 対応、`deliver = "branch"`。これで「手元で完結する meguri」が成立する。

Phase 2(silent モード、`queue --issue`、patch、reaper のローカル完了判定、`review`/`accept`)、Phase 3(planner/reviewer の local 版、`reject`、`notify_command`)、Phase 4(Postgres TaskSource + lease/ハートビート + `ps --all`)は本 PR マージ後に sub-issue として切る。Phase 4 は本 spec と ADR 0003 が設計を固定するのみでコードは書かない。**Phase 3 までは単一マシン前提(ローカル sqlite が唯一の真実)であることを README に明記する。**

Phase 1 の reaper は保守的に倒す: local run の worktree は task が terminal(done/cancelled/needs_human)かつ tree が clean でも自動回収せず、`ActiveRun`/`StateUnknown` 側に倒して残す(deliver=branch の成果物はブランチと worktree そのものなので、勝手に消してはいけない。`meguri accept` が入る Phase 2 で回収条件を設計する)。

## 受け入れ基準(Phase 1)

1. `mode = "local"` で `repo_slug` なしのプロジェクトが設定ロードを通り、`meguri doctor` が gh 認証を要求しない。`mode = "local"` + `deliver = "pr"` は設定エラー。
2. `meguri add "タスク"` が queued な tasks 行を作り、`meguri tasks` に出る。`--file` / `--plan` が効く。
3. watch が queued タスクから worker run を作る。claim はアトミック: 同じタスクへの 2 回目の claim は None を返し、run は Skipped で終わる(既存の label 競合と同じ扱い)。
4. local run が `meguri/t<id>-<slug>-<hash>` ブランチの検証済みコミットで成功し、push も PR 作成も発生しない。task は done になる。
5. run 失敗時に task が needs_human + reason になり、`meguri tasks` / `ps` で見える。再 claim(新しい run)で needs_human が解除される(ラベル版 `claim_issue` の needs-human 除去と同型)。
6. `mode = "github"`(デフォルト)の挙動が完全に不変: 既存のループ/スケジューラ/リーパーのテストが(FakeForge → LabelTaskSource 経由になっても)全部通る。
7. 既存 DB が migration 0004 を通り、既存 runs のデータと active-run 排他が保たれる。

## テスト計画

- `src/store/tasks.rs` の unit test: claim の原子性(連続 claim で二度目が None)、状態遷移、needs_human の reason 保存、needs_human タスクの再 claim が成功し reason がクリアされる(受け入れ基準 5)。
- config: mode/deliver のパースとデフォルト、repo_slug 省略の可否、local+pr の拒否。
- `LabelTaskSource` は既存 `FakeForge` を包んでテストし、`discover_by_label`/`claim_issue` の既存テスト資産(worker_test / planner_test / scheduler_test)をそのまま緑に保つ。
- local ワーカーの流し: `tests/worker_test.rs` のパターン(FakeMux + in-memory Store)に `LocalTaskSource` を差して add → claim → deliver=branch → done を通す。
- migration: 0001〜0003 適用済み DB に runs 行を入れてから 0004 を当て、データ保全と部分 unique index を検証。

## 触るファイル

- `src/tasks.rs`(新規)— `TaskSource` トレイト、`Task`/`TaskKey`/`TaskKind`、`LabelTaskSource`、`LocalTaskSource`
- `src/store/migrations/0004_tasks.sql`(新規)+ `src/store/tasks.rs`(新規)— スキーマ、CRUD、アトミック claim
- `src/store/runs.rs` / `src/store/mod.rs` — runs 再作成 migration、`task_id`、部分 unique index、`create_run_for_task`
- `src/config.rs` — `mode`/`deliver`、`repo_slug: Option<String>`、ロード時検証
- `src/engine/mod.rs` — `Deps.task_source`、`forge: Option<_>`、`Target` に `key: TaskKey`
- `src/engine/scheduler.rs` — `Target` への `TaskKey` 付与、`key` による run 作成分岐(`create_run_for_loop` / `create_run_for_task`)、sort キーの `key` 化
- `src/engine/flow.rs` — claim/escalate/release/complete の付け替え、deliver 分岐、プロンプトの local 対応
- `src/engine/worker.rs` / `planner.rs` — discover の付け替え
- `src/engine/{fixer,reviewer,spec_worker,conflict_resolver}.rs` — forge 不在時の discover 空返し
- `src/engine/reaper.rs` — forge 不在時の保守的分類
- `src/gitops.rs` — `task_branch_name` / `task_from_branch`
- `src/cli.rs` / `src/app.rs` / `src/main.rs` — `add`/`tasks` コマンド、mode 別 Deps 構築、doctor
- `tests/` — 上記テスト計画
- `docs/adr/0003-tasksource-task-moves-run-pins.md` — 語彙とアーキテクチャ決定(本 PR に同梱)

## スコープ外

- silent モード一式、`deliver = "patch"`、`review`/`accept`/`reject`、`notify_command`(Phase 2/3)
- リモート DB 実装、lease 延長ハートビート、`ps --all`、ssh attach ヒント(Phase 4。設計は ADR 0003 が固定)
- local タスク間の依存関係(blocked_by 相当)— 必要になってから
- タスク単位の mode 上書き — プロジェクト単位で十分(issue の決めどころどおり)
