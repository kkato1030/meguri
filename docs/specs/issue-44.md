# Spec: cleaner ループ v0 — リポジトリを定期巡回して乖離レポート issue を更新する(read-only detector) — issue #44

## ゴール

AI がコードを書き続ける環境では、リポジトリは放っておくと少しずつ、しかし確実に散らかっていく。雪かきのようなものだと考えればいい。誰かが定期的にやらなくてはならないし、やり方が派手である必要はどこにもない。

cleaner v0 は修正をしない。定期的にリポジトリを歩いて回り、目についた乖離(spec と実装のずれ、dead code の気配、規約からの逸脱、置き去りの TODO、残骸ブランチ、孤児ラベル)を **1 本のレポート issue** に書き留めて、静かに帰ってくる。それだけのループだ。書き込みはレポート issue の作成・更新のみ。それ以外のものには、指一本触れない。

人間がレポートを読み、本物だと思った項目を通常の issue に切って `meguri:plan` / `meguri:ready` を付ければ、あとは既存の planner / worker / fixer が引き受ける。誤検知なら config の無視リストに載せる。レポート issue 本体を人間が編集する必要はない(どうせ次の巡回で上書きされる)。

## キーデシジョン

### D1. 新 `Loop` 実装 `src/engine/cleaner.rs`(kind: `"cleaner"`)、reviewer 型の独自 drive

