//! p2.1 Planning 契約(pane なし)。§7 の「文脈を送る → 構造化結果を受け取る」を、
//! ACP でなく **proposal.json のファイル契約**で実現する:
//!
//!   1. `prompt`  — Intent + 現グラフ + proposal.json の書き方を出力(人間がエージェントへ渡す)
//!   2. エージェントが `proposal.json` を書く
//!   3. `diff`    — 追加される Outcome を検証して表示
//!   4. `apply`   — 人間の承認で DB に反映(ref → 新 id を配線)
//!
//! p2.1 は **additive**(Outcome を足すだけ)。削除・変更を伴う宣言的 diff(§14 再計画)は後。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde::Deserialize;

use crate::mux::Mux;
use crate::store::{self, Verify};

/// proposal.json の既定パス(`MEGURI_HOME/proposal.json`)。
pub fn default_proposal_path() -> Result<PathBuf> {
    Ok(crate::db::meguri_home()?.join("proposal.json"))
}

/// Intent ごとの proposal パス(`MEGURI_HOME/proposals/i<N>.json`)。`plan run` 用。
/// Intent 単位に分けることで、複数 Intent の並行計画が衝突しない(§ 単一ファイル問題)。
fn session_proposal_path(intent_id: i64) -> Result<PathBuf> {
    let dir = crate::db::meguri_home()?.join("proposals");
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    Ok(dir.join(format!("i{intent_id}.json")))
}

/// p2.2c: pane を開いてエージェントを起動し、planning プロンプトを注入し、
/// `proposal.json` が書かれるのを待つ。書かれたら検証済み Plan とそのパスを返す。
/// 時間切れ / pane 死亡なら None(パスは案内用に返す)。**pane は残す**(§3.5)。
pub struct Runtimes<'a> {
    pub mux: &'a dyn Mux,
    /// エージェント CLI を起動する 1 行(config.agent)。
    pub agent: &'a str,
    /// statement を書く言語(config.lang)。
    pub lang: &'a str,
    /// CLI 起動から注入までの猶予。
    pub grace: Duration,
    /// proposal 出現を待つ上限。
    pub timeout: Duration,
}

pub fn run(conn: &Connection, intent_opt: Option<&str>, rt: &Runtimes) -> Result<(PathBuf, Option<Plan>)> {
    let intent_id = resolve_intent(conn, intent_opt)?;
    let proposal_path = session_proposal_path(intent_id)?;
    // 前回の残骸を消しておく(新しい出現を検知するため)。
    let _ = std::fs::remove_file(&proposal_path);

    // プロンプトをファイルに書き、エージェントには「これを読んで従え」と伝える。
    let prompt_text = prompt(conn, Some(&format!("i{intent_id}")), &proposal_path, rt.lang)?;
    let prompt_path = crate::db::meguri_home()?.join(format!("plan-prompt-i{intent_id}.md"));
    std::fs::write(&prompt_path, &prompt_text)
        .with_context(|| format!("cannot write {}", prompt_path.display()))?;

    let pane = rt.mux.open_pane("meguri", &format!("plan-i{intent_id}"))?;
    // 生成直後の pane はシェル準備前で最初の送信を落とすことがある(herdr で実測、p2.2b)。
    // エージェント起動は 1 回だけ送りたい(再送すると多重起動する)ので、先に少し待つ。
    std::thread::sleep(Duration::from_millis(800));
    rt.mux.send_line(&pane, rt.agent)?;
    std::thread::sleep(rt.grace);
    rt.mux.send_line(
        &pane,
        &format!("Read {} and complete it (write the JSON proposal as instructed).", prompt_path.display()),
    )?;

    // proposal 出現をポーリング(pane 死亡でも打ち切り)。
    let start = Instant::now();
    let appeared = loop {
        if proposal_path.exists() {
            break true;
        }
        if !rt.mux.is_alive(&pane).unwrap_or(false) {
            break false; // pane が死んだ = エージェント終了/失敗
        }
        if start.elapsed() >= rt.timeout {
            break false;
        }
        std::thread::sleep(Duration::from_millis(500));
    };

    if !appeared {
        return Ok((proposal_path, None));
    }
    let json = load(&proposal_path)?;
    let plan = resolve(conn, &json)?;
    Ok((proposal_path, Some(plan)))
}

