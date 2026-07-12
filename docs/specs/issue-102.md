# issue-102 spec — issue ラベルを「フェーズ × ボールの所在」の2軸モデルにする

現状のラベル遷移では「無ラベル」が3つの状態(未トリアージ / 実装 PR が open / 実装完了後の PR)
を兼ねていて、issue 一覧から状況が読み取れない。worker と planner は PR 作成時にトリガーラベル
(`ready` / `plan`)を**剥がして無ラベルにする**ため、一番進んでいる issue が未トリアージと
見分けがつかない(`src/engine/worker.rs:102-112`、`src/engine/planner.rs:170-185`)。

この spec の決定は一行で書ける。**フェーズ(進行状態)とボールの所在(誰の番か)を別軸にし、
PR 作成時にトリガーラベルを「剥がす」代わりに新フェーズラベルへ「差し替える」。** これにより
issue の無ラベルは「未トリアージ」の一義的な意味になる。

2軸モデルそのものと、それが確立するドメイン不変条件(関与済み open issue はフェーズラベル
ちょうど1つ / 無ラベル = 未トリアージ / needs-human はフェーズに重ねる)は spec より長生き
すべきなので **ADR 0005**(本 PR 同梱)に置いた。この spec は実装の当たりと受け入れ基準に絞る。

## 決定

### D1: 新フェーズラベル定数を2つ足す — `src/forge/mod.rs`

- `LABEL_SPECCING = "meguri:speccing"`(🟣 `#6F42C1`)— spec PR が open。
- `LABEL_IMPLEMENTING = "meguri:implementing"`(🟢 `#0E8A16`)— 実装 PR が open。

既存の `LABEL_READY`(🔵)/ `LABEL_PLAN`(🔵)と合わせてフェーズ軸を、`LABEL_WORKING`(🟡)/
`LABEL_NEEDS_HUMAN`(🔴)/ `LABEL_HOLD`(⚪)がボール軸を成す。ラベル一覧(色つき)は ADR 0005 を
参照。

### D2: worker は PR 作成時に `ready` → `implementing` に差し替える — `src/engine/worker.rs`

`WorkerFlavor::settle_labels`(`worker.rs:102-112`)を「`working` と `ready` を剥がす」から
「`working` と `ready` を剥がし、`implementing` を足す」に変える。`implementing` は load-bearing
(無ラベル = 未トリアージの不変条件を担保する)なので、planner の spec-reviewing 付与と同じく
**失敗したら run を失敗させる**(足せないと issue が無ラベルに落ち、未トリアージと誤認される)。
`working` / `ready` の除去は従来どおり best-effort。

### D3: planner は spec PR 作成時に `plan` → `speccing` に差し替える — `src/engine/planner.rs`

`PlannerFlavor::settle_labels`(`planner.rs:170-185`)は現状 PR に `spec-reviewing` を付け、issue
から `working` / `plan` を剥がす。ここに issue へ `speccing` を足す処理を加える(D2 と同様に
load-bearing なので失敗は run 失敗)。PR 側は `spec-reviewing` のまま**現状維持**。

### D4: spec-worker は実装着手時に issue を `speccing` → `implementing` にする — `src/engine/spec_worker.rs`

受け入れ基準「実装着手で `implementing` になる」に忠実に、フリップは **`prepare_work` の
クレーム成功直後**(PR に `working` を付けた後、`spec_worker.rs:149-166`)で行う: issue に
`implementing` を足し、`speccing` を剥がす。add/remove はべき等なので resume で再実行されても
安全。

- PR 側の `settle_labels`(`spec_worker.rs:295-308`: `spec-ready` / `working` を PR から除去)は
  **無変更**。フェーズラベルは issue 側、PR ラベルは PR 側という切り分けを崩さない。
