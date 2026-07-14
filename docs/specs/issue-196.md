# issue-196 spec — `meguri add-project`: プロジェクト追加 onboarding コマンド

いまプロジェクト追加は「`config.toml` を手で開いて `[[projects]]` を書き、`meguri doctor` で
目視確認する」手作業だ(`src/main.rs` `cmd_init`)。managed clone(ADR 0018)で clone が
自動実体化するようになった以上、人間に残った仕事は「宣言を 1 ブロック書き足す」だけ。
この spec の決定は一行で書ける。**その追記を `meguri add-project <owner/repo>` という
host コマンドに畳む。** 設計判断そのものは ADR 0019(本 PR 同梱)に置いた。

## spec の深さ: design(veto 発動)

- **未決定**: config への機械追記の方式、clone を即時にやるか reconcile に委ねるか、
  `gh repo create` をどの層に置くか、`--local` 時の位置引数の扱い。
- **blast radius**: 永続状態(`~/.meguri/config.toml`)を書き換え、新しい CLId(公開契約)を足し、
  `--create` は **GitHub 上に実 repo を作る不可逆操作**。
- veto ルール(永続状態 + 公開契約 + 不可逆リスク)により **migration & rollback は必須**。
  よって normal ではなく design spec とする。

## コマンド形

```
meguri add-project <owner/repo> [--create] [--id <id>] [--local <path>] [--public]
```

- `<owner/repo>`: 既存 GitHub repo。`[[projects]]`(github mode)を config へ追記する。
- `--id <id>`: project id を明示(既定: `repo` 部分を `validate_project_id` 準拠に整形)。
- `--create`: `gh repo create` で新規作成 + 初期コミット(default branch を必ず作る)。
- `--public`: `--create` の可視性(既定 `--private`)。`--create` 無しでは無効フラグ。
- `--local <path>`: local mode で追加(`mode="local"`, `repo_path=<path>`, `repo_slug` 不要)。
  `--local` 時は位置引数を任意にし、id は `--id` か `<path>` の basename から採る。

排他: `--create` と `--local` は同時指定不可。`--local` に github 専用フラグ
(`--create`/`--public`)を付けたら拒否。これらは `check_add_flags`(`src/app.rs`)と同型の
純粋関数に切り出して config 無しで単体テストする。

## 動作フロー(github mode)

1. `Config::load` で既存 config を読む。`id`(と `repo_slug`)が既存プロジェクトと衝突したら
   その場で拒否(追記しない)。id は `validate_project_id` を通す。
2. `--create` の時: `gh repo create <slug> --private|--public --add-readme` を実行。
   `--add-readme` で初期コミット + default branch が生まれる。**この不可逆ステップを最初に、
   単独で実行し、結果を明示表示する**(以降がこけても repo は消さない)。
3. `[[projects]]` ブロックを **末尾テキスト追記**(temp + rename で原子的に)。TOML の
   array-of-tables は末尾に足せるため、既存のコメント・キー順・手編集は無変更。
4. 追記後に `Config::load` で再パースし、壊れていないことを確認(壊れていたら追記を巻き戻す)。
5. `gitops::ensure_bare_clone` を best-effort で 1 回呼び、clone をその場で実体化。失敗しても
   コマンドは成功扱い(次 tick の reconcile が自己修復。ADR 0018)。
6. doctor 相当の環境検査(git / gh / **gh の write 権限** / mux / agent CLI)を新プロジェクトに
   絞って流し、赤をその場で提示。最後に「`meguri watch` して issue に `meguri:ready` を付ければ
   走る」旨を案内する。

## 検討した代替案と決定

- **config 追記の方式**: (A) `toml_edit` で再シリアライズ / (B) 末尾テキスト追記。
  → **(B) を採る**。要件は「追記のみ・コメント保持」。array-of-tables は末尾追記で TOML 的に
  正しく、既存バイトを一切触らないので (B) が最も安全かつ新規依存ゼロ。(A) は全体を
  読み書きするためコメント整形が絡み、壊さない保証が (B) より弱い。
- **clone の実体化タイミング**: (A) reconcile に完全委任 / (B) 即時実行。
  → **両取り**。宣言追記が本体(ADR 0018 が clone を reconcile 化済み)なので追記だけで正しいが、
  コマンド完了時に doctor が緑になるよう即時 best-effort clone も行い、失敗は無視する。
- **`gh repo create` の置き場所**: (A) `Forge` trait に足す / (B) `src/forge/gh.rs` の自由関数。
  → **(B)**。`Forge`/`GhForge` は既存 slug 前提(全メソッドが既存 repo を操作)だが、repo 作成は
  repo が無い段階の操作で shape が違う。`gitops::ensure_bare_clone` が自由関数なのと同じく、
  `gh` へ shell out する唯一の作成点として自由関数 `gh::create_repo(slug, visibility)` を置く。
- **doctor 検査の再利用**: doctor の内部ヘルパ(`can_push` / gh write 検査 / mux / agent CLI)は
  `src/main.rs` に private。→ 検査ロジックを共有関数に切り出して add-project と doctor の
  両方から呼ぶ(重複実装を避ける)。純粋判定(`can_push` 等)は既に切り出し済み。

