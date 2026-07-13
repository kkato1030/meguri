# ADR 0008: spec と impl を単一のパラメタ化ループに対称化する — 必須の内部 self-review(多角視点)+ 任意の GitHub guard レビュー、検査履歴は会話タイムライン外に置き、auto-merge は guard に gate する

- Status: proposed
- Date: 2026-07-13
- Issue: #132
- 関連: ADR 0003(auto-merge / arm-only)・ADR 0006(AI 実装レビューの内部ループ化)・ADR 0007(merge-watch はドリフト検出)

## Context

ADR 0006 は「AI 実装レビューは内部ループである。人間の merge ゲートが唯一のハードゲート
(backstop)」を前提にした。ADR 0003 の auto-merge は**その人間の merge ゲート自体を外す**
(clean になった PR を無人で確定する)。結果、2 つの穴が同時に開いている:

1. **内部 self-review は「PR を開く前」に 1 回きり。**`Flavor::self_reviews()` を true にして
   いるのは worker だけで、しかも `validate`→`open-pr` の間(= 公開前)にしか走らない
   (`src/engine/flow.rs`, `src/engine/impl_reviewer.rs`)。PR が開いた後に head を動かす
   `ci_fixer` / `conflict_resolver` / `fixer` / 人間の push は self-review を再走させない。
   **実際にマージされる head は、AI がレビューした diff とは別物になり得る。**
2. **auto-merge がその穴を無人で通す。** auto-merger の arm 条件は「未解決レビュースレッドが
   ゼロ」を含むが、誰もスレッドを立てなければ自明に真になる。AI にも人間にもレビューされて
   いない最終 head がそのまま arm → マージされ得る(`src/engine/auto_merger.rs`)。

加えて、#108/#109 で名前だけ対称化された `spec_reviewer` / `impl_reviewer` は、**挙動が非対称**の
ままだった:

|            | 内部 self-review | GitHub レビュー |
|------------|------------------|-----------------|
| **plan(spec)** | ❌ なし(planner はただ exec) | ✅ spec_reviewer(**必須ループ**) |
| **impl**       | ✅ あり(**必須**、ADR 0006) | ❌ なし(内部化した) |

spec と impl は本質的に同じ「exec → レビュー → 収束 → 公開」を回しているのに、レビューの
生え方が左右で真逆になっている。この非対称が、上の穴を「どちらか片方でしか塞げない」構造を
生んでいる。

## Decision

spec と impl を**挙動レベルで対称化**する。両方が「必須の内部 self-review(多角視点)+ 任意の
GitHub guard レビュー」を持つ、単一のパラメタ化ループ `kind = Plan | Impl` にする。

```
1. human : label plan / ready
2. ai    : exec(kind)                 … plan→ADR/spec 文書 / ready→コード
3. ai    : self-review → self-fix ×N  … 必須・多角視点(N レンズ/round、clean か cap まで)
4. ai    : create PR
5. ai    : (optional) guard review     … 独立レビュー、commit status + PR 本文 <details>
6. merge : human=advisory / auto-merge=gate
```

### 1. exec / self-review / guard を kind でパラメタ化する(挙動の対称化)

- **`exec(kind)`**: 1 つの exec テンプレを `kind = Plan | Impl` で分岐する(既存 `Flavor` を流用)。
- **共有 self-review は必須で、spec 側にも適用する。**`Flavor::self_reviews()` を planner でも
  true にし、既存の `flow` 内部 self-review を「多角視点(N レンズ)」へ拡張する。レンズ既定は
  `correctness / tests / simplicity / security`(config で増減可)。spec の self-review は文書の
  正確さ・完全性・決定の妥当性を見る(コード用レンズは kind=Plan では文書観点に読み替える)。
- **`guard(kind, optional)`**: `kind` 付きの独立レビューコンポーネント 1 つ。現行 `spec_reviewer`
  はここへ格下げ(= guard(Plan))、impl も同じ口(guard(Impl))を得る。有効/無効は選べる。

この畳み込みの**副作用として spec レビューも「任意」になる**(今は必須ループ)。要件どおり、
kind=Plan の guard を ON なら現行相当、OFF なら内部 self-review だけで運用する。

### 2. 内部 self-review は必須、GitHub guard は任意(要件 1・2)

内部 self-review は品質の下限を **PR を開く前**に引き上げる、ADR 0006 の内部ループをそのまま
拡張したもの — forge を一切触らず、収束はローカルのラウンドカウンタで縛る。**必須。**

GitHub guard は**開いた後の PR**を独立モデルで見る外部レビュー。**有効化/無効化を選べる**
(project × kind 粒度)。guard(Impl) を ON にすれば、self-review が見た diff と実際の head が
乖離しても(ci_fixer/conflict_resolver/人間の push があっても)、最終 head が独立レビューされる
— これが穴 1 を塞ぐ。

### 3. guard の出力は commit status + PR 本文の折り畳みのみ。**inline スレッドにはしない**

ADR 0006 が inline 実装レビューを内部化した理由(reviewer↔fixer の AI 同士 ping-pong と forge
チャタ)を再燃させない。guard は:

- **commit status** を head sha に貼る(`meguri/guard-review`)。
- **PR 本文の折り畳み `<details>`** にラウンド要約(回したレンズ / 指摘数 / 解消可否)を書く。

