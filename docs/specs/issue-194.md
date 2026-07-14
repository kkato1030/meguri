# spec: issue #194 — repo 側ファイルの読み取りを default branch 起点(git show)に統一する

## ねらい

ADR 0011 が定めた宣言セマンティクス「repo 側の宣言は default branch から読む」に、まだ残っている
**working tree 直読の近似**を追いつかせる。対象は「完了契約に効かない助言 / 発見用途」の 4 箇所だけ。
出所の二分(pin=worktree / 助言=default branch)の理由は **ADR 0015** に記録した。

## spec 深度

**normal**。永続状態・スキーマ・公開契約に触れず(migration / rollback は不要)、変更は git の
読み取り出所の差し替えに閉じる。ただし「なぜ ADR 0011 が却下した `git show` をここで使うのか」という
判断は誤読されやすいので ADR 0015 に切り出した。不確実性は低・波及は中(gitops は共有だが新設関数の
追加が主)。

## 対象箇所

置き換える(working tree → default branch):

- `src/engine/scheduler_fire.rs:179` — `render_body` の `body_file` を `repo_path.join(rel)` で読む
  (唯一の実行時 working-tree 依存)
- `src/main.rs:386` — `doctor_schedules` の `body_file` 存在検証
- `src/main.rs:463` — `doctor_repo_configs` の `RepoConfig::load_from_worktree`
- `src/main.rs:502` — `doctor_prompts` の `resolve_preamble_within`

**対象外**(run に紐付き worktree から読むのが正しい。ADR 0015 の表参照):

- 実行時 preamble 解決 `src/engine/flow.rs:1323` — run の worktree から各 turn で live に読む
  (pin ではない。その run のブランチが権威)
- repo config の claim 時 pin `src/engine/flow.rs:442` — worktree から読み checkpoint に pin(ADR 0011)

## 変更方針

### 1. gitops に「default branch 上のファイル内容を読む」関数を新設

git 呼び出しは rust.md ルールどおり `src/gitops.rs` に集約する。

```
pub async fn read_file_at_default_branch(
    repo_path: &Path,
    default_branch: &str,
    rel: &str,
) -> Result<DefaultBranchFile>

pub enum DefaultBranchFile {
    /// 通常ファイルの blob(mode 100644 / 100755)。中身。
    Content(String),
    /// default branch のツリーに `rel` が存在しない。
    Absent,
    /// `rel` は在るが通常ファイルではない(symlink 120000 / tree 040000 /
    /// submodule 160000)。中身を blob として返せない。
    NotRegularFile,
}
```

- base 解決は既存 idiom(`default_branch_head` / `commits_ahead` などの
  `origin/<default>` を rev-parse、無ければ local `<default>`)と同じ倒し方。base ref 自体が解決
  できない等の git 失敗は `Err(_)`。
- **なぜ enum で 4 状態(3 variant + Err)か**: 呼び出し側が「無い」と「通常ファイルでない」で挙動が
  割れる(repo config は Absent=opt-out だが symlink/tree は ❌)。また `git cat-file -e` や
  `git show` は **tree object(ディレクトリ)でも成功**し、`git show <base>:<dir>` は一覧を返すので、
  「存在プローブ→git show」だけでは blob 内容を返す契約を破る(dir を中身と誤読)。
