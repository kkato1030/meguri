# issue-64 spec — routing (1/3): 役割ベースのエージェント振り分け

いまの meguri は、planner も worker も fixer も、全員が同じ CLI を同じ引数で起動する。`[agent]` はグローバルに 1 つ(`src/config.rs:130` の `AgentConfig`)で、5+1 個のループ全部が `spawn_agent_pane`(`src/engine/flow.rs:656`)で `deps.config.agent` を直読みしているからだ。spec の質が下流のターン数すべてを左右する planner と、狭いスコープの diff を直すだけの fixer が同じモデルである必然はない。役割ごとにプロファイルを振り分けられるようにする。

方針は issue の通り: **役割ベース**(難易度推定はしない)、**auto は検出でフィルタした推奨表**、**明示設定は常に auto に勝ち、失敗したら静かにフォールバックせず起動時エラー**。この 3 つは PR 後も残る決定なので ADR 0003 に切り出した。

## 設定モデル

```toml
# プロファイル: CLI ごとの起動方法の束。shape は現行 [agent] と同一
# (command / args / resume_args / herdr_agent_hint)。
[agents.profiles.claude-opus]
command = "claude"
args = ["--dangerously-skip-permissions", "--model", "opus"]
resume_args = ["--resume"]

[agents.profiles.codex]
command = "codex"
args = ["--yolo"]
resume_args = ["resume"]

[routing]
mode = "auto"        # auto | manual(既定 auto)

[routing.roles]      # キーは runs.loop_kind と同じ役割名。明示は常に auto に勝つ
reviewer = "codex"
# worker = "claude-sonnet"
```

