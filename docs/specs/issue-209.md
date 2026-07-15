# issue-209 spec — self-review エスカレート時、commit 済みなら needs-human draft PR を publish する(fallback)

この spec の決定は一行で書ける。**self-review がエスカレートする瞬間に、branch が base より
commit で進んでいれば、push して `meguri:needs-human` ラベル付きの draft PR を開く。**
進んでいなければ従来どおりコメントのみ。設計判断そのものは ADR 0020(本 PR 同梱)に置いた。

## spec の深さ: design 相当

不確実性と波及範囲で決めた(実装量ではなく)。

- **未確定なこと**: PR の意味論を「検証済み成果物」と「未収束の証拠物件」に二分するという
  外部コントラクトの追加。フックを self-review のどこに置くか。証拠 draft をどのイベントで
  記録するか。
- **間違えた時の波及**: エスカレート経路は worker(実装)と planner(spec)が共有する
  flow 共通コード。forge 副作用(GitHub に PR が生える)を伴い、`pr_is_touchable` /
  fixer 系 / stats・ダッシュボード / auto-merger と接する。

永続状態・スキーマは触らないので veto rule の migration は必須ではないが、外部に見える PR の
意味を足すため、下に rollback を短く書く。

## 現状(なぜ穴が空くか)

- `src/engine/self_review.rs` — `needs_human` verdict / `escalate_unconverged`(max_rounds
  到達)/ review・fix ターン失敗の各パスが、いずれも `NeedsHuman` エラーを返して run を終える。
- そのエラーは `flow::drive` → `run_flow` の `Err` 分岐へ伝播し、run を Failed にして
  `flavor.escalate`(issue/task へ needs-human コメント)を呼ぶ。
- push / PR 作成は `open-pr` ステップ(`deliver` → `open_pr`、`src/engine/flow.rs`)の中。
  self-review のエスカレートは **push より手前**なので、成果物はローカル worktree に留まる。

## 決定 1: フックは self-review 関数の境界に1つだけ置く

エスカレート地点は4つある(`needs_human` verdict、`max_rounds`、review ターン失敗、
fix ターン失敗)。これらはすべて `NeedsHuman` エラーで表現される。個別に4箇所へ手を入れる
のではなく、`self_review()` の戻り値境界で `NeedsHuman` を1回だけ捕まえ、伝播の直前に
fallback publish を呼ぶ。

```rust
// self_review.rs 概略
pub(crate) async fn self_review(...) -> Result<StepFlow> {
    match self_review_inner(...).await {
        Err(e) if e.downcast_ref::<NeedsHuman>().is_some() => {
            // best-effort: 失敗しても元の NeedsHuman をそのまま伝播する
            publish_needs_human_draft(deps, run, cp, worktree, flavor).await;
            Err(e)
        }
        other => other,
    }
}
```

- `Stopped` / `Interrupted`(ユーザー stop・pane 死)は `Ok(StepFlow::…)` で返るので発火しない。
  publish するのは **本物のエスカレート**だけ。意図どおり。
- 元の `NeedsHuman` はそのまま伝播する。draft を開いても run は Failed(needs-human)のまま。
  `flavor.escalate` は従来どおり走り、issue には needs-human コメントが付く(通知は据え置き)。
  draft は証拠物件、コメントは通知、という役割分担になる。

## 決定 2: `publish_needs_human_draft` は best-effort、進んでいなければ no-op

`src/engine/flow.rs`(forge・push・PR 作成のロジックが集まる場所)にヘルパを置く。

1. deliver が `Pr` でない、または forge が無い(local mode)→ no-op。
2. `gitops::commits_ahead(worktree, base) == 0` → no-op(進んでいない。従来のコメントのみ)。
3. `gitops::push_branch` → 失敗したら emit してコメントのみに落とす(best-effort)。
4. `forge().create_pr(head, base, title, body, draft = true, labels = [LABEL_NEEDS_HUMAN])`
   —— **needs-human ラベルを作成と同一の forge 呼び出しで付ける**(下の決定 4)。
   - title は `flavor.pr_title(run, cp)` を流用。
   - body は **収束前提の `compose_pr_body` を使わない**。`self_review_details` が
     「clean after N rounds」と書いてしまうため。証拠物件用の別 composer を用意し、
     「これは未収束の証拠物件で、グリーン保証は無い」旨と、これまでの self-review round の
     記録(`cp.self_review_log`)を載せる。issue リンクは `Refs #N`(close しない)。
5. `pr.created` は emit しない。代わりに `self_review.escalated_draft`
   `{ pr, url, rounds, pending }` を emit する(ダッシュボードの「PR 作成 = 成功」と混ざらない)。

