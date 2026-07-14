# ADR 0012: guard(Plan) の findings は `spec_fixer` が planner author lane で駆動する

- Status: proposed
- Date: 2026-07-14
- Issue: #188
- 関連: ADR 0006(guard は inline を出さない)・ADR 0008(spec/impl 対称化: findings は次 push 待ち)・ADR 0005(二軸ラベル)

## Context

ADR 0008 は guard(Plan) の findings を「`meguri:spec-reviewing` を維持し、次の push で
head が動けば再 guard する」と定めた。だが **その push を打つ主体をどのループにも割り当てて
いなかった。**

impl 側には対称物がある: 人間・外部 bot の review スレッド → `fixer` が author lane を
再駆動して push する。plan 側は guard の findings が commit status(`meguri/guard-review =
failure`)と PR 本文の折り畳み `<details>` に出るだけで、`fixer` はスレッド(人間/bot)しか
見ず、guard は ADR 0006 に従って inline スレッドを出さない。**結果、findings を拾う主体が
誰もいない。**

実測(2026-07-14): open 中の spec PR 10 本すべてが guard 一回目の findings で park し、
最長 2 日近く停止していた。guard(Plan) が「全 spec が必ず一度は通る人間ゲート」として
機能してしまい、ADR 0008 の「guard は任意の外部ゲート」という位置づけと矛盾している。

## Decision

**plan 側にも fixer 系ループを生やす。impl 側の `ci_fixer`(赤 CI を拾う)と対称に、
guard(Plan) の findings を拾う `spec_fixer` を追加する。**

- **discover**: `meguri:spec-reviewing` の open PR のうち、**現在の head** の
  `meguri/guard-review` commit status が `failure` のもの(かつ hold/working/needs-human
  でない)。
- **drive**: canonical issue でキーし、**author lane(planner と同一 pane/session)**を
  継続する。PR 本文の guard `<details>` の findings を読み、spec/ADR を修正して commit する。
  push は meguri が行う。
- **収束は head sha が担う**: push 後の新 head には guard status がまだ無い。よって
  spec_fixer の discover 条件(head の guard-review = failure)は偽になり、再発火しない。
  一方 guard は「status 未貼りの head」を候補にするので新 head を自動で再レビューする。
  clean なら `spec-ready` へ、findings なら `spec-reviewing` のまま次ラウンドへ。hidden
  marker も追加の状態も要らない — dedup キーは head sha そのもの。
- **ラウンド上限 ≤3**: 同一 issue の spec_fixer 成功回数(`succeeded_run_count`)で数える。
  `ci_fixer` の `MAX_CI_FIX_RUNS` と同じ仕組み・同じ値。超過は `meguri:needs-human`
  (#153 / ADR 0009 の awaiting_human と合流)。

### なぜこの形か(検討した代替)

- **guard を self-fix にする**: guard は「独立レビュー」であり修正主体ではない(ADR 0008)。
  レビューと修正を同一 lane に畳むと視点の独立性が失われる。却下。
- **既存 `fixer` に本文 findings も読ませる**: `fixer` は「未解決スレッド」を状態源にする
  設計で、収束(reply marker で park)もスレッド前提。findings(status + 本文)は別の状態源で、
  混ぜると両方の収束条件が絡む。impl 側で CI を `ci_fixer` に分離したのと同じ理由で分離する。
- **spec_fixer を独立の新ループにする(採用)**: `ci_fixer` を素直に写せる。plan/impl の
  レビュー→修正が構造的に対称になる。

### ping-pong / 発散への防御

guard は inline を出さない(ADR 0006 不変条件)ので `fixer` 型の無限往復は構造上起きない。
残る発散は「guard が毎回別の findings を出し続ける」ケースだが、これは ≤3 ラウンドの上限で
needs-human に落ちて止まる。

## この決定が確立するドメイン不変条件

- **plan 側 guard findings の駆動主体は `spec_fixer` である**(impl 側の `ci_fixer`/`fixer`
  と対称)。findings で park した spec PR は、人手なしで次 poll 以内に修正 turn に入る。
- **修正は planner と同じ author lane で継続する**(ADR 0004 の lane モデル / 文脈保持)。
- **収束は head sha で dedup する**: 新 head に guard status が貼られるまで spec_fixer は
  再発火しない。ラウンドは issue 単位で ≤3、超過は needs-human。

## Consequences

- guard(Plan) が「必ず通る人間ゲート」ではなくなり、ADR 0008 の「任意の外部ゲート」という
  位置づけと一致する。
- spec 先行パイプラインが guard 一回目の findings で全量停止する構造が解消される。
- delivery mode(combined/separate)非依存: `spec-reviewing` は `spec-ready` 分岐より前の
  段階なので、どちらのモードでも同じく動く。
- ループが 1 つ増える。discover は候補 spec PR ごとに `commit_status` を 1 回叩く
  (`ci_fixer` の rollup poll と同じ粒度)。PR 一覧は per-tick キャッシュを共有する。
- #153 とは補完関係: 本 ADR は「まず自動で直す」側、#153 は「上限超過や clean-park を
  人間に通知する」側。両者の needs-human 経路は awaiting_human に合流する。

## Out of scope

- guard が同じ findings を反復検出しているか(発散 vs 未修正)の内容差分判定 — 上限で足りる。
- per-issue のラウンド上限上書き。
