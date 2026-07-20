# spec: issue #198 — ADR 0012(level-triggered reconciler)の承認と移行の起点

> この spec は使い捨ての足場([ADR 0001-specs-are-disposable-scaffolding]
> (../adr/0001-specs-are-disposable-scaffolding.md))。実装が landed したら削除する。
> 恒久的な設計判断は `docs/adr/0012-loops-are-emergent-level-triggered-reconciler.md` に
> 既に振り分けてある。

## この issue のスコープ

これは**親 issue** である。やることは2つだけ:

1. **ADR 0012 を承認可能な状態にする** — proposed の ADR を書き起こし、spec review で
   大きな決定に合意を収束させる(本 PR のマージ = 承認)。
2. **5 スライスの移行ロードマップを固める** — 承認後にそのまま個別 issue として起票できる
   粒度まで、各スライスの範囲・受け入れ・依存を確定する。

やらないこと: reconciler の実装。`reconcile` / `Verdict` / `Step` を書くのは移行スライスの
仕事で、本 issue では1行も書かない。実装は承認後、本 branch ではなく各スライスの branch で進む。

## spec 深度の理由(adaptive)

**design spec** を選ぶ。不確実性 × 影響範囲がともに最大級だから:

- **不確実性(高)**: `Snapshot` に何を積むか、activeQ の優先度関数の具体形、spec/status 再解釈の
  4 handshake の中身は未決。
- **影響範囲(全域)**: engine 全体(15+ の loop/sweep)、ラベル運用、sqlite、auto-merge の
  マージ挙動に及ぶ。

**Veto 該当**: 永続状態(sqlite workqueue、ラベル意味)・公開 contract(`reconcile`/`Step`)・
不可逆な運用リスク(engine 差し替え、マージ挙動)に触れる。よって **migration & rollback は必須**。

## 受け入れ条件(この issue)

- [ ] `docs/adr/0012-loops-are-emergent-level-triggered-reconciler.md` が存在し、決定の要点
      7 項目・supersede(0007)・不変移設(0003/0009/0006/0008/0001/0005)を明記している。
- [ ] 本 spec が 5 スライスそれぞれについて「範囲・受け入れの芯・依存(blocked_by)・kind」を
      持ち、承認後にそのまま起票できる。
- [ ] ADR が現行の全 loop/sweep(10 `Loop` + 8 poll-tick sweep)を3 Kind のどれが所有するか
      漏れなく割り当てている(対象外を残さない)。
- [ ] ラベル 2 軸の spec/status 再解釈が **ADR 0005 の amend** として位置づけられ、どの既存
      ラベルが spec 軸/status 軸か、人間待ち・停止の権威がどこかまで ADR に書かれている
      (reject ではない)。
