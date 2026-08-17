# ADR 0001: 配送と projection のレイヤー分離・delivery target は repo 単位

- **Status**: Accepted
- **Date**: 2026-08-17
- **関連**: plan.md §10(Artifact)/ §11(GitHub Integration)/ §3.2(identity 所有)/ §20(抽象後払い)、architecture.md「ドメインモデル」

## Context(なぜ考えるのか)

v0.2 で本線(run → verified → **Artifact** → accept → Outcome satisfied)が一周した。
だが accept 後に残る「Artifact」の実体は **ローカル bare clone 内のブランチ `meguri/w<id>`**
に過ぎず、push もされず PR にもなっていない。「成果物を外へどう出すか」が未定義のまま。

plan.md §11 は projection を **GitHub 前提**で書いている。しかし実運用では次の問いが立つ:

1. 「GitHub を projection 先に使うかどうか」は **どこの設定**になるのか(global? Intent? repo?)。
2. **local git だけ**、あるいは **git すら使わない** 場合はどう動くのか。

これらに答えるため、配送(delivery)まわりのレイヤーと設定所在を確定する。

## Decision(何を決めたか)

### 1. 配送を 2 つの独立レイヤーに分ける

- **workspace / artifact 基盤**(上段): Work をどこで作業させ、成果物を何として残すか。
  現状は **git 固定**(worktree / branch / commit、`gitops.rs`)。
- **projection / integration 面**(下段): 成果物を外へどう出し(Issue/PR/merge)、外の事実を
  どう state に映すか。**GitHub はこの下段の一実装**であって前提ではない。

この 2 段は**独立に選べる**。「git を使うか(上段)」と「GitHub を使うか(下段)」は別問題。

### 2. delivery target は **repo 単位**の宣言とする

- global(`~/.meguri/config.toml` = lang/agent 等の meguri 全体の好み)でも Intent 単位でもなく、
  **repo が自分の配送面を宣言する**(原則「repo は自分自身についてしか語れない」)。
- 住所は次のいずれか(併用可):
  - `repo add <name> --from <origin>` の **origin から導出**(github.com URL → GitHub 可、
    local path → local 扱い)。
  - repo ルートの **`meguri.toml`** に明示: `delivery = "github" | "local" | "none"`。

### 3. 配送は 3 モードとして一般化する(GitHub は 1 モード)

- **① github**: Artifact(branch) → push → **PR** → レビュー → merge。satisfied は「PR merged」の
  inbound projection で入る。Human Gate = PR。
- **② local-git**(forge 無し): Artifact = ローカルブランチ。PR は無い。**Human Gate = `meguri accept`**、
  統合は **ローカル git 操作**(main への merge / origin への push / もしくはブランチのまま保持)。
- **③ git-less**: 上段が git でない。成果物は git change でなくなり、Artifact 抽象が
  「任意の耐久成果物」に一般化する。verify は非 git 状態に対する human/command。
  別の workspace/artifact adapter を挿す形(今の `gitops` がその git 実装)。

`meguri accept` はローカル Human Gate であり、github モードでは PR merge がその projection になる
(plan.md §13 の「GitHub 不在時の Gate」と整合)。

### 4. 抽象は後払い(§20)

adapter 境界(GitHub / 別 forge / git-less の切替)は **2 つ目の実例が現れてから結晶**させる。
最初の実例は **GitHub adapter(v0.3)**。それまで上段は git 固定・下段は無しのまま進める。

## Consequences(結果として何が起きるか)

- **良くなること**
  - 「GitHub を使うか」の設定所在が repo に固定され、global/Intent への漏れを防げる。
  - projection を「GitHub 専用」でなく「3 モードの 1 つ」として設計できる。local-git・git-less の
    運用が最初から視野に入る(将来の非 GitHub / 非 git 事業に耐える)。
  - accept(Human Gate)と統合(merge/PR)が別ステップだと明確化され、v0.2 の現在地
    (「② local-git の統合前」)を正しく言語化できる。
- **引き受けるコスト / 割り切り**
  - 現状は依然 **git 固定・下段未実装**。ADR は方向を固定するだけで、adapter 抽象は v0.3 まで作らない。
  - ② local-git の「統合(main への merge / push)」ステップは未実装。accept 済みブランチは
    宙に浮いたまま(別増分)。
  - repo 側 `meguri.toml` の delivery キーはまだ読んでいない(導入は下段実装と同時でよい)。

## Alternatives considered(検討して採らなかった案)

- **global config で GitHub on/off** — 複数 repo が別々の配送面を持てない。所有境界が meguri 全体に
  漏れ、「repo は自分についてだけ語る」に反する。不採用。
- **Intent 単位で projection を持つ** — Intent は repo に紐づく(§ドメイン)。配送面は repo の性質で
  あって Intent の関心ではない。二重管理になる。不採用。
- **GitHub をハードワイヤ(§11 のまま)** — local-git / git-less を排除してしまう。projection を
  差し替え可能な下段として残す方が、Actor モデル(git 非前提)とも整合する。不採用。
