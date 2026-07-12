# issue-105 spec — herdr workspace をプロジェクトごとに分離する

いまの herdr レイアウトは全プロジェクトが 1 つの workspace(`mux.session`、既定 `meguri`)に
同居する。複数プロジェクトを回すとタブが混ざって見通しが悪い。この spec の決定は一行で書ける。
**workspace ラベルを `<session>:<project_id>` にして、issue タブを自プロジェクトの workspace に
作る。** tmux フォールバックは `<session>-<project_id>` の session でこれに対応する。

設計の背骨と根拠は ADR 0005(本 PR 同梱)に置いた。鍵は二つの観察に尽きる:

1. **mux は既にプロジェクト単位で生成されている**(`build_deps` が per-project に `mux::detect`
   を呼ぶ)。ラベルをプロジェクトごとに変える場所は既にある。
2. **既存ペインへの操作は workspace ラベルを要らない**。herdr のペイン id `wN:pM` は workspace を
   内包し、`pane get/run/close/read/wait` と `attach_command` はペイン id だけで宛先が決まる。
   workspace ラベルが要るのは新規コンテナの **生成時だけ**。

この二つから、「project をエンジン全体へ配線する」大改修は不要になる。project を渡すのは
**mux の生成境界(spawn 側)だけ**でよい。既存ペインを扱う経路は project を渡さず、tmux の
attach は印字するシェル文字列の中でペインから実 session を解決する(下記 finding 対応)。

## 決定

### ラベル決定則(論点: `config.rs` の固定制約との整合)

- herdr: `<session>:<project_id>` / tmux: `<session>-<project_id>`(`:` は tmux ターゲットで
  予約語なので `-`)。project 接尾辞なしの base ラベル `<session>` は `meguri top` 用に予約。
- `mux.session` は startup 固定のまま(`ConfigReloader` の巻き戻しは無変更)。決定則が足すのは
  接尾辞だけで、session 自体は動かないので固定制約と衝突しない。

### 生成 API に project を通す(論点: `HerdrMux` は session 名しか知らない)

**trait は変えない。** `Multiplexer` の API はプロジェクト非依存のまま。合成は kind を知っている
生成境界で行う:

- `detect(kind_hint, session, project: Option<&str>)` / `from_kind(kind, session, project)` に
  `project` を足す。`detect` が kind を確定した所でラベルを合成し(herdr は `:`, tmux は `-`)、
  `HerdrMux::new` / `TmuxMux::new` には **合成済みラベルを 1 本** 渡す。
  → 既存の単体テスト(`HerdrMux::new(&label)` を直接呼ぶ `tests/mux_herdr_test.rs` 等)は
    コンストラクタ signature を変えないので壊れない。

### 呼び出し側(論点: pane 作成呼び出し側への project 受け渡し / 既存レコード互換)

- `build_deps`(`src/app.rs`)→ `Some(&project.id)`。これで各プロジェクトの mux が
  自 workspace を find-or-create し、`spawn_pane` のタブがそこに作られる。**これが本 issue の主眼。**
- **既存ペインを扱う経路は project を渡さない(全て `None`)。** これは観察 2 の帰結であり、
  レビュー finding 1 への対応でもある:
  - `meguri top`(`cmd_top`)/ scheduler recovery → `None`(タイル・`pane_alive` はペイン id だけ)。
  - `cmd_attach` / `cmd_logs` / `cmd_stop` フォールバック / `reaper::mux_for` → `None`。
    herdr は `attach_command` がペイン id `wN:pM` から workspace を導くので project 不要。
    tmux も **`project_id` から session を再合成しない**(再合成は分離前に base session で
    作られた既存ペインを取り違える)。代わりに tmux の `attach_command` を、印字するシェル
    文字列の中でペインから実 session を解決する形にする:
    `attach -t "$(tmux display-message -p -t %N '#{session_name}')"`。これで既存ペインも
    新規ペインも取り違えず attach でき、attach 経路への project 配線が丸ごと不要になる。
- 既存ペインの移行は不要: ペイン id は据え置きで有効。`upsert_pane` / `update_run_mux` が
  `mux_session` に載せる値は、観察用に実効ラベル(合成後)へ揃えてもよいが、再構成には使わない
  ので必須ではない(スコープ内の任意)。

## 触るファイル

