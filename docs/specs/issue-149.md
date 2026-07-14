# issue-149 spec — `[projects.prompts]`: ロール別 preamble を turn プロンプトに注入する

turn プロンプトには、issue ごとではなくプロジェクト全体に常時かかる恒常規律
(ガードレール必読・編集ペルソナ遵守・品質基準)を差し込む場所がない。`CLAUDE.md` は
claude 以外のプロファイルに届かずロール別にも出し分けられない。issue 本文への転記は
起票者全員に強制されて漏れる。config にロール → preamble(repo 相対パス)のマップを持ち、
turn プロンプト生成時に埋め込む。worker には品質基準を、planner には企画ガイドラインを、
reviewer には監査観点を、とロール別に渡せるのが要点。

設計判断の durable な部分(埋め込み vs 参照・挿入位置・ロール語彙・合成順・欠落方針)は
**ADR 0012**(本 PR 同梱)に置いた。ここは実装を収束させるための足場である。

## スペックの深さ — なぜ design 寄りか

未決の設計判断が複数あり(注入方式・ロール語彙の齟齬・top/project 合成の粒度・挿入位置・
配線)、かつ **config スキーマという公開契約**に触れる(veto ルール: migration/rollback 必須)。
ただし追加はすべて任意・後方互換で、永続状態・DB スキーマには一切触れないため、実装効果
自体は局所的。よって normal spec に「代替案 / migration・rollback / test 戦略」を足した
design 寄りの薄い spec とする。判断の本体は ADR 0012 へ、ここは受け入れ基準と変更箇所に絞る。

## 決定(要点、詳細は ADR 0012)

- **注入方式 = 埋め込み**。ロールに該当する preamble ファイルを worktree から読み、プロンプト
  本文の冒頭に現物として入れる。参照行にはしない(agent がファイルを読むことに依存させない)。
- **挿入位置 = 本文冒頭**。完了契約は末尾のままで最終権威を保つ。preamble は「プロジェクトの
  恒常規律」とラベル付けし、契約に劣後する前文として枠付けする。
- **ロール語彙 = routing の6ロール + `all`**。`src/routing.rs` の `KNOWN_ROLES` と
  `DEPRECATED_ROLE_ALIASES` を再利用する。旧名は**検証でも解決でも**正規化して扱い、未知キーは
  config 検証で弾く。issue 記述の `spec-reviewer` / `impl-reviewer` は旧名 — 現行では
  `pr-reviewer` / `self-reviewer` に正規化され、その turn で実際に注入される(validate は
  通るのに注入されない、という齟齬を作らない)。`routing::canonical_role` は現状 private
  なので `pub` に上げて config 側から使う。
- **合成 = `all` → ロール別を連結、上書きはキー単位**。per-project の同キーが top-level を
  上書きし、無ければ top-level に落ちる(`language_for` の流儀を map に広げた形)。
- **欠落 = warn + イベント発火で続行**。turn は落とさない。`doctor` は別途 strict に検出。
- **パスは repo 相対に限定**。絶対パス・`..` を config 検証で弾く。中身は agent プロンプトに
  埋め込まれるため、worktree 外の秘密ファイルを読ませない防御線。

## 変更箇所

### 1. config: 2つの任意セクション追加 — `src/config.rs`

- `Config` に `#[serde(default)] pub prompts: HashMap<String, String>`(top-level `[prompts]`、
  ロール名/`all` → repo 相対パス)。
- `ProjectConfig` に同型の `#[serde(default)] pub prompts: HashMap<String, String>`
  (`[projects.prompts]`)。
- 解決ヘルパ `preambles_for(&self, project, role) -> Vec<(String /*key*/, String /*rel path*/)>`:
  呼び出し側は canonical なロール名(`routing_role_for_loop` の結果 / `"self-reviewer"`)を渡す。
  マップの各キーは `routing::canonical_role` を通してから照合する — top-level・per-project
  どちらのキーも canonical 化した上で、`all` とロール `role` それぞれを per-project → top-level
  のキー単位フォールバックで引き、`[all, role]` の順で存在するものだけ返す。これにより
  `spec-reviewer = "..."` と書かれた entry も `pr-reviewer` の turn で正しく注入され、
  旧名(top)と canonical 名(project)が同じロールを指す場合は per-project が勝つ。
