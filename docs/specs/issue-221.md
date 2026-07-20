# issue-221 spec — merge tail を Op に載せ替え、BEHIND を Op(UpdateBranch) で閉じる

ADR 0012（level-triggered reconciler）移行のスライス 1 / 5。正は
[`docs/adr/0012-loops-are-emergent-level-triggered-reconciler.md`](../adr/0012-loops-are-emergent-level-triggered-reconciler.md)。
本 spec はその決定を最初の縦切りへ落とし込む足場であり、実装時に刈られる。

## 深さの判断：design spec（理由）

新しい**公開型 `Op`** と**新しい forge メソッド `observe_merge_tail` / `update_branch`** を足す＝
公開 contract に触れる。
さらに S2〜S5 がここで決めた `Op` / observe の形の上に載るため、間違えたときの波及が広い
（uncertainty 中 × blast radius 大）。公開 contract に触れる以上、veto 規則により migration &
rollback 章は必須。よって design 段に上げる。ただし永続状態・スキーマは**この slice では一切
足さない**（後述の決定 6：forge 権威に触れない Op から始める、が slice の肝）。

## 受け入れ基準

1. **BEHIND が回帰テストで閉じる。** arm 済み PR の base が進んで `mergeStateStatus = BEHIND` に
   なった状態を与えると、`next_step` が `Op(UpdateBranch)` を返す。update-branch で head が動いた
   次の観測で（head-keyed marker が外れるので）自然に再 arm される。この 2 tick を FakeForge 上の
   回帰テストで通す。native / orchestrator 両モードで閉じる。
2. **observe 一括クエリの API コスト実測値が記録される。** 1 sweep あたりの forge リクエスト数
   （と、取得できれば GraphQL `rateLimit.cost`）を runtime event として emit し、その計測方法を
   コードに残す。PR 本文に実測ベースライン（旧：loop ごと個別叩き vs 新：一括）を数値で書く。
3. **`Op(ArmAutoMerge)` / `Op(MergePr)` が判断を変えずに移設されている。** ADR 0003（arm-only）と
   ADR 0009（orchestrator merge）の分岐・ゲート（opt-in / blocking label / review threads /
   pr-review gate / autonomy / merge policy）はビット等価で `next_step` + `act` に移る。既存の
   `auto_merger` / `merge_watch` の単体テストの意味論を落とさない。
4. **「ちょうど 1 つの所有 arm」property test が緑。** merge-tail の観測状態空間を網羅し、
   `next_step` が全状態でちょうど 1 つの結論（`Op` / `Wait` / no-op）を返す（所有の欠落＝BEHIND の
   穴も、二重所有も無い）ことを property test で守る。

## 決定

### 決定 1：`Op` は「この slice が実行する変種だけ」を導入する

ADR 0012 §4 は `Op(UpdateBranch | ArmAutoMerge | MergePr | Finalize | Escalate)` を宣言するが、
未使用の enum 変種は `-D warnings`（dead_code）で CI を落とす。よって S1 は merge tail が実際に
実行する 4 変種だけを足す：

```rust
pub enum Op {
    UpdateBranch,   // BEHIND を閉じる（本 slice の新規）
    ArmAutoMerge,   // ADR 0003：native arm（不変移設）
    MergePr,        // ADR 0009：orchestrator / AlreadyClean 確定（不変移設）
    Escalate,       // pr-review 失敗・Stuck を needs-human へ（不変移設）
}
```

`Finalize`（reaper）・`EnsureClone`（Repo Kind）は行き先の slice（S4）で足す。`Step` の二相
（`Agent` / `Op` / `Wait`）のうち、merge tail は agent を起こさないので S1 が扱うのは `Op` と
`Wait` の 2 つ。`Wait` は「所有 arm が今は静止を選んだ」を表す（人間が auto-merge を無効化した
HumanDisabled、pr-review pending、未解決スレッド待ちなど）。

### 決定 2：observe は merge-tail 用の一括スナップショット。API コストを計測する

現状、`auto_merger` と `merge_watch` は同じ tick で `list_open_prs` を二度叩き、さらに PR ごとに
`pr_comments` / `list_review_threads` / `commit_status` / `merge_policy`（arm 側）と
`pr_comments_meta` / `pr_merge_state` / `pr_check_rollup`（watch 側）を個別に叩く。これを
informer cache 型の **1 回の一括クエリ**に畳み、PR ごとの `Snapshot` を作る（既存 `OpenPrCache`
＝`list_open_prs` の per-tick 共有、の考えを merge-tail の全フィールドへ拡張する）。

engine は `dyn Forge` 越しにしか observe できないので、一括クエリは**トレイトの 1 メソッド**として
足す（PR ごとの生観測と、その観測にかかった API コストを一緒に返す）：

