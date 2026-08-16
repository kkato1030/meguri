//! p0 スパイク: meguri が ACP クライアントとして既存エージェントを駆動できるかの検証。
//!
//! 相手はエージェント可変(env `MEGURI_ACP_AGENT`):
//!   - `claude`(既定): `claude-code-acp` adapter 経由(`@zed-industries/claude-code-acp`)。
//!     本命はこれ。入れ子起動ガードを越えるため CLAUDECODE を unset して起動する。
//!   - `gemini`: `gemini --acp`(ネイティブ ACP、adapter 不要)。最初に往復を実証した相手。
//!
//! ACP は JSON-RPC 2.0 を stdio で流すだけなので、ここでは SDK を使わず手書きする
//! —— 目的は「往復が本当に通るか」であって、綺麗な抽象を作ることではない。
//!
//! 通す往復:  initialize → session/new → session/prompt → (session/update 群) → 応答
//! 成功の定義: エージェントの返答テキストが流れてきて、stopReason 付きの応答で終わる。

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{json, Value};

/// spike が相手にする ACP エージェントの起動レシピ。
struct AgentRecipe {
    program: &'static str,
    args: &'static [&'static str],
    /// 子プロセスから取り除く環境変数(Claude の入れ子ガード対策)。
    env_remove: &'static [&'static str],
}

fn agent_recipe() -> (&'static str, AgentRecipe) {
    match std::env::var("MEGURI_ACP_AGENT").as_deref() {
        Ok("gemini") => (
            "gemini",
            AgentRecipe { program: "gemini", args: &["--acp", "--skip-trust"], env_remove: &[] },
        ),
        // 既定は本命の Claude。CLAUDECODE を消さないと「Claude の中で Claude は起動不可」で弾かれる。
        _ => (
            "claude",
            AgentRecipe {
                program: "claude-code-acp",
                args: &[],
                env_remove: &["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"],
            },
        ),
    }
}

/// gemini --acp の子プロセスと、その stdio に張った JSON-RPC の最小クライアント。
struct Acp {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    next_id: i64,
}

impl Acp {
    fn spawn(recipe: &AgentRecipe) -> std::io::Result<Self> {
        // stderr は捨てる(エージェントのログが応答に混ざらないように)。
        let mut cmd = Command::new(recipe.program);
        cmd.args(recipe.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for k in recipe.env_remove {
            cmd.env_remove(k);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        Ok(Self { child, stdin, reader: BufReader::new(stdout), next_id: 0 })
    }

    /// リクエストを 1 本送り、割り当てた id を返す。
    fn send(&mut self, method: &str, params: Value) -> std::io::Result<i64> {
        self.next_id += 1;
        let id = self.next_id;
        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        writeln!(self.stdin, "{}", msg)?;
        self.stdin.flush()?;
        Ok(id)
    }

    /// stdout から JSON 行を 1 つ読む(空行・非 JSON はスキップ)。EOF で None。
    fn read_msg(&mut self) -> Option<Value> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line).ok()? == 0 {
                return None; // EOF = 子プロセス終了
            }
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<Value>(t) {
                return Some(v);
            }
            // 想定外の非 JSON 行は無視して読み進める。
        }
    }

    /// 指定 id の応答が来るまで読み続ける。その間に来るものを捌く:
    ///  - session/update 通知    → 返答テキスト片(agent_message_chunk)なら表示
    ///  - エージェント→クライアントのリクエスト → 未実装エラーを返して詰まりを防ぐ
    fn await_response(&mut self, want_id: i64) -> Option<Value> {
        loop {
            let msg = self.read_msg()?;
            let has_method = msg.get("method").is_some();
            let msg_id = msg.get("id").and_then(Value::as_i64);

            match (has_method, msg_id) {
                // エージェントがこちらを呼んできた(fs/read や permission 等)。
                // このスパイクはツールを使わない前提だが、来たら空応答せず
                // エラーを返しておく(無視すると相手が待ち続けて固まる)。
                (true, Some(rid)) => {
                    let err = json!({
                        "jsonrpc": "2.0", "id": rid,
                        "error": {"code": -32601, "message": "meguri p0 spike: client method not implemented"}
                    });
                    let _ = writeln!(self.stdin, "{}", err);
                    let _ = self.stdin.flush();
                }
                // 通知。返答テキスト片だけ拾って表示、他は捨てる。
                (true, None) => {
                    if msg.get("method").and_then(Value::as_str) == Some("session/update") {
                        if let Some(update) = msg.pointer("/params/update") {
                            if update.get("sessionUpdate").and_then(Value::as_str)
                                == Some("agent_message_chunk")
                            {
                                if let Some(text) =
                                    update.pointer("/content/text").and_then(Value::as_str)
                                {
                                    print!("{}", text);
                                    let _ = std::io::stdout().flush();
                                }
                            }
                        }
                    }
                }
                // 応答。目的の id ならここで返す。
                (false, Some(id)) if id == want_id => return Some(msg),
                _ => {}
            }
        }
    }
}

fn main() {
    let cwd = std::env::current_dir().expect("cwd");
    let prompt = std::env::args().nth(1).unwrap_or_else(|| {
        "Reply with one short friendly sentence to confirm the ACP round-trip works.".to_string()
    });

    let (agent_name, recipe) = agent_recipe();
    eprintln!("[spike] agent = {} ({})", agent_name, recipe.program);
    let mut acp = Acp::spawn(&recipe)
        .unwrap_or_else(|e| panic!("{} を起動できなかった({}): {e}", recipe.program, agent_name));

    // 1) initialize —— バージョン折衝と capability 交換。
    let id = acp
        .send(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {},
                "clientInfo": {"name": "meguri-p0-spike", "version": "0.0.0"}
            }),
        )
        .expect("send initialize");
    let init = acp.await_response(id).expect("initialize の応答が来ない");
    eprintln!(
        "[spike] initialize ok — agent = {} {}",
        init.pointer("/result/agentInfo/name").and_then(Value::as_str).unwrap_or("?"),
        init.pointer("/result/agentInfo/version").and_then(Value::as_str).unwrap_or("")
    );

    // 2) session/new —— 会話セッションを作る(cwd と mcpServers を渡す)。
    let id = acp
        .send("session/new", json!({"cwd": cwd.to_str().unwrap(), "mcpServers": []}))
        .expect("send session/new");
    let sess = acp.await_response(id).expect("session/new の応答が来ない");
    let session_id = sess
        .pointer("/result/sessionId")
        .and_then(Value::as_str)
        .expect("sessionId が無い")
        .to_string();
    eprintln!("[spike] session = {}", session_id);

    // 3) session/prompt —— これが本命の往復。返答が stream で流れてくる。
    eprintln!("[spike] prompt  = {:?}", prompt);
    eprintln!("[spike] ---- agent reply ----");
    let id = acp
        .send(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": prompt}]
            }),
        )
        .expect("send session/prompt");
    let resp = acp.await_response(id).expect("prompt の応答が来ない");
    println!(); // stream 表示の改行

    if let Some(err) = resp.get("error") {
        eprintln!("[spike] FAILED — prompt がエラー: {}", err);
        let _ = acp.child.kill();
        let _ = acp.child.wait();
        std::process::exit(1);
    }
    let stop = resp.pointer("/result/stopReason").and_then(Value::as_str).unwrap_or("?");
    eprintln!("[spike] ---- end (stopReason = {}) ----", stop);
    eprintln!("[spike] SUCCESS — ACP 往復(initialize→new→prompt→reply)が通った");

    let _ = acp.child.kill();
    let _ = acp.child.wait();
}
