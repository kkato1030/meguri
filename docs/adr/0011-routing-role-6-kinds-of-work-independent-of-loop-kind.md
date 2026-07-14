# ADR 0011: routing role は「仕事の種類」の6分類 — 内部 loop kind とは独立(ADR 0003 改訂)

- Status: accepted
- Date: 2026-07-14
- Issue: #167
- 関連: docs/adr/0003-role-based-agent-routing.md(本 ADR が改訂する)、
  docs/adr/0006-ai-implementation-review-is-an-internal-loop.md、
  docs/adr/0008-symmetric-plan-impl-review-loop.md

## 文脈

ADR 0003 は `[routing.roles]` のキーを `runs.loop_kind` とほぼ同じ粒度で設計した。これが3つの問題を生んでいた。

1. **設定値が何を指すか分かりにくい。** `spec-worker` というキー名は「spec を書く worker」と読める。実際は spec-ready な PR をトリガーに実装を進める worker で、spec を書くのは `planner` である。
2. **登録漏れがバグを生む。** `ci-fixer` は `KNOWN_ROLES` にも推奨チェーン(`recommended_chain`)にも登録されていなかった。結果、auto routing では兄弟の `fixer` / `conflict-resolver` が claude-sonnet に載るのに `ci-fixer` だけ黙って `default` に落ち、`[routing.roles] ci-fixer = "..."` と明示指定すると起動エラーになっていた。`cleaner` も同様に未登録だった。
3. **細粒度キーを持ちながら推奨は家族単位。** 既存の `recommended_chain` はすでに `worker | spec-worker`、`fixer | conflict-resolver`、`self-review | guard` を同一チェーンとして扱っていた。キーを細かく割っている割に、推奨の判断は結局「家族(仕事の種類)」でしか区別していなかった。

## 決定

**routing role を「ユーザーが答える問い = この種類の仕事をどのモデルにやらせるか」の6分類に粗くし、内部 loop kind から明確に分離する。**

| role | 問い | 対応する内部 loop / phase |
|---|---|---|
| `planner` | 計画・spec 作成 | `planner` |
| `worker` | 実装 | `worker`、`spec-worker` |
| `fixer` | PR を merge 可能に直す | `fixer`、`ci-fixer`、`conflict-resolver` |
| `self-reviewer` | PR 公開前の内部レビュー | self-review phase(worker/planner フロー内部) |
| `pr-reviewer` | 公開 PR 上の advisory レビュー(auto-merge gate) | guard loop |
| `cleaner` | 衛生巡回 | `cleaner` |

- **内部 loop kind(`runs.loop_kind`)は細粒度のまま維持する。** budget のカウントや `meguri stats routing` の観測性は loop 単位で意味があるため、内部表示・集計は変更しない。
- 新しい橋渡し関数 `routing_role_for_loop(loop_kind) -> role` を1枚追加した(pane lane 用の `role_for_loop` と同じパターン)。`resolve_run_profile`(`flow.rs`)はこの関数経由で role を求めてから `routing::resolve` を呼ぶ。`self-reviewer` は loop kind を持たない(内部 phase そのもの)ので、self-review turn の起動箇所(`impl_review_lane`)は role 名を直接指定する。
- `KNOWN_ROLES` を上記6つに縮小した。旧キーは `DEPRECATED_ROLE_ALIASES` で新 role に張り替えている: `reviewer` / `spec-reviewer` / `guard` → `pr-reviewer`、`impl-reviewer` / `self-review` → `self-reviewer`、`spec-worker` → `worker`、`conflict-resolver` / `ci-fixer` → `fixer`。
- `recommended_chain` を6 role に張り替えた。推奨内容自体は従来と同一(`planner` → opus 系、`worker` / `fixer` → sonnet 系、`self-reviewer` / `pr-reviewer` → cross-vendor、`cleaner` → default)だが、`ci-fixer` と `cleaner` は今回はじめて正しく家族の推奨チェーンに乗るようになった。
- 「`internal-reviewer` / `external-reviewer`」という対の命名案は検討したうえで棄却した。ADR 0006 が「外部レビュー = 人間または外部 bot」という語彙をすでに定義しており、これと衝突するためである。今回選んだ対称軸は「レビュー対象の所在」であり、`self` = PR が公開される前の自分の作業、`pr` = 公開された PR 上、を指す。

## 帰結

- `[routing.roles]` に書けるキーは6つの role のみになった。旧来の細粒度キーは alias として引き続き解決されるが、新規の設定はこの6分類で書く。
- `ci-fixer` / `cleaner` の登録漏れが解消され、auto routing で兄弟ループと同じ推奨チェーンに乗るようになった。明示指定(`[routing.roles] ci-fixer = "..."`)も起動エラーにならない。
- `meguri stats routing` や `meguri doctor` の loop 単位の表示・集計は変更していない。role は「どのモデルを使うか」を決めるための軸であり、観測性の軸(loop kind)とは独立に存在する。
- ループを新設するときは、まず `routing_role_for_loop` にその loop kind を6 role のどれかへ対応づけて追加する。新しい role が本当に必要になるのは「モデルの選び方そのものが異なる、既存6分類のどれにも属さない仕事」が現れたときに限る。
- 利用者は現時点で meguri 自身のみのため、rename に伴う移行コストは考慮していない(alias は将来の利用者保護のためのものであり、移行期間を設ける意図ではない)。