```rust
/// merge tail の observe：開いている meguri PR 群を 1 回のクエリ束で観測し、
/// PR ごとの生観測と、その観測にかかった API コストを返す。engine 側の純関数
/// next_step はこの生観測から Snapshot を組み立てる（forge は権威境界なので
/// 判断は持たせない）。
async fn observe_merge_tail(&self) -> Result<MergeTailObservation>;

pub struct MergeTailObservation {
    pub prs: Vec<PrObservation>,   // 開いている PR ごとの生観測
    pub cost: ObserveCost,
}
pub struct PrObservation {
    pub pr: PullRequest,                 // number / head_sha / head_branch / labels / is_draft / body
    pub merge: Option<MergeState>,       // mergeable / mergeStateStatus / auto_merge_enabled（読めなければ None＝transient）
    pub armed_since: Option<String>,     // 最古の arm marker の createdAt（marker 無し＝未 arm）
    pub has_unresolved_thread: bool,     // 未解決 review スレッドの有無
    pub rollup_failure: bool,            // required check の失敗（Blocked の切り分け用）
    pub pr_review: Option<CommitStatusState>, // meguri/pr-review status（gate 無効時は照会しない）
}
pub struct ObserveCost { pub requests: u32, pub graphql_cost: Option<u32> }
```

- **`GhForge`**：`list_open_prs` の GraphQL を拡張し、上記フィールド（`mergeStateStatus` /
  `mergeable` / `autoMergeRequest` / `isDraft` / `labels` / arm marker を含む comments の
  `createdAt` / `reviewThreads.isResolved` / `statusCheckRollup` / `meguri/pr-review` の
  commit status）を 1 クエリで引く。`cost.graphql_cost` は応答の `rateLimit { cost }` から、
  `cost.requests` は実際に投げた HTTP 回数から埋める（取れなければ `graphql_cost = None`）。
- **`FakeForge`**：既存のインメモリ map（`merge_status` / `auto_merge_enabled` / `pr_comments` /
  `threads` / `checks` / `commit_statuses` / labels）から `PrObservation` を組み立て、
  `cost.requests = 1`（一括なので PR 数に依らず 1）を返す。これが受け入れ 1 の BEHIND 回帰テストと
  受け入れ 2 の API コストテストの土台になる。

`merge_policy`（base 単位・元から 1 sweep 1 回）と、PR ラベルに opt-in が無い PR だけ引く issue
ラベル fallback は、この一括 observe の外に残す（前者は変更なし、後者は worker が PR へラベルを
コピーするので稀）。純データの `Snapshot` は engine 側で `PrObservation` から組む（壁時計・I/O を
持ち込まない）。

**API コスト実測**（受け入れ 2）：`observe_merge_tail` が返す `cost` を
`store.emit(None, "merge_tail.observe_cost", { "requests": r, "graphql_cost": c, "prs": p })`
で毎 sweep 記録する（`c` は GraphQL `rateLimit { cost }` が取れた場合のみ）。旧実装の「PR 数に
比例した個別叩き」と対比して、「informer cache 化で API コストが観測可能・制御可能になる」（ADR
0012 正の帰結）を数値で裏づける。一過性の実測ベースラインは PR 本文へ書く。

### 決定 3：`next_step` を純関数化し、所有 arm を property test で守る

decide は純関数 `next_step(&Snapshot) -> Step`。同じ snapshot なら常に同じ Step。これにより
「merge-tail の全観測状態にちょうど 1 つの所有 arm」を網羅 property test で機械的に守れる
（ADR 0012 §3）。BEHIND は**この property の穴**として捉える — 旧実装では「arm 済み × base 進行」
に所有 arm が無かった（`merge_watch` は Behind+stale を Stuck へ escalate するだけで、直さない）。

merge-tail 状態空間と所有 arm（抜粋・優先順）：

| 観測状態 | 結論 |
|---|---|
| terminal（merged / closed） | no-op |
| snapshot 読めない（transient） | no-op（次 sweep 再試行） |
| human が auto-merge 無効化 | `Wait`（HumanDisabled） |
| Dirty / Conflicting | no-op（conflict-resolver の縄張り、S3 で arm 化） |
| Blocked + 失敗 required check | no-op（ci-fixer の縄張り、S3 で arm 化） |
| **arm 済み × BEHIND** | **`Op(UpdateBranch)`**（本 slice で埋める穴） |
| arm 済み × Clean/Unstable | `Wait`（GitHub がマージする） |
| arm 済み × Blocked・非 behind・stale | `Op(Escalate)`（Stuck backstop、不変） |
| 未 arm × pr-review 失敗 | `Op(Escalate)`（不変） |
| 未 arm × pr-review pending / 未解決スレッド | `Wait`（不変） |
| 未 arm × 適格・native | `Op(ArmAutoMerge)`（不変） |
| 未 arm × 適格・orchestrator × Mergeable | `Op(MergePr)`（不変） |
| 未 arm × orchestrator × BEHIND | `Op(UpdateBranch)`（orchestrator 側の BEHIND も閉じる） |

