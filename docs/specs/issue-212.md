# issue-212 spec — self-review に findings 台帳と kind を入れ、escalation を挙動化する(slice 1)

親 #211。self-review の cap 落ち(約3割が needs-human)の主因は、reviewer が毎ラウンド
全 diff をゼロから再レビューし新規指摘を出し続ける構造で、収束の意味論が無いこと。
本 slice で「収束とは台帳の open が捌けること」に変え、escalation を回数から挙動へ変える。

設計判断の理由は **ADR 0022**(本 PR 同梱)に置いた。この spec は実装収束用の使い捨て足場で、
実装が landすると消える。

## spec 深度の理由

**design spec**(deeper)を選ぶ。checkpoint(run step の永続 JSON)と `.meguri/self-review.json`
のコントラクトという**永続状態 + 公開契約**に触れるため、veto rule により移行/rollback は必須。
未決定も広い(id 採番の主体、round 2+ のファイル形、in-flight resume の扱い)。

## 触るファイル

| ファイル | 変更 |
|---|---|
| `src/engine/self_review.rs` | `Finding` に `kind`/`id`。台帳(`LedgerEntry`)導入。review turn を round 1 / round 2+ で分岐。fix turn に per-finding 申告(`.meguri/self-review-fix.json`)。ping-pong / decision 異議 / cap→最終fix の分岐。`read_review` の双方向強制。 |
| `src/engine/flow.rs` | `Checkpoint`: `self_review_pending` を `self_review_ledger: Vec<LedgerEntry>` に。`self_review_last_head: Option<String>` と `self_review_final_fix_unreviewed: bool` を追加。`escalate_unconverged` 経路の見直し(cap→最終fix)。`compose_pr_body` / `self_review_details` / `post_self_review_status` を final-fix フラグで分岐(footer / commit status)。 |
| `src/gitops.rs` | 増分 diff 用に `diff_between(worktree, from_sha, "HEAD")` を追加(git 操作は全部ここ)。 |
| `src/config.rs` | `ReviewConfig::max_rounds` の doc comment を実挙動に修正(下記「ついで修正」)。 |
| `src/engine/planner.rs` | `execute_prompt` に「未決定(A か B か)を初回に出し切る」観点を1段落追加。 |
| `docs/adr/0022-*.md` | 本 PR で追加済み。 |

## finding / 台帳 / コントラクト

### Finding(review turn が出す)

```rust
pub enum FindingKind { Defect, Decision }   // serde snake_case

pub struct Finding {
    pub id: Option<String>,   // round 1 は空可(orchestrator が採番)。round 2+ は既存 id を再利用
    pub kind: FindingKind,    // 既定 defect(後方互換)
    pub path: String,
    pub line: u64,
    pub body: String,
    pub lens: Option<String>,
}
```

### 台帳(checkpoint に永続)

