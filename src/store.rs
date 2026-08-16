//! ドメイン型と CRUD。ここは「事実の読み書き」だけを担い、導出(satisfied/ready/
//! blocked)は持たない —— それは derive.rs の仕事。

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};

// ---- 型 ----

#[derive(Debug, Clone)]
pub struct Repo {
    /// o14(Intent↔repo 紐付け)で使う。
    #[allow(dead_code)]
    pub id: i64,
    pub name: String,
    pub origin: String,
    pub default_branch: String,
}

#[derive(Debug, Clone)]
pub struct Intent {
    pub id: i64,
    pub title: String,
    pub description: String,
    /// どの repo で実行するか(o14 で紐付け。未紐付けは None)。
    pub repo_id: Option<i64>,
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
    /// spawn 時に埋まる worktree 情報(§9 の作業場)。
    pub worktree_path: Option<String>,
    #[allow(dead_code)]
    pub branch: Option<String>,
    #[allow(dead_code)]
    pub base_sha: Option<String>,
}

// ---- Repo ----

pub fn add_repo(conn: &Connection, name: &str, origin: &str, default_branch: &str) -> Result<i64> {
    if name.trim().is_empty() {
        bail!("repo name cannot be empty");
    }
    conn.execute(
        "INSERT INTO repos (name, origin, default_branch) VALUES (?1, ?2, ?3)",
        params![name, origin, default_branch],
    )
    .with_context(|| format!("cannot add repo {name:?} (name already used?)"))?;
    Ok(conn.last_insert_rowid())
}

