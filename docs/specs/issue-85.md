# issue #85 — triage (1/3): v0 read-only トリアージレポート

## ゴール

open issue を meguri が自分で巡回し「どう扱うか(ready / plan / needs-human / hold / skip)」を
**推薦のみ・read-only** でレポートする `TriageLoop` を追加する。cleaner をひな形にし、書き込みは
専用レポート issue(`meguri:triage-report`)1 本の作成・更新に限定する(ADR-0003 の踏襲)。
判断の自動付与(ラベル付け・自動着手)は v1 #87 / v2 #88 の別 issue。

> 設計判断の「なぜ」は [ADR-0006](../adr/0006-triage-read-only-report-separate-from-cleaner.md) に記録した。
> 本 spec は実装が landed した時点で削除される使い捨ての足場。

## 受け入れ条件

- `src/engine/triage.rs` に `TriageLoop`(`Loop` trait 実装)が追加され、`default_loops()` の
  **cleaner の後(最低優先)** に登録される。
- `[triage]` config セクション(`TriageConfig { mode, interval_hours, ignore }`)を追加。
  **既定 `mode = "off"`(完全オプトイン)**。`clean_for` に倣った `triage_for()` アクセサ。
- `mode = "report"` のときのみ discover が起動する。`off` / `advise` / `auto` は v0 では起動しない
  (`advise`/`auto` は forward-compat のためパースだけ通し、v0 は idle)。
- **discover**: `meguri:triage-report` レポート issue の head/interval/max_issue マーカーで律速。
  - 対象 issue 集合 = 「open かつ `meguri:*` ワークフローラベルが未付与、`meguri:hold` でない、
    未解決 blocker がない」open issue。レポート issue 自身(`meguri:triage-report`)も
    `meguri:clean-report` も `meguri:*` プレフィックスで自動的に除外される。
  - 再走査の律速は cleaner の `needs_scan()` 条件(default-branch head 移動 + `interval_hours`
    経過)に加え、**マーカーの `max_issue`(前回走査時点の最大 open issue 番号)より大きい番号の
    open issue が存在すれば head が静止していても再走査する**(ただし律速は常に interval —
    新規 issue シグナルも `interval_hours` の経過を待ってから発火する。これで失敗時に `scanned`
    を進めるだけで全シグナルの再試行が interval に律速される)。triage の入力は issue 集合なので、
    head の移動だけを律速にすると「head 静止中に立った新規 issue を次の push まで拾わない」
    under-triage になるため。既存 issue の更新(updatedAt)に追従する*再*トリアージは v1 #87 の
    スコープで、v0 は新規 issue の初回トリアージにのみ反応する。過剰トリアージは v0 では単なる
    上書きで無害。
  - レポート issue に `meguri:hold` が付いていたら discover は空を返す。
- **prepare-worktree**: cleaner と同じ read-only detached checkout(`gitops::create_review_worktree`)。
- **execute**: read-only turn。対象 issue 群(番号・タイトル・本文)をプロンプトに埋め、
  各 issue の扱いを投機させて **判定 JSON を `.meguri/triage-report.json` に書かせる**。
  コミット・push・他 issue への書き込みは一切なし。cleaner と同じく checkout の pristine 検証
  (working tree clean + HEAD 不変)+ JSON パース検証を行い、失敗は **静かに諦めて次回巡回**
  (escalate しない)。
- **settle**: 判定をまとめて推薦テーブル本文を全上書きで書く。**唯一の forge write はこのレポート
  issue の create/update**(下記 quiet-skip の initializing レポート issue 作成もこの内側)。
  cleaner と同様レポート issue は閉じないので、自分の pane / worktree は
  自分で回収する(reaper に残さない)。
- 単体テスト: マーカー round-trip、`needs_scan` 判定表(head 移動 / interval / `max_issue` 超えの
  新規 issue による再走査 / initializing マーカー `head=none max_issue=0` が interval 経過後に
  再走査になること)、`triage-report.json` のパース、
  レポート本文レンダリング(推薦テーブル + マーカー + ignore 適用)、プロンプト内容、
  discover の対象フィルタ(ワークフローラベル付き / hold / blocker を除外)を FakeForge で検証。
