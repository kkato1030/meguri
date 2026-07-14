# issue-134 spec — 分解を spec-review ゲートに通す(分解提案 spec → 承認 → materialization)

planner が「実装 spec」の代わりに **分解提案 spec** を書けるようにし、既存の spec-review ゲートで
承認された後、専用の軽量ステップが親子 issue + GitHub dependencies を materialize する。設計判断の
「なぜ」は同梱の **ADR 0012** に置いた。この spec は「何を・どこを触るか」に絞る。

## spec 深度: design(veto により migration & rollback 必須)

**理由**: materialization は GitHub issue + dependencies という**永続状態**を生む唯一の書き込みで
あり、途中失敗で**重複 issue を作ると取り返しがつかない**(起こした issue は自動で消せない)。
適応的 spec 深度(ADR 0010)の veto ルール — 永続状態・取り返しのつかない運用リスク — に該当する
ため、design tier とし、migration & rollback セクションを必須で書く。

## 全体フロー

```
大きな issue に meguri:plan
  → planner: 調査 → 「分解が要る」と in-context 判断
  → 分解提案 spec を docs/specs/issue-<N>.md に書く
      (散文: 親ゴール / 要求カバレッジ / 依存 graph / rollout 順 / 各子の完了条件)
      (機械可読ブロック: children[] = title/body/kind/blocked_by)
      (PR body にマーカー <!-- meguri:decompose-proposal --> )
  → spec PR(spec-reviewing → 既存 review ゲート → spec-ready)
  → materialization sweep: spec-ready かつマーカー付き PR を拾い
      → children ブロックを parse → 子 issue を作成 → blocked_by を wire
      → 各子に指定 phase ラベル(ready/plan、human は無ラベル)
      → 親: phase ラベルを剥がし全子に blocked_by(= tracking issue)
      → 冪等マーカーを親 body に追記(index → 子#)
      → 完了後 提案 PR を未マージで close
```

## 決定(論点への回答)

1. **materialization の実行主体 = 専用の軽量掃引**(spec-worker の終端動作にしない)。
   materialization は forge 純操作でコード/commit/worktree を生まないため、spec-worker の
   「takeover して実装を積む」モデルと重ならない。handoff / reaper と同じ watch poll 相乗りの
   掃引にし、combined / separate 両方で一様に効かせる。→ ADR 0012 §2。
