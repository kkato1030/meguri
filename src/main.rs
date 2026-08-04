//! meguri v2 — AI コーディングエージェントを terminal multiplexer の生きた
//! pane で回すオーケストレーター。
//!
//! v0 の範囲: **local task 1 本 → tmux pane のエージェント → 検証済みブランチ**。
//! watch ループも GitHub も永続化もまだ無い。ただし v1 で消せない概念の核は
//! v0 から入っている:
//!
//! 1. **完了コントラクト** — orchestrator は worktree に prompt ファイルを書き、
//!    エージェントは `.meguri/result.json` を書くことで完了を申告する。画面は
//!    読まない([`turn`])。
//! 2. **trust-but-verify** — `success` の申告は独立に検証する: git tree が
//!    clean、base より commit が進んでいる、`check_command` が通る([`gitops`])。
//! 3. **生きた pane** — エージェントは headless ではなく herdr / tmux の対話
//!    セッションで動き、人間はいつでも attach して介入できる([`mux`])。
//!
//! 読む順番: main.rs(この流れ)→ config.rs → turn.rs → mux/ → gitops.rs。

mod config;
mod gitops;
mod mux;
mod turn;

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "meguri",
    version,
    about = "AI コーディングエージェントを tmux の生きた pane で回す"
)]
enum Cli {
    /// タスク 1 本を実行する: worktree を切り、pane でエージェントを走らせ、
    /// 検証済みブランチを残す
    Run {
        /// タスクの内容(1 行のメモで良い。エージェントへの指示になる)
        task: String,
        /// config.toml のプロジェクト id(1 件だけ設定済みなら省略可)
        #[arg(long)]
        project: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse() {
        Cli::Run { task, project } => run(&task, project.as_deref()),
    }
}

/// v0 の全体フロー。上から下へ、1 run の一生がそのまま並んでいる。
fn run(task: &str, project: Option<&str>) -> Result<()> {
    let cfg = config::load()?;
    let project = cfg.project(project)?;

    // --- 1. 作業場所: base ブランチから worktree を切る --------------------
    let run_id = now_millis().to_string();
    let branch = format!("meguri/{}-{run_id}", slug(task));
    let worktree = config::worktrees_root().join(&project.id).join(&run_id);
    let base_sha = gitops::create_worktree(
        &project.repo_path,
        &worktree,
        &branch,
        &project.default_branch,
    )
    .context("worktree の作成")?;
    println!("worktree: {} (branch {branch})", worktree.display());

    // --- 2. 完了コントラクト: prompt を書き、pane でエージェントを起動 -----
    let turn_id = format!("t-{run_id}");
    let prompt_path =
        turn::write_prompt(&worktree, &turn_id, task, project.check_command.as_deref())
            .context("prompt の書き出し")?;
    let mux = mux::detect(&cfg.mux.kind, &project.id)?;
    let mut command = vec![cfg.agent.command.clone()];
    command.extend(cfg.agent.args.iter().cloned());
    let pane = mux
        .spawn_agent(&branch, &worktree, &command)
        .context("エージェント pane の起動")?;
    println!("pane: {} ({})", pane.id, pane.attach_hint);
    // エージェントの起動を少し待ってから、prompt を読むよう 1 行だけ打ち込む。
    std::thread::sleep(Duration::from_secs(cfg.limits.spawn_grace_secs));
    mux.send_line(
        &pane,
        &format!(
            "{} を読んで、その内容を完遂してください。",
            prompt_path.display()
        ),
    )?;

    // --- 3. 申告を待つ(画面は読まない) -----------------------------------
    let result = wait_for_result(
        &worktree,
        &turn_id,
        &*mux,
        &pane,
        cfg.limits.max_turn_runtime_secs,
    )?;
    println!("agent: {} — {}", result.status, result.summary);

    // --- 4. trust-but-verify: success の申告を独立に検証する ---------------
    match result.status.as_str() {
        "success" => {}
        "needs_human" => bail!(
            "エージェントが人間の判断を求めています。続きは: {}",
            pane.attach_hint
        ),
        _ => bail!(
            "エージェントが失敗を申告しました。経緯は: {}",
            pane.attach_hint
        ),
    }
    gitops::verify(&worktree, &base_sha, project.check_command.as_deref())
        .context("success 申告の独立検証(pane は残してあるので attach して確認できます)")?;

    println!(
        "done: 検証済みブランチ {branch} が {} にあります",
        project.repo_path.display()
    );
    Ok(())
}

/// `.meguri/result.json` の出現を待つ。pane が死んだら諦め、時間を使い切ったら
/// タイムアウト。どちらも「エージェントの画面を読んで判断する」ことはしない —
/// 耐久のあるシグナル(ファイルと pane の生死)だけを見る。
fn wait_for_result(
    worktree: &Path,
    turn_id: &str,
    mux: &dyn mux::Mux,
    pane: &mux::Pane,
    max_secs: u64,
) -> Result<turn::TurnResult> {
    let deadline = Instant::now() + Duration::from_secs(max_secs);
    loop {
        if let Some(result) = turn::read_result(worktree, turn_id)? {
            return Ok(result);
        }
        if !mux.pane_alive(pane)? {
            bail!("エージェントの pane が終了しました(result.json は未提出)");
        }
        if Instant::now() > deadline {
            bail!(
                "{max_secs} 秒待っても result.json が現れませんでした。pane は生きています: {}",
                pane.attach_hint
            );
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// ブランチ名に使える短い slug(ASCII 英数のみ、その他は `-`)。
fn slug(task: &str) -> String {
    let s: String = task
        .chars()
        .take(40)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() { "task".into() } else { s }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_keeps_ascii_and_replaces_the_rest() {
        assert_eq!(slug("Fix login bug"), "fix-login-bug");
        assert_eq!(slug("ログインを直す"), "task");
        assert_eq!(slug("--x--"), "x");
    }
}
