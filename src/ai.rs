//! The Ask AI panel's engine: run Claude Code or Copilot CLI headless, on the
//! user's own subscription, with Tabula's MCP server attached for that run
//! only so the agent can read the live schema and run read-only queries.
//!
//! Each run gets only Tabula's tools: no shell, no file edits, no other MCP
//! servers, no hooks.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::agents::{self, Agent};

/// What the panel sees while a run streams.
#[derive(Debug, Clone, PartialEq)]
pub enum AiEvent {
    /// More of the answer.
    Text(String),
    /// The agent called one of Tabula's tools.
    Tool { name: String, detail: String },
    /// The run ended. `session` continues the conversation (Claude Code).
    Done {
        session: Option<String>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct AskRequest {
    pub agent: Agent,
    /// The Tabula connection the agent works on.
    pub connection: String,
    pub database: String,
    pub prompt: String,
    /// Earlier turns, for agents that can't resume a session.
    pub history: Vec<(String, String)>,
    /// Claude Code session to continue.
    pub resume: Option<String>,
    /// `--model` for the agent; `None` leaves the agent's own default.
    pub model: Option<String>,
}

/// A run in progress. Dropping it does not stop the run; call [`Run::stop`].
#[derive(Clone)]
pub struct Run {
    child: Arc<Mutex<Option<Child>>>,
    /// Told to stop, for runs that aren't a child process (Copilot's SDK).
    abort: Option<Arc<tokio::sync::Notify>>,
}

impl Run {
    /// A run that stops when `abort` is notified.
    pub fn aborting(abort: Arc<tokio::sync::Notify>) -> Run {
        Run {
            child: Arc::new(Mutex::new(None)),
            abort: Some(abort),
        }
    }

    pub fn stop(&self) {
        if let Some(mut c) = self.child.lock().expect("child lock").take() {
            let _ = c.kill();
        }
        if let Some(a) = &self.abort {
            a.notify_one();
        }
    }
}

pub(crate) fn system_prompt(req: &AskRequest) -> String {
    format!(
        "You are the AI assistant inside Tabula, a PostgreSQL client. The user is working on the connection \"{conn}\" (database \"{db}\"). \
Use the `tabula` MCP tools to look at the real schema and data before answering: list_tables, describe_table, sample_rows, run_query, explain_query. \
Pass connection=\"{conn}\" to every tool. \
Put any SQL the user might run in a ```sql fenced block; Tabula shows an Insert button on those blocks. \
Be concise. Don't modify data unless the user explicitly asks.",
        conn = req.connection,
        db = req.database
    )
}

pub(crate) fn scratch_dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("tabula-ai-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// The MCP config for one run: just Tabula, scoped to the connection.
fn mcp_config(connection: &str, copilot: bool) -> Value {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "tabula".into());
    let mut server = json!({
        "type": if copilot { "local" } else { "stdio" },
        "command": exe,
        "args": ["mcp", "--connection", connection],
    });
    if copilot {
        server["tools"] = json!(["*"]);
    }
    json!({ "mcpServers": { agents::SERVER_NAME: server } })
}

/// The arguments and stdin for a run.
pub fn plan(req: &AskRequest, files: &std::path::Path) -> (Vec<String>, Option<String>, Vec<(PathBuf, String)>) {
    let mut writes = Vec::new();
    match req.agent {
        Agent::Claude => {
            let cfg = files.join("mcp.json");
            writes.push((cfg.clone(), mcp_config(&req.connection, false).to_string()));
            let mut a: Vec<String> = vec![
                "-p".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--verbose".into(),
                "--include-partial-messages".into(),
                "--strict-mcp-config".into(),
                "--mcp-config".into(),
                cfg.to_string_lossy().to_string(),
                // No built-in tools (shell, files, web); Tabula's are allowed.
                "--tools".into(),
                String::new(),
                "--allowedTools".into(),
                format!("mcp__{}", agents::SERVER_NAME),
                "--settings".into(),
                r#"{"disableAllHooks":true}"#.into(),
                "--append-system-prompt".into(),
                system_prompt(req),
            ];
            if let Some(s) = &req.resume {
                a.push("--resume".into());
                a.push(s.clone());
            }
            if let Some(m) = &req.model {
                a.push("--model".into());
                a.push(m.clone());
            }
            (a, Some(req.prompt.clone()), writes)
        }
        Agent::Copilot => {
            let cfg = files.join("copilot-mcp.json");
            writes.push((cfg.clone(), mcp_config(&req.connection, true).to_string()));
            let mut prompt = format!("{}\n\n", system_prompt(req));
            for (q, a) in &req.history {
                prompt.push_str(&format!("User: {q}\n\nAssistant: {a}\n\n"));
            }
            prompt.push_str(&format!("User: {}", req.prompt));
            let mut a: Vec<String> = vec![
                "-p".into(),
                prompt,
                "-s".into(),
                "--no-ask-user".into(),
                "--additional-mcp-config".into(),
                format!("@{}", cfg.to_string_lossy()),
                "--allow-tool".into(),
                agents::SERVER_NAME.into(),
                "--deny-tool".into(),
                "shell".into(),
                "--deny-tool".into(),
                "write".into(),
            ];
            if let Some(m) = &req.model {
                a.push("--model".into());
                a.push(m.clone());
            }
            (a, None, writes)
        }
    }
}

/// Start a run. Events arrive on the returned channel, ending with `Done`.
/// Copilot runs through its SDK; Claude Code through its command line.
pub fn ask(req: AskRequest) -> (futures::channel::mpsc::UnboundedReceiver<AiEvent>, Run) {
    match req.agent {
        Agent::Copilot => crate::copilot::ask(req),
        Agent::Claude => ask_cli(req),
    }
}

/// Run the agent's command line once for this question.
pub(crate) fn ask_cli(req: AskRequest) -> (futures::channel::mpsc::UnboundedReceiver<AiEvent>, Run) {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let run = Run {
        child: Arc::new(Mutex::new(None)),
        abort: None,
    };
    let files = scratch_dir();
    let (args, stdin, writes) = plan(&req, &files);
    for (p, text) in writes {
        let _ = std::fs::write(p, text);
    }
    let (program, pre) = agents::resolve_command(req.agent.command());
    let mut cmd = Command::new(&program);
    cmd.args(&pre)
        .args(&args)
        .current_dir(&files)
        .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.unbounded_send(AiEvent::Done {
                session: None,
                error: Some(format!(
                    "couldn't start {} ({e}). Install it: {}",
                    req.agent.label(),
                    req.agent.install_hint()
                )),
            });
            return (rx, run);
        }
    };
    if let (Some(text), Some(mut w)) = (stdin, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = w.write_all(text.as_bytes());
        });
    }
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    *run.child.lock().expect("child lock") = Some(child);
    let err_text = Arc::new(Mutex::new(String::new()));
    {
        let err_text = err_text.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut e = err_text.lock().expect("stderr lock");
                if e.len() < 4_000 {
                    e.push_str(&line);
                    e.push('\n');
                }
            }
        });
    }
    let agent = req.agent;
    let child = run.child.clone();
    std::thread::spawn(move || {
        let mut parser = StreamParser::default();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let events = match agent {
                Agent::Claude => parser.claude_line(&line),
                Agent::Copilot => vec![AiEvent::Text(format!("{line}\n"))],
            };
            for e in events {
                let _ = tx.unbounded_send(e);
            }
        }
        let status = child
            .lock()
            .expect("child lock")
            .take()
            .and_then(|mut c| c.wait().ok());
        let stopped = status.is_none();
        let failed = status.is_some_and(|s| !s.success());
        let mut error = parser.error.take();
        if error.is_none() && failed {
            let e = err_text.lock().expect("stderr lock").trim().to_string();
            error = Some(if e.is_empty() {
                format!("{} exited with an error", agent.label())
            } else {
                e
            });
        }
        if stopped {
            error = Some("Stopped".into());
        }
        if !parser.saw_text {
            if let Some(r) = parser.result.take() {
                let _ = tx.unbounded_send(AiEvent::Text(r));
            }
        }
        let _ = tx.unbounded_send(AiEvent::Done {
            session: parser.session,
            error,
        });
    });
    (rx, run)
}

