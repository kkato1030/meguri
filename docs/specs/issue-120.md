# issue-120 spec — `meguri add`: 雑メモ一言で即 issue を立て、AI が後追いで整形する(capture-first)

投入の摩擦をシステムのスループット上限にしないために、投入(capture)を LLM から切り離す。`meguri add <project> "<雑な一言>"` を足す: 即座に無ラベル issue を作成して番号 + URL を返し(capture — LLM を経由しない)、その後 headless の agent がリポジトリを読んでタイトル・本文を整形して書き戻す(refine — best-effort)。

この spec の設計判断(capture-first、原文 verbatim の主権、refine は寿命モデル外の one-shot)は spec より長生きするので **ADR 0006**(本 PR 同梱)に置いた。以下は実装を収束させるための足場。

## 決定(論点への回答)

issue の「論点(planning で詰める)」への答え:

1. **refine の実行形態 — headless 一発で確定。** AgentProfile に headless 呼び出しの型が無いので、**新フィールド `headless_args: Option<Vec<String>>` を `AgentProfile` に足す**(claude 系 builtin は `Some(["-p"])`)。`None`(headless 非対応)のプロファイルに当たったら refine をスキップして raw のまま、**一行警告**を出す(silent fallback にしない)。refine は agent の stdout を整形結果として受け取るだけで、agent 自身は forge も files も触らない。

2. **refiner の routing — role `refiner` を routing に乗せる。** `routing::KNOWN_ROLES` に `"refiner"` を追加し、`recommended_chain("refiner")` を安価側(`["claude-sonnet", DEFAULT_PROFILE]`)に倒す。これで `[routing.roles] refiner = "..."` が `validate` を通り、`routing::resolve(cfg, "refiner", detect)` で解決できる。`[routing]` 無しなら default、の既存規律に従う。KNOWN_ROLES はこれ以降「loop_kind + one-shot コマンドの役割」を含む(ADR 0006 の帰結。doc コメントを更新する)。

3. **project 引数の省略 — v1 で実装する(推奨どおり)。** `<project>` を省略でき、cwd が登録済み project の `repo_path` 配下なら推定する。曖昧(複数 project にマッチ、または cwd がどの repo_path 配下でもなく project が複数)なら明示エラー。

4. **refine の読み取り範囲 — `repo_path` を read-only。** worktree は作らない。cwd = `repo_path` で headless agent を起動し、書き込み・コミットはさせない(refine のプロンプトは「出力のみ・ファイルを書くな・コミットするな」)。yolo フラグ(`--dangerously-skip-permissions`)は headless 呼び出しには渡さない。

5. **update の競合 — 軽い guard を入れる。** refine の update 前に body を再取得し、**投入時の raw body のままのときだけ**上書きする(title も同様に raw title のときだけ)。数秒の窓に人間が編集していたらその編集を尊重して refine の書き戻しをスキップ(一行の note)。

6. **補助フラグ — v1 は `--raw` のみ + 決定由来の `--plan` / `--ready`。** `-m` / `--edit` 的なリッチ入力はやらない(`gh issue create` の領分)。

## capture-first のフロー(`cmd_add`)

`meguri add` は run/pane/store を作らない(ADR 0006: 寿命モデルの外)。必要なのは config と forge だけ — `build_deps` は使わず、`GhForge::new(&project.repo_slug)` を直に組む。

1. **project 解決** — 明示 `<project>` が最優先。無ければ cwd を各 `project.repo_path` と正規化して前方一致で推定。1 個に定まらなければ明示エラー。
2. **ラベル決定** — `--plan` → `[LABEL_PLAN]`、`--ready` → `[LABEL_READY]`、どちらも無し → `[]`(無ラベル = 未トリアージ)。`--plan` と `--ready` は排他(両方でエラー)。
3. **capture** — `create_issue(title, body, labels)`。初期 title = 原文(長ければ 1 行分に切り詰め)、初期 body = 原文メモ verbatim(refine が一度も走らなくても原文は body に在る = 受け入れ基準 3)。
4. **即時出力** — `issue #N created: <url>`。`create_issue` は番号のみ返す(確認済み)ので、**URL は `repo_slug` から合成**する(`https://github.com/{slug}/issues/{number}`)。trait シグネチャは変えない。
5. `--raw` または headless 非対応プロファイル → ここで終了(非対応時は一行 note)。
6. **refine(best-effort)** — `refining…` を出し、headless agent を起動(タイムアウトと Ctrl-C を tokio select で監視)。成功したら stdout をパースして整形 title/body を得る。**原文 verbatim フッタは meguri が付す**(モデル出力には含ませない)。論点5 の guard(raw のままか再取得確認)を通してから、`update_issue_title` + `update_issue_body`。`done` と整形結果の要約を出す。
7. **どんな失敗でも raw のまま capture 成功を報告**(exit 0)— agent CLI 不在・非ゼロ終了・パース失敗・タイムアウト・Ctrl-C いずれも一行警告のみ。

