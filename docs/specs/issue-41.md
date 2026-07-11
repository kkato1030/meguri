# issue-41 spec — auto-merge (1/3): `meguri:automerge` オプトインで GitHub ネイティブ auto-merge を arm する

いまの meguri はマージを完全に人間に任せている。forge にマージ系 API は一行もない。この spec はそこに「自動マージ」を持ち込むが、**meguri 自身は決してマージしない**。条件の揃った PR に GitHub ネイティブの auto-merge を arm する(`gh pr merge --auto`)だけで、「マージして安全か」の最終判断は GitHub(branch protection + required checks)に委ねる。meguri は CI 結果や approval を自前で再判定しない。この権威の分離は spec より長生きするべき決定なので ADR 0003 に切り出した(looper の ADR-0005 に準拠)。

## 全体像

新しいループは作らない。arm は agent turn を要さない軽い API 呼び出しなので、run レコードも pane も要らない。reaper と同じく **watch のポーリングに相乗りする sweep**(`src/engine/scheduler.rs:62` の `reaper::sweep` の隣)として実装する。

```
watch poll → auto_merger::sweep(deps)
  ├─ config: 実効 [pr.auto_merge].enabled でなければ即 return
  ├─ discovery: list_open_prs() → meguri/ ブランチ + オプトイン で絞る
  ├─ 各候補: arm 条件を全部チェック
  ├─ draft なら mark_pr_ready
  ├─ enable_auto_merge(pr, strategy, head_sha)   ← --match-head-commit で head 固定
  └─ arm マーカーコメントを PR に投稿(次回 sweep の冪等キー)
```

## 1. config — `src/config.rs`

既存の `PrConfig` にネストする。プロジェクト上書きは既存の `pr_for()`(project の `pr` セクションがあれば**丸ごと**勝つ)の意味論をそのまま使う — `[projects.pr.auto_merge]` を書いたプロジェクトは `draft` も含めて自分の `[projects.pr]` の値(未記載はデフォルト)になる。セクション単位の上書きは既存挙動で、キー単位マージの仕組みをここで発明しない。

```toml
[pr.auto_merge]
enabled = false                  # マスタースイッチ
strategy = "squash"              # squash | merge | rebase(リポジトリで不許可なら fallback せず拒否)
require_branch_protection = true # required checks 付き protection がなければ arm しない
opt_in = "label"                 # label | all(all は全 meguri PR が対象)
```

```rust
pub struct AutoMergeConfig {
    pub enabled: bool,                      // default false
    pub strategy: MergeStrategy,            // default Squash
    pub require_branch_protection: bool,    // default true
    pub opt_in: AutoMergeOptIn,             // default Label
}
```

`MergeStrategy { Squash, Merge, Rebase }` は forge の語彙なので `src/forge/mod.rs` に置き、config は serde(lowercase)でそれを直接デシリアライズする。不正な文字列は config ロード時にエラー(fail-fast の第一段)。

## 2. Forge 拡張 — `src/forge/mod.rs` / `gh.rs` / `fake.rs`

### 語彙

- ラベル定数 `LABEL_AUTOMERGE = "meguri:automerge"`
- `PullRequest` に `is_draft: bool` を追加(`gh` の JSON フィールド `isDraft`。`pr_from_json` と FakeForge の `RecordedPr` に追随)
- リポジトリのマージ設定のスナップショット:

```rust
pub struct MergePolicy {
    pub auto_merge_allowed: bool,               // repo の "Allow auto-merge"
    pub allowed_strategies: Vec<MergeStrategy>, // allow_squash_merge / allow_merge_commit / allow_rebase_merge
    pub protected_with_required_checks: bool,   // base に required checks 付き protection があるか
}
```

### trait メソッド(3 つ)

