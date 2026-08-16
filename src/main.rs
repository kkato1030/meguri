//! meguri v0.1 p1 — Intent / Outcome Graph / Work のデータモデル + 永続化 + CLI。
//!
//! ここで扱うのは「グラフを作る・見る」まで。planning 対話(pane + proposal.json)や
//! 実行系(pane で Work を回す)は後続の増分(p2 以降)。

mod config;
mod db;
mod derive;
#[allow(dead_code)] // v0.2: o14(spawn)で `meguri run` から配線する
mod gitops;
mod mux;
mod plan;
mod render;
mod store;

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

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
        /// Output as Mermaid (to stdout)
        #[arg(long)]
        mermaid: bool,
        /// Write a self-contained clickable HTML graph and open it in a browser
        #[arg(long)]
        html: bool,
        /// Where to write the HTML (default: MEGURI_HOME/graph.html)
        #[arg(long)]
        out: Option<PathBuf>,
        /// Don't open the HTML in a browser (with --html)
        #[arg(long)]
        no_open: bool,
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
        /// Read this Intent's proposal (proposals/i<N>.json). Symmetric with `plan run --intent`
        #[arg(long)]
        intent: Option<String>,
        /// Read an explicit file instead (overrides --intent)
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Approve proposal.json and apply it (additive)
    Apply {
        #[arg(long)]
        intent: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
    /// Launch an agent in a pane, inject the prompt, wait for the proposal, then apply
    Run {
        #[arg(long)]
        intent: Option<String>,
        /// Override the agent command (default: config `agent`)
        #[arg(long)]
        agent: Option<String>,
        /// Launch + inject, then return immediately (talk in the pane; harvest later with `plan diff/apply --intent`)
        #[arg(long)]
        detach: bool,
        /// Seconds to wait after launching the agent before injecting the prompt
        #[arg(long, default_value_t = 8)]
        grace_secs: u64,
        /// Seconds to wait for the proposal to appear (ignored with --detach)
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
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
    /// Edit an Intent's title / description
    Edit {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Remove an Intent and everything under it (outcomes, edges, works)
    Rm { id: String },
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
        /// Fuller detail (why / what it means / acceptance)
        #[arg(long, default_value = "")]
        description: String,
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
    /// Show one Outcome's full detail (statement, description, verify, deps, state)
    Show { id: String },
    /// Edit an Outcome's statement / description / verify / needs
    Edit {
        id: String,
        #[arg(long)]
        statement: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// Change verify to command
        #[arg(long)]
        check: Option<String>,
        /// Change verify to rollup (milestone)
        #[arg(long)]
        milestone: bool,
        /// Change verify to human
        #[arg(long)]
        human: bool,
        /// Replace the prerequisite set (comma-separated, e.g. o1,o2; empty string clears)
        #[arg(long)]
        needs: Option<String>,
    },
    /// Remove an Outcome (its requires edges both ways and serving works go too)
    Rm { id: String },
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
    /// Edit a Work's objective / executor
    Edit {
        id: String,
        #[arg(long)]
        objective: Option<String>,
        #[arg(long)]
        by: Option<String>,
    },
    /// Remove a Work
    Rm { id: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let conn = db::open()?;

    match cli.cmd {
        Cmd::Intent(c) => intent(&conn, c)?,
        Cmd::Outcome(c) => outcome(&conn, c)?,
        Cmd::Work(c) => work(&conn, c)?,
        Cmd::Graph { intent, mermaid, html, out, no_open } => {
            let iid = intent.map(|s| parse_id(&s, 'i')).transpose()?;
            let outcomes = store::list_outcomes(&conn, iid)?;
            if html {
                let path = match out {
                    Some(p) => p,
                    None => db::meguri_home()?.join("graph.html"),
                };
                std::fs::write(&path, render::html(&outcomes))
                    .with_context(|| format!("cannot write {}", path.display()))?;
                println!("wrote {}", path.display());
                if !no_open {
                    open_in_browser(&path);
                }
            } else if mermaid {
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
    // proposal パスの解決: --file 最優先、次に --intent(session パス)、どちらも
    // 無ければ既定の proposal.json。これで run と diff/apply の --intent が一致する。
    let resolve_path = |file: Option<PathBuf>, intent: Option<&str>| -> Result<PathBuf> {
        if let Some(f) = file {
            Ok(f)
        } else if intent.is_some() {
            plan::session_proposal_path_for(conn, intent)
        } else {
            plan::default_proposal_path()
        }
    };
    match c {
        PlanCmd::Prompt { intent, file } => {
            let out = resolve_path(file, intent.as_deref())?;
            let lang = config::load()?.lang;
            let text = plan::prompt(conn, intent.as_deref(), &out, &lang)?;
            print!("{text}");
        }
        PlanCmd::Diff { intent, file } => {
            let p = resolve_path(file, intent.as_deref())?;
            let plan = plan::resolve(conn, &plan::load(&p)?)?;
            print!("{}", plan::diff_text(&plan));
        }
        PlanCmd::Apply { intent, file, yes } => {
            let p = resolve_path(file, intent.as_deref())?;
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
        PlanCmd::Run { intent, agent, detach, grace_secs, timeout_secs, yes } => {
            let cfg = config::load()?;
            let agent = agent.unwrap_or(cfg.agent);
            let mux = mux::select();
            let rt = plan::Runtimes {
                mux: mux.as_ref(),
                agent: &agent,
                lang: &cfg.lang,
                grace: Duration::from_secs(grace_secs),
                timeout: Duration::from_secs(timeout_secs),
            };
            if detach {
                // launch だけして返す。人間が pane で対話 → あとで harvest する。
                eprintln!("[plan] launching agent in a pane and injecting the prompt...");
                let l = plan::launch(conn, intent.as_deref(), &rt)?;
                println!("launched. talk to the agent in the pane. when it has written a proposal:");
                println!("  meguri plan diff  --intent i{}   # review", l.intent_id);
                println!("  meguri plan apply --intent i{}   # apply", l.intent_id);
                return Ok(());
            }
            eprintln!("[plan] launching agent in a pane and injecting the prompt...");
            eprintln!("[plan] waiting for the proposal (up to {timeout_secs}s). the pane stays alive.");
            let (path, plan) = plan::run(conn, intent.as_deref(), &rt)?;
            match plan {
                None => {
                    println!(
                        "no proposal at {} yet (timed out or the pane exited). the pane is left alive; \
                         finish in it, then run `meguri plan apply --file {}`.",
                        path.display(),
                        path.display()
                    );
                }
                Some(plan) => {
                    print!("{}", plan::diff_text(&plan));
                    if !yes && !confirm("Apply?")? {
                        println!("cancelled (proposal kept at {})", path.display());
                        return Ok(());
                    }
                    let ids = plan::apply(conn, &plan)?;
                    let list: Vec<String> = ids.iter().map(|i| format!("o{i}")).collect();
                    println!("applied: added {}", list.join(", "));
                }
            }
        }
    }
    Ok(())
}

/// ファイルをブラウザで開く(best-effort。失敗しても黙る)。
fn open_in_browser(path: &std::path::Path) {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(opener).arg(path).spawn();
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
        IntentCmd::Edit { id, title, description } => {
            let iid = parse_id(&id, 'i')?;
            if title.is_none() && description.is_none() {
                bail!("nothing to edit (pass --title and/or --description)");
            }
            store::edit_intent(conn, iid, title.as_deref(), description.as_deref())?;
            println!("edited i{iid}");
        }
        IntentCmd::Rm { id } => {
            let iid = parse_id(&id, 'i')?;
            let (outs, works) = store::remove_intent(conn, iid)?;
            println!("removed i{iid} ({outs} outcomes, {works} works)");
        }
    }
    Ok(())
}

fn outcome(conn: &rusqlite::Connection, c: OutcomeCmd) -> Result<()> {
    match c {
        OutcomeCmd::Add { statement, intent, description, check, milestone, needs } => {
            let iid = resolve_intent(conn, intent.as_deref())?;
            let verify = match (check, milestone) {
                (Some(_), true) => bail!("--check and --milestone cannot be combined"),
                (Some(cmd), false) => Verify::Command(cmd),
                (None, true) => Verify::Rollup,
                (None, false) => Verify::Human, // 既定
            };
            let reqs = parse_id_list(needs.as_deref(), 'o')?;
            let id = store::add_outcome(conn, iid, &statement, &description, &verify, &reqs)?;
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
        OutcomeCmd::Show { id } => {
            let oid = parse_id(&id, 'o')?;
            let o = store::get_outcome(conn, oid)?;
            println!("o{}  [{}]  {}", o.id, o.verify.kind_str(), o.statement);
            if !o.description.trim().is_empty() {
                println!("\n{}", o.description);
            }
            if let store::Verify::Command(cmd) = &o.verify {
                println!("\nverify (command): {cmd}");
            }
            if !o.requires.is_empty() {
                let list: Vec<String> = o.requires.iter().map(|r| format!("o{r}")).collect();
                println!("\nneeds: {}", list.join(", "));
            }
        }
        OutcomeCmd::Edit { id, statement, description, check, milestone, human, needs } => {
            let oid = parse_id(&id, 'o')?;
            let verify = match (check, milestone, human) {
                (None, false, false) => None,
                (Some(cmd), false, false) => Some(Verify::Command(cmd)),
                (None, true, false) => Some(Verify::Rollup),
                (None, false, true) => Some(Verify::Human),
                _ => bail!("pass at most one of --check / --milestone / --human"),
            };
            if statement.is_none() && description.is_none() && verify.is_none() && needs.is_none() {
                bail!("nothing to edit (pass --statement / --description / --check|--milestone|--human / --needs)");
            }
            store::edit_outcome(conn, oid, statement.as_deref(), description.as_deref(), verify.as_ref())?;
            if let Some(n) = needs {
                store::set_needs(conn, oid, &parse_id_list(Some(&n), 'o')?)?;
            }
            println!("edited o{oid}");
        }
        OutcomeCmd::Rm { id } => {
            let oid = parse_id(&id, 'o')?;
            let (works, dependents) = store::remove_outcome(conn, oid)?;
            print!("removed o{oid} ({works} works removed");
            if dependents > 0 {
                print!("; {dependents} outcome(s) lost it as a prerequisite");
            }
            println!(")");
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
        WorkCmd::Edit { id, objective, by } => {
            let wid = parse_id(&id, 'w')?;
            if objective.is_none() && by.is_none() {
                bail!("nothing to edit (pass --objective and/or --by)");
            }
            store::edit_work(conn, wid, objective.as_deref(), by.as_deref())?;
            println!("edited w{wid}");
        }
        WorkCmd::Rm { id } => {
            let wid = parse_id(&id, 'w')?;
            store::remove_work(conn, wid)?;
            println!("removed w{wid}");
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
