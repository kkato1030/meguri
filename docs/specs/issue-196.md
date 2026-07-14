# issue-196 spec — `meguri add-project`: プロジェクト追加 onboarding コマンド

いまプロジェクト追加は「`config.toml` を手で開いて `[[projects]]` を書き、`meguri doctor` で
目視確認する」手作業だ(`src/main.rs` `cmd_init`)。managed clone(ADR 0018)で clone が
自動実体化するようになった以上、人間に残った仕事は「宣言を 1 ブロック書き足す」だけ。
この spec の決定は一行で書ける。**その追記を `meguri add-project <owner/repo>` という
host コマンドに畳む。** 設計判断そのものは ADR 0019(本 PR 同梱)に置いた。

## spec の深さ: design(veto 発動)

- **未決定**: config への機械追記の方式、clone を即時にやるか reconcile に委ねるか、
  `gh repo create` をどの層に置くか、`--local` 時の位置引数の扱い。
- **blast radius**: 永続状態(`~/.meguri/config.toml`)を書き換え、新しい CLI(公開契約)を足し、
  `--create` は **GitHub 上に実 repo を作る不可逆操作**。
- veto ルール(永続状態 + 公開契約 + 不可逆リスク)により **migration & rollback は必須**。
  よって normal ではなく design spec とする。

## コマンド形

公開契約として **github mode と local mode で形を分ける**。位置引数 `owner/repo` は
github mode 専用で、local mode では取らない(mode が使う入力の綴りが違うため)。

```
# github mode(既存/新規 GitHub repo)
meguri add-project <owner/repo> [--create] [--id <id>] [--public]

# local mode(GitHub を使わない手元プロジェクト)
meguri add-project --local <path> [--id <id>]
```

- `<owner/repo>`: 既存 GitHub repo。`[[projects]]`(github mode)を config へ追記する。
- `--id <id>`: project id を明示(既定: github は `repo` 部分、local は `<path>` の basename を
  `validate_project_id` 準拠に整形)。
- `--create`: `gh repo create` で新規作成 + 初期コミット(default branch を必ず作る)。
- `--public`: `--create` の可視性(既定 `--private`)。`--create` 無しでは無効フラグ。
- `--local <path>`: local mode で追加(`mode="local"`, `repo_path=<path>`, `repo_slug` 不要)。

### clap 上の表現

clap で契約をそのまま表す(手書き検証に頼らない):

- 位置引数 `slug: Option<String>` に `required_unless_present = "local"` +
  `conflicts_with = "local"`。→ github mode では必須、local mode では**受け付けない**
  (両方指定はパースエラー)。
- `--create` / `--public` に `conflicts_with = "local"`。→ `--local` に github 専用フラグを
  付けたらパースエラー。`--public` は `requires = "create"`(`--create` 無しでは無効)。

これで「github は位置引数必須・local は `--local` 必須で位置引数不可」「モード間フラグの排他」を
clap が保証し、位置引数任意という中間状態は契約に現れない。clap で表しきれない残り
(例: 既存プロジェクトとの衝突、slug の厳密検証)だけを `check_add_flags`(`src/app.rs`)と同型の
純粋関数に置き、config 無しで単体テストする。

## 動作フロー(github mode)

1. 入力を検証する(後述「入力検証と TOML 安全書き込み」)。`<owner/repo>` を GitHub slug として
   厳密に検証し、`id` は `validate_project_id`、`--local <path>` は絶対パスとして通す。どれか一つでも
   不正なら config を触らず拒否。
2. `Config::load` で既存 config を読む。`id`(と `repo_slug`)が既存プロジェクトと衝突したら
   その場で拒否(追記しない)。
3. `--create` の時: `gh repo create <slug> --private|--public --add-readme` を実行。
   `--add-readme` で初期コミット + default branch が生まれる。**この不可逆ステップを最初に、
   単独で実行し、結果を明示表示する**(以降がこけても repo は消さない)。
4. `[[projects]]` ブロックを **末尾テキスト追記**(temp + rename で原子的に)。ブロックの各
   string 値は **TOML serializer で生成**する(生 `format!` で `"..."` に差し込まない)。TOML の
   array-of-tables は末尾に足せるため、既存のコメント・キー順・手編集は無変更。
5. 追記後に `Config::load` で再パースし、壊れていないことを確認(壊れていたら追記を巻き戻す)。
6. `gitops::ensure_bare_clone` を best-effort で 1 回呼び、clone をその場で実体化。失敗しても
   コマンドは成功扱い(次 tick の reconcile が自己修復。ADR 0018)。
