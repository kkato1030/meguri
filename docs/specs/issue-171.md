# spec: combined モードの実装 diff に内部 self-review を通す(issue #171)

決定は [ADR 0011](../adr/0011-combined-impl-diff-self-review.md) に記録した。
選択肢 (a) を採る: **spec_worker に必須の内部 self-review を持たせる**。

## spec の深さ

normal。未決事項は「(a)/(b)/(c) のどれか」の 1 点で、それは ADR 0011 で決着済み。
影響範囲は combined モード(opt-in、既定は separate)の実装ターンのみ。永続状態・
スキーマ・公開契約に触れず、override 1 つでロールバック可能なので、migration/rollback
セクションは不要(veto 条件に当たらない)。

## 決定の要点

- `SpecWorkerFlavor::self_reviews()` を `true` にする。kind は既定の `Impl` のまま。
- self-review は default branch との差分を読む(`impl_reviewer.rs`、flavor の `verify_base`
  ではない)。spec は execute 段階で削除済みなので、レビュー対象は combined 差分
  = **ADR + 実装コード**になる。特別扱い不要。
- 根拠・却下した (b)/(c) は ADR 0011 を参照。

## 受け入れ基準

- combined モードの spec-worker ランが `validate` の後に `self-review` フェーズを通る
  (worker と同じ review→fix ループ、forge 呼び出しゼロ)。
- self-review が clean、または rounds cap 到達で PR が公開される(worker と同じ backstop)。
- 内部 self-review が無効(`review.enabled = false`)なら従来どおり素通りする。
- combined PR 本文の `<details>` に実装 self-review のラウンド要約が出る。
- separate / ready 経路の挙動は不変。

## 触るファイル

- `src/engine/spec_worker.rs` — `SpecWorkerFlavor` に `fn self_reviews(&self) -> bool { true }`
  を追加(worker/planner の override と対称)。ドキュメントコメントに combined の
  self-review 意図を一言。
- `docs/adr/0011-combined-impl-diff-self-review.md` — 決定の記録(作成済み)。
- `docs/adr/0008-symmetric-plan-impl-review-loop.md` の Consequences は改訂しない
  (ADR は積む方針)。0011 が 0008 の隙間を埋める旨は 0011 側に書いた。

## テスト方針

- 単体(`src/engine/spec_worker.rs` の `#[cfg(test)]`): `SpecWorkerFlavor.self_reviews()`
  が `true` を返すことを確認する小さな回帰テスト。
- 統合: 既存の combined 経路テスト(spec-ready PR takeover)が self-review フェーズを
  挟んでも緑のままであることを確認する。疑似エージェント TUI が review→fix を
  素通り(clean 即返し)できれば追加コストは小さい。既存の worker self-review テストの
  枠組みを流用する。
- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo nextest run`
  / `cargo test --doc` を通す。
