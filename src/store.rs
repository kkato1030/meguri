//! ドメイン型と CRUD。ここは「事実の読み書き」だけを担い、導出(satisfied/ready/
//! blocked)は持たない —— それは derive.rs の仕事。

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};

// ---- 型 ----

#[derive(Debug, Clone)]
pub struct Intent {
    pub id: i64,
    pub title: String,
    pub description: String,
}

/// verify の種類(§4)。command/human/rollup の 3 つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verify {
    /// シェルコマンドが exit 0 なら達成(担当 Work が通り、かつマージされたとき)。
    Command(String),
    /// 人が「達成」と表明したら達成(sticky)。
    Human,
    /// まとめ節点。子(requires)が全て satisfied なら達成。自分では確かめない。
    Rollup,
}

impl Verify {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Verify::Command(_) => "command",
            Verify::Human => "human",
            Verify::Rollup => "rollup",
        }
    }
    fn command_str(&self) -> Option<&str> {
        match self {
            Verify::Command(c) => Some(c),
            _ => None,
        }
    }
    fn from_row(kind: &str, command: Option<String>) -> Result<Verify> {
        Ok(match kind {
            "command" => Verify::Command(command.unwrap_or_default()),
            "human" => Verify::Human,
            "rollup" => Verify::Rollup,
            other => bail!("unknown verify.kind: {other}"),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub id: i64,
    /// 所属 Intent。永続モデルの一部。表示・絞り込みは list 側で行うため直接読む箇所はまだ無い。
    #[allow(dead_code)]
    pub intent_id: i64,
    pub statement: String,
    /// 詳しい説明(なぜ / 何を意味するか / 受け入れの詳細)。任意。Intent の description と対称。
    pub description: String,
    pub verify: Verify,
    /// human 充足表明(sticky)。kind=human の satisfied 判定に使う事実。
    pub human_satisfied: bool,
    /// 前提となる Outcome の id(requires 辺)。
    pub requires: Vec<i64>,
}

#[derive(Debug, Clone)]
pub struct Work {
    pub id: i64,
    pub serves_id: i64,
    pub objective: String,
    pub executor: String, // 'ai' | 'human'
    pub state: String,
}

// ---- Intent ----

pub fn add_intent(conn: &Connection, title: &str, description: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO intents (title, description) VALUES (?1, ?2)",
        params![title, description],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_intents(conn: &Connection) -> Result<Vec<Intent>> {
    let mut stmt = conn.prepare("SELECT id, title, description FROM intents ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Intent { id: r.get(0)?, title: r.get(1)?, description: r.get(2)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn intent_exists(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.query_row("SELECT 1 FROM intents WHERE id = ?1", [id], |_| Ok(())).is_ok())
}

// ---- Outcome ----

pub fn add_outcome(
    conn: &Connection,
    intent_id: i64,
    statement: &str,
    description: &str,
    verify: &Verify,
    requires: &[i64],
) -> Result<i64> {
    if !intent_exists(conn, intent_id)? {
        bail!("intent i{intent_id} does not exist");
    }
    // 前提の存在チェック(同じ intent 内)。
    for &req in requires {
        let ok: bool = conn
            .query_row("SELECT intent_id = ?2 FROM outcomes WHERE id = ?1", params![req, intent_id], |r| r.get(0))
            .optional_bool()?;
        if !ok {
            bail!("required o{req} does not exist or belongs to another intent");
        }
    }
    conn.execute(
        "INSERT INTO outcomes (intent_id, statement, description, verify_kind, verify_command) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![intent_id, statement, description, verify.kind_str(), verify.command_str()],
    )?;
    let id = conn.last_insert_rowid();
    for &req in requires {
        add_requires(conn, id, req)?;
    }
    Ok(id)
}

/// requires 辺を足す。DAG を保つため、逆到達(サイクル)を弾く。
pub fn add_requires(conn: &Connection, outcome_id: i64, requires_id: i64) -> Result<()> {
    if outcome_id == requires_id {
        bail!("o{outcome_id} cannot require itself");
    }
    // requires_id から outcome_id に到達できるなら、この辺はサイクルを作る。
    if reachable(conn, requires_id, outcome_id)? {
        bail!("o{outcome_id} -> o{requires_id} would create a cycle (DAG violation)");
    }
    conn.execute(
        "INSERT OR IGNORE INTO outcome_requires (outcome_id, requires_id) VALUES (?1, ?2)",
        params![outcome_id, requires_id],
    )?;
    Ok(())
}

/// `from` から requires 辺をたどって `target` に到達できるか(サイクル検出用)。
fn reachable(conn: &Connection, from: i64, target: i64) -> Result<bool> {
    let mut stack = vec![from];
    let mut seen = std::collections::HashSet::new();
    while let Some(cur) = stack.pop() {
        if cur == target {
            return Ok(true);
        }
        if !seen.insert(cur) {
            continue;
        }
        for req in requires_of(conn, cur)? {
            stack.push(req);
        }
    }
    Ok(false)
}

pub fn requires_of(conn: &Connection, outcome_id: i64) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT requires_id FROM outcome_requires WHERE outcome_id = ?1 ORDER BY requires_id")?;
    let rows = stmt
        .query_map([outcome_id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(rows)
}

pub fn get_outcome(conn: &Connection, id: i64) -> Result<Outcome> {
    let (intent_id, statement, description, kind, command, human): (i64, String, String, String, Option<String>, bool) = conn
        .query_row(
            "SELECT intent_id, statement, description, verify_kind, verify_command, human_satisfied FROM outcomes WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get::<_, i64>(5)? != 0)),
        )
        .with_context(|| format!("no outcome o{id}"))?;
    Ok(Outcome {
        id,
        intent_id,
        statement,
        description,
        verify: Verify::from_row(&kind, command)?,
        human_satisfied: human,
        requires: requires_of(conn, id)?,
    })
}

/// intent 配下(intent 未指定なら全部)の Outcome を id 順で。
pub fn list_outcomes(conn: &Connection, intent_id: Option<i64>) -> Result<Vec<Outcome>> {
    let ids: Vec<i64> = match intent_id {
        Some(iid) => {
            let mut stmt = conn.prepare("SELECT id FROM outcomes WHERE intent_id = ?1 ORDER BY id")?;
            let rows = stmt.query_map([iid], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
            rows
        }
        None => {
            let mut stmt = conn.prepare("SELECT id FROM outcomes ORDER BY id")?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<i64>>>()?;
            rows
        }
    };
    ids.into_iter().map(|id| get_outcome(conn, id)).collect()
}

/// human 充足表明を立てる/外す(kind=human のときだけ意味を持つ)。
pub fn set_human_satisfied(conn: &Connection, id: i64, value: bool) -> Result<()> {
    let o = get_outcome(conn, id)?;
    if o.verify != Verify::Human {
        bail!("o{id} has verify.kind={}, so a human mark is invalid (human only)", o.verify.kind_str());
    }
    conn.execute(
        "UPDATE outcomes SET human_satisfied = ?2 WHERE id = ?1",
        params![id, value as i64],
    )?;
    Ok(())
}

// ---- Work ----

pub fn add_work(conn: &Connection, serves_id: i64, objective: &str, executor: &str) -> Result<i64> {
    get_outcome(conn, serves_id).with_context(|| format!("no outcome o{serves_id} to serve"))?;
    if executor != "ai" && executor != "human" {
        bail!("executor must be ai or human (got: {executor})");
    }
    conn.execute(
        "INSERT INTO works (serves_id, objective, executor) VALUES (?1, ?2, ?3)",
        params![serves_id, objective, executor],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn list_works(conn: &Connection, serves_id: Option<i64>) -> Result<Vec<Work>> {
    let map = |r: &rusqlite::Row| {
        Ok(Work {
            id: r.get(0)?,
            serves_id: r.get(1)?,
            objective: r.get(2)?,
            executor: r.get(3)?,
            state: r.get(4)?,
        })
    };
    let rows: Vec<Work> = match serves_id {
        Some(sid) => {
            let mut stmt = conn.prepare(
                "SELECT id, serves_id, objective, executor, state FROM works WHERE serves_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map([sid], map)?.collect::<rusqlite::Result<Vec<Work>>>()?;
            rows
        }
        None => {
            let mut stmt = conn
                .prepare("SELECT id, serves_id, objective, executor, state FROM works ORDER BY id")?;
            let rows = stmt.query_map([], map)?.collect::<rusqlite::Result<Vec<Work>>>()?;
            rows
        }
    };
    Ok(rows)
}

// ---- 小さなヘルパ ----

trait OptionalBool {
    fn optional_bool(self) -> Result<bool>;
}
impl OptionalBool for rusqlite::Result<bool> {
    /// 行が無ければ false、あればその真偽。
    fn optional_bool(self) -> Result<bool> {
        match self {
            Ok(b) => Ok(b),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}
