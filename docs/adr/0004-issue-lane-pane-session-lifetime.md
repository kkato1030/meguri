# ADR 0004: issue が寿命の単位 — pane は (project, issue, lane)、resume の真実は panes.agent_session_id

## Status

Accepted(issue #92)

## Context

meguri は「1 issue = 1 pane = 1 live claude session、issue close まで維持」(#13)を核に設計されたが、実測で三つの綻びが見つかった:

1. `Target.issue_number` は loop によって GitHub issue 番号だったり **PR 番号**だったりする汎用フィールドで、pane の PK `(project_id, issue_number)` がこれをそのまま鍵にしていた。fixer / conflict_resolver / ci_fixer は同じ issue branch を編集するのに PR 番号で鍵るため、実装時とは別 pane・別 session に分裂し、「review が来たら実装した AI と対話して詰める」が構造的に成立しない。
2. resume が読むのは `runs.agent_session_id` のみで、これを埋める経路(agent 自己申告 / herdr 報告)はほぼ死んでいる。確実に効くファイル走査(`agent_session::latest_session_id(cwd)`)は reaper でしか呼ばれず、書き先も `panes.agent_session_id` で resume 側と繋がっていない。実測 59 run 中 capture 5 件・`--resume` 実行 0 件。
3. loop ごとの pane / session / worktree の寿命が暗黙で、どこで落ちたら何が残るか読み切れない。

## Decision

**issue 番号を全リソース(pane・claude session・worktree・run の dedup)の寿命の単位にする。**

1. **canonical issue** — PR を対象にする loop も `Target.issue_number` には必ず GitHub issue 番号を入れる。復元は branch 命名(`meguri/<issue>-…`)を第一経路、PR 本文の `Closes #N` を fallback とし、PR 番号は checkpoint に運んで forge 呼び出しにだけ使う。
2. **pane の鍵は `(project, issue, lane)`** — 厳密 1:1 をやめ、issue の下に役割(lane)を持たせる。
   - **author lane**: planner → worker / spec_worker → fixer / ci_fixer / conflict_resolver。同じ branch を編集する仕事は同一 pane・同一 session で文脈を継ぐ。
   - **review lane**: reviewer は read-only checkout の別 pane・別 session。独立視点は保ちつつ、鍵が issue 番号なので issue 経由で discover / attach / resume できる。
   - cleaner はレポート issue 専用の standalone(lane モデル外、現状維持)。
   - lane の一般化(3 つ以上)はしない。必要になったときに再訪する。
3. **resume の真実の置き場は `panes.agent_session_id`** — session id は run(ephemeral)ではなく issue 寿命の panes 行に置く。主経路は cwd のファイル走査(claude transcript は worktree が残る限り復元可能で、agent の自己申告に依存しない)で、turn 完了ごとに upsert する。resume spawn が即死したら id を落として fresh spawn に劣化する。

## Consequences

- 実装 → review 指摘対応 → conflict 解消が一本の会話として続き、pane が死んでも次に lane が触れた瞬間に `--resume` で同じ session に戻れる。「pane を残す」ことと「文脈を残す」ことが初めて一致する。
- pane / worktree / session の回収判定は issue の状態(forge が Authority)だけを見ればよく、loop ごとの特例が消える。
- `runs.issue_number` の意味変更により、PR 番号鍵だった旧 run のカウント(dedup・resolve 予算)は新しい鍵から見えなくなる。二重作業の防止は forge 上の marker(head-sha / thread reply)が担っているため許容する。
- 同一 issue 番号の author / review pane が並立するため、attach などの issue 起点操作は lane の指定を要する(既定は author)。
