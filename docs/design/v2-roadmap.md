# v2 ロードマップ — 増分の順序と参照する失敗カタログ

v2 はフルスクラッチの書き直しである(2026-08-04 開始)。目的は**理解の再取得**:
v1 の刈り込み(kernel-pruning-plan、〜PR #272)で概念は核まで絞れたが、コード
には 10 ループ時代の骨格が残り、所有者がコードベース全行を理解している状態には
戻れなかった。v2 は概念の核だけを、通読できるサイズを保ちながら積み直す。

## 規律

- **地図が常に正確であること。** 「現時点のアーキテクチャと機能」は
  `docs/architecture.md` が正であり、増分 PR は必ず同時にこれを更新する。
  所有者の理解はコード通読ではなくこの地図で維持する — v1 の理解負債の実体は
  「正確な地図がどこにもない」ことだった。
- **1 増分 = 1 PR = レビューできるサイズ。** レビューを通らない速度で書かない。
  これは v1 の失敗(dogfooding が検証ゲートを外し、無検証の表面積が増殖した)の
  直接の再発防止である。
- **増分は直列。** 一度に 1 機構。動かして価値を確認してから次へ。
- **失敗カタログ参照。** 各増分は、v1 で同じ問題を解いた ADR(`docs/adr/`)を
  設計前に読む。ADR の解をそのまま写す義務はないが、ADR が記録している
  **失敗そのもの**は再発させない。
- **依存最小。** 新しい crate は、それが解く問題が実際に現れたときに足す。

## 増分の並び(暫定 — 各増分の設計時に見直す)

| # | 増分 | 中身 | 参照する v1 の知識 |
|---|---|---|---|
| v0 | 単発 run(済) | worktree → pane → result.json → 検証 → ブランチ | 完了コントラクト、trust-but-verify、`.meguri/` exclude |
| v0.0.1 | herdr 対応(済) | mux trait 化 + herdr backend(spawn/send/alive)。オーナーの常用環境を優先して前倒し | v1 herdr.rs のコマンド面(tab create / pane run / send-text)、pane run = shell 内起動で画面が残る |
| v0.0.2 | preflight prime(済) | claude の folder-trust ダイアログで run が始まらない失敗が v2 で再観測(初回 run で実測)→ v1 の preflight を最小移植 | preflight prime(deny-all settings + strict-mcp-config、ADR 0027 D1、issue #235) |
| v0.1 | 検証フィードバック | 検証落ちを fix turn としてエージェントに差し戻す(上限付き) | 虚偽申告の訂正(validate_turns、ADR 0002 系) |
| v0.2 | ブロック検知と nudge | 沈黙の nudge(上限付き)、pane の Blocked 判定は「破綻」とだけ区別 | idle_grace / nudge_limit、「ブロック ≠ 失敗」 |
| v0.3 | resume | run の checkpoint 永続化(sqlite 導入)+ agent session id 保存 → クラッシュ後再開 | crash recovery、ADR 0029(会話可能な session だけ resume) |
| v1.0 | watch ループ | task キュー(sqlite が権威)+ 直列/並列ディスパッチ + 排他 lock | 権威反転、claim no-steal(ADR 0027) |
| v1.1 | GitHub 入出力 | intake(ready/hold ラベル、低頻度)+ 投影(working/needs-human)+ PR 作成 | 権威反転の read 予算(intake 2 req/周期)、ラベルは投影 |
| v1.2 | 運用面 | ps / logs / attach / stop / prune | 介入面 = 耐久シグナルだけで駆動 |
| — | escalation の infra 分類、session health、herdr socket(状態検出) | 必要になった時点で | ADR 0028 / 0029 |

v1 の未解決 issue のうち v2 設計に直接効くもの: #273(pane の識別を issue 番号に
依存させない — v2 は最初から run/task キーで設計する)、#274(投影の成否に権威の
判断を依存させない)、#276(claim 直後クラッシュの回収)、#277(forge read の
予算を破らない)。

## v1 資産の扱い

- `docs/adr/` は**失敗カタログ**としてそのまま残す(STATUS.md の kernel/dormant
  分類は v1 時点のスナップショット)。
- v1 実装は main と git 履歴(`pre-prune` タグ = 56k 行時点、main = 15k 行核)。
- v2 が main を置き換える時点で、README・ADR 台帳を v2 の現実に合わせて棚卸しする。
