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
**mux の生成境界**(と、既存ペインの宛先を tmux 用に再合成する少数の経路)だけでよい。

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
- 既存ペインを扱う経路は project を渡さなくても成立する(観察 2):
  - `meguri top`(`cmd_top`)→ `None`(base workspace に dashboard、ペインは id でタイル)。
  - scheduler の recovery → `None`(`pane_alive` はペイン id だけで判定、workspace 非依存)。
- **tmux attach のためだけ** project が要る経路 → そのペインの `project_id` を渡す:
  - `cmd_attach` / `cmd_logs` / `cmd_stop` のフォールバック、`reaper::mux_for`。
    project_id は run / pane レコードに載っている(`resolve_attach_pane` を (kind, pane, project)
    を返す形に広げる)。herdr ではラベルは無視され、tmux では正しい session が再合成される。
    **新たな永続化は不要。**
- 既存ペインの移行は不要: ペイン id は据え置きで有効。`upsert_pane` / `update_run_mux` が
  `mux_session` に載せる値は、観察用に実効ラベル(合成後)へ揃えてもよいが、再構成には使わない
  ので必須ではない(スコープ内の任意)。

## 触るファイル

- `src/mux/mod.rs` — `detect` / `from_kind` に `project: Option<&str>`、ラベル合成、doc。
- `src/mux/herdr.rs` — コンストラクタは合成済みラベルを受ける(実質無変更)。doc の
  「one workspace labeled with the configured session name」を per-project に更新。
- `src/mux/tmux.rs` — 同上(合成済み session 名を受ける)。attach が正しい session を使うことの確認。
- `src/app.rs` — `build_deps`(`Some(project.id)`)、`cmd_top`(`None`)、`cmd_attach` /
  `cmd_logs` / `cmd_stop`(ペインの project)、`resolve_attach_pane` を project も返す形に。
- `src/engine/scheduler.rs` — recovery の `from_kind(..., None)`。
- `src/engine/reaper.rs` — `mux_for` が `Some(&deps.project.id)`。
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
   そこに作られる。attach コマンドが当該ペインの session を指す。
4. 既存ペイン(単一 workspace 時代の id)への `pane_alive` / `attach` / `kill` が、workspace 分離
   後も変わらず成立する(ペイン id 指定なので workspace ラベルに非依存)。
5. `detect` / `from_kind` の新 `project` 引数を各呼び出し側が正しく渡す(build_deps=project、
   top/recovery=None、attach/logs/stop/reaper=ペインの project)。
6. README(en/ja)が per-project workspace の命名則を記述している。
7. 既存テストが全て通る(特に mux / scheduler / reaper 系の非破壊)。

## テスト計画

- ラベル決定則は純粋関数なので単体で網羅する(session のみ → base、`Some(project)` →
  herdr `:` / tmux `-`)。`detect` / `from_kind` の合成もここで確認。
- `tests/mux_herdr_test.rs`(`MEGURI_TEST_HERDR=1` ゲート)に、project 付きラベルで作った
  workspace に spawn したタブがその workspace に属することの検査を 1 本足す。既存ペイン互換
  (基準 4)は同ファイルのパターンで、生成後にペイン id 指定の操作が通ることで担保する。
- `tests/mux_tmux_test.rs` に per-project session の spawn/attach を 1 本。
- 呼び出し側の配線(基準 5)は既存のエンジンテスト(FakeMux)が非破壊で通ることで十分
  — FakeMux は session を持たず、この変更の影響を受けない。

## スコープ外(将来の話)

- タブ label を `meguri#<N>` から `<project>#<N>` へ変える整形(workspace で既に分離される
  ため機能上は不要。やるなら別 issue)。
- `meguri top` に「project でグループ表示」等の UI 拡張。
- workspace を跨いだペインの再配置ポリシー(project 変更時など、現状発生しない)。
