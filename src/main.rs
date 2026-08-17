//! meguri v0.1 p1 — Intent / Outcome Graph / Work のデータモデル + 永続化 + CLI。
//!
//! ここで扱うのは「グラフを作る・見る」まで。planning 対話(pane + proposal.json)や
//! 実行系(pane で Work を回す)は後続の増分(p2 以降)。

mod config;
mod db;
mod derive;
mod exec;
mod gitops;
mod mux;
mod plan;
mod render;
mod store;
mod verify;

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
    /// Repo (a managed bare clone that Works run against)
    #[command(subcommand)]
    Repo(RepoCmd),
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
    /// Run a ready Outcome: spawn a Work, launch an agent in its worktree, wait for the result
    Run {
        /// The Outcome to work on (must be ready; e.g. o13)
        outcome: String,
        /// Override the agent command (default: config `agent`)
        #[arg(long)]
        agent: Option<String>,
        /// Launch + inject, then return immediately (state stays 'running'; check the pane yourself)
        #[arg(long)]
        detach: bool,
        /// Seconds to wait after launching the agent before injecting the prompt
        #[arg(long, default_value_t = 8)]
        grace_secs: u64,
        /// Seconds to wait for .meguri/result.json to appear (ignored with --detach)
        #[arg(long, default_value_t = 600)]
        timeout_secs: u64,
        /// Seconds between re-injections if the agent hasn't produced a result yet (cold-start insurance)
        #[arg(long, default_value_t = 30)]
        nudge_secs: u64,
    },
    /// Accept a verified Work (local Human Gate): its Outcome becomes satisfied
    Accept {
        /// The Work to accept (must be verified; e.g. w3)
        work: String,
    },
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
enum RepoCmd {
    /// Register a repo and create its managed bare clone
    Add {
        /// A short name (used for paths and to bind Intents)
        name: String,
        /// Clone source (a git URL or a local path)
        #[arg(long)]
        from: String,
        /// Default branch to cut worktrees from
        #[arg(long, default_value = "main")]
        branch: String,
    },
    Ls,
    /// Update the bare clone from origin
    Fetch { name: String },
    /// Remove the repo and its bare clone
    Rm { name: String },
}

#[derive(Subcommand)]
enum IntentCmd {
    /// e.g. meguri intent add "Make auth production-ready" --repo meguri
    Add {
        /// Title
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        /// Bind to a registered repo (name)
        #[arg(long)]
        repo: Option<String>,
    },
    Ls,
    /// Edit an Intent's title / description / repo
    Edit {
        id: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// Bind to a registered repo (name)
        #[arg(long)]
        repo: Option<String>,
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
        Cmd::Repo(c) => repo(&conn, c)?,
        Cmd::Intent(c) => intent(&conn, c)?,
        Cmd::Outcome(c) => outcome(&conn, c)?,
        Cmd::Work(c) => work(&conn, c)?,
        Cmd::Graph { intent, mermaid, html, out, no_open } => {
            let iid = intent.map(|s| parse_id(&s, 'i')).transpose()?;
            let outcomes = store::list_outcomes(&conn, iid)?;
            let accepted = store::accepted_outcome_ids(&conn)?;
            if html {
                let path = match out {
                    Some(p) => p,
                    None => db::meguri_home()?.join("graph.html"),
                };
                std::fs::write(&path, render::html(&outcomes, &accepted))
                    .with_context(|| format!("cannot write {}", path.display()))?;
                println!("wrote {}", path.display());
                if !no_open {
                    open_in_browser(&path);
                }
            } else if mermaid {
                print!("{}", render::mermaid(&outcomes, &accepted));
            } else {
                print!("{}", render::text(&outcomes, &accepted));
            }
        }
        Cmd::Plan(c) => plan_cmd(&conn, c)?,
        Cmd::Run { outcome, agent, detach, grace_secs, timeout_secs, nudge_secs } => run_cmd(
            &conn,
            &outcome,
            &RunOpts {
                agent,
                detach,
                grace: Duration::from_secs(grace_secs),
                timeout: Duration::from_secs(timeout_secs),
                nudge: Duration::from_secs(nudge_secs),
            },
        )?,
        Cmd::Accept { work } => accept_cmd(&conn, &work)?,
    }
    Ok(())
}

