//! multiplexer 抽象。meguri が mux に求めることは 3 つだけ:
//! pane を作る・1 行打ち込む・生死を確かめる。
//!
//! 実装は herdr(推奨。ユーザーの常用環境)と tmux の 2 つ。エージェントの
//! 状態推定(Working/Idle/Blocked)はまだ持たない — 完了は result.json、
//! 破綻は pane の死で判定する(状態検出は nudge の増分で herdr のネイティブ
//! 検出から入れる予定)。

pub mod herdr;
pub mod tmux;

use std::path::Path;

use anyhow::{Result, bail};

pub struct Pane {
    /// mux 内で pane を名指しする id(tmux: `%3` / herdr: `w2:p5`)。
    pub id: String,
    /// 人間向けの「ここに介入できる」案内文(mux ごとに形が違う)。
    pub attach_hint: String,
}

pub trait Mux {
    /// プロジェクトの workspace/session に pane を 1 枚作り、`command` を
    /// 対話モードで起動する。
    fn spawn_agent(&self, title: &str, cwd: &Path, command: &[String]) -> Result<Pane>;
    /// pane に 1 行打ち込んで Enter。テキストと Enter を分けるのは、貼り付け
    /// 検知を持つ CLI が改行込みの一括送信を「入力途中」と誤認するため。
    fn send_line(&self, pane: &Pane, line: &str) -> Result<()>;
    fn pane_alive(&self, pane: &Pane) -> Result<bool>;
}

/// `mux.kind` の解決: `auto` は herdr の socket が生きていれば herdr、
/// いなければ tmux。明示指定は検出せずそのまま使う(選んだものが動かない
/// なら loud に失敗させる)。
pub fn detect(kind: &str, project: &str) -> Result<Box<dyn Mux>> {
    match kind {
        "herdr" => Ok(Box::new(herdr::Herdr::new(project))),
        "tmux" => Ok(Box::new(tmux::Tmux::new(project))),
        "auto" => {
            if herdr::socket_live() {
                Ok(Box::new(herdr::Herdr::new(project)))
            } else {
                Ok(Box::new(tmux::Tmux::new(project)))
            }
        }
        other => bail!("mux.kind = {other:?} は不明です(auto | herdr | tmux)"),
    }
}
