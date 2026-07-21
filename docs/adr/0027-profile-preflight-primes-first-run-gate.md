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

**pane 起動の直前に、その CLI 自身の headless 起動を対象 cwd で一度走らせて（prime）、フォルダ信頼を
CLI 自身の形式で永続化させる。** meguri は `~/.claude.json` を一切パースも書き込みもしない — 書くのは
常に CLI 本体である。

- **prime の担当はフォルダ信頼だけ（親 spec D1 に忠実）**。初回ゲートは bypass 受諾（config-dir 単位）と
  フォルダ信頼（パス単位）の2つ。親 spec D1 は前者を doctor（#234、既マージ）＋人間の一度きり受諾に、
  後者を本 prime に割り当てている。prime は bypass を書かせない。
- claude の既定 pre-flight は **`claude -p '<no-op>'`（yolo なし・permission-mode override なし）** を
  対象 cwd で走らせるだけ。headless `-p` は唯一ゲートを素通りする経路で、その一回で当該パスのフォルダ信頼を
  CLI 自身が書き残す。以降、同じパスへの対話 pane 起動はフォルダ信頼ゲートに当たらない。
- **yolo を外したことが injection の根治（f10/f12/f13）**。prime は worktree 上で実モデルターンを走らせ
  CLAUDE.md を読むので、yolo だと外部 PR 等の injection が pane 起動前に任意のツール操作を誘発しうる。
  yolo を外し ask モードにすると、headless には権限プロンプトに答える人間が居らず、どのツール呼び出しも
  承認されない — injection が指示を奪ってもアクチュエータが無い。脆いツール封じフラグ（CLI・バージョンで
  揺れ、`--dangerously-skip-permissions` が allowlist を無視する恐れもある）に頼らず、「headless × 非 yolo
  ⇒ ツール実行不可」という堅い性質だけで安全を担保する。前ラウンドの「args を鏡写して yolo を持ち込む」案は
  撤回した（f3→f12）。
- **既定 argv は一つに固定（f12）**。profile に関わらず claude は `["-p", <no-op>]`。実機検証に失敗
  （非 yolo `-p` がフォルダ信頼を書かない）した場合は yolo を足すのではなく meguri 所有 config-dir 案
  （後述 rejected 案 2）へ切り替える。yolo を足す道は injection 面を復活させるので採らない。
- **明示 override は危険な opt-in（f13）**。`preflight` に非空値を書くと argv はそのまま実行され、既定の
  非 yolo 縛りを迂回できる。host は信頼境界の内側（ADR 0011）なのでブロックはしないが、yolo 相当フラグを
  含む override は config ロード時に警告し、README で「injection 無防備・自己責任」と明示する。全 preflight
  に安全 argv を強制注入しないのは、`preflight` が `headless_args` 同様「完全な argv をそのまま使う」契約で、
  meguri が勝手に引数を足し引きすると override の意味が壊れるため。
- cursor-agent は `--trust`/`--force` を launch `args` に載せて毎回素通りする既存方式のままで、
  pre-flight は空（不要）。
- **副作用**: prime は実モデルターンを1回・cwd で走らせ CLAUDE.md を読む（トークンと1往復）。非 yolo・
  ツール承認者なしなので injection が指示を奪ってもツールは実行されない。README に明記する。
- **config-dir の一致**: tmux/herdr はサーバー経由で pane を作るため、prime（daemon 環境）と pane
  （サーバー環境）で `CLAUDE_CONFIG_DIR` がずれ得る。実効 config-dir を絶対パスに解決し、prime の env
  と `PaneSpec.env` の両方へ明示的に渡して一致させる（f1）。
- **実行**: prime は `tokio::process` で async に走らせ、timeout・reap も `.await` する。同期実行だと
  最大 30 秒 Tokio worker を塞ぎ、並列 run・scheduler・crash recovery を巻き添えにするため（f4）。
- **「一度だけ」の統制**: 「済み」状態は `(command, config_dir, argv, 絶対対象パス)` 単位のマーカーで
  持ち、プロセス内 async ロックで確認→prime→記録を直列化し、成否を記録して二度は試みない（claim-once）。
  command 非依存の単一フラグだと別 command が必要な prime を握り潰し、非直列だと並列 reviewer が重複 prime
  し、成功時のみ記録だと失敗後に毎 spawn 再 prime する — その3つを塞ぐ（f6/f7/f8）。マーカーは ephemeral な
  cwd の中ではなく `~/.meguri/preflight/`（`config::meguri_home()`）に置く。advisor_dir は毎回削除・再作成
  されるので、cwd 内に置くと re-embodiment ごとに再 prime して claim-once が壊れるため（f11）。
- **prime の対象 pane は2か所**: `spawn_agent_pane`（worker/planner/fixer/pr-reviewer）と
  `spawn_advisor_pane`（collab advisor）。後者も fresh dir で同じ対話 CLI を直接起動しフォルダ信頼ゲートに
  当たる（f9）。

なぜ prime が効くのか（version-stable の理由）: フォルダ信頼の受け皿がどのフィールドであろうと、それを
正しく書けるのは CLI 本体だけである。meguri はフィールドを知る必要がなく、CLI に「信頼済みの状態」を
作らせるだけ。フィールド名が変わっても prime の argv は変わらない。

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

- 新規パスの identity ごとに一度だけ軽い prime が走る。フォルダ信頼はパス単位なので prime は必ず対象の
  cwd（worker は worktree、advisor は advisor_dir）で走らせる。
- pre-flight が書かせる状態は「人がフォルダ信頼プロンプトに一度答えたのと同じ」もの（無害な path trust）。
  bypass 受諾は prime では書かない — どの profile でも共有 config-dir を勝手に変えない。meguri を
  ロールバックしてもフォルダ信頼は `~/.claude.json` に残るが、CLI 側の資産であり meguri の管理外。
- bypass ゲートは doctor（#234）の担当: doctor が gate-probe で検知し、人間が config-dir ごと一度だけ
  受諾する（永続）。フォルダ信頼は doctor では検知できない per-worktree の担保で、prime が自動化し、
  実ターン起動が通ることで結果的に担保される。
- config スキーマに `preflight`（前捌き argv）を足す。これは public contract の追加なので
  spec 側で migration & rollback を明記する。
