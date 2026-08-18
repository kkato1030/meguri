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
mod outcome;
mod plan;
mod render;
mod store;
mod verify;
mod work;

use std::io::{IsTerminal, Write};
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
        /// Fold satisfied Outcomes; show only what's still active (ready/blocked)
        #[arg(long)]
        active: bool,
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
        /// Block here until the Work finishes (verify + gate), instead of detaching (the default)
        #[arg(long)]
        wait: bool,
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
    /// Reconcile running Works: harvest any that produced a result (verify -> gate -> Artifact)
    Watch {
        /// Do a single reconcile pass and exit (instead of looping until drained)
        #[arg(long)]
        once: bool,
        /// Seconds between passes while Works are still running
        #[arg(long, default_value_t = 5)]
        interval_secs: u64,
        /// Surface a running Work as timed_out after this many seconds since launch (o24)
        #[arg(long, default_value_t = 1800)]
        timeout_secs: u64,
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
    /// Mark satisfied by hand (human assertion; any non-rollup Outcome, no Work needed)
    Done { id: String },
    /// Clear all acceptances (human mark + work-originated), rolling back a mistaken accept
    Undone {
        id: String,
        /// Skip the confirmation prompt
        #[arg(long)]
        yes: bool,
    },
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
        Cmd::Graph { intent, mermaid, html, out, no_open, active } => {
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
                print!("{}", render::text(&outcomes, &accepted, active));
            }
        }
        Cmd::Plan(c) => plan_cmd(&conn, c)?,
        Cmd::Run { outcome, agent, wait, grace_secs, timeout_secs, nudge_secs } => run_cmd(
            &conn,
            &outcome,
            &RunOpts {
                agent,
                wait,
                grace: Duration::from_secs(grace_secs),
                timeout: Duration::from_secs(timeout_secs),
                nudge: Duration::from_secs(nudge_secs),
            },
        )?,
        Cmd::Accept { work } => accept_cmd(&conn, &work)?,
        Cmd::Watch { once, interval_secs, timeout_secs } => {
            watch_cmd(&conn, once, Duration::from_secs(interval_secs), Duration::from_secs(timeout_secs))?
        }
    }
    Ok(())
}

/// watch が沈黙 Work に再注入する間隔。launch の初回注入が cold-start で落ちた場合の救済。
const WATCH_NUDGE_INTERVAL: Duration = Duration::from_secs(20);

/// step 2/3 (o27): 最小 reconciler。running な Work を走査し、result が出ていれば harvest、
/// まだなら(pane があれば上限付き・間隔付きで)再注入する。level-triggered delivery の芽(§3.5)。
/// `finalize_work` は **pane を要らない**ので harvest 自体は live ハンドル無しで回る。
/// 既定は running が捌け切るまでループ、`--once` で 1 パス。
fn watch_cmd(conn: &rusqlite::Connection, once: bool, interval: Duration, timeout: Duration) -> Result<()> {
    let mux = mux::select();
    // Work ごとの nudge 記録(回数, 最後に送った時刻)。この watch プロセスの寿命でのみ持つ。
    let mut nudges: std::collections::HashMap<i64, (u32, std::time::Instant)> =
        std::collections::HashMap::new();
    // TTY かつループ時のみ、要約行を同じ行で上書きする(harvest/nudge の恒久行はそのまま残す)。
    let interactive = std::io::stdout().is_terminal() && !once;
    let mut transient = false; // 上書き対象の要約行が出ている最中か
    loop {
        // 前パスの一時要約行を消してから、恒久イベント(harvest/nudge)を新しい行に出す。
        if transient {
            print!("\r\x1b[2K");
            let _ = std::io::stdout().flush();
            transient = false;
        }
        let running: Vec<store::Work> = store::list_works(conn, None)?
            .into_iter()
            .filter(|w| w.state == "running")
            .collect();
        if running.is_empty() {
            println!("no running works to reconcile");
            break;
        }
        let mut finalized = 0;
        for w in &running {
            if reconcile_work(conn, w, mux.as_ref(), &mut nudges, timeout)? {
                finalized += 1;
            }
        }
        let still = running.len() - finalized;
        let msg = format!("reconciled {finalized} / {} running ({still} still running)", running.len());
        let last = once || still == 0;
        if interactive && !last {
            print!("\r\x1b[2K{msg}"); // 同じ行を上書き(改行しない)
            let _ = std::io::stdout().flush();
            transient = true;
        } else {
            println!("{msg}");
        }
        if last {
            break;
        }
        std::thread::sleep(interval);
    }
    Ok(())
}

