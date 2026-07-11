# ADR 0003: タスク調整レイヤーを TaskSource として Forge から分離する — task はホスト間を移動し、run はホストに pin される

- Status: accepted
- Date: 2026-07-12
- Issue: #54

(採番メモ: 0001 が 3 本並存しているが、0002 より後の空き番号として 0003 を使う)

## Context

meguri のワークフロー調整(キュー・排他・エスカレーション・完了判定)は GitHub
ラベルの上に実装されており、`Forge` トレイトが「タスクソース」「ラベル操作」
「PR 操作」「レビュースレッド」を一枚岩で兼ねている。ラベルを触れない/触りたく
ないリポジトリで meguri を使うローカル/サイレントモード(#54)と、将来の
リモート DB マルチホスト対応の両方が、この調整レイヤーの差し替えを要求する。

選択肢は (a) `LocalForge` で issue を偽装して Forge のまま通す、(b) 調整レイヤー
だけを `TaskSource` トレイトとして切り出す、(c) DB を丸ごとリモート化する。

## Decision

1. **調整レイヤーを `TaskSource`(discover / claim / release / escalate /
   complete)として切り出す。** 実装は `LabelTaskSource`(現行ラベル動作)と
   `LocalTaskSource`(sqlite `tasks` テーブル)の二枚。`Forge` には issue の
   読み取り・PR 操作・レビュースレッド・依存グラフが残る。(a) は PR やレビュー
   スレッドまで偽装することになり歪むため不採用。
2. **claim の契約は最初から非同期・アトミック・lease 前提で切る。**
   `claim(key, host) -> Option<Task>` が単一のアトミック操作(None = 良性の競合)。
   スキーマは `claimed_by` / `lease_until` を初日から持ち、単一ホストでは未使用の
   まま眠らせる。マルチホスト化は「lease 失効も claim 可能」という WHERE 句の
   拡張と lease 延長ハートビートの追加であって、契約の変更ではない。ラベル claim
   の構造的な穴(ホスト死亡で `meguri:working` が残留する)は lease 失効による
   自己回復で塞がる。
3. **語彙: task はホスト間を移動する調整単位、run はホストに pin された実行単位。**
   run は worktree・pane・agent セッションを持つ以上、生まれたホストを離れられ
   ない。ホストをまたいで引き継がれるのは task であり、その受け渡しはブランチ
   (`meguri/t<id>-*`)を介して行う。
4. **リモート DB は調整レイヤーの置き換えであって、Store の置き換えではない。**
   共有すべき状態は tasks(キュー・claim・エスカレーション)と run のサマリ
   (`ps --all` 用、ハートビートで反映)だけ。runs / turns / events の実体、
   pane、worktree は各ホストの sqlite に残る(ADR 0001/0002 の「状態は共有
   ストレージにある」路線の延長)。実装本命は Postgres(`SKIP LOCKED` +
   `LISTEN/NOTIFY`)。
5. **github モードでは tasks 行のミラーを作らない。** ラベルが唯一の真実であり
   続ける(Authority 原則)。tasks テーブルに載るのは local/silent 起源のタスク
   だけで、task の同一性は `TaskKey`(issue 番号 | tasks rowid)で表す。

## Consequences

- ローカル/サイレントモード(#54 Phase 1〜3)は `LocalTaskSource` の追加だけで
  成立し、既存のラベル運用は `LabelTaskSource` として無変更に保たれる。
- マルチホスト対応(Phase 4)は `TaskSource` 実装をもう一枚足す作業に閉じる。
  claim の契約・スキーマ・ブランチ命名は本 ADR 時点で固定済み。
- run がホストに pin されるため、リモート attach は「run サマリの attach ヒント
  から `ssh <host> -t tmux attach ...` を提示する」形になる(リモートで pane を
  操作する仕組みは持たない)。
- ラベルと tasks の二重状態を排した代償として、silent モードでは issue の状態
  (closed 等)とローカル task の状態がズレうる。完了判定側(reaper)が issue
  読み取りとローカル状態の両方を見て吸収する。
- Phase 3 までの local/silent モードは単一マシン前提(ローカル sqlite が唯一の
  真実)。複数マシンで同じリポジトリを回したければ Phase 4 を待つ。
