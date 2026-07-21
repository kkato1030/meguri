# ADR 0027: profile pre-flight は CLI 自身の headless 起動で初回ゲートを素通りさせる — meguri は CLI の設定 JSON を書かない

- Status: accepted
- Date: 2026-07-21
- Issue: #235（親 #232）
- 関連: docs/adr/0012-launch-mode-role-pane-or-direct-keep-pane-subordinate.md（pane mode の
  role がこのゲートに詰まる）、`src/gate.rs`（issue #234 の doctor gate-probe — 同じ「CLI の
  内部フィールドを読まない・書かない」原則）、`.claude/rules/overview.md`（meguri は画面を読まない）

## 文脈

対話 pane で起動する role（planner / worker / fixer / pr-reviewer は ADR 0012 で `Pane`）は、
agent CLI の初回対話ゲートに詰まる。claude なら「Bypass Permissions mode」の一度きり受諾と、
fresh worktree のフォルダ信頼プロンプト。meguri は画面を読まないので、人が `2` を押さない限り
永久に止まる。

調査で分かったこと:

- bypass 受諾を非対話で満たす supported で version 安定な口は無い。`settings.json` の
  `permissions.defaultMode` を設定しても対話受諾は消えない。
- 受諾フラグの名前・場所は CLI のバージョンで揺れる（`bypassPermissionsModeAccepted` 等）。
- フォルダ信頼はパス単位で `~/.claude.json` に記録され、信頼判定は repo 側 settings を読む
  **前**に走る。だから worktree 内の settings では pre-trust できない。
- 唯一素通りするのは headless `-p`。

meguri が `~/.claude.json` の内部フィールドを直書きすれば動くが、フィールド名がバージョンで
揺れる以上それは version-fragile な結合であり、`src/gate.rs`（doctor）が既に「読まない・
書かない」と決めた原則にも反する。

## 決定

**pane 起動の直前に、その CLI 自身の headless 起動を worktree の cwd で一度走らせて（prime）、
初回ゲートの受諾を CLI 自身の形式で永続化させる。** meguri は `~/.claude.json` を一切パースも
書き込みもしない — 書くのは常に CLI 本体である。

- claude の既定 pre-flight は `claude --dangerously-skip-permissions -p 'ok'`。headless `-p` は
  唯一ゲートを素通りする経路であり、その一回の実行で bypass 受諾（config-dir 単位）と
  フォルダ信頼（cwd のパス単位）を CLI 自身が書き残す。以降、同じ config-dir・同じ worktree
  への対話 pane 起動はゲートに当たらない。
- cursor-agent は `--trust`/`--force` を launch `args` に載せて毎回素通りする既存方式のままで、
  pre-flight は空（不要）。「非対話でゲートを前捌きする」という枠は共通で、実現手段が CLI ごとに
  違う（claude=prime プロセス / cursor-agent=launch フラグ）だけである。

なぜ prime が効くのか（version-stable の理由）: 受諾の受け皿がどのフィールドであろうと、それを
正しく書けるのは CLI 本体だけである。meguri はフィールドを知る必要がなく、CLI に「受諾済みの
状態」を作らせるだけ。フィールド名が変わっても prime の argv は変わらない。

pre-flight は best-effort に徹する。timeout・spawn 失敗・非ゼロ終了のいずれでも pane は殺さず、
そのまま起動する（ゲートは前ターンで既に満たされているかもしれず、人の attach 導線も残る）。
prime の失敗が hang より悪い結果を生んではならない。

## 退けた代替案

1. **`~/.claude.json` の内部フィールド直書き。** version-fragile。フィールド名・場所が
   バージョンで揺れる。doctor が既に退けた結合を launch 経路で再導入することになる。
2. **`CLAUDE_CONFIG_DIR` を meguri 所有にして prime。** 一見きれいだが、その config-dir には
   認証情報が無い。ユーザーの `~/.claude` から資格情報を **供給** し、ファイル権限で **保護** し、
   profile 間で **分離** し、後で **削除** する仕組みを全部作る羽目になる（4つとも新しい攻撃面・
   運用面）。既定の `~/.claude` をそのまま継承し、CLI に自分の receipt を書かせる案（採用案）は
   この4問題を丸ごと回避する。empirical 検証で prime が共有 config-dir に受諾を永続化しないと
   判明した場合の最終フォールバックとしてのみ残すが、その場合は上記4点を別 issue で設計する。
3. **当該 role を direct 起動へ倒す。** direct は headless `-p` なのでゲートに当たらないが、
   ADR 0012 が planner/worker/fixer/pr-reviewer を `pane` にしたのは attach と会話継続のため。
   ゲート回避のために attach 価値を捨てるのは本末転倒。

## 帰結

- 新規 worktree ごとに一度だけ軽い prime が走る。フォルダ信頼はパス単位なので prime は必ず
  worktree の cwd で走らせる。
- pre-flight が書かせる状態は「人が一度受諾したのと同じ」もの。meguri をロールバックしても
  その状態は `~/.claude.json` に残るが無害（CLI 側の資産であり meguri の管理外）。
- doctor（#234）の bypass gate-probe は、prime が bypass 受諾を永続化していれば緑になる。
  フォルダ信頼は doctor では検知できない per-worktree の担保で、これは実ターン起動が通ることで
  担保する。
- config スキーマに `preflight`（前捌き argv）を足す。これは public contract の追加なので
  spec 側で migration & rollback を明記する。
