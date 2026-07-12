# ADR 0005: herdr workspace はプロジェクトごとに分ける(ラベルは `<session>:<project>`)

## ステータス

採用(issue #105)

## コンテキスト

herdr レイアウトは長らく **workspace 1 個**(`mux.session` のラベル、既定 `meguri`)+
**issue ごとに 1 タブ**だった。`HerdrMux` は session ラベルで workspace を find-or-create し
(`src/mux/herdr.rs` の `workspace_id()`)、全プロジェクトの issue タブがそこに同居する。
複数プロジェクトを回すと 1 つの workspace にタブが混ざり、見通しが悪くなる。

一方で `config.rs` には制約がある: `mux.kind` / `mux.session` は daemon のライフタイムで固定で、
リロード時に startup 値へ巻き戻される(`ConfigReloader::poll`)。workspace ラベルの決定則は
この制約と衝突してはならない。

設計を決めるにあたっての鍵となる観察が二つある:

1. **`HerdrMux` は既にプロジェクト単位で生成されている。** `build_deps`(`src/app.rs`)は
   プロジェクトごとに `Deps` を作り、その中で `mux::detect` を 1 回呼ぶ。つまり mux インスタンスと
   プロジェクトは 1:1 に対応しており、workspace ラベルをプロジェクトごとに変える「場所」は既にある。
2. **既存ペインへの操作は workspace ラベルを必要としない。** herdr のペイン id は `wN:pM` の形で
   workspace を内包する。`pane get/run/close/read/wait`、そして `attach_command`
   (`pane.0.split(':')` で workspace を取り出す)はすべてペイン id だけで宛先が決まる。
   workspace ラベルが要るのは **新規コンテナの生成時だけ** — `ensure_session` /
   `spawn_pane`(タブ生成)/ `ensure_dashboard` / `find_workspace` の find-or-create。

## 決定

1. **herdr workspace ラベルを `<session>:<project_id>` にする**(例: `meguri:myproj`)。
   session 部分は据え置きなので config の固定制約と整合する — 変わるのは project 接尾辞だけで、
   これはプロジェクト config の安定キーである。base ラベル `<session>`(接尾辞なし)は
   **`meguri top` の横断ビュー用に予約**する(#96 の dashboard タブが載る workspace)。

2. **プロジェクトは mux の生成時にラベルへ畳み込む。Multiplexer の trait / API は
   プロジェクト非依存のまま変えない。** `detect` / `from_kind` に `project: Option<&str>` を
   足し、kind を知っている生成境界でラベルを合成する(herdr は `:`, tmux は `-`)。
   `HerdrMux::new` / `TmuxMux::new` は合成済みラベルを 1 本受け取る形のままにする。
   これにより「project をエンジン全体へ配線する」必要がなくなる — 観察 2 の通り、
   既存ペインを扱う経路(attach / logs / reaper / recovery / top)はペイン id で宛先が決まるからだ。

3. **既存ペインを扱う経路(attach / logs / kill / recovery / top)には project を渡さない。**
   herdr は `attach_command` が既にペイン id `wN:pM` から workspace id を導く。tmux は attach が
   `self.session` に依存する唯一の経路だが、**`project_id` から session を再合成しない** — 再合成は
   分離前に base session で作られた既存ペインを取り違える(その run の project から作った
   `<session>-<project>` は、当該ペインが実在しない session を指す)。代わりに、**印字するシェル
   文字列の中でペインから実 session を解決する** — `attach -t "$(tmux display-message -p -t %N
   '#{session_name}')"`。これで宛先が真にペイン id 由来になり、分離前の既存ペインも分離後の新規
   ペインも取り違えなく attach でき、attach 経路へ project を配線する必要すら無くなる。結果として
   **project を渡すのは spawn 側(`build_deps`)だけ**で、他の全呼び出し側は `None` を渡す。

4. **tmux フォールバックはプロジェクトごとの session** `<session>-<project_id>` にする
   (`:` は tmux のターゲット指定で予約語なので `-` を使う)。ペイン id `%N` は tmux サーバ全体で
   一意なので get/kill/read は session 非依存で成立する。session が要るのは新規 window 生成
   (spawn 側、project は既知)と attach だけで、後者は決定 3 の自己解決方式で賄う。

## 帰結

- issue タブは自プロジェクトの workspace(`meguri:<project>`)に作られ、workspace 一覧が
  プロジェクト単位で分かれる。単一 workspace 時代に作られた既存ペインは **移行不要** —
  ペイン id はそのまま有効で、変わるのは新規ペインの作成先だけ。その帰結として、base workspace
  `<session>` は `meguri top` の dashboard に加えて分離前の既存ペインも当面同居する
  (移行しないため。cosmetic で、時間とともに issue の close で解消する)。
- `meguri top` は base workspace(`meguri`)に dashboard タブを持ち、各プロジェクト
  workspace からペインを **id 指定で** タイルする(`pane move` は workspace 跨ぎで動く)。
  横断ビューは自然に成立し、#96 の実装は無変更で乗る。
- config の固定制約は温存される: 変わるのは決定則が `session` に足す接尾辞だけで、
  `mux.session` 自体はリロード時に巻き戻り続ける。
- Multiplexer trait は増えない。`detect` / `from_kind` の引数 1 個(`Option<&str>`)だけが
  増える最小の変更で、mux 実装のロジックはほぼそのまま。
