# issue-165 spec — 二層 config: repo ルート `meguri.toml`(run 開始時に worktree から読んで pin)

per-project の設定が host 側 `~/.meguri/config.toml` に増殖している。この spec は、
**プロジェクト内在の設定を repo ルート `meguri.toml` に宣言できるようにし、それを run claim 時に
worktree から一度だけ読んで run に pin する**機構を入れる。境界原理・脅威モデル・保証範囲は
ADR 0011(本 PR 同梱)に置いた — spec より長生きするため。ここでは実装に収束させる。

> **設計変更の経緯**: 初版は「default branch(trusted ref)から読む」方式だったが、linked worktree が
> primary clone と git dir を共有するため agent が `git update-ref` で trusted ref を改竄でき、保証に
> ならないことが判明した(ADR 0011 参照)。本 spec は「worktree から読み、claim 時に pin する」方式に
> 差し替える。守れる保証は *「開始済み run の完了契約は claim 後に不変(ファイル編集・ref 改竄・
> crash→resume に動じない)」* に正直に絞る。

決定の骨子は ADR に譲り、この spec は「どこに何を足すか」だけを扱う。

## 決定の要点(ADR 0011 の実装への射影)

- repo config の**読み元は run の worktree の `meguri.toml`**。trusted ref も blob も fetch も使わない。
- **worktree 確定直後・最初の agent turn の前に一度だけ読み、run の `Checkpoint` に pin する**
  (本 spec で「claim 時 pin」はこの配置を指す)。以後の検証・PR 作成・prompt は pin 値を
  読む。resume は worktree を再読せず pin を使う(`base_sha` / `agent_profile` / `body_digest` と同じ
  「claim 時 settle・resume 間不変」idiom)。
- repo config は**専用の狭いスキーマ**(`RepoConfig`)。repo-eligible キーだけを持ち
  `#[serde(deny_unknown_fields)]` で host 専用キーを parse 時に弾く。
- precedence は `既定 < host グローバル < repo < host [projects.*] override` の 4 層。**実装は
  「pin 済み `RepoConfig` を effective `ProjectConfig` に畳み込み、run スコープの `Deps` に差し替える」**
  方式。既存の `*_for` リゾルバ群は無改修で 4 層になる。
- parse 失敗 = warn + イベント emit + 「無いもの扱い」フォールバック。プロセスは死なない。

## 初期の repo-eligible キー

```toml
# <repo>/meguri.toml — このファイルの値は run 開始時(claim 時)に一度だけ読まれ、その run に固定される。
# run 中に書き換えても、その run の検証には効かない(次に開始する run から反映)。
# 新規タスク run は default branch を base に作られるので、default branch にマージした設定がその run に効く。

language = "日本語"
check_command = "cargo test"

[pr]
draft = false          # repo 可。auto_merge をここに書くと doctor がエラー(host 専用)
```