- **判定は `git ls-tree` で「完全一致の 1 エントリ」を得て型を確定**する。素朴に
  `git ls-tree <base> -- <rel>` だと、`rel` が `src/` のように**末尾スラッシュ付き**だと git は tree 自身
  ではなく**配下を列挙**し、先頭が `100644` なら通常ファイルと誤判定する(finding)。pathspec マジックや
  glob(`*.md` 等)も同じ穴になる。よって次で塞ぐ:
  - `rel` の末尾スラッシュを拒否する(`validate_repo_relative` に末尾 `/` 拒否を足す。config load と
    gitops 入口の両方で弾く)。
  - pathspec マジックを無効化して呼ぶ: `git ls-tree --full-tree -z --format='%(objectmode) %(objecttype) %(objectname) %(path)' <base> -- ':(literal)<rel>'`(または `GIT_LITERAL_PATHSPECS=1`)。
    ディレクトリを渡しても**再帰しない**(`-r` を付けない)ので、tree はその tree 1 エントリとして返る。
  - 出力を `-z`(NUL 区切り)でパースし、**エントリが正確に 1 つ**かつ **`%(path)` が `rel` と完全一致**の
    ものだけ採用する。0 件 → `Absent`。複数件や path 不一致 → `Absent` 扱い(誤って Content にしない)。
  - mode で分岐:
    - `100644` / `100755`(通常 blob)→ **その `%(objectname)`(blob object id)から `git cat-file blob <oid>`**
      で内容取得 → `Content`。pathspec を再解決せず immutable な oid から読むので、判定と取得のパスがズレない。
    - `120000`(symlink)/ `040000`(tree)/ `160000`(submodule gitlink)→ `NotRegularFile`。
  - これで symlink・ディレクトリ(末尾スラッシュ含む)・submodule・glob を Content から確実に外す
    (finding 2 / 3 と本 finding)。
- doctor helper は sync(`fn`)だが async な `cmd_doctor` から呼ばれているので、**helper を async 化**して
  この関数を `await` する(`check_auto_merge` が既に async 化の先例)。sync 版
  `run_git_sync` を増やすより素直。

### 2. 呼び出し側の差し替え

- `render_body`(`scheduler_fire.rs`)を **async 化**し(呼び出し元 `fire_one` は既に async)、
  `body_file` を新関数で読む。`Content`→ body に使う。`Absent`(default branch に無い)/
  `NotRegularFile`(symlink・dir 等)/`Err` は現状の read エラー同様 `bail!`/`with_context` 相当で
  fire を失敗させる(sweep はログして次 tick 再試行)。
- `doctor_schedules`: `Content`→✅、`Absent`→❌「default branch に無い」、
  `NotRegularFile`→❌「通常ファイルでない(symlink/dir)」、`Err`→❌。
- `doctor_repo_configs`: `Absent`→ opt-out(何も出さない、現状維持)、`Content(raw)`→
  `RepoConfig` を **文字列からパース**して lint。`load_from_worktree` は working tree 前提なので、
  raw を受けてパースする経路(例: `RepoConfig::parse_str` 相当)に寄せるか、新関数で得た raw を
  既存パースに渡す。`NotRegularFile`→❌、`Err`→❌。
- `doctor_prompts`: `resolve_preamble_within`(symlink 追跡 + containment)を新関数に置換。
  `Content`→✅、`Absent`→❌「default branch に無い」、`NotRegularFile`→❌「symlink/dir(既定 branch から
  内容を検証できない)」、`Err`→❌。symlink を silent に ✅ しない(finding 3)。

### 3. パス検証(issue が明示した確認事項)

git ツリーの読み取りは **blob 直読**なので、`resolve_preamble_within` の symlink 追跡込み
containment とは脱出経路の性質が変わる:

- ツリールート内に構造的に閉じる。`..` はツリー外指定として git が拒否、絶対パスはルート相対扱いで
  該当エントリ無し(=`Absent`)になる。
- symlink は `ls-tree` の mode 120000 で判別して **`NotRegularFile` に落とす**(辿らない・内容も返さない)。
  したがって preamble の `Escapes`(symlink で外へ)も、内へ向く symlink も、区別なく「通常ファイルでない」
  として扱う。doctor はこれを ❌ + 「symlink」注記で **可視化**する(silent ✅ にしない)。
- **`validate_repo_relative` を拡張**する: 現状の絶対 / `..` 拒否に加え、**末尾スラッシュ(`src/` 等)を拒否**する。
  末尾スラッシュは `ls-tree` の配下列挙を誘発してディレクトリを通常ファイルと誤判定させる穴なので、
  入口で弾く(gitops 側の「完全一致 1 エントリ」判定と二重の防御)。