2. **親の表現 = body チェックリスト + `blocked_by`**。GitHub sub-issues 機能は Forge トレイトに
   無く、導入はスコープ外。既存の親子 `blocked_by`(#24 が既に張っている)を流用する。
3. **親の phase ラベル = 剥がす(無ラベル tracking)**。2軸モデル(ADR 0005)どおり。
4. **冪等性 = 親 body の隠しマーカー**で作成済み子を記録し、再実行は続きから。→ ADR 0012 §3。
5. **子のデフォルト phase = 提案側で子ごとに指定**。`ChildIssue.kind`(ready/plan/human)を流用。

## 検出とルーティング(delivery mode に依存しない)

分解提案 spec PR を通常の実装 spec PR と区別する要が **PR body のマーカー**
`<!-- meguri:decompose-proposal -->`(planner が pr_body に書き込む)。`list_prs_with_label` が返す
`PullRequest.body` に既に載るので、各ループの discover は**ファイル読みなしで**判定できる。

- **materialization sweep(新規)**: `spec-ready` かつマーカー付き かつ head branch が issue を
  encode する **open** PR を拾う。skip 条件は**提案 PR が open でない(= closed/merged)**の一点
  だけ。materialization の唯一の commit point は「提案 PR を close する」ことなので、PR が open で
  ある限り毎掃引が全 materialization 手順を冪等に再実行する(下記 §2)。「子マーカーが全部揃った」を
  完了とはみなさない — それは「子作成済み」であって「親 tracking 化 + PR close 済み」ではない。
- **spec-worker discover**: マーカー付き PR を**除外**(実装 takeover しない)。
- **handoff sweep**: マーカー付き PR を**除外**(`speccing → ready` 張替をしない)。
- **guard(Plan) / reviewer**: 無変更。分解提案も通常 spec と同じく `spec-reviewing → spec-ready` を
  張り替えるだけ。カバレッジのレビューは spec 本文を読む prompt/内容の問題で、機構は変えない。

## 触るファイル

### 1. `src/engine/planner.rs` — 分解を提案 spec 経路に一本化
- execute prompt の「Too big for one spec?」節を書き換える: 即時 `status: decompose` を誘わず、
  **分解提案 spec を書く**よう指示する。必須の内容(親ゴール / 要求カバレッジ表 / 依存 graph /
  rollout 順 / 各子の完了条件)と、機械可読な children ブロックの形、PR body マーカーを明記。
- 分解提案 spec も disposable scaffolding である旨(materialization 後に破棄、default branch に
  残さない)を書く。1レベルのみ(decomposition child では分解提案も禁止)は既存 `is_decomposed_child`
  分岐を流用。
- `on_decompose` の子 filing 中核(children 検証・issue 作成・`blocked_by` wire・親コメント・
  親ラベル剥がし)を **materialization から共有できる関数に切り出す**(例: `materialize_children`)。
  即時 `TurnStatus::Decompose` 経路は planner prompt から外す(retire)。filing ロジック本体・
  `validate_children`・`decompose_child_footer` は残して再利用する。

### 2. `src/engine/decompose_materializer.rs`(新規)— materialization sweep
- handoff.rs と同型: watch poll で回る軽量掃引、run record / pane 無し、**全体が再入可能で冪等**。
- discover: 上記の検出条件。drive: 提案 spec を head branch から読み(`gitops` の
  `git show origin/<branch>:docs/specs/issue-<N>.md`)、children ブロックを parse・検証
  (`validate_children` 再利用)、下記の冪等シーケンスを index 順に走らせる。
- **冪等の要 = 子の body に埋め込む安定 materialization key**。子1件ごとに、body に
  `<!-- meguri:decompose-child parent=<N> idx=<i> -->` を入れて作成する。key は `create_issue` に
  渡す body の一部なので、**「作成」と「key 付きで発見可能」が単一 API 呼び出しで同時に起きる** —
  作成と記録の間にクラッシュ窓が無い。各 index の手順:
  1. **adopt-or-create**: まず親 body の台帳(下記)にこの idx があれば、その子 # を採用。無ければ
     forge を key で検索(新 API `find_issue_by_marker`、下記 §7)。見つかれば採用し台帳へ backfill。
     どちらにも無ければ真に未作成なので `create_issue`(key 入り body)で作る。→ 作成後に落ちて
     台帳追記に失敗しても、次掃引の検索が同じ子を拾い**二重作成しない**(受入 4)。
  2. **台帳追記**: 親 body へ隠し行 `<!-- meguri:decompose-ledger idx=<i> issue=<#> -->` を冪等追記
     (既にあれば no-op)。台帳は検索の高速パスであって正しさの根拠ではない — 正しさは子 body の
     key が担う。
  3. **wire**: `blocked_by`(子→ブロッカー、親→子)とラベル付けを張る。いずれも冪等 API。
- **finalize(毎掃引で再実行、PR が open の限り)**: 全 index を処理したら親を tracking 化
  (phase ラベル剥がし・全子に `blocked_by`・チェックリスト body。すべて冪等)。最後に **提案 PR を
  未マージ close**(新 API `close_pr`、§7)。**この close が唯一の commit point** — 成功して初めて
  discover が対象外になる(state != open)。finalize の途中(親更新の後 close の前)で落ちても、
  次掃引が §1–3 と finalize を丸ごとやり直す(全部冪等なので安全)。
- kill-switch(config、後述)が off なら discover は空。

### 3. `src/gitops.rs` — ブランチ上のファイル読み
- `show_file_at_ref(branch, path) -> Result<String>`(内部 `git show <ref>:<path>`)。git 操作は
  gitops に集約する規約に従う。materialization sweep は fetch 済みの `origin/<branch>` を読む。

### 4. `src/engine/mod.rs` — `default_loops()` に materialization sweep を挿入
- ADR 0001 の逆順(merge に近い順)で **SpecWorkerLoop の前後**に置く。materialization 対象の
  残工程は「子を起こす」だけで実装より短いが、spec-ready を消費する点で spec-worker と同順帯。
  spec-worker より前に置き、マーカー付き PR を materialization が先に掴む(spec-worker は
  マーカーで除外するので競合しないが、順序でも保険を掛ける)。

### 5. `src/engine/spec_worker.rs` / `src/engine/handoff.rs` — 分解提案の除外
- spec_worker `discover`: PR body にマーカーがあれば skip。
- handoff `process_issue`(または sweep): 対応 spec PR がマーカー付きなら skip。

### 6. `src/config.rs` — kill-switch
- materialization の有効/無効を1つ(例 `[decompose] materialize_enabled = true`、既存
  `CleanConfig` / `[review]` の前例に倣う小 struct)。watch の毎 tick 再読込に乗るので運転中に
  止められる。rollback の operational lever(後述)。

### 7. `src/forge/mod.rs` / `src/forge/gh.rs` / `src/forge/fake.rs` — Forge API 2 本追加
既存の `create_issue` / `add_blocked_by_in` / `update_issue_body` / ラベル API は揃っているが、
以下の 2 メソッドが無い(現 trait を確認済み)。新しい振る舞いはまず trait に足す規約に従う。
- **`async fn close_pr(&self, pr: i64) -> Result<()>`**: PR を**未マージで close**(受入 7 の
  commit point)。gh 実装は `gh pr close <n>`。fake 実装は PR ストアの `state` を `"closed"` に。
  現 trait に close 手段は皆無なので、これが無いと実装者は提案 PR を畳めない。
- **`async fn find_issue_by_marker(&self, marker: &str) -> Result<Option<i64>>`**: body に
  `marker` を含む open issue の番号(冪等の adopt-or-create 検索)。gh 実装は
  `gh search issues "<marker>" --json number`(repo 限定)。fake 実装は issue ストアの body 走査。
- fake の PR/issue ストアはマーカー付き body を保持できる(`PullRequest.body` は既にある)。

### 8. `README.md` / `README.ja.md`
- spec-first flow の節に「分解提案 spec → 承認 → materialization」経路を1段落。即時 decompose の
  記述(あれば)を承認ゲート付きに更新。Decompose scope(ADR 0009)の記述は据え置き。

## architecture impact

- 新ループ1本(materialization sweep)を forge 純掃引として足す。既存の scheduler / run / pane
  モデルには乗らない(handoff / reaper と同じ poll 相乗り)。
- planner の分解出力型が「即時 filing」から「reviewable spec」へ移る。filing ロジックは共有関数に
  切り出して再利用するので、子起こしの挙動(footer マーカー・cross-repo scope・1レベル制限)は
  不変。
- delivery mode(combined / separate)に**分解経路を依存させない**。マーカーで分岐するので、
  spec-worker(combined 専用)にも handoff(separate 専用)にも materialization を寄生させない。

## alternatives considered & 決定

- **A. materialization を spec-worker の終端動作にする**(論点 1 の対案)。spec-worker は
  spec-ready PR を既に discover し worktree を attach するので、そこで「実装せず子を起こす」分岐を
  足す案。→ **却下**。spec-worker は combined 専用で separate では動かず、分解経路が delivery mode に
  縛られる。かつ commit を積まない forge 純操作を「実装ループ」に寄生させると、self-review /
  PR morph / diff 検証など spec-worker の全段が特例だらけになる。専用掃引の方が薄い。
- **B. 構造化 children を spec ではなく別の場所(store / 親 body の隠し payload)に永続し、承認後に
  replay する**。→ **却下**。レビュー対象(spec 散文)と実体化対象(payload)が2表現に分裂し、
  レビュー中の spec 編集が payload に反映されない divergence を生む。カバレッジのレビュー(受入 5)を
  実効化するには「レビューした children ブロック = 起こす children」を一致させる本 spec の方が強い。
- **C. GitHub sub-issues 機能で親子を表現する**(論点 2 の対案)。→ **却下**(Forge トレイトに無い、
  API 追加のスコープ増、blocked_by で受入基準は満たせる)。
- **D. 即時 `status: decompose`(#24)を残し、reviewed 経路と併存させる**。→ **却下**。分解機構が
  2つになり planner の判断が濁る。承認ゲートを付けるのが本 issue の目的なので、分解は一本化する
  (filing 中核は再利用)。

## migration & rollback(veto により必須)

- **migration**: 移行するデータは無い(新挙動。既存 issue / スキーマの変換なし)。store スキーマ
  変更なし(進捗は forge 側マーカーに置く)。config に kill-switch を1つ足すが既定 on で既存
  プロジェクトはそのまま。
- **冪等 = 前進的部分適用の安全策**: materialization は index 順に adopt-or-create で子を起こす。
  二重作成の防止は**子 body の安定 key**(`create_issue` と同じ API 呼び出しで書かれる)+ 作成前の
  key 検索で担保され、親台帳の追記が作成の後で落ちても再掃引の検索が拾う(作成⇄記録のクラッシュ窓が
  無い)。`blocked_by`・ラベル・親 tracking 化はいずれも冪等。**唯一の commit point は「提案 PR を
  close」**: close するまで sweep は同じ提案の全手順を毎回やり直す(前進のみ・二重作成なし)。
  finalize(親更新 → PR close)の途中で落ちても次掃引が丸ごと再実行する。
- **rollback**: materialization は取り返しのつかない forge 書き込み(issue 作成)を含むため、
  自動 un-create は無い。ロールバック手段は2段:
  1. **未実行分**: `materialize_enabled = false` で掃引を止める(毎 tick 再読込で即効)。承認済み
     提案は materialize されず、spec PR は spec-ready のまま人間判断待ちで残る。
  2. **実行済み分**: 起こした子 issue は通常の meguri issue として扱われる(ready/plan で回る)。
     取り消したいなら人間が子を close し親のラベルを戻す。planner の分解経路自体を戻すなら prompt
     変更を revert すれば即時 decompose に戻る(filing 中核は共有関数なので温存)。
- **1レベル制限**は既存不変条件を維持(decomposition child は分解提案も禁止 → needs-human)。
  これで「分解の分解」による無限増殖を塞ぐ。

## observability

- events(sqlite、`meguri logs`): `issue.decompose_proposed`(spec PR 開く)・
  `issue.materialized`(**提案 PR を close した commit point で** `{parent, children:[#…]}`)・
  `issue.materialize_resumed`(既存子を検索で採用した再開時)。既存 `issue.decomposed` の命名に倣う。
- 親 issue のコメント: 起こした子一覧 + 依存順の要約(#24 の親コメントを流用)。冪等の台帳/key
  マーカーは隠しコメント/隠し body 行なので会話を汚さない。
- 掃引失敗は既存 handoff と同じく `tracing::warn!` で1 issue 単位に握って続行。
- 提案 PR を close するとブランチは残るが、親は open な tracking issue のままなので、その worktree
  の回収は親が閉じるとき(人間 or 将来の自動 close)に reaper が行う — 既存 tracking 親と同じ扱い。

## test strategy

- **単体(loop 単位)**: FakeForge + マーカー付き PR で materialization sweep の discover フィルタ
  (spec-ready のみ / マーカー必須 / **skip は PR が open でない一点**)、children parse・検証、
  子作成・`blocked_by` 順序、親 tracking 化、`close_pr` を検証。
- **冪等(クラッシュ窓ごとに)**: FakeForge の記録で「**重複 issue が増えない**」を3ケースで確認
  (受入 4):
  1. **作成後・台帳追記前に crash**: 子 body に key だけある状態から再掃引 → `find_issue_by_marker`
     が既存子を採用し、`create_issue` を再発行しない。
  2. **一部の子だけ作成済み**: 残りだけ作られ、既存子はそのまま。
  3. **全子作成済み・親更新後・PR close 前に crash**: 再掃引が子を作らず finalize(親 tracking +
     `close_pr`)だけ完了する。PR close 後は discover が空(受入 7 の commit point)。
- **API 追加**: `close_pr` が PR state を closed にすること、`find_issue_by_marker` が body 一致で
  番号を返す/無ければ None を、FakeForge の単体で確認。
- **除外**: 同じマーカー付き PR で spec-worker / handoff の discover が空になること(実装 takeover /
  張替をしない)。
- **planner prompt**: 分解提案 spec を誘う文面(children ブロック / カバレッジ / PR body マーカー)を
  含み、decomposition child では誘わないことを既存 prompt テストの型で確認。
- **統合(`tests/*.rs`)**: 既存の疑似エージェント TUI + 実 git worktree + bare origin で、
  分解提案 spec を書く → spec-ready → materialization が実 forge fake 上で子を起こす、までを通す。
  受入 3(blocked された子を既存 discovery がスキップ)は既存 dependency gate テストの再利用で確認。
- CI と同じ並び(`cargo fmt --check` / `clippy -D warnings` / `nextest` / `--doc`)を通す。

## 受け入れ基準(acceptance criteria)

1. 分解が必要な issue に `meguri:plan` を貼ると、子候補・依存 graph・要求カバレッジを含む
   **分解提案 spec PR** が開く(即時に子を起こさない)。
2. spec 承認(spec-ready)後、materialization が子 issue 群を作成し、`blocked_by` を GitHub
   dependencies として張り、各子に指定 phase ラベル(ready/plan、human は無ラベル)を付ける。
3. blocked された子は既存 discovery がスキップし、ブロッカー完了後に自然に着手される(既存動作の
   確認 — README の dependency gate)。
4. materialization が途中で失敗しても、再掃引で**重複 issue を作らない**。作成⇄記録のクラッシュ窓を
   含め(子 body の安定 key + 作成前検索で adopt-or-create)、どの中断点から再開しても子は一意。
5. 親の要求が子のどれかでカバーされていることが、分解提案 spec 上でレビューできる(children ブロック
   + カバレッジ散文が spec に載る)。
6. 分解提案 spec PR は spec-worker の実装 takeover / handoff の `speccing→ready` 張替の対象に**ならない**。
7. materialization 完了後、親は phase ラベルの無い tracking issue になり、提案 spec PR は
   `close_pr` で**未マージ close**され(唯一の commit point)、`docs/specs/` は default branch に
   残らない。close が済むまで sweep は同じ提案を再処理する。
8. `materialize_enabled = false` で materialization sweep の discover が常に空(kill-switch)。
9. 既存テスト(planner / spec_worker / handoff / scheduler)が非破壊で通る。

## スコープ外(将来枠)

- tracking 親の**自動 close**(子が全部 close したら親を閉じる)。当面は人間が閉じる(#24 と同じ)。
  goal の括弧書きに対する意図的な保留 — 自動 close は reopened / not-planned 子の扱いなど別の設計面が
  あり、別 issue で扱う。
- GitHub sub-issues 機能の採用(親表現は body チェックリスト + `blocked_by`)。
- 複数 repository にまたがる分解の新スキーマ(起票スコープは ADR 0009 のまま)。
- 実行中 PR の自動分割。
- triage(#85/#87/#88)への `decompose` recommendation 語彙追加(将来の受け皿、本 issue 外)。
