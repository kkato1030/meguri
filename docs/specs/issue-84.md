# issue-84 spec — 実装 diff の AI レビューループ新設(impl-reviewer)

いまの meguri には妙な非対称がある。fixer は review スレッドを読んで直す機構として完成しているのに(`src/engine/fixer.rs`)、そのスレッドを meguri 自身が生成することは一度もない。reviewer ループがレビューするのは `meguri:spec-reviewing` の spec PR だけで、spec-worker が実装を積む頃にはそのラベルはもう消えている(`src/engine/spec_worker.rs:264-277`)。卓球台とラケットは揃っているのに、サーブを打つ者がいない。実装 diff は人間か外部 bot が最初の一球を打つのを待っている。

この spec の決定は一行で書ける。**実装 diff の AI レビューループ(impl-reviewer)を新設し、その findings を inline review スレッドとして投稿して、既存の reviewer↔fixer ping-pong に流し込む。**

## 決定: 新設する(割り切らない)

issue の論点 1 に対する答え。理由は三つ。

1. **消費側が既に完成している。** fixer の入力は「未解決の review スレッド」であり、作者を問わない(`src/engine/fixer.rs:38-44` の `thread_awaits_fixer` は author を見ない)。AI が作ったスレッドも人間のスレッドと寸分違わぬ経路で fix される。新設コストの大半(fix 側、収束機構、re-review 検知)は既に払い終わっている。
2. **設計意図が既にそちらを向いている。** reviewer のモジュール doc は「Inline review threads are future work」と自ら書いている(`src/engine/reviewer.rs:7-8`)。
3. **人間の merge ゲートは残る。** impl review は approve も request-changes もしない(後述の event=COMMENT)。ゲートを置き換えるのではなく、ゲートに届く前の品質を上げる。判断の重心は変わらない。

割り切り案(README 一行明記のみ)を退けた理由: fixer という機構の半身が永久に外部依存のままになる。meguri は自律 issue→PR 装置であり、「自分の書いた diff を一度も見返さずに人間へ渡す」のは設計思想の穴であって仕様ではない。

この決定(AI レビューの対象を spec と実装 diff の両方にする、人間は merge ゲートに残る)は spec より長生きするべきなので **ADR 0004**(本 PR 同梱)に置いた。

**issue の「まずやること」(README に spec 限定の一行明記)について**: 本 spec は同一ブランチで実装まで進むため、「AI レビューは spec 限定」という中間状態の一行は、同じ PR が書いた直後に嘘にする。README は実装後の最終状態(spec + 実装 diff の両方)を一度だけ書く。受け入れ基準 7 で担保する。

## 形: 専用ループ(reviewer の拡張ではなく)

issue の論点 2 に対する答え。reviewer はラベル状態機械に強く結合している — `spec-reviewing` を入力に取り、clean なら `spec-ready` へ遷移させるのが仕事の半分だ(`src/engine/reviewer.rs:545-575`)。impl review にはラベル遷移が一切ない: 入力はラベルレスの状態ベース discovery(fixer と同型)、出力は review スレッドとマーカーコメントのみ。入口も出口も違うものを一つのループに畳むと、共有されるのは中間の「worktree で diff を読む」だけになる。よって **`src/engine/impl_reviewer.rs` を新設**し、reviewer とは `.meguri/pr-diff.patch` / `.meguri/review.json` の規約と head マーカーのパターンだけを共有する(マーカー文字列自体は `<!-- meguri:impl-review head=... -->` で別物。reviewer のマーカーと衝突しない)。

## 変更箇所

### 1. Forge に `create_pr_review` を追加 — `src/forge/mod.rs:189-251`, `gh.rs`, `fake.rs`

現状の Forge には review スレッドを**作る**手段がない(`comment_pr` = 会話コメント、`reply_review_thread` = 既存スレッドへの返信のみ)。ping-pong 再利用の要はこの一メソッド。

