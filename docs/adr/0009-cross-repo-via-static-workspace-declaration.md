# ADR 0009: repo 跨ぎは実行系を拡張せず、config の静的 workspace 宣言だけで表現する

## ステータス

採用(issue #154)

## コンテキスト

#134(decompose)は複数 repository にまたがる分解を明示的に「やらないこと」に置いた。
一方 meguri がいま扱いづらい長期・大型の作業 — repository の分割/統合、API とクライアントの
連動変更、repository 設計込みの greenfield — は本質的に repo を跨ぐ。

2026-07-13 の設計検討で得た観察は、**跨ぎ対応に必要なのは実行系の拡張ではなく、
「どの repository 群がひとまとまりか」という静的な宣言だけ**である、というものだ。
その根拠は既存機構が既に跨ぎを担える形になっていること:

1. **実行順の制御は既存の dependency gate がそのまま担える。** discovery は
   GitHub ネイティブの `blocked_by` を唯一の真実として issue をスキップする
   (`src/tasks.rs` の `has_unresolved_blockers`)。この gate は blocker の
   `state`/`state_reason` だけを見て「completed で閉じた blocker のみ解決」と判定し、
   **読めない blocker(`Err`)を未解決として安全側に倒す**。cross-repo でも blocker の
   状態さえ取れれば判定則は不変で、取れなければ安全に止まる。
2. **run は単一 repo・単一 worktree・単一 branch のまま。** 複合 worktree や PR セット、
   repo 跨ぎの原子的 deliverable は作らない。跨ぎは「別々の run が別々の repo で回り、
   dependency graph が順序を与える」で表現できる。
3. **meguri が実行できない不可逆操作(`gh repo create`、公開設定、履歴書き換え等)は
   人間ノードで表現できる。** ラベルなしの子 issue = 未トリアージ = 人間の作業、という
   既存セマンティクスがそのまま「人間が閉じるまで依存側を止める DAG ノード」になる。
   専用の ops 実行モードや承認ゲート機構は要らない。

## 決定

**project の上に `workspace` = 関連 project の静的グルーピングを config にだけ導入し、
実行系(run / turn)には一切現れさせない。** workspace の用途は次の 3 つに限定する:

1. **decompose の起票スコープ**(#134): planner が子 issue を作ってよい repository の範囲。
2. **cross-repo blocker の解決範囲**: discovery が他 repo の blocker を解決しに行く相手を
   workspace 内の sibling に限定する。
3. **表示のグルーピング**: `meguri ps` / `meguri top` を workspace 単位で束ねる
   (ADR 0005 の per-project mux workspace の表示上の延長)。

### 不変条件

- **実行系に現れない。** worktree・pane・branch・検証契約・task の home は無変更。
  workspace は discovery/decompose/表示という周縁の「範囲」概念にとどまる。
- **状態を持たない。** sqlite にテーブルを作らない。config の静的宣言が全て。
  ADR 0003(TaskSource: task はホスト間を移動し run は pin される)の語彙で言えば、
  workspace は「repo の恒久的まとまり」であって、分解の親 issue のような
  「仕事の一時的まとまり」とは別の層に属する。

### スコープ拡大権限はホスト運用者に限定する(セキュリティ上の分界)

起票スコープを issue body 側で宣言させる案は**採用しない**。issue body は agent への
プロンプト入力であり、write 権限者が「この goal を実行せよ」と書けてしまう。もし body で
スコープを広げられれば、write 権限者はホストの `gh` token が届く任意の repo へ issue を
撒く goal を書けることになる。

スコープを **config 側(`[[workspaces]]`)に固定**することで、meguri のラベルゲートと
同じ責任分界を保つ:

- **実行させられる人 = write 権限者**(ラベル付与 / issue 起票ができる)。
- **スコープを決める人 = ホスト運用者**(config.toml を編集できる)。

decompose が子 issue を起票してよい repo は、親 issue が属する project とその workspace の
sibling に**厳密に限定**される。workspace に属さない project の issue を decompose しても、
起票先は自 repo のみ(既存挙動と不変)。

### 人間ノードは既存セマンティクスで表す

不可逆操作は decompose の子として「トリガーラベルを持たない issue」で表す。discovery は
トリガーラベル(`meguri:ready` / `meguri:plan`)を持つ issue しか拾わないので、ラベルなしの
子は自動的に「人間が手を入れ、閉じるまで依存側を止める」ノードになる。assignee 等の追加
シグナルは必須にしない — ラベルの不在それ自体が「未トリアージ = 人間の作業」の既存の合図
だからだ。

## 帰結

- cross-repo 対応の追加コードは「config の宣言」「decompose の起票先を sibling に広げる配線」
  「cross-repo dependency の書き込み」「表示のグルーピング」に閉じる。scheduler・worker・
  reaper・検証・deliver といった実行系のホットパスには一切触れない。
- workspace を定義しない既存 config の挙動は完全に不変(概念は opt-in)。
- 跨ぎの「原子性」は提供しない。API とクライアントを一斉に変えるような変更は、
  dependency graph で順序づけられた複数 PR に分解され、各 PR は単独でマージ可能・レビュー
  可能でなければならない。これは制約であり、同時に「小さく安全な単位に割る」強制でもある。
- GitHub の cross-repo issue dependencies が期待通り機能するか(blocker 状態の inline 取得、
  cross-repo な `blocked_by` の設定可否)は比較的新しい機能で、実装時に実地検証を要する
  (spec の論点参照)。ただし検証が外れても既存の安全側フォールバック(`Err = 未解決`)が
  効くため、最悪でも「跨ぎ blocker で過剰に止まる」side にしか倒れない。
