# ADR 0024: 外部モデルの self-review findings は author への prompt injection 面である — 明示し、fix の waive 裁量を緩衝と位置づける

- Status: proposed
- Date: 2026-07-16
- Issue: #214(親: #211)
- 関連: ADR 0023(round 1 並列 reviewer)・ADR 0022(findings 台帳・fix turn の waive)・ADR 0006(self-review は forge を触らない内部ループ)

## Context

ADR 0023 で `[[review.reviewers]]` に claude 以外の profile(codex / grok 等)を並べられる
ようになる。self-review の findings は台帳に積まれ、その `body` は**そのまま** author の
fix prompt に列挙される(`fix_prompt`)。

これまで reviewer は self-reviewer profile 一択(多くは author と同系のモデル)だった。
異種・外部モデルを混ぜると、**外部モデル → author** という新しい信頼境界が生まれる。
外部 reviewer が出力する finding body は、実質 author への指示テキストとして prompt に入る。
悪意ある、あるいは単に暴走した外部モデルが finding body に「これまでの指示を無視して
〜せよ」の類を混ぜれば、author の fix turn を誘導しうる — prompt injection 面である。

この面は明示しておかないと、モデルを1本足す設定変更の裏で静かに開く。

## Decision

**この信頼境界を存在するものとして明記し、既存の緩衝を設計上の防御と位置づける。実行時に
外部 finding body を sanitize する機構は今回は入れない。**

- **injection 面の明示。** 外部 reviewer の finding body は author の fix prompt に無検閲で
  入る。これは `[[review.reviewers]]` に非 self-reviewer profile を置いた時にのみ開く面で、
  未設定なら存在しない。
- **`[[review.reviewers]]` は host-only。** どの外部モデルを reviewer に据えるかは信頼の宣言なので、
  ADR 0011(二層 config)の「信頼の宣言は host 専用」に従い host `config.toml` にのみ書ける
  (repo `meguri.toml` からは指定不可 = `RepoConfig` に入れない)。run 中の agent が自分の
  worktree から reviewer 編成を書き換えて、より緩い/悪意ある外部モデルを注入することを防ぐ。
- **緩衝は fix turn の waive 裁量。** ADR 0022 の「同意しない finding は直さなくてよい
  (waive・理由必須)」は、author が finding を**無条件では実行しない**ことを意味する。
  author は finding を「直す指示」ではなく「検討対象の指摘」として扱い、同意できなければ
  理由付きで却下する。これが injection に対する第一の緩衝である。
- **爆発半径の限定。** self-review は forge を触らない内部ループ(ADR 0006)で、成果物は
  worktree 内の commit のみ。最終的な公開は tree clean + base より進行 + `check_command`
  + human merge gate を通る。外部 finding が author を誘導しても、これらの検証と人間の
  merge を越えて公開されるわけではない。
- **sanitize は入れない(今回)。** finding body の機械的フィルタは、正当な指摘の表現も削り
  かねず(false positive)、recall を落とす。境界の存在を明示し緩衝を設計に組み込むことを
  優先し、能動的 sanitize は必要が実測されるまで持ち越す。

## Consequences

- **モデルを混ぜる設定のリスクが文書化される。** `[[review.reviewers]]` に外部 profile を
  置く判断は、この injection 面を承知の上での判断になる。信頼できる profile を選ぶ責任は
  設定者にある、と明示される。
- **waive 裁量が単なる同意機構でなく防御として位置づく。** ADR 0022 では「較正の緩衝」
  だった waive が、本 ADR で「injection の緩衝」でもあると二重に意味を持つ。fix prompt が
  finding を「指示」ではなく「指摘」として提示する語り口は、この防御の一部として維持する。
- **残余リスクは受容する。** 緩衝は author の判断に依存し、決定的な遮断ではない。将来
  外部 finding 起点の誘導が実測されたら、body の sanitize や信頼度別の扱いを後続で足せる
  (ADR 0020 の「実行時は union、取捨は人間がオフライン」と同じく、まず観測してから絞る)。
