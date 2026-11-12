//! GitHub Copilot through its SDK.
//!
//! One Copilot CLI server is started on first use and kept for the life of
//! the app; the SDK talks to it over JSON-RPC. That gives what running
//! `copilot -p` per question could not: a session that remembers the
//! conversation, text as it streams, each tool call as it happens, and the
//! list of models the signed-in account can use.
//!
//! The session gets Tabula's MCP server for the connection in use and nothing
//! else. Every other permission request (shell, file writes, file reads,
//! URLs) is refused.

use std::sync::{Arc, OnceLock};

use anyhow::anyhow;
use github_copilot_sdk::session::Session;
use github_copilot_sdk::types::{
    McpServerConfig, McpStdioServerConfig, MessageOptions, PermissionRequestKind, SessionConfig,
    SystemMessageConfig,
};
use github_copilot_sdk::{CliProgram, Client, ClientOptions, permission};
use serde_json::Value;
use tokio::sync::{Mutex, Notify};

use crate::agents::{self, Agent};
use crate::ai::{self, AiEvent, AskRequest, Run};

type Events = futures::channel::mpsc::UnboundedSender<AiEvent>;

/// The session a conversation continues in.
struct Live {
    id: String,
    session: Arc<Session>,
    connection: String,
    model: Option<String>,
}

#[derive(Default)]
struct State {
    client: Option<Arc<Client>>,
    live: Option<Live>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

fn sdk<T>(r: Result<T, github_copilot_sdk::Error>) -> anyhow::Result<T> {
    r.map_err(|e| anyhow!("{e}"))
}

/// The running Copilot server, started if it isn't yet.
async fn client(st: &mut State) -> anyhow::Result<Arc<Client>> {
    if let Some(c) = &st.client {
        return Ok(c.clone());
    }
    let (program, pre) = agents::resolve_command(Agent::Copilot.command());
    let mut options = ClientOptions::default();
    options.program = CliProgram::Path(program);
    options.prefix_args = pre.into_iter().map(Into::into).collect();
    options.working_directory = ai::scratch_dir();
    let c = Arc::new(sdk(Client::start(options).await)?);
    st.client = Some(c.clone());
    Ok(c)
}

/// The models the signed-in account can pick, as (name, id).
pub async fn models() -> anyhow::Result<Vec<(String, String)>> {
    let client = client(&mut *state().lock().await).await?;
    Ok(sdk(client.list_models().await)?.into_iter().map(|m| (m.name, m.id)).collect())
}

fn session_config(req: &AskRequest) -> SessionConfig {
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "tabula".into());
    let mut server = McpStdioServerConfig::default();
    server.command = exe;
    server.args = vec!["mcp".into(), "--connection".into(), req.connection.clone()];
    let mut cfg = SessionConfig::default()
        .with_system_message(SystemMessageConfig::new().with_content(ai::system_prompt(req)))
        // Tabula's own tools and nothing else: no shell, no files, no URLs.
        .with_permission_handler(permission::approve_if(|request| {
            request.kind == Some(PermissionRequestKind::Mcp)
                && request.extra.to_string().contains(agents::SERVER_NAME)
        }));
    cfg.streaming = Some(true);
    cfg.model = req.model.clone();
    cfg.client_name = Some("tabula".into());
    cfg.mcp_servers = Some(
        [(agents::SERVER_NAME.to_string(), McpServerConfig::Stdio(server))].into_iter().collect(),
    );
    // Only the server above: not the MCP servers and instructions the user
    // has set up for their own Copilot sessions.
    cfg.enable_config_discovery = Some(false);
    // The agent may use Tabula's tools and no others. Copilot runs some
    // shell commands without asking, so refusing permission requests is not
    // enough: the built-in tools must not be offered at all.
    cfg.available_tools = Some(
        crate::mcp::TOOL_NAMES.iter().map(|t| format!("{}-{t}", agents::SERVER_NAME)).collect(),
    );
    cfg
}

/// The session for this turn: the one the conversation is already in, or a
/// new one.
async fn session_for(req: &AskRequest) -> anyhow::Result<(String, Arc<Session>)> {
    let mut st = state().lock().await;
    let client = client(&mut st).await?;
    let same = st.live.as_ref().is_some_and(|l| {
        req.resume.as_deref() == Some(l.id.as_str())
            && l.connection == req.connection
            // Going back to the default model needs a fresh session.
            && (l.model == req.model || req.model.is_some())
    });
    if same {
        let live = st.live.as_mut().expect("checked above");
        if live.model != req.model {
            if let Some(m) = &req.model {
                sdk(live.session.set_model(m, None).await)?;
                live.model = req.model.clone();
            }
        }
        return Ok((live.id.clone(), live.session.clone()));
    }
    if let Some(old) = st.live.take() {
        let _ = old.session.disconnect().await;
    }
    let session = Arc::new(sdk(client.create_session(session_config(req)).await)?);
    let id = session.id().to_string();
    st.live = Some(Live {
        id: id.clone(),
        session: session.clone(),
        connection: req.connection.clone(),
        model: req.model.clone(),
    });
    Ok((id, session))
}

/// What a tool call was about, for the line the panel shows.
fn tool_detail(arguments: Option<&Value>) -> String {
    let Some(args) = arguments else { return String::new() };
    ["sql", "table", "schema", "connection"]
        .iter()
        .find_map(|k| args.get(k).and_then(Value::as_str))
        .map(str::to_string)
        .unwrap_or_default()
}

