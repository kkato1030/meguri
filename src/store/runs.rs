use anyhow::Result;
use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};

use super::{Store, now};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Interrupted,
    Succeeded,
    Failed,
    Cancelled,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Interrupted => "interrupted",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "interrupted" => Some(Self::Interrupted),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            _ => None,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Interrupted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionState {
    AgentWorking,
    AwaitingHuman,
    HumanDriving,
    Paused,
}

impl InteractionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AgentWorking => "agent_working",
            Self::AwaitingHuman => "awaiting_human",
            Self::HumanDriving => "human_driving",
            Self::Paused => "paused",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "agent_working" => Some(Self::AgentWorking),
            "awaiting_human" => Some(Self::AwaitingHuman),
            "human_driving" => Some(Self::HumanDriving),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

/// Control channel written by CLI commands, honored by the orchestrator.
/// This is a *target* the orchestrator converges to; clearing it (NULL)
/// means "run normally" — so `resume` and `handback` just clear it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredState {
    Paused,
    Stopped,
    Takeover,
}

impl DesiredState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::Takeover => "takeover",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "paused" => Some(Self::Paused),
            "stopped" => Some(Self::Stopped),
            "takeover" => Some(Self::Takeover),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunRecord {
    pub id: String,
    pub project_id: String,
    pub loop_kind: String,
    pub issue_number: i64,
    pub issue_title: Option<String>,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub step: String,
    pub checkpoint_json: String,
    pub status: RunStatus,
    pub interaction_state: Option<InteractionState>,
    pub desired_state: Option<DesiredState>,
    pub mux_kind: Option<String>,
    pub mux_session: Option<String>,
    pub mux_pane_id: Option<String>,
    pub turn_no: i64,
    pub current_turn_id: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
}

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<RunRecord> {
    let status: String = row.get("status")?;
    let interaction: Option<String> = row.get("interaction_state")?;
    let desired: Option<String> = row.get("desired_state")?;
    Ok(RunRecord {
        id: row.get("id")?,
        project_id: row.get("project_id")?,
        loop_kind: row.get("loop_kind")?,
        issue_number: row.get("issue_number")?,
        issue_title: row.get("issue_title")?,
        branch: row.get("branch")?,
        worktree_path: row.get("worktree_path")?,
        step: row.get("step")?,
        checkpoint_json: row.get("checkpoint_json")?,
        status: RunStatus::parse(&status).unwrap_or(RunStatus::Failed),
        interaction_state: interaction.as_deref().and_then(InteractionState::parse),
        desired_state: desired.as_deref().and_then(DesiredState::parse),
        mux_kind: row.get("mux_kind")?,
        mux_session: row.get("mux_session")?,
        mux_pane_id: row.get("mux_pane_id")?,
        turn_no: row.get("turn_no")?,
        current_turn_id: row.get("current_turn_id")?,
        error: row.get("error")?,
        created_at: row.get("created_at")?,
    })
}

impl Store {
    pub fn create_run(
        &self,
        project_id: &str,
        issue_number: i64,
        issue_title: &str,
    ) -> Result<RunRecord> {
        let id = uuid::Uuid::new_v4().to_string();
        // Short ids are friendlier CLI handles; keep full uuid uniqueness.
        let id = format!("run-{}", &id[..8]);
        self.with_conn(|c| {
            c.execute(
                "INSERT INTO runs (id, project_id, issue_number, issue_title, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'queued', ?5)",
                params![id, project_id, issue_number, issue_title, now()],
            )?;
            Ok(())
        })?;
        Ok(self.get_run(&id)?.expect("run just inserted"))
    }

    pub fn get_run(&self, id: &str) -> Result<Option<RunRecord>> {
        self.with_conn(|c| {
            let mut stmt = c.prepare("SELECT * FROM runs WHERE id = ?1")?;
            let mut rows = stmt.query([id])?;
            match rows.next()? {
                Some(row) => Ok(Some(run_from_row(row)?)),
                None => Ok(None),
            }
        })
    }

