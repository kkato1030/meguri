# ADR 0006: AI 実装レビューは「内部ループ」である(GitHub は人間レビューにだけ残す)

## ステータス

採用(issue #108)。**ADR 0004(AI レビューは spec と実装 diff の両方を対象にする)を置換する。**
0004 の「meguri は自分の実装 diff にもレビューを生成する」「人間の merge が唯一のハードゲート」
「閉ループには構造的な栓を義務付ける」という骨子は残る。変わるのは **その閉ループを回す場所**
だけ — forge 上(外部ループ)から run の worktree 内(内部ループ)へ移す。

## コンテキスト

meguri のループは今すべて **外部ループ**である: discover を forge から取り、成果と状態を
forge に settle する。transport(findings を運ぶ)も state(何が済んだか / 今どっちの番か /
何ラウンド目か)も forge 上に置く — これが looper の "Authority" 原則で、restart / multi-host
耐性と、人間・外部 bot・meguri-AI が **完全に同一経路**を流れる統一性を生んでいる。

AI 実装レビュー(ADR 0004 の impl-reviewer v0)をこの枠で回すと、GitHub が 3 つの役割を
同時に背負う:

| 役割 | 内容 | AI↔AI で必然か |
|---|---|---|
| transport | `impl_reviewer` → `fixer` に findings を運ぶ | ❌ 両者とも meguri 内部 |
| state | 「この head はレビュー済み」「今どっちの番」「何ラウンド目」 | ❌ 2 ループが握手するから要るだけ |
| display | 人間が PR 上で AI の指摘と対応を追える | ⭕️ ただし人間が見るのは最終形で十分 |

代償は実在する:

- ポーリング毎の PR / thread / CI 取得と多数の書き込み(API・レート消費)。
- `impl_reviewer` 投稿 → 次周回で `fixer` discover → push+reply → 次周回で再 discover という、
  **ラウンドあたり両側スケジューラ 1 周分**のレイテンシ。
- **人間が PR を開くと、AI 同士の往復(inline thread + summary + 返信)の壁が先に見える** —
  GitHub が本来得意な人間レビュー体験を AI の会話が汚す。
- findings が diff の NEW 側の行に anchor できないと `create_pr_review` が弾く脆さ。

観察: **人間レビューが GitHub を通るのは必然(人間もマージゲートもそこに居る)。だが AI↔AI の
レグが GitHub を通るのは付随的で、本質ではない。** 握手が 2 つの独立ループの周回をまたぐから
forge に state が要るのであって、握手を畳めば state も transport も要らなくなる。

## 決定

### 1. ループを「外部 / 内部」の 2 クラスに分ける

- **外部ループ**(現状すべて): forge を discover 源・transport・state とする。人間や外部 bot と
  同じ土俵で回る。`planner` / `spec_reviewer` / `worker` / `spec_worker` / `fixer` /
  `conflict_resolver` / `ci_fixer` / `cleaner`。
- **内部ループ**(新設): run の worktree の中で回る。対象はローカル diff、transport/state は
  run の checkpoint、**forge に一切触らない**。収束は forge マーカーではなくローカルのラウンド
  カウンタで縛る。durable な出力は commit(元から真実)だけ。

**AI 実装レビューは内部ループである。**

### 2. impl review を worker の「公開前フェーズ」に embed する

共有フロー(`engine/flow.rs`)の `validate` と `open-pr` の間に self-review フェーズを挿す:

```
execute(実装・commit) → validate(project check) → self-review(内部ループ) → open-pr(push・PR)
```

self-review は同一 run 内で review→fix を回す。review turn がローカル diff を読み findings を
書き、fix turn が潰して commit・再 validate、clean かラウンド上限まで繰り返す。ラウンド上限で
未収束でも **block せず publish する**(0004 と同じ思想 — レビュー済み PR が人間ゲート前で開いて
いるのは正常状態)。「N ラウンド未収束」は run イベントと PR 本文フッタ 1 行にだけ残す。

**forge 呼び出しはこのレグにゼロ。** thread も comment もポーリングもない。findings は
checkpoint 経由でメモリ内を渡り、GitHub には出ない。中断・再開は他ステップと同じく
checkpoint から。これは Authority を破るどころか **強める** — forge にも local(sqlite)にも
新しい state を足さず、真実は push 済み commit のまま。

フェーズにするかは Flavor フック(`Flavor::self_reviews() -> bool`)で切り替える。worker のみ
true。fixer への適用(人間コメント対応後の再自己レビュー)は将来の選択肢。

### 3. GitHub は人間・外部レビューにだけ残す — fixer は温存

`fixer` は変えない。AI が thread を作らなくなるので、fixer の discover は自然と
**人間・外部 bot の thread だけ**を拾う。GitHub をレビュー transport に使うのは「人間が居る側」
に限定され、そこでは GitHub がまさに正しく効く。外部レビュー bot 環境との互換もこの経路で
保たれる(旧 `review.impl_enabled = false` の役目は「外部 bot がいるなら自己レビューを切る」に
引き継がれる)。

### 4. 人間の merge が唯一のハードゲート(0004 から不変)

自己レビューは PR を block しない。approve も request-changes もしない(そもそも forge に
出ない)。品質はゲート**前**で上がる。判断の重心は人間のまま。

### 5. モデル分離を保つ

「自分の diff を自分でレビュー」は弱い。routing の `impl-reviewer` role を残し、review turn は
その profile で解決する。内部ループでもモデル分離は保てる。

## 帰結 / トレードオフ

- **得るもの**: review→fix が run 内で閉じ、人間には自己レビュー済み PR が最初から届く。
  PR 会話は人間・外部レビューだけの綺麗な場に。forge API/レート消費減、ラウンドあたりの
  レイテンシがスケジューラ 2 周 → 1 run 内に。
- **失うもの**:
  - PR 上の AI findings 監査ログ → フッタ 1 行 / run イベントで代替(往復トランスクリプトは
    載せない)。
  - impl review の独立スケジュール/優先度 → worker slot が review+fix 分だけ長く占有。多量
    並行時のスループット特性が変わる。
  - 「他人(人間)の PR を AI レビュー」能力 → 旧 `impl_reviewer` も meguri ブランチしか触って
    いなかったので実質ゼロ。
- **命名の対称化**: `reviewer`(無印なのに spec 専用)→ `spec_reviewer`(外部/GitHub)、
  `impl_reviewer` は内部ループの実装に再分類。対称ペア `spec_reviewer` / `impl_reviewer`。

## スコープ外

spec レビュー(`spec_reviewer`)は外部ループのまま残す。`spec-reviewing` / `spec-ready`
ラベルが `spec_worker` のゲートを兼ねるため、内部化は別 issue で扱う。