### refine の入出力

- **入力プロンプト骨組み**: 「一言メモから issue の title と body を整形せよ。body は内容種別に応じた骨組み(例: 症状 / 期待動作 / 関連しそうな箇所)。`repo_path` を読んで関連箇所を推測してよい。**勝手にスコープを広げるな。ラベル推定・優先度判定・重複検出はしない。ファイルを書くな・コミットするな。**原文メモは出力に含めるな(meguri が付す)。」+ 言語指定(`config.language_for(project)`、既存 `flow::language_instruction` を流用)。
- **出力形式**: 厳密な JSON `{"title": "...", "body": "..."}` を stdout に。パースできなければ refine 失敗として raw のまま(best-effort の一部)。
- **verbatim フッタ**(meguri が付与、モデルに委ねない):

  ```
  <refined body>

  ---
  ## 原文メモ
  <original text verbatim>
  ```

## 触るファイル

- `src/cli.rs` — `Add { project: Option<String>, text: String, plan: bool, ready: bool, raw: bool }` バリアント(`--plan`/`--ready`/`--raw`)。
- `src/main.rs` — ディスパッチ追加。
- `src/app.rs` — `cmd_add`。project 推定ヘルパ(cwd → repo_path)、capture→即時出力→(raw/非対応でなければ)refine→書き戻し。URL は repo_slug から合成。
- `src/forge/mod.rs` / `gh.rs` / `fake.rs` — **`update_issue_title(number, title)` を新設**(`Forge` trait に title 更新が無いことを確認済み。gh 実装は `gh issue edit <n> --title`)。`create_issue` / `update_issue_body` / `add_label` は既存で足りる。
- `src/config.rs` — `AgentProfile` に `headless_args: Option<Vec<String>>`(default `None`)。
- `src/routing.rs` — `KNOWN_ROLES` に `"refiner"`、`recommended_chain` に `refiner` の安価チェーン、builtin claude プロファイルに `headless_args = Some(["-p"])`。doc コメント更新(役割 = loop_kind + one-shot)。
- refiner のプロンプトと headless 起動(`src/app.rs` 内、または小さな `src/refine.rs`)。
- `README.md` / `README.ja.md` — 「投入口」の節を追加(現状「GitHub で issue を立ててラベルを貼る」しか無い)。`meguri add` の紹介と capture-first の要点、`--raw`/`--plan`/`--ready`、project 省略。
- `docs/adr/0006-capture-first-issue-intake.md` — 決定の記録(本 PR 同梱済み)。
- `tests/add_test.rs`(新規)— FakeForge ベース。

## 受け入れ基準

1. `meguri add myproj "雑な一言"` → LLM を待たずに issue 番号 + URL が表示され、GitHub に無ラベル issue が存在する。
2. refine 完了後、title が要約され body に構造が付き、**原文メモが verbatim で body 内に残っている**。
3. agent CLI 不在・refine 失敗・途中 Ctrl-C・headless 非対応プロファイルのいずれでも issue は raw のまま存在し、コマンドは capture 成功を報告する(silent に issue が消えない)。
4. `--plan` / `--ready` 付きなら対応ラベルが付き、watch が通常どおり拾う。`--plan` と `--ready` の同時指定はエラー。
5. 無ラベルで作った issue を watch が拾わないことは既存どおり(回帰確認)。
6. `--raw` は LLM を一切呼ばない。
7. `<project>` 省略時、cwd が単一 project の `repo_path` 配下なら推定し、曖昧なら明示エラー。
8. refine の update 前に人間が body を編集していたら、その編集を上書きしない(論点5 の guard)。
9. README(en/ja)に「投入口」の節があり `meguri add` と capture-first を説明している。

## テスト計画

`tests/add_test.rs` を新設し FakeForge に乗る。refine の headless 呼び出しは実 agent を起動できないので、**整形ステップを注入可能にする**(パース済み `{title, body}` を返すクロージャ/trait を `cmd_add` のコアに渡す形にリファクタし、テストは固定値を返す fake refiner を差し込む)。これで基準 2・3・6・8 を agent 抜きで検証できる。project 推定(基準 7)、ラベル(基準 4)、URL 合成、verbatim フッタは純関数として単体テストする。`update_issue_title` は fake に実装。既存の scheduler/planner/forge テストは非破壊。

## スコープ / 割り切り(v1)

- 単発 CLI コマンドのみ。watch ループへの組み込みはしない(常駐 triage は将来の別 issue)。
- refine は title と body の整形のみ。ラベル推定・優先度判定・重複検出はしない。
- 添付・テンプレート・対話ウィザードはやらない。
- refine の read-only 保証は「headless -p + yolo 無し + プロンプトで write/commit 禁止 + 出力のみ回収」で担保する。厳密なツール制限フラグの追加は実装時の裁量(read-only を破らなければよい)。