7. doctor 相当の環境検査(git / gh / **gh の write 権限** / mux / agent CLI)を新プロジェクトに
   絞って流し、赤をその場で提示。最後に「`meguri watch` して issue に `meguri:ready` を付ければ
   走る」旨を案内する。

### 入力検証と TOML 安全書き込み

CLI から来る文字列がそのまま TOML へ書かれるため、**config 注入(injection)を構造的に防ぐ**。

- **slug の厳密検証**: `<owner/repo>` は 2 段で受理する。まず `^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$`
  で「owner と repo がちょうど 1 つの `/` で区切られ、両者が非空、許容文字のみ(空白・改行・`\`・
  追加の `/` を含まない)」を保証する。**ただしこの regex 単体では owner/repo が `.` や `..` だけの
  場合も通る**ため、regex の後に各 component を個別チェックし、`.` と `..`(パストラバーサル)を
  明示的に拒否する。両方を満たした時だけ受理し、外れたら即拒否。GitHub の許容文字に寄せた
  保守的な集合。
- **id**: 既存の `validate_project_id`(単一の安全なパス構成要素。`/` `\` `.` `..` 空文字を拒否)を
  そのまま通す。
- **`--local <path>`**: 絶対パスを要求し、`repo_path` として書く。
- **値は必ず serialize する**: 検証を通っても、ブロック生成は生文字列補間ではなく TOML の
  シリアライズ経路(小さな struct を `toml::to_string`、または basic-string エスケープ)で行う。
  検証(不正入力を弾く)と serialize(通った入力も正しく escape する)の二段で、引用符・改行・
  バックスラッシュを含む入力でも `key = "..."` の外に出られないことを保証する。
- **1〜4 が失敗すれば config は不変**(検証は追記前、追記は原子的)。

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
  既存コメントが残ることを確認(「壊さない」の実証)、(6) **config 注入対策**: slug 検証が
  引用符・改行・バックスラッシュ・複数 `/`・空 owner/repo・空白、および owner/repo が `.`/`..`
  だけの component(パストラバーサル)を拒否すること、および
  `--local <path>` に `"`/改行/`\` を含むパスを渡しても追記後の再 `load` で `repo_path` が
  入力と完全一致し、TOML 構造(他キー・他テーブル)が壊れない/新キーが注入されないことを検証する。
- **統合**: gh の repo 作成・clone は network/gh 依存で FakeForge の範囲外。orchestration core を
  fake seam で駆動して受け入れ基準を満たす。既存 `tests/*.rs` の実 git + local bare origin 流儀は
  clone 部分の確認に流用可能。

## 受け入れ基準

1. 既存 GitHub repo に `meguri add-project owner/repo` → `meguri watch` だけで、issue に
   `meguri:ready` を付ければ run が走る状態になる(config に github mode の `[[projects]]` が
   追記され、clone が実体化する)。
2. `--create` で作った直後の repo でも同様に走る(初期コミットにより default branch が必ず存在)。
3. 手編集・コメント入りの既存 config を壊さない(末尾追記のみ、コメント保持、原子的書き込み)。
4. `id`/`repo_slug` の衝突、slug 形式違反、`validate_project_id` 違反、フラグ排他違反はその場で
   拒否し、config を触らない。引用符・改行・バックスラッシュを含む入力でも config 注入が起きない
   (値は検証を通り、かつ TOML serialize で escape される)。
5. 追記後に gh write 権限 / mux / agent CLI の赤をその場で提示する。
6. `--local <path>` で local mode プロジェクト(`repo_slug` なし・`repo_path` 手指定)が追記される。
7. `--create` の部分失敗時、作成済み repo を消さず、残作業を明示して止まる。
8. `meguri init` の案内が「stub を手編集」から `meguri add-project` 誘導へ更新される。

## 触るファイル

- `src/cli.rs` — `add-project` サブコマンド + フラグ定義(位置引数の `required_unless_present`/
  `conflicts_with`、`--public` の `requires` で github/local の形を clap 上で強制)
- `src/app.rs` — `cmd_add_project` + orchestration core + 純粋なフラグ/整形/衝突/slug 検証ヘルパ
- `src/config.rs` — config 末尾追記関数(原子的書き込み + 値の TOML serialize)、slug 検証、
  id 導出、`INIT_TEMPLATE` 誘導文言
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
