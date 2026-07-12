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
- **discover**: `meguri:triage-report` レポート issue の head/interval マーカーで律速。
  - 対象 issue 集合 = 「open かつ `meguri:*` ワークフローラベルが未付与、`meguri:hold` でない、
    未解決 blocker がない」open issue。レポート issue 自身(`meguri:triage-report`)も
    `meguri:clean-report` も `meguri:*` プレフィックスで自動的に除外される。
  - cleaner の `needs_scan()`(default-branch head 移動 + `interval_hours` 経過)で再走査を律速。
    対象 issue 集合はマーカーに含めない(過剰トリアージは v0 では単なる上書きで無害)。
  - レポート issue に `meguri:hold` が付いていたら discover は空を返す。
- **prepare-worktree**: cleaner と同じ read-only detached checkout(`gitops::create_review_worktree`)。
- **execute**: read-only turn。対象 issue 群(番号・タイトル・本文)をプロンプトに埋め、
  各 issue の扱いを投機させて **判定 JSON を `.meguri/triage-report.json` に書かせる**。
  コミット・push・他 issue への書き込みは一切なし。cleaner と同じく checkout の pristine 検証
  (working tree clean + HEAD 不変)+ JSON パース検証を行い、失敗は **静かに諦めて次回巡回**
  (escalate しない)。
- **settle**: 判定をまとめて推薦テーブル本文を全上書きで書く。**唯一の forge write はこのレポート
  issue の create/update**。cleaner と同様レポート issue は閉じないので、自分の pane / worktree は
  自分で回収する(reaper に残さない)。
- 単体テスト: マーカー round-trip、`needs_scan` 判定表、`triage-report.json` のパース、
  レポート本文レンダリング(推薦テーブル + マーカー + ignore 適用)、プロンプト内容、
  discover の対象フィルタ(ワークフローラベル付き / hold / blocker を除外)を FakeForge で検証。

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

レポート本文 = マーカー `<!-- meguri:triage head=<sha> scanned=<epoch> -->` +
推薦テーブル(issue 番号 / 推薦 / 確信度 / 複雑度 / 根拠、`missing_info` は注記)+
「採用は自分で `meguri:ready`/`meguri:plan` を貼る・誤判定は `triage.ignore` へ・停止は
`meguri:hold`」のフッター。cleaner と同型。

## 触るファイル

- **`src/engine/triage.rs`(新規)**: `TriageLoop`、`TriageCheckpoint`、判定型
  (`Recommendation` / `Complexity` / `TriageItem` / `TriageReportFile`)、execute プロンプト、
  検証、`render_report`、settle。cleaner.rs をひな形に。
- **`src/engine/mod.rs`**: `pub mod triage;` と `default_loops()` へ `TriageLoop` を cleaner の後に追加。
- **`src/forge/mod.rs`**: ラベル定数 `LABEL_TRIAGE_REPORT = "meguri:triage-report"` を追加。
  `Forge` trait に **`list_open_issues()`**(全 open issue 列挙)を追加。
- **`src/forge/gh.rs`**: `list_open_issues()` = `gh issue list --state open --json
  number,title,body,labels --limit 50`(`list_open_prs` と同型)。
- **`src/forge/fake.rs`**: `list_open_issues()` = closed を除いた全 issue を返す。
- **`src/config.rs`**: `TriageConfig` + `TriageMode`(off/report/advise/auto)+ `triage_for()`。
  `Config` に `#[serde(default)] pub triage: TriageConfig`、`ProjectConfig` に per-project override。
- **`README.md`**: ループ表 / config サンプルに triage を追記(実装時)。

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
4. **マーカー/dedup の共有**: `needs_scan()` は純粋・cleaner でテスト済み。triage は tag 違い
   (`meguri:triage`)のマーカーが要るので、`needs_scan` を再利用しつつマーカーの
   parse/format/replace は tag を引数化した薄いヘルパにする(cleaner と triage の重複ロジックを
   避ける)。実装詳細のため、単純なら triage.rs 内複製でも可。
5. **対象集合の取得タイミング**: discover が返すのはレポート issue 1 件のみ。実トリアージ対象は
   execute でプロンプト構築時に `list_open_issues()` + フィルタで集める(最新状態を反映)。
   blocker gate は issue ごとに `blocked_by` を引く(v0 の小規模リポジトリでは許容)。
6. **失敗は静かに諦める**: 誤検知が壊すものが無いので `needs-human` escalation も bot ループ防止も
   不要(cleaner と同じ)。永続失敗時も marker の `scanned` だけ進めて次回 interval まで待つ。

## 非スコープ(別 issue)

- v1 #87: 対象 issue への提案コメント / `meguri:triage-*` 提案ラベル、updatedAt 追従の再トリアージ。
- v2 #88: 閾値超えを `meguri:ready`/`meguri:plan` として直接付与し worker/planner へ投入。
  レート制限・可逆性・オプトイン。昇格の是非はそのときの ADR で判断(ADR-0003 / ADR-0006 に従う)。

## テスト

- `cargo test`(triage の新規単体テスト + 既存の回帰)。
- `cargo clippy` / `cargo fmt`。
