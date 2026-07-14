# ADR 0014: pr_reviewer(plan) の findings は escalate せず spec_fixer に委譲する — ADR 0012 と ADR 0013 の統合

- Status: accepted
- Date: 2026-07-14
- Issue: #192
- 関連: ADR 0012(escalation 集約・guard findings は一律 needs-human)・ADR 0013(spec_fixer が
  plan レビュー findings を駆動)・ADR 0007(merge_watch は fixer 系に委譲して no-op する)

## Context

同じ日に入った ADR 0012(#176)と ADR 0013(#188)が、実装として衝突していた。

- ADR 0012: guard(pr_reviewer)の findings は plan/impl とも一律 `needs-human` を貼る。
- ADR 0013: plan レビューの findings は `spec_fixer` が拾って自動修正する新しいループ。

`needs-human` は全ループ共通の untouchable ラベルなので、ADR 0012 の実装が先に
plan findings へ `needs-human` を貼ってしまうと、ADR 0013 の `spec_fixer` discover
(`needs-human` の付いた PR を skip する)が一度も発火しない。実測(2026-07-14)でも、
spec_fixer マージ直後の watch で spec PR が pr_reviewer(plan) findings の直後に
`needs-human` へ即転落し、spec_fixer が起動しないまま止まっていた。

## Decision

**plan 側の人間ゲートは `spec_fixer` が持つ。pr_reviewer(plan) の settle は findings でも
escalate しない。**

- **kind = Plan の findings**: `spec-reviewing` ラベルと `meguri/pr-review = failure`
  ステータスをそのまま残す(working の claim だけ外す)。escalate しない。次 poll で
  `spec_fixer` が discover し、修正 push → 再レビューのループに入る(ADR 0013)。
  ラウンド上限(`MAX_SPEC_FIX_RUNS` = 3)超過時の `needs-human` は `spec_fixer` 自身が貼る。
- **kind = Impl の findings**: ADR 0012 のまま変更しない。impl 側には自動修正ループが
  無いため、pr_reviewer の settle が引き続き `escalate_pr` で `needs-human` を貼る。

これは ADR 0007(merge_watch は fixer 系ループの守備範囲に手を出さず no-op する)と同じ
原理: 「直す主体を持つループがいるサイトでは、汎用ゲートが先回りして needs-human を
貼ってその主体を締め出してはいけない」。

### ADR 0012 との関係

ADR 0012 の「guard(plan) findings → escalate」の行は、本 ADR により
「guard(plan) findings → spec_fixer に委譲、escalate は spec_fixer 自身のラウンド上限
超過時のみ」に読み替える。ADR 0012 の他の決定(escalation の中央ヘルパ集約・impl 側の
escalate・self-review の3値化・autonomy モード)には影響しない。

## Consequences

- spec PR が pr_reviewer(plan) の一回目の findings で人間待ちに落ちなくなり、
  ADR 0013 が意図した自動修正ループが実際に発火する。
- plan 側で「人間に渡るタイミング」が変わる: 即時(pr_reviewer)から
  ラウンド上限超過時(spec_fixer)へ後ろ倒しになる。これは ADR 0013 が最初から意図していた
  挙動であり、退行ではない。
- impl 側の挙動(pr_reviewer findings → 即 escalate)は変わらない。