/// ローカル Human Gate: verified な Work を accept する。
/// その serve 先 Outcome が satisfied になり(導出)、後続 Outcome が ready になる。
fn accept_cmd(conn: &rusqlite::Connection, work: &str) -> Result<()> {
    let wid = parse_id(work, 'w')?;
    let w = store::get_work(conn, wid)?;
    if w.state != "verified" {
        bail!("w{wid} is [{}], not verified — only verified Work can be accepted", w.state);
    }
    store::set_work_state(conn, wid, "accepted")?;
    println!("accepted w{wid} → o{} is now satisfied", w.serves_id);

    // 後続で新たに ready になった Outcome を案内する(リングが繋がったことの確認)。
    let o = store::get_outcome(conn, w.serves_id)?;
    let outcomes = store::list_outcomes(conn, Some(o.intent_id))?;
    let accepted = store::accepted_outcome_ids(conn)?;
    let states = derive::states(&outcomes, &accepted);
    let mut newly_ready: Vec<i64> = outcomes
        .iter()
        .filter(|x| x.requires.contains(&w.serves_id) && states.get(&x.id) == Some(&derive::State::Ready))
        .map(|x| x.id)
        .collect();
    newly_ready.sort();
    if newly_ready.is_empty() {
        println!("  no newly-ready outcomes (this may complete a branch of the graph)");
    } else {
        let list: Vec<String> = newly_ready.iter().map(|id| format!("o{id}")).collect();
        println!("  now ready: {}  (run one with `meguri run <o>`)", list.join(", "));
    }
    Ok(())
}

/// `meguri run` の実行オプション(o15 の起動・o16 の待機を制御)。
struct RunOpts {
    agent: Option<String>,
    detach: bool,
    grace: Duration,
    /// 初回注入が落ちた場合の保険。この間隔で上限付き再注入する。
    nudge: Duration,
    timeout: Duration,
}

/// o14: ready な Outcome を Work にし、その repo の bare から隔離 worktree を切る。
fn run_cmd(conn: &rusqlite::Connection, outcome: &str, opts: &RunOpts) -> Result<()> {
    let oid = parse_id(outcome, 'o')?;
    let o = store::get_outcome(conn, oid)?;
    if o.verify == Verify::Rollup {
        bail!("o{oid} is a milestone (rollup); it has no work to run");
    }

    // repo を解決(Intent に紐付いていること)。
    let it = store::get_intent(conn, o.intent_id)?;
    let repo_id = it.repo_id.with_context(|| {
        format!("intent i{} has no repo; bind one: meguri intent edit i{} --repo <name>", it.id, it.id)
    })?;
    let repo = store::get_repo(conn, repo_id)?;

    // ready 判定(その Intent の導出状態)。
    let outcomes = store::list_outcomes(conn, Some(o.intent_id))?;
    let accepted = store::accepted_outcome_ids(conn)?;
    let states = derive::states(&outcomes, &accepted);
    match states.get(&oid) {
        Some(derive::State::Ready) => {}
        Some(s) => bail!("o{oid} is not ready (currently {})", s.label()),
        None => bail!("o{oid} not found in its intent"),
    }

    // bare を最新化 → Work を作る → worktree を切って紐付け(失敗なら Work を戻す)。
    let bare = bare_path(&repo.name)?;
    gitops::fetch(&bare)?;
    let wid = store::add_work(conn, oid, &o.statement, "ai")?;
    let key = format!("w{wid}");
    let wt_parent = db::worktrees_dir()?.join(&repo.name);
    let wt = match gitops::create_worktree(&bare, &repo.default_branch, &wt_parent, &key) {
        Ok(wt) => wt,
        Err(e) => {
            let _ = store::remove_work(conn, wid);
            return Err(e);
        }
    };
    store::set_work_worktree(
        conn,
        wid,
        wt.path.to_str().unwrap_or_default(),
        &wt.branch,
        &wt.base_sha,
    )?;

    let short = wt.base_sha.chars().take(7).collect::<String>();
    println!("spawned w{wid} for o{oid} (repo {})", repo.name);
    println!("  worktree: {}", wt.path.display());
    println!("  branch:   {} (base {short})", wt.branch);

    // o15: worktree の pane でエージェントを起動し、実装プロンプトを注入する。
    // o16: --detach でなければ result.json の出現を待って完了を判定する。
    launch_work(conn, wid, &o, &wt.path, &wt.base_sha, &wt.branch, opts)?;
    Ok(())
}