- 代替案(settle 成功時にフリップ)を退けた理由: spec-worker が実装途中で `needs-human` に落ちた
  とき、issue が `speccing` のまま「spec で詰まった」ように見えてしまう。クレーム時フリップなら
  `implementing` + `needs-human` で「実装で詰まった」が正しく残る。トレードオフは、spec-worker が
  benign にスキップ/失敗して PR が `spec-ready` に戻った場合に issue だけ `implementing` が先行し
  うる点だが、その状態でも「実装 PR(= spec PR)が open」の意味は保たれ、再 discovery で
  spec-worker が引き取り直すため実害はない。

### D5: escalate はフェーズラベルを剥がさない(変更不要の確認)— `src/engine/flow.rs`

`escalate_on_forge`(`flow.rs:431-445`)は `needs-human` を足し `working` を剥がすだけで、
フェーズラベルには触れていない。2軸モデルの「needs-human はフェーズに重ねる」規約はこの既存挙動
そのものなので、**コード変更は不要**。spec-worker の `escalate`(`spec_worker.rs:322-325`)も PR の
`working` 除去 + issue への `needs-human` で、issue のフェーズラベル(`implementing`)は残る。
受け入れ基準「どのループが needs-human に落としてもフェーズラベルは残る」はこれで満たす。

### D6: cleaner にフェーズラベル不変条件のチェックを足す — `src/engine/cleaner.rs`

「孤児 `meguri:working`」を報告する既存の machine check(`orphan_working`、`cleaner.rs:797-829`)に
倣い、**フェーズラベル異常**を報告する machine check を足す(report-only、書き込み境界はレポート
issue のまま — ADR 0003)。フェーズラベルはクレームマーカーではなく「issue クローズまで残る」ので
「active run が無い = 孤児」ロジックは**適用しない**(常に誤検知する)。代わりに不変条件そのものを
検査する:

- open issue が**フェーズラベルを2つ以上**持つ(差し替えの取りこぼし)。
- open issue が**ボールラベル(`working` / `needs-human`)を持つのにフェーズラベルが0個**
  (関与済みなのにフェーズが欠落)。

`MachineFindings` に `phase_label_anomaly` を足し、`render_report` に節を1つ追加する。実装は各
フェーズラベルで `list_issues_with_label` を呼んで集計する軽量なもの。backing PR とのクロス
チェック(閉じられた PR に取り残された `implementing` 等)はスコープ外(将来)。

### D7: ラベル色をコード化する — `src/forge/gh.rs`

`ensure_label`(`gh.rs:236-249`)は現状すべてのラベルを `#1D76DB` + "managed by meguri" で作る。
色が2軸の意味(spec 中=紫 / 実装中=緑 / 待ち=青 / working=黄 / human=赤 / hold=灰)を担うので、
既知の meguri ラベル → (色, 説明) の**静的マップ**を持たせ、そのラベルはスキーム色で作成する
(未知のラベルは従来の汎用青にフォールバック)。これで新規リポジトリでも色が自動で揃う。

既存リポジトリで既に汎用青で作られてしまったラベルの色是正は、**一度きりの ops**
(`gh label edit <name> --color <hex>`、または `gh label create --force`)で行う — 手順を README /
本 spec に明記する。毎ポーリングで既存ラベルを recolor する(`create --force` を全 label 操作で
呼ぶ)のは、人間が付けた色を上書きし続ける副作用と API ノイズがあるので採らない。

### D8: README を更新 — `README.md` / `README.ja.md`

- ラベル表(`README.md:126-133` / `README.ja.md:125-132`)を2軸(フェーズ / ボール)構成に
  書き直し、`meguri:speccing` / `meguri:implementing` を追加、無ラベル = 未トリアージを明記。
- 冒頭のフロー図(`README.md:10-11` 付近)の「discover & claim」注記をフェーズ差し替えに合わせて
  更新。
- 上記 D7 の色是正 ops 手順を Labels セクションに一文添える。

## 変わらないもの(意図どおり)

- **PR 側ラベル**(`spec-reviewing` / `spec-ready`)と、それに依存する reviewer / impl-reviewer /
  fixer / ci-fixer / conflict-resolver の discovery。これらは PR ラベルで動き、issue のフェーズ
  ラベルには触れない。