pub fn list_repos(conn: &Connection) -> Result<Vec<Repo>> {
    let mut stmt = conn.prepare("SELECT id, name, origin, default_branch FROM repos ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Repo { id: r.get(0)?, name: r.get(1)?, origin: r.get(2)?, default_branch: r.get(3)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_repo_by_name(conn: &Connection, name: &str) -> Result<Repo> {
    conn.query_row(
        "SELECT id, name, origin, default_branch FROM repos WHERE name = ?1",
        [name],
        |r| Ok(Repo { id: r.get(0)?, name: r.get(1)?, origin: r.get(2)?, default_branch: r.get(3)? }),
    )
    .with_context(|| format!("no repo named {name:?}"))
}

pub fn remove_repo(conn: &Connection, name: &str) -> Result<()> {
    if conn.execute("DELETE FROM repos WHERE name = ?1", [name])? == 0 {
        bail!("no repo named {name:?}");
    }
    Ok(())
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
    let mut stmt = conn.prepare("SELECT id, title, description, repo_id FROM intents ORDER BY id")?;
    let rows = stmt
        .query_map([], |r| {
            Ok(Intent { id: r.get(0)?, title: r.get(1)?, description: r.get(2)?, repo_id: r.get(3)? })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn get_intent(conn: &Connection, id: i64) -> Result<Intent> {
    conn.query_row(
        "SELECT id, title, description, repo_id FROM intents WHERE id = ?1",
        [id],
        |r| Ok(Intent { id: r.get(0)?, title: r.get(1)?, description: r.get(2)?, repo_id: r.get(3)? }),
    )
    .with_context(|| format!("no intent i{id}"))
}

/// Intent に repo を紐付ける(o14)。
pub fn set_intent_repo(conn: &Connection, intent_id: i64, repo_id: i64) -> Result<()> {
    if !intent_exists(conn, intent_id)? {
        bail!("intent i{intent_id} does not exist");
    }
    conn.execute("UPDATE intents SET repo_id = ?2 WHERE id = ?1", params![intent_id, repo_id])?;
    Ok(())
}

pub fn get_repo(conn: &Connection, id: i64) -> Result<Repo> {
    conn.query_row(
        "SELECT id, name, origin, default_branch FROM repos WHERE id = ?1",
        [id],
        |r| Ok(Repo { id: r.get(0)?, name: r.get(1)?, origin: r.get(2)?, default_branch: r.get(3)? }),
    )
    .with_context(|| format!("no repo with id {id}"))
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

/// statement / description / verify を書き換える(指定したものだけ)。needs は set_needs で。
pub fn edit_outcome(
    conn: &Connection,
    id: i64,
    statement: Option<&str>,
    description: Option<&str>,
    verify: Option<&Verify>,
) -> Result<()> {
    get_outcome(conn, id)?; // 存在確認
    if let Some(s) = statement {
        if s.trim().is_empty() {
            bail!("statement cannot be empty");
        }
        conn.execute("UPDATE outcomes SET statement = ?2 WHERE id = ?1", params![id, s])?;
    }
    if let Some(d) = description {
        conn.execute("UPDATE outcomes SET description = ?2 WHERE id = ?1", params![id, d])?;
    }
    if let Some(v) = verify {
        conn.execute(
            "UPDATE outcomes SET verify_kind = ?2, verify_command = ?3 WHERE id = ?1",
            params![id, v.kind_str(), v.command_str()],
        )?;
    }
    Ok(())
}

/// requires(前提の辺)を丸ごと置き換える。存在・同一 intent・サイクルを検証する。
pub fn set_needs(conn: &Connection, id: i64, needs: &[i64]) -> Result<()> {
    let o = get_outcome(conn, id)?;
    for &req in needs {
        let ok: bool = conn
            .query_row("SELECT intent_id = ?2 FROM outcomes WHERE id = ?1", params![req, o.intent_id], |r| r.get(0))
            .optional_bool()?;
        if !ok {
            bail!("required o{req} does not exist or belongs to another intent");
        }
    }
    conn.execute("DELETE FROM outcome_requires WHERE outcome_id = ?1", params![id])?;
    for &req in needs {
        add_requires(conn, id, req)?; // 自己参照・サイクルはここで弾く
    }
    Ok(())
}

/// Outcome を削除する。両方向の requires 辺と、それを serves する Work も消す。
/// (id, 消した辺数, 消した Work 数, 前提を失った依存側の数)を返す。
pub fn remove_outcome(conn: &Connection, id: i64) -> Result<(usize, usize)> {
    get_outcome(conn, id)?; // 存在確認
    let dependents: usize = conn
        .query_row("SELECT COUNT(*) FROM outcome_requires WHERE requires_id = ?1", params![id], |r| r.get::<_, i64>(0))
        .map(|n| n as usize)?;
    let works = conn.execute("DELETE FROM works WHERE serves_id = ?1", params![id])?;
    // FK があるので、参照している辺(両方向)を先に消す。
    conn.execute("DELETE FROM outcome_requires WHERE outcome_id = ?1 OR requires_id = ?1", params![id])?;
    conn.execute("DELETE FROM outcomes WHERE id = ?1", params![id])?;
    Ok((works, dependents))
}

// ---- Intent の edit / remove ----

pub fn edit_intent(conn: &Connection, id: i64, title: Option<&str>, description: Option<&str>) -> Result<()> {
    if !intent_exists(conn, id)? {
        bail!("intent i{id} does not exist");
    }
    if let Some(t) = title {
        if t.trim().is_empty() {
            bail!("title cannot be empty");
        }
        conn.execute("UPDATE intents SET title = ?2 WHERE id = ?1", params![id, t])?;
    }
    if let Some(d) = description {
        conn.execute("UPDATE intents SET description = ?2 WHERE id = ?1", params![id, d])?;
    }
    Ok(())
}

/// Intent とその配下(Outcome / requires 辺 / Work)を丸ごと削除する。
/// requires は同一 intent 内なので、部分グラフとして自己完結に消える。
/// (消した Outcome 数, Work 数)を返す。
pub fn remove_intent(conn: &Connection, id: i64) -> Result<(usize, usize)> {
    if !intent_exists(conn, id)? {
        bail!("intent i{id} does not exist");
    }
    // FK 順に: works → requires 辺 → outcomes → intent。
    let works = conn.execute(
        "DELETE FROM works WHERE serves_id IN (SELECT id FROM outcomes WHERE intent_id = ?1)",
        params![id],
    )?;
    conn.execute(
        "DELETE FROM outcome_requires WHERE outcome_id IN (SELECT id FROM outcomes WHERE intent_id = ?1)",
        params![id],
    )?;
    let outs = conn.execute("DELETE FROM outcomes WHERE intent_id = ?1", params![id])?;
    conn.execute("DELETE FROM intents WHERE id = ?1", params![id])?;
    Ok((outs, works))
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
            worktree_path: r.get(5)?,
            branch: r.get(6)?,
            base_sha: r.get(7)?,
        })
    };
    const COLS: &str = "id, serves_id, objective, executor, state, worktree_path, branch, base_sha";
    let rows: Vec<Work> = match serves_id {
        Some(sid) => {
            let mut stmt = conn
                .prepare(&format!("SELECT {COLS} FROM works WHERE serves_id = ?1 ORDER BY id"))?;
            let rows = stmt.query_map([sid], map)?.collect::<rusqlite::Result<Vec<Work>>>()?;
            rows
        }
        None => {
            let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM works ORDER BY id"))?;
            let rows = stmt.query_map([], map)?.collect::<rusqlite::Result<Vec<Work>>>()?;
            rows
        }
    };
    Ok(rows)
}

/// Work の状態を更新する(planned → running → …)。
pub fn set_work_state(conn: &Connection, id: i64, state: &str) -> Result<()> {
    conn.execute("UPDATE works SET state = ?2 WHERE id = ?1", params![id, state])?;
    Ok(())
}

/// spawn 時に worktree 情報を Work に書き込む(§9 の作業場)。
pub fn set_work_worktree(conn: &Connection, id: i64, path: &str, branch: &str, base_sha: &str) -> Result<()> {
    conn.execute(
        "UPDATE works SET worktree_path = ?2, branch = ?3, base_sha = ?4 WHERE id = ?1",
        params![id, path, branch, base_sha],
    )?;
    Ok(())
}

fn work_exists(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.query_row("SELECT 1 FROM works WHERE id = ?1", [id], |_| Ok(())).is_ok())
}

pub fn get_work(conn: &Connection, id: i64) -> Result<Work> {
    conn.query_row(
        "SELECT id, serves_id, objective, executor, state, worktree_path, branch, base_sha FROM works WHERE id = ?1",
        [id],
        |r| Ok(Work {
            id: r.get(0)?, serves_id: r.get(1)?, objective: r.get(2)?, executor: r.get(3)?,
            state: r.get(4)?, worktree_path: r.get(5)?, branch: r.get(6)?, base_sha: r.get(7)?,
        }),
    )
    .with_context(|| format!("no work w{id}"))
}

pub fn edit_work(conn: &Connection, id: i64, objective: Option<&str>, executor: Option<&str>) -> Result<()> {
    if !work_exists(conn, id)? {
        bail!("work w{id} does not exist");
    }
    if let Some(o) = objective {
        conn.execute("UPDATE works SET objective = ?2 WHERE id = ?1", params![id, o])?;
    }
    if let Some(e) = executor {
        if e != "ai" && e != "human" {
            bail!("executor must be ai or human (got: {e})");
        }
        conn.execute("UPDATE works SET executor = ?2 WHERE id = ?1", params![id, e])?;
    }
    Ok(())
}

/// Work を削除する(何からも参照されないので単純に消える)。
pub fn remove_work(conn: &Connection, id: i64) -> Result<()> {
    if conn.execute("DELETE FROM works WHERE id = ?1", params![id])? == 0 {
        bail!("work w{id} does not exist");
    }
    Ok(())
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
