# ADR 0015: repo 側ファイルの読み取りは「助言/発見」用途と「完了契約の pin」用途で出所を分ける

- Status: proposed
- Date: 2026-07-14
- Issue: #194

## コンテキスト

ADR 0011 / #165 は「repo 側の宣言は default branch から読む」という宣言セマンティクスを定めた。
しかし実装には、primary clone の **working tree を直接読む近似**が残っている
(`scheduler_fire` の `body_file`、`doctor` の repo config / preamble / body_file 検証)。
working tree は checkout 状態や未 commit 変更で default branch と乖離しうるし、後続の managed
bare clone 化では working tree 自体が無くなる。

ここで一見矛盾がある。ADR 0011 は「trusted ref(`git show origin/<default>:…`)から読む」案を
**明確に却下**した。ならば本 issue が `git show` に寄せるのは ADR 0011 への逆行ではないのか。

## 決定

repo 側ファイルの読み取りを**用途で二分**し、出所を分ける。

> **完了契約に効く「pin」読み取りは run の worktree から読み claim 時に固定する(ADR 0011)。
> 完了契約に効かない「助言 / 発見」読み取りは default branch から読む(`git show`)。**

| 用途 | 例 | 出所 | 理由 |
|---|---|---|---|
| pin(完了契約) | run flow の repo `meguri.toml`(`flow.rs:442`)、実行時 preamble(`flow.rs:1323`) | run の **worktree**(claim 時 pin) | ADR 0011。改竄が PR diff に現れ監査可能。ref 分離が無い現行アーキで「default から読む」は偽の安心 |
| 助言 / 発見 | schedule の `body_file`(`scheduler_fire`)、`doctor` の repo config / preamble / body_file 検証 | **default branch**(`git show origin/<default>:<rel>`、remote 無しは local `<default>`) | run に紐付かず、完了契約にも効かない。宣言セマンティクスどおり「マージ済みの内容」を映すのが正しい |

ADR 0011 が `git show` を却下したのは、**敵対的 agent が共有 git dir 越しに `update-ref` で
trusted ref を改竄でき、それが完了契約の検証を素通りする**という一点に対してだった。助言 / 発見
読み取りは:

- run の worktree に紐付かず、走行中の agent が介在しない(scheduler は run が存在しない発見時、
  doctor は run を持たない CLI lint)。
- 完了契約(`check_command` / clean tree / commits-ahead)に一切効かない。

したがって「default branch を improve できる敵対者」を守る対象に含めない。ここで守りたいのは
**「working tree の checkout 状態や未 commit 編集で、宣言と実挙動がズレる」**という失敗モードで、
それには「マージ済みの内容を読む」が素直な正解になる。ADR 0011 の脅威モデルとは守る対象が違う。

## 帰結

- `scheduler_fire` の `body_file` と `doctor` の 3 箇所は、default branch の内容基準で扱われる。
  working tree でだけ編集した値は反映されない(それが意図)。
- managed bare clone 化(後続 issue)で working tree が消えても、これらの読み取りは blob 直読なので
  そのまま動く。本 ADR がその前提を整える。
- 「repo 側の読み取り」を将来増やすときは、まず本表で用途を判定する。完了契約に効くなら worktree +
  claim 時 pin、効かないなら default branch。

## 却下した代替案

- **すべて worktree から読む(現状維持)**: checkout 状態依存で宣言と乖離し、bare clone 化で壊れる。
- **すべて default branch から読む**: 完了契約の pin まで `git show` に寄せると ADR 0011 が却下した
  「共有 git dir 越しの `update-ref` 改竄」を再び招く。pin は worktree のまま。
