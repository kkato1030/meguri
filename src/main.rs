//! meguri v0.1 p1 — Intent / Outcome Graph / Work のデータモデル + 永続化 + CLI。
//!
//! ここで扱うのは「グラフを作る・見る」まで。planning 対話(pane + proposal.json)や
//! 実行系(pane で Work を回す)は後続の増分(p2 以降)。

mod config;
mod db;
mod derive;
#[allow(dead_code)] // p2.2c(`meguri plan` からの起動)で配線する
mod mux;
mod plan;
mod render;
mod store;

use std::io::Write;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use store::Verify;

#[derive(Parser)]
#[command(name = "meguri", version, about = "Turn intent into an outcome graph and coordinate execution and judgment")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Intent (what you want; the root of a graph)
    #[command(subcommand)]
    Intent(IntentCmd),
    /// Outcome (a desired state; a node in the graph)
    #[command(subcommand)]
    Outcome(OutcomeCmd),
    /// Work (a means to satisfy an Outcome)
    #[command(subcommand)]
    Work(WorkCmd),
    /// Show the Outcome Graph (state is derived)
    Graph {
        /// Limit to this Intent (e.g. i1 / 1)
        #[arg(long)]
        intent: Option<String>,
        /// Output as Mermaid
        #[arg(long)]
        mermaid: bool,
    },
    /// Planning (propose via an agent -> proposal.json -> apply on approval)
    #[command(subcommand)]
    Plan(PlanCmd),
}