- **役割名 = `loop_kind`**: `planner` / `reviewer` / `worker` / `spec-worker` / `fixer` / `conflict-resolver`。issue の表には 5 ループしか出てこないが、コードには conflict resolver(#35)が既にいる。小 diff の修正という点で fixer と同型なので、推奨表でも fixer と同じ扱いにする。
- **ビルトインプロファイル**: `claude-opus` / `claude-sonnet` / `codex` の 3 つは推奨表と一緒にバイナリに焼き込む。`[routing] mode = "auto"` と 1 行書くだけで動くようにするため。ユーザーが `[agents.profiles.<同名>]` を書けばそちらが勝つ。
- **`default` プロファイル**: 既存の `[agent]`(未定義ならその serde 既定)がそのまま `default` という名前のプロファイルになる。`[agents.profiles.default]` の定義は予約名として起動時エラー(`[agent]` を使え、と案内する)。

## 解決規則

役割 → プロファイル名の解決は 3 段:

1. **legacy**: `[routing]` セクションがない(serde 上 `Option` で `None`)→ **`[agents]` の有無によらず**全役割が `default`。検出も走らない。**現行動作と完全一致**がここで担保される。
   - **auto の発動条件は `[routing]` の明示に限定する**(`[agents.profiles]` の有無ではない)。`[agents.profiles.<name>]` だけを書いて `[routing]` を書かない config は legacy のままで、profiles は「`[routing]` から参照されるまで完全に不活性(inert)」— 定義しただけでは何も起きない。こうしないと、推奨チェーンに現れないカスタム名プロファイルを定義した瞬間に「セクションの存在だけがスイッチになって」全役割が推奨表 + 検出に切り替わり、ユーザーが書いた内容そのものは無効という、ADR 0003 の「書いたものと違うものが黙って走るのは動かないことより悪い」に反する状態になる。「`[routing] mode = "auto"` と 1 行書くだけで動く」という設計目標は `[routing]` ゲートのままで満たせる。
2. **明示**(`[routing.roles]` にある役割): そのプロファイルを必ず使う。プロファイル不在・`command` の検出失敗・未知の役割名は**起動時に明示エラー**(daemon の非対応プラットフォームと同じ `bail!` 流儀。静かなフォールバックはしない)。`mode = "manual"` は「roles に書いていない役割は `default`」の意味で、auto の推奨表を完全に切る。`[routing.roles]` に明示で `"default"` を書くこともでき(例: `worker = "default"`)、その役割は従来どおり `[agent]` で起動する — 明示なので**検出はかけない**(auto チェーン終端の `default` と同じく検出対象外)。「この役割だけ従来どおり」を manual/auto いずれでも 1 行で書ける。
3. **auto**(mode = auto で roles にない役割): 推奨チェーンを検出(`command --version`、doctor の `run_capture` と同じ)でフィルタし、最初に通ったものを使う。チェーンの終端は常に `default` で、`default` には検出をかけない(現行の「検出せず spawn する」挙動の温存)。

推奨表(2026-07 時点、`src/routing.rs` に `GENERATED_AT: &str = "2026-07-12"` と共に集約。鮮度チェックは routing 2/3):

| 役割 | チェーン |
|---|---|
| planner | claude-opus → default |
| reviewer | codex → claude-opus → default(書いたモデルと別ベンダーを優先) |
| worker / spec-worker | claude-sonnet → default |
| fixer / conflict-resolver | claude-sonnet → default |

## 解決のタイミング(issue からの意図的な逸脱が 1 点)

issue は「run 生成時に固定」と言うが、**run の最初のペイン spawn 時(= run 開始時)に解決して `runs.agent_profile` に永続化**する。理由:

- run 生成箇所は scheduler(`src/engine/scheduler.rs:101`)と `meguri run`(`src/app.rs:56`)の 2 つ、加えて migration 前に生成された既存 run(`agent_profile` NULL)がある。spawn 時の lazy 固定なら 1 箇所(`flow.rs`)で全経路を覆える。
- `create_run_for_loop` のシグネチャを変えずに済む。受け入れ条件「既存テスト無変更で通る」に効く(この関数はテストから 10 箇所以上直接呼ばれている)。
- 「一度固定したら途中で変えない」という issue の本旨は変わらない。2 回目以降の spawn と resume は保存済みの `agent_profile` を必ず使い、その名前のプロファイルが config から消えていたら**明示エラー**(勝手に default に落ちない)。

resume(`ensure_pane` → `spawn_agent_pane(.., Some(session_id))`)は固定済みプロファイルの `resume_args` を使う。`herdr_agent_hint` もプロファイル単位になる。

auto の検出はこの spawn 時 lazy 解決の中で走るので、**検出のタイミングは run ごと**になり、run が spawn された時点の `PATH` 差(CLI の後付けインストール等)で解決結果が変わりうる。ただし一度 `runs.agent_profile` に固定した後は再解決しない(resume も保存値を使う)ので、単一 run の中では常に不変で実害はない。

## 触るファイル

- `src/config.rs` — `AgentConfig` を `AgentProfile` に改名(`pub type AgentConfig = AgentProfile` は不要、使用箇所が少ないので追随)。`Config` に `agents: Option<AgentsConfig>`(`profiles: HashMap<String, AgentProfile>`)と `routing: Option<RoutingConfig>`(`mode`, `roles: HashMap<String, String>`)を追加。`[agent]` フィールドは現状のまま
- `src/routing.rs`(新規)— ビルトインプロファイル、推奨チェーン、`GENERATED_AT`。`resolve(cfg, role, detect) -> Result<String>`、`profile_by_name(cfg, name) -> Result<&AgentProfile>`、`validate(cfg, detect) -> Result<()>`(起動時検証)、本番検出器 `detect_command`(`--version`)。`detect` は `&dyn Fn(&str) -> bool` で注入可能にし、フォールバックチェーンをサブプロセスなしで単体テストできるようにする
- `src/engine/flow.rs` — `spawn_agent_pane` の `deps.config.agent` 直読みをやめ、`run.agent_profile`(NULL なら解決して永続化)から引いた `AgentProfile` を使う。`pane.spawned` イベントに `"profile"` を追加
- `src/store/migrations/0004_agent_profile.sql` — `ALTER TABLE runs ADD COLUMN agent_profile TEXT;`
- `src/store/runs.rs` — `RunRecord.agent_profile: Option<String>`、`update_run_agent_profile`。`RunRecord` は `Serialize` なので serve の API には自動で載る
- `src/app.rs` — `cmd_watch` / `cmd_run` の入口で `routing::validate` を呼ぶ(明示設定の起動時エラーはここ)。`cmd_ps` に PROFILE 列を追加
- `src/main.rs` — doctor: 定義済み全プロファイル(ビルトイン + ユーザー + default)の検出結果一覧と、最終的な役割→プロファイル解決表を表示
- `README.md` — Configuration セクションに `[agents.profiles]` / `[routing]` を追記
- `docs/adr/0003-role-based-agent-routing.md` — 決定の記録(本 PR に同梱)

## 受け入れ条件

1. `[agents]` / `[routing]` 未定義の既存 config で現行動作と完全一致。既存テストが無変更で通る
2. auto モード: codex 未検出の環境で reviewer が claude-opus に落ちる(注入検出器でのフォールバックチェーン単体テスト)
3. 明示指定(manual / 部分上書き): 指定プロファイル不在・検出失敗・未知の役割名で `meguri watch` / `run` が起動時に明示エラー
4. 6 ループがそれぞれ解決済みプロファイルでペインを起動する(FakeMux e2e: manual モード config で `spawned_commands()` が各ループのプロファイル command/args から始まることを検証。検出を伴わない manual を使い、テスト環境の CLI 有無に依存させない)
5. resume 時に固定済みプロファイルの `resume_args` が使われる(`tests/resume_test.rs` に追加)
6. `runs.agent_profile` が記録され、`meguri ps`(PROFILE 列)と serve(`RunRecord` の JSON)で確認できる
7. doctor がプロファイル一覧(検出結果つき)と役割解決結果を表示する

## テスト計画

- `src/config.rs`: 後方互換 parse(空 TOML → `agents`/`routing` が `None`)、profiles / routing の parse、`default` 予約名
- `src/routing.rs`: 検出器をクロージャで注入し、(a) legacy 経路、(b) 明示が auto に勝つ、(c) codex 不在 → claude-opus、claude も不在 → default、(d) 不在プロファイル / 未知役割の validate エラー、(e) **`[agents.profiles]` のみ定義で `[routing]` なし → legacy(全役割 default・検出なし。profiles が inert)**、(f) `[routing.roles]` に `"default"` を明示した役割は検出をかけず `[agent]` で解決
- `src/store`: migration 0004 の適用と `agent_profile` の roundtrip
- e2e(`tests/worker_test.rs` 等の既存パターン): manual モードで役割別プロファイルを与え、FakeMux の `spawned_commands()` を検証。resume 経路も同様

## スコープ外(routing 2/3, 3/3)

- 成果ベースの検査と推奨表の鮮度チェック(`GENERATED_AT` はその布石)
- エスカレーションと explore。`runs.agent_profile` の記録はこれらの土台