`create_pr` が失敗したら emit してコメントのみに落とす(best-effort)。run の終了挙動は
何があっても変わらない。成功したときは PR は **必ず** needs-human 付きで生まれている
(ラベルだけ失敗する中間状態が存在しない)。

## 決定 3: 二種類の PR を意味論で分ける

- 通常 PR = 検証済み成果物。`open_pr` が開き `pr.created` を emit する。
- needs-human draft = 未収束の証拠物件。`publish_needs_human_draft` が開き、
  draft + `meguri:needs-human` の組で区別され、`self_review.escalated_draft` を emit する。

この分離は spec より長生きするので ADR 0020 に置いた。`compose_pr_body` /
`post_self_review_status` が前提にしている「published PR は必ず self-review clean」という
不変条件は **通常 delivery 側でのみ真**であり、証拠 draft はその経路を通らない(別 composer・
別ステータス無し)ことでこの不変条件を破らない。

## 決定 4: needs-human ラベルは PR 作成と不可分にする(競合しない失敗モード)

**プラン査読の指摘への回答。** ラベルを PR 作成後の別呼び出し(`add_pr_label`)で付ける設計だと、
作成とラベル付与の間でプロセスが落ちる/ラベル付与だけ失敗すると、**ラベルなしの未収束 draft**
が残る。`pr_is_touchable`(`src/engine/mod.rs`)は draft を見ず `meguri:needs-human` だけで
除外するため、この隙間の draft を fixer / ci_fixer / conflict_resolver が claim して未完成
ブランチに書き込む競合が起きる。ADR 0020 の「生まれた瞬間から needs-human」「新しいガードは
要らない」が崩れる。

対策は「作成時にラベルを同時に付ける API にする」を採る。`Forge::create_pr` に
`labels: &[&str]` を足し、`gh pr create --label <label>`(存在しないラベルは先に
`ensure_label`)で作成と同時に貼る。**既存の `create_issue(title, body, labels)` と同型**の
前例踏襲で、PR 作成が成功した瞬間には必ずラベルが載っている。meguri 制御下の「作成後・ラベル前」
という窓は消え、`create_pr` が返せば `pr_is_touchable` は最初の観測時点から除外できる。

- `create_pr` が **失敗**を返したら、meguri は「delivered な draft」を一切記録せず
  コメントのみへ落ちる。ごく稀に「gh が PR を作った直後にエラー報告」した孤児 draft が
  残りうるが、これは GitHub が create+label をトランザクションで提供しない以上どの単一
  forge mutation も負う残余であり、少なくとも meguri は「publish した」と誤認しない。
- 既存の `open_pr` 側はこの `labels` 引数に空配列を渡し、挙動は不変(automerge ラベルの
  コピーは従来どおり作成後のまま — automerge ラベルは touchability のガードではないので、
  そこに窓があっても競合を生まない)。
- 却下した代替: 「ラベル失敗時に PR を閉じる/hold する」は、閉じる呼び出し自体がまた
  失敗しうる二段構えで窓を完全には消せない。「draft/本文マーカーも untouchable 条件に足す」は、
  `pr.draft = true` 運用時の**通常** draft PR(fixer 系が CI を保つべき対象)まで巻き込むため
  過剰。作成時ラベルが最小かつ窓ゼロ。

## アーキテクチャ影響

- 変更は self-review のエスカレート境界と flow のヘルパ、加えて `Forge::create_pr` の
  シグネチャ拡張(`labels: &[&str]`)に閉じる。状態機械・ステップ・checkpoint スキーマは
  無変更(draft の URL/番号を checkpoint に持つ必要はない — run は Failed で終わり、この情報を
  再利用する後続ステップが無い)。
- `create_pr` への `labels` 追加は forge トレイト・`gh.rs`・`fake.rs`・既存呼び出し元
  (`open_pr`)に波及するが、`open_pr` は空配列を渡すだけで挙動不変。
- `pr_is_touchable` は無変更で機能する。needs-human ラベルは PR 作成と同時に付くので
  (決定 4)、fixer 系は最初の観測時点から除外できる。
- planner・worker 両方に効く(`self_review()` は flow 共通)。planner は separate delivery
  では spec draft が、worker では実装 diff draft が証拠物件になる。

## 検討して見送った代替案

- **常に draft PR 先行**(self-review の前に push + draft 作成): 却下。理由は ADR 0020 §4
  ——`pr_is_touchable` が draft を見ないため ci_fixer が未完成ブランチを claim して二重書き込み、
  happy path でも forge に触れる、常時 CI/掃除コスト。
- **4つのエスカレート地点を個別に publish**: 却下。同型コードの散在は ADR 0012 が集約した
  「人間対応 ⇒ needs-human」の精神に反する。境界1点で捕まえる方が漏れが無い。