```rust
/// GitHub ネイティブ auto-merge を arm する。head_sha で固定
/// (`--match-head-commit`)。既に arm 済みなら成功扱い(冪等)。
async fn enable_auto_merge(&self, pr: i64, strategy: MergeStrategy, head_sha: &str) -> Result<()>;
/// draft PR を ready 化する(`gh pr ready`)。
async fn mark_pr_ready(&self, pr: i64) -> Result<()>;
/// base ブランチに対するリポジトリのマージ設定を読む。
async fn merge_policy(&self, base_branch: &str) -> Result<MergePolicy>;
```

### GhForge 実装

- `enable_auto_merge`: `gh pr merge <n> --repo <slug> --auto --<strategy> --match-head-commit <head_sha>`。「already enabled」系のエラーは成功に読み替える(冪等性の受け入れ条件)。head がズレて失敗した場合はエラーのまま返す — sweep 側が warn して次のポーリングで新 head を再判定する
- `mark_pr_ready`: `gh pr ready <n> --repo <slug>`
- `merge_policy`: `gh api repos/{slug}` から `allow_auto_merge` / `allow_squash_merge` / `allow_merge_commit` / `allow_rebase_merge`、`gh api repos/{slug}/branches/{base}/protection/required_status_checks` の成否(404 = required checks なし)で `protected_with_required_checks` を判定。**classic branch protection のみ対応**。rulesets 運用のリポジトリでは検出できないので、その場合の逃げ道が `require_branch_protection = false`(README に明記)

### FakeForge 実装

- `armed: Mutex<HashMap<i64, (MergeStrategy, String)>>`(PR → strategy + head_sha)。再 arm は上書きで成功
- `mark_pr_ready` は `RecordedPr.draft = false` に落とす
- `policy: Mutex<MergePolicy>` + セッター(`set_merge_policy`)。デフォルトは「全部許可 + protection あり」でテストが素直に書けるようにする

## 3. arm 判定と sweep — `src/engine/auto_merger.rs`(新規)

`reaper::sweep` と同型の `pub async fn sweep(deps: &Deps) -> Result<()>` と、テストしやすい純関数群に分ける。

### マーカー(冪等性と人間の上書き尊重を 1 つの仕組みで)

reviewer の head-sha マーカー(`src/engine/reviewer.rs:42`)と同じ流儀:

```rust
pub fn armed_marker(head_sha: &str) -> String {
    format!("<!-- meguri:automerge armed head={head_sha} -->")
}
pub fn head_already_armed(comments: &[String], head_sha: &str) -> bool;
```

**現在の head に対するマーカーがあれば無条件でスキップ。** これだけで二つが同時に成立する:

- 冪等性: 同一 head を二度 arm しない(auto-merge の現在状態を問い合わせる必要すらない)
- 人間の上書き尊重: マーカーがあるのに auto-merge が無効 = 人間が PR 上で解除した → その head では再 arm しない。push で新しい head が来たらマーカーが古くなり、条件を再判定する

順序は **ready 化 → arm → マーカー投稿**。arm に失敗したらマーカーは残らないので次の sweep が再試行する。arm 成功後マーカー投稿だけ失敗した場合も、次の sweep の再 arm が冪等(成功扱い)なので収束する。

### 候補の絞り込みと arm 条件(安い順にチェック)

`list_open_prs()` の各 PR について:

1. `pr.head_branch` が `meguri/` で始まる(meguri の PR しか触らない — fixer/conflict-resolver と同じ)
2. PR ラベルに `meguri:hold` / `meguri:needs-human` / `meguri:working` / `meguri:spec-reviewing` / `meguri:spec-ready` の**いずれも付いていない**(spec フェーズ中は絶対に arm しない。spec-worker は実装完了時に spec-ready を外す — `spec_worker.rs:243` — ので、その後は自然に armable になる)
3. PR body から追跡 issue へのリンクを取る: 先頭行の `Closes #N`(meguri が `flow.rs:1014` で必ず書く形式)を厳密にパースする `linked_issue(body) -> Option<i64>`。取れなければスキップ — **ブランチ規約とリンクの両方**が揃わない PR は対象外(looper と同じく片方では不十分)
4. オプトイン判定: `opt_in = "all"`、または PR 自体に `LABEL_AUTOMERGE`(直接貼っても効く)、または `get_issue(N)` の issue に `LABEL_AUTOMERGE`
5. 未解決 review thread がゼロ: `list_review_threads()` で `!t.resolved` が 1 つでもあればスキップ。**`thread_awaits_fixer`(fixer が返信済みで再レビュー待ち)よりも厳しくする** — fixer が返信した状態は「reviewer が納得した」ではないので、resolve されるまで arm しない(判定機構 = review thread の resolution は fixer と共有)
6. マーカーチェック: `pr_comments()` に現在 head のマーカーがあればスキップ
7. `MergePolicy`(候補が 1 つでもあるとき、プロジェクトごとに sweep 1 回だけ取得): `auto_merge_allowed` でない / strategy が `allowed_strategies` にない / `require_branch_protection = true` なのに `protected_with_required_checks` でない → **warn してスキップ**(watch 起動時の fail-fast をすり抜けて後から設定が変わったケース)

全部通ったら: `is_draft` なら `mark_pr_ready` → `enable_auto_merge(pr, strategy, head_sha)` → `comment_pr(armed_marker + 人間向け一行)` → `store.emit(None, "pr.automerge_armed", {...})`。

コメント本文の例(reviewer のコメント様式に倣う):

```
<!-- meguri:automerge armed head=abc123... -->
🔁 **meguri** — auto-merge (squash) を `abc123456789` で arm しました。
required checks が通れば GitHub がマージします。解除したい場合は PR の
auto-merge を無効化してください(この head には再 arm しません)。
```

### push 後の再判定

GitHub ネイティブ auto-merge は push されても armed のまま残る。`--match-head-commit` が守るのは arm 時点の head 一致だけだ。push で head が変わると新 head にはマーカーがないので、sweep が条件を**再判定**し、通れば再 arm(冪等成功)+ 新マーカーを打つ。条件が崩れていた場合(新しい未解決スレッド等)の**解除(disarm)はしない** — ドリフト検出は後段の merge-watch(別 issue)の仕事で、この issue では「armed のまま待つ」が仕様。

## 4. worker の引き継ぎ — `src/engine/flow.rs`

issue に `meguri:automerge` が付いていたら、worker が PR を開くときに引き継ぐ:

- `Checkpoint` に `automerge: bool` を追加。`claim_issue`(デフォルト `prepare_work`)で issue ラベルから記録する
- `open_pr`(`flow.rs:993`)で `cp.automerge` のとき:
  - **最初から non-draft で開く**(`draft = config && !cp.automerge`)。draft のまま required checks を待つ時間が無駄になるため
  - PR 作成直後に `add_pr_label(pr, LABEL_AUTOMERGE)` でラベルを PR へコピーする(以後の sweep は issue を見なくても判定できるが、コピー漏れに備えて sweep 側の issue ラベル判定も残す)

planner の spec PR も同じ `open_pr` を通るのでラベルは引き継がれるが、spec PR には `meguri:spec-reviewing` が付くので条件 2 で arm されない。spec フローが完走してラベルが外れた時点で初めて armable になる — 意図どおりの挙動。既に draft で開いてしまった過去の PR は、sweep が arm 時に ready 化する(条件チェック後の `mark_pr_ready`)。

## 5. fail-fast — `src/app.rs` / `src/main.rs`

`enabled = true` なのにリポジトリ側の前提が欠けている状態を、マージ時ではなく**起動時に**拒否する:

- `cmd_watch`(`app.rs:105`): deps 構築後、実効 auto_merge が enabled なプロジェクトごとに `merge_policy(default_branch)` を取得して検証。auto-merge 不許可 / strategy 不許可 / (require 時) protection なし → 理由を並べて `bail!`。検証ロジックは `auto_merger::validate_policy(cfg, policy) -> Result<(), Vec<String>>` として切り出し、sweep 内の条件 7・doctor と共有する
- `cmd_doctor`(`main.rs:77`): 項目を追加。enabled なプロジェクトについて「auto-merge: repo 設定 OK(strategy=squash, protection あり)」/ ❌ を出す。forge(async)を呼ぶため `cmd_doctor` を async 化する(`main` は既に `#[tokio::main]`)

