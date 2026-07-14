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

**対象外**(ADR 0011 の pin ゆえ worktree のまま正しい):

- 実行時 preamble 解決 `src/engine/flow.rs:1323`
- repo config の claim 時 pin `src/engine/flow.rs:442`

## 変更方針

### 1. gitops に「default branch 上のファイル内容を読む」関数を新設

git 呼び出しは rust.md ルールどおり `src/gitops.rs` に集約する。

```
pub async fn show_file_at_default_branch(
    repo_path: &Path,
    default_branch: &str,
    rel: &str,
) -> Result<Option<String>>
```

- base 解決は既存 idiom(`default_branch_head` / `commits_ahead` などの
  `origin/<default>` を rev-parse、無ければ local `<default>`)と同じ倒し方。
- 戻り値 `Result<Option<String>>` で **3 状態を区別**する(呼び出し側が「無い」の扱いで割れるため):
  - `Ok(Some(content))` — blob が存在し、その内容
  - `Ok(None)` — default branch のツリーに `rel` が**存在しない**
  - `Err(_)` — base ref 自体が解決できない等の git 失敗
- 「存在しない」と「その他の失敗」の分離は、base 解決後に `git cat-file -e <base>:<rel>` の存在プローブ
  → 成功時のみ `git show <base>:<rel>` で内容取得、で行う(2 コール。doctor / scheduler は低頻度)。
- doctor helper は sync(`fn`)だが async な `cmd_doctor` から呼ばれているので、**helper を async 化**して
  この関数を `await` する(`check_auto_merge` が既に async 化の先例)。sync 版
  `run_git_sync` を増やすより素直。

### 2. 呼び出し側の差し替え

- `render_body`(`scheduler_fire.rs`)を **async 化**し(呼び出し元 `fire_one` は既に async)、
  `body_file` を新関数で読む。`Ok(None)`(default branch に無い)は現状の read エラー同様
  `bail!`/`with_context` 相当で fire を失敗させる(sweep はログして次 tick 再試行)。
- `doctor_schedules`: `Ok(Some)`→✅、`Ok(None)`→❌「default branch に無い」、`Err`→❌。
- `doctor_repo_configs`: `Ok(None)`→ opt-out(何も出さない、現状維持)、`Ok(Some(raw))`→
  `RepoConfig` を **文字列からパース**して lint。`load_from_worktree` は working tree 前提なので、
  raw を受けてパースする経路(例: `RepoConfig::parse_str` 相当)に寄せるか、新関数で得た raw を
  既存パースに渡す。`Err`→❌。
- `doctor_prompts`: `resolve_preamble_within`(symlink 追跡 + containment)を新関数に置換。
  `Ok(Some)`→✅、`Ok(None)`→❌「default branch に無い」、`Err`→❌。

### 3. パス検証(issue が明示した確認事項)

`git show <base>:<rel>` は **blob 直読**なので、`resolve_preamble_within` の symlink 追跡込み
containment とは脱出経路の性質が変わる:

- `git show` はツリールート内に構造的に閉じる。`..` はツリー外指定として git が拒否、絶対パスは
  ルート相対扱いで該当 blob 無し(=`Ok(None)`)になる。**symlink を辿らない**(symlink blob は
  リンク文字列そのものを返すだけ)ので、preamble の `Escapes` 経路は構造的に消える。
- preamble パスは config load 時に `validate_repo_relative`(絶対 / `..` 拒否)を既に通る。
  一方 **`body_file` はこの検証を通っていない**(現状 working tree 直読なので `..`/絶対で clone 外に
  出られる軽微な穴が既にある)。統一のため **config load 時の schedule 検証(`validate_schedules`)に
  `validate_repo_relative(body_file)` を追加**し、runtime / doctor の両方で `..`/絶対を一様に弾く。
  git show の構造的閉じ込めはその backstop。

**判断**: 助言 / 発見読み取りでは「`rel` の正規化(`..`/絶対拒否)+ git show の構造的閉じ込め」で足りる。
symlink 追跡 containment は不要(そもそも辿らない)。

### 4. 挙動変化(既知)

- preamble を repo 内 symlink で張っていた場合、旧実装はリンク先の内容を返したが、新実装はリンク文字列を
  返す(= 実質 Missing 扱い)。symlink preamble は稀なエッジケース。挙動変化として記録。
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

- 新 gitops 関数: (a) origin 優先で default branch の blob を読む、(b) remote 無しで local `<default>`
  fallback、(c) ツリーに無いパス→`Ok(None)`、(d) working tree でだけ変えた値は反映されない
  (commit 済み内容が返る)、(e) `git show` が symlink を辿らない。
- `scheduler_fire`: FakeForge で、working tree 編集済み / default branch に commit 済みの `body_file`
  で fire し、enqueue された body が **default branch の内容**であることを記録に対しアサート。
- doctor 3 helper: default branch に無い / working tree だけの `meguri.toml` / preamble / body_file で
  ✅/❌ 判定が default branch 基準になることを確認。
- config load: `body_file` に `..`/絶対を与えると load が reject されることを追加。

## 関連

- ADR 0015(本 issue で新設。用途で出所を二分する判断)
- ADR 0011(二層 config: pin は worktree、claim 時固定)/ ADR 0012(preamble containment 二段ゲート)
- #165(repo 側 meguri.toml)
- 後続: managed bare clone 化 issue(本 issue が前提)