/// o15/o16: Work の worktree で pane を開き、実装プロンプトを注入してエージェントを起こし、
/// (--detach でなければ)耐久 result ファイルの出現を待って完了を判定する。pane は残す(§3.5)。
fn launch_work(
    conn: &rusqlite::Connection,
    wid: i64,
    o: &store::Outcome,
    worktree: &std::path::Path,
    base_sha: &str,
    branch: &str,
    opts: &RunOpts,
) -> Result<()> {
    let cfg = config::load()?;
    let agent_cmd = opts.agent.clone().unwrap_or(cfg.agent);
    let scratch = worktree.join(".meguri");
    std::fs::create_dir_all(&scratch)
        .with_context(|| format!("cannot create {}", scratch.display()))?;
    let result_path = scratch.join("result.json");
    let prompt_path = scratch.join("prompt.md");
    let _ = std::fs::remove_file(&result_path); // 古い残骸を消す(消さないと前回の result を即検知してしまう)

    let prompt = exec::impl_prompt(o, worktree, &result_path, &cfg.lang);
    std::fs::write(&prompt_path, &prompt)
        .with_context(|| format!("cannot write {}", prompt_path.display()))?;

    let mux = mux::select();
    let pane = mux.open_pane("meguri", &format!("w{wid}"), Some(worktree))?;
    // 生成直後の pane はシェル準備前で最初の送信を落とすことがある(p2.2b の学び)。
    std::thread::sleep(Duration::from_millis(800));
    mux.send_line(&pane, &agent_cmd)?;
    std::thread::sleep(opts.grace); // spawn_grace: CLI 起動待ち
    let instruction = "Read .meguri/prompt.md and complete it (implement, commit, then write the result file).";
    mux.send_line(&pane, instruction)?; // 初回注入(grace 経過後)

    store::set_work_state(conn, wid, "running")?;
    println!("  launched agent in a pane (working). it will write .meguri/result.json when done.");
    // pane は detached な場所で開く。人間が覗けるように案内する(§3.5)。
    println!("  watch it:  {}", mux.attach_hint("meguri"));

    if opts.detach {
        println!("  detached — state stays 'running'. use the command above to watch, then re-run to harvest.");
        return Ok(());
    }

    // o16: 画面は読まず、耐久 result ファイルの出現だけを見て完了を判定する(§8)。
    // 初回注入が CLI 起動前で落ちた場合の保険として、result が出るまで上限付きで再注入する。
    match wait_result(mux.as_ref(), &pane, &result_path, instruction, opts.nudge, opts.timeout) {
        Some(r) => finalize_work(conn, wid, o, worktree, base_sha, branch, &r)?,
        None => {
            // pane 死亡 or timeout。詳細な失敗経路(nudge/timeout/pane 死亡)は o23-o25。
            // ここでは pane を残し、state は 'running' のまま人間に委ねる(§3.5)。
            println!("  no durable result detected (pane died or timed out). state stays 'running'; attach to the pane to check.");
        }
    }
    Ok(())
}