- `Config::validate` にキー検証を追加: `prompts` / `[projects.prompts]` の各キーは
  `routing::canonical_role` を通して `KNOWN_ROLES` に含まれるか、`all` のどちらか。外れたら
  `bail!`(routing の未知ロール拒否と同じメッセージ調)。同一マップ内で alias と canonical が
  同じロールに畳まれて衝突する場合(例: `pr-reviewer` と `spec-reviewer` を併記)も、
  どちらが勝つか曖昧なので `bail!` で弾く。
- **パス値の安全検証(必須)**: 各値は「repo 相対パス」を契約とする。preamble の中身は
  agent プロンプトにそのまま埋め込まれるため、絶対パスや `..` を含む値を許すと worktree の
  外(例: `~/.ssh/id_rsa`)を読んで agent に漏らす入口になる。共通ヘルパ
  `validate_repo_relative(rel) -> Result<()>` を足し、絶対パス(`Path::is_absolute`)と
  親ディレクトリ参照(`Component::ParentDir` を含む)を `bail!` で拒否する。`Config::validate`
  で `prompts` / `[projects.prompts]` の全値に適用する。解決も doctor も、読み込む直前に
  同じヘルパを必ず通してから `join` する(検証を単一の関門に集約し、経路差で漏れないように
  する)。
- `INIT_TEMPLATE` にコメント例を追記(過剰採用を避ける一言 —「CLAUDE.md で足りるなら不要」)。

### 2. 配線: preamble の解決・読み込み・埋め込み — `src/engine/flow.rs` + `src/turn/`

- 解決と読み込みは flow 層に置く(`Deps` が config/project/store を持つため)。
  `run_turn`(author)は `routing::routing_role_for_loop(&run.loop_kind)`、`run_review_turn`
  は `"self-reviewer"` を routing ロールとして `run_turn_in` に渡す。
- `run_turn_in` で `preambles_for` → 各パスは `validate_repo_relative` を通してから
  `worktree.join(rel)` で読む(config validate を通った値でも、防御的に解決関門で再確認する)。読めたものを
  「## プロジェクトの恒常規律(以下に従うこと。ただし meguri の完了契約・検証ルールが優先)」
  のラベル付きブロックに `all` → ロール順で連結する。
- 位置の責務は `src/turn/prompts.rs` が持つ: `prepare_turn` / `write_prompt_file` に
  `preamble: &str` 引数を足し、`<!-- meguri prompt -->` ヘッダの直後・`{body}` の前に置く
  (完了契約は末尾のまま)。prompts.rs は Config に依存させず、組み立て済みテキストを受け取って
  **配置だけ**する(解決は flow、配置は turn の分担)。空文字なら現状と完全に同一の出力。

### 3. 欠落ポリシー: warn + イベント — `src/engine/flow.rs`

- パスが worktree に無い / 読めない場合、`deps.store.emit(Some(&run.id), "prompt.preamble_missing",
  json!({ "role": …, "key": …, "path": rel }))` + warn ログを出し、その preamble を飛ばして続行。
  `worktree_setup` の非 `required` 既定と同じ「死なない」方針。成功時は
  `prompt.preamble_injected`(roles/paths)を observability として発火してよい。

### 4. `meguri doctor`: primary clone 上の存在検証 — `src/main.rs`

- `doctor_schedules` の `body_file` 検査と同型の新セクション: 各 project の `prompts` /
  top-level `prompts` を列挙し、`validate_repo_relative` を通した上で
  `project.repo_path.join(rel)` の存在を確認。無ければ ❌ で問題として報告(実行時は warn 続行
  だが、doctor は設定ミス = typo を捕まえる役)。config validate が絶対パス/`..` を先に弾くので
  doctor がそれらを見ることは通常ないが、解決は同じ関門を共有する。