    /// Resolve a run by exact id, unique prefix, or — if the input parses as
    /// a number — the single active run for that issue number.
    pub fn find_run(&self, needle: &str) -> Result<Option<RunRecord>> {
        if let Some(run) = self.get_run(needle)? {
            return Ok(Some(run));
        }
        self.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM runs WHERE id LIKE ?1 || '%' ORDER BY created_at DESC LIMIT 2",
            )?;
            let matches: Vec<RunRecord> = stmt
                .query_map([needle], run_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            if matches.len() == 1 {
                return Ok(Some(matches[0].clone()));
            }
            if let Ok(issue) = needle.parse::<i64>() {
                let mut stmt = c.prepare(
                    "SELECT * FROM runs WHERE issue_number = ?1
                       AND status IN ('queued','running','interrupted')
                     ORDER BY created_at DESC LIMIT 1",
                )?;
                let mut rows = stmt.query([issue])?;
                if let Some(row) = rows.next()? {
                    return Ok(Some(run_from_row(row)?));
                }
            }
            Ok(None)
        })
    }

    pub fn list_runs(&self, active_only: bool) -> Result<Vec<RunRecord>> {
        self.with_conn(|c| {
            let sql = if active_only {
                "SELECT * FROM runs WHERE status IN ('queued','running','interrupted')
                 ORDER BY created_at DESC"
            } else {
                "SELECT * FROM runs ORDER BY created_at DESC LIMIT 50"
            };
            let mut stmt = c.prepare(sql)?;
            let runs = stmt
                .query_map([], run_from_row)?
                .collect::<rusqlite::Result<_>>()?;
            Ok(runs)
        })
    }

    pub fn update_run_status(
        &self,
        id: &str,
        status: RunStatus,
        error: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|c| {
            let (started, finished) = match status {
                RunStatus::Running => (Some(now()), None),
                RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled => {
                    (None, Some(now()))
                }
                _ => (None, None),
            };
            c.execute(
                "UPDATE runs SET status = ?2, error = COALESCE(?3, error),
                   started_at = COALESCE(?4, started_at),
                   finished_at = COALESCE(?5, finished_at)
                 WHERE id = ?1",
                params![id, status.as_str(), error, started, finished],
            )?;
            Ok(())
        })
    }

    pub fn update_run_step(&self, id: &str, step: &str, checkpoint_json: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET step = ?2, checkpoint_json = ?3 WHERE id = ?1",
                params![id, step, checkpoint_json],
            )?;
            Ok(())
        })
    }

    pub fn update_run_worktree(&self, id: &str, branch: &str, worktree_path: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET branch = ?2, worktree_path = ?3 WHERE id = ?1",
                params![id, branch, worktree_path],
            )?;
            Ok(())
        })
    }

    pub fn update_run_mux(&self, id: &str, kind: &str, session: &str, pane: &str) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET mux_kind = ?2, mux_session = ?3, mux_pane_id = ?4 WHERE id = ?1",
                params![id, kind, session, pane],
            )?;
            Ok(())
        })
    }

    pub fn update_interaction_state(
        &self,
        id: &str,
        state: Option<InteractionState>,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET interaction_state = ?2 WHERE id = ?1",
                params![id, state.map(|s| s.as_str())],
            )?;
            Ok(())
        })
    }

    pub fn set_desired_state(&self, id: &str, state: Option<DesiredState>) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET desired_state = ?2 WHERE id = ?1",
                params![id, state.map(|s| s.as_str())],
            )?;
            Ok(())
        })
    }

    pub fn read_desired_state(&self, id: &str) -> Result<Option<DesiredState>> {
        self.with_conn(|c| {
            let s: Option<String> =
                c.query_row("SELECT desired_state FROM runs WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?;
            Ok(s.as_deref().and_then(DesiredState::parse))
        })
    }

    pub fn begin_turn(
        &self,
        run_id: &str,
        turn_id: &str,
        purpose: &str,
        prompt_path: &str,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE runs SET turn_no = turn_no + 1, current_turn_id = ?2 WHERE id = ?1",
                params![run_id, turn_id],
            )?;
            let turn_no: i64 =
                c.query_row("SELECT turn_no FROM runs WHERE id = ?1", [run_id], |r| {
                    r.get(0)
                })?;
            c.execute(
                "INSERT INTO turns (id, run_id, turn_no, purpose, prompt_path, started_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![turn_id, run_id, turn_no, purpose, prompt_path, now()],
            )?;
            Ok(())
        })
    }

    pub fn finish_turn(
        &self,
        turn_id: &str,
        outcome: &str,
        result_json: Option<&str>,
    ) -> Result<()> {
        self.with_conn(|c| {
            c.execute(
                "UPDATE turns SET outcome = ?2, result_json = ?3, finished_at = ?4 WHERE id = ?1",
                params![turn_id, outcome, result_json, now()],
            )?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_lifecycle() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 7, "Fix the bug").unwrap();
        assert_eq!(run.status, RunStatus::Queued);
        assert_eq!(run.issue_number, 7);

        store
            .update_run_status(&run.id, RunStatus::Running, None)
            .unwrap();
        store
            .update_run_step(&run.id, "execute", "{\"a\":1}")
            .unwrap();
        store
            .update_run_mux(&run.id, "tmux", "meguri", "%3")
            .unwrap();
        store
            .update_interaction_state(&run.id, Some(InteractionState::AgentWorking))
            .unwrap();

        let got = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(got.status, RunStatus::Running);
        assert_eq!(got.step, "execute");
        assert_eq!(got.mux_pane_id.as_deref(), Some("%3"));
        assert_eq!(got.interaction_state, Some(InteractionState::AgentWorking));
    }

    #[test]
    fn unique_active_run_per_issue() {
        let store = Store::open_in_memory().unwrap();
        store.create_run("demo", 7, "t").unwrap();
        assert!(store.create_run("demo", 7, "t").is_err());
    }

    #[test]
    fn find_run_by_prefix_and_issue() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 42, "t").unwrap();
        let prefix = &run.id[..6];
        assert_eq!(store.find_run(prefix).unwrap().unwrap().id, run.id);
        assert_eq!(store.find_run("42").unwrap().unwrap().id, run.id);
        assert!(store.find_run("nope").unwrap().is_none());
    }

    #[test]
    fn desired_state_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 1, "t").unwrap();
        store
            .set_desired_state(&run.id, Some(DesiredState::Paused))
            .unwrap();
        assert_eq!(
            store.read_desired_state(&run.id).unwrap(),
            Some(DesiredState::Paused)
        );
        store.set_desired_state(&run.id, None).unwrap();
        assert_eq!(store.read_desired_state(&run.id).unwrap(), None);
    }

    #[test]
    fn turns_recorded() {
        let store = Store::open_in_memory().unwrap();
        let run = store.create_run("demo", 1, "t").unwrap();
        store
            .begin_turn(&run.id, "turn-1", "execute", "/tmp/p.md")
            .unwrap();
        store.finish_turn("turn-1", "success", Some("{}")).unwrap();
        let got = store.get_run(&run.id).unwrap().unwrap();
        assert_eq!(got.turn_no, 1);
        assert_eq!(got.current_turn_id.as_deref(), Some("turn-1"));
    }
}
