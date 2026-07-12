# ADR 0006: issue 投入は capture-first — 投入は LLM を経由せず即成功、整形(refine)は best-effort で寿命モデルの外

- Status: accepted
- Date: 2026-07-12
- Issue: #120
- 関連: ADR 0005(#102, 無ラベル = 未トリアージ)/ #92(issue↔pane↔session 寿命モデル)/ ADR 0003(#64, 役割ベース routing)

## 文脈

meguri は issue を全リソースの寿命の単位にした(#92)。だが issue の投入口は GitHub 側にしかなく、「ちゃんとした issue を書いてラベルを貼る」摩擦が投入点そのものを詰まらせている。issue が生まれなければ何も始まらないので、入口の摩擦はシステム全体のスループット上限になっている。

ADR 0005 で「無ラベル = 未トリアージ」が一義的な意味を持ったことで、**「雑に投げておくだけ」が正当な状態**として存在できるようになった。これを土台に、投入(capture)と整形/トリアージ(refine/triage)を分離できる。

素朴な案は「一言を LLM に渡し、整形された issue を作る」だが、これだと投入が LLM 呼び出しの成否に従属する。投入摩擦を下げる機能自体が、LLM 待ち・agent 不調・CLI 不在で詰まったら本末転倒になる。

## 決定

**issue 投入は capture-first にする。投入(capture)は決して LLM を経由せず、待たせず、失敗させない。AI による整形(refine)はその後追いの best-effort であり、issue↔pane↔session の寿命モデル(#92)の外にある一発の headless 処理として扱う。** これを 4 つの不変条件に固定する。

1. **capture は LLM を経由しない。** issue 作成は `Forge::create_issue` 直で、番号と URL を即座に返す。refine が失敗しても・タイムアウトしても・Ctrl-C されても、issue は raw のまま GitHub に残り、コマンドは capture 成功として報告する(silent に issue が消えることは決してない)。

2. **原文は必ず verbatim で残り、オーサリングの主権を持つ。** 整形後 body の末尾に原文メモを一字一句そのまま保存する。この verbatim 保存は **モデルにではなくオーケストレータが責任を負う**(モデルの出力に原文を含ませるのではなく、meguri がフッタとして必ず付す)。整形は足場であって、意図の権威は原文にある — AI が意図を歪めていないかいつでも照合できる。

3. **capture のデフォルトは無ラベル = 未トリアージ。** watch は拾わない(ADR 0005 の既存動作)。トリアージは後から GitHub 上で `meguri:plan` / `meguri:ready` を貼る通常フローに合流する。急ぎは投入時のフラグで即フェーズ投入できるが、それは明示のオプトインであって既定ではない。

4. **refine は寿命モデルの外の one-shot。** ライブ pane 哲学(README)は「作業ループ」のためのものだ。数秒の整形一発に pane / lane / #92 の寿命モデルや result.json 完了契約を持ち込まない。refine は headless 一発呼び出しで、worktree を作らず、`repo_path` を read-only で読むだけで、書き込み・コミットは一切しない。整形結果は agent の stdout として受け取り、forge への書き戻し(`update_issue_title` / `update_issue_body`)はオーケストレータが行う。

## 帰結

- 投入の CLI(`meguri add`)は run/pane を作らず、mux も store も要らない。必要なのは forge だけ — 寿命モデルの外にいることが構造にそのまま表れる。
- refine が「作業」ではなく「一発の headless 整形」であることは、agent プロファイルに **headless 呼び出しの型**を要求する。それを持たないプロファイルに当たったら refine はスキップし raw のまま残す(silent fallback ではなく一行警告)。この型は将来の他の one-shot 用途(常駐 triage など)でも再利用できる。
- routing(ADR 0003)の役割の集合は、もはや `runs.loop_kind` と厳密には一致しない。`refiner` のような **ループを持たない one-shot コマンドの役割**も routing に載る。「安いモデルで十分な整形」を推奨チェーンの安価側に倒せる。`[routing]` 無しなら default、の既存規律はそのまま。
- 常駐 triage(未トリアージ issue を定期的に AI が下ごしらえする)は将来の別 issue になるが、その土台(capture と triage の分離、headless refine の型)はここで据わる。
