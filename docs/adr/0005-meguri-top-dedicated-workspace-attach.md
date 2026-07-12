# ADR 0005: meguri top は専用 workspace を構成して起動時に attach し、status ヘッダはペイン内で描画する

- Status: accepted
- Date: 2026-07-12
- Issue: #104(#96 / #101 の top 新設を継承・改修)

## Context

`meguri top`(#96 / #101)は「ライブ agent ペインを 1 つの mux コンテナにタイルした
ターミナルダッシュボード」として新設された。しかし実運用すると herdr を使っている
実感が得られない。原因は挙動が二重に呼び出し元ターミナル寄りに寄っていることにある。

1. **画面が herdr 側に立ち上がらない。** 現状の `ensure_dashboard` は既存 meguri
   workspace の中に `meguri:top` ラベルの**タブを 1 つ**足すだけで、status ヘッダは
   `cmd_top` を実行した**呼び出し元ターミナルの stdout に print** される
   (`src/app.rs` `render_top`)。herdr 側では新タブが 1 個増える以外に見えるものが
   増えず、ユーザーが自分で attach hint を打たない限りタイルされた画面に到達できない。
   ダッシュボードとしての「開いたら見える」体験が欠けている。

2. **タブ単位のダッシュボードは workspace 分離と噛み合わない。** 関連 issue で
   「プロジェクトごとの workspace 分離」が予定されており、ダッシュボードが
   agent 用 workspace の内側のタブだと、workspace をまたぐレイアウトの置き場所が
   なくなる。ダッシュボードは agent 群とは独立した器であるべき。

`meguri attach`(`src/app.rs` `cmd_attach`)は既に「解決したペイン/コンテナへ
`exec` で attach して即座に画面へ入る」方式を確立している。top も同じ土俵に乗せられる。

## Decision

1. **`meguri top` は専用 workspace(label `"<session>:top"`、既定で `meguri:top`)を
   herdr 側に構成する。** agent ペインが住む workspace(`<session>`)とは別の器にする。
   `ensure_dashboard` の返す `DashboardId` の意味を「agent workspace 内のタブ」から
   「専用 workspace 内の、ペインをタイルするタブ」へ移す。冪等: 同ラベルの
   workspace / タブが既にあれば再利用し、二重生成しない。

2. **status ヘッダは呼び出し元 stdout ではなく、ダッシュボードの status ペイン内で
   描画する。** `meguri top` は専用 workspace の status ペインで内部ループコマンドを
   `pane run` し、そのループが (a) panes テーブルからペインを解決してタイルし、
   (b) `render_top` の出力を**自分のペインの端末**へ `\x1b[2J\x1b[H` で in-place 更新
   する。呼び出し元プロセスは描画に関与しない。

3. **`meguri top` は起動時に専用 workspace へ `exec` attach する。** `cmd_attach` と
   同じ `exec` 方式(herdr は `workspace focus … ; herdr`、tmux は `tmux attach`)。
   実行した瞬間にタイル済み画面へ入る。冪等: 2 回目の `meguri top` は既存の
   workspace / status ペインを再利用し、ループを二重起動せず attach だけ行う。

4. **ペインは run 記録ではなく panes テーブル(issue の永続ペイン)から解決する。**
   1 issue = 1 pane の永続ペインが正であり(`src/store/panes.rs`、
   `resolve_attach_pane` のコメント)、run 開始時のスナップショット
   `run.mux_pane_id` は古くなりうる。top のタイル/生死判定/AGENT 列も
   `resolve_attach_pane` と同じ優先順位(panes テーブル優先、無ければ run 記録)で
   解決する。

5. **tmux フォールバックは専用 session を対応物にする。** 既存 meguri session 内の
   window ではなく、ダッシュボード専用 session(または少なくとも `tmux attach` で
   直接開ける単位)を器とし、`join-pane` + `select-layout tiled` でタイルする。

## Consequences

- 「開いたら見える」ダッシュボードになる: `meguri top` 一発で専用 workspace に
  入り、status ヘッダとタイルされた agent ペインが即座に見える。attach hint を
  人手で打つ必要がなくなる(hint は「後からもう一度入る」ための補助に降格)。
- status ループがダッシュボード内プロセスとして常駐するため、呼び出し元ターミナルは
  attach に専念できる。ループはペインの生死とともに畳まれる。
- ダッシュボードが独立 workspace になったことで、将来のプロジェクト別 workspace
  分離と器の階層が衝突しない。ペインの移動はペイン id 指定なので、agent が
  どの workspace にいてもダッシュボード workspace のタブへタイルできる
  (**要検証**: herdr `pane move --tab` が workspace をまたいで移動できること。
  できなければ移動先タブを agent workspace 側に置くなどの代替をとる)。
- ペイン解決を panes テーブルに寄せたことで、古い run 記録由来の
  `pane_not_found` / 全行 `unknown` が解消し、`meguri attach` と同じ 1 本の
  解決規則に一本化される。
- 内部ループコマンドを status ペインで走らせる分、`meguri top` は「外側=セットアップ
  + attach」「内側=ループ + 描画」の 2 役に分かれる。内側は隠しサブコマンド
  (または隠しフラグ)で表現し、public な `top` の help を汚さない。