`create_pr_review`(inline スレッド)は**使わない**。fixer の discover は未解決レビュースレッド
を拾う(`thread_awaits_fixer`)ので、guard が inline を出せば fixer が反応して ping-pong が
戻る。したがって guard(Impl) は旧 `impl_reviewer`(inline + fixer 連結)とは**別物**であり、
ADR 0006 の「AI 実装レビューは内部ループ」を破らない — guard はサマリのみの任意の上乗せである。

### 4. 検査履歴は「会話タイムライン外」に置く(ADR 0006 の「会話は人間専用」を割らない)

- (a) commit **status** `meguri/self-review`・`meguri/guard-review`(gh ユーザートークンで貼れる、
  `POST /repos/{repo}/statuses/{sha}`)。粒度は最終 verdict の一行(`clean · 2 rounds` 等)。
- (b) PR 本文の折り畳み `<details>`。ラウンドごとの要約。**生の全トランスクリプトは載せない**
  (sqlite events・pane に既にある)。
- local モードは sqlite events(`meguri logs`)のみ(forge が無い)。
- check-run(リッチだが GitHub App 必須)は将来枠。

status も本文 `<details>` も**会話コメントではない**ため、ADR 0006 の「PR 会話は人間・外部
レビュー専用」を割らない。

### 5. human=advisory / auto-merge=gate、ただし ADR 0007 のデッドロック罠を避ける

- **human マージ = advisory。** guard 失敗は赤チェック(`meguri/guard-review` = failure)+ 本文
  指摘を出すが**止めない**。厳密ゲート化したい利用者は GitHub のブランチ保護で
  `meguri/guard-review` を required check に指定する(meguri は二重判定しない — ADR 0003)。
- **auto-merge = gate。** auto-merger の arm 条件に「該当 kind の guard が有効なら
  `meguri/guard-review` が success」を足す。**失敗 → arm せず `meguri:needs-human`**。
  ただし ADR 0007 の教訓に従い:
  - guard が**無効**なら条件を課さない(存在しない status を要求してデッドロックさせない)。
  - guard 有効だが status が**未到達/pending** なら **no-op で次掃引にリトライ**(escalate しない)。
  - **明示的な failure のときだけ** needs-human。
- **ci-fixer は `meguri/*` の commit status を「直せる CI」に数えない。**`meguri/guard-review` の
  failure は rollup 上 Failure に見えるが、ci-fixer が拾うと直せる失敗ログが無く空振り(かつ
  advisory を needs-human へ誤昇格)する。ci-fixer の fixable 判定から `meguri/` 接頭辞の status
  context を除外する。required でない guard 失敗は GitHub 上 `UNSTABLE`(マージ可能)なので、
  human マージも native auto-merge も止めない = advisory の担保。

### 6. plan 経由の納品は既定 2 本(`plan_delivery = separate`)、combined は morph 型で温存

- **`separate`(既定・新設)**: ADR/spec PR は独立の PR として review → **マージ**され、その後
  実装が別 PR で続く。受け渡し: **ADR PR マージ → issue を `speccing` → `ready` へ自動張替 →
  worker の exec が拾う。** そのため separate の spec PR は `Closes #N` を**使わない**
  (マージで issue を閉じてはいけない)— `Refs #N` 等の非クローズ参照にし、マージ検出の掃引が
  `speccing → ready` を張り替える。
- **`combined`**: 現行の morph 型(spec-worker が同一ブランチを takeover して spec+実装を 1 PR に、
  ≒ #98)。`spec_worker` は combined のときだけ活きる。
- 設定名は `ProjectMode`(github/local)に相乗りさせない独立キー。per-issue 上書きは後回し。

## Consequences

- レビューの生え方が spec/impl で対称になり、「開いた後の head を誰もレビューしない」穴が
  guard(Impl) + auto-merger gate で塞がる。auto-merge を使う運用は impl guard を ON にすることで
  ADR 0003↔0006 の隙間を閉じられる(mechanism を用意し、閉じるかは運用者の選択)。
- spec レビューが必須ループから任意 guard に格下げされる。既定は guard(Plan)=ON なので既存
  挙動は保たれるが、ラベル状態機械の「spec-ready が spec-worker の takeover を駆動する」性質は
  `combined` に限定され、`separate` では「ADR PR マージ → ready 張替」に変わる。
- 検査履歴が status + 本文 `<details>` に残り、`meguri top` / GitHub UI から追える。会話は汚さない。
- 新しい二重判定は増やさない: required 判定は GitHub、guard の advisory/gate 分岐だけを meguri が
  持つ。auto-merger の新条件は「guard が有効かつ failure」でのみ escalate する保守的な形。
- guard(Impl) は inline を出さないので fixer は無変更のまま。ci-fixer にだけ `meguri/*` status
  除外という小さな変更が要る。

## Out of scope(将来枠)

- check-run 化(GitHub App 前提のリッチ表示)。
- guard レビュー観点の per-issue カスタム / per-issue の plan_delivery 上書き。
- separate モードで spec/ADR PR まで auto-merge する(既定は spec-ready を blocking のままにし、
  ADR のマージは人間が握る)。
- 外部レビュー bot との重複抑制(guard(Impl)=OFF で足りる)。
