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

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde::Deserialize;

use crate::store::{self, Verify};

/// proposal.json の既定パス(`MEGURI_HOME/proposal.json`)。
pub fn default_proposal_path() -> Result<PathBuf> {
    Ok(crate::db::meguri_home()?.join("proposal.json"))
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

/// エージェントに渡す planning プロンプトを組み立てる。
pub fn prompt(conn: &Connection, intent_opt: Option<&str>, out_path: &Path) -> Result<String> {
    let intent_id = resolve_intent(conn, intent_opt)?;
    let it = store::list_intents(conn)?
        .into_iter()
        .find(|i| i.id == intent_id)
        .context("intent が消えた")?;
    let existing = store::list_outcomes(conn, Some(intent_id))?;

    let mut s = String::new();
    s.push_str("あなたは meguri の Planning を手伝う。以下の Intent を、到達したい状態\n");
    s.push_str("(Outcome)の DAG に分解し、提案を JSON で書き出してほしい。\n\n");
    s.push_str(&format!("# Intent (i{})\n{}\n", it.id, it.title));
    if !it.description.is_empty() {
        s.push_str(&format!("{}\n", it.description));
    }
    s.push('\n');

    s.push_str("# 既存の Outcome(重複を避け、前提にする時は下の o<id> で参照)\n");
    if existing.is_empty() {
        s.push_str("(まだ無い)\n");
    } else {
        for o in &existing {
            s.push_str(&format!("- o{} [{}] {}\n", o.id, o.verify.kind_str(), o.statement));
        }
    }
    s.push('\n');

    s.push_str(
        r#"# 依頼
- Intent を「〜されている」という到達状態(Outcome)に分解する。作業(タスク)ではなく状態を書く。
- 各 Outcome には verify(達成をどう確かめるか)を付ける:
    - {"kind":"command","command":"<テスト/検査コマンド>"}  … コマンドが通れば達成
    - {"kind":"human"}                                       … 人が判断(既定)
    - {"kind":"rollup"}                                      … 自分では確かめず、needs が全て達成なら達成(マイルストーン)
- 依存は needs に、proposal 内の ref 名か既存の "o<id>" を並べる。
- 追加のみ(既存の削除・変更はしない)。

# 出力(必ずこの JSON をファイルに書く)
"#,
    );
    s.push_str(&format!("- proposal の \"intent\" は必ず \"i{}\"(この Intent)にする。\n\n", it.id));
    s.push_str(&format!("書き出し先: {}\n\n", out_path.display()));
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
    {{ "ref": "provider", "statement": "OAuth プロバイダ設定が存在する",
      "verify": {{"kind": "human"}} }},
    {{ "ref": "state", "statement": "不正な state が弾かれる",
      "verify": {{"kind": "command", "command": "cargo test state_validation"}},
      "needs": ["provider"] }},
    {{ "ref": "e2e", "statement": "認証が E2E で検証されている",
      "verify": {{"kind": "rollup"}}, "needs": ["state", "provider"] }}
  ]
}}
```"#
    )
}

// ---- load + resolve(検証) ----

pub fn load(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .with_context(|| format!("proposal を読めない: {}(先に `meguri plan prompt`)", path.display()))
}

/// proposal を検証し、追加計画に解決する。
pub fn resolve(conn: &Connection, json: &str) -> Result<Plan> {
    let proposal: Proposal =
        serde_json::from_str(json).context("proposal.json の JSON が壊れている")?;
    if proposal.outcomes.is_empty() {
        bail!("proposal に outcomes が無い");
    }
    let intent_id = resolve_intent(conn, proposal.intent.as_deref())?;

    // ref の一意性チェック。
    let mut refs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for po in &proposal.outcomes {
        if po.r#ref.trim().is_empty() {
            bail!("ref が空の Outcome がある");
        }
        if !refs.insert(po.r#ref.clone()) {
            bail!("ref が重複している: {:?}", po.r#ref);
        }
        if po.statement.trim().is_empty() {
            bail!("statement が空(ref {:?})", po.r#ref);
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
                    format!("needs {:?}(ref {:?})は proposal 内の ref でも既存 o<id> でもない", n, po.r#ref)
                })?;
                let ok = store::get_outcome(conn, id).map(|o| o.intent_id == intent_id).unwrap_or(false);
                if !ok {
                    bail!("needs の o{id}(ref {:?})が存在しないか別 Intent", po.r#ref);
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
    let mut s = format!("i{} に {} 個の Outcome を追加:\n", plan.intent_id, plan.items.len());
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
            format!("  ← {}", list.join(", "))
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
    t.strip_prefix('o').unwrap_or(t).parse::<i64>().with_context(|| format!("o<id> として解釈できない: {s:?}"))
}

/// outcome add と同じ Intent 解決(1 件なら省略可、複数なら要指定)。
fn resolve_intent(conn: &Connection, opt: Option<&str>) -> Result<i64> {
    if let Some(s) = opt {
        let id = s.trim().strip_prefix('i').unwrap_or(s.trim()).parse::<i64>()
            .with_context(|| format!("intent id として解釈できない: {s:?}"))?;
        if !store::intent_exists(conn, id)? {
            bail!("intent i{id} が存在しない");
        }
        return Ok(id);
    }
    let intents = store::list_intents(conn)?;
    match intents.as_slice() {
        [] => bail!("Intent がまだ無い(先に `meguri intent add \"...\"`)"),
        [only] => Ok(only.id),
        many => {
            let ids: Vec<String> = many.iter().map(|i| format!("i{}", i.id)).collect();
            bail!("Intent が複数あるので proposal の \"intent\" か --intent で指定を({})", ids.join(", "))
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
