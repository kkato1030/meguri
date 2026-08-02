# ADR ステータス台帳

ミニマム核への刈り込み(docs/design/kernel-pruning-plan.md)に伴う全 ADR の分類。

- **kernel** — 削減後も生きる決定。核の設計を拘束し続ける
- **dormant** — 対応する機構をコードから削除した決定。**失敗モード・ライブラリ**として
  保存する。再導入条件: その ADR の動機となった失敗がミニマム構成で再観測されること
  (再成長規律 1)。再導入時は既存 ADR を土台に再設計し、削除条件を宣言した新 ADR を書く

ADR 本文は編集しない。分類の正はこのファイル。

| ADR | 分類 | 備考 |
|---|---|---|
| 0001-daemon-single-binary-os-supervisor | dormant | daemon 廃止、foreground watch のみ |
| 0001-scheduler-priority-wip-first | kernel | redispatch(中断再開)を新規 discover より先に行う原則は核に残る |
| 0001-specs-are-disposable-scaffolding | dormant | plan/spec パイプライン休眠 |
| 0002-serve-reads-sqlite-no-ipc | kernel | serve 自体は廃止済み。「CLI は IPC なしで sqlite 直読」の原則は ps/doctor に継承 |
| 0003-auto-merge-github-native-arm-only | dormant | auto-merge 廃止。マージは人間、検出は git protocol |
| 0003-cleaner-read-only-single-report-issue | dormant | |
| 0003-role-based-agent-routing | dormant | routing 廃止。profile は default + 名前付き + project override のみ |
| 0003-tasksource-task-moves-run-pins | kernel | TaskSource seam は権威反転の土台。LocalTaskSource が全モードの権威に |
| 0004-ai-review-covers-implementation-diffs | dormant | レビュー機構休眠 |
| 0004-automerge-gate-renovate-side-on-free-private | dormant | |
| 0004-issue-lane-pane-session-lifetime | kernel | 1 task = 1 pane(author lane)。review lane は休眠 |
| 0005-issue-labels-two-axis-phase-and-ball | dormant | 権威反転によりラベルは投影に降格。二軸モデルは解消 |
| 0005-meguri-top-dedicated-workspace-attach | dormant | top 廃止 |
| 0005-per-project-mux-workspace | kernel | |
| 0006-ai-implementation-review-is-an-internal-loop | dormant | |
| 0006-capture-first-issue-intake | dormant | refine 廃止。原文 verbatim 保存の原則のみ `add` に残る |
| 0006-collab-advisor-role-reembodiment | dormant | |
| 0006-triage-read-only-report-separate-from-cleaner | dormant | |
| 0007-merge-watch-defers-to-fixer-loops-and-backstops-drift | dormant | fixer 一族休眠 |
| 0007-routing-freshness-and-outcome-drift | dormant | |
| 0007-tag-driven-self-owned-release-workflow | kernel | リリース手順は刈り込みと直交 |
| 0008-agent-instructions-via-apm | dormant | |
| 0008-symmetric-plan-impl-review-loop | dormant | |
| 0009-agent-skill-distribution-symptom-trigger-honest-pitch | dormant | |
| 0009-auto-merge-orchestrator-side-merge-on-free-private | dormant | |
| 0009-body-edit-is-a-reattention-signal | dormant | |
| 0009-cross-repo-via-static-workspace-declaration | dormant | workspace 宣言廃止 |
| 0009-parked-review-awaiting-human-signal | dormant | |
| 0009-schedules-enqueue-only-not-a-cron-replacement | dormant | |
| 0010-adaptive-spec-depth | dormant | |
| 0011-combined-impl-diff-self-review | dormant | |
| 0011-discovery-throttles-not-before-and-cadence | dormant | |
| 0011-routing-role-6-kinds-of-work-independent-of-loop-kind | dormant | |
| 0011-two-layer-config-repo-meguri-toml-pinned-at-run-start | dormant | check_command 等は host config へ戻す |
| 0012-acquisition-skill-as-apm-subpath-github-ref | dormant | |
| 0012-aggregate-escalation-needs-human-two-layer-autonomy | dormant | escalation は「needs_human 化 + 投影コメント」に縮小 |
| 0012-launch-mode-role-pane-or-direct-keep-pane-subordinate | dormant | pane のみに。direct は休眠 |
| 0012-loops-are-emergent-level-triggered-reconciler | kernel | アーキテクチャの核。純粋 decider + workqueue は維持 |
| 0012-role-preamble-injected-into-turn-prompt | kernel | 縮小形(単一 preamble)で維持 |
| 0013-profile-escalation-and-explore-canary | dormant | |
| 0013-spec-fixer-drives-plan-review-findings | dormant | |
| 0014-plan-review-findings-defer-to-spec-fixer-not-escalation | dormant | |
| 0015-repo-side-reads-advisory-from-default-branch-pinned-from-worktree | dormant | |
| 0015-triage-advise-proposal-labels-and-idempotency | dormant | |
| 0016-decompose-through-spec-review-gate-then-materialize | dormant | |
| 0016-operator-surface-run-why-attach | kernel | run / attach は維持。why は削除(再導入候補) |
| 0017-collab-effect-measured-only-by-orchestration-plane-signals | dormant | |
| 0017-triage-auto-promotes-real-labels-guarded | dormant | |
| 0018-managed-clone-derives-repo-path-from-slug | dormant | repo_path 明示のみに |
| 0019-add-project-onboarding-command | dormant | |
| 0020-notify-sink-event-driven-best-effort | dormant | 無人運転を再開する際の再導入第一候補 |
| 0020-self-review-measured-from-orchestration-events-union-merge | dormant | |
| 0021-escalate-time-needs-human-draft-pr-as-evidence | dormant | |
| 0022-self-review-findings-ledger-and-behavioral-escalation | dormant | |
| 0023-self-review-round1-parallel-reviewers | dormant | |
| 0024-external-reviewer-findings-are-a-prompt-injection-surface | kernel | 原則は intake にも適用: forge 由来テキストをプロンプトに入れる全経路が injection 面 |
| 0025-guard-is-a-safety-tripwire-advisory-does-not-block | dormant | |
| 0026-review-effectiveness-measured-by-cost-times-catch | dormant | cost×catch の考え方は再成長規律 3(削除条件)に一般化して継承 |
| 0026-schedules-are-repo-eligible-read-from-default-branch | dormant | |
| 0026-signal-binding-and-step-policy | dormant | |
| 0027-claim-identity-no-steal | kernel | claim の意味論は LocalTaskSource に残る |
| 0027-profile-preflight-primes-first-run-gate | kernel | preflight は核に残す(証拠ゲート充足済み) |
| 0028-infra-command-failures-are-not-needs-human | kernel | インフラ障害の分類は縮小 escalation に残る |
| 0029-resume-only-a-conversable-session | kernel | agent_session / resume 機構は核 |
