# Spec: `meguri watch` の daemon 化(detach 起動 + launchd 監視) — issue #25

## ゴール

`meguri watch` はフォアグラウンド常駐で、ターミナルを閉じると止まる。設計上 kill-safe
(Authority は forge のラベル、run は sqlite checkpoint + 起動時 recovery)なので落ちても
壊れないが、「気づいたら止まっていた」が起きる。本 issue で得たいのは:

1. シェルを閉じても watch が回り続ける(detach 起動)
2. login / 再起動 / クラッシュ後に勝手に復帰する(launchd 監視)
3. 「いま動いているか・どこにログがあるか」が 1 コマンドでわかる

Phase 3(CLI↔daemon IPC・バイナリ分離・HTTP API)はスコープ外。個人運用のスケールでは
sqlite 直読み + pidfile で足りる(→ ADR 0001)。

## キーデシジョン

### D1. コマンドは `meguri daemon <verb>` 名前空間に置く

issue の段階案はフラットな `meguri start/stop/status/logs` を挙げているが、
`meguri stop <run>` / `meguri logs <run>` は **run 操作としてすでに存在する**
(`src/cli.rs`)。衝突を避け、looper の `looper daemon …` 前例にも揃えて:

```
meguri daemon start        # detach 起動(Phase 1)
meguri daemon stop         # 停止(launchd モードなら bootout して自動再起動も止める)
meguri daemon restart      # stop + start(モード維持)
meguri daemon status       # PID / モード / 稼働状態 / ログ位置
meguri daemon logs [-f]    # daemon ログの tail / follow
meguri daemon install --mode launchd    # LaunchAgent 生成 + bootstrap(Phase 2)
meguri daemon uninstall    # LaunchAgent の bootout + plist 削除
```

フラットな `meguri start` エイリアスは追加しない(表面積を増やさない)。

### D2. 単一バイナリ。daemon の実体は「`meguri watch` を子プロセスとして spawn」

`megurid` は作らない。`daemon start` は `std::env::current_exe()` を
`watch` サブコマンドで spawn する:

- `pre_exec` で `setsid()`(制御端末から切り離し)
- stdin は `/dev/null`、stdout/stderr はログファイルへ append
- 親は state を書いて即 exit(子は reparent されるので double-fork 不要)

watch プロセス側の変更は最小限: 起動時に排他ロックを取り、supervision メタデータを
書くだけ(D3, D4)。scheduler / recovery のロジックは変更しない。

### D3. 単一インスタンス保証は pidfile ではなく flock

watch プロセス(フォアグラウンド・detached・launchd いずれも)は起動直後に
`~/.meguri/daemon/watch.lock` の **排他 flock** を取得し、取れなければ
「already running (pid N)」で明示エラー。stale pidfile 問題を構造的に回避し、
「detached で起動済みなのに手動 `meguri watch` して二重スケジューラ」も防ぐ。

### D4. status の情報源は state ファイル + liveness チェック(IPC なし)

watch プロセス自身が起動時に `~/.meguri/daemon/state.json` を書く:

```json
{ "pid": 12345, "mode": "foreground|detached|launchd",
  "started_at": "...", "version": "0.1.0", "log_path": "..." }
```

mode は環境変数 `MEGURI_SUPERVISED`(detach spawner / plist が設定、無ければ
foreground)から判定。`daemon status` は state.json + `kill(pid, 0)` で稼働判定し、
launchd モードでは `launchctl print gui/$UID/<label>` で supervisor 側の状態
(restart policy、last exit status)も併記する。走行中 run の数は sqlite 直読み。

### D5. launchd: user LaunchAgent、restart policy / throttle は config から

- plist: `~/Library/LaunchAgents/dev.meguri.watch.plist`、label `dev.meguri.watch`
- `ProgramArguments` = [meguri の絶対パス, "watch"]
- config → plist のマッピング(looper 踏襲):
  - `restart_policy`: `never` → KeepAlive なし(RunAtLoad のみ) / `on-failure`(既定)
    → `KeepAlive.SuccessfulExit = false` / `always` → `KeepAlive = true`
  - `throttle_secs`(既定 10)→ `ThrottleInterval`
- `EnvironmentVariables.PATH` に **install 時のユーザー PATH をそのまま焼き込む**
  (launchd 既定 PATH には homebrew の `gh`/`tmux`/`herdr`/`claude` が無い)。
  `HERDR_SOCKET_PATH` / `MEGURI_HOME` が設定されていれば同様に焼き込む
- install は plist 生成 + `launchctl bootstrap gui/$UID`、uninstall は bootout + 削除。
  config 変更(policy/throttle)は plist 再生成が必要 = `daemon install` 再実行で反映