**status は reviewer が確定した真実であり、作者の申告(disposition)とは別に持つ**(下記
「status 遷移表」で finding #2 の曖昧さを潰す)。作者の「直した/waive する」は次の review へ渡す
入力であって、それ自体では open を閉じない。

```rust
pub enum FindingStatus { Open, Fixed, Waived }   // reviewer 確定
pub enum Disposition { Fixed, Waived }           // 作者の最新申告(transient)

pub struct LedgerEntry {
    pub id: String,           // orchestrator が採番した安定 id(例: "f1","f2",…)
    pub kind: FindingKind,
    pub path: String,
    pub line: u64,
    pub body: String,
    pub lens: Option<String>,
    pub status: FindingStatus,          // reviewer が確定した状態(omit=解消 / 再掲=open のまま)
    pub author_disposition: Option<Disposition>,  // 直近 fix turn の作者申告。次 review の入力
    pub fix_attempts: u32,    // このエントリを何回 fix turn が「触った(disposition を書いた)」か
    pub waive_reason: Option<String>,  // waive の理由 / decision の決定内容
    pub origin_round: u32,
}
```

### fix turn の申告ファイル `.meguri/self-review-fix.json`(新設・fix 作者が書く)

```json
{ "dispositions": [
  { "id": "f1", "action": "fixed" },
  { "id": "f2", "action": "waived", "reason": "既存の X が同じ検証を持つため重複" },
  { "id": "f3", "action": "fixed", "reason": "A を採用し spec に記録" }
] }
```

- `action ∈ {fixed, waived}` は台帳の `author_disposition` になる(reviewer 確定の `status` とは別)。
  `waived` は `reason` 必須。decision 型は「決定して spec に記録」を `fixed` として申告し、`reason` に
  決定内容を書く(台帳の `waive_reason` に格納)。**この申告は open を閉じない** — 次 review が
  omit して初めて `fixed`/`waived` に落ちる(上記「status 遷移表」)。
- 検証(fix turn 後、orchestrator 側):open な全 id に disposition があること、waived に reason が
  あること、`.meguri/` は git 除外なので tree は clean のまま。欠けたら1回だけ corrective turn。

## 収束ループ(self_review_inner の新形)

```
loop:
  review turn:
    (a) 増分 diff を先に作る: round 2+ は 保存済み self_review_last_head..HEAD を取る
        (round 1 は last_head 未設定なので base 全 diff のみ)
    round 1     → 従来の全 diff レビュー(kind 付き findings を出す)
    round 2+    → 台帳(open + waive 理由 + 決定内容)と「base 全 diff + (a) の増分 diff」を渡す。
                  役割 = 前回指摘の解消確認 + blocking 新規のみ。still-open を同 id で再掲、resolved は omit、
                  新規は id 空。decision は記録確認のみ・再審禁止。
    (b) review turn を回し終えたら self_review_last_head = 現在の HEAD を保存(次ラウンドの起点)
  台帳更新(reviewer 確定のみ status を動かす。下記「status 遷移表」参照):
    - review が再掲しなかった open → 解消。author_disposition==waived なら status=waived、
      それ以外は status=fixed(reviewer が omit=確認)
    - review が同 id で再掲した open → status=open のまま(作者の申告は却下された)
    - 新規 finding → 採番して open で追加
  verdict==needs_human           → escalate(理由 1 / decision 異議 = 3。同じ経路)
  ping-pong: 台帳に fix_attempts>=2 かつ status=open あり → escalate(理由 2)
  台帳の open が 0(=clean)       → 収束・publish(converged)
  rounds >= max_rounds(cap):
    残りは軽微な blocking のみ(ping-pong/decision 異議は上で捌け済み)
      → 最終 fix turn + validate → publish(footer に「最終ラウンドの fix は未再レビュー」)
  それ以外:
    fix turn(作者が per-finding 申告)→ open な各エントリに author_disposition を記録し fix_attempts++
      (status は open のまま。閉じるのは次 review)→ validate → next round
```

- **`self_review_last_head` の更新順**(上の (a)/(b)):増分 diff は「**保存済み**
  `self_review_last_head`..現在の HEAD」で作り、**その review turn を回した後に** `self_review_last_head`
  を現在の HEAD へ進める。先に更新すると diff が `HEAD..HEAD` になり増分が空になる。round 1 では
  `last_head` が未設定(None)なので増分 diff は付けず base 全 diff だけを渡し、round 1 の review 後に
  初めて `last_head` を立てる。fix turn が新しい commit を積むので、次 round の増分は
  「前回 review 時点 → 直近 fix 後」の差分になる。
- **id 採番は orchestrator が持つ**(reviewer/作者に任せない)。round 1 の findings に順に採番、
  round 2+ の新規にも採番。reviewer は round 2+ で既存 id を**再利用**して再掲する(同一性の担保)。

### status 遷移表(finding #2 の曖昧さ解消)

作者の申告だけでは open は閉じない。**fix_attempts は fix turn で加算、status 遷移は review turn だけ**で起きる。

| いつ | 起きること | status | author_disposition | fix_attempts |
|---|---|---|---|---|
| round 1 review が F を出す | 台帳に追加 | `open` | `None` | 0 |
| fix turn が open F を直したと申告 | 申告を記録 | `open`(据え置き) | `Fixed` | +1 |
| fix turn が open F に不同意(waive) | 理由を `waive_reason` へ | `open`(据え置き) | `Waived` | +1 |
| 次 review が F を omit(disposition=Fixed) | reviewer が解消を確認 | `fixed` | — | 据え置き |
| 次 review が F を omit(disposition=Waived) | reviewer が waive を受理 | `waived` | — | 据え置き |
| 次 review が F を同 id で再掲 | 作者の申告を却下 | `open`(継続) | 次 fix で上書き | 次 fix で +1 |
| open F が fix_attempts>=2 のまま再掲 | 本当の ping-pong | `open` → escalate | — | — |

- **waive の再掲が ping-pong で測れる:** 作者が waive → reviewer が同 id で再掲(却下)→ open 継続。
  作者がまた waive(fix_attempts++)→ reviewer がまた再掲 → fix_attempts==2 & open → escalate。
  「同意しない指摘」の水掛け論も回数で人間に渡る。
- **decision の解消:** fix turn は `Fixed`(決定を spec に記録)を申告し `waive_reason` に決定内容を書く。
  次 review が omit すれば `fixed`。異議があるなら再掲ではなく `needs_human` verdict(理由 3)。

## read_review の検証(双方向強制)

- `fixable` かつ findings 空 → reject(「fixable なら最低1件 anchored finding が要る」)。
- `clean`/`needs_human` かつ findings 非空 → reject(既存の clean 側に加え needs_human も)。
- 各 finding は `path`/`line>=1`/`body` 非空 + `kind` が defect|decision。
- round 2+: 再掲 id は台帳に存在すること、omit された open は解消扱いになる旨をプロンプトで明示。

## escalation の挙動化(ADR 0012 の cap 行の置き換え)

- `escalate_unconverged` は「reviewer verdict / ping-pong / decision 異議」専用に残す。
- cap 到達で残りが軽微 → 新関数 `final_fix_and_publish`(最終 fix + validate + footer)へ。

### 最終 fix outcome を checkpoint に永続する(finding #1 の解消)

最終 fix→publish 経路は `self_review` を抜けて `open-pr` へ**戻る**。そこから呼ぶ
`compose_pr_body` / `post_self_review_status` は checkpoint しか読めないので、clean 収束と
「最終 fix 未再レビュー」を checkpoint 上で区別できないと footer/commit status が落ちる。
resume でこの段から再開した時も同じ。だから outcome を永続フラグで持つ:

- `Checkpoint` に `self_review_final_fix_unreviewed: bool`(`#[serde(default)]`)を追加する。
  既存の `self_review_converged: bool` と並置する(enum に畳まないのは #176 の resume 短絡が
  `self_review_converged` に依存しており、置換の blast radius を避けるため。両フラグの意味は直交:
  converged=フェーズ完了・publish 可、final_fix_unreviewed=その publish が未再レビュー fix 込み)。
- `final_fix_and_publish` は最終 fix + validate 後に `self_review_converged = true` と
  `self_review_final_fix_unreviewed = true` を**両方**立てて persist する。converged を立てるので
  ループ冒頭の `if cp.self_review_converged` 短絡が resume を正しく拾い、再レビューしない。
- `compose_pr_body` / `self_review_details` / `post_self_review_status` は
  `self_review_final_fix_unreviewed` を読んで分岐する:
  - footer: `self_review_details_with_outcome` に "最終 fix 未再レビュー · N rounds" を渡す
    (通常は "clean after N rounds")。加えて本文に「最終ラウンドの fix は未再レビュー」の1行を出す。
  - commit status: state は Success のまま、desc を "final fix unreviewed · N rounds" にする
    (通常は "clean · N rounds")。
- **PR body と commit status の期待値をテスト対象にする**(下記テスト戦略 §4 参照)。

## ついで修正

- `src/config.rs` の `max_rounds` doc comment(現状「Once reached, the PR is published as-is」)は
  #176 以降の実挙動でも本 slice の実挙動でもないので、「cap 到達時は挙動で分岐(軽微残は最終
  fix→publish、ping-pong/decision 異議/needs_human は escalate)」に直す。

