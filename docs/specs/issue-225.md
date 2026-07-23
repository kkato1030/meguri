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
2. **8 切り直しが ADR 0013 の分類に一致**: `prompts` / `worktree_setup` / `plan_delivery` /
   `clean` / `cadence` / `notify.labels` を Store C 可にし、`triage` は键単位分割
   (`ignore` は C 可、`mode`/`apply`/`confidence_threshold`/`max_actions_per_tick` は A/B 専用)、
   `schedules` の読み機構(default-branch fire 時)を分類表どおりに明示する。
3. **混入は従来どおりエラー**: A/B 専用キー(`repo_slug` / `[agent]` / `triage.mode` …)を
   `meguri.toml` に書くと parse error(`deny_unknown_fields`)、`meguri doctor` が報告。
4. **desired state(Store D)と engine config(A/B/C)の分離が構造に現れる**: D は precedence
   鎖に入らない(ラベル/本文は engine config を上書きしない)。ドキュメント + 型/コメントで明示。
5. **hot reload 非回帰**: Store A/B の hot reload は不変(process-bound の pin も不変)。C 可に
   なったキーの host `[projects.*]`(B)側 hot 編集は従来どおり効く。既存 `ConfigReloader`
   テストが全て緑のまま、C 可キーの host 上書き reload の非回帰テストを追加。
6. `check_command` の claim 時 pin(ADR 0011 の完了契約セキュリティ)が不変であること。

## 主要な決定(ADR 0013 で確定済み・実装が従う)

- **partition**: 全キーの所有 store は ADR 0013 分類表で確定。section 内键単位境界を許す
  (`[pr]` に加え `[triage]` / `[notify]` が 2・3 例目)。
- **precedence**: `builtin < A(host global) < C(repo, 読み機構ごとに pin) < B(host [projects.*])`。
  host が最後に勝つ(不変)。D は鎖の外(直交)。
- **Store C の読み機構は 4 種**: claim 時 pin / observe 時(clean・triage.ignore)/ discover 時
  (cadence)/ default-branch fire 時(schedules)。所有 store と読み機構は独立軸。
- `clean` の repo 化は Repo Kind observe の読み取り点を使う(ADR 0011 が保留した前提はスライス
  1〜4 で解消)。
- `autonomy` / `review` / `pr.auto_merge` は Store B(信頼境界)に留める — repo 可にしない。

## 触るファイル

- `docs/adr/0013-config-four-stores-desired-state-vs-engine.md` — 本 PR で land(作成済み)。
- `src/config.rs` — Store C 面(`RepoConfig` / `RepoManifest`)を 8 切り直しに合わせて拡張:
  `prompts` / `worktree_setup` / `plan_delivery` / `clean` / `triage`(ignore のみ) /
  `cadence` / `notify`(labels のみ)を repo-eligible に。键単位分割は `RepoPrConfig` に倣った
  repo 専用 subset 型(`RepoTriageConfig` 等)で表す。`deny_unknown_fields` の網羅を維持。
  partition の totality を守るテスト(全 `ProjectConfig` フィールドの所有 store 表明)を追加。
- `src/engine/mod.rs` — `Deps::with_repo_config` の fold を新 C 可キーへ拡張(precedence:
  host `[projects.*]` が set なら wholesale で勝つ、未 set なら repo 値で埋める)。
- `src/engine/flow.rs` — claim 時 pin 対象(prompts / worktree_setup / plan_delivery)を
  `Checkpoint.repo_config` に載せる。pin は最初の agent turn より先(ADR 0011 不変)。
- `src/engine/cleaner.rs` / `src/engine/triage.rs` / `src/engine/repo_reconciler.rs` —
  clean / triage.ignore の observe 時読み取り(Repo Kind の読み取り点)。
- `src/tasks.rs`(cadence の discover 時読み取り経路)。
- `README.md` / `README.ja.md` の Configuration・Repo config 節、`INIT_TEMPLATE`
  (`src/config.rs`)のコメント、`docs/architecture/loops.md` の config への言及を 4 store に更新。
- ADR 0011 に「本 ADR の分類は ADR 0013 が 4 store へ一般化した」旨の追記(supersede ではなく延長)。

## migration & rollback

- **永続状態**: `Checkpoint.repo_config`(= `RepoConfig`)は sqlite に serialize される
  (ADR 0011)。**型を後方互換に拡張する**: 新フィールドは全て `#[serde(default)]` で、
  旧バイナリが書いた checkpoint(新キー無し)を新バイナリが読めること、逆に新バイナリが
  書いた checkpoint を**旧バイナリが読んでも既存キーが decode できる**ことをテストで固定
  (`RepoManifest::pinned()` がバイト安定を守ってきた idiom を継続、#222)。
- **移行手順**: `meguri.toml` を置いていない既存 repo の挙動は完全に不変(C は opt-in)。
  現在 host `[projects.*]` に書いてあるキーはそのまま効き続ける(B が最後に勝つ)。
  repo 化は「repo に書けるようになる」拡張であり、既存の書き方を壊さない。
- **rollback**: 旧バイナリへ戻しても、新しく `meguri.toml` に書いた C 可キーは旧バイナリの
  `deny_unknown_fields` で parse error → warn + 無いもの扱い(プロセスは死なない)。host 側の
  設定だけで run は継続する。破壊的な不可逆状態は無い。
- **schema 破壊の非対象**: forge ラベル(Store D)・DB マイグレーションは本スライスで変更しない。

## observability

- 既存 `repo_config.invalid` イベント(不正 `meguri.toml`)を新 C 可キーの parse error にも
  適用(経路は既存の `RepoConfig::load_from_worktree` → warn + emit)。
- 新規イベントは増やさない(config 化は既存の観測点に乗る)。

## test strategy

- **partition totality**: `ProjectConfig` の全フィールドを所有 store へ写像する表を持ち、
  漏れ・重複でコンパイル/テストが落ちる(ADR 0013 決定 2 の機械的担保)。
- **eligibility**: C 可キーは `meguri.toml` から読めて claim/observe/discover/fire の各機構で
  効く。A/B 専用キーは `meguri.toml` で parse error(`deny_unknown_fields`、既存テスト拡張)。
- **precedence**: host `[projects.*]`(B)set → repo(C)を wholesale で上書き / host 未 set →
  repo 値が効く、を C 可キーごとに 1 ケース(既存 `pr_for` / `clean_for` テストに倣う)。
- **claim 時 pin 不変**: `check_command` を run 中に書き換えても pin 値で検証される既存テストが
  緑、prompts / worktree_setup も同様に pin されることを追加。
- **hot reload 非回帰**: 既存 `ConfigReloader` テスト群が全緑、C 可キーの host 上書きの
  reload 反映テストを追加。
- **後方互換 checkpoint**: 新旧バイナリ間の `RepoConfig` serialize/deserialize round-trip。
