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
4. `forge().create_pr(head, base, title, body, draft = true)`。
   - title は `flavor.pr_title(run, cp)` を流用。
   - body は **収束前提の `compose_pr_body` を使わない**。`self_review_details` が
     「clean after N rounds」と書いてしまうため。証拠物件用の別 composer を用意し、
     「これは未収束の証拠物件で、グリーン保証は無い」旨と、これまでの self-review round の
     記録(`cp.self_review_log`)を載せる。issue リンクは `Refs #N`(close しない)。
5. `forge().add_pr_label(pr, LABEL_NEEDS_HUMAN)`。生まれた瞬間から needs-human を付ける。
6. `pr.created` は emit しない。代わりに `self_review.escalated_draft`
   `{ pr, url, rounds, pending }` を emit する(ダッシュボードの「PR 作成 = 成功」と混ざらない)。

`create_pr` / `add_pr_label` の失敗は best-effort(emit してコメントのみに落ちる)。run の
終了挙動は何があっても変わらない。

## 決定 3: 二種類の PR を意味論で分ける

- 通常 PR = 検証済み成果物。`open_pr` が開き `pr.created` を emit する。
- needs-human draft = 未収束の証拠物件。`publish_needs_human_draft` が開き、
  draft + `meguri:needs-human` の組で区別され、`self_review.escalated_draft` を emit する。

この分離は spec より長生きするので ADR 0020 に置いた。`compose_pr_body` /
`post_self_review_status` が前提にしている「published PR は必ず self-review clean」という
不変条件は **通常 delivery 側でのみ真**であり、証拠 draft はその経路を通らない(別 composer・
別ステータス無し)ことでこの不変条件を破らない。

## アーキテクチャ影響

- 変更は self-review のエスカレート境界と flow のヘルパに閉じる。状態機械・ステップ・
  checkpoint スキーマは無変更(draft の URL/番号を checkpoint に持つ必要はない — run は
  Failed で終わり、この情報を再利用する後続ステップが無い)。
- `pr_is_touchable` は無変更で機能する。needs-human ラベルで既に fixer 系から除外される。
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
- push 失敗・PR 作成失敗は個別に emit(例: `self_review.draft_push_failed` /
  `self_review.draft_failed`)し、best-effort が黙って握り潰さないようにする。
- `pr.created` は通常 delivery でのみ発火し続ける(不変)。

## migration & rollback

- 永続状態・DB スキーマの変更なし。
- 追加は純粋に上乗せ。fallback を無効化すれば従来のコメントのみ挙動に戻る(rollback は
  ヘルパ呼び出しを外すだけ)。
- forge 側に生えた証拠 draft は、人間が PR を閉じれば消える。マージは draft ゲートで
  ブロックされるため、誤って本流に入ることはない。

## テスト計画

FakeForge は `create_pr`(draft フラグ・labels を記録)/ `add_pr_label` を既に持つので、
実 forge 無しで検証できる。

- **単体(self_review.rs)**: エスカレート時、`commits_ahead > 0` なら FakeForge に
  draft=true・`meguri:needs-human` ラベル付きの PR が1件記録され、`self_review.escalated_draft`
  が emit され、run は Failed のままであること。`commits_ahead == 0` なら PR が作られない
  こと。push 失敗時にコメントのみへ落ちること。
- **単体**: 通常 delivery 側(clean 収束→ `open_pr`)が `pr.created` を emit し、draft でない
  PR を開く挙動が無変更であること。
- **統合(tests/)**: 既存の虚偽申告・validation feedback 系テストと同じ土台
  (`fake_agent.sh` + 実 tmux/git)で、未収束エスカレート run が実 origin に needs-human draft
  を残すことを1本追加(planner か worker のどちらか一方で代表)。

## 触るファイル

- `src/engine/self_review.rs` — エスカレート境界で `NeedsHuman` を捕まえ fallback publish を呼ぶ。
- `src/engine/flow.rs` — `publish_needs_human_draft` ヘルパ + 証拠物件用の PR body composer。
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
5. push / PR 作成 / ラベル付与が失敗しても run は落ちず、コメントのみへ best-effort で落ちる。
6. happy path(clean 収束)は無変更 — 通常 PR は非 draft で `pr.created` を emit する。
7. worker と planner の両方でこの fallback が効く(flow 共通)。
8. 既存テストが全て通る(特に self-review 収束系・escalation 系の非破壊)。

## スコープ外

- エスカレートコメントへ draft URL を差し込む連携(`escalate_task` は URL を取らない。
  やるなら別 issue)。
- 証拠 draft のライフサイクル自動管理(放棄 draft の自動クローズ等)。人間が閉じる前提。
- autonomy モード別の挙動分岐。この fallback はモード非依存(ADR 0012 の方針を踏襲)。