- e2e テスト(`tests/triage_test.rs`、cleaner_test と同じ FakeForge + FakeMux + ローカル origin 構成、
  scripted agent が `.meguri/triage-report.json` を書く)で **read-only 境界を run 全体で検証**する:
  - 既定 `mode = "off"` では discover が空(巡回が一切起動しない)。
  - 初回巡回 → レポート issue が `meguri:triage-report` 付きで作成され、推薦テーブルとマーカーが
    本文に載る。再巡回では同じ issue の本文が全上書きされ、前回の項目が消えている。
  - **書き込み境界**: run 完了後、origin の refs が不変(push なし)、PR なし、レポート issue
    **以外の** issue の本文・ラベル・コメントが不変(triage 対象 issue に何も書いていないこと)。
  - レポート issue に `meguri:hold` → discover が空。
  - agent が成果物を出さない / JSON が壊れている / checkout が pristine でない → run は静かに skip、
    `meguri:needs-human` なし・コメントなし。レポート issue が既にあればマーカーの `scanned` のみ
    更新して(head は記録しない)次回 interval まで律速される。**初回巡回の失敗でレポート issue が
    まだ無い場合は、cleaner の `settle_skip` と同型に `head=none max_issue=0` マーカーの
    initializing レポート issue を作成する**(`scanned` の置き場がないと poll ごとの再試行に
    なるため。レポート issue 以外への write は失敗時もゼロ)。
  - settle 後に pane が閉じ worktree ディレクトリが消えている(reaper に残さない)。

## 判定の出力スキーマ(v0)

エージェントが `.meguri/triage-report.json` に書く:

```json
{
  "recommendations": [
    {
      "issue": 81,
      "recommendation": "ready | plan | needs-human | hold | skip",
      "confidence": 0.0,
      "estimated_complexity": "small | medium | large",
      "rationale": "…なぜこの扱いか(1〜2 文)",
      "missing_info": "…着手前に人間へ確認すべき点(任意)"
    }
  ]
}
```

`recommendation` / `estimated_complexity` は kebab-case enum、`confidence` は 0.0–1.0。
`missing_info` は任意(空可)。空配列(対象 issue なし)も正当な結果。

レポート本文 = マーカー `<!-- meguri:triage head=<sha> scanned=<epoch> max_issue=<n> -->` +
推薦テーブル(issue 番号 / 推薦 / 確信度 / 複雑度 / 根拠、`missing_info` は注記)+
「採用は自分で `meguri:ready`/`meguri:plan` を貼る・誤判定は `triage.ignore` へ・停止は
`meguri:hold`」のフッター。cleaner と同型。

## 触るファイル

- **`src/engine/triage.rs`(新規)**: `TriageLoop`、`TriageCheckpoint`、判定型
  (`Recommendation` / `Complexity` / `TriageItem` / `TriageReportFile`)、execute プロンプト、
  検証、`render_report`、settle、quiet-skip(cleaner の `settle_skip` と同型)。
  cleaner.rs をひな形に。
- **`src/engine/mod.rs`**: `pub mod triage;` と `default_loops()` へ `TriageLoop` を cleaner の後に追加。
- **`src/forge/mod.rs`**: ラベル定数 `LABEL_TRIAGE_REPORT = "meguri:triage-report"` を追加。
  `Forge` trait に **`list_open_issues()`**(全 open issue 列挙)を追加。
- **`src/forge/gh.rs`**: `list_open_issues()` = `gh issue list --state open --json
  number,title,body,labels --limit 50`(`list_open_prs` と同型)。
- **`src/forge/fake.rs`**: `list_open_issues()` = closed を除いた全 issue を返す。
- **`src/config.rs`**: `TriageConfig` + `TriageMode`(off/report/advise/auto)+ `triage_for()`。
  `Config` に `#[serde(default)] pub triage: TriageConfig`、`ProjectConfig` に per-project override。
