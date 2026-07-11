# Spec: spec を使い捨てに徹して実装時に刈る + planner の永続価値振り分けを強化 (#48)

spec というものは、考えてみれば足場に似ている。建物が建ってしまえば、足場は静かに撤去される。誰もそれを惜しまない。ところがいまの meguri では、足場が建物と一緒にデフォルトブランチへ merge され、`docs/specs/issue-1.md, issue-2.md, …` と永久に残り続ける。誰も読み返さないスナップショットの墓場だ。この spec は、その寿命と保存形式のねじれを解消するための計画を述べる。

方針はシンプルに二本立てになる。**spec は実装時に spec-worker が刈る**。そして刈っただけでは行き場のない知識が消えてしまうから、**planner が永続価値のあるものを ADR / ドメイン文書へ振り分けるよう指示を強化する**。この二つはセットであり、どちらか片方だけでは意味をなさない。

なお、この決定そのものの経緯と理由は [ADR-0001](../adr/0001-specs-are-disposable-scaffolding.md) に記録した。spec が刈られる世界で長生きすべき判断は、まさにそこに置かれるべきだからだ。

## 受け入れ条件

1. planner の `execute_prompt` が、spec は「実装時に削除され merge には残らない使い捨ての足場」だと明記し、永続価値のあるもの（設計判断 → `docs/adr/NNNN-<slug>.md` の次の空き番号 / ビジネスロジック・ドメイン規則 → リポジトリ既存の永続ドメイン文書）を spec ではなくそちらへ書くよう指示している。
2. spec-worker の `execute_prompt` が、実装完了後に `docs/specs/issue-<N>.md` を削除してその削除を commit するよう指示している。
3. spec-worker の `verify_work` が反転している: spec ファイルが**まだ存在したら** `Err`（削除を促す corrective ターンへ）、不在なら `Ok(())`。planner の「存在必須」と対称になる。
4. 両モジュールの doc コメントが新しい寿命モデル（spec は transient、spec-worker が刈る）を語っている。
5. `cargo test` が全部通る（下記のテスト更新込み）。
6. `README.md` / `README.ja.md` の spec 先行フロー説明に「spec は実装時に刈られ、永続価値は ADR へ」の一文が入っている。

## 触るファイル

- `src/engine/planner.rs` — 変更A: `execute_prompt`（現 78–85 行あたりの spec / ADR 指示）を強化。モジュール doc コメント追記。単体テスト（`prompt_demands_spec_not_implementation` 付近）に「使い捨て・ADR 振り分け」文言のアサートを追加。
- `src/engine/spec_worker.rs` — 変更B: `execute_prompt` に削除指示を追加。`verify_work` を「spec 残存なら Err」に反転。モジュール doc コメント追記。単体テスト追加（プロンプトに削除指示 / `verify_work` の反転挙動）。
- `tests/spec_worker_test.rs` — テスト更新（下記）。
- `README.md` / `README.ja.md` — フロー説明への軽い追記。
- `docs/adr/0001-specs-are-disposable-scaffolding.md` — 本 issue の決定の永続記録（この PR で新規追加。奇しくも新方式の最初の実例になる）。

## 主要な決定

- **`issue-<N>` という命名は変えない。** spec のパスは issue 番号だけから再構成できる content-addressable なキーであり、planner（書く: `planner.rs` の `spec_rel_path`）・spec-worker（読む: `spec_worker.rs` の `execute_prompt`）・検証の3箇所が依存している。名前は役割に対して正しい。
- **永続知識の器は既に ADR が担う。** planner のプロンプトは既に ADR に軽く触れているが、「spec が消える」前提を明示して振り分けを義務に格上げする。ドメイン文書の新設は「その issue がそうした規則を導入する場合に限る」と絞る。
- **Checkpoint への spec キャッシュは不要。** `execute_prompt` は 1 run につき最初の execute ターンで 1 回だけ呼ばれ（`flow.rs:557`）、以降の corrective / validation-fix ターンは spec を埋め込まない汎用プロンプト（`flow.rs:627`, `flow.rs:695`）を使う。しかも meguri はライブな対話セッションであり、エージェントはターン1で受け取った spec を保持し続ける。削除後に文脈を失う問題は起きない。
- **spec 不在への graceful degrade（`spec_worker.rs` の `unwrap_or_else`）は維持する。** その場合 `verify_work` は最初から `Ok` になるだけで、辻褄は合っている。
- **merge 済み PR の diff から spec が消えるのは意図通り。** planner が add し spec-worker が delete するので相殺される。spec はレビュー段階で役目を終えている。レビューは削除前に走るため（reviewer は spec-worker より前段）、レビューには影響しない。

## テスト計画

- `src/engine/spec_worker.rs` 単体:
  - プロンプトに spec 削除指示が含まれること。
  - `verify_work` が spec 残存を弾き（`Err` に spec パスが含まれる）、不在を許すこと。
- `src/engine/planner.rs` 単体: プロンプトが「spec は使い捨て」「永続価値は ADR / ドメイン文書へ」を含むこと。
- `tests/spec_worker_test.rs`:
  - `commit_implementation` を spec 削除込みに変更（`git add -A` で削除をステージし、spec が既に無くても冪等に通るようにする）。
  - `spec_worker_happy_path_...`: check_command を `test -f cache.txt && test ! -f docs/specs/issue-5.md` に変更。tree 検証（現 `spec_still_there`）を「FETCH_HEAD に spec が**無い**こと」へ反転。commit 数のアサートは spec commit + 実装 commit で `2` のまま。
  - `spec_worker_validation_failure_feeds_back_then_passes`: ターン1が execute の `verify_work` を通過できるよう、ターン1でも spec を削除して commit する（cache.txt はまだ作らないので validate は従来どおり失敗し、fix ターンで cache.txt が作られる）。
- `tests/reviewer_test.rs` の spec 存在アサート（276 行付近）は spec-worker より**前**の段階を検証しているので変更不要。

## スコープ外

- `issue-<N>` の命名変更・`docs/specs/` のディレクトリ構造化（名前はキーとして維持）。
- 既に merge 済みの過去 spec の遡及削除。
- reviewer / fixer / worker の各ループへの変更。
