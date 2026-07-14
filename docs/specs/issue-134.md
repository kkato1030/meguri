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
      (機械可読ブロック: children[] = title/body/kind/blocked_by/project(任意)。構文は下記
       「children ブロックの構文」)
      (PR body にマーカー <!-- meguri:decompose-proposal --> )
  → spec PR(spec-reviewing → 既存 review ゲート → spec-ready)
  → materialization sweep: spec-ready かつマーカー付き "open" PR を拾い
      → children を index 順に adopt-or-create(子 body の安定 key で既存子を照合、無ければ作成)
      → 各子に指定 phase ラベル(ready/plan、human は無ラベル)+ blocked_by を wire(冪等)
      → 親: phase ラベルを剥がし全子に blocked_by(= tracking issue)
      → 提案 PR を未マージ close(唯一の commit point。PR が open の限り毎掃引で全手順を再実行)
```

## 決定(論点への回答)

1. **materialization の実行主体 = 専用の軽量掃引**(spec-worker の終端動作にしない)。
   materialization は forge 純操作でコード/commit/worktree を生まないため、spec-worker の
   「takeover して実装を積む」モデルと重ならない。handoff / reaper と同じ watch poll 相乗りの
   掃引にし、combined / separate 両方で一様に効かせる。→ ADR 0012 §2。
2. **親の表現 = body チェックリスト + `blocked_by`**。GitHub sub-issues 機能は Forge トレイトに
   無く、導入はスコープ外。既存の親子 `blocked_by`(#24 が既に張っている)を流用する。
3. **親の phase ラベル = 剥がす(無ラベル tracking)**。2軸モデル(ADR 0005)どおり。
4. **冪等性の真実 = 子 body の安定 key + PR close(唯一の commit point)**。親の依存 graph
   (`blocked_by(parent)`、強整合・closed 子も含む)を「作成済み」の正典とし、親 body の台帳は
   検索の高速パスにすぎない。→ ADR 0012 §3。
5. **子のデフォルト phase = 提案側で子ごとに指定**。`ChildIssue.kind`(ready/plan/human)を流用。
6. **cross-repo 分解(#154 / ADR 0009)は不変**。`ChildIssue` には任意の `project`(workspace sibling
   への起票先、`src/turn/prompts.rs` の `ChildIssue.project`)が既にあり、children ブロックの schema・
   planner prompt・parse/validate のすべてで **optional `project` をそのまま運ぶ**。materialization も
   `resolve_child_target`(`src/engine/planner.rs`)を再利用して子ごとに sibling forge を解決するので、
   レビュー済み payload が既存の cross-repo 能力を落とさない。

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

## children ブロックの構文(厳密定義 — 「レビュー対象 = 実体化対象」の要)

分解提案 spec(`docs/specs/issue-<N>.md`)の中に、**info string が `json meguri-children` の
fenced code block をちょうど1個**置く:

````markdown
```json meguri-children
[
  {"title": "...", "body": "...", "kind": "ready", "blocked_by": []},
  {"title": "...", "body": "...", "kind": "plan",  "blocked_by": [0], "project": "sibling-id"}
]
```
````

- **形式 = JSON 配列**、要素は既存 `ChildIssue`(`src/turn/prompts.rs`)そのもの。turn result file と
  同じ serde 定義を流用するので、field schema は `title`(必須)/ `body`(省略可、default 空)/
  `kind`(必須: `ready` | `plan` | `human`)/ `blocked_by`(省略可: 先行 index の配列)/
  `project`(省略可: workspace sibling の project id、#154)。新しい parse 型を発明しない。
- **探し方**: spec Markdown の fenced block のうち info string に `meguri-children` を含むものを取る。
  一意な info string なので通常の ```json 例示ブロックと衝突しない。
- **ちょうど1個が正**: 0 個(ブロック無し)・2 個以上(どれが正か不定)・JSON parse 失敗・
  `validate_children` 失敗(不正 kind / 前方参照 / スコープ外 project)は、いずれも **materialize
  しない**。掃引は `tracing::warn!` で握って skip し、提案 PR に**冪等なエラーコメント**(隠しマーカー
  `<!-- meguri:decompose-error -->` 付きで、同一 head sha に対して1回だけ)を残す。issue は一切
  作らない(取り返しのつかない操作の前で止める)。