### 決定 4：BEHIND の解は `Op(UpdateBranch)` + 再 arm。再 arm は「創発」させる

update-branch は base を head に取り込み、**head sha を進める**。arm marker は head-keyed
（`armed_marker(head_sha)`）なので、head が動くと marker が外れ、次の観測で「未 arm × 適格」と
判定されて自然に再 arm される。つまり「再 arm」は明示の第 2 ステップではなく、level-triggered な
観測から**創発**する（ADR 0012 §4「arm 1本で閉じる」の実体）。エッジ駆動の「update したら arm しろ」
という状態遷移をコードに書かない — ここが本 slice の設計上の要。

orchestrator モードも同型：BEHIND なら `pr_mergeable` が Mergeable にならず現状は永久 skip する
（もう 1 つの BEHIND 系 stall）。`Op(UpdateBranch)` 後の観測で Mergeable になり、`Op(MergePr)` が
発火して閉じる。

### 決定 5：forge に `update_branch` を足す（TOCTOU-safe）

```rust
/// PR のブランチに base を取り込む（PUT /repos/{o}/{r}/pulls/{n}/update-branch）。
/// `expected_head_sha` を渡し、観測時と head がずれていれば GitHub が弾く
/// （arm / merge と同じ --match-head-commit 相当の TOCTOU 安全性）。
async fn update_branch(&self, pr: i64, expected_head_sha: &str) -> Result<UpdateBranchOutcome>;
```

`UpdateBranchOutcome` は「更新した / 既に最新（no-op）/ head がずれていた」を区別する（arm の
`ArmOutcome` に倣う。ずれ・既最新は次 sweep が再導出するので silent skip）。gh 実装は上記 REST を
`gh api --method PUT`。fake 実装は呼び出しを記録し、テスト側が base 進行→head 更新を仕込める。

### 決定 6：モジュール構成 — 2 sweep を 1 つの merge-tail reconcile へ畳む。名前衝突を避ける

`auto_merger` / `merge_watch` を **1 つの merge-tail モジュール**（observe → next_step → act）へ
畳む。純粋ヘルパ（`linked_issue` / `validate_policy` / `armed_marker` / `classify` 相当）はそのまま
移す。scheduler poll-tick の `auto_merger::sweep` + `merge_watch::sweep` の 2 呼び出しは、この
モジュールの 1 エントリに置き換える。

- **workqueue / requeue / `Verdict` は S1 では作らない。** dispatch は既存の poll-tick sweep の
  まま（ADR 0012 §6 の workqueue は S3）。S1 は「sweep の act を Op に載せ替える」縦切りに徹する。
- **`reconcile` という名前を新設しない。** 既存 `src/engine/reconcile.rs`（#142 body-edit 再注意）
  と衝突する。改名（`reconcile_body_edits`）は ADR 0012 が S4 と決めているので、S1 では触らない。
  `Op` / `Step` の定義は当面この merge-tail モジュールに置き、reconciler core への昇格は S3/S4。

## 変更箇所

- `src/forge/mod.rs`：`Op` は engine 側に置く（forge は権威境界）。ここへトレイトメソッド
  `observe_merge_tail`（+ `MergeTailObservation` / `PrObservation` / `ObserveCost`）と
  `update_branch`（+ `UpdateBranchOutcome`）を追加。
- `src/forge/gh.rs`：`observe_merge_tail` の GraphQL 実装（決定 2 のフィールドを 1 クエリ束で引き、
  `rateLimit.cost` と HTTP 回数を `ObserveCost` に詰める）と、`update_branch` の gh 実装
  （REST `PUT .../update-branch` + `expected_head_sha`）。
- `src/forge/fake.rs`：`observe_merge_tail` を既存インメモリ map から組んで返す（`requests = 1`）、
  `update_branch` の記録・base 進行の仕込み。
- `src/engine/`：merge-tail モジュール新設（`Op` / `Snapshot` / `next_step` / `act` / observe）。
  `auto_merger.rs` / `merge_watch.rs` の純粋ロジックを移設し、両 sweep を畳む。
- `src/engine/scheduler.rs`：poll-tick の 2 sweep 呼び出しを 1 つに置換（`auto_merger::sweep` /
  `merge_watch::sweep` の行）。
- `src/store` 直接変更なし（永続状態を足さない）。event kind `merge_tail.observe_cost` を emit。
- テスト：`next_step` property test、BEHIND 回帰テスト（native / orchestrator）、移設等価テスト。

