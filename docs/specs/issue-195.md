# issue-195 spec — repo_slug 宣言だけで managed bare clone を実体化する

いまの `[[projects]]` は `repo_path`(手で clone した working copy への絶対パス)を必須にし、
「clone は既にある」前提をハードコードしている。この spec は、host が `repo_slug` を宣言すれば
meguri が `~/.meguri/repos/<id>` に bare clone を実体化して所有する、という一手を入れる。
決定の「なぜ」は **ADR 0018**(本 PR 同梱)に置いた。この spec はその「どう」= 触る箇所と
割り切りだけを書く使い捨ての足場である。

## spec 深度: design(理由)

**design spec を選ぶ。** veto ルールに該当する — 永続状態(ディスク上の bare clone)を新規に
作り、公開契約(config schema の `repo_path` 必須→optional)を変える。よって migration /
rollback を必須セクションとして書く。不確実性そのものは中程度(事前調査で影響面は
「config schema + gitops clone 関数 + reconcile 前段 + doctor 分岐」に収まると確認済み)だが、
間違えたときの波及先が「ユーザーの手元 clone」「新規ネットワーク副作用」に届くため深めに倒す。

## 決定 1: `repo_path` を optional にし、実効パスは resolver で解決する

- `ProjectConfig.repo_path: PathBuf` → `Option<PathBuf>`(serde `#[serde(default)]`)。
- 実効パスの解決を一箇所に集約する resolver を足す。既存の `pr_for` / `deliver_for` /
  `language_for` と同じ `*_for` 慣習に倣う:

  ```rust
  // src/config.rs
  impl Config {
      /// github mode で repo_path 省略時は ~/.meguri/repos/<id> に導出。
      /// 明示指定と local mode はその値を使う。
      pub fn repo_path_for(&self, project: &ProjectConfig) -> PathBuf {
          project.repo_path.clone()
              .unwrap_or_else(|| meguri_home().join("repos").join(&project.id))
      }
  }
  ```
- 呼び出し側は `deps.project.repo_path`(30 箇所・10 ファイル)を経由している。`Deps` に薄い
  `repo_path(&self) -> PathBuf` を足し、各所を `&deps.repo_path()` に寄せる。doctor / app の
  `&project.repo_path`(`src/main.rs`・`src/app.rs`)も `cfg.repo_path_for(project)` に置換。
  機械的な置換で、導出ロジックは resolver 一箇所に閉じる。
- **`repos` ディレクトリ helper**を `config.rs` に足す(`worktrees_root()` と対):
  `pub fn repos_root() -> PathBuf { meguri_home().join("repos") }`。

### 検証(`Config::validate`)の更新

現状は「非 local なのに `repo_slug` 無し」を弾いている。ここに:
- **local mode + `repo_path` 省略 → エラー**(clone 元が無いので導出できない)。
- github mode は `repo_path` 省略可(導出する)。`repo_slug` 必須は不変。
- **project `id` を単一パス要素に validate する**(下記)。

### project `id` のパス安全性(finding 対応)

導出パスは `repos_root().join(&project.id)` を作る。`id` は今でも worktree パスの要素
(`worktree_path` / reaper の `project_worktree_root`)だが、これを clone のルートに昇格させると、
`../x` や `a/b` のような `id` で管理 clone が `repos` 配下から逃げる/意図しない階層を作る余地が
広がる。現行 config validation に `id` を安全な文字列へ制限する規則は無い(空文字・重複・パス
要素の検査すべて無い)。

