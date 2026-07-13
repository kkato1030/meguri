# issue-154 spec — workspace: 関連 project の静的グルーピング(cross-repo のスコープ宣言と表示単位)

この issue の決定は一行で書ける。**project の上に `workspace` = 関連 project の静的な
グルーピングを config にだけ足し、decompose の起票スコープ・cross-repo blocker の解決範囲・
`ps`/`top` の表示単位、の 3 つだけに使う。実行系(run / turn)には一切触れない。**

なぜこの形なのか(実行系を拡張せず静的宣言だけで跨ぎを表す / スコープ拡大権限はホスト運用者に
限定する / 人間ノードは既存の「ラベルなし子 issue」で表す)という設計判断は spec より長生き
するので、本 PR 同梱の **ADR 0009** に置いた。この spec は「どこを、どう触るか」に絞る。

## 前提の確認(コードで裏取り済み)

- **dependency gate は blocker の状態しか見ない。** `src/tasks.rs:143` の
  `has_unresolved_blockers` は `forge.blocked_by(issue)` の各 blocker について
  `!b.resolved()` を数えるだけで、`resolved()` は `state == "closed" && state_reason ==
  "completed"`(`src/forge/mod.rs:273`)。読めなければ `Err(_) => true`(未解決扱い)。
  → cross-repo でも blocker 状態が inline で取れれば **gate は無変更で動く**。
- **forge は project(repo)ごとに 1 個。** `build_coordination`(`src/app.rs:33`)が
  `GhForge::new(&slug)` を project ごとに作る。cross-repo は「別 project の forge を
  もう 1 個作る」で表現でき、trait を跨がせる必要はない。
- **decompose の子起票は親の forge 1 個に固定。** `on_decompose`(`src/engine/planner.rs:224`)
  は `deps.forge()`(親 repo)にしか `create_issue` / `add_blocked_by` しない。跨ぎ起票は
  ここを sibling forge へ広げる。
- **mux workspace は既に `<session>:<project>`**(ADR 0005)。表示グルーピングは
  mux 層を増やさず、`ps`/`top` のレンダリングで束ねれば足りる。

## 決定(論点への回答)

### 論点 1・2: cross-repo blocker の読みと workspace 境界

- **読み(discovery)は原則コード不変。** GitHub の
  `repos/{repo}/issues/{n}/dependencies/blocked_by` は cross-repo の blocker も
  `state`/`state_reason` を inline で返す想定。返るなら既存 gate がそのまま順序を与える。
  **実装時に実地検証すること**(下記「検証」)。inline で状態が取れないと分かった場合の
  フォールバックは、**workspace 内 sibling に限って** blocker repo の forge を引いて状態を
  解決する経路を足す(引ける forge = config に宣言された workspace sibling のみ、が
  スコープ境界の実装上の担保になる)。
- **workspace 外 repo への blocked-by は「未解決扱いで止める」。** meguri は workspace 外の
  forge を新規に引かない。inline 状態が付いていればそれは尊重するが、meguri から能動的に
  workspace 外 repo を解決しに行くことはしない → 取れなければ安全側(blocking)に倒れる。
  これで「起票スコープ = 解決スコープ = workspace」という一貫した境界になる。

### 論点 3: 表示だけ束ねる(mux 層は挟まない)

`meguri:<project>` の per-project workspace(ADR 0005)はそのまま。workspace は
`ps`/`top` の**レンダリング上のグルーピング**にのみ効かせる。実行系(pane 生成先)に
workspace 層を挟むと不変条件「実行系に現れない」を破るため、挟まない。

### 論点 4: 親 issue(tracking)の住処

分解対象の issue がある repo のまま(既存挙動と不変)。cross-repo decompose でも
**親は origin repo に留まり、子だけが sibling を指せる**。この慣習を planner の
decompose 指示文とコメントに明文化する。

### 論点 5: 人間ノードの運用

「ラベルなし子 issue = 未トリアージ = 人間の作業」で足りる。追加シグナル(assignee 等)は
必須にしない。実装としては decompose の子に `kind = "human"` を足し、**トリガーラベルを
一切付けずに** issue を起票する(discovery が拾わない = 人間が閉じるまで依存側が止まる)。
本文にはその旨の注記フッターを付す。

## 変更箇所

### 1. config に `[[workspaces]]` を足す — `src/config.rs`

```toml
[[workspaces]]
id = "shop"
projects = ["shop-api", "shop-web", "shop-infra"]
```

- `WorkspaceConfig { id: String, projects: Vec<String> }` と `Config.workspaces:
  Vec<WorkspaceConfig>`(`#[serde(default)]`)を追加。
- `Config::validate()` に検証を追加(既存の repo_slug 検証と同じ hard-fail 方針):
  - `projects` の各要素が定義済み project を指すこと(未定義 project 参照を拒否)。
  - 同一 project が複数 workspace に重複所属していないこと。
  - (任意)workspace `id` の重複、空 `projects` の拒否。
- ヘルパ: `Config::workspace(id) -> Option<&WorkspaceConfig>`、
  `Config::workspace_of(project_id) -> Option<&WorkspaceConfig>`、
  `Config::workspace_siblings(project_id) -> Vec<&ProjectConfig>`(自身を除く同 workspace の
  project 群 — decompose 起票スコープと cross-repo 解決範囲の両方が使う)。
- `INIT_TEMPLATE` にコメントアウトした `[[workspaces]]` 例を追記。
- 単体テスト: パース、未定義参照の拒否、重複所属の拒否、workspace 無し config の不変、
  `workspace_of`/`workspace_siblings` の解決。

