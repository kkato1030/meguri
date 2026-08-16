//! meguri v0.1 p1 — Intent / Outcome Graph / Work のデータモデル + 永続化 + CLI。
//!
//! ここで扱うのは「グラフを作る・見る」まで。planning 対話(pane + proposal.json)や
//! 実行系(pane で Work を回す)は後続の増分(p2 以降)。

mod db;
mod derive;
mod plan;
mod render;
mod store;

use std::io::Write;
use std::path::PathBuf;

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
    /// Planning 対話(pane で提案 → proposal.json → 承認で反映)
    #[command(subcommand)]
    Plan(PlanCmd),
}

#[derive(Subcommand)]
enum PlanCmd {
    /// エージェントに渡す planning プロンプトを出力
    Prompt {
        #[arg(long)]
        intent: Option<String>,
        /// proposal.json の書き出し先(既定: MEGURI_HOME/proposal.json)
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// proposal.json を検証し、追加される Outcome を表示
    Diff {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// proposal.json を承認して反映(additive)
    Apply {
        #[arg(long)]
        file: Option<PathBuf>,
        /// 確認プロンプトを飛ばす
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum IntentCmd {
    /// 例: meguri intent add "認証を production-ready にする"
    Add {
        /// タイトル
        title: String,
        #[arg(long, default_value = "")]
        description: String,
    },
    Ls,
}

#[derive(Subcommand)]
enum OutcomeCmd {
    /// 例: meguri outcome add "不正な state が弾かれる" --check "cargo test" --needs o1
    Add {
        /// 到達状態の宣言(「〜されている」)
        statement: String,
        /// 所属 Intent(省略時: Intent が 1 件ならそれ。例: i1 / 1)
        #[arg(long)]
        intent: Option<String>,
        /// verify=command: 達成を確かめるコマンド(exit 0 で達成)
        #[arg(long)]
        check: Option<String>,
        /// verify=rollup: まとめ節点(子が全て満たされたら達成)。--check と排他
        #[arg(long)]
        milestone: bool,
        /// 前提 Outcome(カンマ区切り。例: o1,o2)
        #[arg(long)]
        needs: Option<String>,
    },
    Ls {
        #[arg(long)]
        intent: Option<String>,
    },
    /// 達成を表明する(verify=human のみ)
    Done { id: String },
    /// 達成表明を外す
    Undone { id: String },
}

#[derive(Subcommand)]
enum WorkCmd {
    /// 例: meguri work add "state 検証を実装" --for o2
    Add {
        /// 何をするか
        objective: String,
        /// 満たそうとする Outcome(例: o2 / 2)
        #[arg(long)]
        r#for: String,
        /// 実装フェーズの担当(ai | human)
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
            let text = plan::prompt(conn, intent.as_deref(), &out)?;
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
            if !yes && !confirm("反映しますか?")? {
                println!("中止");
                return Ok(());
            }
            let ids = plan::apply(conn, &plan)?;
            let list: Vec<String> = ids.iter().map(|i| format!("o{i}")).collect();
            println!("反映: {} を追加", list.join(", "));
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
        OutcomeCmd::Add { statement, intent, check, milestone, needs } => {
            let iid = resolve_intent(conn, intent.as_deref())?;
            let verify = match (check, milestone) {
                (Some(_), true) => bail!("--check と --milestone は同時指定できない"),
                (Some(cmd), false) => Verify::Command(cmd),
                (None, true) => Verify::Rollup,
                (None, false) => Verify::Human, // 既定
            };
            let reqs = parse_id_list(needs.as_deref(), 'o')?;
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
        OutcomeCmd::Done { id } => {
            let oid = parse_id(&id, 'o')?;
            store::set_human_satisfied(conn, oid, true)?;
            println!("o{oid} を達成に(human 表明)");
        }
        OutcomeCmd::Undone { id } => {
            let oid = parse_id(&id, 'o')?;
            store::set_human_satisfied(conn, oid, false)?;
            println!("o{oid} の達成表明を外した");
        }
    }
    Ok(())
}

fn work(conn: &rusqlite::Connection, c: WorkCmd) -> Result<()> {
    match c {
        WorkCmd::Add { objective, r#for, by } => {
            let sid = parse_id(&r#for, 'o')?;
            let id = store::add_work(conn, sid, &objective, &by)?;
            println!("w{id} を作成(for o{sid}, by {by})");
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
            bail!("Intent が複数あるので --intent で指定を({})", ids.join(", "))
        }
    }
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
