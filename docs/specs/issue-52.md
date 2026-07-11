# issue-52 spec — scheduler: 完了に近い仕事から順にスロットを配る

考えてみれば不思議なことだが、いまの meguri には「どの仕事から手をつけるか」という考えがそもそも存在しない。`default_loops()` の登録順と `gh issue list` が返す順番——つまり誰かがたまたまレコード棚に並べた順に、針を落としているだけだ。新しい issue が偶然先に拾われ、仕掛かりの PR は棚の奥で静かに埃をかぶっていく。WIP はそうやって積み上がる。雪が降り積もるみたいに、音もなく。

この spec がやろうとしているのは、ごく小さな、しかし方向のはっきりした変更だ。**merge までの残り工程が少ない仕事から順にスロットを配る。** パイプラインの逆順、と言ってもいい。

## 優先順位(これがすべての核心だ)

| 優先 | ループ | 対象 | merge までの残り工程 |
|---|---|---|---|
| 1 | fixer | レビュー指摘が残っている meguri PR | 修正 push のみ |
| 2 | spec-worker | `spec-ready` な spec PR | 実装 → レビュー/修正 |
| 3 | reviewer | `spec-reviewing` の PR | レビュー → 人間承認 → 実装 → … |
| 4 | worker | `meguri:ready` issue | 実装 → レビュー/修正(未着手) |
| 5 | planner | `meguri:plan` issue | spec 作成から全部(最上流) |

ひとつだけ順序が残工程数と食い違う場所がある。worker(残り 4 工程)は数字の上では reviewer の対象(残り 5 工程)より完了に近い。それでも reviewer を上に置く。reviewer は仕掛かり品を人間の承認ゲートまで運ぶ短時間のジョブで、これを止めるとゲートの前に行列ができてしまうからだ。原則はこうだ——**新規着手より仕掛かりの完了を優先する(WIP を減らす)**。この原則の帰結として、表はパイプラインのきれいな逆順になる。原則そのものは ADR 0001 に置いた(spec より長生きするべき決定だから)。

同一ループ内では issue/PR 番号の昇順、つまり古い順の FIFO。古い PR ほどコンフリクトのリスクが澱のように溜まっていく。先に生まれたものを先に送り出す。

## 変更箇所(3 点。scheduler の周りだけで完結する)

### 1. `default_loops()` の並び替え — `src/engine/mod.rs:69`

```rust
/// ディスパッチ優先度順(パイプラインの逆順 = 完了に近い順)。
/// scheduler は先頭のループから順にスロットを配る。
pub fn default_loops() -> Vec<Arc<dyn Loop>> {
    vec![
        Arc::new(fixer::FixerLoop),
        Arc::new(spec_worker::SpecWorkerLoop),
        Arc::new(reviewer::ReviewerLoop),
        Arc::new(worker::WorkerLoop),
        Arc::new(planner::PlannerLoop),
    ]
}
```

`Scheduler::discover` はループ順にスロットを埋めて `max_concurrent` で打ち切る greedy な実装なので(`src/engine/scheduler.rs:76-112`)、並び順を変えるだけで「空きスロットは必ず下流の仕事から埋まる」が成立する。新しい機構は要らない。

### 2. ネストの反転 — `src/engine/scheduler.rs:81`

現状は `for project { for loop }`。これだとプロジェクト A の planner がプロジェクト B の fixer より先にスロットを取ってしまう。`for loop { for project }` に反転し、優先度がプロジェクト順より強く効くようにする。

### 3. discover 結果のソート — `src/engine/scheduler.rs:86`

```rust
let mut targets = lp.discover(deps).await?;
targets.sort_by_key(|t| t.issue_number);   // 同一ステージ内は古い順(FIFO)
```

各ループの `discover` 実装(fixer は `list_open_prs` の順、worker は `gh issue list` の新しい順、など)には手を触れない。scheduler の一箇所で正規化する。`Target.issue_number` は fixer/reviewer では PR 番号だが、どちらにせよ「先に生まれた番号が小さい」ので FIFO の意味は変わらない。

## 変わらないもの(意図どおり)

- **中断 run の再開が最優先のまま**(`scheduler.rs:34-39`)。走りかけの run はもっとも完了に近い仕事だ。discovery より先に再ディスパッチされる現状の構造をそのまま活かす。
- **プリエンプションはしない。** 走行中の run を止めて優先度の高い仕事に譲る、ということはやらない。スロットが空いた次の判断点で、下流が先に取る。それで十分だ。
- **`Loop` trait は変えない。** `priority()` のようなメソッドは生やさない。並び順そのものが優先度になる。

## 飢餓について(心配は要らない、と思う)

fixer や reviewer が仕事を持ち続けると worker/planner が永遠に走らないように見えるかもしれない。でも下流の仕事は、上流が新しい成果物を作らない限り有限で、必ず涸れる。fixer の対象は未解決スレッドの残る PR だけだし、reviewer は head ごとに一度レビューしたら再発見されない(`reviewer.rs:109` の `head_already_reviewed`)。井戸はいつか底を打つ。「下流を涸らしてから上流に着手する」——それがまさに狙いの挙動だ。

## 受け入れ基準(acceptance criteria)

1. `max_concurrent = 1` で fixer 対象と worker 対象が同時に存在するとき、fixer の run が先にディスパッチされる。
2. 同一ループに複数 target があるとき、issue/PR 番号の昇順で run が作られる。
3. 2 プロジェクト構成で、プロジェクト B の fixer がプロジェクト A の planner より先にスロットを取る(ネスト反転の担保)。
4. 中断 run の再開は引き続き discovery より優先される(既存挙動の非破壊)。
5. 既存の scheduler / ループ系テストが全部通る(ループ順に暗黙に依存しているテストがあれば追随修正)。

## テスト計画

`tests/scheduler_test.rs` の既存パターン(FakeForge + フェイクループ + `Scheduler` 直組み)に乗る。`FixedLoop` のような最小フェイクループが既にあるので(`scheduler_test.rs:414`)、kind の異なるフェイクループを複数登録して:

- `max_concurrent = 1` で優先ループの target が先に run になることを検証
- 複数 target のフェイクループで番号昇順のディスパッチを検証
- `projects` を 2 つ渡し、ループ→プロジェクトの順で埋まることを検証

既存テストは `default_loops()` をそのまま使っており順序への依存は見当たらないが、実装時に再確認する。

## 触るファイル

- `src/engine/mod.rs` — `default_loops()` の並び替えとコメント
- `src/engine/scheduler.rs` — ネスト反転、target ソート
- `tests/scheduler_test.rs` — 優先度テストの追加
- `docs/adr/0001-scheduler-priority-wip-first.md` — 原則の記録(本 PR に同梱)

## スコープ外(将来の話)

特定 issue を優先したくなったら、`meguri:priority` ラベルで同一ループ内の先頭に持ってくる拡張がこの構造の上に素直に乗る。`Loop` trait に `priority()` を生やすのはそのときで十分だし、たぶんそのときも要らない。
