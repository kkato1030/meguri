# issue-42 spec — auto-merge (2/3): merge-watch(arm 済み PR のドリフト検出と回送)

issue は「#41 で arm したが GitHub 側で止まっている PR(conflict / red CI / protection 変更 / 人間による解除)を watch 掃引でポーリングし、分類して回送する」と言っている。**watch はドリフト検出であってマージ権威ではない**(ADR-0003 / looper ADR-0005)。

ところが調査すると、この issue が書かれた時点の前提が**もう一段進んでいる**。「conflict / red CI を needs-human で回送する(専用ループができるまでの当座しのぎ)」という設計の前提だった 2 つの専用ループが、この issue の起票後にすでに landed している。この spec の仕事は、機能を額面どおり実装することではなく「進化した現状の下で、issue の本来の目的 — arm 済み PR を誰にも気づかれず放置(#35 と同じ問題)しないこと — をどう非重複に達成するか」に収束させることだ。

## 調査で判明した前提(現在の main)

- **#41 auto-merger は landed**(`src/engine/auto_merger.rs`)。掃引で eligible PR に arm し、head ごとのマーカーコメント `<!-- meguri:automerge armed head=<sha> -->` を残す。このマーカーが冪等キー**兼**人間オーバーライドキー: 同一 head は二度 arm せず、**人間が auto-merge を無効化した head には再 arm しない**。push で head が動けば再評価。掃引は run/pane を持たない軽 API 掃引で、scheduler が reaper の直後に呼ぶ(`scheduler.rs`)。
- **#35 conflict-resolver は landed かつ CLOSED**(`src/engine/conflict_resolver.rs`、`default_loops()` に登録済み)。**arm の有無に関係なく** open な meguri PR で `mergeable == CONFLICTING` のものを discover し、ベースを取り込んで解消コミットを push する。push で head が動くので auto-merger が新 head に再 arm する。解消不能なら自分で `meguri:needs-human` にエスカレーションする。
- **ci-fixer は landed**(`src/engine/ci_fixer.rs`、`default_loops()` に登録済み)。open な meguri PR の check rollup が `FAILURE` のものを discover し、失敗ログを食わせて修正 push する。予算超過で自分で `meguri:needs-human` にする。
- **reaper**(`src/engine/reaper.rs`)は issue が close したら worktree/pane を回収する。arm 済み PR がマージ→ issue が `Closes #N` で close → reaper が回収、で完結する。
- `Forge::pr_mergeable` は `gh pr view --json mergeable,mergeStateStatus` を叩くが **`mergeStateStatus` を捨てている**(`gh.rs:661`)。auto-merge が有効かどうか(`autoMergeRequest`)を取る手段はまだ無い。

つまり issue の分類 5 種のうち **Conflict と RedCI は、当座しのぎだった needs-human 回送の「回送先」が既に常駐ループとして存在し、arm と無関係にそれらを拾っている**。ここで決定が要る。

## 決定: merge-watch の役割を「fixer ループの重複」ではなく「取りこぼしの backstop」に再定義する

進化した現状では、issue の分類をそのまま実装すると重複するばかりか、**危険な干渉を生む**:

> **critical:** Conflict / RedCI の PR に merge-watch が `meguri:needs-human` を貼ると、conflict-resolver も ci-fixer も **needs-human 付き PR を discover から除外する**ため、機械的に直せるはずのドリフトを**永久にデッドロックさせる**。当座しのぎのラベルは、回送先が存在する今や有害。

したがって merge-watch は次のように振る舞う(分類は純関数 `Classify`、副作用は掃引側):

| 分類(GitHub スナップショット) | 現状の担い手 | merge-watch の動作 |
|---|---|---|
| **Merged**(`state == merged`) | reaper が worktree 回収 | 何もしない(終端。watch 対象から外れる) |
| **Conflict**(`mergeable == CONFLICTING` / `mergeStateStatus == DIRTY`) | **conflict-resolver が拾う** | **何もしない**(委譲。needs-human を貼らない=デッドロックさせない) |
| **RedCI**(`mergeStateStatus == BLOCKED` かつ check rollup が `FAILURE`) | **ci-fixer が拾う** | **何もしない**(委譲) |
| **Healthy/Waiting**(`CLEAN` / `UNSTABLE` / `BEHIND` / pending) | GitHub が自動でマージ or 進行中 | 何もしない(`UNSTABLE` = required でない check の失敗。GitHub はマージするので触らない) |
| **HumanDisabled**(arm マーカーあり・`autoMergeRequest` が null・未 merged) | 人間の決定が最終 | **黙って手を引く**(再 arm もコメントもしない。既に auto-merger のマーカーが保証しているが、merge-watch も明示的に何もしない) |
| **Stuck**(上記いずれの担い手も無いまま arm-since が閾値超) | **どのループも拾わない** | **唯一エスカレーションする分類**: `meguri:needs-human` + 状況コメント |

**merge-watch が固有に価値を持つのは最後の Stuck だけ**である。例: branch protection に required check が後から追加されて workflow 側には存在しない → その check は永久に走らず PR は `BLOCKED`、しかし conflict でもなく rollup も `FAILURE` でもないので conflict-resolver も ci-fixer も拾わない → **誰にも気づかれず放置される**。これが issue の言う #35 と同じ放置問題の、専用ループでは塞げない最後の穴。merge-watch はこの穴の backstop。加えて、arm 済みなのに長時間 `BLOCKED`(人間レビュー待ちが長引く等)も、放置検出として同じ backstop で人間に nudge する。

### 「required checks のみを数える」を GitHub の判定に委ねる

issue は「red CI は required checks のみを数える」と要求する。required かどうかを meguri が branch protection の required check 名を列挙して自前判定すると、GitHub 側の設定変更に判定が置いていかれる(ADR-0003 が禁じた二重判定)。代わりに **GitHub の `mergeStateStatus` をそのまま権威として使う**:

- required でない check が失敗 → GitHub は `UNSTABLE`(マージ可能)を返す → Healthy 扱い、触らない。→ 受け入れ条件「required でない check の失敗が RedCI 扱いにならない」を**構造的に**満たす。
- required check が失敗 → GitHub は `BLOCKED` を返す → rollup が `FAILURE` なら RedCI(ci-fixer に委譲)。

required 判定は GitHub が下すもので、meguri は再導出しない。ADR-0003 の「権威は一箇所(GitHub)」と一貫。

### watch 状態は専用マーカーを新設せず「forge 由来」で持つ(ローカル状態ゼロ)

issue は `<!-- meguri:merge-watch ... -->` マーカーの upsert でリトライ回数・次回試行時刻を永続化せよと言う。だが調査すると、必要な状態はすべて**既に forge 上にある**もので導出でき、専用マーカー(= コメント編集用の comment-id 取得と upsert の複雑さ)を新設せずに済む:

- **arm-since**: #41 の arm マーカーコメントの `createdAt`。「この PR をいつから watch しているか」。
- **今の状態**: 毎掃引ライブに取る `mergeStateStatus` / `autoMergeRequest` / rollup。
- **エスカレーション済みか**: `meguri:needs-human` ラベルの有無(貼れば以降の掃引は素通り=冪等。他ループと同じブレーキ)。

→ Stuck 判定 = 「arm 済み・未 merged・Conflict でも RedCI でも HumanDisabled でもない状態が、arm-since から `STALE_AFTER` 超」。sqlite に一切持たず、meguri をいつ kill しても forge から再導出でき、受け入れ条件「マーカーコメントだけで再起動後も watch が継続」を(専用マーカーすら使わず)満たす。TransientError(429/5xx でスナップショットが取れない)も同じ backstop に畳む: 取れないまま `STALE_AFTER` 超なら Stuck としてエスカレーション。掃引ごとのリトライは自動(次掃引で再試行)なので明示のリトライカウンタは不要。

> **レビューでの採否**: 専用 `meguri:merge-watch` マーカーを敢えて設けない、が推奨。もし `meguri top` 等で「各 arm 済み PR の watch 状態」を可視化したい要求があるなら、その時に marker を導入する余地は残す(この spec ではスコープ外)。

## 実装内容(この branch の続きでやること)

1. **`src/forge/mod.rs`**:
   - `MergeStateStatus` enum(`Clean` / `Blocked` / `Behind` / `Dirty` / `Unstable` / `Draft` / `HasHooks` / `Unknown`)を追加。
   - arm 済み PR のスナップショットを 1 API で取る `Forge::pr_merge_state(number) -> MergeState { mergeable: MergeableState, status: MergeStateStatus, auto_merge_enabled: bool }` を追加(`gh pr view --json mergeable,mergeStateStatus,autoMergeRequest`)。既存 `pr_mergeable` は conflict-resolver が使うので温存。
   - arm-since を取るため、PR コメントを timestamp 付きで読む手段を追加(既存 `pr_comments` は body だけ)。最小案: `pr_comments_meta(number) -> Vec<PrComment { body, created_at }>` を新設(既存 `pr_comments` は #41 が使うため温存)。
2. **`src/forge/gh.rs` / `src/forge/fake.rs`**: 上記の実装。FakeForge には `set_merge_state_status` / `set_auto_merge_enabled` / コメント timestamp とスナップショット失敗(transient)注入用のセッタを足す。
3. **`src/engine/merge_watch.rs`(新規)**:
   - arm マーカー(`auto_merger::armed_marker` の head 非依存プレフィックス `<!-- meguri:automerge armed`)を持つ PR を discovery。
   - `Classify(snapshot) -> WatchClass`(純関数、単体テスト対象。looper の `mergewatch.Classify()` 相当)。
   - `sweep(deps)`: 各 arm 済み open PR を分類し、Stuck のみ `add_pr_label(needs-human)` + `comment_pr`。それ以外は no-op。auto-merge 無効時の設定ゲート(`am.enabled`)は auto-merger と同じ。
4. **`src/engine/mod.rs`**: `pub mod merge_watch;`。
5. **`src/engine/scheduler.rs`**: watch ループの掃引で `auto_merger::sweep` の**後**に `merge_watch::sweep` を呼ぶ(arm した直後の PR も同掃引で一度見られる順序)。
6. **`STALE_AFTER`**: モジュール定数(既定 24h 目安。他ループの `MAX_*_RUNS` に倣い、まずは定数。config 化は将来)。
7. **`tests/merge_watch_test.rs`(新規)**: 下記 e2e。
8. **ADR 0007**(本 PR に同梱): 「merge-watch は fixer ループに委譲し、どのループも拾わない stall だけをエスカレーションする / required 判定は GitHub の `mergeStateStatus` に委ねる / watch 状態はローカルに持たない」。
9. **`README.md`(任意)**: ループ/掃引の説明に merge-watch を 1 段落追記(auto-merger の記述の隣)。

## 受け入れ条件(元 issue から改訂)

- [ ] `Classify` の単体テスト: Merged / Conflict / RedCI / HumanDisabled / Healthy(UNSTABLE) / Stuck / Transient の各スナップショットが正しい分類になる。
- [ ] FakeForge での e2e:
  - Merged → 何もしない(needs-human もコメントも付かない)。
  - Conflict → **needs-human を付けない**(conflict-resolver をデッドロックさせないこと)。
  - RedCI(BLOCKED + rollup FAILURE)→ **needs-human を付けない**(ci-fixer に委譲)。
  - HumanDisabled → 再 arm もコメントもしない。
  - Stuck(arm-since が閾値超・どのループも拾わない BLOCKED)→ needs-human + コメント 1 回。二掃引目は needs-human で素通り(冪等)。
- [ ] `mergeStateStatus == UNSTABLE`(required でない check の失敗)が RedCI 扱いにならず、触られないこと。
- [ ] watch 継続がローカル状態に依存しないこと(sqlite を使わず、arm マーカーの createdAt + ライブ状態 + needs-human ラベルだけから再導出できる)。

## スコープ外 / 別 issue

- ci-fixer に「required check 失敗を能動的に食わせる」拡張(issue 本文が「将来の別 issue」と明記)。現状 ci-fixer は rollup FAILURE を独自に拾うので、arm 済み required 失敗はそのまま ci-fixer が処理する。
- `BEHIND`(ベースが進んだだけ)の自動 update-branch。今回は Healthy 扱いで放置(長引けば Stuck backstop が拾う)。
- `meguri:merge-watch` 専用マーカーによる watch 状態の可視化(`meguri top` 連携)。
- auto-merge 3/3(#43 系。連作の最終段)。

## レビューで決めてほしいこと(key decisions)

1. **Conflict / RedCI を merge-watch では no-op(委譲)にする**方針で良いか。issue の額面(needs-human 回送)は回送先が無かった時代の当座しのぎで、今それをやると fixer ループをデッドロックさせる。→ 推奨: no-op。
2. **専用マーカーを新設せず forge 由来の状態のみ**で watch する方針で良いか。→ 推奨: 新設しない。
3. **Stuck の閾値**(`STALE_AFTER`)を定数 24h で始める点。arm 済み・人間レビュー待ちが長引いた PR も backstop で nudge する(= 望ましい放置検出)ことを許容するか。