## Architecture impact

- 公開型 `Op` と forge メソッド `observe_merge_tail` / `update_branch` が増える（純増、既存 API の
  削除・変更なし）。`Loop` trait は撤去しない（S4）。移行中は「新（merge-tail は Op 経由）× 旧
  （他ループは従来通り）」が併存する — ADR 0012 の想定どおり。
- observe が informer cache 化し、merge tail の API 叩きが「PR×メソッド」から「1 一括クエリ」へ。
- `next_step` の純関数化で、以後のトリガ追加（S2〜）が「arm を 1 本足す」で済む土台ができる。

## Alternatives considered

- **`Op` enum を ADR 宣言どおり全変種で先に定義する** → 未使用変種が `-D warnings` を落とす。却下。
  変種は slice が実行するぶんだけ足す（決定 1）。
- **BEHIND を明示の「update→arm」2 ステップ状態遷移で書く** → エッジ駆動に戻り、level-triggered の
  利点（穴を property で塞ぐ）を捨てる。却下。再 arm は head-keyed marker で創発させる（決定 4）。
- **S1 で workqueue / `Verdict` / requeue まで作る** → slice が縦に厚くなり独立 rollback 性を損なう。
  却下。ADR 0012 の slice 順（S3 で queue）に従う。
- **既存 `merge_watch` の Behind→Stuck escalation を残す** → BEHIND を人手待ちにする現状の欠陥を
  温存する。却下。Behind は `Op(UpdateBranch)` が所有し、Stuck は「Blocked・非 behind・stale」に狭める。

## Migration & rollback

- **永続状態・スキーマ変更なし（この slice の設計上の肝）。** 権威は forge のまま（arm marker /
  ラベル / PR 状態）。sqlite に新テーブル・新カラムを足さない。したがってデータ移行は不要。
- **rollback は PR revert のみで完結。** 併存設計なので、revert すれば旧 `auto_merger` /
  `merge_watch` の挙動へ戻る。中途半端な永続状態が残らないため、部分適用/巻き戻しの危険がない。
- **operational risk：update-branch は PR ブランチに commit を積む**（マージ操作族の一員）。
  `expected_head_sha` ピン留めで、観測後に head が動いた PR は GitHub が弾く（意図しない head を
  更新しない）。opt-in / blocking label / autonomy=full ゲートを arm と同じく通過した PR にのみ
  適用し、対象は `meguri/` 自前ブランチに限る。

## Observability

- `merge_tail.observe_cost`（requests / graphql_cost / prs）を毎 sweep emit（受け入れ 2）。
- `Op` 実行を event 化：`pr.branch_updated`（UpdateBranch）を新設。既存の `pr.automerge_armed` /
  `pr.automerge_merged` / `pr.merge_watch_stuck` / `automerge.pr_review_failed` は emit 名を保つ
  （移設等価・受け入れ 3）。
- BEHIND を閉じた回数が event から追える（`pr.branch_updated` の件数）。

## Test strategy

- **純関数 property test**（受け入れ 4）：`Snapshot` のフィールド直積を網羅し、`next_step` が全状態で
  ちょうど 1 結論・所有の欠落と二重所有が無いことを検証。BEHIND セルが `Op(UpdateBranch)` を返す
  ことを含む。
- **BEHIND 回帰テスト**（受け入れ 1）：FakeForge で arm→base 進行→`Op(UpdateBranch)`→head 更新→
  次観測で再 arm、を native / orchestrator 両モードで通す。
- **移設等価テスト**（受け入れ 3）：`auto_merger` / `merge_watch` の既存単体テスト（`validate_policy`
  / `classify` / marker / blocking label など）を新モジュールへ引き継ぎ、意味論を落とさない。
- **API コスト計測テスト**（受け入れ 2）：`observe_merge_tail` が返す `ObserveCost.requests` が PR 数に
  依らず一定（FakeForge では 1）であること、および emit された `merge_tail.observe_cost` event を
  アサート。旧実装の「PR 数比例の個別叩き」との対比を裏づける。
- 変更後は `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo nextest run`
  / `cargo test --doc` を通す。

## 永続知識の振り分け（ADR / ドメイン文書）

本 slice の設計判断（level-triggered 再構成・Op 二相・observe informer cache・所有 arm invariant・
BEHIND を Op(UpdateBranch) で閉じる・不変移設）は**すべて ADR 0012 に既に記録済み**。この spec は
その実装の縦切りであり、新規の恒久判断は生まない。よって**新 ADR は追加しない**。slice ローカルな
選択（Op 変種の逐次導入・update-branch の TOCTOU 設計・再 arm の創発）はコードとモジュール doc へ
蒸留する。API コスト実測値は一過性のベースラインなので runtime event と PR 本文に残す。
