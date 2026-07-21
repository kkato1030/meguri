# ADR 0028: forge/mux のコマンド失敗は needs-human ではなく `infra` イベント + 集約通知

- Status: accepted
- Date: 2026-07-21
- Issue: #250
- 関連: ADR 0012(集約エスカレーション・needs-human は一箇所に集約)・ADR 0020(通知シンクは
  イベント駆動・best-effort)・`docs/design/needs-human-friction-and-delivery-speed.md` §3-E / P6

## Context

ADR 0012 は「needs-human を貼れるのは `escalation.rs` だけ」という不変条件を立てた。しかし
`run_flow`(`src/engine/flow.rs`)の catch-all は run の失敗理由を区別せず、すべてを
`Flavor::escalate`(既定は `escalation::escalate_task`)に流し込んでいた。結果、herdr が落ちている
間の `MuxError::Io(ConnectionRefused)`(`ensure_pane` → `deps.mux.ensure_session()` /
`spawn_pane()`)のような**リトライすれば直る接続断**が、判断待ちの `NeedsHuman` エラーと
まったく同じ経路で issue に `meguri:needs-human` を貼っていた。実測(design doc §2)では
07-14 以降この種の重複 escalation が 23 件観測され、人間の TODO リストを機械的な瞬断が占拠していた。

## Decision

1. **`run_flow` の失敗経路を二分岐にする**。`super::escalation::infra_reason(&err)` が
   `Some(reason)` を返す場合(エラーチェーンに `crate::mux::MuxError`、または `gh` の
   プロセス起動そのものが失敗したことを表す専用型 `crate::forge::gh::GhSpawnFailed` がある
   場合)は forge/mux コマンド故障とみなし、`flavor.release_claim`(claim だけ外す — 次 sweep
   が再試行できる)+ `escalation::escalate_infra` を呼ぶ。それ以外(`NeedsHuman` を含む従来の
   全ケース)は `flavor.escalate` のまま、挙動は不変。
   自己レビュー finding f1(issue #250)の指摘どおり、**裸の `std::io::Error` はチェーンに
   あるだけでは分類しない**。`io::Error` は git(`src/gitops.rs`)・direct-mode エージェントの
   `cmd.spawn()`・prompt/log のファイル書き込みなど forge/mux 以外からも大量に発生するため、
   型でその境界を狭く保つ(下記 §3)。
2. **`infra` はイベントも通知も needs-human と別トークンにする**。`escalate_infra` は
   `infra.raised` イベントを毎回 emit する(統計上、`escalation.raised`/needs-human 系のカウンタ
   から自然に除外される)一方、通知(`Notification::infra`)の `dedup_key` は **issue/run 番号を
   含まず `reason` のみ**にする。これにより同一原因(例: `mux_connection_refused`)で N 個の
   issue が同時に落ちても、`Notifier` の既存 throttle 窓の中では通知は1本に収束する
   (「同一 reason は backoff 付きで1本化」)。`infra` は ADR 0020 の allowlist に新規トークンとして
   追加し、既定では非購読(既存ホストの通知挙動は無改変)。
3. **分類は型で行い、文字列パターンマッチも「io::Error なら infra」という緩い型判定もしない**。
   forge 呼び出し(`gh` CLI)の失敗の大半は `bail!` による無型のエラー文字列で、ビジネスロジック
   上の拒否(権限不足・404 等、人間の判断が要る)と接続断を安全に区別する材料が無い。無理に
   stderr を文字列一致させる代わりに、`src/forge/gh.rs` の `gh`/`gh_try`/`gh_stdin`/`create_repo`
   —— `gh` プロセスの起動そのものが失敗する4箇所 —— だけを専用の `GhSpawnFailed` 型で包む。
   `infra_reason` はこの型と `MuxError` のみを見る。裸の `std::io::Error` は(git・direct-mode
   エージェント spawn・ファイル I/O など出処が多すぎるため)一律で対象外とし、従来どおり
   needs-human 側に残す(過検知より見落としを許容しない側に倒す)。

## Consequences

- mux が停止している間に sweep が回しても、issue は `meguri:needs-human` を受け取らず、working
  claim だけを外されて次の sweep to 再試行される(受け入れ基準)。人間は `infra` 通知(購読時)で
  瞬断の存在を知れるが、対応不要な限りは needs-human の TODO リストに現れない。
- ADR 0012 の不変条件(`escalation.rs` だけが needs-human を貼る)は保たれる。`escalate_infra` は
  同じファイルに置かれているが needs-human ラベルには一切触れない、別分類の関数として明確に分離。
- config 編集中の一瞬の profile 消失(design doc §3-E の別事例)は、forge/mux コマンド故障ではなく
  `routing.rs` の設定解決エラーなので、本 ADR の分類には入らない。別課題として扱う。
- `gh` プロセス起動失敗以外の forge 側ネットワーク断(`gh` プロセス自体は起動しレスポンス無しで
  非ゼロ終了するケース)は無型のため今回は `infra` に分類されない。型を持たせる forge 側の変更は
  スコープ外(必要になれば別 issue)。
- 裸の `std::io::Error`(git・direct-mode エージェント spawn・prompt/log のファイル書き込み等)は
  引き続き `flavor.escalate` 経由で needs-human に残る。これは意図した保守的側であり、自己レビュー
  finding f1 の回帰テスト(`src/engine/escalation.rs::tests::infra_reason_is_none_for_a_bare_io_error`)
  で固定した。