// ---- proposal.json のスキーマ(エージェントが書く形) ----

#[derive(Deserialize)]
struct Proposal {
    /// 対象 Intent(例 "i1")。省略時は Intent が 1 件ならそれ。
    #[serde(default)]
    intent: Option<String>,
    outcomes: Vec<ProposedOutcome>,
}

#[derive(Deserialize)]
struct ProposedOutcome {
    /// proposal 内のローカル名(needs から参照する)。
    r#ref: String,
    statement: String,
    #[serde(default)]
    verify: ProposedVerify,
    /// 前提。proposal 内の ref か、既存 Outcome の "o3" を書く。
    #[serde(default)]
    needs: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum ProposedVerify {
    #[default]
    Human,
    Command {
        command: String,
    },
    Rollup,
}

impl From<ProposedVerify> for Verify {
    fn from(p: ProposedVerify) -> Self {
        match p {
            ProposedVerify::Human => Verify::Human,
            ProposedVerify::Command { command } => Verify::Command(command),
            ProposedVerify::Rollup => Verify::Rollup,
        }
    }
}

// ---- 解決済みプラン(検証を通した追加計画) ----

enum NeedRef {
    /// この proposal 内で作られる Outcome(ref 名)。
    Local(String),
    /// 既存の Outcome id。
    Existing(i64),
}

struct PlanItem {
    r#ref: String,
    statement: String,
    verify: Verify,
    needs: Vec<NeedRef>,
}

pub struct Plan {
    intent_id: i64,
    items: Vec<PlanItem>,
}

// ---- prompt(1) ----

/// エージェントに渡す planning プロンプトを組み立てる。`lang` は statement を書く言語
/// (自然言語名。config の lang)。
pub fn prompt(conn: &Connection, intent_opt: Option<&str>, out_path: &Path, lang: &str) -> Result<String> {
    let intent_id = resolve_intent(conn, intent_opt)?;
    let it = store::list_intents(conn)?
        .into_iter()
        .find(|i| i.id == intent_id)
        .context("intent disappeared")?;
    let existing = store::list_outcomes(conn, Some(intent_id))?;

    let mut s = String::new();
    s.push_str("You are helping plan work in meguri. Break the Intent below into a DAG of\n");
    s.push_str("desired states (Outcomes) and write a JSON proposal.\n\n");
    s.push_str(&format!("# Intent (i{})\n{}\n", it.id, it.title));
    if !it.description.is_empty() {
        s.push_str(&format!("{}\n", it.description));
    }
    s.push('\n');

    s.push_str("# Existing Outcomes (avoid duplicates; reference one as a prerequisite by its o<id>)\n");
    if existing.is_empty() {
        s.push_str("(none yet)\n");
    } else {
        for o in &existing {
            s.push_str(&format!("- o{} [{}] {}\n", o.id, o.verify.kind_str(), o.statement));
        }
    }
    s.push('\n');

    s.push_str("# Instructions\n");
    s.push_str("- Decompose the Intent into Outcomes phrased as desired *states* (\"... is in place\"), not tasks.\n");
    s.push_str(&format!("- Write each statement in {lang}.\n"));
    s.push_str(
        r#"- Give each Outcome a `verify` (how we confirm it is achieved):
    - {"kind":"command","command":"<test/check command>"}  - achieved when the command exits 0
    - {"kind":"human"}                                       - a human judges it (default)
    - {"kind":"rollup"}                                      - no check of its own; achieved when all `needs` are (a milestone)
- List prerequisites in `needs`, using a proposal-local `ref` or an existing "o<id>".
- Additive only: do not remove or modify existing Outcomes.
"#,
    );
    s.push_str(&format!("- Set the proposal's \"intent\" to \"i{}\" (this Intent).\n\n", it.id));
    s.push_str(&format!("Write the JSON to: {}\n\n", out_path.display()));
    s.push_str(&schema_example(it.id));
    s.push('\n');
    Ok(s)
}

/// スキーマ例。`"intent"` は今の Intent の id を埋める(別 Intent に書かせないため)。
fn schema_example(intent_id: i64) -> String {
    format!(
        r#"```json
{{
  "intent": "i{intent_id}",
  "outcomes": [
    {{ "ref": "provider", "statement": "OAuth provider is configured",
      "verify": {{"kind": "human"}} }},
    {{ "ref": "state", "statement": "Invalid state is rejected",
      "verify": {{"kind": "command", "command": "cargo test state_validation"}},
      "needs": ["provider"] }},
    {{ "ref": "e2e", "statement": "Auth is verified end-to-end",
      "verify": {{"kind": "rollup"}}, "needs": ["state", "provider"] }}
  ]
}}
```"#
    )
}

// ---- load + resolve(検証) ----

pub fn load(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("cannot read proposal: {} (run `meguri plan prompt` first)", path.display()))
}

/// proposal を検証し、追加計画に解決する。
pub fn resolve(conn: &Connection, json: &str) -> Result<Plan> {
    let proposal: Proposal =
        serde_json::from_str(json).context("proposal.json is not valid JSON")?;
    if proposal.outcomes.is_empty() {
        bail!("proposal has no outcomes");
    }
    let intent_id = resolve_intent(conn, proposal.intent.as_deref())?;

    // ref の一意性チェック。
    let mut refs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for po in &proposal.outcomes {
        if po.r#ref.trim().is_empty() {
            bail!("an outcome has an empty ref");
        }
        if !refs.insert(po.r#ref.clone()) {
            bail!("duplicate ref: {:?}", po.r#ref);
        }
        if po.statement.trim().is_empty() {
            bail!("empty statement (ref {:?})", po.r#ref);
        }
    }

    // needs を解決(proposal 内の ref か、既存の同 intent の Outcome か)。
    let mut items = Vec::new();
    for po in proposal.outcomes {
        let mut needs = Vec::new();
        for n in &po.needs {
            let n = n.trim();
            if refs.contains(n) {
                needs.push(NeedRef::Local(n.to_string()));
            } else {
                let id = parse_o(n).with_context(|| {
                    format!("needs {:?} (ref {:?}) is neither a proposal ref nor an existing o<id>", n, po.r#ref)
                })?;
                let ok = store::get_outcome(conn, id).map(|o| o.intent_id == intent_id).unwrap_or(false);
                if !ok {
                    bail!("needs o{id} (ref {:?}) does not exist or belongs to another intent", po.r#ref);
                }
                needs.push(NeedRef::Existing(id));
            }
        }
        items.push(PlanItem {
            r#ref: po.r#ref,
            statement: po.statement,
            verify: po.verify.into(),
            needs,
        });
    }
    Ok(Plan { intent_id, items })
}

// ---- diff(3) ----

pub fn diff_text(plan: &Plan) -> String {
    let mut s = format!("Add {} outcome(s) to i{}:\n", plan.items.len(), plan.intent_id);
    for it in &plan.items {
        let needs = if it.needs.is_empty() {
            String::new()
        } else {
            let list: Vec<String> = it
                .needs
                .iter()
                .map(|n| match n {
                    NeedRef::Local(r) => format!("@{r}"),
                    NeedRef::Existing(id) => format!("o{id}"),
                })
                .collect();
            format!("  <- {}", list.join(", "))
        };
        s.push_str(&format!("  + [{}] {} (@{}){}\n", it.verify.kind_str(), it.statement, it.r#ref, needs));
    }
    s
}

// ---- apply(4) ----

/// 2 相で反映: まず全 Outcome を作って ref→id を得てから、needs を配線する
/// (ref 同士の前後関係に依存しないため)。
pub fn apply(conn: &Connection, plan: &Plan) -> Result<Vec<i64>> {
    let mut map = std::collections::HashMap::new();
    // 1 相: 作成(requires は後で)。
    for it in &plan.items {
        let id = store::add_outcome(conn, plan.intent_id, &it.statement, &it.verify, &[])?;
        map.insert(it.r#ref.as_str(), id);
    }
    // 2 相: needs を配線(store 側でサイクル検出)。
    for it in &plan.items {
        let oid = map[it.r#ref.as_str()];
        for n in &it.needs {
            let req = match n {
                NeedRef::Local(r) => map[r.as_str()],
                NeedRef::Existing(id) => *id,
            };
            store::add_requires(conn, oid, req)?;
        }
    }
    let mut ids: Vec<i64> = plan.items.iter().map(|it| map[it.r#ref.as_str()]).collect();
    ids.sort_unstable();
    Ok(ids)
}

// ---- helpers ----

fn parse_o(s: &str) -> Result<i64> {
    let t = s.trim();
    t.strip_prefix('o').unwrap_or(t).parse::<i64>().with_context(|| format!("not a valid o<id>: {s:?}"))
}

/// outcome add と同じ Intent 解決(1 件なら省略可、複数なら要指定)。
fn resolve_intent(conn: &Connection, opt: Option<&str>) -> Result<i64> {
    if let Some(s) = opt {
        let id = s.trim().strip_prefix('i').unwrap_or(s.trim()).parse::<i64>()
            .with_context(|| format!("not a valid intent id: {s:?}"))?;
        if !store::intent_exists(conn, id)? {
            bail!("intent i{id} does not exist");
        }
        return Ok(id);
    }
    let intents = store::list_intents(conn)?;
    match intents.as_slice() {
        [] => bail!("no intents yet (run `meguri intent add \"...\"` first)"),
        [only] => Ok(only.id),
        many => {
            let ids: Vec<String> = many.iter().map(|i| format!("i{}", i.id)).collect();
            bail!("multiple intents exist; set proposal \"intent\" or pass --intent ({})", ids.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        // メモリ DB にスキーマだけ用意する(db::open はファイルを開くのでテストでは使わない)。
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        conn.execute_batch(crate::db::SCHEMA).unwrap();
        conn
    }

    #[test]
    fn resolve_and_apply_wires_refs_and_existing() {
        let conn = mem();
        let iid = store::add_intent(&conn, "T", "").unwrap();
        // 既存 o1 を作っておく。
        let o1 = store::add_outcome(&conn, iid, "既存", &Verify::Human, &[]).unwrap();
        assert_eq!(o1, 1);

        let json = r#"{
            "intent": "i1",
            "outcomes": [
                {"ref":"a","statement":"A","verify":{"kind":"human"}},
                {"ref":"b","statement":"B","verify":{"kind":"command","command":"t"},"needs":["a","o1"]}
            ]
        }"#;
        let plan = resolve(&conn, json).unwrap();
        let ids = apply(&conn, &plan).unwrap();
        assert_eq!(ids.len(), 2);

        // b は a(新規)と o1(既存)を requires するはず。
        let all = store::list_outcomes(&conn, Some(iid)).unwrap();
        let b = all.iter().find(|o| o.statement == "B").unwrap();
        let a = all.iter().find(|o| o.statement == "A").unwrap();
        let mut req = b.requires.clone();
        req.sort_unstable();
        let mut want = vec![a.id, o1];
        want.sort_unstable();
        assert_eq!(req, want);
    }

    #[test]
    fn rejects_unknown_need() {
        let conn = mem();
        store::add_intent(&conn, "T", "").unwrap();
        let json = r#"{"outcomes":[{"ref":"a","statement":"A","needs":["nope"]}]}"#;
        assert!(resolve(&conn, json).is_err());
    }

    #[test]
    fn rejects_duplicate_ref() {
        let conn = mem();
        store::add_intent(&conn, "T", "").unwrap();
        let json = r#"{"outcomes":[
            {"ref":"a","statement":"A"},
            {"ref":"a","statement":"B"}
        ]}"#;
        assert!(resolve(&conn, json).is_err());
    }

    #[test]
    fn verify_defaults_to_human() {
        let conn = mem();
        store::add_intent(&conn, "T", "").unwrap();
        let json = r#"{"outcomes":[{"ref":"a","statement":"A"}]}"#;
        let plan = resolve(&conn, json).unwrap();
        assert_eq!(plan.items[0].verify, Verify::Human);
    }
}
