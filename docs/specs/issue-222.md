# spec: issue #222 — scheduler_fire を Schedule Kind へ（repo-side config を包含）

ADR 0012（level-triggered reconciler）移行のスライス2 / 5。`scheduler_fire` の poll-tick sweep を
**Schedule Kind** の reconciler（observe → 純関数 next_step → act）に載せ替え、あわせて **schedule を
repo-side config（`meguri.toml`）に取り込む**二層化を行う。

durable な設計判断はこの spec には残さない:
- 「schedule を repo-eligible にし、default branch から発見読み取りする」理由 → **ADR 0026**（本 PR で追加）。
- reconciler の Kind 分割・観測境界・enqueue-only の不変 → **ADR 0012 / 0009 / 0015**（既存）。

この spec は実装着手時に削除される足場である。

## spec 深度: design（深い方）

**理由**: 新しい公開 config 契約（repo `meguri.toml` の `schedules`）を足し、cron が自動で issue/task を
起票するという運用リスクを持ち、かつ受け入れの芯が「kill / resync 後も二重発火・取りこぼしなし」という
永続状態の正しさである。veto ルール（永続状態・公開契約・不可逆な運用リスクのいずれかに触れるなら
migration & rollback 必須）に該当するため、design 深度で migration & rollback を書く。

## 受け入れの芯（issue より）

1. cron 起票が **Schedule Kind 経由**で動き、既存の消化ループと非回帰（`tests/schedule_test.rs` が緑）。
2. enqueue-only 原則（ADR 0009）は不変のまま Schedule reconciler 化（pane も run record も作らない）。
3. 発火時刻の永続がローカル store に載り、kill / resync 後も**二重発火・取りこぼしなし**。
4. schedule を repo ルート `meguri.toml` に置け、default branch の内容が効く（ADR 0026）。host 側の
   `[[projects.schedules]]` を書かない既存プロジェクトの挙動は完全に不変。

## 触るファイル

- `src/engine/scheduler_fire.rs` → **`src/engine/schedule.rs` にリネーム**（1 Kind = 1 file、ADR 0012 の
  「Schedule Kind」語彙に揃える）。内部を observe → `next_step`（純関数）→ act に再構成。
- `src/engine/mod.rs` — `pub mod` 名の差し替え、doc comment の参照更新。
- `src/engine/scheduler.rs` — 呼び出し `scheduler_fire::sweep` → `schedule::sweep`（poll tick の位置は
  不変）。
- `src/config.rs` — `RepoConfig` に `schedules: Vec<ScheduleConfig>` を追加。per-schedule 検証
  （cron / body xor body_file / body_file パス安全 / local×plan）を `Config::validate_schedules` の
  インラインから**再利用可能な関数** `validate_schedule(mode, &ScheduleConfig)` に切り出す。
- `src/main.rs` の `doctor_schedules` — default branch 上の repo schedules も読んで lint し、
  host/repo shadow を報告。
- `src/app.rs` の `cmd_schedules` — 表示を「有効 schedule 集合（host ∪ repo）」＋出所（host/repo）に拡張。
- 参照追随（doc comment / import）: `src/tasks.rs` `src/notify/mod.rs` `src/store/schedules.rs`
  `tests/schedule_test.rs`。
- テスト: `src/engine/schedule.rs` の unit（`next_step` の property test を含む）と、`tests/schedule_test.rs`
  に repo-side schedule のケースを追加。

sqlite migration は**不要**（`schedule_state` のスキーマ・キーは不変）。

## 主要な決定（すべてこの pass で確定）

### D1. モジュール名 = `schedule.rs`
`scheduler_fire.rs` を `schedule.rs` にリネーム。merge_tail スライスの先例（新概念に新ファイル名）に倣い、
ADR 0012 の「Schedule Kind」に語彙を合わせる。`epoch_now` / `schedule_marker` も移動。リネームは機械的で、
挙動は変えない。

### D2. reconciler の型（observe → next_step → act）
merge_tail と同じ三分割にする。schedule の identity は `(project, name)`。

