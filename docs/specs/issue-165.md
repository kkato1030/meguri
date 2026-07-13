# issue-165 spec — 二層 config: repo ルート `meguri.toml`(default branch から読む)

per-project の設定が host 側 `~/.meguri/config.toml` に増殖している。この spec は、
**プロジェクト内在の設定を repo ルート `meguri.toml` に宣言できるようにし、それを worktree では
なく default branch(trusted ref)から読む**機構を入れる。境界原理とセキュリティモデルは
ADR 0011(本 PR 同梱)に置いた — spec より長生きするため。ここでは実装に収束させる。

決定の骨子は ADR に譲り、この spec は「どこに何を足すか」だけを扱う。

## 決定の要点(ADR 0011 の実装への射影)

- repo config の**読み元は primary clone の trusted ref**(`origin/<default_branch>`、無ければ
  ローカル `<default_branch>`)の `meguri.toml` blob。worktree のファイルは絶対に読まない。
- repo config は**専用の狭いスキーマ**(`RepoConfig`)にする。repo-eligible キーだけを持ち
  `#[serde(deny_unknown_fields)]` で host 専用キーを parse 時に弾く。
- precedence は `既定 < host グローバル < repo < host [projects.*] override` の 4 層。
  **実装は「build_deps で effective `ProjectConfig` に repo 層を畳み込む」**方式にする(下記)。
  これにより既存の `*_for` リゾルバ群は無改修で 4 層になる。
- parse 失敗 = warn + イベント emit + 「無いもの扱い」フォールバック。プロセスは死なない。

## 初期の repo-eligible キー

```toml
# <repo>/meguri.toml — このファイルは default branch にマージされて初めて効く。
# run 中のブランチで書き換えても、その run の検証には一切影響しない。

language = "日本語"
check_command = "cargo test"

[clean]
ignore = ["docs/legacy"]

[pr]
draft = false          # repo 可。auto_merge をここに書くと doctor がエラー(host 専用)
```