- **worker の needs-plan 降格**(`worker.rs:120-169`: `ready` → `plan` に差し替え)。既に
  「フェーズラベルを差し替える」形で、2軸モデルと整合。無変更。
- **decompose の親 issue の無ラベル化**(`planner.rs:270-276`)。親は全フェーズを外れ「人間が
  最終判断」に戻る。ADR 0005 で無ラベルの意味と整合すると明記済み。無変更。

## 受け入れ基準(acceptance criteria)

1. 直行フロー: `ready` → `+working` → PR 作成で issue が `implementing` に差し替わり(`ready` /
   `working` は消える)、issue クローズまで残る。
2. spec-first フロー: `plan` → `+working` → spec PR 作成で issue が `speccing` に差し替わり、
   spec-worker のクレームで `implementing` になる。
3. どのループが `needs-human` に落としても、issue のフェーズラベルは残る(`implementing` +
   `needs-human` 等)。
4. meguri が関与した open issue にフェーズラベルがちょうど1つ付いている(無ラベル = 未トリアージ)。
5. cleaner が、フェーズラベル2つ以上 / ボールラベルありでフェーズ0個 の open issue をレポート
   issue に報告する(他への書き込みは無し)。
6. 新規作成される meguri ラベルがスキームの色で作られる(`speccing`=紫、`implementing`=緑 等)。
7. README(en/ja)のラベル表が2軸構成で、`speccing` / `implementing` と「無ラベル = 未トリアージ」
   を記述している。
8. 既存テストが全部通る(下記の更新込み)。

## テスト計画

- **更新が要る既存テスト**:
  - `tests/scheduler_test.rs:178-182`(PR 後に `ready` / `working` が消える)に、`implementing` が
    **付いている**ことの assert を追加。
  - `tests/planner_test.rs:282-285` 付近(spec PR 後に `plan` が消える)に、issue が `speccing` に
    なっていることの assert を追加。
  - `src/engine/worker.rs` / `spec_worker.rs` の settle 系ユニットテスト(あれば)を新遷移に更新。
- **新規**:
  - worker: settle 後に issue が `{implementing}` ちょうど(ready/working 無し)。
  - planner: settle 後に issue が `{speccing}`、PR が `{spec-reviewing}`。
  - spec-worker: prepare_work のクレーム後に issue が `speccing` → `implementing`。
  - cleaner: フェーズ2つ / フェーズ0個+ボール の issue が machine findings に載り、正常な issue
    (フェーズ1つ)は載らないこと(`tests/cleaner_test.rs` の FakeForge パターン)。

## 触るファイル

- `src/forge/mod.rs` — `LABEL_SPECCING` / `LABEL_IMPLEMENTING` 定数
- `src/forge/gh.rs` — `ensure_label` の色マップ
- `src/engine/worker.rs` — `settle_labels`(→ `implementing` 差し替え)
- `src/engine/planner.rs` — `settle_labels`(→ `speccing` 差し替え)
- `src/engine/spec_worker.rs` — `prepare_work`(`speccing` → `implementing` フリップ)
- `src/engine/cleaner.rs` — フェーズラベル異常の machine check + render
- `README.md` / `README.ja.md` — ラベル表・フロー図・色是正 ops 手順
- `tests/scheduler_test.rs` / `tests/planner_test.rs` / `tests/cleaner_test.rs`(+ worker/spec-worker
  のユニットテスト)— 新遷移の assert
- `docs/adr/0005-issue-labels-two-axis-phase-and-ball.md` — 決定の記録(本 PR 同梱済み)

## スコープ外(将来の話)

- PR 側のマージ待ち可視化(`meguri:awaiting-merge`)。必要になったら PR ラベルとして足す。
- cleaner による backing-PR クロスチェック(閉じられた PR に取り残されたフェーズラベルの検出)。
- フェーズラベルの per-project カスタム名 / 色。