## オーケストレーションの seam(テスト容易性)

`cmd_add` が `add_core(&dyn Forge, …)` に副作用を注入して FakeForge で試験できるのに倣い、
`cmd_add_project` も orchestration core を切り出し、「repo 作成」「clone」「config 追記」
「環境検査」を注入可能な形にする。gh/network に触れる 2 点(作成・clone)を薄い seam の裏に置き、
純粋部分(ブロック整形・id 導出・衝突検知・フラグ整合)を単体テストで網羅する。

## migration & rollback(必須)

- **migration**: schema 変更なし・DB migration なし。追加される `[[projects]]` は既存 schema の
  通常エントリで、既存 config は不変。新 CLI は純加算。後方互換。
- **rollback(config 追記)**: 可逆。追記ブロックを消せば元通り。コマンド途中失敗時は原子的
  書き込みで元ファイルを守り、再パース不整合なら自分の追記を巻き戻す。
- **rollback(`--create`)**: **不可逆**。作成された GitHub repo は meguri が自動削除しない
  (repo 削除は破壊的)。部分失敗時は「repo は作成済み・config 追記/clone は未完」を明示し、
  人間が手で片付ける前提で止まる。`--create` は自動ロールバック不能である旨を README/help に明記。
- **機能そのものの rollback**: 追加された CLI・関数はコミット revert で残滓なく消える(既に
  書かれた config エントリは正規の `[[projects]]` として有効なまま)。

## observability

- store に `project.added`(と `--create` 時 `repo.created`)イベントを emit し、ADR 0018 の
  `repo.cloned` と揃える(監査用)。人間向けの一次面は従来どおり stdout と `meguri doctor`。

## テスト計画

- **単体**: (1) ブロック整形、(2) id 導出 + `validate_project_id` 拒否、(3) 既存 config との
  id/slug 衝突検知、(4) フラグ整合(`--create`×`--local` 排他、`--local` の github フラグ拒否)、
  (5) コメント付き config へ追記 → 再 `load` して既存プロジェクト + 新規が揃い、生バイトに
  既存コメントが残ることを確認(「壊さない」の実証)。
- **統合**: gh の repo 作成・clone は network/gh 依存で FakeForge の範囲外。orchestration core を
  fake seam で駆動して受け入れ基準を満たす。既存 `tests/*.rs` の実 git + local bare origin 流儀は
  clone 部分の確認に流用可能。

## 受け入れ基準

1. 既存 GitHub repo に `meguri add-project owner/repo` → `meguri watch` だけで、issue に
   `meguri:ready` を付ければ run が走る状態になる(config に github mode の `[[projects]]` が
   追記され、clone が実体化する)。
2. `--create` で作った直後の repo でも同様に走る(初期コミットにより default branch が必ず存在)。
3. 手編集・コメント入りの既存 config を壊さない(末尾追記のみ、コメント保持、原子的書き込み)。
4. `id`/`repo_slug` の衝突、`validate_project_id` 違反、フラグ排他違反はその場で拒否し、config を
   触らない。
5. 追記後に gh write 権限 / mux / agent CLI の赤をその場で提示する。
6. `--local <path>` で local mode プロジェクト(`repo_slug` なし・`repo_path` 手指定)が追記される。
7. `--create` の部分失敗時、作成済み repo を消さず、残作業を明示して止まる。
8. `meguri init` の案内が「stub を手編集」から `meguri add-project` 誘導へ更新される。

## 触るファイル

- `src/cli.rs` — `add-project` サブコマンド + フラグ定義
- `src/app.rs` — `cmd_add_project` + orchestration core + 純粋なフラグ/整形/衝突ヘルパ
- `src/config.rs` — config 末尾追記関数(原子的書き込み)、id 導出、`INIT_TEMPLATE` 誘導文言
- `src/forge/gh.rs` — 自由関数 `create_repo(slug, visibility)`(`gh repo create --add-readme`)
- `src/main.rs` — `Command::AddProject` の分岐、doctor 検査ヘルパの共有化、`cmd_init` の案内更新
- `skills/meguri/references/setup.md` — 手動 clone/手編集手順を add-project ベースへ更新
- `README.md` / `README.ja.md` — add-project の説明と `--create` 不可逆の明記
- `tests/add_project_test.rs` — 新規(orchestration core + 追記の非破壊性)
- `docs/adr/0019-add-project-onboarding-command.md` — 決定の記録(本 PR 同梱済み)

## スコープ外

- auto-merge の自動設定(branch protection 未設定の新規 repo では起動拒否。手順は
  `docs/ops/github-settings.md`)。
- フェーズラベルの自動付与(`meguri:ready`/`plan` は human のゲート。ADR 0005)。
- check_command の自動推測(未設定でも git 条件で動く)。
- agent-skills install(`--project`)や repo `meguri.toml` 雛形の同梱(必要なら別 issue)。
