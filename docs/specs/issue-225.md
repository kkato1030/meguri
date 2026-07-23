# spec: issue #225 — config 键粒度を新構造(ADR 0013 の 4 store)に整える

> 使い捨ての足場(ADR 0001)。恒久的な設計判断は
> [`docs/adr/0013-config-four-stores-desired-state-vs-engine.md`](../adr/0013-config-four-stores-desired-state-vs-engine.md)
> に置いた。実装完了時に本 spec は削除する。

## これは何 / なぜ

ADR 0012(level-triggered reconciler)移行のスライス 5 / 5。config の粒度を新構造
(4 store + desired state / engine config の分離)に合わせて整える。設計の芯は ADR 0013。
本 spec は**実装前にレビューを収束させる**ためのもので、受け入れ基準・触るファイル・
決定事項に徹する。

## spec 深度の選択理由

**design spec**(深い層)を選ぶ。config schema・公開型(`RepoConfig` / `RepoManifest`)・
ADR 0011 のセキュリティ境界・hot reload に触れるため、uncertainty × blast radius が大きい。
veto ルール(schema / 公開 contract に触れる)により migration & rollback は必須。

## 受け入れ基準

1. **4 store 分類が総体で閉じている**: `ProjectConfig` の全フィールドがちょうど1つの所有
   store を持つ(欠落も二重も無い)ことを**テストで機械的に担保**する。新フィールド追加時に
   分類漏れが CI で落ちる。
2. **partition が総体で閉じる = 8 切り直しの所有 store が ADR 0013 分類に一致**。ここで
   「閉じる」は**分類の総体性**であって全キーの物理移設ではない(下記 3 と分ける):
   - `prompts` は **物理移設して Store C 可**にする(唯一 land するキー。fold 後 read)。
   - `worktree_setup` / `clean` / `cadence` / `notify.labels` と `triage.ignore` は
     **所有 store を C に確定しつつ物理移設は staged**(読み取り点が run 外 = default-branch
     読みの一般化を要する)。
   - `plan_delivery` は **Store B に留める**(読み取り点が pre-claim/ambient のため C 不可)。
   - `triage` 键単位分割(`ignore`=C、`mode`/`apply`/`confidence_threshold`/`max_actions_per_tick`
     =A/B)と `notify` 键単位分割(`labels`=C、webhook=A)を分類として確定。
3. **物理移設の範囲は `prompts` のみ(本 slice)**。理由は「読み取り点条件」(ADR 0013 決定 2):
   権威ある read が fold 後 run `deps`(= `pr.draft` と同じ)なので既存機構に無改造で乗る。他の
   C 確定キーは read が pre-claim/observe/discover のため、routing 実装を後続に回す(所有 store は
   確定済み)。
4. **混入は従来どおりエラー**: A/B 専用キー(`repo_slug` / `[agent]` / `triage.mode` …)を
   `meguri.toml` に書くと parse error(`RepoManifest` の `deny_unknown_fields`)、`meguri doctor`
   が報告。
5. **desired state(Store D)と engine config(A/B/C)の分離が構造に現れる**: D は precedence
   鎖に入らない(ラベル/本文は engine config を上書きしない)。ドキュメント + 型/コメントで明示。
6. **hot reload 非回帰**: Store A/B の hot reload は不変(process-bound の pin も不変)。C 可に
   なったキーの host `[projects.*]`(B)側 hot 編集は従来どおり効く。既存 `ConfigReloader`
   テストが全て緑のまま、C 可キーの host 上書き reload の非回帰テストを追加。
7. **claim pin の版互換**: `check_command` の claim 時 pin(ADR 0011 の完了契約セキュリティ)が
   不変。かつ**永続 pin 型(`RepoConfig`)の拡張が旧バイナリの `Checkpoint` decode を壊さない**
   (下記 migration & rollback)。

## 主要な決定(ADR 0013 で確定済み・実装が従う)

- **partition**: 全キーの所有 store は ADR 0013 分類表で確定。section 内键単位境界を許す
  (`[pr]` に加え `[triage]` / `[notify]` が 2・3 例目)。
- **C-eligibility の読み取り点条件**(f2 の解): C 可にできるのは、権威ある read がすべて
  **claim 後(fold 済み deps)か default-branch 読み**で賄えるキーだけ。pre-claim/ambient で
  読まれる `plan_delivery`(`is_combined` が reconciler snapshot・pr_reviewer park・fixer
  `pr_is_touchable` から ambient Deps で読む)は **Store B に留める**。`worktree_setup` も
  現状 fold 前(`prepare_worktree` 内)に走るため物理移設は staged。
- **precedence**: `builtin < A(host global) < C(repo, 読み機構ごとに pin/read) < B(host [projects.*])`。
  host が最後に勝つ(不変)。D は鎖の外(直交)。
- **Store C の読み機構は 2 系統**: claim 時 pin(fold 後 read、`Checkpoint` に載る)/
  default-branch 読み(run 外キーの共有機構、`schedules` = ADR 0015 が既存例)。所有 store と
  読み機構は独立軸。
- **永続 pin 面は最小に保つ**: `Checkpoint` に載せるのは fold 後 read かつ run 中改竄が問題に
  なるキーだけ。default-branch 読みの値(clean / cadence / …)は `Checkpoint` に入れない。
- `autonomy` / `review` / `pr.auto_merge` / `plan_delivery` は Store B(信頼境界)に留める。

## 触るファイル