`prompts`(#149)・`worktree_setup`(#139)は本機構に**後から乗る**顧客であり、初期スコープ外。
`schedules`(#146)は host 側据え置き(ADR 0011 の分類 / 論点 2)。

## 変更箇所

### 1. `RepoConfig` スキーマ — `src/config.rs`

repo-eligible キーだけの狭い struct を新設。`deny_unknown_fields` が境界の強制装置。

```rust
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub check_command: Option<String>,
    #[serde(default)]
    pub clean: Option<CleanConfig>,      // 既存 struct を再利用(wholesale)
    #[serde(default)]
    pub pr: Option<RepoPrConfig>,        // [pr] のうち repo 可のキーだけ
}

/// `[pr]` の repo 可サブセット。`auto_merge` フィールドを持たないので、
/// meguri.toml の `[pr]` に auto_merge を書くと deny_unknown_fields で弾かれる
/// — 「同一セクション内のキー単位境界」の実装(ADR 0011)。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct RepoPrConfig {
    #[serde(default)]
    pub draft: Option<bool>,
}
```

`CleanConfig` は既存の serde struct をそのまま再利用する(`deny_unknown_fields` は付いていないが、
`RepoConfig` 直下のキー混入は捕まる。`[clean]` 内の未知キーまで弾きたいかは実装判断 — 初期は
既存 `CleanConfig` 流用で足りる)。

### 2. trusted ref からのロード — `src/config.rs`(または `gitops` と協調)

```rust
impl RepoConfig {
    /// primary clone の trusted ref から meguri.toml を解決する。
    /// - ファイルが無い → Ok(None)(opt-in の既定)
    /// - 読めたが parse/検証失敗 → Err(…)(呼び出し側が warn + emit + 無視)
    /// fetch はしない(既存 fetch サイクルが origin/<default_branch> を前進させる)。
    pub fn load_from_default_branch(repo_path: &Path, default_branch: &str)
        -> Result<Option<(String /*blob_id*/, RepoConfig)>>;
}
```

- ref 解決は `default_branch_head` と同じ優先(`origin/<default_branch>` → `<default_branch>`)。
  実装は `git rev-parse <ref>:meguri.toml`(blob id)で存在確認・変更検知を安く行い、
  `git show <blob_id>` で内容を得る。いずれも `gitops::run_git_sync`(既存の sync git runner)。
- blob id を返すのは**キャッシュ**と**変更ログ**のため(下記 3)。ファイル不在(`rev-parse` が
  非ゼロ)は `Ok(None)`。

### 3. precedence 配線 — `src/app.rs` `build_deps`

repo 層は **effective `ProjectConfig` への畳み込み**で入れる。`build_deps` が組み立てる
`deps.project` に、host project が明示していないフィールドだけ repo 値を載せる:

- `language`: `project.language.or(repo.language)`
- `check_command`: `project.check_command.or(repo.check_command)`
- `clean`: `project.clean.or(repo.clean)`(wholesale)
- `pr`: host が `[projects.pr]` を持てば**そのまま wholesale で勝つ**(draft も auto_merge も)。
  持たず repo が `draft` を出す場合のみ、`project.pr = Some(PrConfig { draft: repo_draft,
  auto_merge: host_global.pr.auto_merge.clone() })` を合成する。
  → auto_merge は常に host(project override か global)由来になり、repo は決して寄与しない。

この畳み込みの結果、`pr_for` / `clean_for` / `language_for` などの既存リゾルバは**無改修**で
「effective project vs host グローバル」の 2 層を評価し、実効的に 4 層 precedence になる。
`check_command` は verification が `deps.project.check_command` を直接読む
(`src/engine/flow.rs:1324`)ので、畳み込み済みの値が自動でそこに効く。**worktree のファイルは
一度も読まれないため、受け入れ基準 2 は構造的に満たされる。**

畳み込みは build_deps で毎 reload tick 実行される(論点 1: fetch 後の tick ごと解決)。
blob id を前 tick と比較して変化時のみ INFO ログ + parse。同一 blob なら parse 結果を使い回す
(キャッシュは watch ループが notifiers と並べて持つ per-project map、または専用の小さな struct)。
`meguri run` 一発 / `doctor` はキャッシュ不要(単発読み)。

**pin について**: repo config は trusted ref からのみ読むので、run 中の worktree 改竄は原理的に
無効。加えて in-flight run は「開始時の config を保つ」既存の hot-reload 挙動
(`src/app.rs:263` の注記)にそのまま乗る。`agent_profile` / `body_digest` 型の run 単位 pin 列を
新設するのは YAGNI(default branch は fetch 周期でしか動かず、改竄は既に不能)。初期スコープ外。

### 4. parse 失敗フォールバック + イベント — `src/app.rs` / `src/events.rs`

`load_from_default_branch` が `Err` を返したら、build_deps は repo 層を**無い扱い**にして続行し、
`store.emit(None, "repo_config.invalid", json!({ "project": id, "error": … }))` を一度だけ emit、
`tracing::warn!` を出す(既存の `ConfigReloader::poll` の「壊れた config を一度だけ warn」流儀)。
build_deps は既に `open_store()` 済みなので run_id 無しで emit できる。

### 5. `meguri doctor` — `src/main.rs`

プロジェクトごとに `RepoConfig::load_from_default_branch` を呼び、

- ファイルなし → 無言(opt-in、正常)。
- parse OK → `✅ repo config (<project>): meguri.toml OK`。
- **host 専用キー混入 / TOML エラー**(deny_unknown_fields で `Err`)→ `❌` で doctor を fail
  させる(routing・schedules と同じ「静かにフォールバックしない」原則)。

`doctor_repo_configs(cfg) -> bool` を新設し、`doctor_schedules` などと同列に `ok &=` で畳む。

### 6. init テンプレート + ドキュメント

- `INIT_TEMPLATE`(`src/config.rs`)にコメントで「プロジェクト内在の設定は repo の `meguri.toml`
  に置ける(default branch 経由で反映)」を一言。
- `README.md` の Configuration に**二層の説明 + 境界原理の要約**(詳細は ADR 0011 へリンク)、
  repo-eligible キー一覧、`meguri.toml` の例。
- `README.md` の Security に「repo config は trusted default branch 経由でのみ反映され、run 中の
  worktree 改竄は `check_command` に効かない」を追記。`SECURITY.md` の該当箇所からも参照。

### 7. テスト — `src/config.rs` unit + `tests/`

- precedence 4 層の解決(既定 / host グローバル / repo / host override の各優先)。
- **trusted ref 読み**: temp git repo を作り、default branch に `meguri.toml`、別ブランチで
  改竄した `meguri.toml` を置き、effective `check_command` が default branch 側の値になること
  (worktree ブランチの変更が効かないこと)。`run_git_sync` で組む
  (`conflict_resolver.rs` の既存テストが手本)。
- parse 失敗 → 無視フォールバック + `repo_config.invalid` イベント emit。
- doctor の host 専用キー検出(例: `repo_slug` / `agent` を書くと fail)。
- `[pr]`: repo の `draft` が効き、`auto_merge` を書くと doctor エラー。host `[projects.pr]` が
  あれば draft も host が勝つ。

## 論点への回答

1. **読み込みタイミング**: fetch 後の tick ごとに build_deps が trusted ref を解決、blob id 変化時に
   ログ。追加 fetch はしない(既存サイクル依存)。run 外ループ(cleaner interval / discovery)は
   その project の Deps を共有するので同じ effective config を見る。→ 足りる。
2. **schedules を repo-eligible にするか**: 初期は host 側据え置き(ADR 0011 の分類)。緩和は #146 側の判断。
3. **local mode**: primary clone のローカル `<default_branch>` から読む(ref 解決が origin 無しで
   ローカルに fallback するので一貫)。
4. **`[pr]` の分割**: `RepoPrConfig`(`draft` のみ、deny_unknown_fields)で表現。`auto_merge` は
   合成時に必ず host 由来。→ 同一セクション内キー単位境界の最初の実装。
5. **非権威ヒント**(repo 側 `workspace_hint` を doctor が表示): 初期スコープ外(YAGNI)。

## やらないこと(issue 準拠)

- workspace(#154)の repo 側宣言 — 境界原理により host 専用で確定。
- worktree ブランチからの config 読み込み(いかなる形でも)。
- host 専用キーの repo 側受け入れ(silent ignore もしない — doctor でエラー)。
- repo config の hot reload 通知機構の新設。
- `prompts`(#149)/ `worktree_setup`(#139)/ `schedules`(#146)の repo 化 — 別 issue。

## 受け入れ基準

1. repo ルートに `meguri.toml` を置くと、`check_command` / `language` / `clean` / `pr.draft` が
   その repo の run に効く。host `[projects.*]` に同キーがあれば host が勝つ。
2. run 中のブランチで `meguri.toml` を書き換えても、その run の検証(`check_command`)には一切
   影響しない。
3. `meguri.toml` に host 専用キー(例: `repo_slug`、`agent`)を書くと `meguri doctor` がエラーを報告する。
4. `meguri.toml` が壊れている場合、warn の上で host config のみで動作継続する(プロセスは死なない)。
5. `meguri.toml` を置かない既存プロジェクトの動作は完全に不変(opt-in)。
