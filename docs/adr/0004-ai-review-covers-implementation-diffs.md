# ADR 0004: AI レビューは spec と実装 diff の両方を対象にする(人間は merge ゲートに残る)

## ステータス

採用(issue #84、impl-reviewer v0)

## コンテキスト

meguri の AI レビューは spec 限定だった。reviewer ループの対象は `meguri:spec-reviewing` の
spec PR のみで、実装 diff へのレビューを meguri 自身が生成することはなく、実装レビューは
「人間 or 外部 bot がスレッドを付ける → fixer が直す」という片翼の経路しかなかった。
一方で消費側の機構は完成している: fixer は review スレッドの作者を問わず、未解決スレッドを
入力として fix を push し、🔁 reply で parked にする収束プロトコルを持つ。自律 issue→PR
装置が自分の書いた diff を一度も見返さずに人間へ渡すのは、仕様ではなく設計の穴である。

同時に、AI が AI の diff をレビューして AI が直す閉ループには固有のリスクがある:
収束しない ping-pong によるコスト暴走と、AI の approve が人間の判断を侵食すること。

## 決定

1. **meguri は自分の実装 PR の diff にもレビューを生成する(impl-reviewer ループ)。**
   findings は inline review スレッドとして投稿し、既存の fixer 経路に流し込む。
   AI レビューの対象はこれで spec と実装 diff の両方になる。
2. **人間の merge が唯一のハードゲートであり続ける。** AI レビューは event=COMMENT のみ —
   approve も request-changes も決してしない。レビューはゲートの置換ではなく、
   ゲートに届く前の品質向上である。
3. **閉ループには構造的な栓を義務付ける。** head ごと 1 回(forge 上のマーカーが真実 —
   Authority 原則)、ラウンド上限(上限到達後は静かに引く。`needs-human` は付けない —
   レビュー済み PR が merge 待ちで開いているのは正常状態)、clean 時はスレッドを作らない
   (fixer の入力が生まれず自然に止まる)、そして config のキルスイッチ。

## 帰結

- review→fix の ping-pong が上限までは AI 内で閉じ、人間には「AI が一巡した後の PR」が届く。
  人間・外部 bot のレビューは置換ではなく上乗せで、経路は完全に同一(fixer は作者を見ない)。
- ラベル状態機械は増えない: このループはラベルレスで、状態は forge 上のマーカーと
  スレッドだけに置かれる。
- コストはラウンド上限とキルスイッチで抑えられる。外部レビュー bot のある環境では
  無効化すればよい。
- 将来 spec reviewer を inline thread 化する場合も、本決定の `create_pr_review` と
  同じ栓(head マーカー、event=COMMENT)の上に乗せる。