```rust
/// 純粋な観測入力（壁時計・I/O を持ち込まない）。
struct Snapshot {
    seeded: bool,          // schedule_state 行が既にあるか
    due: bool,             // is_due（cron 窓に発生が入るか。既存の純関数を流用）
    allow_overlap: bool,   // 定義値
    last_item_open: bool,  // due かつ !allow_overlap のときだけ意味を持つ
}

/// この Kind が出す Step。ADR 0012 §4 の Op/Wait に対応（agent は起動しない）。
enum Step {
    Seed,               // 初観測: 窓底を seed、発火しない（バックフィル抑止）
    Fire,              // 発火: enqueue + record_schedule_fire(Some(key))
    SkipOverlap,       // due だが直近 item が open: 窓は消費、key は据え置き
    Wait(&'static str), // not due（＝所有 arm が「今は動かない」）
}

fn next_step(s: &Snapshot) -> Step; // 純関数。同じ Snapshot なら常に同じ Step。
```

- `last_item_open` は `due && !allow_overlap` のときだけ forge / store に問い合わせて解決し、それ以外は
  `false` を入れる（`next_step` が `due` を先に見るので無害）。merge_tail が `policy_ok` を遅延解決して
  Snapshot に畳むのと同じ流儀。
- forge が openness を返せない（API エラー）ときは **observe エラーとして per-schedule warn し、次 tick で
  リトライ**（発火も窓前進もしない = 二重発火を避ける）。現行 `fire_one` の `?` 伝播と同じ。Step には
  しない。
- **property test**（ADR 0012 §3）: `(seeded, due, allow_overlap, last_item_open)` の全組合せを列挙し、
  `next_step` が常にちょうど1つの Step を返すこと（所有の欠落も二重所有もない）を機械的に保証する。

### D3. schedule は repo-eligible（ADR 0026）
`RepoConfig` に `schedules: Vec<ScheduleConfig>` を追加する。run flow はこの値を**使わない**
（`Deps::with_repo_config` / `RepoConfig::has_values` は schedules を無視 — schedule だけの `meguri.toml` は
run に何も畳み込まない）。フィールドを足すのは、`deny_unknown_fields` の下で per-run の worktree parse が
`schedules` キーを弾かないようにするため。

### D4. 有効 schedule 集合 = host ∪ repo（default branch）
`schedule::sweep` は発火対象を、host `deps.project.schedules` と、default branch の `meguri.toml`
（`gitops::read_file_at_default_branch` → `RepoConfig::parse_str` → `.schedules`）の**和**として作る。
claim 時 pin ではなく発見読み取り（ADR 0015 / 0026）。

### D5. 名前衝突は host が勝つ
同名 schedule は host 定義を採用し、repo 側は落とす。`schedule.shadowed`（`{project, name}`）を emit + warn。
黙殺しない。

### D6. 壊れた repo config はプロセスを殺さない
default branch の `meguri.toml` が parse / 検証に失敗したら repo 由来 schedule は「無いもの扱い」に
フォールバック（warn + `repo_config.invalid` emit）。host schedule はそのまま発火。個々の schedule 検証
エラーはその1件だけ落として残りを活かす（sweep の per-schedule 失敗隔離と同じ）。

### D7. 検証ロジックの一本化
`Config::validate_schedules` にインラインだった per-schedule ルール（cron parse / body xor body_file /
body_file の repo-relative 安全性 / local mode × `kind=plan` 拒否）を `validate_schedule` 関数に切り出し、
host config load・sweep 時の repo 読み・doctor の三者で共有する。cross-layer の重複 name は D5 で解決する
（この関数の範囲外）。

### D8. doctor を repo schedules に拡張
`doctor_schedules` は host schedules に加え、default branch の repo schedules を読んで cron / body_file を
lint し、host/repo shadow を表示する。doctor が repo 側検証の人間向け面という ADR 0015 の役割は不変。

### D9. `meguri schedules` の表示
`cmd_schedules` を「有効集合（host ∪ repo default branch）＋出所列（host/repo）」に拡張。repo schedule が
黙って発火して CLI には見えない、という観測ギャップを塞ぐ。

### D10. 発火状態のキーは不変
`schedule_state` は `(project_id, name)` キーのまま。host ↔ repo の移動は name が同じなら state を継ぐ
（二重発火・再バックフィルなし）。sqlite migration なし。

### D11. enqueue-only は不変（ADR 0009）
発火は issue（github）/ task（local）を1件作るだけ。pane も run record も作らない。reconciler 化で
ここは一切変えない。