- **回復経路**: branch 上の spec を直して push すれば、次掃引が読み直して自己回復する(掃引は毎回
  branch の spec を読むだけで状態を持たない)。放置する判断なら人間が提案 PR を close すれば
  discover から外れる。

## 触るファイル

### 1. `src/engine/planner.rs` — 分解を提案 spec 経路に一本化
- execute prompt の「Too big for one spec?」節を書き換える: 即時 `status: decompose` を誘わず、
  **分解提案 spec を書く**よう指示する。必須の内容(親ゴール / 要求カバレッジ表 / 依存 graph /
  rollout 順 / 各子の完了条件)と、機械可読な children ブロックの構文(上記 fence + field schema)、
  PR body マーカーを明記。workspace 所属プロジェクトでは optional `project` の説明(sibling id 一覧)
  も出す — 既存 prompt の Cross-repo scope 節(`cross_repo_scope`、`src/engine/planner.rs`)を
  そのまま流用する。
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
  (構文は上記「children ブロックの構文」、検証は `validate_children` 再利用)、下記の冪等シーケンスを
  index 順に走らせる。子の起票先 forge は `resolve_child_target`(`src/engine/planner.rs`)の再利用で
  子ごとに解決する(`project` 指定の cross-repo 子も #154 どおり)。
- **冪等の要 = 子 body の安定 key + 親の依存 graph を正典にする**。子1件ごとに、body に
  `<!-- meguri:decompose-child parent=<parent_slug>#<N> idx=<i> -->` を入れて作成する(親は slug で
  修飾 — cross-repo 子でも一意。既存 `decompose_child_footer_ref` の親参照修飾と同じ理屈)。key は
  `create_issue` の body の一部なので「作成」と「key 付き」は不可分。
  「idx i は作成済みか?」の**正典は親の依存 graph** — `blocked_by(parent)` は依存関係の直リレーション
  (gh 実装は REST `repos/<slug>/issues/<N>/dependencies/blocked_by`、`src/forge/gh.rs`)で
  **強整合・作成直後でも読め・closed 子も含む**(既存 `Blocker` は `state` を持つので子が human/worker
  に閉じられていても関係は残る)。full-text search はインデックス遅延と open 限定の弱さがあるので**正典に
  しない**(下の backstop に限定)。
  **前提の型拡張(§7)**: 現行 `Blocker` は `number` / `state` / `state_reason` しか持たず
  (`src/forge/mod.rs`)、このままでは blocker の body を照合できない。そこで `Blocker` に
  `body: String` と `repo: String`(blocker の repo slug — cross-repo 子の同定に必要)を足す。
  gh 実装は上記 REST レスポンスが issue オブジェクト(`body` / `repository_url` 込み)を返すので
  **追加 API call なし**で field を読むだけ。各 index の手順:
  1. **既存判定**: `blocked_by(parent)` の各ブロッカーの `Blocker.body` に key `idx=i` があれば、
     その子(`Blocker.repo` + `Blocker.number`)を採用(all-state・強整合)。→ **finding: 子が close
     されていても関係で拾えるので再作成しない**。
  2. **未リンク検出(reservation-first で作成⇄リンクの窓を塞ぐ)**: graph に無いとき、
     - この idx が**未 reserve**(親 body に `<!-- meguri:decompose-reserve idx=<i> -->` が無い)なら
       真に新規。reserve を親 body に追記 → `create_issue`(key 入り body)→ 直後に
       `add_blocked_by(parent, child)` でリンク。ここで正典(graph)に載る。
     - この idx が**既 reserve だが graph に無い**なら、作成⇄リンクの窓で落ちた疑い。**再作成しない**。
       backstop の all-state key 検索(§7 `find_issue_by_marker`、`--state all`。repo 単位の API
       なので、その idx の起票先 forge — cross-repo 子なら sibling forge — で引く)で子を探し、あれば
       リンクして採用。無ければ**この掃引では作らず defer**(次掃引で再判定)。検索インデックス遅延を
       跨ぐため、`reserve` から一定掃引/経過を超えても all-state 検索で見つからないときだけ「作成は
       着地しなかった」と判断して再 create する(下限は検索遅延 ≪ 掃引間隔で安全側)。二重作成より
       bounded な遅延を選ぶ(取り返しのつかない操作なので)。
  3. **wire + 台帳**: 子→ブロッカーの `blocked_by`(cross-repo は既存 `add_blocked_by_in` が
     slug 込みで張れる)・ラベル・親 body の台帳行
     `<!-- meguri:decompose-ledger idx=<i> issue=<slug>#<N> -->`(slug 込みで cross-repo 子も一意)を
     張る。いずれも冪等(§7 で `add_blocked_by` の冪等契約を明記)。台帳は人間可読な高速パスで、
     正しさの根拠は graph + 子 key。
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

### 7. `src/forge/mod.rs` / `src/forge/gh.rs` / `src/forge/fake.rs` — Forge API(2 本追加 + 1 契約強化 + 1 型拡張)
既存の `create_issue` / `add_blocked_by_in` / `update_issue_body` / ラベル API は揃っているが、
以下が要る(現 trait を確認済み)。新しい振る舞いはまず trait に足す規約に従う。
- **`Blocker` に `body: String` と `repo: String` を追加**(型拡張): §2 の既存判定は blocker body の
  key 照合で成り立つが、現行 `Blocker` は `number` / `state` / `state_reason` のみ
  (`src/forge/mod.rs`)。gh 実装の `blocked_by` は REST
  `repos/<slug>/issues/<N>/dependencies/blocked_by` で issue オブジェクトを丸ごと受けているので、
  同じレスポンスから `body` と `repository_url`(→ slug)を読むだけ — **各 blocker への追加
  `get_issue` は不要**。fake 実装も issue ストアから body / slug を写す。field 欠落は空文字に
  degrade(key 照合に不一致 → 採用しないだけで、安全側)。既存利用者(dependency gate の
  `resolved()`)は追加 field を読まないので無影響。
- **`async fn close_pr(&self, pr: i64) -> Result<()>`**(新規): PR を**未マージで close**(受入 7 の
  commit point)。gh 実装は `gh pr close <n>`。fake 実装は PR ストアの `state` を `"closed"` に。
  現 trait に close 手段は皆無なので、これが無いと実装者は提案 PR を畳めない。
- **`async fn find_issue_by_marker(&self, marker: &str) -> Result<Option<i64>>`**(新規): body に
  `marker` を含む issue の番号(adopt-or-create の backstop)。**必ず all-state**で探す — 子が
  human/worker に close されていても拾えねばならない。gh 実装は
  `gh issue list --state all --search "<marker> in:body" --json number`(repo 限定)。**正典は親の
  依存 graph**(§2)で、これはインデックス遅延を伴う backstop 位置づけ。fake 実装は issue ストアを
  state 問わず body 走査。
- **`add_blocked_by` / `add_blocked_by_in` の冪等契約を明記・強化**: materialization は PR close まで
  毎掃引で wire を張り直すので、**既存 edge の再追加は成功(no-op)でなければならない**。現 trait は
  これを契約として書いていない。doc comment に「既存依存の再追加は冪等」と明記し、gh 実装は GitHub の
  「dependency already exists」エラーを成功に丸める(まず現 `blocked_by` を引いて既存なら skip、または
  重複エラーを握る)。fake 実装は同一 edge を重複追加しない。既存 #24 decompose にも安全側で効く。
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
  変更なし(進捗と完了判定は forge 側の状態 — 依存 graph と PR の open/closed — に置く)。config に
  kill-switch を1つ足すが既定 on で既存プロジェクトはそのまま。
- **冪等 = 前進的部分適用の安全策**: materialization は index 順に adopt-or-create で子を起こす。
  「作成済みか」の正典は**親の依存 graph**(`blocked_by(parent)`、強整合・closed 子も含む)で、
  子 body の安定 key を join に使う。作成⇄リンクの窓は **reserve-first**(作成前に親へ reserve を
  記す)と all-state 検索の backstop で塞ぎ、疑わしい時は**再作成せず defer**(二重作成 < bounded
  遅延)。`add_blocked_by`・ラベル・親 tracking 化はいずれも冪等(§7 で契約明記)。**唯一の commit
  point は「提案 PR を close」**: close するまで sweep は同じ提案の全手順を毎回やり直す(前進のみ・
  二重作成なし)。finalize(親更新 → PR close)の途中で落ちても次掃引が丸ごと再実行する。
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
  `issue.materialize_resumed`(依存 graph / all-state 検索で既存子を採用した再開時)・
  `issue.materialize_deferred`(reserve 済み未リンクの子を再作成せず次掃引へ送った時)。既存
  `issue.decomposed` の命名に倣う。
- 親 issue のコメント: 起こした子一覧 + 依存順の要約(#24 の親コメントを流用)。冪等の台帳/key
  マーカーは隠しコメント/隠し body 行なので会話を汚さない。
- 掃引失敗は既存 handoff と同じく `tracing::warn!` で1 issue 単位に握って続行。children ブロックの
  構文/検証エラーは提案 PR への冪等なエラーコメント(「children ブロックの構文」参照)で人間に見える。
- 提案 PR を close するとブランチは残るが、親は open な tracking issue のままなので、その worktree
  の回収は親が閉じるとき(人間 or 将来の自動 close)に reaper が行う — 既存 tracking 親と同じ扱い。

## test strategy

- **単体(loop 単位)**: FakeForge + マーカー付き PR で materialization sweep の discover フィルタ
  (spec-ready のみ / マーカー必須 / **skip は PR が open でない一点**)、children parse・検証、
  子作成・`blocked_by` 順序、親 tracking 化、`close_pr` を検証。
- **children ブロック構文(parse 単体)**: info string `json meguri-children` の fence をちょうど1個
  採ること。0 個・2 個以上・壊れた JSON・`validate_children` 失敗(不正 kind / 前方 index 参照 /
  スコープ外 `project`)の各異常系で **issue を1つも作らず**、冪等なエラーコメント1回 + skip に
  なること。optional `project` が parse で保持されること。通常の ```json 例示ブロックを誤検出
  しないこと。
- **cross-repo 子(#154 非退行)**: `project` 指定の子が sibling forge(FakeForge factory)に起き、
  親の graph 採用が `Blocker.repo` + `Blocker.number` で正しい repo の子を同定し再作成しないこと。
- **冪等(クラッシュ窓ごとに)**: FakeForge の記録で「**重複 issue が増えない**」を確認(受入 4):
  1. **作成+リンク済み・台帳前に crash**: `blocked_by(parent)` に子が載る状態から再掃引 → graph で
     採用し、`create_issue` を再発行しない。
  2. **作成済みだが未リンク(reserve だけ残る)で crash**: all-state 検索が拾ってリンク採用し、再作成
     しない。見つからない間は defer し、二重作成しないこと。
  3. **採用対象の子が close 済み**: `blocked_by(parent)` は closed ブロッカーも返すので graph で採用
     でき、close された子を再作成しないこと(**finding 直結**)。
  4. **一部の子だけ作成済み**: 残りだけ作られ、既存子はそのまま。
  5. **全子作成済み・親更新後・PR close 前に crash**: 再掃引が子を作らず finalize(親 tracking +
     `close_pr`)だけ完了する。PR close 後は discover が空(受入 7 の commit point)。
- **API 追加/契約**: FakeForge と(可能なら)GH パーサ単体で —
  - `blocked_by` が返す `Blocker` に `body` / `repo` が載る(gh はレスポンス JSON からの写経、fake は
    ストアから)。欠落時は空文字 degrade で採用に使われないこと。
  - `close_pr` が PR state を closed にする。
  - `find_issue_by_marker` が **all-state**で body 一致の番号を返す/無ければ None(open/closed 両方の
    子でヒットすること)。
  - **`add_blocked_by(_in)` の冪等**: 同一 edge を2回張っても成功し、graph に重複が出ないこと
    (Fake は重複無視、GH は「already exists」エラーを成功扱い)。materialization が毎掃引で張り直す
    前提を担保。
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
2. spec 承認(spec-ready)後、materialization が子 issue 群を作成し(`project` 指定の子は
   workspace sibling に — #154 の既存スコープどおり)、`blocked_by` を GitHub dependencies として
   張り、各子に指定 phase ラベル(ready/plan、human は無ラベル)を付ける。
3. blocked された子は既存 discovery がスキップし、ブロッカー完了後に自然に着手される(既存動作の
   確認 — README の dependency gate)。
4. materialization が途中で失敗しても、再掃引で**重複 issue を作らない**。作成⇄記録のクラッシュ窓を
   含め(子 body の安定 key + 作成前検索で adopt-or-create)、どの中断点から再開しても子は一意。
5. 親の要求が子のどれかでカバーされていることが、分解提案 spec 上でレビューできる(children ブロック
   + カバレッジ散文が spec に載る)。children ブロックは「children ブロックの構文」の一意な fence に
   従い、構文/検証エラー時は issue を作らずエラーコメント + skip になる。
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