## observability

- 新イベント `self_review.escalated_draft`(pr / url / rounds / pending)。
- push 失敗・PR 作成失敗(ラベル付与は作成に含まれる)は個別に emit(例:
  `self_review.draft_push_failed` / `self_review.draft_failed`)し、best-effort が黙って
  握り潰さないようにする。
- `pr.created` は通常 delivery でのみ発火し続ける(不変)。

## migration & rollback

- 永続状態・DB スキーマの変更なし。
- 追加は純粋に上乗せ。fallback を無効化すれば従来のコメントのみ挙動に戻る(rollback は
  ヘルパ呼び出しを外すだけ)。
- forge 側に生えた証拠 draft は、人間が PR を閉じれば消える。マージは draft ゲートで
  ブロックされるため、誤って本流に入ることはない。

## テスト計画

FakeForge は `create_pr`(draft フラグ・labels を記録)を持つので、実 forge 無しで検証できる
(`create_pr` に `labels` を足すのに合わせ、fake も作成時ラベルを記録するよう更新する)。

- **単体(self_review.rs)**: エスカレート時、`commits_ahead > 0` なら FakeForge に
  draft=true・**作成と同時に** `meguri:needs-human` が載った PR が1件記録され
  (別 `add_pr_label` 呼び出しに依存しない)、`self_review.escalated_draft` が emit され、
  run は Failed のままであること。`commits_ahead == 0` なら PR が作られないこと。
  push 失敗時・`create_pr` 失敗時にコメントのみへ落ち、meguri が draft を記録しないこと。
- **単体**: 通常 delivery 側(clean 収束→ `open_pr`)が `pr.created` を emit し、draft でない
  PR を開く挙動が無変更であること。
- **統合(tests/)**: 既存の虚偽申告・validation feedback 系テストと同じ土台
  (`fake_agent.sh` + 実 tmux/git)で、未収束エスカレート run が実 origin に needs-human draft
  を残すことを1本追加(planner か worker のどちらか一方で代表)。

## 触るファイル

- `src/engine/self_review.rs` — エスカレート境界で `NeedsHuman` を捕まえ fallback publish を呼ぶ。
- `src/engine/flow.rs` — `publish_needs_human_draft` ヘルパ + 証拠物件用の PR body composer。
  `open_pr` の `create_pr` 呼び出しに空の `labels` を渡す。
- `src/forge/mod.rs` — `Forge::create_pr` に `labels: &[&str]` を追加。
- `src/forge/gh.rs` — `gh pr create --label`(存在しないラベルは先に `ensure_label`。
  `create_issue` と同型)。
- `src/forge/fake.rs` — `create_pr` が作成時ラベルを PR レコードに記録するよう更新。
- `docs/adr/0020-escalate-time-needs-human-draft-pr-as-evidence.md` — 決定の記録(本 PR 同梱)。
- テスト — `src/engine/self_review.rs` の `#[cfg(test)]` に単体、必要なら `tests/` に統合1本。

## 受け入れ基準(acceptance criteria)

1. self-review が `needs_human` verdict / `max_rounds` / review・fix ターン失敗のいずれで
   エスカレートしても、`commits_ahead > 0` なら draft + `meguri:needs-human` の PR が1件開く。
2. `commits_ahead == 0` なら PR は作られず、従来どおりコメントのみ(挙動不変)。
3. 証拠 draft は `pr.created` を emit せず `self_review.escalated_draft` を emit する。
   run の終了ステータスは Failed(needs-human)のまま。
4. draft の本文が「未収束の証拠物件・グリーン保証なし」を明記し、issue を close しない
   (`Refs #N`)。
5. `meguri:needs-human` は PR 作成と同一の forge 呼び出しで付き、「作成後・ラベル前」の
   ラベルなし draft という中間状態が存在しない。`create_pr` 成功時は必ずラベル付き、
   失敗時は meguri が draft を記録せずコメントのみへ落ちる。
6. push / PR 作成が失敗しても run は落ちず、コメントのみへ best-effort で落ちる。
7. happy path(clean 収束)は無変更 — 通常 PR は非 draft で `pr.created` を emit する
   (`open_pr` は `create_pr` に空 `labels` を渡す)。
8. worker と planner の両方でこの fallback が効く(flow 共通)。
9. 既存テストが全て通る(特に self-review 収束系・escalation 系の非破壊)。

## スコープ外

- エスカレートコメントへ draft URL を差し込む連携(`escalate_task` は URL を取らない。
  やるなら別 issue)。
- 証拠 draft のライフサイクル自動管理(放棄 draft の自動クローズ等)。人間が閉じる前提。
- autonomy モード別の挙動分岐。この fallback はモード非依存(ADR 0012 の方針を踏襲)。
