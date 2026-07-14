# ADR 0012: launch mode は role 単位で pane / direct を選べる — keep_pane はその従属設定(ADR 0004 / 0006 部分改訂)

- Status: accepted
- Date: 2026-07-14
- Issue: #169
- 関連: docs/adr/0004-issue-lane-pane-session-lifetime.md(本 ADR が「lane = pane」の前提を
  緩める)、docs/adr/0006-ai-implementation-review-is-an-internal-loop.md(self-review が
  内部ループである結論はそのまま、実行体も内部化する)、
  docs/adr/0011-routing-role-6-kinds-of-work-independent-of-loop-kind.md(本 ADR の role
  語彙はこの6分類を使う)

## 文脈

すべての turn は mux(tmux/herdr)の生きた pane 内で対話セッションとして走ってきた
(ADR 0004)。しかし pane が要るのは「turn が終わったあともその実行体に用がある」役割
だけである。`self-reviewer`(ADR 0006 の内部ループ、人間は介在せず transcript も外に出ない)
や `cleaner`(read-only sweep、終了後は用済み)は、pane を張ったまま維持する理由がない。

現状の pane 後始末には、この「全 role 一律 pane」の設計に起因する綻びが2つある:

1. `keep_pane = "never"` は run 自身の author lane しか見ない(`flow::finish_pane`)。
   self-review lane の pane は release されず残り続ける。
2. cleaner の `Stopped` 経路だけ共有の `release_pane`(session 保存 + `mark_pane_reclaimed`)
   を通らず、`run.mux_pane_id` に対して直接 `kill_pane` していた。session id が保存されず、
   pane 行が「生きているつもり」のまま残る。

どちらも個別にパッチすることもできるが、根本原因は同じ — 「pane を残す価値のない役割にも
pane を強制している」こと自体である。

## 決定

**設定を直交する2軸に分ける。**

- **軸 A: launch mode**(`[launch]`、role 単位)— `pane` / `direct` / 省略時 auto。
  判断基準は「turn 終了後にその実行体に用があるか」:
  - 用がある(人間が attach する / issue の会話を跨いで継続する)→ `pane`
  - 終了時に即消えてよい → `direct`(`claude -p` 相当の非対話サブプロセス)
- **軸 B: keep_pane**(`pane` mode の role にのみ意味を持つ)— `until-issue-closed`(既定)/
  `never`。**失敗(needs-human)時は `never` でも pane を残す**現行挙動を仕様として明文化する
  (人間の attach 導線を切らないため)。

auto 推奨表(role は ADR 0011 の6分類):

| role | 推奨 | 根拠 |
|---|---|---|
| planner / worker / fixer | `pane` | ADR 0004 の核 — author lane の会話継続 + needs-human 時の attach |
| pr-reviewer | `pane` | 再レビューラウンドの文脈と attach 価値(throughput 重視なら direct に落とせる筆頭) |
| self-reviewer | `direct` | 内部ループ(ADR 0006)。人間は介在せず transcript も外に出ない |
| cleaner | `direct` | read-only sweep。現状も sweep 終了時に自前で即回収している |

未知の role は安全側(`pane`)に倒す。`[launch.roles]` の明示指定は auto より常に優先する
(routing の auto/明示と同じパターン)。

```toml
[launch]
# 省略時: すべて auto(上の推奨表)。明示指定は auto より常に優先
[launch.roles]
pr-reviewer = "direct"   # 例
```

### 実装の骨子

- **executor 抽象**: `TurnEngine::await_completion`(pane 版、既存)と
  `TurnEngine::await_completion_direct`(direct 版、新設)に分割する。pane 版は
  `pane_alive` 監視 + result file、direct 版は「サブプロセス spawn → exit 待ち → result file
  読み」。両方とも `PaneDied`(死亡)と「result なしで exit」を同じ `TurnOutcome::PaneDied`
  に写像するので、上位の `flow.rs` は launch mode を意識しない。
- **`AgentProfile::direct_args`** を追加(claude の既定は `["-p"]`)。起動列は
  `{command} {args} {direct_args} [{resume_args} <session-id>] <trigger>`。
- **resume は mode 非依存**: session id の真実は引き続き `panes.agent_session_id`
  (ADR 0004)に置く。ただし ADR 0004 が定義した「lane = pane」の前提をここで緩める —
  lane は「issue-scoped な resumable context」を指し、pane はその一実装手段(optional)に
  なる。worktree transcript 走査(`agent_session::latest_session_id`)は cwd ベースで
  direct でも機能するため、pane ↔ direct を切り替えても同じ会話に resume で戻れる。
- **escalation 文言の分岐**: direct role では「pane に文脈がある、`meguri attach` せよ」の
  代わりに `claude --resume <session-id>` を提示する。`meguri ps` に `MODE` 列を追加する。

## 帰結

- 綻び2件は個別修正ではなく**構造的に消滅する**: self-reviewer / cleaner が既定で `direct`
  になることで、release すべき「self-review lane の pane」「cleaner の pane」自体が存在
  しなくなる。念のため両経路とも共有の `release_pane` を通す形に揃えてあるので、
  `[launch.roles]` で明示的に `pane` へ戻した場合でも正しく後始末される。
- `self-reviewer` / `cleaner` は tmux/herdr のオーバーヘッド(pane 起動・監視・nudge)なしで
  turn を回せるようになり、内部ループの往復コストが下がる。
- ADR 0004 の「pane の鍵は `(project, issue, lane)`」「resume の真実は
  `panes.agent_session_id`」という骨子は変わらない。変わるのは「lane には必ず生きた pane が
  ある」という暗黙の前提だけで、それを「pane は lane の optional な一形態」に緩める。
- ADR 0006 の「self-review は内部ループである」という結論は変わらない。本 ADR はその実行体も
  (デフォルトで)内部化し、GitHub に一切出ないことに加えて mux にも一切出ないようにする。
- 明示設定で `pr-reviewer` を `direct` に倒すなど、throughput 重視の運用を選べる余地を残す。