そこで **`Config::validate` に `id` の検査を足す**: 空でないこと、そして**単一パス要素**である
こと(`Path::new(id).components()` がちょうど1個の `Normal` になる — `/`・`\`・`.`・`..`・
先頭 `/` を弾く)。既存の `validate_repo_relative` と同じ「パスとして解釈して危険成分を弾く」
方針に揃える。導出パスは検査を通った `id` だけが作る(escape ではなく reject を採る — escape は
sqlite の run キー等で使う生 `id` と導出パスの `id` がずれてデバッグを難しくするため)。
既存ユーザーの `id` はほぼ英数字なので実害はほぼ無い。もし将来ずれた `id` が必要になっても、
これは load 時の loud なエラーで、silent なパス破壊より安全側に倒れる。

## 決定 2: gitops に clone 関数を新設する

git に触れるロジックは `src/gitops.rs` に集約する原則どおり、clone を gitops に足す。
mux/forge のようなトレイト抽象は無いので `gh` を直接呼ぶ(forge が gh 完全依存なのと一貫)。

```rust
// src/gitops.rs
/// repo_slug の bare clone を dest に作る(gh の credential helper を継承)。
/// 冪等: dest が既に「健全な」bare clone なら no-op、不在なら clone、
/// 壊れた残骸なら loud に失敗する。
pub async fn ensure_bare_clone(dest: &Path, repo_slug: &str) -> Result<()> {
    match clone_health(dest).await {
        CloneHealth::Healthy => return Ok(()),           // no-op
        CloneHealth::Absent => { /* 下で clone */ }
        CloneHealth::Broken(why) =>                       // 途中失敗の残骸など
            bail!("managed clone at {} is broken ({why}); remove it and retry", dest.display()),
    }
    // gh repo clone <slug> <dest> -- --bare
    // その後 remote.origin.fetch を明示設定して初回 fetch:
    //   git -C <dest> config remote.origin.fetch +refs/heads/*:refs/remotes/origin/*
    //   git -C <dest> fetch origin
    ...
}
```

**健全性判定(finding 対応)**: no-op 条件を `HEAD` の存在だけにしない。`dest` が空/不在なら
`Absent`(clone する)。存在するなら、次のすべてを満たすときだけ `Healthy`:
- `git -C <dest> rev-parse --is-bare-repository` が `true`(ただのファイル `HEAD` や non-bare を弾く)、
- `remote.origin.url` が設定済み、かつ `remote.origin.fetch` が期待の refspec
  (`+refs/heads/*:refs/remotes/origin/*`)、
- `refs/remotes/origin/*` が最低1本張られている(初回 fetch まで完了した証拠)。

いずれか欠ければ `Broken` として **loud に bail** する。これで「clone が途中で失敗して `HEAD` だけ
残った」ケースが、次 tick で健全扱いされて後段の git 操作が別の分かりにくいエラーになる、という
finding の穴を塞ぐ。復旧は「壊れた `dest` を消して再 clone」— メッセージにそれを書く
(自動 `rm -rf` はしない。人間の宣言外のディレクトリを消さない安全側)。

要点(すべて ADR 0018 の根拠):
- **bare**(`--bare`)。**`--mirror` は使わない**(mirror refspec が実行中の `meguri/*` を刈る)。
- clone 後に `remote.origin.fetch = +refs/heads/*:refs/remotes/origin/*` を設定し `fetch origin`。
  これで `refs/remotes/origin/*` が張られ、`create_worktree` 等の `origin/<default>` 参照が
  ローカルの古い ref に silent fallback しない。
- remote 名は必ず `origin`(gitops 全関数のハードコードに合わせる)。
- 失敗は `bail!` で loud に返す(認証・ネットワーク・slug 誤記・壊れた残骸)。

## 決定 3: clone は tick 最上段の per-project hook で埋める

「宣言あり・clone 無し」を level-triggered な乖離として、tick ごとに冪等に埋める(ADR 0012)。

**実行位置は「tick の最上段・全処理の前」に固定する。** `prepare_worktree` 前段や discover の中
だけでは足りない。現行 scheduler の1 tick は次の順で `repo_path` を触る
(`src/engine/scheduler.rs`):
1. **redispatch_interrupted** — 中断 run を checkpoint から再開(discover より前)。
2. **discover** — `active_weight < max_concurrent` のときだけ走る(**slot 満杯なら丸ごと skip**)。
3. **sweep 群** — `scheduler_fire` / `reaper` / `auto_merger` / `merge_watch` / `plan_handoff` /
   `decompose_materializer` / `routing_drift` / `reconcile`(discover の外)。

つまり clone を discover に置くと、slot 満杯 tick で skip され、その裏で redispatch が未 clone の
`repo_path` に対して走る。sweep 群も discover の外で `repo_path` を触る。よって clone hook は
**redispatch・discover・sweep のすべてより前**に、**slot 予算に関係なく**、per-project で一度
必ず走らせる。

- 具体的には tick ループ本体の**最初**に per-project の準備段(仮 `ensure_project_ready`)を置き、
  そこで各 github-mode project に `gitops::ensure_bare_clone` を呼ぶ。
- **clone を試みるのは `repo_path` を省略した(=導出パスの)github-mode project だけ**。
  明示 `repo_path` は「host の信頼宣言」なので meguri は勝手に clone しない(finding 対応)。
- 準備に失敗した project は、その tick の redispatch・discover・sweep から**丸ごと除外**する
  (他 project は止めない)。`repo.clone.failed` を emit + escalate。silent skip はしない。
- 成功時は `repo.cloned` を emit(下記 observability)。
- 一回きりの CLI(`meguri run` の one-shot)も同じ helper を prepare の前で呼ぶ。`meguri doctor`
  は例外で clone せず「未 clone」と報告するだけ(決定 4)。

### 明示 `repo_path` が未存在のときの扱い(finding 対応)

移行方針(「明示 `repo_path` は従来どおり」「新規に省いた config だけが clone を誘発」)と
一貫させる。**managed clone は `repo_path` 省略時に限る。** 明示 `repo_path` が指すディスクが
無い場合は、従来どおり後段の git 操作が **loud error** になるに任せる(typo や一時的なパス欠如で
meguri が任意の明示パスへ bare clone を作り始める、という大きな挙動変更を避ける)。よって
`ensure_bare_clone` を呼ぶ条件は「`project.repo_path.is_none()`」だけで判定し、明示パスの存在
チェックで分岐しない。

`prepare_worktree` は clone 済み前提のまま(hook が保証)なので、実装差分は hook の一関数に閉じる。

## 決定 4: `default_branch` の実測はこの spec に含めない(follow-on)

当初は `default_branch` を clone の `origin/HEAD` から実測する案を入れていたが、外す。理由は
**信頼境界が別物**だから。ADR 0011 は `default_branch` を **host 専用(bootstrap)** と明記して
いる —「新規 worktree の base / PR の base /『どのブランチが権威か』の宣言。repo 側に置くと信頼の
自己宣言になる」(ADR 0011 の host/repo 境界表)。これを clone から実測に倒すのは、`repo_path` の
値を導出にする(宣言そのものは host に残る)のとは違い、**権威ブランチの宣言を repo 側の観測値に
移す**変更で、ADR 0011 を明示的に supersede する設計とリスク整理が要る。本 issue の主題(clone の
実体化)には重すぎるので、**後続 issue に分離**する。

- 本 spec では `default_branch` は**現状のまま**(host config、既定 `"main"`)。`Option` 化も
  resolver 化もしない。
- 実測が欲しい `master` 系 repo は、これまでどおり `default_branch = "master"` を明示する。
- 分離しても非破壊: managed clone は `remote.origin.fetch` 経由で全 `refs/remotes/origin/*` を
  張るので、後続 issue が `origin/HEAD` から実測する余地は残る。

## 決定 5: doctor の分岐と gh auth write 検査

- doctor が `repo_path` にディスク前提で git を打つ箇所(`read_file_at_default_branch` を使う
  schedules / repo config / preamble の3セクション、`src/main.rs`)で、「宣言済み・未 clone」を
  **正常系**(これから reconcile、情報表示)として扱い、`❌` にしない。
- 「clone 失敗」(過去に試みて残骸/エラー)とは表示を区別する。受け入れ基準で担保。
- **gh auth の write 権限検査を doctor に追加**する。現状 read-only トークンでも discovery は
  通り、`push_branch` / `create_pr` で初めて落ちる。clone 所有で認証責務が meguri に寄るので
  ここで前倒し検出する(例: `gh api` で repo の push 権限、または `gh auth status` のスコープ確認)。

## 変更箇所

- `src/config.rs` — `repo_path` を `Option` 化、`repo_path_for` / `repos_root()` 追加、
  `validate`(local の repo_path 必須、project `id` のパス安全性)、
  `INIT_TEMPLATE`(repo_path をコメント化し「省略で導出」を明記)。`default_branch` は無変更。
- `src/gitops.rs` — `ensure_bare_clone` 新設 + テスト。
- `src/engine/mod.rs`(`Deps::repo_path()`)/ `src/engine/scheduler.rs`(tick 最上段の
  per-project 準備 hook) — clone reconcile。
- `src/engine/{reaper,cleaner,triage,flow,pr_reviewer,scheduler_fire,conflict_resolver,
  decompose_materializer}.rs` — `deps.project.repo_path` → `deps.repo_path()` の機械置換。
- `src/main.rs` / `src/app.rs` — doctor 分岐 + gh auth write 検査、`&project.repo_path` の置換。
- `README.md` / `README.ja.md` — 最小構成(`id` + `repo_slug`)、managed clone の置き場所と所有、
  local mode は `repo_path` 必須の明記。
- `docs/adr/0018-managed-clone-derives-repo-path-from-slug.md` — 決定の記録(本 PR 同梱済み)。
- `tests/*.rs` — 統合テスト(下記テスト計画)。

## architecture impact / 影響

- 実行経路は既に worktree 経由に decouple 済み(pane cwd = worktree、repo `meguri.toml` は
  worktree から pin、agent-skills `--project` は cwd ベース、cross-repo workspace は slug ベース、
  fetch 系は best-effort でオフライン耐性あり)。よって bare 化の影響は上記の狭い面に収まる。
- 先行の git show 化(ADR 0015 / #194)は着地済み。doctor / scheduler_fire の repo 側読みは
  `read_file_at_default_branch`(bare でも `ls-tree`/`cat-file` が動く)経由なので、bare 化で
  壊れない。残る doctor 課題は「clone が無い」ときのパス欠如だけで、決定 5 が埋める。
- 新しい副作用はネットワーク越しの clone(host の gh 認証)。tick に入るので、初回は clone の
  ぶんだけ遅くなる(冪等 no-op なので2回目以降は HEAD 存在チェックのみ)。

## alternatives considered

- **`repo_path` に non-bare の primary checkout を導出**: 却下。reaper の primary 保護は
  worktree_root プレフィックスのみで、checkout が dirty/branch 保持で競合を生む。bare が
  「meguri 所有・触る余地ゼロ」を構造で保証する(ADR 0018)。
- **`--mirror` で clone**: 却下。mirror refspec が `fetch --prune` で実行中の `meguri/*` を刈る。
- **clone を run の `prepare_worktree` / discover だけに置く**: 却下。redispatch は discover より
  前に走り、discover は slot 満杯 tick で丸ごと skip され、sweep 群は discover の外で `repo_path` を
  触る。よって tick 最上段の hook 以外だと未 clone のまま進む窓が残る(決定 3)。
- **明示 `repo_path` 未存在でも clone する**: 却下。typo や一時的なパス欠如で meguri が任意の
  明示パスへ bare clone を作り始めるのは挙動変更が大きい。managed clone は `repo_path` 省略時に
  限る(決定 3)。
- **`repo_path` を必須のまま `git init`+`remote add` を別コマンド化**: 却下。手動手順が残り、
  「宣言だけで動く」という受け入れ基準を満たさない。

## migration & rollback(必須)

**前方**:
- 既存 config(`repo_path` 明示)は無変更で動く。resolver は明示値をそのまま返し、clone も
  しない(ディスク存在時)。破壊的変更なし。
- 新規に `repo_path` を省く config だけが clone を誘発する。オプトインに近い(書かなければ
  従来挙動)。
- 永続状態の追加は `~/.meguri/repos/<id>` 配下のみ。sqlite schema 変更は無し
  (`default_branch` は無変更で host config のまま、DB にも持たせないため)。

**rollback**:
- コードを戻せば、`~/.meguri/repos/<id>` の bare clone は孤児ディレクトリとして残るだけで
  無害(worktree_root の外なので reaper も触らない)。手動 `rm -rf` で消せる。
- config を `repo_path` 明示に戻せば旧挙動へ即復帰。導出 clone に依存した worktree は
  repo_path 先を張り替えれば作り直せる(worktree は使い捨て)。
- schema マイグレーションが無いので DB のダウングレード手順は不要。

## observability

- `repo.cloned`(slug / dest)/ `repo.clone.failed`(slug / dest / 理由)を emit。
- clone 失敗は既存の escalate 経路(needs-human / notify)に乗せ、silent skip しない。
- doctor が project ごとに「clone 済み / 未 clone(これから reconcile)/ clone 失敗」を
  区別表示し、gh auth write 権限の可否を出す。

## test strategy

- **gitops 単体**: `ensure_bare_clone` をローカル bare origin(統合テストが既に使う土台)に対して
  実行し、(1) `refs/remotes/origin/*` が張られる (2) `remote.origin.fetch` が期待値
  (3) 2回目呼び出しが no-op (4) **壊れた `dest`**(空ディレクトリに `HEAD` という普通のファイルだけ、
  または non-bare な git repo)で **loud に失敗**すること、を検証。`gh` に依存しない経路
  (ローカルパス clone)でテストできる形にするか、gh 依存部を薄く分けてローカル origin で叩ける
  ようにする。
- **config 単体**: `repo_path_for` の導出(省略時 `~/.meguri/repos/<id>`、明示時そのまま)、
  local mode + repo_path 省略の validate 拒否、github mode 省略の許容。**project `id` の
  パス安全性**: `../x` / `a/b` / 先頭 `/` / 空文字の `id` を validate が拒否し、通常の英数字 `id` は
  通ること。
- **reconcile 前段**: repo_slug のみの Deps で tick を回し、未 clone → clone 実体化 →
  従来フロー(worktree→turn→PR)が FakeForge / scripted agent で通ること。clone 失敗時に
  loud skip + escalate すること。
- **reaper 非回収**: `~/.meguri/repos/<id>` が worktree として誤分類されないこと
  (プレフィックス比較のリグレッションガード)。
- **doctor 表示**(受け入れ基準 5 に対応): (1) 導出パスが未 clone の github project を doctor が
  **正常系(未 clone・これから reconcile)**として表示し `❌` にしない (2) 壊れた `dest`
  (途中失敗の残骸)を **clone 失敗**として区別表示する (3) gh の **write 権限不足**を検出して
  表示する、を最低でも単体または統合テストで確認する。gh 依存部は薄いラッパに分けてフェイク
  可能にし、doctor 判定ロジック自体はネットワーク無しでテストする。
- **既存テスト非破壊**: `repo_path` 明示の全既存テストが無変更で通ること。

## 受け入れ基準

1. `[[projects]]` に `id` + `repo_slug` だけ書けば、watch/run が `~/.meguri/repos/<id>` への
   bare clone を自動実体化し、従来フロー(worktree → turn → PR)が通る。
2. `repo_path` 明示指定の既存 config が無変更で動き続ける(clone もしない)。
3. local mode は従来どおり `repo_path` 必須のまま動く(省略は validate で拒否)。
4. reaper / cleaner が管理 bare clone を worktree として誤回収しない。
5. doctor が「未 clone(これから reconcile)」と「clone 失敗」を区別して表示し、gh auth の
   write 権限の可否を表示する。
6. clone 失敗(認証・ネットワーク・slug 誤記)は loud に escalate し、silent skip しない。
   壊れた `dest`(途中失敗の残骸)も次 tick で健全扱いせず loud に失敗する。
7. パス要素として危険な project `id`(`../x` / `a/b` / 先頭 `/` / 空文字)は config load 時に
   loud に拒否される。

## スコープ外(follow-on)

- **worktree_setup の copy 系 secrets**: `.env` / `.claude/settings.local.json` を `repo_path`
  から `cp` する運用は、managed clone では「人間が維持する元(working copy)」が消える。ただし
  これは meguri のコードではなくユーザーの `worktree_setup.commands` の運用で、本 issue の
  受け入れ基準に無い。host 側供給源(例: `~/.meguri/secrets/<id>/`)を明示する設計は別 issue に
  分離する。README で「managed clone では repo_path 由来の cp は使えない」旨だけ触れる。
- **`meguri add` の onboarding コマンド**(後続 issue、関連に記載)。
- **`default_branch` の clone からの実測**(決定 4)。ADR 0011 が host 専用に置く権威ブランチの
  宣言を repo 側観測値に倒す別の信頼境界変更なので、ADR 0011 を supersede する別 ADR を伴う
  後続 issue に分離する。
