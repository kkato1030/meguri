# apm × worktree_setup を meguri 自身のループで動かす(#139)

#137([ADR 0008](../adr/0008-agent-instructions-via-apm.md))で apm をエージェント指示の
ソースにし、#138 で `[projects.worktree_setup]` フックを実装した。本ドキュメントは、その2つを
組み合わせて **meguri プロジェクト自身**(`kkato1030/meguri`)のループで dogfooding する手順と、
実機検証で見つかった落とし穴を記録する。

meguri の `config.toml` は `~/.meguri/config.toml` にありリポジトリ外(ホストごとの運用設定)
なので、ここに書く TOML はコピー用のリファレンスであり、このリポジトリのファイルではない。

## ホストへの apm CLI インストール

```bash
brew install microsoft/apm/apm   # または: curl -sSL https://aka.ms/apm-unix | sh
apm --version                    # 動作確認(このドキュメントは apm 0.24.0 で検証)
```

`meguri` を動かすホスト(worker/planner/fixer などのペインを起動するマシン)に入れる。
worktree ごとに apm を入れる必要はない — CLI 自体はホストに1つあればよい。

## 確定した config 設定

`~/.meguri/config.toml` の `[[projects]]`(meguri 自身のエントリ)に追加する:

```toml
[projects.worktree_setup]
commands = [
  "apm install --frozen",
  "git checkout -- apm.lock.yaml",
  "apm compile",
]
exclude = ["CLAUDE.md", ".claude/rules", "AGENTS.md", ".codex/", "apm_modules/", ".agents/"]
# required は既定 false のまま(apm 失敗時は warn して続行 — 下記「動作確認」参照)
```

issue #139 が最初に挙げていた案は `commands = ["apm install --frozen"]` のみだったが、
実機で dogfood した結果 **それだけでは不十分**なことが分かった。理由は次の2点。

### 1. `git checkout -- apm.lock.yaml` が要る理由

`apm.lock.yaml` は git 管理下の**追跡ファイル**である。`apm install`(`--frozen` 付きでも)は
実行するたびに `local_deployed_files` / `local_deployed_file_hashes` をディスク上の展開結果に
合わせて書き換える([ADR 0008](../adr/0008-agent-instructions-via-apm.md) で既知の副作用) —
展開先のファイル内容が変わらない no-op 実行でも書き換わる。

`worktree_setup.exclude` は `.git/info/exclude` に積むだけなので、**未追跡ファイルにしか
効かない**。追跡済みの `apm.lock.yaml` の変更は隠せない。結果、`apm install --frozen` だけを
worktree_setup に登録すると:

- meguri がターンの success 申告を検証するとき(`git status --porcelain` が空であることを見る、
  `src/gitops.rs` の `status_clean` / `src/engine/flow.rs` の実行ターンループ)、
  **エージェント自身は一切触っていない `apm.lock.yaml` のせいで** tree が dirty 判定になる。
- meguri は "working tree clean: false" として success 申告を突き返し、訂正ターンを強制する。
  エージェントは身に覚えのない差分の犯人探しをする羽目になる。

対処は、`apm install --frozen` の直後に `git checkout -- apm.lock.yaml` を worktree_setup の
コマンド列に加えること。ロックファイルを毎回コミット済みの最小形(`dependencies: []` のみ)へ
戻す。3 回連続実行しても収束することを確認済み(下記「検証結果」)— `--frozen` が見るのは
`apm.yml` との依存構造の整合性であり、`local_deployed_files` の有無ではないため、
戻した直後に再度 `apm install --frozen` を叩いても失敗しない。

### 2. `apm compile` も要る理由

`apm.yml` の `targets` は `claude` と `codex` の2つ(`src/routing.rs` の built-in 推奨テーブルが
self-reviewer / pr-reviewer に codex を優先的に使う)。`apm install` 単体では Claude Code が
直接読む `.claude/rules/` しか展開されず、**Codex が読む `AGENTS.md` / `src/AGENTS.md` は
`apm compile` を実行しないと生成されない**。`apm install --frozen` だけの構成だと、codex に
ルーティングされたターンはリポジトリ固有の静的指示を一切持たずに動くことになる。