- **`tests/triage_test.rs`(新規)**: 上記 e2e(cleaner_test.rs をひな形に)。
- **`README.md` / `README.ja.md`**: 両方を並行更新(実装時)。対象節:
  (1) Labels / ラベル節 — 記録用ラベルの段落に `meguri:triage-report` を `meguri:clean-report` と
  並記、(2) cleaner 節の直後に triage 節(read-only 推薦レポートの説明、既定 off のオプトイン)、
  (3) ループ表 — triage 行を追加、(4) Configuration / 設定 — `[triage]` サンプル
  (`mode` / `interval_hours` / `ignore` と既定値)、(5) Status / roadmap — ループ数
  (9 → 10)の更新と triage の一文。

## 主要な設計判断

1. **cleaner と別レポート issue**: read-only 掃引(cleaner)と意思決定(triage)は責務が別。
   単一 issue に同居させると本文の書き込み境界とマーカーが混ざるため、`meguri:triage-report` に
   別立てする(ADR-0006)。
2. **対象フィルタは `meguri:` プレフィックスで判定**: 個別ラベル列挙ではなく「`meguri:` で始まる
   ラベルを 1 つでも持てば triage 対象外」。ワークフローラベル・レポートラベル・将来の提案ラベルを
   まとめて除外でき、triage が自分/cleaner のレポート issue を再トリアージする事故も防げる。
   「未ラベル = 未トリアージ」の ADR-0005 不変条件と一致する。
3. **新 forge プリミティブ `list_open_issues()`**: 現状の list は `list_issues_with_label` のみで
   「ラベルなし open issue」を引けない。`--search` は使わずまず全列挙 → Rust 側でラベル/hold/blocker
   フィルタ(cleaner の `phase_label_anomaly` が全 issue をクライアント側で分類するのと同じ方針)。
4. **マーカー/dedup の共有**: マーカーの parse/format/replace は tag(`meguri:triage`)と
   `max_issue` フィールドが違うだけなので、tag を引数化した薄いヘルパにする(cleaner と triage の
   重複ロジックを避ける)。走査判定は cleaner の `needs_scan()`(純粋・テスト済み)を土台にするが、
   そのままでは head 静止中の新規 issue に反応できないため、`max_issue` シグナルを加えた triage 版の
   純粋関数として持つ。実装詳細のため、単純なら triage.rs 内複製でも可。
5. **対象集合の取得タイミング**: discover が返すのはレポート issue 1 件のみ。実トリアージ対象は
   execute でプロンプト構築時に `list_open_issues()` + フィルタで集める(最新状態を反映)。
   blocker gate は issue ごとに `blocked_by` を引く(v0 の小規模リポジトリでは許容)。
   対象 issue 群の番号・タイトル・本文をプロンプトへ全量埋める設計も、`--limit 50` の対象規模・
   本文長を含め同じく v0 の小規模リポジトリでは許容(コンテキスト圧迫が問題になったら本文の
   切り詰め等を後続で検討)。
6. **失敗は静かに諦める**: 誤検知が壊すものが無いので `needs-human` escalation も bot ループ防止も
   不要(cleaner と同じ)。永続失敗時も marker の `scanned` だけ進めて次回 interval まで待つ。
   初回巡回の失敗ではまだレポート issue が無く `scanned` を保存する場所がないため、cleaner の
   `settle_skip` と同型に **`head=none` の initializing レポート issue の作成だけは許す**
   (作らないと poll ごとの再試行になり「interval まで律速」が成り立たない)。これもレポート
   issue への create なので「唯一の forge write はレポート issue の create/update」の
   書き込み境界は変わらない。

## 非スコープ(別 issue)

- v1 #87: 対象 issue への提案コメント / `meguri:triage-*` 提案ラベル、updatedAt 追従の再トリアージ。
- v2 #88: 閾値超えを `meguri:ready`/`meguri:plan` として直接付与し worker/planner へ投入。
  レート制限・可逆性・オプトイン。昇格の是非はそのときの ADR で判断(ADR-0003 / ADR-0006 に従う)。

## テスト

- `cargo test`(triage の新規単体テスト + `tests/triage_test.rs` の e2e + 既存の回帰)。
- `cargo clippy` / `cargo fmt`。