issue ラベル起点でも PR 起点でもないので `Flavor`/`run_flow` には乗せない。reviewer(#30)を雛形に、
`prepare-work → prepare-worktree → execute → settle` の 4 ステップをチェックポイント付きで進める。
`default_loops()`(`src/engine/mod.rs`)に登録する。

- **prepare-work**: レポート issue を forge で再確認(`meguri:hold` → Skip、マーカーが既に現 head を指す → Skip の benign race 処理)。reviewer と違い **`meguri:working` ラベルでの claim はしない** — 書き込み境界(ADR 0003)を守るためで、重複防止は DB の `(project_id, loop_kind, issue_number)` 一意制約と head マーカーで足りる。
- **prepare-worktree**: default branch head の read-only detached worktree。既存の `gitops::create_review_worktree` をそのまま使う(ディレクトリ名は `clean-<run_id>`)。
- **execute**: 巡回プロンプト 1 ターン + 検証失敗時の corrective turn 1 回(reviewer と同型)。
- **settle**: 機械検査を実行し、agent findings と合流させて本文をレンダリングし、レポート issue を上書き更新(なければ作成)。**成功時は worktree を自前で `gitops::remove_worktree` する**(D9)。

### D2. discovery は read-only。レポート issue が無い初回は `issue_number = 0` をターゲット

run の一意性はレポート issue 番号で既存の一意制約に乗せるが、初回はまだ issue が存在しない。
discovery で issue を作る案は不採用(discovery は他ループ同様 read-only に保つ)。初回だけ番号 `0` を
ターゲットにし、settle が issue を作成する。以後の discovery は実番号を返すので、一意制約は自然に効く。

discovery の判定(純関数に切り出して unit test する):

1. `list_issues_with_label(LABEL_CLEAN_REPORT)` でレポート issue を探す(複数あれば最小番号を採用)
2. `meguri:hold` が付いていれば対象外(人間の停止スイッチ)
3. 本文のマーカーを parse。current head は repo_path で `fetch origin <default>`(best-effort)+
   `rev-parse origin/<default>`(fallback: ローカル `<default>`)
4. 対象になる条件: 「issue またはマーカーが無い」or「`head != marker.head` **かつ** `now - marker.scanned >= interval`」。
   同一 head は間隔が空いても再走査しない(受け入れ条件)

### D3. head マーカー: `<!-- meguri:clean head=<sha> scanned=<unix-epoch> -->`

reviewer のレビューマーカーと同じ手法だが、「前回巡回からの経過時間」も forge 側に持たせる必要が
あるので `scanned`(unix epoch 秒)を足す。ローカル state には何も置かない(Authority: 何を走査済み
かは issue 本文が真実)。本文先頭行に埋め、人間向けには別途 `head` 短縮 sha と巡回日時を表示する。
時刻は `std::time::SystemTime` の epoch 秒(chrono 依存なし。表示用は既存の `store::now()`)。

### D4. Forge 拡張は 2 メソッドのみ: `create_issue` / `update_issue_body`

```rust
async fn create_issue(&self, title: &str, body: &str, labels: &[&str]) -> Result<i64>;
async fn update_issue_body(&self, number: i64, body: &str) -> Result<()>;
```

- `gh.rs`: `gh issue create --label ...` / `gh issue edit <n> --body-file -`。
  ラベル未定義だと `gh issue create --label` は失敗するので、`gh label create
  "meguri:clean-report" --force` を best-effort で先行実行する。
- `fake.rs`: in-memory 実装 + 作成/更新をそのまま `issues` に反映(テストは `labels_of` /
  `get_issue().body` で観察できる)。
- ラベル定数 `LABEL_CLEAN_REPORT: &str = "meguri:clean-report"` を `src/forge/mod.rs` に追加。

### D5. config: グローバル `[clean]` + per-project override(`pr` / `language` と同型)

```toml
[clean]
interval_hours = 24        # 巡回間隔(head が進んでいてもこれ未満なら走らない)
stale_branch_days = 30     # 最終コミットがこれより古いリモートブランチを stale とみなす
ignore = []                # 誤検知の無視パターン(部分文字列マッチ)
```

`CleanConfig`(serde デフォルト付き)+ `ProjectConfig.clean: Option<CleanConfig>` +
`Config::clean_for(&project)`。無視リストは本質的に project 固有なので override は最初から必要。

### D6. agent 成果物 `.meguri/clean-report.json` と検証、失敗は静かなスキップ

```json
{"findings": [
  {"category": "spec-drift" | "dead-code" | "convention" | "todo",
   "file": "src/x.rs", "line": 12,
   "note": "何がどうずれているか",
   "confidence": "high" | "medium" | "low"}
]}
```

(`line` は nullable。)検証は reviewer と同型: (1) worktree が clean かつ HEAD が claim した head の
まま、(2) JSON が parse できる。失敗したら corrective turn 1 回、それでもだめなら
`WorkerOutcome::Skipped` — **`meguri:needs-human` エスカレーションもコメントもしない**
(read-only で失うものが無い。受け入れ条件)。

ただし黙って run を捨てるだけだと、次の poll(既定 60 秒)で即座に再挑戦してしまい、恒常的に
失敗する agent が 1 分ごとにターンを焼き続ける。そこでスキップ時もマーカーの `scanned` だけを
現在時刻に更新する(head は前回値のまま — 「走査した」とは記録しない。issue が無ければ
「初期化中」本文 + `head=none` マーカーで作成する)。これで再挑戦は自然に interval 間隔に律速される。

### D7. 機械検査は settle 内の純 Rust(エージェント不使用、同一 run 内)

- **stale ブランチ**(`gitops` に helper 追加): fetch 後に
  `for-each-ref --format='%(refname:short) %(committerdate:unix)' refs/remotes/origin` を列挙し、
  default branch と open PR の head ブランチ(`list_open_prs` — 既存)を除外した上で、
  「`merge-base --is-ancestor` で default に merged」or「最終コミットが `stale_branch_days` 超」を
  報告する。
- **孤児 `meguri:working`**: `list_issues_with_label(LABEL_WORKING)` + `list_prs_with_label(LABEL_WORKING)`
  のうち、この host の store に active run が無いものを報告する(confidence: medium)。
  現行の list API は open のみを返すため「closed issue に残った working」は v0 では拾えない —
  既知の限界として本文フッターには書かず、コード内コメントに残す。マルチホスト運用では他 host の
  正当な claim を誤検知しうるが、report-only なので許容(無視リストで黙らせられる)。

### D8. 本文はスナップショットの完全上書き。無視リストはレンダリング時に適用

- 構成: マーカー行 → サマリー(head 短縮 sha、巡回日時、件数)→ agent findings のカテゴリ別
  セクション(`file:line` + note + confidence)→ 機械検査セクション(stale branches / orphan labels)
  → 人間向けフッター(採用は issue 化して `meguri:plan`/`meguri:ready`、誤検知は config の
  `clean.ignore` へ)。
- 無視リストは finding の `file` / `note` / ブランチ名 / `issue#` 表記への **単純部分文字列マッチ**で
  除外(v0 は glob も regex も持たない)。除外はレンダリング時なので、パターンを足せば次回巡回で
  レポートから消える(受け入れ条件)。
- findings が 0 件でも本文は更新する(マーカーを前進させないと同一 head を再走査してしまう)。
  前回あった項目は再検出されなければ消える — 履歴ではなく現在の乖離のスナップショット。

### D9. worktree は cleaner 自身が回収する(reaper に任せられない)

reaper は「issue が closed になったら回収」だが、レポート issue は原則閉じない。detached worktree は
branch も持たないため、放置すると巡回のたびにフルチェックアウトが 1 個ずつ積み上がる。settle の
最後(および Skipped 確定時)に `gitops::remove_worktree` を best-effort で実行する。interrupted は
従来どおりチェックポイントから resume するので触らない。

## 触るファイル

| ファイル | 変更 |
|---|---|
| `src/engine/cleaner.rs`(新規) | `CleanerLoop`: discovery 判定(純関数)、drive 4 ステップ、マーカー、プロンプト、成果物検証、機械検査、本文レンダリング |
| `src/engine/mod.rs` | `default_loops()` に `CleanerLoop` を登録 |
| `src/forge/mod.rs` | `LABEL_CLEAN_REPORT` 定数、`Forge::create_issue` / `update_issue_body` |
| `src/forge/gh.rs` | 上記 2 メソッドの gh CLI 実装(+ ラベル best-effort 作成) |
| `src/forge/fake.rs` | 上記 2 メソッドの in-memory 実装 |
| `src/config.rs` | `CleanConfig`、`ProjectConfig.clean`、`Config::clean_for` |
| `src/gitops.rs` | default branch head 取得 helper、リモートブランチ列挙(名前・committerdate・merged 判定)helper |
| `README.md` / `README.ja.md` | cleaner 節 + `[clean]` 既定値 |
| `tests/cleaner_test.rs`(新規) | 下記 e2e |
| `docs/adr/0003-cleaner-read-only-single-report-issue.md`(新規) | 書き込み境界とスナップショット方式の記録 |

## テスト

unit(`cleaner.rs` 内):

- マーカーの format / parse 往復(head + scanned、`head=none` 含む)
- discovery 判定の純関数: マーカー無し / 同一 head / head 前進 + interval 未満 / head 前進 + interval 経過 / hold
- 本文レンダリング: カテゴリ別整形、無視リストで agent finding・stale branch・orphan label が消える、0 件でもマーカーが載る
- config: `[clean]` デフォルト(24h / 30d / 空リスト)と per-project override(`config.rs`)

e2e(`tests/cleaner_test.rs`、reviewer_test と同じ FakeForge + FakeMux + ローカル origin 構成。
scripted agent が `.meguri/clean-report.json` を書く):

- 初回巡回 → レポート issue が `meguri:clean-report` 付きで作成され、findings とマーカーが本文に載る
- **書き込み境界**: run 完了後、origin の refs が不変(push なし)、`forge.prs()` 空(PR なし)、
  レポート issue 以外の issue の本文・ラベル・コメントが不変
- 同一 head の再 discovery → 空。head を進めて interval 内 → 空。interval 経過(古い `scanned` を
  seed)→ 対象になり本文が上書きされ、前回の項目が消えている
- レポート issue に `meguri:hold` → discovery が空
- agent が成果物を出さない(corrective turn 込みで失敗)→ run は `Skipped`、`meguri:needs-human`
  なし・コメントなし、マーカーの `scanned` のみ更新
- `clean.ignore` に載せたパターンの項目が本文から消える
- stale ブランチ(origin に古い日付のブランチを seed)と孤児 `meguri:working`(active run の無い
  working ラベル issue を seed)が機械検査セクションに載る
- settle 後に worktree ディレクトリが消えている

## 受け入れ条件(issue から)

- [ ] FakeForge + FakeMux での e2e テスト(巡回 → レポート issue が作成/更新される)
- [ ] 書き込みはレポート issue の作成・更新のみ(push / ブランチ作成 / 削除 / 他 issue・PR への操作を一切しない)
- [ ] 同一 head を再走査しない(head マーカー)。head が進んでいても設定間隔内なら走査しない
- [ ] レポート issue の `meguri:hold` でループが停止する
- [ ] エージェントが成果物を出せなかった場合は `meguri:needs-human` ではなく静かにスキップし、次回巡回に委ねる
- [ ] 無視リストに載せた項目がレポートから消える

## スコープ外(将来 issue — issue #44 のとおり)

- 検出項目の自動 issue 化・自動修正 PR(confidence 階層と冪等 PR 管理が必要になってから)
- fitness function 等による設計適合の決定的検査、乖離メトリクスの計測
- 検出器自体の鮮度検査(無視リスト肥大の検出などのメタ検査)
- closed issue に残った `meguri:working` の検出(list API の制約。forge に closed 込みの列挙を足すときに)