- preamble パスは config load 時に `validate_repo_relative` を既に通る。一方 **`body_file` はこの検証を
  通っていない**(現状 working tree 直読なので `..`/絶対で clone 外に出られる軽微な穴が既にある)。統一のため
  **config load 時の schedule 検証(`validate_schedules`)に `validate_repo_relative(body_file)` を追加**し、
  runtime / doctor の両方で `..`/絶対/末尾スラッシュを一様に弾く。ツリーの構造的閉じ込めはその backstop。

**判断**: 助言 / 発見読み取りでは「`rel` の正規化(`..`/絶対/末尾スラッシュ拒否)+ pathspec マジック無効化 +
`ls-tree` で完全一致 1 エントリの mode 判定 + blob object id から内容取得」で足りる。symlink 追跡
containment は不要(そもそも辿らず、`NotRegularFile` として報告)。

### 4. 挙動変化(既知)

- **doctor と実行時 preamble の symlink 判定は一致しない。これは意図した割り切り**(finding 3)。
  実行時 preamble(`flow.rs:1323`・本 issue 対象外)は run の worktree を `resolve_preamble_within` で
  読むので、repo 内へ向く symlink は今までどおり**辿って内容を注入**し、外へ向く symlink は skip する。
  一方 doctor はこの issue で default branch を **blob として lint** するため symlink を辿れず、
  `NotRegularFile` として ❌ + 注記で報告する。doctor は「default branch から中身を検証できない preamble」を
  正直に告げるのであって、runtime を再現しようとはしない(runtime は別ソース=run のブランチを読む)。
  symlink preamble 自体が稀なエッジケース。両者の差はこの割り切りとして記録する。
- 新関数は**暗黙 fetch しない**(local の `origin/<default>` ref をそのまま読む)。理由: doctor は
  projects × preamble key でループし、scheduler は毎 tick 走るため、read ごとの fetch は無駄で
  ネットワーク依存を増やす。freshness は run flow 側の既存 fetch
  (`default_branch_head` / `create_worktree` / `fetch_base_tip`)が担う。`default_branch_head` が
  best-effort fetch する idiom とはここで意図的に分岐する(**要レビュー合意**)。

## 受け入れ基準

- default branch に無い / working tree でだけ編集された `body_file`・`meguri.toml`・preamble が、
  実行時(scheduler)・doctor の双方で「default branch の内容」基準で扱われる。
- remote 無し(local mode)repo で従来どおり動く(local `<default>` fallback)。
- 既存テスト + 下記の乖離ケースのテストが通る。

## テスト戦略

`src/gitops.rs` の tempdir + 実 git repo idiom(`init_repo` / bare origin)に乗せる:

- 新 gitops 関数: (a) origin 優先で default branch の通常 blob を `Content` で読む、(b) remote 無しで
  local `<default>` fallback、(c) ツリーに無いパス→`Absent`、(d) working tree でだけ変えた値は
  反映されない(commit 済み内容が返る)、(e) default branch 上の **symlink → `NotRegularFile`**
  (リンク先文字列を Content にしない)、(f) **ディレクトリパス → `NotRegularFile`**、
  (g) **`src/foo.rs` の隣に `src` がある状態で `rel="src/"`(末尾スラッシュ)を渡しても Content にならない**
  (配下列挙で先頭 blob を誤採用しない)、(h) submodule gitlink → `NotRegularFile`、
  (i) base ref が無い→`Err`。
- `scheduler_fire`: FakeForge で、working tree 編集済み / default branch に commit 済みの `body_file`
  で fire し、enqueue された body が **default branch の内容**であることを記録に対しアサート。
- doctor 3 helper: default branch に無い / working tree だけの `meguri.toml` / preamble / body_file で
  ✅/❌ 判定が default branch 基準になることを確認。
- config load: `body_file` に `..`/絶対/末尾スラッシュを与えると load が reject されること、および
  `validate_repo_relative` が末尾スラッシュを弾くことを追加。

## 関連

- ADR 0015(本 issue で新設。用途で出所を二分する判断)
- ADR 0011(二層 config: pin は worktree、claim 時固定)/ ADR 0012(preamble containment 二段ゲート)
- #165(repo 側 meguri.toml)
- 後続: managed bare clone 化 issue(本 issue が前提)