順序は `apm install` → `apm compile` を守ること。先に `.claude/rules/` を展開しておくと、
`apm compile` はそれを重複コンテキストとして扱い `CLAUDE.md` の生成を自動スキップする
(README / ADR 0008 に記載済みの既知挙動)。`apm compile` は `apm.lock.yaml` を書き換えない
ことを確認済みなので、`git checkout` の位置は `apm install` の直後であれば十分。

## 動作確認方法

1. ホストで `apm --version` が通ることを確認する。
2. `~/.meguri/config.toml` の meguri エントリに上記 `[projects.worktree_setup]` を設定する
   (hot reload 対象外の可能性があるため、`meguri watch` の再起動を挟むと確実)。
3. 通常どおり issue に `meguri:ready` を付ける(または `meguri add`)。meguri が worktree を
   準備する際に自動でコマンド列が走る。
4. 該当 worktree(`~/.meguri/worktrees/meguri/<branch>/`)の中で確認する:
   - `.claude/rules/{overview,rust,docs}.md` が存在する
   - `AGENTS.md` と `src/AGENTS.md` が存在する(`CLAUDE.md` は仕様上生成されない — 上記参照)
   - `git status --porcelain` がエージェントの本来の変更以外に何も出さない
     (`apm.lock.yaml` の差分が残っていないこと)
5. PR がオープンされたら diff を見て、`CLAUDE.md` / `AGENTS.md` / `.claude/rules/` / `.codex/` /
   `apm_modules/` / `.agents/` は一切含まれず、`apm.lock.yaml` も変更されていないことを確認する。
6. 失敗時 warn 継続の確認: 一時的に `apm` を `PATH` から外す(またはコマンドを存在しない名前に
   変える)状態で worktree を準備させ、run が失敗扱いにならず(`required = false` のまま)、
   `worktree_setup.failed` イベントが記録され、以降の通常フローが続くことを確認する。

## 検証結果(#139, apm 0.24.0)

このドキュメントを書いている worktree(issue #139 自身の実行ターン)は、まさに
`[projects.worktree_setup]` の対象として起動されたものだった — 開始時点で
`.claude/rules/{overview,rust,docs}.md` は生成済み、`apm.lock.yaml` は `local_deployed_files`
付きで modified 状態のまま残っており、**上記1.の問題を実地で踏んだ**。以下を確認した:

- `apm install --frozen` → `git checkout -- apm.lock.yaml` → `apm compile` のサイクルを
  3 回連続実行し、毎回 exit code 0、最終的に `git status --porcelain` が空になることを確認した
  (冪等性の確認)。
- `apm compile` は `AGENTS.md`(ルート)・`src/AGENTS.md` を生成し、`.gitignore` により
  `git status` には一切出てこないことを確認した。
- `apm compile` 単体では `apm.lock.yaml` に差分が出ないことを確認した(`git checkout` の位置は
  `apm install` 直後で十分という結論の根拠)。
- 存在しないコマンド名で1コマンド目を失敗させても(exit code 127 相当)、`apm.lock.yaml` に
  副作用が残らないことを確認した — apm 未インストールのホストでも tree は汚れず、
  `worktree_setup` の「失敗は warn して続行」という既定の設計(#138)と矛盾しない。

## CI での `apm audit --ci`(任意、[apm-action](https://github.com/microsoft/apm-action))の検討

issue #139 のスコープには「導入するか検討する」までが含まれる。結論: **現時点では見送り、
将来 `.apm/instructions/` の編集頻度が上がったら再検討する。**

理由: `apm audit` はデプロイ済み(コミット済み)ファイルの改ざん・ドリフトを検知する道具だが、
meguri はコンパイル成果物(`.claude/rules/` / `AGENTS.md` など)を一切コミットしない設計
(ADR 0008)なので、CI 上で監査する対象がそもそも存在しない。得られる価値があるとすれば
「`apm.yml` / `.apm/instructions/*.instructions.md` を編集した PR で `apm install --frozen &&
apm compile` が実際に成功するか」という軽量なビルド検証であり、これは `apm audit --ci` ではなく
既存の CI に1ジョブ足すだけで済む(apm-action 導入は不要)。今のところ `.apm/instructions/` の
更新頻度は低く、専用 CI ジョブを足すコストに見合わないため見送る。

## 経緯

- ソース整備: #137(ADR 0008)
- `worktree_setup` フック実装: #138
- meguri 自身のループへの配線・dogfood 検証・本ドキュメント: #139