- [ ] `reconcile` の名前衝突(既存 body-edit sweep #142)への対処方針が書かれている。
- [ ] 本 PR は ADR + spec のみを変更し、`src/` を変更しない(実装ゼロ)。

## 移行ロードマップ(承認後に起票する 5 スライス)

各スライスは独立に review・rollback できる縦切り。番号 = 起票順・依存順。

1. **merge tail(Op のみ)** — kind: plan。auto_merger / merge_watch を `Op` に載せ替え、
   **BEHIND を `Op(UpdateBranch)` + 再 arm で閉じる**。observe 一括クエリの **API コストを
   実測**して以降のスライスの前提にする。forge 権威に触れない Op から始めることで移行中の二重
   権威リスクを避ける(最小 blast radius)。芯: BEHIND が回帰テストで閉じ、API コスト実測値が
   記録される。
2. **Schedule Kind + repo-side config(#165)** — kind: plan。scheduler_fire を Schedule
   Kind に。芯: cron 起票が Schedule Kind 経由で動き、既存の消化ループと非回帰。blocked_by: [1]。
3. **queue + fixer 家族** — kind: plan。workqueue(activeQ / backoffQ / parked)を導入し、
   fixer / ci_fixer / conflict_resolver を Issue Kind の arm に畳む。芯: 3 fixer が arm 化され、
   「全状態にちょうど1つの所有 arm」property test が緑。blocked_by: [1]。
4. **planner / worker / spec_worker / guard + Repo Kind** — kind: plan。残りの重い agent 起動系
   と cleaner / triage / routing_drift を吸収し、**旧 `Loop` trait を撤去**。ここで残りの
   issue-identity 駆動 sweep も畳む: `reaper`(→ `Op(Finalize)`)・`decompose_materializer`
   (→ spec-ready 分解提案への act)・既存 `reconcile`(body-edit、→ `reconcile_body_edits` へ
   退避)。芯: default_loops と全 poll-tick sweep が消え、全 Kind が reconciler 経由。
   blocked_by: [2, 3]。
5. **config 键粒度(ADR 0013)** — kind: plan。設定粒度を新構造に整える。芯: config が新構造に
   追随し hot reload 非回帰。blocked_by: [4]。

> 補足: ADR 0013 / 0014 / 0015 / 0016(#197)の実装は上記スライスに合流する(独立起票しない)。
> 起票は本 PR マージ後の「次の動き」。本 turn では起票しない(issue の指示どおり)。

## 触るファイル(この issue の PR)

- `docs/adr/0012-loops-are-emergent-level-triggered-reconciler.md`(新規、本 PR で追加)
- `docs/specs/issue-198.md`(本ファイル、実装完了時に削除)
- `src/` は**変更しない**。

移行スライスが将来触る中心(参考、本 PR では触らない): `src/engine/mod.rs`(`Loop` trait /
`default_loops`)・`src/engine/scheduler.rs`(dispatch)・`src/engine/{auto_merger,merge_watch,
fixer,ci_fixer,conflict_resolver,cleaner,triage,routing_drift,scheduler_fire,reaper,
decompose_materializer,plan_handoff,reconcile}.rs`(10 loop + 8 poll-tick sweep すべて)・
`src/store/`(workqueue テーブル)・`src/forge/mod.rs`(`LABEL_*` の spec/status 再解釈)。

## 鍵となる決定(と残る未決)

決定済み(ADR に振り分け済み):

- 3 Kind(Issue / Repo / Schedule)。新トリガは Kind でなく arm。
- `reconcile(id) -> Verdict`、identity のみ、判断は毎回観測から。
- `next_step` は純関数。「ちょうど1つの所有 arm」を網羅 property test で保証。
- BEHIND の解は `Op(UpdateBranch)` + arm 1本。
- dispatch = workqueue + resync。イベントは最適化、resync が正しさ。

**本 issue(この承認 PR)で決めること** — 上記「決定済み」の粒度、つまり ADR の 7 項目そのものが
本 issue の決定物である。加えて本 PR は次の2点をここで確定させる(後続に送らない):

- **`reconcile` の名前衝突の対処方針**: 既存の body-edit sweep(#142、`src/engine/reconcile.rs`)
  が同名。**旧 sweep を `reconcile_body_edits` へ退避する**方針を本 PR で確定する(改名の機械的
  適用はスライス 4 が実施するが、どちらに寄せるかの判断は今ここで閉じる)。
- **5 スライスの切り方と依存順**: 上の移行ロードマップが本 issue の成果物であり、承認 = この
  切り方への合意。

**後続スライスで確定する未決事項**(本 issue では意図的に開けておく — 実測や実装の中でしか
決められないため):

- **Snapshot の境界**: informer cache に何を積むか(issue/PR/label/CI status/mergeable/
  base 差分…)。API コストと直結するので、スライス 1 の実測を待って固める。
- **spec/status 4 handshake の具体表**: 誰がどの spec 軸遷移を書けるか。ADR 0005 の
  phase(plan/speccing/ready/implementing)への写像表は、status 再構築の実装が固まるスライス
  4 で確定する。

## Architecture への影響

`Loop` trait(discover / drive)+ 登録順優先度 という現行の分散モデルを、単一の reconcile
契約 + workqueue へ集約する。`docs/architecture/loops.md`(設計者向けの loop 地図)は移行
完了時に「reconciler の地図」へ書き換える対象になる(本 PR では触らない)。

## 検討した代替案と、選んだ理由

- **16 個目の loop(base 更新 loop)を足す** — 却下。BEHIND は直るが、トリガ組み合わせ爆発の
  構造的欠陥はそのまま。次の組み合わせでまた loop が増える。
- **big-bang で全 loop を一度に reconciler へ** — 却下。engine 全域 × 永続状態 × マージ挙動を
  同時に差し替えるのは rollback 不能。5 スライスの縦切りにして各段で review・rollback 可能に
  する。
- **spec/status 再解釈を先にやる** — 却下。status を観測から再構築する義務(決定 3/5)が
  実装される前に意味を変えると、旧ラベル運用と食い違う。だから forge 権威に触れない merge tail
  (Op のみ)から始める。

## Migration & rollback(Veto により必須)

**移行戦略**: 5 スライスの縦切り。各スライスは動く main を保ったまま薄く縦に移す。新旧(旧
`Loop` trait と新 reconcile)は移行中**併存**し、旧 trait の撤去はスライス 4 まで遅らせる。

**永続状態への影響**:

- **sqlite**: workqueue テーブル(activeQ / backoffQ / parked)をスライス 3 で追加。既存の
  run 進行管理・`schedule_state`・`runs.cadence_label` とは別テーブルで、既存を破壊しない。
- **ラベル(forge = 権威)**: spec/status 再解釈は **ADR 0005 の意味の amend** で、ラベル文字列
  (`meguri:*`)自体は変えない。rollback 安全性の担保は軸ごとに分けて考える(過大評価しない):
  - **status 軸の進捗ラベル**(`working` / `speccing` / `implementing`)は観測(PR 状態・CI・
    mergeable・run 履歴)から再構築する義務があり、消失しても再導出できる。
  - **spec 軸の human 宣言**(`plan` / `ready` / `hold` / `needs-human`)は観測から作れない
    入力で、forge に書かれた値が権威。**人間待ち・停止の権威はここにあり、機械は再構築しない**。
    rollback 時もこれらは触らず forge をそのまま信じる — これが「機械が人間の停止を勝手に
    上書きしない」安全性の担保。

**rollback**:

- スライス 1〜3 は旧 `Loop` trait が生きているので、当該スライスの PR revert で旧挙動へ戻る
  (workqueue テーブルは残っても不活性)。
- スライス 4(`Loop` trait 撤去)は最も不可逆。ここで初めて旧経路が消えるため、4 は 2・3 が
  main で安定してから起票する(blocked_by で強制)。撤去 PR は単独 revert で trait を復元できる
  よう、trait 撤去と最終吸収を同一 PR に閉じる。
- **本 issue の PR 自体**(ADR + spec のみ)は `src/` を触らないので rollback リスクゼロ。

## Observability

- **property test**: 「全 Snapshot 状態にちょうど1つの所有 arm」を網羅テストで担保(欠落 =
  BEHIND 類、二重 = 競合を回帰検出)。これが移行の一次的な安全網。
- **API コスト**: スライス 1 で observe 一括クエリのコストを実測し、以降の前提として記録する
  (`meguri logs` / 既存の event 系に載せる)。
- **既存の可視化非回帰**: `meguri tasks`(silent skip の理由表示)・`meguri stats routing`・
  merge 系の event が移行後も同じ情報を出すことを各スライスの受け入れに含める。

## テスト戦略

- **純関数 `next_step` の property test**(網羅) — 上記の「ちょうど1つの所有 arm」。
- **BEHIND の回帰テスト** — arm 済み × base 進行 の Snapshot が `Op(UpdateBranch)` を返す
  (スライス 1)。
- **統合テスト**(`tests/*.rs`、実 tmux・実 git worktree・bare origin) — 既存の loop 統合
  テストが移行後も通ること(非回帰)を各スライスで確認。`FakeForge` / `FakeMux` で observe/act の
  境界をアサートする。
- 本 PR はドキュメントのみのため、CI の `cargo fmt --check` / `clippy` / `nextest` / `--doc` は
  実装を含まないが、リポジトリ規約どおり commit 前に一通り通す。
