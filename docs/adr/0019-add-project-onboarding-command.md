# ADR 0019: プロジェクト追加は 1 コマンドの host 操作にする(config 追記 + 任意の repo 新規作成)

- Status: proposed
- Date: 2026-07-15
- Issue: #196

## コンテキスト

managed clone(ADR 0018)で「`repo_slug` を宣言すれば clone は meguri が実体化する」が
成立した。その結果、プロジェクト追加に人間が手でやることは「`config.toml` に `[[projects]]`
を書き足す」だけになった。だが `meguri init` はいまも stub を書いて手編集を促すだけで、
`id` / `repo_slug` の記入・重複チェック・追記後の環境検査は人間の目視に委ねられている
(`src/main.rs` `cmd_init`、`skills/meguri/references/setup.md`)。

投入のハードルを下げるという点で、これは #120(`meguri add` の capture-first)と同じ問題である。
「宣言を書き足すだけ」なら、それを 1 コマンドに畳める。

## 決定

**プロジェクト追加を `meguri add-project <owner/repo>` という host 側の 1 コマンドにする。**

1. **config は追記のみ。** 既存の `config.toml` の末尾に `[[projects]]` ブロックを
   **テキストとして追記**する。TOML の array-of-tables はファイル末尾に足せる仕様なので、
   既存のコメント・手編集・キー順は 1 バイトも触らない。書き込みは temp + rename で原子的に行う。
   追記前に既存 config を読み、`id`(と slug)の衝突を弾く。追記後に `Config::load` で
   再パースし、壊れていないことを確かめる。

2. **clone は追記しonly。実体化は既存の reconcile に委ねる。** ADR 0018 が clone を
   level-triggered な reconcile ステップ(`ensure_project_clone`)にしたので、
   add-project は宣言を書くだけでよい。次の watch tick が冪等に clone する。
   コマンド完了時点で doctor が緑になるよう、**その場で `ensure_bare_clone` を best-effort で
   一度呼ぶ**が、失敗してもコマンドは失敗にしない(自己修復するため。ADR 0018 の思想と一致)。

3. **`--create` は meguri が行う唯一の不可逆 forge 操作。** `gh repo create` で repo を
   新規作成する。**初期コミットまで含める**(`--add-readme` 相当):コミット 0 の repo は
   default branch が無く、`worktree add` の起点(`src/gitops.rs`)も PR の base
   (`src/engine/flow.rs`)も崩れるため、default branch の存在保証は onboarding の責務になる。

## なぜ `--create` を meguri にやらせてよいか(不可逆操作の境界)

meguri は「run が不可逆操作(repo 作成・可視性変更・履歴改変)を自律実行しない」という
線を引いている(decompose の `human` child がこの類を人間へ回すのはそのため)。
`--create` はこの線を破らない。**引き金を引くのは自律ループではなく、`meguri add-project
--create` を打った人間**だからだ。CLI で明示的に要求された不可逆操作を host コマンドが
その場で実行するのは、doctor が `gh` を直接叩くのと同じ「人間主導の host 操作」であって、
座標モデル上の run ではない。

その代わり、**meguri は作った repo を自動で消さない**。`--create` 後に clone や config 追記が
こけても、既にできた repo は残す(repo の削除こそ破壊的操作)。部分的に失敗したら、
「何ができていて、何を人間が手で片付ける必要があるか」を正直に表示して止まる。
`--create` は自動ロールバック不能である、と受け入れる。

## スコープに載せないもの(意図的)

- **auto-merge の設定**: 新規 repo に branch protection は無く、preflight で起動拒否になる
  (`src/app.rs`)。非 admin トークンは 403 になる。従来どおり `docs/ops/github-settings.md` の
  手順で後から opt-in する(ADR 0003 系の判断は動かさない)。
- **フェーズラベル付与**: `meguri:ready` / `meguri:plan` は human のゲートのまま(ADR 0005)。
  add-project は「向けるだけでは動かない」を変えない。
- **check_command の自動推測**: 未設定でも git 条件だけで動く。人間(または agent)が決める。

## 帰結

- 最小の onboarding が `meguri add-project owner/repo` の 1 行になる。手動 clone も手編集も消える。
- config は「追記のみ・コメント保持」を守るため `toml_edit` 等の再シリアライズは使わず、
  末尾テキスト追記で済ませる(新規依存を増やさない)。
- 追記が唯一の書き込み経路になるので、**`meguri init` はもう live な `[[projects]]` stub を
  書かない**。テンプレートの stub はコメント化し、init 直後の config は有効プロジェクト 0 件に
  する。さもないと `init` → `add-project` でダミーの `owner/repo` が残り、doctor/watch が実
  project 扱いして赤くする。追記される最初の実プロジェクトが唯一の live entry になる。
- 本コマンドは**永続状態(`config.toml`)+ 公開契約(新 CLI)**に触れ、`--create` は**不可逆**なので、
  紐づく spec 側で migration / rollback を必須とする。