/// running な Work を 1 件 reconcile する。result があれば harvest して `true`。無ければ pane を
/// mux の生死で見張り、**結果を残さず死んでいれば failed に表面化して `true`**(o25)。まだ生きて
/// いれば(必要なら沈黙 nudge して)`false`。
fn reconcile_work(
    conn: &rusqlite::Connection,
    w: &store::Work,
    mux: &dyn mux::Mux,
    nudges: &mut std::collections::HashMap<i64, (u32, std::time::Instant)>,
    timeout: Duration,
) -> Result<bool> {
    let (Some(wt), Some(base), Some(branch)) = (&w.worktree_path, &w.base_sha, &w.branch) else {
        return Ok(false); // worktree 未紐付け(通常ありえない)
    };
    let worktree = std::path::Path::new(wt);
    let result_path = worktree.join(".meguri").join("result.json");
    if let Some(r) = exec::read_result(&result_path)? {
        let o = store::get_outcome(conn, w.serves_id)?;
        println!("w{} (o{}):", w.id, w.serves_id);
        // pane があれば fix turn の差し戻しをそこへ注入できる(live ハンドル無しでも harvest 自体は回る)。
        let pane = w.pane_id.as_ref().map(|p| mux::PaneId(p.clone()));
        let deliver = pane.as_ref().map(|p| (mux, p));
        finalize_work(conn, w.id, &o, worktree, base, branch, &r, deliver)?;
        // fix turn の差し戻し(o22)は state を 'running' のまま残す。その場合は **未完了**として
        // 扱い、watch に見続けさせる(finalized に数えると still==0 で watch が抜けてしまう)。
        if store::get_work(conn, w.id)?.state == "running" {
            return Ok(false);
        }
        nudges.remove(&w.id);
        return Ok(true);
    }
    // まだ result 無し。pane が無ければ判定材料が無いので見張るだけ(通常ありえない)。
    let Some(pid) = &w.pane_id else {
        return Ok(false);
    };
    let pane = mux::PaneId(pid.clone());
    // pane を mux の生死で一度だけ見る。is_alive が Err(生死不明)のときは判断を保留し、
    // 死と断定して落とさない(次パスで見直す)。
    let alive = match mux.is_alive(&pane) {
        Ok(a) => a,
        Err(_) => return Ok(false),
    };
    // o25: 結果を残さないまま pane が死んだら、failed として表面化する(running から外れる)。
    if work::judge_pane(alive, false) == work::PaneVerdict::Failed {
        store::set_work_state(conn, w.id, "failed")?;
        nudges.remove(&w.id);
        println!("  w{} pane died without a result → [failed]", w.id);
        return Ok(true);
    }
    // o24: launch から timeout を過ぎても result が出ない running は timed_out に表面化する。
    // **pane は殺さない**(§3.5、人間が最終画面を覗いて続きを決められる)。
    if let Some(secs) = store::work_elapsed_secs(conn, w.id)? {
        if work::judge_timeout(Duration::from_secs(secs.max(0) as u64), timeout, false)
            == work::TimeoutVerdict::TimedOut
        {
            store::set_work_state(conn, w.id, work::TIMED_OUT_STATE)?;
            nudges.remove(&w.id);
            println!("  w{} timed out → [{}] (pane kept alive)", w.id, work::TIMED_OUT_STATE);
            return Ok(true);
        }
    }
    // pane は生きている。detach 既定では run が注入しないので、watch が **初回発見で即注入**し
    // (count==0)、以降は間隔をあけて上限まで再注入(nudge)する。
    // ただし fix turn 中(fix_turns>0)は差し戻し指示が現行の指示なので、汎用 nudge はしない。
    if w.fix_turns > 0 {
        return Ok(false);
    }
    // pane は生きている(上で death 判定を通過済み)。沈黙 nudge を出すかは work の純粋方針が
    // 決める(有界。o23)。ここは注入と記録だけ。
    let (count, last) = *nudges.entry(w.id).or_insert((0, std::time::Instant::now()));
    if let work::Nudge::Send { attempt, first } =
        work::nudge(count, last.elapsed(), WATCH_NUDGE_INTERVAL)
    {
        let _ = mux.send_line(&pane, INJECT_INSTRUCTION);
        nudges.insert(w.id, (count + 1, std::time::Instant::now()));
        let kind = if first { "injected" } else { "nudged" };
        println!("  w{} {kind} ({attempt}/{})", w.id, work::NUDGE_MAX);
    }
    Ok(false) // まだ実っていない
}