- 非 macOS では `--mode launchd` は明示エラー(silent fallback しない)。
  systemd user unit は後続 issue

config 追加(`[daemon]` セクション):

```toml
[daemon]
restart_policy = "on-failure"  # never | on-failure | always
throttle_secs = 10
```

### D6. ログは `~/.meguri/logs/` に恒久配置、モードで分離

- detached: `~/.meguri/logs/watch.log`(spawn 時に append で open)
- launchd: `~/.meguri/logs/launchd.log`(plist の StandardOut/ErrorPath。
  起動失敗=startup エラーも launchd がここへ書くので、looper の startup ログ分離は
  このファイル分離で代替)
- `daemon logs` は state.json の `log_path` を tail。ローテーションは初回スコープ外
  (起動時に一定サイズ超なら `.1` に rename する程度は任意で可)

### D7. stop のセマンティクス: SIGTERM、graceful shutdown は追加しない

meguri は kill-safe(次回起動の recovery が running run を interrupted に倒して再開)
なので、`daemon stop` は SIGTERM 送信 + state 掃除で十分。launchd モードでは
`launchctl bootout`(KeepAlive による復活も止める)。agent pane は mux 側で生き続け、
再起動後の recovery が拾う — これは既存挙動のまま。

## 検証(launchd 配下から herdr/tmux/`gh` に届くか)

user LaunchAgent は login セッション内(同一 UID)で動くため、`$HOME` 配下の
unix socket(herdr `~/.config/herdr/herdr.sock`、tmux `/tmp/tmux-$UID/`)には
届く見込み。既知のリスクは PATH(D5 で対処)と `gh` の keychain アクセス。
実装前ではなく **実装後の受け入れ確認として手動チェックリスト**で潰す:

1. `meguri daemon install --mode launchd` → `daemon status` が running
2. launchd 配下の watch が `meguri:ready` issue を拾って pane を spawn する
   (= herdr/tmux socket + `gh` 認証 + agent PATH がすべて通っている証明)
3. 再ログイン後に自動復帰し、recovery が live pane を再アダプトする
4. 届かなかった場合の fallback(daemon が mux server を自前起動する等)は
   別 issue に切り出す — 本 issue では明示エラーで報告できれば良い

## 触るファイル

| ファイル | 変更 |
|---|---|
| `src/cli.rs` | `Daemon { … }` サブコマンド(ネストした verb enum)追加 |
| `src/main.rs` | dispatch 追加 |
| `src/daemon/mod.rs`(新規) | detach spawn、flock、state.json 読み書き、status/stop/logs |
| `src/daemon/launchd.rs`(新規) | plist 生成(純関数として test 可能に)、launchctl 呼び出し、platform gate |
| `src/config.rs` | `DaemonConfig`(restart_policy / throttle_secs)追加 |
| `src/app.rs` | `cmd_watch` 起動時に flock 取得 + state.json 書き込み |
| `README.md` / `README.ja.md` | 運用節(daemon start/install/status)追加 |
| `tests/daemon_test.rs`(新規) | 下記 |

## テスト

- unit: plist 生成のスナップショット(policy 3 種 × throttle、PATH 焼き込み)、
  state.json roundtrip、config デフォルト
- integration(`tests/daemon_test.rs`): `MEGURI_HOME` を tempdir に向けて
  flock の排他(二重起動が明示エラー)、`daemon status` の not-running / running 表示
- 非 macOS(CI Linux)で `--mode launchd` が明示エラーになること(`#[cfg]` で両側テスト)
- launchd 実機系は CI 不能 → 上記手動チェックリスト(PR 説明に結果を記載)

## 受け入れ条件(issue から)

- [ ] `meguri daemon start` 後にシェルを閉じても watch が回り続け、`meguri:ready` issue が処理される
- [ ] launchd モードで再ログイン後に自動復旧し、startup recovery が live pane を再アダプトする
- [ ] `meguri daemon status` で PID / モード / 稼働状態 / ログ位置がわかる
- [ ] 非対応プラットフォームで launchd モードは明示エラー(silent fallback しない)
- [ ] `cargo test` / `clippy -- -D warnings` グリーン

## スコープ外

- CLI↔daemon IPC / HTTP API / バイナリ分離(Phase 3、ADR 0001)
- systemd user unit(Linux)— 後続 issue
- ログローテーション、自動 upgrade(`meguri upgrade`)
- #5(events ベースの watch)・#7(awaiting_human 通知)・#13(reaper)は常駐化で
  価値が上がるが、本 issue では触らない