`prompts`(#149)・`worktree_setup`(#139)は本機構に**後から乗る**顧客であり、初期スコープ外。
`schedules`(#146)は host 側据え置き(ADR 0011 の分類 / 論点 2)。

**`clean` も初期スコープから外す**(境界原理上は repo 可 — ADR 0011)。cleaner loop は通常の run flow に
乗らない: `interval_hours` は run がまだ存在しない discover 時に
`deps.config.clean_for(&deps.project)` で読まれ(`src/engine/cleaner.rs:262`)、cleaner 本体も共有
`flow::Checkpoint` ではなく独自の `CleanCheckpoint` を持つ独自 `drive` で `clean.ignore` を読む
(`src/engine/cleaner.rs:357-428`, `src/engine/cleaner.rs:739`)。つまり本 spec の
「`flow::Checkpoint` に pin + `with_repo_config` で畳む」機構は cleaner に**届かない**。repo 化するなら
cleaner 専用の読み込み・pin・precedence の設計が要る(将来 issue、本 spec の対象外)。

## 変更箇所

### 1. `RepoConfig` スキーマ — `src/config.rs`

repo-eligible キーだけの狭い struct を新設。`deny_unknown_fields` が境界の強制装置。checkpoint に
pin するため `Serialize` も derive する(下記 3)。

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub check_command: Option<String>,
    #[serde(default)]
    pub pr: Option<RepoPrConfig>,        // [pr] のうち repo 可のキーだけ
}

/// `[pr]` の repo 可サブセット。`auto_merge` フィールドを持たないので、
/// meguri.toml の `[pr]` に auto_merge を書くと deny_unknown_fields で弾かれる
/// — 「同一セクション内のキー単位境界」の実装(ADR 0011)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoPrConfig {
    #[serde(default)]
    pub draft: Option<bool>,
}
```

`[clean]` は初期スコープ外(上記)なので `RepoConfig` にフィールドを持たない —
`meguri.toml` に `[clean]` を書くと `deny_unknown_fields` で弾かれ doctor がエラーを報告する。
将来 cleaner 専用機構と一緒に足す。

### 2. worktree からのロード — `src/config.rs`

```rust
impl RepoConfig {
    /// run の worktree ルートから meguri.toml を読む。
    /// - ファイルが無い → Ok(None)(opt-in の既定)
    /// - 読めたが parse/検証失敗 → Err(…)(呼び出し側が warn + emit + 無視)
    /// git は一切触らない(worktree の作業ツリー上のファイルをそのまま読む)。
    pub fn load_from_worktree(worktree: &Path) -> Result<Option<RepoConfig>>;
}
```

- 実装は `std::fs::read_to_string(worktree.join("meguri.toml"))` → `toml::from_str`。
  `NotFound` は `Ok(None)`、その他の IO / parse エラーは `Err`。
- trusted ref 解決・blob id・キャッシュ・fetch は**すべて不要**(初版の複雑さはここで消える)。

### 3. claim 時 pin + run スコープ Deps — `src/engine/flow.rs`

**pin 列を `Checkpoint` に追加**(`src/engine/flow.rs` の `Checkpoint`):

```rust
/// repo `meguri.toml` の値を claim 時(初回 worktree 準備時)に一度だけ解決して固定したもの。
/// resume 時は worktree を再読せずこれを使う(改竄経路を塞ぐ / ADR 0011)。
/// `None` は「まだ解決していない」、`Some(RepoConfig::default())` は「読んだが meguri.toml 無し」。
#[serde(default)]
pub repo_config: Option<RepoConfig>,
```

**pin の設定は共有 `drive`(`src/engine/flow.rs:370`)で、worktree 確定の直後・`STEP_EXECUTE` に
入る前に一度だけ**。`drive` は `STEP_PREPARE_WORKTREE` の直後に run record を再読して worktree path を
確定する(`src/engine/flow.rs:397-406`)。その直後に、`checkpoint.repo_config` が未設定(新規 run)なら
`RepoConfig::load_from_worktree(&worktree)` を呼び、結果(`Err` 時は `default()`)を入れて
**checkpoint を persist してから**先へ進む。設定済み(resume)なら**読まない**。effective `Deps` の
構築も同じ場所で行い、**以降の `execute` / `validate` / self-review / `deliver` はすべてそれを使う**:

```rust
// drive: worktree path 確定の直後(STEP_EXECUTE に入る前)。
if checkpoint.repo_config.is_none() {
    let pinned = match RepoConfig::load_from_worktree(&worktree) {
        Ok(opt) => opt.unwrap_or_default(),          // ファイル無し = 空 pin
        Err(e) => { /* warn + emit(下記 4) */ RepoConfig::default() }
    };
    checkpoint.repo_config = Some(pinned);
    save_step(deps, &run, &step, &checkpoint)?;      // pin を固定してから先へ
}