- `docs/adr/0013-config-four-stores-desired-state-vs-engine.md` — 本 PR で land(作成済み)。
- `src/config.rs` — (a) Store C 面(`RepoConfig` / `RepoManifest`)に **`prompts` を追加**
  (物理移設する唯一のキー)。(b) **永続 pin 型 `RepoConfig` から `deny_unknown_fields` を外す**
  (eligibility の enforce は `RepoManifest` 側に残す。旧バイナリが未知 pin キーを寛容に無視できる
  ようにする = 下記 rollback)。(c) `triage` / `notify` の键単位分割を表現する repo 専用 subset 型
  (`RepoTriageConfig` 等、`RepoPrConfig` に倣う)を `RepoManifest` に追加(所有 store の確定。
  fold への配線は staged)。(d) partition の totality を守るテスト(全 `ProjectConfig` フィールドの
  所有 store 表明表 — 欠落・重複で落ちる)。
- `src/engine/mod.rs` — `Deps::with_repo_config` の fold を **`prompts` へ拡張**(precedence:
  host `[projects.*]` が set なら wholesale で勝つ、未 set なら repo 値で埋める)。
- `src/engine/flow.rs` — 物理移設対象(`prompts`)を `Checkpoint.repo_config` に載せる。pin は
  最初の agent turn より先(ADR 0011 不変)。**新フィールドは `#[serde(default)]`**、かつ pin 型は
  `deny_unknown_fields` を持たない(版互換)。
- `README.md` / `README.ja.md` の Configuration・Repo config 節、`INIT_TEMPLATE`
  (`src/config.rs`)のコメント、`docs/architecture/loops.md` の config への言及を 4 store に更新。
- ADR 0011 に「本 ADR の分類は ADR 0013 が 4 store へ一般化した」旨の追記(supersede ではなく延長)。
- **本 slice の非対象(staged、所有 store は ADR 0013 で確定済み)**: `worktree_setup` の読み順
  変更、`clean` / `triage.ignore` / `cadence` / `notify.labels` の default-branch 読みの一般化。
  これらの `src/engine/cleaner.rs` / `triage.rs` / `repo_reconciler.rs` / `src/tasks.rs` への
  配線は後続 issue。

## migration & rollback

- **永続状態と版互換(f1 の解)**: `Checkpoint.repo_config`(= `RepoConfig`)は sqlite に
  serialize される。現行 `RepoConfig` は `deny_unknown_fields` 付きなので、**新フィールドを
  そのまま足すと旧バイナリが新 checkpoint の `RepoConfig` decode に失敗し、`Checkpoint` 全体が
  `flow.rs` の `unwrap_or_default` に落ちて `pr_number` / `base_sha` / `thread_ids` まで失う**。
  これは #222 / ADR 0026 が守った不変条件に反する。したがって:
  - **eligibility の enforce は `RepoManifest`(parse gate)に残し、永続 pin 型 `RepoConfig` からは
    `deny_unknown_fields` を外す**。旧バイナリは未知 pin キーを寛容に無視し、残りの `Checkpoint` を
    保つ(その run はその機能が無かった時の挙動へ degrade。正しい後退)。
  - 新フィールドは `#[serde(default)]`(旧 checkpoint = 欠落を default 補完)。
  - **pin 面は最小**: default-branch 読みの値は `Checkpoint` に入れない(#222 が schedules を
    pin に入れなかったのと同じ)。
- **移行手順**: `meguri.toml` を置いていない既存 repo の挙動は完全に不変(C は opt-in)。
  現在 host `[projects.*]` に書いてあるキーはそのまま効き続ける(B が最後に勝つ)。
- **rollback**: 旧バイナリへ戻しても、(a) `meguri.toml` に書いた `prompts` は旧バイナリの
  `RepoManifest` が知らず parse error → warn + 無いもの扱い(プロセスは死なない)、(b) 進行中 run の
  新 checkpoint は上記の寛容 decode で既存 pin が生存する。破壊的な不可逆状態は無い。
- **schema 破壊の非対象**: forge ラベル(Store D)・DB マイグレーションは本スライスで変更しない。

## observability

- 既存 `repo_config.invalid` イベント(不正 `meguri.toml`)を新 C 可キーの parse error にも
  適用(経路は既存の `RepoConfig::load_from_worktree` → warn + emit)。
- 新規イベントは増やさない(config 化は既存の観測点に乗る)。

## test strategy

- **partition totality**: `ProjectConfig` の全フィールドを所有 store へ写像する表を持ち、
  漏れ・重複でコンパイル/テストが落ちる(ADR 0013 決定 2 の機械的担保)。
- **eligibility**: `prompts` は `meguri.toml` から読めて fold 後 run で効く。A/B 専用キー
  (`triage.mode` / `plan_delivery` / `[agent]` …)は `meguri.toml` で parse error
  (`RepoManifest` の `deny_unknown_fields`、既存テスト拡張)。
- **precedence**: host `[projects.prompts]`(B)set → repo(C)を上書き / host 未 set → repo 値が
  効く(既存 `pr_for` テストに倣う)。
- **claim 時 pin 不変**: `check_command` を run 中に書き換えても pin 値で検証される既存テストが
  緑、`prompts` も同様に pin されることを追加。
- **hot reload 非回帰**: 既存 `ConfigReloader` テスト群が全緑、host `[projects.prompts]` 上書きの
  reload 反映テストを追加。
- **版互換 checkpoint(f1)**: `RepoConfig` から `deny_unknown_fields` を外したうえで、
  (新バイナリ ← 旧 checkpoint = 欠落 default 補完)/(旧バイナリ ← 新 checkpoint = 未知キー無視で
  既存 pin 生存、`Checkpoint` が `unwrap_or_default` に落ちない)の両方向を round-trip で固定。