## 受け入れ基準

- [ ] `[prompts]` / `[projects.prompts]` が role→path マップとしてパースされる。
- [ ] 未知ロールキーは config 読み込みで拒否される。旧ロール名(`spec-reviewer` 等)と `all`
      は受理される。同一マップ内での alias + canonical 併記(`pr-reviewer` と `spec-reviewer`)は
      衝突として拒否される。
- [ ] `preambles_for` が per-project→top-level のキー単位上書きを行い、`all`→ロールの順で返す
      (role のみ / `all` のみ / 両方 / どちらも無し の各ケース)。
- [ ] 旧名キー(`spec-reviewer`)で設定した preamble が、canonical ロール(`pr-reviewer`)の
      turn で注入される。top を旧名・project を canonical 名にした場合は per-project が勝つ。
- [ ] turn プロンプトで、該当 preamble が本文冒頭・完了契約より前に埋め込まれる。設定が空の
      プロジェクトは現状と同一のプロンプトになる(後方互換)。
- [ ] preamble パスが欠落しても turn は続行し、`prompt.preamble_missing` イベントが出る。
- [ ] 絶対パス(例: `/etc/passwd`)や `..` を含む値(例: `../../secret`)は config 読み込みで
      拒否される。
- [ ] `meguri doctor` が、設定済みだが primary clone に無いパスを ❌ で報告する。
- [ ] README / config ドキュメントに新セクションと「CLAUDE.md との住み分け」を追記。
- [ ] `cargo fmt --check` / `clippy -D warnings` / `nextest run` / `cargo test --doc` が通る。

## test 戦略

- **config**(`src/config.rs` unit): パース、未知キー拒否、alias+canonical 衝突拒否、
  旧名/`all` 受理、`preambles_for` の合成4ケース、および旧名キーが canonical ロールで解決される
  こと・旧名(top)↔canonical(project)上書きの2ケース。加えて `validate_repo_relative` の
  単体テスト(絶対パス拒否・`..` 拒否・正常な相対パス受理)と、それらを含む config が
  load で弾かれること。
- **配置**(`src/turn/prompts.rs` unit): 非空 preamble が本文の前・契約の前に入ること、
  空 preamble で現行出力と一致すること(既存 `prompt_file_contains_contract_and_turn_id` を拡張)。
- **欠落**(flow 層 / `FakeStore` で): 欠落時に `prompt.preamble_missing` が記録され turn 続行。
- **doctor**(`src/main.rs`): 存在パス ✅ / 欠落パス ❌ を返す小テスト。
- 実 tmux/git を使う統合テスト(`tests/*.rs`)までは要らない — 埋め込みは純テキスト整形で、
  既存の疑似エージェント経路に新しい振る舞いを足さないため。

## migration / rollback(veto: config は公開契約)

- **追加的・後方互換**: 任意の2セクションが増えるだけ。未設定なら preamble は注入されず、
  プロンプトは現状と一字一句同じ。既存 config は無改変で読める。
- **永続状態なし**: DB スキーマ・sqlite・run 状態には触れない。マイグレーション対象データは無い。
- **rollback**: config からセクションを消す(即無効化)、またはコードを revert すれば
  プロンプトは現行形に戻る。段階的な巻き戻しは不要。

## 代替案(退けた理由は ADR 0012)

- **参照行方式**: agent の読み込みに依存し「必ず届く」要件を満たさない。→ 埋め込みを採用。
- **top/project の wholesale 上書き**(`pr`/`review`/`clean` 流儀): top-level `all` を置いた上で
  per-project でロールを足すたびに `all` を再宣言させられ、`all` 合成の意図と噛み合わない。
  → キー単位上書きを採用。
- **独自ロール語彙**: routing と別の語彙は二重管理と「静かなフォールバック」を招く。
  → routing の `KNOWN_ROLES` を正典として共有。