// host project が明示していないフィールドだけ repo pin 値で埋める。
let deps_owned;
let deps = if let Some(repo) = checkpoint.repo_config.as_ref().filter(|r| /* 何か repo 値がある */) {
    deps_owned = deps.with_repo_config(repo);   // Deps を clone し .project を effective 版に差し替え
    &deps_owned
} else {
    deps
};
```

この配置が要点(guard レビュー指摘への回答):

- **`drive` の頭(checkpoint deserialize 直後)で畳んでは駄目**。新規 run はその時点で
  `repo_config = None` であり、pin 後に `Deps` を作り直さない限り、初回の `validate` は元の
  `deps.project.check_command`(`src/engine/flow.rs:1324`)を見てしまう。pin と effective `Deps` の
  構築を `STEP_EXECUTE` の手前に置くことで、**新規 run の最初の `execute` / `validate` から pin 値が効く**。
- **flavor の `prepare_worktree` 実装は無改修**。trait は `cp: &Checkpoint` の不変借用
  (`src/engine/flow.rs:113`)なので flavor 側では pin を書けない。pin は共有 `drive` の一箇所に置く。
- **pin persist は最初の agent turn より必ず先**。worktree 作成〜pin persist の間に crash しても、
  その区間では agent は一度も走っていないから、resume 時の再読(`repo_config` がまだ `None`)が
  改竄に晒されることはない。pin が persist された後の resume は再読しない。
- `prepare_work`(claim)と `prepare_worktree` 自体には repo config は効かないが、repo-eligible キーは
  いずれも worktree 準備より前に消費されないので問題ない。
- 本機構の導入前に claim された in-flight run は checkpoint に `repo_config` が無く、次の drive 再開時
  (step が execute より後でも)に一度だけ pin される。一回きりの移行エッジとして許容する。

`Deps::with_repo_config`(新設)は `self.clone()` して `.project` を effective `ProjectConfig` に置き換える
(`Deps` は mux/store 等を Arc 共有する clone 前提の構造 — scheduler.rs 参照)。effective `ProjectConfig` の畳み込み:

- `language`: `project.language.or(repo.language)`
- `check_command`: `project.check_command.or(repo.check_command)`
- `pr`: host が `[projects.pr]` を持てば**そのまま wholesale で勝つ**(draft も auto_merge も)。
  持たず repo が `draft` を出す場合のみ `PrConfig { draft: repo_draft, auto_merge:
  host_global.pr.auto_merge.clone() }` を合成する。→ auto_merge は常に host 由来、repo は寄与しない。

この結果、`pr_for` / `language_for` や `deps.project.check_command`(validate,
`src/engine/flow.rs:1324`)などの既存の消費点は**無改修**で effective 値を見る。**worktree のファイルは
claim 時の一度しか読まれず、resume 後も pin 値を使うため、受け入れ基準 2 が構造的に満たされる。**

> **なぜ build_deps で畳み込まないか**: build_deps は project 単位・run 前に走るため、run 固有の worktree が
> まだ無い。worktree の `meguri.toml` は run スコープの事実なので、畳み込みは claim 後の run スコープ
> (`drive`)でしか正しくできない。これが初版(trusted ref を build_deps で畳む)との構造的な違い。

### 4. parse 失敗フォールバック + イベント — `src/engine/flow.rs`

`load_from_worktree` が `Err` を返したら、pin を `Some(RepoConfig::default())`(= 無い扱い)にして
run を継続し、`deps.store.emit(Some(&run.id), "repo_config.invalid", json!({ "error": … }))` を
一度 emit、`tracing::warn!` を出す(既存の「壊れた config を一度だけ warn」流儀)。run スコープなので
`run_id` 付きで emit できる。

### 5. `meguri doctor` — `src/main.rs`

doctor は run を持たないので、各プロジェクトの **primary clone の作業ツリー**
(`<repo_path>/meguri.toml`)を lint する(doctor は完了契約を決めないので、どの断面を読むかは
advisory。作業ツリー読みで十分)。

- ファイルなし → 無言(opt-in、正常)。
- parse OK → `✅ repo config (<project>): meguri.toml OK`。
- **host 専用キー混入 / TOML エラー**(deny_unknown_fields で `Err`)→ `❌` で doctor を fail
  させる(routing・schedules と同じ「静かにフォールバックしない」原則)。

`doctor_repo_configs(cfg) -> bool` を新設し、`doctor_schedules` などと同列に `ok &=` で畳む。

### 6. init テンプレート + ドキュメント

- `INIT_TEMPLATE`(`src/config.rs`)にコメントで「プロジェクト内在の設定は repo の `meguri.toml`
  に置ける(run 開始時にそのブランチから読まれ、その run に固定される)」を一言。
- `README.md` の Configuration に**二層の説明 + 境界原理の要約**(詳細は ADR 0011 へリンク)、
  repo-eligible キー一覧、`meguri.toml` の例、**反映タイミング**(default branch にマージ → 以後の
  新規 run / PR ブランチに commit → その PR の run)を書く。
- `README.md` の Security に「repo config は run 開始時に worktree から読んで **その run に pin** され、
  run 中の worktree 改竄・ref 改竄・resume では変わらない。保証範囲は *開始済み run の完了契約の不変性*
  であり、敵対的 agent の完全隔離ではない(ADR 0011)」を追記。`SECURITY.md` の該当箇所からも参照。

### 7. テスト — `src/config.rs` unit + `tests/`

- **`load_from_worktree`**: ファイル無し → `Ok(None)`、正常 → `Ok(Some(..))`、host 専用キー混入 →
  `Err`(deny_unknown_fields)。
- precedence 4 層の畳み込み(既定 / host グローバル / repo pin / host override の各優先)を
  `with_repo_config` の unit で検証。
- **claim 時 pin(改竄が効かないこと)**: temp git repo + worktree を作り、claim(pin 設定)後に
  worktree の `meguri.toml` を書き換えて(あるいは `git update-ref` で ref を弄って)から
  effective `check_command` を解決し、**claim 時の値のまま**であることを確認する。`run_git_sync` で
  組む(`conflict_resolver.rs` の既存テストが手本)。
- **resume で pin を再利用**: `cp.repo_config` が設定済みなら `load_from_worktree` を呼ばず(= worktree を
  書き換えても)pin 値が使われることを確認。
- **新規 run の初回 step から効くこと**: pin と effective `Deps` の構築が `STEP_EXECUTE` の手前にある
  こと(変更箇所 3 の配置)を、pin 直後に解決した effective `check_command` が repo 値になる形で確認。
- parse 失敗 → 無視フォールバック + `repo_config.invalid` イベント emit。
- doctor の host 専用キー検出(例: `repo_slug` / `agent` を書くと fail)。
- `[pr]`: repo の `draft` が効き、`auto_merge` を書くと doctor エラー。host `[projects.pr]` が
  あれば draft も host が勝つ。

## 論点への回答

1. **読み込みタイミング**: run claim 時(初回 worktree 準備時)に一度だけ worktree から読み、checkpoint に
   pin。resume は再読しない。hot reload / fetch cadence への追従は不要(反映は「そのブランチへの commit」で、
   新規 run が次に claim する時に自然に効く)。→ 初版の guard 指摘(`ConfigReloader::poll` が repo blob 変化を
   拾えない)は本方式では**そもそも発生しない**。
2. **schedules を repo-eligible にするか**: 初期は host 側据え置き(ADR 0011 の分類)。緩和は #146 側の判断。
3. **local mode**: worktree は origin 無しでもローカル `<default_branch>` を base に作られる(gitops)。
   worktree の `meguri.toml` を読むだけなので origin の有無に依らず一貫。
4. **`[pr]` の分割**: `RepoPrConfig`(`draft` のみ、deny_unknown_fields)で表現。`auto_merge` は
   畳み込み時に必ず host 由来。→ 同一セクション内キー単位境界の最初の実装。
5. **非権威ヒント**(repo 側 `workspace_hint` を doctor が表示): 初期スコープ外(YAGNI)。

## やらないこと(issue 準拠)

- workspace(#154)の repo 側宣言 — 境界原理により host 専用で確定。
- trusted ref / default branch blob からの config 読み込み(却下 — ADR 0011)。
- run 中の worktree live 読み(pin せず)/ resume 時の worktree 再読(どちらも改竄経路になる — 却下)。
- host 専用キーの repo 側受け入れ(silent ignore もしない — doctor でエラー)。
- `prompts`(#149)/ `worktree_setup`(#139)/ `schedules`(#146)の repo 化 — 別 issue。
- `clean` の repo 化 — cleaner loop は run flow の外で config を読む(discover 時の `interval_hours`、
  独自 `CleanCheckpoint` の sweep 内 `ignore` — 「初期の repo-eligible キー」節参照)ため、
  claim 時 pin 機構が届かない。cleaner 専用機構の設計と一緒に将来 issue で扱う。

## 受け入れ基準

1. repo ルートに `meguri.toml` を置くと、`check_command` / `language` / `pr.draft` が
   その repo の run に効く。**新規 run の最初の `execute` / `validate` から**効き、
   host `[projects.*]` に同キーがあれば host が勝つ。
2. **開始済み run の完了契約は claim 後に不変**: run 中に worktree の `meguri.toml` を書き換えても、
   `git update-ref` で ref を弄っても、crash→resume を挟んでも、その run の検証(`check_command`)は
   claim 時の値のまま変わらない。
3. `meguri.toml` に host 専用キー(例: `repo_slug`、`agent`)を書くと `meguri doctor` がエラーを報告する。
4. `meguri.toml` が壊れている場合、warn の上で host config のみでその run を継続する(プロセスは死なない)。
5. `meguri.toml` を置かない既存プロジェクトの動作は完全に不変(opt-in)。
