# ADR 0024: 外部モデルの self-review findings は author への prompt injection 面である — 明示し、fix の waive 裁量を緩衝と位置づける

- Status: proposed
- Date: 2026-07-16
- Issue: #214(親: #211)
- 関連: ADR 0023(round 1 並列 reviewer)・ADR 0022(findings 台帳・fix turn の waive)・ADR 0021(escalate 時 needs-human draft を forge に publish)・ADR 0011(二層 config・信頼の宣言は host 専用)・ADR 0006(self-review は happy path で forge を触らない内部ループ)

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
- **爆発半径の限定 — ただし forge に出ない訳ではない。** self-review は happy path では forge を
  触らない内部ループ(ADR 0006)で、成果物は worktree 内の commit のみ。通常 PR として公開される
  中身は tree clean + base より進行 + `check_command` + human merge gate を通る。
  **例外は escalate-time の needs-human draft(ADR 0021)である。** self-review が `needs_human` で
  エスカレートし branch が base より進んでいると、`publish_needs_human_draft` が**未再レビューの
  commit を含む draft PR を forge に publish する**。外部 finding に誘導された author の commit も、
  この経路なら merge 前に forge 上で可視になりうる。したがって「injection 起点の commit は forge に
  一切出ない」とは言えない — これは残余リスクとして正直に記録する(下記 Consequences)。
- **draft 経路でも守られている線。** それでも publish されるのは **draft**・**needs-human ラベル付き**で、
  ADR 0021 が「人間が見るための証拠物件」と位置づけたものである。auto-merge されず、human merge gate を
  越えない。injected commit は「人間の目の前に draft として置かれる」のであって「検証を抜けて本公開される」
  のではない。可視化(exposure)は起きるが、無人での merge は起きない。
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
- **残余リスクは受容する。** 緩衝は author の判断に依存し、決定的な遮断ではない。加えて
  escalate-time の needs-human draft(ADR 0021)経路では、injection に誘導された未再レビュー
  commit が draft PR として forge 上に**可視化されうる**(auto-merge はされない)。この exposure を
  隠さず残余リスクとして受容する。将来、外部 finding 起点の誘導が実測されたら、body の sanitize・
  信頼度別の扱い・draft publish 前の追加ゲートなどを後続で足せる(ADR 0020 の「実行時は union、
  取捨は人間がオフライン」と同じく、まず観測してから絞る)。