/// harvest の芯(o16-o21): 収穫した result を受けて、独立検証 → gate(verified/rework)→ Artifact
/// 記録までを行う。**pane を要らない**(result.json と git 状態だけを見る)ので、将来の reconciler
/// (`watch`)が live な pane ハンドル無しでも同じ処理を回せる。この関数が launch/harvest 分離の
/// harvest 側の実体。
fn finalize_work(
    conn: &rusqlite::Connection,
    wid: i64,
    o: &store::Outcome,
    worktree: &std::path::Path,
    base_sha: &str,
    branch: &str,
    r: &exec::WorkResult,
) -> Result<()> {
    println!("  agent reported [{}]: {}", r.status, r.summary);
    let reported = exec::state_for(&r.status);
    if reported == "reported" {
        // success 報告: 申告を信じきらず meguri 側で独立検証して gate する(§3.5、o17-o20)。
        let checks = verify::run_all(o, worktree, base_sha)?;
        for c in &checks {
            print_check(c);
        }
        let state = if verify::all_pass(&checks) { "verified" } else { "rework" };
        store::set_work_state(conn, wid, state)?;
        if state == "verified" {
            // o21: 検証済みの commit を Artifact として記録する(Work の耐久成果物)。
            let sha = gitops::head_sha(worktree)?;
            store::set_work_artifact(conn, wid, &sha)?;
            let short = sha.chars().take(7).collect::<String>();
            println!("  w{wid} passed verification → [verified]");
            println!("  artifact: {branch} @ {short}");
        } else {
            let n = checks.iter().filter(|c| !c.pass).count();
            println!("  w{wid} failed verification ({n} check(s)) → [rework] (fix turn is o22)");
        }
    } else {
        // failure / needs_human はエージェント申告のまま(検証しても意味がない)。
        store::set_work_state(conn, wid, reported)?;
        println!("  w{wid} is now [{reported}]");
    }
    Ok(())
}

/// 検証子(o17-)の結果を 1 行で表示する。
fn print_check(c: &verify::Check) {
    let mark = if c.pass { "PASS" } else { "FAIL" };
    println!("  verify {}: {mark} — {}", c.name, c.detail);
}

/// 初回注入が落ちても最大この回数まで再注入する(cold-start 保険)。以降は静かに待つ
/// (作業中のエージェントを叩き続けないため)。本格的な沈黙 nudge は o23。
const NUDGE_MAX: u32 = 3;