#[derive(Subcommand)]
enum PlanCmd {
    /// Print the planning prompt to hand to an agent
    Prompt {
        #[arg(long)]
        intent: Option<String>,
        /// Where the agent should write proposal.json (default: MEGURI_HOME/proposal.json)
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Validate proposal.json and show the outcomes it would add
    Diff {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Approve proposal.json and apply it (additive)
    Apply {
        #[arg(long)]
        file: Option<PathBuf>,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum IntentCmd {
    /// e.g. meguri intent add "Make auth production-ready"
    Add {
        /// Title
        title: String,
        #[arg(long, default_value = "")]
        description: String,
    },
    Ls,
}

#[derive(Subcommand)]
enum OutcomeCmd {
    /// e.g. meguri outcome add "Invalid state is rejected" --check "cargo test" --needs o1
    Add {
        /// The desired state ("... is in place")
        statement: String,
        /// Owning Intent (default: the only Intent if there is one; e.g. i1 / 1)
        #[arg(long)]
        intent: Option<String>,
        /// verify=command: command that confirms achievement (exit 0)
        #[arg(long)]
        check: Option<String>,
        /// verify=rollup: milestone (achieved when all children are). Conflicts with --check
        #[arg(long)]
        milestone: bool,
        /// Prerequisite Outcomes (comma-separated, e.g. o1,o2)
        #[arg(long)]
        needs: Option<String>,
    },
    Ls {
        #[arg(long)]
        intent: Option<String>,
    },
    /// Mark as achieved (verify=human only)
    Done { id: String },
    /// Clear the achieved mark
    Undone { id: String },
}

#[derive(Subcommand)]
enum WorkCmd {
    /// e.g. meguri work add "Implement state validation" --for o2
    Add {
        /// What to do
        objective: String,
        /// The Outcome this serves (e.g. o2 / 2)
        #[arg(long)]
        r#for: String,
        /// Who does the implementation phase (ai | human)
        #[arg(long, default_value = "ai")]
        by: String,
    },
    Ls {
        #[arg(long)]
        r#for: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let conn = db::open()?;

    match cli.cmd {
        Cmd::Intent(c) => intent(&conn, c)?,
        Cmd::Outcome(c) => outcome(&conn, c)?,
        Cmd::Work(c) => work(&conn, c)?,
        Cmd::Graph { intent, mermaid } => {
            let iid = intent.map(|s| parse_id(&s, 'i')).transpose()?;
            let outcomes = store::list_outcomes(&conn, iid)?;
            if mermaid {
                print!("{}", render::mermaid(&outcomes));
            } else {
                print!("{}", render::text(&outcomes));
            }
        }
        Cmd::Plan(c) => plan_cmd(&conn, c)?,
    }
    Ok(())
}

fn plan_cmd(conn: &rusqlite::Connection, c: PlanCmd) -> Result<()> {
    let path = |f: Option<PathBuf>| -> Result<PathBuf> {
        match f {
            Some(p) => Ok(p),
            None => plan::default_proposal_path(),
        }
    };
    match c {
        PlanCmd::Prompt { intent, file } => {
            let out = path(file)?;
            let lang = config::load()?.lang;
            let text = plan::prompt(conn, intent.as_deref(), &out, &lang)?;
            print!("{text}");
        }
        PlanCmd::Diff { file } => {
            let p = path(file)?;
            let plan = plan::resolve(conn, &plan::load(&p)?)?;
            print!("{}", plan::diff_text(&plan));
        }
        PlanCmd::Apply { file, yes } => {
            let p = path(file)?;
            let plan = plan::resolve(conn, &plan::load(&p)?)?;
            print!("{}", plan::diff_text(&plan));
            if !yes && !confirm("Apply?")? {
                println!("cancelled");
                return Ok(());
            }
            let ids = plan::apply(conn, &plan)?;
            let list: Vec<String> = ids.iter().map(|i| format!("o{i}")).collect();
            println!("applied: added {}", list.join(", "));
        }
    }
    Ok(())
}

/// 標準入力で y/N を尋ねる。
fn confirm(msg: &str) -> Result<bool> {
    print!("{msg} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

fn intent(conn: &rusqlite::Connection, c: IntentCmd) -> Result<()> {
    match c {
        IntentCmd::Add { title, description } => {
            let id = store::add_intent(conn, &title, &description)?;
            println!("created i{id}");
        }
        IntentCmd::Ls => {
            for it in store::list_intents(conn)? {
                println!("i{}  {}", it.id, it.title);
                if !it.description.is_empty() {
                    println!("     {}", it.description.replace('\n', " "));
                }
            }
        }
    }
    Ok(())
}

fn outcome(conn: &rusqlite::Connection, c: OutcomeCmd) -> Result<()> {
    match c {
        OutcomeCmd::Add { statement, intent, check, milestone, needs } => {
            let iid = resolve_intent(conn, intent.as_deref())?;
            let verify = match (check, milestone) {
                (Some(_), true) => bail!("--check and --milestone cannot be combined"),
                (Some(cmd), false) => Verify::Command(cmd),
                (None, true) => Verify::Rollup,
                (None, false) => Verify::Human, // 既定
            };
            let reqs = parse_id_list(needs.as_deref(), 'o')?;
            let id = store::add_outcome(conn, iid, &statement, &verify, &reqs)?;
            println!("created o{id} ([{}] {})", verify.kind_str(), statement);
        }
        OutcomeCmd::Ls { intent } => {
            let iid = intent.map(|s| parse_id(&s, 'i')).transpose()?;
            for o in store::list_outcomes(conn, iid)? {
                let reqs = if o.requires.is_empty() {
                    String::new()
                } else {
                    let list: Vec<String> = o.requires.iter().map(|r| format!("o{r}")).collect();
                    format!("  ← {}", list.join(", "))
                };
                println!("o{}  [{}] {}{}", o.id, o.verify.kind_str(), o.statement, reqs);
            }
        }
        OutcomeCmd::Done { id } => {
            let oid = parse_id(&id, 'o')?;
            store::set_human_satisfied(conn, oid, true)?;
            println!("marked o{oid} done (human)");
        }
        OutcomeCmd::Undone { id } => {
            let oid = parse_id(&id, 'o')?;
            store::set_human_satisfied(conn, oid, false)?;
            println!("cleared o{oid} done mark");
        }
    }
    Ok(())
}

fn work(conn: &rusqlite::Connection, c: WorkCmd) -> Result<()> {
    match c {
        WorkCmd::Add { objective, r#for, by } => {
            let sid = parse_id(&r#for, 'o')?;
            let id = store::add_work(conn, sid, &objective, &by)?;
            println!("created w{id} (for o{sid}, by {by})");
        }
        WorkCmd::Ls { r#for } => {
            let sid = r#for.map(|s| parse_id(&s, 'o')).transpose()?;
            for w in store::list_works(conn, sid)? {
                println!("w{}  for o{}  [{}/{}]  {}", w.id, w.serves_id, w.executor, w.state, w.objective);
            }
        }
    }
    Ok(())
}

/// outcome add 用: Intent を解決する。明示指定が最優先。省略時は Intent が
/// ちょうど 1 件ならそれ、0 件ならエラー、複数なら --intent を要求する
/// (v2 の「1 件だけ設定済みなら --project 省略可」と同じ発想)。
fn resolve_intent(conn: &rusqlite::Connection, opt: Option<&str>) -> Result<i64> {
    if let Some(s) = opt {
        let id = parse_id(s, 'i')?;
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
            bail!("multiple intents exist; pass --intent ({})", ids.join(", "))
        }
    }
}

/// "o3" でも "3" でも受ける(prefix は任意)。
fn parse_id(s: &str, prefix: char) -> Result<i64> {
    let t = s.trim();
    let digits = t.strip_prefix(prefix).unwrap_or(t);
    digits
        .parse::<i64>()
        .with_context(|| format!("not a valid id: {s:?} (e.g. {prefix}1 or 1)"))
}

/// "o1,o2" → [1, 2]。None/空なら空。
fn parse_id_list(s: Option<&str>, prefix: char) -> Result<Vec<i64>> {
    let Some(s) = s else { return Ok(vec![]) };
    s.split(',')
        .map(|x| x.trim())
        .filter(|x| !x.is_empty())
        .map(|x| parse_id(x, prefix))
        .collect()
}