```rust
/// Draft of one inline review comment (a thread anchor).
pub struct ReviewCommentDraft {
    pub path: String,
    pub line: Option<u64>,
    pub body: String,
}

/// Post a PR review with inline comments (event=COMMENT — never
/// approve/request-changes; the human merge gate stays human).
async fn create_pr_review(&self, pr: i64, body: &str, comments: &[ReviewCommentDraft]) -> Result<()>;
```

gh 実装は REST `gh api repos/{owner}/{repo}/pulls/{n}/reviews`(`event=COMMENT`, `comments[]` に path/line/side=RIGHT)。inline anchor が diff 上の行に載らず 422 で落ちた場合は、findings を本文に畳んで `comment_pr` にフォールバックする(レビューを失わない。ただし fixer には乗らないので、フォールバックはログに残す)。fake 実装は `list_review_threads` が返すストアに未解決スレッドとして積む — これが fixer との連結テストの土台になる。

### 2. 新ループ `src/engine/impl_reviewer.rs`

reviewer(680 行)と同じ骨格: detached worktree at head → diff を `.meguri/pr-diff.patch` に置く → agent turn が `.meguri/review.json` を書く → settle で投稿。verdict ファイルだけ findings の配列を持つ形に拡張する:

```json
{"verdict": "clean" | "findings",
 "review": "<Markdown サマリ>",
 "findings": [{"path": "src/x.rs", "line": 42, "body": "<指摘>"}]}
```

**discover**(`list_open_prs` から絞る。fixer 型):
- state open、head ブランチ `meguri/` プレフィックス(meguri 自身の PR のみ)
- `spec-reviewing`(spec 段階)/ `spec-ready`(worker が実装中 — fixer と同じ理由で不可侵)/ `working` / `hold` を持たない
- CI rollup が `Success`(`pr_check_rollup`。Failure は ci-fixer の縄張り、Pending は次 tick — 変わりうる head をレビューしない)
- `thread_awaits_fixer` なスレッドが無い(fix が先。修正 push 後の新 head を見る)
- 現 head が未レビュー(`<!-- meguri:impl-review head=... -->` マーカー、reviewer と同型の dedup)
- ラウンド上限未満(後述)

**settle**(ラベルは一切触らない):
- findings → `create_pr_review`(サマリ + inline comments)+ マーカー入りサマリコメントを `comment_pr` で投稿(PR review の本文は `pr_comments` に載らないため、dedup マーカーは reviewer と同じく会話コメント側に置く)
- clean → マーカー入りの短い clean コメントのみ。スレッドを作らないので fixer は反応しない — それが収束の栓の一つ

### 3. 収束の担保 — AI↔AI 無限 ping-pong を止める三つの栓

1. **head マーカー**: 同一 head は一度きり(reviewer の `head_already_reviewed` と同型、`src/engine/reviewer.rs:41-48`)。
2. **ラウンド上限**: `pr_comments` 中の impl-review マーカー総数が `impl_max_rounds` に達した PR は discovery から外す。以降は静かに引く — `needs-human` は付けない(レビュー済み PR が人間の merge 待ちで開いているのは正常状態であって異常ではない)。
3. **clean はスレッドを作らない**: fixer の入力が生まれず、ループが自然に止まる。

fixer が push → 新 head → 新ラウンドのレビュー(古いスレッドは fixer の 🔁 reply で parked のまま。impl-reviewer は既存スレッドに返信も resolve もしない — resolve は人間の仕事、`tests/fixer_test.rs:262` の想定どおり)。

### 4. `default_loops()` への挿入 — `src/engine/mod.rs:86-97`

ADR 0001 の逆順原則(merge に近い順)で **FixerLoop の直後、SpecWorkerLoop の前**。impl-review 対象の残工程は「レビュー→fix→人間 merge」で、spec-worker 対象(これから実装)より merge に近い。

### 5. config `[review]` セクション — `src/config.rs`