## アーキテクチャ影響 / 代替案

- **影響**: scheduler_fire は既に out-of-band sweep で state を sqlite に持ち、`is_due` は純関数で、
  enqueue-only。したがって本スライスの主眼は**語彙の再構成**（Snapshot / next_step / Step / property test）と
  **二層化**であり、消化ループ（worker / planner）の discover 経路には触れない。poll tick 内の呼び出し位置も
  不変。
- **代替案（schedule 読み取りの出所）**: worktree から読む案／claim pin する案は ADR 0026「却下した代替案」
  参照（run が無い・working tree 依存・bare clone で壊れる、で却下）。
- **代替案（reconciler を作らず現状維持）**: ADR 0012 のスライス移行で「全 Kind が reconciler 経由」を
  成立させるため、Schedule Kind を残す選択肢は無い（スライス4 で旧 `Loop` trait を撤去する前提）。

## migration & rollback

- **データ移行**: なし。`schedule_state` のスキーマ・キー・意味は不変。既存プロジェクトは host config の
  `[[projects.schedules]]` のまま動き、state もそのまま引き継ぐ。
- **前方移行（host → repo への移設手順）**: 運用者が schedule を repo 化するときは、host の
  `[[projects.schedules]]` から同名定義を `meguri.toml` に移して default branch にマージする。**name を
  保てば** state が継続し、切替時に二重発火も取りこぼしも起きない（D10）。過渡的に host と repo に同名が
  並んでも host が勝つ（D5）ので、二重登録による二重発火は起きない。
- **rollback**: repo 化をやめるには `meguri.toml` から schedules を消して default branch にマージ（または
  host 側に戻す）。default branch から消えた瞬間に発見読み取りが空になり、host 定義だけに戻る。sqlite の
  state はそのまま残るため、host に同名で戻せば発火履歴も継続する。コード面の rollback は PR revert のみで
  完結（スキーマ変更が無いので不可逆な残留物なし）。
- **不可逆リスクの評価**: 最悪ケースは「repo に誤った cron を書いて過剰起票する」だが、(a) enqueue-only で
  やることは issue 作成のみ、(b) overlap guard が直近 item が open の間は skip、(c) 反映には default branch
  への commit = 人間マージゲート / branch protection が要る、の三重で緩和される。

## observability

- 既存イベントは不変: `schedule.fired` / `schedule.skipped` / `schedule.failed`。
- 追加イベント:
  - `schedule.shadowed`（`{project, name}`）— repo 側同名が host に負けて落ちたとき（D5）。
  - `repo_config.invalid` — default branch の `meguri.toml` が parse / 検証に失敗し repo schedule を
    無効化したとき（D6、既存イベント名の再利用）。
- `schedule.fired` の payload に出所（host/repo）を足すと、どの層由来の発火かを後から追える。
- `meguri schedules` / `meguri doctor` が有効集合と shadow を表示（D8 / D9）。

## test strategy

- **unit（`src/engine/schedule.rs`）**:
  - `is_due` の既存テスト（窓 / catch-up 折り畳み / no-backfill）を維持。
  - `next_step` の property test（D2）: 全組合せで単一 Step を保証。
  - `Seed` / `Fire` / `SkipOverlap` / `Wait` の分岐ごとの単体。
- **config（`src/config.rs`）**: `RepoConfig` が `schedules` を受理し、host-only キー混入は依然
  parse error（`deny_unknown_fields` 不変）。`validate_schedule` の切り出しが host 検証の既存テストを
  非回帰で通す。
- **統合（`tests/schedule_test.rs`）**:
  - 既存ケース（発火 / catch-up / backfill 抑止 / overlap guard / hot-reload 追加）を `schedule::sweep`
    経由で非回帰（芯1）。
  - repo-side schedule: 実 git worktree の default branch に `meguri.toml` を commit → sweep が発火する
    こと（芯4）。
  - host/repo 同名: host が勝ち `schedule.shadowed` が出ること（D5）。
  - 壊れた `meguri.toml`: repo schedule は無効化されるが host schedule は発火すること（D6）。
  - host ↔ repo 移設で name 一致なら二重発火しないこと（芯3 / D10）。