## 6. 受け入れ基準

issue の受け入れ条件をそのままテストに写像する:

1. FakeForge e2e: 条件が揃った PR が arm される(strategy と head_sha が記録される)/ spec ラベル付き・hold・未解決スレッドあり・(non-draft 化されないままの)draft では arm されない
2. `--match-head-commit` 相当: arm 記録に head_sha が固定される。push で head が変わったら(マーカーが旧 head のみ)新 head で再判定・再 arm される
3. 人間が解除した head には再 arm しない: マーカーあり + FakeForge の armed 状態をクリアしても、同一 head では enable_auto_merge が呼ばれない
4. `enabled = true` + リポジトリ設定不足(auto-merge 不許可 / strategy 不許可 / protection なし)で watch 起動が fail-fast する。doctor にも同じ判定の項目が出る
5. 既に arm 済みの PR への再 arm は成功扱い(FakeForge は上書き成功、GhForge は「already enabled」を成功に読み替え)
6. worker: `meguri:automerge` 付き issue の PR は non-draft で開かれ、PR にラベルがコピーされる

## 7. テスト計画

- `tests/auto_merge_test.rs`(新規): FakeForge + `auto_merger::sweep` 直呼びで上記 1–3, 5 を検証。既存の `reaper_test.rs` / `fixer_test.rs` のパターン(FakeForge シード → sweep → 記録をアサート)に乗る
- `src/engine/auto_merger.rs` の unit test: `linked_issue` のパース、`armed_marker`/`head_already_armed`、`validate_policy`、arm 条件の純関数部分
- `src/config.rs`: `[pr.auto_merge]` のデフォルト・上書き・不正 strategy のロード失敗
- `tests/worker_test.rs` または flow の既存テスト: 引き継ぎ(non-draft + ラベルコピー)
- GhForge の gh コマンド組み立て(`--match-head-commit` の引数列)は既存 gh.rs テストの流儀に合わせ、判定ロジック(`parse_*` 相当)を関数に切って unit test

## 8. 触るファイル

- `src/config.rs` — `AutoMergeConfig` / `AutoMergeOptIn`、`PrConfig` へのネスト
- `src/forge/mod.rs` — `LABEL_AUTOMERGE`、`MergeStrategy`、`MergePolicy`、trait メソッド 3 つ、`PullRequest.is_draft`
- `src/forge/gh.rs` — `enable_auto_merge` / `mark_pr_ready` / `merge_policy` 実装、`isDraft` パース
- `src/forge/fake.rs` — armed 記録、ready 化、policy セッター、draft 追随
- `src/engine/auto_merger.rs`(新規)— sweep、arm 条件、マーカー、`validate_policy`
- `src/engine/mod.rs` — `pub mod auto_merger;`
- `src/engine/scheduler.rs` — watch ループから `auto_merger::sweep` を呼ぶ
- `src/engine/flow.rs` — `Checkpoint.automerge`、`claim_issue` での記録、`open_pr` の non-draft + ラベルコピー
- `src/app.rs` — `cmd_watch` の fail-fast
- `src/main.rs` — doctor 項目(async 化)
- `tests/auto_merge_test.rs`(新規)
- `README.md` / `README.ja.md` — `[pr.auto_merge]` の説明、rulesets 非対応と `require_branch_protection = false` の逃げ道
- `docs/adr/0003-auto-merge-github-native-arm-only.md` — 権威の分離(本 PR に同梱)

## 9. スコープ外(後段の issue)

- **merge-watch(ドリフト検出)**: armed 後に条件が崩れた PR の検出・解除(auto-merge 2/3)
- **reviewer ゲート(`require_clean_review`)**: meguri 自身のレビューが clean であることを arm 条件に足す(auto-merge 3/3)
- rulesets ベースの protection 検出(classic protection API のみ。当面は `require_branch_protection = false` で運用)