```toml
[review]
impl_enabled = true    # キルスイッチ(false でループ全体が沈黙)
impl_max_rounds = 3    # head マーカー総数の上限
```

`CleanConfig`(`src/config.rs:88-113`)の前例に倣った小さな struct。watch の毎 tick 再読込(#73)に自動で乗るので、運転中に絞れる。per-project override は今回は見送り(スコープ外)。

### 6. README 2 枚(`README.md` / `README.ja.md`)

「Spec-first flow」の後に impl review の段落を追加: AI レビューの対象は spec PR と実装 diff の両方であること、findings は review スレッドとして fixer に流れること、head ごと 1 回・ラウンド上限・`[review]` での無効化。Labels 表(`README.md:127-138`)は変更なし — このループはラベルレス。

## 変わらないもの(意図どおり)

- **reviewer(spec)ループは無変更。** spec の inline thread 化は将来の別 issue(`create_pr_review` が入るので土台はできる)。
- **fixer は無変更。** AI 生成スレッドも人間のスレッドと同じ入力。`pr_is_fixable` の spec-ready 除外もそのまま。
- **ラベル状態機械は無変更。** 新ラベルは足さない。何をレビューしたかの真実は forge 上のマーカー(Authority 原則)。
- **人間の merge ゲート。** event=COMMENT のみ。approve / request-changes は決してしない(ADR 0004)。

## 受け入れ基準(acceptance criteria)

1. CI green で head 未レビューの meguri 実装 PR(`meguri/` head、spec 系ラベルなし)が impl-reviewer の discovery に載る。
2. findings 時、inline review スレッドが作成され、**同じ FakeForge 上で fixer の discover がその PR を target にする**(ping-pong 接続の実証)。
3. clean 時、マーカーコメントのみが投稿され、スレッドは作られず、fixer は反応しない。
4. 同一 head は再レビューされない。マーカー総数が `impl_max_rounds` に達した PR は discovery から外れる。
5. `spec-reviewing` / `spec-ready` / `working` / `hold` の PR、CI Failure/Pending の head、`thread_awaits_fixer` なスレッドを持つ PR は対象外。
6. `impl_enabled = false` で discovery が常に空。
7. README(en/ja)が「AI レビューは spec と実装 diff の両方」であることと収束の栓を記述している。
8. 既存テストが全部通る(特に `fixer_test.rs` / `reviewer_test.rs` / `scheduler_test.rs` の非破壊)。

## テスト計画

`tests/impl_reviewer_test.rs` を新設し、既存の reviewer/fixer テストのパターン(FakeForge + scripted agent pane)に乗る。FakeForge に `create_pr_review` を実装して `list_review_threads` のストアへ反映させるのが肝 — これで受け入れ基準 2(impl-reviewer が作ったスレッドで fixer discovery が発火する連結テスト)が FakeForge だけで書ける。discovery のフィルタ条件(基準 1, 4, 5, 6)はループ単体で網羅する。

## 触るファイル

- `src/forge/mod.rs` / `src/forge/gh.rs` / `src/forge/fake.rs` — `create_pr_review` + `ReviewCommentDraft`
- `src/engine/impl_reviewer.rs` — 新ループ(新規)
- `src/engine/mod.rs` — `default_loops()` への挿入
- `src/config.rs` — `[review]` セクション
- `README.md` / `README.ja.md` — AI レビューの対象範囲の明記
- `tests/impl_reviewer_test.rs` — 新規
- `docs/adr/0004-ai-review-covers-implementation-diffs.md` — 決定の記録(本 PR に同梱済み)

## スコープ外(将来の話)

- spec reviewer の inline thread 化(reviewer.rs の「future work」はそのまま。`create_pr_review` の上に素直に乗るが別 issue)。
- レビュー観点の config カスタム(prompt への追加指示)、per-project の `[review]` override。
- 外部レビュー bot との重複抑制(外部 bot がいる環境では `impl_enabled = false` で十分)。