### 2. `meguri doctor` に workspace 検証を出す — `src/main.rs`(`cmd_doctor`)

`config.validate()` が load 時に既に hard-fail するので不正 config は doctor の config 行で
落ちる。加えて、正常時に workspace 一覧とメンバー(project 参照の妥当性)を 1 セクションで
出す(routing プロファイル一覧の隣に並べる形。AC 1 の「doctor が検証する」を可視化)。

### 3. decompose を workspace 内 sibling repo へ広げる — `src/turn/prompts.rs`, `src/engine/planner.rs`

- `ChildIssue` に `#[serde(default)] project: Option<String>`(起票先 project id。省略 =
  親と同 repo、既存挙動)と、`kind` に `"human"` を追加。
- planner の decompose 指示文(`decompose_instruction` / execute プロンプト)に、
  親 project の **workspace sibling の id 一覧**と「子は sibling を `project` で指定できる
  / 親は origin repo に留まる / 不可逆操作は `kind:"human"` のラベルなしノードにする」旨を
  注入する(workspace 未定義なら従来通り自 repo のみ、の文面)。
- `validate_children`: `project` が指定されたら親の workspace sibling(または親自身)で
  あることを検証。範囲外を **拒否**(ADR 0009 のスコープ境界の実装上の要)。`kind` に
  `"human"` を許可。
- `on_decompose` の materialization:
  - 子ごとに起票先 forge を選ぶ(自 project は `deps.forge()`、sibling は
    その project の slug から `GhForge::new` を都度生成)。project→forge の解決ヘルパを
    足す(local mode の sibling は起票不可 → 検証で弾く)。
  - `child_label`: `"human"` は**ラベルなし**(空)で起票。`"ready"`/`"plan"` は従来通り。
  - `blocked_by` の配線を cross-repo 対応にする(下記 4)。子→子・親→子の両方。

### 4. cross-repo な `add_blocked_by` — `src/forge/mod.rs`, `gh.rs`, `fake.rs`

現状の `add_blocked_by`(`gh.rs:493`)は blocker の DB id を **自 repo から**引いてから
依存側 repo に POST する。cross-repo では blocker が sibling repo に居るので、blocker の
DB id を **blocker の repo から**引く必要がある。

- trait に blocker の repo を渡せる経路を足す。案: `add_blocked_by_in(&self, issue: i64,
  blocker_repo: &str, blocker: i64)`(既存 `add_blocked_by` は `blocker_repo = self.repo`
  で委譲)。`issue_id` は GitHub 全体で一意なので、id さえ blocker の repo から取れれば
  POST 先(依存側 repo = `self.repo`)は不変。
- `fake.rs` は cross-repo dependency を記録できるよう最小拡張(planner の cross-repo
  decompose テストの土台)。

### 5. `ps` / `top` の workspace グルーピング — `src/app.rs`

- `cmd_ps`: run を project→workspace で束ねて表示。workspace に属す project の行は
  workspace 見出しの下にまとめ、無所属 project は従来通り。列追加ではなく見出しグルーピングで
  最小変更にする(既存の整列書式は維持)。
- `top_refresh` のレンダリング: 同様に workspace 単位で行を束ねる。mux のタイル先
  (dashboard)や pane 解決ロジックには触れない(表示のみ)。

### 6. ドキュメント — `README.md` / `README.ja.md`

`## Configuration` に `[[workspaces]]` の節を追加(3 用途と「実行系に現れない/状態を持たない/
スコープはホスト運用者が config で決める」を要約、ADR 0009 へのリンク)。

## 検証(実装時に必ず行う)

1. **GitHub cross-repo dependencies の実地確認**(論点 1/2 の前提):
   - sibling repo の issue を blocker にした `blocked_by` を実 API で設定できるか
     (`add_blocked_by_in`)。
   - 依存側 issue の `blocked_by` 取得が cross-repo blocker を `state`/`state_reason` 付きで
     返すか。返れば gate 無変更で AC 3 が成立。返らなければ §決定・論点 1 のフォールバック
     (workspace sibling forge で解決)を実装。
   - どちらに転んでも「読めない = 未解決」フォールバックが効くことを確認。
2. `cargo test`(config・planner・forge fake のユニット/結合)。
3. workspace 未定義の既存 config で `doctor` / `ps` / `top` の出力が完全に不変であること
   (AC 5 の回帰)。

## 受け入れ基準

1. `[[workspaces]]` を config に書け、`meguri doctor` が project 参照の妥当性
   (未定義 project・重複所属)を検証する。
2. decompose の提案が workspace 内の別 repo への子 issue を含められ、materialization が
   その repo に起票 + cross-repo dependency を設定する。範囲外 repo への起票は拒否される。
3. workspace 内の別 repo の blocker が未完了の間、discovery が依存側 issue をスキップし、
   blocker 完了後に自然に着手される。
4. `meguri ps` / `meguri top` が workspace 単位のグルーピングで表示できる。
5. workspace を定義しない既存 config の動作が完全に不変(概念は opt-in)。
6. 不可逆操作を `kind:"human"` のラベルなし子 issue として起票でき、それが完了するまで
   依存側 issue が止まる。

## やらないこと

- 複合 worktree・PR セット・repo 跨ぎの原子的 deliverable(run は単一 repo のまま)。
- task の home repo の移動。
- workspace の動的状態・保存層(sqlite テーブルを作らない)。
- 承認ゲートや ops 実行モードの新設(人間ノードで代替)。