## planner round 1 の観点追加

- `execute_prompt` に「未決定事項(A か B か)を初回に洗い出し、spec に決めて明記する。後半で
  decision が湧かないよう出し切る」旨を1段落追加。既存の「files/decisions to make」の並びに接ぐ。

---

## アーキテクチャ影響

- self-review の状態が「最新ラウンド findings のスナップショット」から「finding 単位の累積台帳」へ。
  収束判定・escalation・PR footer が台帳を読む単一ソースになる。
- review turn が round 1 / round 2+ で非対称になる(プロンプトと入力 diff が変わる)。fix turn は
  申告ファイルを書くようになる(結果 JSON とは別。`.meguri/` 配下で tree は汚さない)。
- `meguri stats review`(#213)が読む event 名(`self_review.*`)は温存する。cap→最終fix は
  新 event(例 `self_review.final_fix`)を足し、既存の `unconverged` の意味は「本当に escalate した」に絞る。

## 検討した代替案と決定

- **id を reviewer/作者に採番させる:** 却下。ラウンドをまたいだ同一性が壊れやすく、ping-pong 検知が
  不安定になる。orchestrator が採番し reviewer は再掲時に再利用する(§ ループ)。
- **round 2+ を「still-open を全部並べ直す」ではなく「解消/未解消の per-id 判定ファイル」にする:**
  却下(重い)。reviewer が「再掲したものが未解消・omit は解消」という薄い規約で足りる。fix 側だけ
  明示申告を持たせる(waive 理由が台帳に要るため)。
- **cap 到達で常に最終 fix→publish(escalate を全廃):** 却下。ping-pong と decision 異議は人間の
  領分なので残す。挙動で分ける(ADR 0022 §4)。
- **severity を入れて「軽微」を明示フラグにする:** 却下。findings は定義上全 blocking。「軽微」は
  「ping-pong/decision 異議でない残り」で構造的に定義でき、フラグは要らない(ADR 0022 §1)。

## 移行 / rollback(必須)

**永続状態:** checkpoint(`store.update_run_step` の JSON)。in-flight の self-review run が
バイナリ更新をまたぐと、旧 `self_review_pending` を新 `self_review_ledger` が読めない懸念。

- **移行方針:** `self_review_ledger` は `#[serde(default)]` で追加。`self_review_pending` は
  deserialize 可能なまま**残し**、resume 時に「ledger 空 かつ pending 非空」なら pending を
  status=open の台帳へ**昇格**する(best-effort。id は採番、fix_attempts=0)。これで更新直後に
  self-review 中だった run も台帳へ移行して継続できる。次 slice で pending を撤去してよい。
- **決定(採用):** 上記昇格を入れる。self-review は短いフェーズで衝突窓は小さいが、
  「resume でゼロからやり直し」は cap 落ち削減の趣旨に反するため昇格で守る。
- **追加フィールドはすべて `#[serde(default)]`:** `self_review_ledger` / `self_review_last_head` /
  `self_review_final_fix_unreviewed`。旧 checkpoint は既定値(空・None・false)で読め、
  false は「clean 収束扱い」= 従来の footer に落ちるので安全側。
- **rollback:** 本 slice を revert しても、新フィールドは `#[serde(default)]` なので旧バイナリは
  未知キーを無視して読める(serde 既定)。危険な不可逆操作は無い。forge 側の状態は増えない
  (self-review は forge を触らない。escalate 時の draft は ADR 0021 の既存経路)。

## 観測性(observability)

- `store.emit` の event を温存 + 追加:
  - 既存: `self_review.reviewed` / `self_review.fixed` / `self_review.clean` / `self_review.needs_human` /
    `self_review.correction` / `self_review.unconverged` はそのまま。
  - 追加: `self_review.final_fix`(cap→最終fix→publish 経路。`rounds`/`pending` を載せる)、
    `self_review.pingpong`(理由2 の escalate)。decision 異議は既存 `needs_human` に理由を載せる。
- `unconverged` は「本当に escalate した未収束」だけに意味を絞る(#213 の集計が「救済」を
  escalate と誤認しないため)。emit サイトと `meguri stats review` の対応を崩さない。

## テスト戦略

`self_review.rs` の unit tests(`FakeMux`/`FakeForge` + in-memory store)を主に、受け入れ観点を写す:

1. **台帳の永続と resume**:台帳を積んだ checkpoint を serialize→deserialize しても status・
   author_disposition・fix_attempts・waive 理由・決定内容が維持される(+ 旧 `self_review_pending`
   からの昇格)。`self_review_final_fix_unreviewed` も serialize round-trip で維持されることを確認。
2. **status 遷移**(finding #2):open→fix turn 申告(status 据え置き・fix_attempts++)→ 次 review が
   omit で `fixed`/`waived`、再掲で `open` 継続、を遷移表どおり検証。waive 再掲で fix_attempts が
   積まれ ping-pong に届くことも。
3. **round 2+ プロンプト**:台帳(open + waive 理由 + 決定内容)と増分 diff が prompt に含まれ、
   「解消確認 + 新規のみ」「decision は再審禁止」の文言が入る。
4. **ping-pong で escalate**:同一 id が fix を2回経てなお open → `NeedsHuman` + `pingpong` event。
5. **cap + 軽微残 → 最終 fix → publish**(finding #1 込み):ping-pong/decision 異議が無く cap 到達
   → 最終 fix + validate → publish、`final_fix` event、escalate しない。かつ:
   - `self_review_final_fix_unreviewed==true` が checkpoint に persist される。
   - `compose_pr_body` の**出力**に「最終 fix 未再レビュー」footer/本文行が入る(clean 収束の
     出力には入らない、を対で確認)。
   - `post_self_review_status` が渡す desc が "final fix unreviewed · N rounds"(clean は
     "clean · N rounds")である。FakeForge の記録に対してアサートする。
   - 最終 fix 済み checkpoint(converged=true & final_fix_unreviewed=true)で resume すると
     再レビューせず Continue(footer 判定が resume 後も生きる)。
6. **decision finding**:記録(fixed 申告 + spec 反映)で解消される経路と、reviewer が
   `needs_human`(記録済み decision への異議)で escalate する経路。
7. **read_review 双方向**:`fixable`+findings 空、`needs_human`+findings 非空 が reject。kind 既定。

統合テスト(`tests/*.rs` + `fake_agent.sh`)は本 slice では必須にしない(unit で受け入れ観点を
被覆できる)。既存 self-review 統合テストがあれば台帳導入で壊れないことだけ確認する。

## 受け入れ基準(issue 由来)

- [ ] 台帳が checkpoint に永続し、crash resume 後も維持される
- [ ] round 2+ の prompt に台帳と増分 diff が含まれる
- [ ] ping-pong(同一 finding が fix 2回後も open)で escalate する
- [ ] cap 到達 + 軽微残のみ → 最終 fix → publish(footer 記録込み)
- [ ] decision finding:決定記録で解消、異議で needs_human
- [ ] `fixable ⇔ findings 非空` が双方向で強制される
- [ ] planner round 1 に「未決定を出し切る」観点が入る
- [ ] `config.rs` の `max_rounds` doc comment が実挙動に一致
- [ ] `cargo fmt --check` / `clippy -D warnings` / `nextest run` / `test --doc` が通る
