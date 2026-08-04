# meguri v2 — 現在のアーキテクチャと機能

> **この文書は「現時点の meguri が何であるか」を常に正確に映す地図である。**
> 増分 PR は必ずこの文書を同時に更新する(更新のない機能追加はレビューで弾く)。
> 未来の計画は書かない — それは [design/v2-roadmap.md](design/v2-roadmap.md) の
> 仕事。ここに書いてよいのは、いまの main(v2)で実際に動くものだけ。

最終更新: v0 + herdr 対応 + preflight prime 時点。

## いまできること

設定済みプロジェクトに対する **`meguri run "タスク"` の 1 コマンドだけ**。
タスク 1 本を herdr / tmux pane のエージェントに実行させ、検証済みブランチを残す。

無いもの: 検証落ちの差し戻し・nudge・resume・キュー/watch・GitHub 連携・並列
実行。

## コンポーネント(ソースと 1:1)

| モジュール | 責務 |
|---|---|
| `main.rs` | CLI と 1 run のオーケストレーション(下の流れそのもの) |
| `config.rs` | `~/.meguri/config.toml` の読み込み。書いた項目だけ上書き、未知キーは loud に拒否 |
| `turn.rs` | 完了コントラクト: prompt の書き出しと result.json の読み取り |
| `mux/` | mux に求める 3 操作(pane を作る・1 行打つ・生死を見る)の trait と herdr / tmux の 2 実装 |
| `preflight.rs` | claude の folder-trust prime: pane 起動前に deny-all の headless 1 発で trust を記録(best-effort) |
| `gitops.rs` | git 操作の集約: worktree 作成と trust-but-verify。他所から git を直接叩かない |

## 1 run の流れ

```
meguri run "タスク" [--project <id>]     ※空タスクは即エラー
   │
   ▼
[config]  config.toml を読む。--project 省略は「1 件だけ設定済み」のときのみ可
   │
   ▼
[gitops]  repo_path の default_branch 先端から worktree を切る
          - branch: meguri/<slug>-<run_id>   (run_id = unix millis)
          - 場所:   ~/.meguri/worktrees/<project>/<run_id>
          - .meguri/ を共有側 .git/info/exclude に登録
            (worktree 個別の info/exclude は git が読まない — 実測済み)
          - 切った時点の base SHA を保持 ← 検証②の基準点
   │
   ▼
[turn]    worktree に .meguri/prompt-<turn_id>.md を書く。タスク本文と
          「完了の作法」(result.json の形式・.meguri/ はコミット禁止・
          check_command があることの予告)を prompt 自身に埋め込む —
          エージェント側の事前設定を要求しない
   │
   ▼
[preflight] agent が claude なら、pane 起動前にその worktree で headless の
          claude を 1 回走らせ folder trust を記録する(fresh worktree の
          「このフォルダを信頼するか?」ダイアログで run が始まらない問題の
          対策)。この 1 回は yolo なし + meguri 所有の deny-all --settings +
          --strict-mcp-config で走り、ツールを一切実行できない。失敗しても
          警告して pane 起動に進む(その場合はダイアログに人間が答える)
   │
   ▼
[mux]     mux.kind(既定 auto: herdr socket が生きていれば herdr、いなければ
          tmux)を解決し、run ごとの pane を作ってエージェント CLI(既定:
          claude --dangerously-skip-permissions)を対話モードで起動:
          - herdr: workspace「meguri:<project>」に tab create → pane run
            (tab のシェル内で起動。CLI が exit しても pane と最終画面が残る)
          - tmux:  session「meguri-<project>」に window を作りコマンド直起動
          spawn_grace_secs(既定 8s)待って「<prompt パス> を読んで、その内容を
          完遂してください。」の 1 行だけを打ち込む(テキストと Enter は分離)
   │
   ▼
[待機]    2 秒間隔で .meguri/result.json をポーリング。判断材料は
          「ファイルの出現」と「pane の生死」の 2 つだけ(画面は読まない):
          - turn_id 不一致・壊れた JSON → 過去の残骸として無視、待ち続行
          - pane 死亡 → 失敗(result 未提出)
          - max_turn_runtime_secs(既定 2700s)超過 → タイムアウト。
            pane は殺さない
   │
   ▼
[検証]    status = "success" のときだけ、独立に 3 点検証(trust-but-verify):
          ① git status --porcelain が空(未 commit の変更なし)
          ② rev-list --count <base_sha>..HEAD > 0(何もせず success を弾く)
          ③ check_command が通る(設定時。orchestrator 自身が sh -c で実行)
   │
   ▼
成功:     検証済みブランチが repo_path に残る(push はしない)
それ以外: 理由と attach 案内(mux ごとの文言)を表示して非ゼロ終了。
          pane は必ず残す — 人間が続きを引き取る
          (failure / needs_human / 検証落ち / タイムアウト、すべて同じ)
```