/// o16: 耐久 result ファイル(`.meguri/result.json`)の出現をポーリングで待つ。**画面は読まない**(§8)。
/// `nudge` 間隔で `instruction` を最大 `NUDGE_MAX` 回だけ再注入する(初回送信が CLI 起動前に
/// 落ちるケースの保険)。返り値: `Some(result)`=検知 / `None`=pane 死亡 or timeout(pane は残す)。
fn wait_result(
    mux: &dyn mux::Mux,
    pane: &mux::PaneId,
    result_path: &std::path::Path,
    instruction: &str,
    nudge: Duration,
    timeout: Duration,
) -> Option<exec::WorkResult> {
    let start = std::time::Instant::now();
    let mut last_nudge = std::time::Instant::now();
    let mut nudges: u32 = 0;
    loop {
        // Ok(Some)=完了 / Ok(None)=まだ / Err=書き込み途中の部分 JSON → まだ完成していない扱いで待つ。
        if let Ok(Some(r)) = exec::read_result(result_path) {
            return Some(r);
        }
        if !mux.is_alive(pane).unwrap_or(false) {
            return None; // pane が死んだ(詳細な扱いは o25)
        }
        if start.elapsed() >= timeout {
            return None; // timeout(詳細な扱いは o24。pane は残す)
        }
        // まだ result が無く、上限内なら再注入(初回が起動前で落ちていた場合の救済)。
        if nudges < NUDGE_MAX && last_nudge.elapsed() >= nudge {
            let _ = mux.send_line(pane, instruction);
            nudges += 1;
            last_nudge = std::time::Instant::now();
        }
        std::thread::sleep(Duration::from_millis(500));
    }
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

/// 管理 repo の bare clone のパス(`MEGURI_HOME/repos/<name>.git`)。
fn bare_path(name: &str) -> Result<PathBuf> {
    Ok(db::repos_dir()?.join(format!("{name}.git")))
}

/// Work に紐づく git worktree とブランチを実体ごと消す(best-effort)。
/// spawn 済みでなければ何もしない。DB 行の削除は呼び出し側で行う。
fn cleanup_worktree(conn: &rusqlite::Connection, w: &store::Work) {
    let (Some(path), Some(branch), Some(base_sha)) =
        (w.worktree_path.clone(), w.branch.clone(), w.base_sha.clone())
    else {
        return;
    };
    let repo = store::get_outcome(conn, w.serves_id)
        .and_then(|o| store::get_intent(conn, o.intent_id))
        .ok()
        .and_then(|it| it.repo_id)
        .and_then(|rid| store::get_repo(conn, rid).ok());
    if let Some(repo) = repo {
        if let Ok(bare) = bare_path(&repo.name) {
            let wt = gitops::Worktree { path: path.into(), branch, base_sha };
            if let Err(e) = gitops::remove_worktree(&bare, &wt) {
                eprintln!("warning: could not remove worktree {}: {e}", wt.path.display());
            }
        }
    }
}

fn repo(conn: &rusqlite::Connection, c: RepoCmd) -> Result<()> {
    match c {
        RepoCmd::Add { name, from, branch } => {
            // 先に登録(名前の一意性チェック)→ clone。clone 失敗なら登録を戻す。
            store::add_repo(conn, &name, &from, &branch)?;
            let bare = bare_path(&name)?;
            if let Err(e) = gitops::bare_clone(&from, &bare) {
                let _ = store::remove_repo(conn, &name);
                return Err(e);
            }
            println!("added repo {name} (bare: {})", bare.display());
        }
        RepoCmd::Ls => {
            for r in store::list_repos(conn)? {
                println!("{}  [{}]  {}", r.name, r.default_branch, r.origin);
            }
        }
        RepoCmd::Fetch { name } => {
            store::get_repo_by_name(conn, &name)?;
            gitops::fetch(&bare_path(&name)?)?;
            println!("fetched {name}");
        }
        RepoCmd::Rm { name } => {
            store::get_repo_by_name(conn, &name)?;
            store::remove_repo(conn, &name)?;
            let bare = bare_path(&name)?;
            let _ = std::fs::remove_dir_all(&bare);
            println!("removed repo {name}");
        }
    }
    Ok(())
}

fn intent(conn: &rusqlite::Connection, c: IntentCmd) -> Result<()> {
    match c {
        IntentCmd::Add { title, description, repo } => {
            let id = store::add_intent(conn, &title, &description)?;
            if let Some(name) = repo {
                let r = store::get_repo_by_name(conn, &name)?;
                store::set_intent_repo(conn, id, r.id)?;
            }
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
        IntentCmd::Edit { id, title, description, repo } => {
            let iid = parse_id(&id, 'i')?;
            if title.is_none() && description.is_none() && repo.is_none() {
                bail!("nothing to edit (pass --title / --description / --repo)");
            }
            store::edit_intent(conn, iid, title.as_deref(), description.as_deref())?;
            if let Some(name) = repo {
                let r = store::get_repo_by_name(conn, &name)?;
                store::set_intent_repo(conn, iid, r.id)?;
            }
            println!("edited i{iid}");
        }
        IntentCmd::Rm { id } => {
            let iid = parse_id(&id, 'i')?;
            // 行の cascade 削除の前に、配下 Work の worktree 実体を掃除する。
            for o in store::list_outcomes(conn, Some(iid))? {
                for w in store::list_works(conn, Some(o.id))? {
                    cleanup_worktree(conn, &w);
                }
            }
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
                if let Some(p) = &w.worktree_path {
                    println!("     worktree: {p}");
                }
                if let (Some(b), Some(sha)) = (&w.branch, &w.artifact_sha) {
                    let short = sha.chars().take(7).collect::<String>();
                    println!("     artifact: {b} @ {short}");
                }
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
            let w = store::get_work(conn, wid)?;
            cleanup_worktree(conn, &w); // 実体の git worktree/ブランチも消す
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
