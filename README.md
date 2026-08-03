# meguri v2（フルスクラッチ書き直し中）

**AI コーディングエージェントを terminal multiplexer の生きた pane で回すオーケストレーター。**

このブランチは meguri のフルスクラッチ書き直し(v2)です。v1(〜15k 行の核)は main / git 履歴にあり、その運用で確定した概念だけを、**全行を通読・理解できるサイズを保ちながら**積み直します。書き直しの主目的は理解の再取得 — 1 増分 = 1 PR = 人間が全行読めるサイズ、を規律とします。

## v1 から持ち越す不変条件

1. **完了コントラクト** — orchestrator は worktree に `.meguri/prompt-<turn_id>.md` を書き、エージェントは `.meguri/result.json`(`turn_id` / `status: success|failure|needs_human` / `summary`)で完了を申告する。画面のパースはしない。
2. **trust-but-verify** — `success` の申告は独立に検証する: git tree が clean、base より commit が進んでいる、`check_command`(設定時)が通る。3 つ揃わない限り成功として扱わない。
3. **生きた pane** — エージェントは headless ではなく tmux の対話セッションで動き、人間はいつでも attach して介入できる。ブロックは失敗ではない。
4. **権威はローカル** — キューの判断はローカルの状態が権威で、GitHub は低頻度のエッジ入力と best-effort の投影(v1 の権威反転。GitHub 対応の増分で再導入)。

v1 が蓄積した失敗の知識(ブロックダイアログ、虚偽申告、session health、crash recovery …)は `docs/adr/`(dormant 台帳: `docs/adr/STATUS.md`)にあり、各増分はこのカタログを参照して設計する。

## v0(現在)

**local task 1 本 → tmux pane のエージェント → 検証済みブランチ。** 約 800 行・依存 5 crate。

```bash
cargo install --path .
mkdir -p ~/.meguri && cat > ~/.meguri/config.toml <<'EOF'
[[projects]]
id = "myproj"
repo_path = "/abs/path/to/clone"
check_command = "cargo test"
EOF

meguri run "READMEのtypoを直す"
# → worktree を切り、tmux pane で claude が走り、
#   result.json の申告を独立検証して、検証済みブランチを残す
```

途中で介入したければ `tmux attach -t meguri-myproj`。失敗・needs_human・検証落ちのときも pane は残るので、attach して続きを人間が引き取れる。

### コードの読み順(全 5 ファイル)

1. `src/main.rs` — 1 run の一生が上から下へ並ぶ
2. `src/config.rs` — 最小 config(書いた項目だけ上書き)
3. `src/turn.rs` — 完了コントラクト
4. `src/mux.rs` — tmux に求める 3 操作(spawn / send / alive)
5. `src/gitops.rs` — worktree と trust-but-verify

## ロードマップ

増分の順序と各増分が参照する失敗カタログは [docs/design/v2-roadmap.md](docs/design/v2-roadmap.md) を参照。

## License

MIT