/// Reads Claude Code's `stream-json` output.
#[derive(Default)]
struct StreamParser {
    session: Option<String>,
    saw_text: bool,
    result: Option<String>,
    error: Option<String>,
}

impl StreamParser {
    fn claude_line(&mut self, line: &str) -> Vec<AiEvent> {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return vec![];
        };
        if let Some(s) = v["session_id"].as_str() {
            self.session = Some(s.to_string());
        }
        match v["type"].as_str().unwrap_or("") {
            "stream_event" => {
                let ev = &v["event"];
                if ev["type"] == "content_block_delta" && ev["delta"]["type"] == "text_delta" {
                    if let Some(t) = ev["delta"]["text"].as_str() {
                        self.saw_text = true;
                        return vec![AiEvent::Text(t.to_string())];
                    }
                }
                // A new text block after tool calls: start on a new line.
                if ev["type"] == "content_block_start"
                    && ev["content_block"]["type"] == "text"
                    && self.saw_text
                {
                    return vec![AiEvent::Text("\n\n".into())];
                }
                vec![]
            }
            "assistant" => v["message"]["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|b| b["type"] == "tool_use")
                        .map(|b| {
                            let name = b["name"].as_str().unwrap_or("tool");
                            let name = name
                                .strip_prefix(&format!("mcp__{}__", agents::SERVER_NAME))
                                .unwrap_or(name)
                                .to_string();
                            let input = &b["input"];
                            let detail = input["sql"]
                                .as_str()
                                .or(input["table"].as_str())
                                .or(input["schema"].as_str())
                                .unwrap_or("")
                                .to_string();
                            AiEvent::Tool { name, detail }
                        })
                        .collect()
                })
                .unwrap_or_default(),
            "result" => {
                if v["is_error"].as_bool() == Some(true) {
                    self.error = Some(
                        v["result"]
                            .as_str()
                            .unwrap_or("Claude Code reported an error")
                            .to_string(),
                    );
                } else {
                    self.result = v["result"].as_str().map(str::to_string);
                }
                vec![]
            }
            _ => vec![],
        }
    }
}

