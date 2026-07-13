# issue-142 spec — reconcile loop が issue 本文の編集を検知して再着手できるようにする

## 一行で

一度処理した issue の「処理済み判定」を **ラベルではなく本文のダイジェスト**に対して行うようにし、本文が実質的に変わったら reconcile loop がそれを検知して(1)恒久サプレッションを解除し、(2)`再着手を促すシグナル`を残す。**実行の権限ゲートはラベルのまま**にして、prompt-injection とラベル真実源モデルを一切緩めない。

## 背景(現状の確認)

- `LabelTaskSource::discover`(`src/tasks.rs:180`)は `issue_has_succeeded_run`(`src/store/runs.rs:325`)で「一度 succeeded した (project, loop, issue) は永久にサプレッション」している。以後どれだけ本文を編集しても再ディスカバリされず、`meguri run --issue N` で明示バイパスするしかない(`src/app.rs:89` の `cmd_run` は guard を通らない)。spec-worker も同じ guard を使う(`src/engine/spec_worker.rs:79`)。
- forge 層は `updatedAt` を取得していない(`src/forge/gh.rs:380` は `number,title,body,labels`)。ただし **本文(`body`)は既にディスカバリ時点で手元にある** — `list_issues_with_label` が返している(`src/forge/gh.rs:421`)。store に body スナップショットも last_seen も無い。
- 既存の前例: fixer は review スレッド由来で PR を何度でも再処理する(`src/engine/fixer.rs:51` `thread_awaits_fixer`、succeeded-run guard 無し、収束はマーカー返信)。コメント由来の再着手のパターンは既にある。
- ラベルの真実源モデルと prompt-injection 対策(`README.md:53` の label gate: 「誰が agent を起動できるか = 誰が write 権限を持つか」)、2軸ラベル(ADR 0005)を壊してはならない。

## 鍵となる決定(詳細は ADR 0008)

1. **`updatedAt` は使わず、本文の内容ダイジェストを直接比較する。** GitHub の `updatedAt` はラベル付け替えでも動くため「本文が実体として変わったか」を見分けられない(issue の論点そのもの)。本文をディスカバリ時点で既に持っているので、正規化した本文の SHA-256 を比較すれば、ラベル churn は原理的に無視できる(受け入れ基準 1 を直接満たす)。追加の API 呼び出しも不要。

2. **本文編集は「実行トリガー」ではなく「再着手シグナル」。実行の権限ゲートはラベルのまま。** 本文編集を実行トリガーにすると label gate(collaborator 権限)で守っていた起動権限モデルが緩む。よって本文編集そのもので agent を起動しない。reconcile loop は本文変更を検知して(a)サプレッションを解除し、(b)イベント + 一度きりのコメントで人間に「再実装が要るなら `meguri:ready` を付け直して」と促すに留める。実際に worker が走るのは **collaborator がトリガーラベルを付け直したとき**だけ。「誰が起動できるか = 誰が write 権限を持つか」は完全に維持される。

3. **サプレッションは succeeded run に紐づく本文ダイジェストに対して行う。** `issue_has_succeeded_run`(処理済みなら常に抑止)を、`issue_processed_current_body`(処理済み **かつ** 現在の本文ダイジェストが処理時のものと一致するときだけ抑止)に置き換える。既存の succeeded run(ダイジェスト NULL)は「常に一致」= 従来どおり恒久抑止として扱い、アップグレード時の暴発を防ぐ(本文ダイジェストを記録するのは本 issue 以降の新しい success だけ)。

4. **振動防止**: (a) 比較前に本文を正規化(前後空白の trim + 連続空白の畳み込み + 改行の LF 統一)して whitespace-only / reflow の微小編集を無視する。(b) 再処理が succeeded したら新しいダイジェストを記録するので、同一内容の再ポーリングで再発火しない。(c) シグナルコメントは「signaled したダイジェスト」で dedup し、同じ新本文に対して一度きり。編集距離ベースの typo 閾値は過剰なのでスコープ外(正規化 + label gate + `implementing` では `ready` が無いので自動起動しない、で十分バウンドされる)。

## メカニズム(2 つの半身)

### 半身 A — サプレッションを本文アウェアにする(コア)

label/state ベースのループのディスカバリ guard を本文ダイジェスト比較に差し替える。トリガーラベルが付いている issue が対象なので、`ready` を(人間が)付け直したときだけ再着手が起きる。

- **記録**: `claim_task`(`src/engine/flow.rs:506`)で `cp.issue_body` から正規化ダイジェストを計算し、run に保存(`runs.body_digest`)。claim 時点で書くので、その run が処理した本文が一意に残る。
- **判定**: `LabelTaskSource::discover`(`src/tasks.rs:180`)と `SpecWorkerLoop::discover`(`src/engine/spec_worker.rs:79`)の `issue_has_succeeded_run(...)` 呼び出しを `store.issue_processed_current_body(project, loop, issue, current_digest)` に置換。抑止を解除する(= 本文が変わっていた)と判明したら `issue.body_changed` イベントを emit する(受け入れ基準 3 の traceability)。

### 半身 B — 能動的検知シグナル(reconcile sweep)

success 後の issue はラベルが `ready`→`implementing` に差し替わる(ADR 0005)ため、`list_issues_with_label(ready)` からは見えない。そこで reaper / auto-merger と同様に **poll に相乗り**する軽量 sweep を新設し、フェーズラベルを持つ「meguri 関与済み」issue の本文変更を能動的に検知する。

