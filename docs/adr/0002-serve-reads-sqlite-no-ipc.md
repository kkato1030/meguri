# ADR 0002: web ダッシュボードは SQLite 直読みの独立プロセス、watch 生死は DB ハートビートで判定

- Status: superseded — `meguri serve` は issue #95 で撤去。将来のダッシュボードは
  mux ネイティブな `meguri top`（別 issue）に寄せる。heartbeat 機構（heartbeats
  テーブル + `Store::latest_heartbeat`）は `meguri top` の watch 生死表示のため温存。
- Date: 2026-07-11 (superseded: 2026-07-12)
- Issue: #36 (superseded by #95)

(採番メモ: 0001 は issue #25 の spec PR で採番済み・未マージのため 0002 を使う)

## Context

`meguri serve` で読み取り専用の web ダッシュボードを提供するにあたり、サーバーが
状態をどこから読むかが論点。選択肢は (a) watch プロセスへの IPC / HTTP 問い合わせ、
(b) SQLite 直読みの独立プロセス。

ADR 0001 で「CLI↔daemon IPC を持たない — 状態はすべて共有ストレージ(sqlite +
state ファイル)にある」と決めており、web UI も同じ前提に立てる。加えて meguri は
`meguri run` 単発(watch なし)でも使われるため、watch の存在を前提にすると
UI が使えないケースが生まれる。

## Decision

1. **serve は `Config::load` + `Store::open` だけの独立プロセス。** watch への
   IPC は持たない。CLI の pause/stop が `desired_state` カラム経由で watch と
   協調しているのと同型に、read path はすべて sqlite。ペイン出力だけは
   `run.mux_kind` / `run.mux_pane_id` から mux を復元して読む(`meguri logs` と
   同じ経路)。watch が動いていなくても過去 run を閲覧できる。
2. **watch の生死は heartbeats テーブルで判定する。** scheduler が tick ごとに
   1 行(`name='watch'`)を UPSERT し、serve は鮮度(poll interval の 2 倍 + 余裕)
   で alive を導出する。events テーブルに書く案は、tick ごとに行が無限に増え
   イベントトレイルを汚すため不採用。プロセス生死を pid で推測するのではなく
   「仕事をしている証拠」を見る(ADR 0001 の flock と同じく構造的判定)。
3. **UI はビルドステップなしでバイナリに埋め込む。** 単一 HTML + vanilla JS を
   `include_str!` で埋め込み、node ツールチェーンを持ち込まない。更新は
   ポーリングで始める(SSE は Phase 3)。

## Consequences

- serve と watch は完全に疎結合: どちらか片方だけでも動き、`meguri watch --serve`
  の同居起動も「同じ Store を渡して両方 spawn する」だけで足りる。
- Phase 2 の制御操作(pause/stop の POST)も `desired_state` への書き込みで
  実現でき、IPC 導入は不要のまま(app.rs のロジック共有化のみが課題)。
- ライブ性はポーリング間隔と heartbeat 鮮度に律速される。「即時」が要件になったら
  SSE(Phase 3)を DB ポーリングの上に足す — 直読みという土台は変わらない。
- sqlite は WAL + busy_timeout で並行リーダーに耐えるが、serve が重いクエリを
  持ち込まないことが前提(Phase 1 は全て単純な SELECT)。