/// The ```sql blocks in an answer, for the Insert buttons.
pub fn sql_blocks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    for line in text.lines() {
        let t = line.trim_start();
        match &mut cur {
            None if t.starts_with("```sql") || t.starts_with("```postgres") || t.starts_with("```pgsql") => {
                cur = Some(String::new())
            }
            Some(_) if t.starts_with("```") => {
                let block = cur.take().unwrap_or_default();
                if !block.trim().is_empty() {
                    out.push(block.trim_end().to_string());
                }
            }
            Some(b) => {
                b.push_str(line);
                b.push('\n');
            }
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(agent: Agent) -> AskRequest {
        AskRequest {
            agent,
            connection: "shop db".into(),
            database: "postgres".into(),
            prompt: "how many orders?".into(),
            history: vec![("hi".into(), "hello".into())],
            resume: Some("abc".into()),
            model: None,
        }
    }

    #[test]
    fn claude_runs_with_only_tabula_tools() {
        let d = tempfile::tempdir().unwrap();
        let (a, stdin, writes) = plan(&req(Agent::Claude), d.path());
        assert_eq!(stdin.as_deref(), Some("how many orders?"));
        let at = |flag: &str| a.iter().position(|x| x == flag).map(|i| a[i + 1].clone());
        assert_eq!(at("--tools").as_deref(), Some(""));
        assert_eq!(at("--allowedTools").as_deref(), Some("mcp__tabula"));
        assert_eq!(at("--resume").as_deref(), Some("abc"));
        assert_eq!(at("--model"), None);
        let with_model = AskRequest { model: Some("opus".into()), ..req(Agent::Claude) };
        let (a, _, _) = plan(&with_model, d.path());
        assert!(a.windows(2).any(|w| w[0] == "--model" && w[1] == "opus"));
        assert!(a.contains(&"--strict-mcp-config".to_string()));
        let cfg: Value = serde_json::from_str(&writes[0].1).unwrap();
        assert_eq!(cfg["mcpServers"]["tabula"]["args"], json!(["mcp", "--connection", "shop db"]));
    }

    #[test]
    fn copilot_gets_history_in_the_prompt_and_no_shell() {
        let d = tempfile::tempdir().unwrap();
        let (a, stdin, _) = plan(&req(Agent::Copilot), d.path());
        assert!(stdin.is_none());
        assert!(a[1].contains("User: hi") && a[1].ends_with("User: how many orders?"));
        assert!(a.windows(2).any(|w| w[0] == "--deny-tool" && w[1] == "shell"));
        assert!(a.windows(2).any(|w| w[0] == "--allow-tool" && w[1] == "tabula"));
        assert!(!a.contains(&"--model".to_string()));
        let with_model = AskRequest { model: Some("auto".into()), ..req(Agent::Copilot) };
        let (a, _, _) = plan(&with_model, d.path());
        assert!(a.windows(2).any(|w| w[0] == "--model" && w[1] == "auto"));
    }

    #[test]
    fn parses_claude_stream_json() {
        let mut p = StreamParser::default();
        assert!(p.claude_line(r#"{"type":"system","subtype":"init","session_id":"s1"}"#).is_empty());
        assert_eq!(
            p.claude_line(r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"There are "}}}"#),
            vec![AiEvent::Text("There are ".into())]
        );
        assert_eq!(
            p.claude_line(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__tabula__run_query","input":{"sql":"select 1"}}]}}"#),
            vec![AiEvent::Tool { name: "run_query".into(), detail: "select 1".into() }]
        );
        p.claude_line(r#"{"type":"result","subtype":"success","is_error":false,"result":"There are 5","session_id":"s1"}"#);
        assert_eq!(p.session.as_deref(), Some("s1"));
        assert_eq!(p.result.as_deref(), Some("There are 5"));
        assert!(p.error.is_none());
    }

    #[test]
    fn finds_sql_blocks() {
        let t = "Try:\n```sql\nselect 1;\n```\nor\n```\nnot sql\n```\n```sql\nselect 2\n```";
        assert_eq!(sql_blocks(t), vec!["select 1;", "select 2"]);
    }
}