## 契約とデータ

**完了コントラクト**(orchestrator ↔ エージェントの唯一の接点):

```
.meguri/prompt-<turn_id>.md   orchestrator → エージェント(指示)
.meguri/result.json           エージェント → orchestrator(申告)
  {"turn_id": "...", "status": "success" | "failure" | "needs_human", "summary": "..."}
```

**設定**(`~/.meguri/config.toml`、`MEGURI_HOME` で移動可):

```toml
[[projects]]
id = "myproj"
repo_path = "/abs/path/to/clone"   # 必須。meguri は clone を所有しない
default_branch = "main"            # 既定 main
check_command = "cargo test"       # 任意。検証③で orchestrator が実行

[agent]                            # 既定: claude + yolo
command = "claude"
args = ["--dangerously-skip-permissions"]

[mux]
kind = "auto"                      # auto | herdr | tmux(auto = socket 検出)

[limits]
spawn_grace_secs = 8               # CLI 起動から prompt 投入までの猶予
max_turn_runtime_secs = 2700       # result.json を待つ上限(pane は殺さない)
```

**ファイルシステム**:

```
~/.meguri/config.toml                        設定
~/.meguri/worktrees/<project>/<run_id>/      run ごとの worktree(掃除は手動)
~/.meguri/preflight/deny-settings.json       preflight 用 deny-all(0600、meguri 所有)
<repo_path> のブランチ meguri/<slug>-<id>    成果物
```

永続化(DB)は無い。run の状態はプロセスの中にだけあり、プロセスが死ねば
worktree と pane が残骸として残る(resume は v0.3 で導入予定)。

## 設計上の決めごと(v1 から持ち越した不変条件)

1. **画面は読まない** — 成否の判断材料は耐久のあるシグナル(result.json、
   pane の生死、git の状態)だけ。
2. **trust-but-verify** — エージェントの自己申告を信じない。成功の定義は
   3 点検証が揃うこと。
3. **ブロック ≠ 失敗** — どの結末でも pane を残し、人間がいつでも attach して
   引き取れる。人間の介入をエラーとして扱わない。
4. **抽象は後払い** — trait 等の抽象は 2 つ目の実装が実際に現れたときに
   導入する(mux trait は herdr 対応の増分で、2 実装目とともに入った)。
5. **依存最小** — crate は 5 つ(anyhow / clap / serde / serde_json / toml)。
   新しい依存はそれが解く問題が現れた増分で足す。

## 既知の割り切り(v0 の意図的な穴)

- 検証落ちはエージェントに差し戻さず人間行き(v0.1 で fix turn を導入予定)
- エージェントが沈黙しても nudge しない(タイムアウトまで待つだけ)
- worktree / pane の掃除コマンドが無い(pane close / kill-session と
  `git worktree remove` を手で)
- 同一プロジェクトの並列 run は動くが、スロット制御は無い
