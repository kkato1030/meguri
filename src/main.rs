//! meguri v0.1 p1 — Intent / Outcome Graph / Work のデータモデル + 永続化 + CLI。
//!
//! ここで扱うのは「グラフを作る・見る」まで。planning 対話(pane + proposal.json)や
//! 実行系(pane で Work を回す)は後続の増分(p2 以降)。

mod db;
mod derive;
mod render;
mod store;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use store::Verify;

#[derive(Parser)]
#[command(name = "meguri", version, about = "Intent を Outcome Graph に変換し実行・判断を管理する")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Intent(実現したいこと。グラフの根)
    #[command(subcommand)]
    Intent(IntentCmd),
    /// Outcome(到達したい状態。グラフのノード)
    #[command(subcommand)]
    Outcome(OutcomeCmd),
    /// Work(Outcome を満たす手段)
    #[command(subcommand)]
    Work(WorkCmd),
    /// Outcome Graph を表示(状態は導出)
    Graph {
        /// この Intent に絞る(例: i1 / 1)
        #[arg(long)]
        intent: Option<String>,
        /// Mermaid で出力
        #[arg(long)]
        mermaid: bool,
    },
}

#[derive(Subcommand)]
enum IntentCmd {
    Add {
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        description: String,
    },
    Ls,
}

#[derive(Subcommand)]
enum OutcomeCmd {
    Add {
        /// 所属 Intent(例: i1 / 1)
        #[arg(long)]
        intent: String,
        /// 到達状態の宣言(「〜されている」)
        #[arg(long)]
        statement: String,
        /// verify=command: 充足を確かめるコマンド(exit 0 で達成)
        #[arg(long)]
        verify_command: Option<String>,
        /// verify=rollup: まとめ節点(子が全て満たされたら達成)。--verify-command とは排他
        #[arg(long)]
        milestone: bool,
        /// 前提 Outcome(カンマ区切り。例: o1,o2)
        #[arg(long)]
        requires: Option<String>,
    },
    Ls {
        #[arg(long)]
        intent: Option<String>,
    },
    /// human 充足表明を立てる(verify=human のみ)
    Satisfy { id: String },
    /// human 充足表明を外す
    Unsatisfy { id: String },
}

#[derive(Subcommand)]
enum WorkCmd {
    Add {
        /// 満たそうとする Outcome(例: o1 / 1)
        #[arg(long)]
        serves: String,
        #[arg(long)]
        objective: String,
        /// 実装フェーズの担当(ai | human)
        #[arg(long, default_value = "ai")]
        executor: String,
    },
    Ls {
        #[arg(long)]
        serves: Option<String>,
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
    }
    Ok(())
}

fn intent(conn: &rusqlite::Connection, c: IntentCmd) -> Result<()> {
    match c {
        IntentCmd::Add { title, description } => {
            let id = store::add_intent(conn, &title, &description)?;
            println!("i{id} を作成");
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
        OutcomeCmd::Add { intent, statement, verify_command, milestone, requires } => {
            let iid = parse_id(&intent, 'i')?;
            let verify = match (verify_command, milestone) {
                (Some(_), true) => bail!("--verify-command と --milestone は同時指定できない"),
                (Some(cmd), false) => Verify::Command(cmd),
                (None, true) => Verify::Rollup,
                (None, false) => Verify::Human, // 既定
            };
            let reqs = parse_id_list(requires.as_deref(), 'o')?;
            let id = store::add_outcome(conn, iid, &statement, &verify, &reqs)?;
            println!("o{id} を作成([{}] {})", verify.kind_str(), statement);
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
        OutcomeCmd::Satisfy { id } => {
            let oid = parse_id(&id, 'o')?;
            store::set_human_satisfied(conn, oid, true)?;
            println!("o{oid} を satisfied(human 表明)に");
        }
        OutcomeCmd::Unsatisfy { id } => {
            let oid = parse_id(&id, 'o')?;
            store::set_human_satisfied(conn, oid, false)?;
            println!("o{oid} の human 表明を外した");
        }
    }
    Ok(())
}

fn work(conn: &rusqlite::Connection, c: WorkCmd) -> Result<()> {
    match c {
        WorkCmd::Add { serves, objective, executor } => {
            let sid = parse_id(&serves, 'o')?;
            let id = store::add_work(conn, sid, &objective, &executor)?;
            println!("w{id} を作成(serves o{sid}, executor {executor})");
        }
        WorkCmd::Ls { serves } => {
            let sid = serves.map(|s| parse_id(&s, 'o')).transpose()?;
            for w in store::list_works(conn, sid)? {
                println!("w{}  serves o{}  [{}/{}]  {}", w.id, w.serves_id, w.executor, w.state, w.objective);
            }
        }
    }
    Ok(())
}

/// "o3" でも "3" でも受ける(prefix は任意)。
fn parse_id(s: &str, prefix: char) -> Result<i64> {
    let t = s.trim();
    let digits = t.strip_prefix(prefix).unwrap_or(t);
    digits
        .parse::<i64>()
        .with_context(|| format!("id として解釈できない: {s:?}(例: {prefix}1 か 1)"))
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