/// ローカル Human Gate: verified な Work を accept する。
/// その serve 先 Outcome が satisfied になり(導出)、後続 Outcome が ready になる。
fn accept_cmd(conn: &rusqlite::Connection, work: &str) -> Result<()> {
    let wid = parse_id(work, 'w')?;
    let w = store::get_work(conn, wid)?;
    if w.state != "verified" {
        bail!("w{wid} is [{}], not verified — only verified Work can be accepted", w.state);
    }
    // 受理を Outcome 側の耐久事実として記録する(この行が satisfied の根拠。Work を掃除しても残る)。
    let o = store::get_outcome(conn, w.serves_id)?;
    let repo_id = store::get_intent(conn, o.intent_id)?.repo_id;
    store::add_acceptance(conn, w.serves_id, Some(wid), repo_id, w.artifact_sha.as_deref())?;
    store::set_work_state(conn, wid, "accepted")?; // Work 側は運用状態として記録(satisfied の根拠ではない)
    println!("accepted w{wid} → o{} is now satisfied", w.serves_id);

    // 後続で新たに ready になった Outcome を案内する(リングが繋がったことの確認)。
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
    /// true なら完了(検証・gate)までここでブロックする。既定 false = detach(watch が harvest)。
    wait: bool,
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
    // 基準は **fetch で更新される remote-tracking ref**(`origin/<branch>`)。bare の local
    // `refs/heads/<branch>` は clone 時から動かないので、そこから切ると毎回古い base になる。
    let bare = bare_path(&repo.name)?;
    gitops::fetch(&bare)?;
    let base_ref = format!("origin/{}", repo.default_branch);
    let wid = store::add_work(conn, oid, &o.statement, "ai")?;
    let key = format!("w{wid}");
    let wt_parent = db::worktrees_dir()?.join(&repo.name);
    let wt = match gitops::create_worktree(&bare, &base_ref, &wt_parent, &key) {
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
    // pane ハンドルを保存(watch が沈黙 Work を再注入するのに使う)。
    store::set_work_pane(conn, wid, &pane.0)?;
    // 生成直後の pane はシェル準備前で最初の送信を落とすことがある(p2.2b の学び)。
    std::thread::sleep(Duration::from_millis(800));
    mux.send_line(&pane, &agent_cmd)?; // エージェント CLI を起動

    store::set_work_state(conn, wid, "running")?;
    store::mark_work_started(conn, wid)?; // o24: watch の timeout 判定の基準時刻
    println!("  launched agent in a pane (working). it will write .meguri/result.json when done.");
    // pane は detached な場所で開く。人間が覗けるように案内する(§3.5)。
    println!("  watch it:  {}", mux.attach_hint("meguri"));

    if !opts.wait {
        // 既定: 起動したら即返る(grace も注入もしない)。実装プロンプトの注入と harvest は
        // `meguri watch`(reconciler)が担う — watch は初回発見で注入し、以降 nudge する。
        println!("  detached (default) — run `meguri watch` to drive & harvest it (or re-run with --wait).");
        return Ok(());
    }

    // --wait: ここで初回注入し、result が出るまで待って harvest する(同期パス)。
    std::thread::sleep(opts.grace); // spawn_grace: CLI 起動待ち
    mux.send_line(&pane, INJECT_INSTRUCTION)?; // 初回注入(grace 経過後)
    // 初回注入が CLI 起動前で落ちた場合の保険として、result が出るまで上限付きで再注入する。
    match wait_result(mux.as_ref(), &pane, &result_path, INJECT_INSTRUCTION, opts.nudge, opts.timeout) {
        WaitOutcome::Result(r) => {
            finalize_work(conn, wid, o, worktree, base_sha, branch, &r, Some((mux.as_ref(), &pane)))?
        }
        WaitOutcome::Timeout => {
            // o24: タイムアウトを握りつぶさず表面化する。ただし pane は殺さず残す —— 人間が
            // 最終画面を覗いて続きを決められる(§3.5)。pane を落とさない不変を判定側に問うて明示する。
            debug_assert!(!work::TimeoutVerdict::TimedOut.kills_pane(), "timeout は pane を残す(o24)");
            store::set_work_state(conn, wid, work::TIMED_OUT_STATE)?;
            println!(
                "  w{wid} timed out → [{}] (pane kept alive — attach to check: {})",
                work::TIMED_OUT_STATE,
                mux.attach_hint("meguri")
            );
        }
        WaitOutcome::PaneDied => {
            // o25: 結果を残さないまま pane が死んだ = エージェントが落ちた → failed として表面化する。
            store::set_work_state(conn, wid, "failed")?;
            println!("  w{wid} pane died without a result → [failed]");
        }
    }
    Ok(())
}

/// harvest の芯(o16-o21): 収穫した result を受けて、独立検証 → gate(verified/rework)→ Artifact
/// 記録までを行う。**pane を要らない**(result.json と git 状態だけを見る)ので、将来の reconciler
/// (`watch`)が live な pane ハンドル無しでも同じ処理を回せる。この関数が launch/harvest 分離の
/// harvest 側の実体。
// Work の場所(worktree/base/branch)と収穫物(result)、差し戻し先(deliver)を素の値で受ける。
// harvest の芯なので引数は多いが、まとめ型を作るほどの再利用は無い。
#[allow(clippy::too_many_arguments)]
fn finalize_work(
    conn: &rusqlite::Connection,
    wid: i64,
    o: &store::Outcome,
    worktree: &std::path::Path,
    base_sha: &str,
    branch: &str,
    r: &exec::WorkResult,
    deliver: Option<(&dyn mux::Mux, &mux::PaneId)>,
) -> Result<()> {
    println!("  agent reported [{}]: {}", r.status, r.summary);
    let reported = exec::state_for(&r.status);
    if reported == "reported" {
        // success 報告: 申告を信じきらず meguri 側で独立検証して gate する(§3.5、o17-o20)。
        let checks = verify::run_all(o, worktree, base_sha)?;
        for c in &checks {
            print_check(c);
        }
        if verify::all_pass(&checks) {
            store::set_work_state(conn, wid, "verified")?;
            // o21: 検証済みの commit を Artifact として記録する(Work の耐久成果物)。
            let sha = gitops::head_sha(worktree)?;
            store::set_work_artifact(conn, wid, &sha)?;
            let short = sha.chars().take(7).collect::<String>();
            println!("  w{wid} passed verification → [verified]");
            println!("  artifact: {branch} @ {short}");
        } else {
            // o22: 検証落ちは、上限付きで同じエージェントに差し戻す(fix turn)。予算が尽きたら人間へ。
            finalize_fix_turn(conn, wid, worktree, &checks, deliver)?;
        }
    } else {
        // failure / needs_human はエージェント申告のまま(検証しても意味がない)。
        store::set_work_state(conn, wid, reported)?;
        println!("  w{wid} is now [{reported}]");
    }
    Ok(())
}

/// o22: 検証落ちの Work を、上限付きで同じエージェントに差し戻す(fix turn)。
///
/// `work::decide` が耐久カウンタ(`works.fix_turns`)を見て Retry/GiveUp を決める。
/// - Retry: 落ちた検証の診断を pane に注入し、fix_turns を 1 進め、state は 'running' のまま
///   にして harvest 対象に残す。**古い result.json は消す**(消さないと同じ成功報告を
///   即再検知し、エージェントが直す前に予算を空回りで焼き切ってしまう)。pane が無い経路
///   (live ハンドル無しの watch など)では注入だけ省くが、予算と state は同じに進める。
/// - GiveUp: 予算切れ。[rework] にして人間へ委ねる。
fn finalize_fix_turn(
    conn: &rusqlite::Connection,
    wid: i64,
    worktree: &std::path::Path,
    checks: &[verify::Check],
    deliver: Option<(&dyn mux::Mux, &mux::PaneId)>,
) -> Result<()> {
    let n = checks.iter().filter(|c| !c.pass).count();
    let spent = store::get_work(conn, wid)?.fix_turns;
    match work::decide(spent, checks) {
        work::FixTurn::Retry { attempt, instruction } => {
            store::set_work_fix_turns(conn, wid, attempt)?;
            store::set_work_state(conn, wid, "running")?; // まだ回す(差し戻し中)
            // 古い成功報告を消して、直した結果の新しい result.json だけを次に拾う。
            let _ = std::fs::remove_file(worktree.join(".meguri").join("result.json"));
            let injected = match deliver {
                Some((mux, pane)) if mux.is_alive(pane).unwrap_or(false) => {
                    mux.send_line(pane, &instruction).is_ok()
                }
                _ => false,
            };
            let how = if injected { "sent to agent" } else { "pending inject" };
            println!(
                "  w{wid} failed verification ({n} check(s)) → fix turn {attempt}/{} ({how})",
                work::FIX_TURN_MAX
            );
        }
        work::FixTurn::GiveUp => {
            store::set_work_state(conn, wid, "rework")?;
            println!(
                "  w{wid} failed verification ({n} check(s)) → [rework] (fix turn budget {}/{} spent — over to a human)",
                spent,
                work::FIX_TURN_MAX
            );
        }
    }
    Ok(())
}

/// 検証子(o17-)の結果を 1 行で表示する。
fn print_check(c: &verify::Check) {
    let mark = if c.pass { "PASS" } else { "FAIL" };
    println!("  verify {}: {mark} — {}", c.name, c.detail);
}

/// pane を開いたエージェントに送る初回・再注入の指示(launch と watch で共有)。
const INJECT_INSTRUCTION: &str =
    "Read .meguri/prompt.md and complete it (implement, commit, then write the result file).";

/// `wait_result` の帰結。`None` で timeout と pane 死亡を混ぜず、呼び手が別々に表面化できるよう
/// 三分岐にする(o24: timeout は pane を残して [timed_out]、o25: pane 死亡は [failed])。
enum WaitOutcome {
    /// 耐久 result を検知した(harvest へ)。
    Result(exec::WorkResult),
    /// 期限切れ。pane は残す(o24)。
    Timeout,
    /// 結果を残さないまま pane が死んだ(o25)。
    PaneDied,
}

/// o16: 耐久 result ファイル(`.meguri/result.json`)の出現をポーリングで待つ。**画面は読まない**(§8)。
/// `nudge` 間隔で `instruction` を最大 `NUDGE_MAX` 回だけ再注入する(初回送信が CLI 起動前に
/// 落ちるケースの保険)。返り値は [`WaitOutcome`]: result 検知 / timeout(pane 残す) / pane 死亡。
fn wait_result(
    mux: &dyn mux::Mux,
    pane: &mux::PaneId,
    result_path: &std::path::Path,
    instruction: &str,
    nudge: Duration,
    timeout: Duration,
) -> WaitOutcome {
    let start = std::time::Instant::now();
    let mut last_nudge = std::time::Instant::now();
    let mut nudges: u32 = 0;
    loop {
        // Ok(Some)=完了 / Ok(None)=まだ / Err=書き込み途中の部分 JSON → まだ完成していない扱いで待つ。
        if let Ok(Some(r)) = exec::read_result(result_path) {
            return WaitOutcome::Result(r);
        }
        if !mux.is_alive(pane).unwrap_or(false) {
            return WaitOutcome::PaneDied; // pane が死んだ(o25。result 無しでの死亡)
        }
        // o24: 期限切れは timeout として表面化する(pane は残す)。result はこの時点で無い(上で拾う)。
        if work::judge_timeout(start.elapsed(), timeout, false) == work::TimeoutVerdict::TimedOut {
            return WaitOutcome::Timeout;
        }
        // まだ result が無く、上限内なら再注入(初回が起動前で落ちていた場合の救済)。
        if nudges < work::NUDGE_MAX && last_nudge.elapsed() >= nudge {
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
            // 人手で「達成済み」と表明する(o28)。中身は outcome::mark_done に集約。
            let oid = parse_id(&id, 'o')?;
            outcome::done(conn, oid)?;
            println!("marked o{oid} satisfied (human)");
        }
        OutcomeCmd::Undone { id, yes } => {
            let oid = parse_id(&id, 'o')?;
            if !yes && !confirm(&format!("Clear ALL acceptances of o{oid} (human + work-originated)?"))? {
                bail!("aborted");
            }
            let n = outcome::undone(conn, oid)?;
            if n == 0 {
                println!("o{oid} had no acceptance to clear");
            } else {
                println!("cleared {n} acceptance(s) of o{oid}");
            }
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