- `src/mux/mod.rs` — `detect` / `from_kind` に `project: Option<&str>`、ラベル合成、doc。
- `src/mux/herdr.rs` — コンストラクタは合成済みラベルを受ける(実質無変更)。doc の
  「one workspace labeled with the configured session name」を per-project に更新。
- `src/mux/tmux.rs` — コンストラクタは合成済み session 名を受ける。**`attach_command` を
  ペイン由来の session 解決に変える**(`attach -t "$(tmux display-message -p -t %N
  '#{session_name}')"`)。`dashboard_attach_command` は top の mux が base session を所有するので
  現状のままで正しい(対称性のため同方式にしてもよい)。
- `src/app.rs` — `build_deps`(`Some(project.id)`)、他の呼び出し側(`cmd_top` / `cmd_attach` /
  `cmd_logs` / `cmd_stop`)は `None`。`resolve_attach_pane` の拡張は不要。
- `src/engine/scheduler.rs` — recovery の `from_kind(..., None)`。
- `src/engine/reaper.rs` — `mux_for` は `None`(ペイン id で宛先が決まるため)。
- `README.md` / `README.ja.md` — `session` の注釈を「base ラベル。workspace は
  `<session>:<project>`(tmux は `<session>-<project>`)」に更新。
- `docs/adr/0005-per-project-mux-workspace.md` — 決定の記録(本 PR に同梱済み)。
- テスト — 下記。

## 変わらないもの(意図どおり)

- `Multiplexer` trait / 既存ペイン id / panes テーブルのレコード(移行なし)。
- config の `mux.session` 固定制約と巻き戻しロジック。
- `meguri top` の実装(#96)— タイルはペイン id 指定なので workspace 分離に無依存。
- `FakeMux`(session を持たない)と、それに依存するエンジンのユニットテスト。

## 受け入れ基準(acceptance criteria)

1. `project_id = "foo"` で構成した herdr mux が `spawn_pane` するとき、workspace ラベル
   `<session>:foo` を find-or-create し、そこにタブを作る(別 project は別 workspace)。
2. base ラベル `<session>`(project なし)の mux は従来どおり `<session>` workspace を使う
   —`meguri top` の dashboard が載る先。
3. tmux mux は project ごとに session `<session>-<project>` を使い、`spawn_pane` の window が
   そこに作られる。`attach_command` が印字するシェル文字列は、`self.session` ではなく
   `#{session_name}` でペインの実 session を解決する(分離前/後どちらのペインでも当該ペインを表示)。
4. 既存ペイン(単一 workspace 時代の id、旧 base session / workspace 所属)への
   `pane_alive` / `kill` / `attach` が、workspace 分離後も変わらず成立する。herdr は attach が
   ペイン id 由来、tmux は attach が上記の実 session 解決由来で、いずれも project 再合成に依存しない。
5. `detect` / `from_kind` の新 `project` 引数は **`build_deps` のみ `Some(project)`**、他の全呼び出し側
   (top / recovery / attach / logs / stop / reaper)は `None` を渡す。
6. README(en/ja)が per-project workspace の命名則を記述している。
7. 既存テストが全て通る(特に mux / scheduler / reaper 系の非破壊)。

## テスト計画

- ラベル決定則は純粋関数なので単体で網羅する(session のみ → base、`Some(project)` →
  herdr `:` / tmux `-`)。`detect` / `from_kind` の合成もここで確認。
- `tests/mux_herdr_test.rs`(`MEGURI_TEST_HERDR=1` ゲート)に、project 付きラベルで作った
  workspace に spawn したタブがその workspace に属することの検査を 1 本足す。既存ペイン互換
  (基準 4)は同ファイルのパターンで、生成後にペイン id 指定の操作が通ることで担保する。
- `tests/mux_tmux_test.rs` に per-project session の spawn を 1 本。加えて `attach_command` が
  `#{session_name}` 解決を含むシェル文字列を返すこと(= `self.session` を直書きしないこと)を
  文字列アサートで確認 — これが finding 1 の回帰防止になる。
- 呼び出し側の配線(基準 5)は既存のエンジンテスト(FakeMux)が非破壊で通ることで十分
  — FakeMux は session を持たず、この変更の影響を受けない。

## スコープ外(将来の話)

- タブ label を `meguri#<N>` から `<project>#<N>` へ変える整形(workspace で既に分離される
  ため機能上は不要。やるなら別 issue)。
- `meguri top` に「project でグループ表示」等の UI 拡張。
- workspace を跨いだペインの再配置ポリシー(project 変更時など、現状発生しない)。