- `src/engine/reconcile.rs` を新設し、`scheduler.rs` の poll ループ(`src/engine/scheduler.rs:96-112` の sweep 群)に追加。
- フェーズラベル(特に `implementing`)を持ち succeeded run がある issue について、現在の正規化ダイジェスト ≠ 直近 succeeded run の `body_digest` なら:
  - `issue.body_changed` を emit、
  - 一度きりのシグナルコメントを投稿(「本文が更新されました。再実装が必要なら `meguri:ready` を付け直してください」)。
- **dedup**: 小さな `issue_reconcile(project_id, issue_number, signaled_digest, signaled_at)` テーブルに signaled 済みダイジェストを upsert し、同じ新本文に対しては再コメントしない(振動防止 c)。
- キルスイッチで無効化可能(下記 config)。

> 半身 A だけでも「必要なら再処理できる / ラベルと併存 / 真実源を壊さない」(基準 1・2・4)は満たせる。半身 B は「reconcile loop が **検知して** シグナルを残す」(issue タイトルと基準 3 の能動側)を満たす部分。レビューで重すぎると判断されれば、B は同一ブランチの後段フェーズに回せる(A が土台になる)。

## config

`ReviewConfig` / `CleanConfig`(`src/config.rs`)の小 struct 前例に倣う:

```toml
[reconcile]
body_edits = true       # キルスイッチ(false で半身 A・B とも無効 = 従来の恒久サプレッション)
signal_comment = true   # 半身 B のシグナルコメント投稿(false ならイベントのみ)
```

`watch` の毎 tick 再読込(#73)に自動で乗る。per-project override はスコープ外。

## 触るファイル

- `src/store/migrations/0008_reconcile.sql`(新規)— `runs` に `body_digest TEXT`(nullable、`ALTER TABLE ADD COLUMN`)を追加 + `issue_reconcile` テーブル新設。
- `src/store/runs.rs` — `set_run_body_digest` / `issue_processed_current_body` / 直近 succeeded run のダイジェスト取得を追加。`RunRecord` に `body_digest` フィールド。
- `src/store/mod.rs`(または新 `src/store/reconcile.rs`)— `issue_reconcile` の upsert / read。
- `src/tasks.rs` — 本文正規化ダイジェスト helper + `LabelTaskSource::discover` の guard 差し替え。
- `src/engine/flow.rs` — `claim_task` で `body_digest` を記録。
- `src/engine/spec_worker.rs` — discover の guard 差し替え。
- `src/engine/reconcile.rs`(新規)— sweep 本体。
- `src/engine/scheduler.rs` — poll ループに sweep を追加。
- `src/config.rs` — `[reconcile]` セクション。
- `src/forge/fake.rs` — 本文編集をテストで再現できるよう `set_issue_body` 等の補助(必要なら)。
- `README.md` / `README.ja.md` — 本文編集はシグナルであってトリガーではないこと、`[reconcile]` を追記。
- `docs/adr/0008-body-edit-is-a-reattention-signal.md`(本 PR 同梱)。
- `tests/reconcile_test.rs`(新規)ほか、既存 `tasks` / `scheduler` テストの非破壊。

## 受け入れ基準(issue のたたき台への対応)

1. **本文の実質更新をラベル付け替えと区別できる。** 正規化本文ダイジェストの比較で判定する(`updatedAt` を使わないのでラベル churn は原理的に無視)。テスト: 同一 issue にラベルを付け外ししてもダイジェスト不変 → 抑止継続;本文を変えると抑止解除。
2. **振動しない。** 同一内容の再ポーリングで再発火しない(処理後に新ダイジェストを記録);whitespace-only 編集は正規化で無視;シグナルコメントは新本文ごとに一度きり(dedup テーブル)。
3. **検知 → 再着手の遷移が durable かつ追跡可能。** `issue.body_changed` イベントを emit し(`meguri logs` / events から追える)、再処理の run は通常どおり `run.discovered` 以降を記録する。
4. **既存のラベル trigger 経路と併存し、真実源モデルを壊さない。** 本文編集で agent は起動しない;実行は依然としてトリガーラベル(collaborator 権限)が必要;サプレッションのダイジェスト化はローカルの冪等性補助であって、既存の `issue_has_succeeded_run` と同じくラベルをミラーしない。

## スコープ外(別 issue に切り出す)

- **既存コメントの編集 / issue コメント・PR 会話コメントの自然文 trigger**(issue の検討事項でも別出し可とされている)。本 spec は **issue 本文(description)** に限定。
- **PR description の編集**。本文ダイジェスト機構はそのまま流用できるが、対象を issue 本文に絞って spec を小さく保つ。spec-worker の guard 差し替え(半身 A)で PR ↔ issue の橋渡しの一部はカバーされるが、PR body そのものの編集検知は別 issue。
- **編集距離ベースの typo 閾値 / 編集者ごとの信頼度判定**。label gate が権限を担うので不要。
- **本文編集での自動起動(signal ではなく auto)**。将来 `body_edits = "auto"` として opt-in できる設計余地は残すが、デフォルトは signal-only。

## テスト計画

- `tasks` 単体: ラベル churn でダイジェスト不変 → 抑止継続、本文変更 → 抑止解除 + `issue.body_changed` emit。正規化(whitespace-only 無視)。NULL ダイジェスト(レガシー run)は従来どおり抑止。
- `reconcile` sweep 単体(FakeForge): `implementing` issue の本文を変更 → イベント + シグナルコメント一度きり、再 sweep で追加コメント無し。`signal_comment = false` でコメント無し・イベントのみ。`body_edits = false` で完全無効。
- 非破壊: 既存 `tasks` / `scheduler` / `spec_worker` テストがそのまま通る(NULL ダイジェスト互換)。