/// Send the prompt and forward what comes back until the agent is idle.
async fn turn(session: &Session, req: &AskRequest, tx: &Events, abort: &Notify) -> anyhow::Result<()> {
    let mut events = session.subscribe();
    sdk(session.send(MessageOptions::new(req.prompt.clone())).await)?;
    // Whether this message's text already arrived as deltas, and whether
    // text has been shown before it (so the next message starts a paragraph).
    let (mut streamed, mut shown) = (false, false);
    loop {
        let event = tokio::select! {
            _ = abort.notified() => {
                let _ = session.abort().await;
                return Err(anyhow!("Stopped"));
            }
            event = events.recv() => event.map_err(|e| anyhow!("lost the Copilot session: {e}"))?,
        };
        if std::env::var_os("TABULA_AI_DEBUG").is_some() {
            let data: String = event.data.to_string().chars().take(600).collect();
            eprintln!("[copilot] {} {data}", event.event_type);
        }
        let text = |key: &str| event.data.get(key).and_then(Value::as_str).unwrap_or("");
        match event.event_type.as_str() {
            "assistant.message_delta" => {
                let delta = text("deltaContent");
                if !delta.is_empty() {
                    if !streamed && shown {
                        let _ = tx.unbounded_send(AiEvent::Text("\n\n".into()));
                    }
                    streamed = true;
                    shown = true;
                    let _ = tx.unbounded_send(AiEvent::Text(delta.to_string()));
                }
            }
            "assistant.message" => {
                let whole = text("content");
                if !streamed && !whole.is_empty() {
                    let sep = if shown { "\n\n" } else { "" };
                    let _ = tx.unbounded_send(AiEvent::Text(format!("{sep}{whole}")));
                    shown = true;
                }
                streamed = false;
            }
            "tool.execution_start" => {
                let name = Some(text("mcpToolName")).filter(|n| !n.is_empty()).unwrap_or(text("toolName"));
                // Copilot's own bookkeeping tools aren't worth a line.
                if !name.is_empty() && name != "report_intent" {
                    let _ = tx.unbounded_send(AiEvent::Tool {
                        name: name.to_string(),
                        detail: tool_detail(event.data.get("arguments")),
                    });
                }
            }
            "session.error" => {
                let message = text("message");
                return Err(anyhow!("{}", if message.is_empty() { "Copilot reported an error" } else { message }));
            }
            "session.idle" => return Ok(()),
            _ => {}
        }
    }
}

/// Start a run. Events arrive on the returned channel, ending with `Done`.
/// If the installed Copilot CLI can't be driven by the SDK (it is too old,
/// say), the run falls back to `copilot -p`.
pub fn ask(req: AskRequest) -> (futures::channel::mpsc::UnboundedReceiver<AiEvent>, Run) {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    let abort = Arc::new(Notify::new());
    let run = Run::aborting(abort.clone());
    crate::rt::runtime().spawn(async move {
        let (id, session) = match session_for(&req).await {
            Ok(s) => s,
            Err(e) => {
                fall_back(req, tx, &abort, e).await;
                return;
            }
        };
        let error = turn(&session, &req, &tx, &abort).await.err().map(|e| e.to_string());
        let _ = tx.unbounded_send(AiEvent::Done { session: Some(id), error });
    });
    (rx, run)
}

/// Answer with `copilot -p` instead, saying why if that fails too.
async fn fall_back(req: AskRequest, tx: Events, abort: &Notify, why: anyhow::Error) {
    use futures::StreamExt as _;
    let (mut events, cli) = ai::ask_cli(req);
    loop {
        let event = tokio::select! {
            _ = abort.notified() => {
                cli.stop();
                continue;
            }
            event = events.next() => event,
        };
        match event {
            Some(AiEvent::Done { session, error }) => {
                let error = error.map(|e| format!("{e}\n(Copilot's SDK couldn't be used either: {why})"));
                let _ = tx.unbounded_send(AiEvent::Done { session, error });
                return;
            }
            Some(e) => {
                let _ = tx.unbounded_send(e);
            }
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_lines_show_what_was_asked() {
        assert_eq!(tool_detail(Some(&json!({"connection": "demo", "sql": "select 1"}))), "select 1");
        assert_eq!(tool_detail(Some(&json!({"table": "shop.orders"}))), "shop.orders");
        assert_eq!(tool_detail(Some(&json!({"limit": 5}))), "");
        assert_eq!(tool_detail(None), "");
    }

    #[test]
    fn a_session_gets_only_tabulas_server() {
        let req = AskRequest {
            agent: Agent::Copilot,
            connection: "shop db".into(),
            database: "postgres".into(),
            prompt: "hi".into(),
            history: vec![],
            resume: None,
            model: Some("auto".into()),
        };
        let cfg = session_config(&req);
        assert_eq!(cfg.model.as_deref(), Some("auto"));
        assert_eq!(cfg.streaming, Some(true));
        assert_eq!(cfg.enable_config_discovery, Some(false));
        let tools = cfg.available_tools.clone().expect("an allowlist");
        assert!(tools.contains(&"tabula-run_query".to_string()));
        assert!(tools.iter().all(|t| t.starts_with("tabula-")), "{tools:?}");
        let servers = cfg.mcp_servers.expect("servers");
        assert_eq!(servers.len(), 1);
        let McpServerConfig::Stdio(s) = &servers[agents::SERVER_NAME] else { panic!("stdio") };
        assert_eq!(s.args, vec!["mcp", "--connection", "shop db"]);
    }

    /// Needs Copilot installed and signed in: `cargo test -- --ignored copilot`.
    #[test]
    #[ignore]
    fn lists_the_accounts_models() {
        // Some accounts are offered no named models, only "auto".
        let models = crate::rt::block_on(models()).expect("models");
        println!("{} models", models.len());
        for (name, id) in models {
            println!("{name} = {id}");
        }
    }
}
