//! `tabula mcp`: an MCP server over stdio (newline-delimited JSON-RPC 2.0),
//! so Claude Code, Copilot CLI or any MCP client can explore and query the
//! databases saved in Tabula.
//!
//! Agents see connection *names* only, never passwords. What they may do is
//! set per connection ([`AiAccess`]): by default every statement runs in a
//! read-only transaction with a timeout; "ask for writes" sends a write to
//! the open Tabula window for you to approve.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail};
use serde_json::{Value, json};

use crate::db::{self, ObjectKind, QueryOutcome, Session};
use crate::link::{self, LinkRequest};
use crate::profiles::{self, AiAccess, Profile};
use crate::rt;

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CELL: usize = 1_000;

const INSTRUCTIONS: &str = "Tabula gives you the user's PostgreSQL databases. Start with `list_connections`, then `list_tables` and `describe_table` to learn the schema before writing SQL. `run_query` runs one statement; most connections are read-only for agents. Use `open_in_tabula` to show the user a query's results in the Tabula app.";

pub struct Server {
    /// Connection used when a tool call names none.
    default_connection: Option<String>,
    /// Profiles to use instead of the saved ones (tests).
    profiles: Option<Vec<(Profile, Option<String>)>>,
    sessions: RefCell<HashMap<String, Arc<Session>>>,
    agent: RefCell<String>,
    /// Ask the open app to approve writes and log activity.
    use_link: bool,
}

impl Server {
    pub fn new(default_connection: Option<String>) -> Self {
        Server {
            default_connection,
            profiles: None,
            sessions: RefCell::new(HashMap::new()),
            agent: RefCell::new("agent".into()),
            use_link: true,
        }
    }

    #[cfg(test)]
    fn with_profiles(profiles: Vec<(Profile, Option<String>)>) -> Self {
        Server {
            profiles: Some(profiles),
            use_link: false,
            ..Server::new(None)
        }
    }

    fn all_profiles(&self) -> Vec<Profile> {
        match &self.profiles {
            Some(p) => p.iter().map(|(p, _)| p.clone()).collect(),
            None => profiles::load(),
        }
        .into_iter()
        .filter(|p| p.ai != AiAccess::Off)
        .collect()
    }

    fn password(&self, name: &str) -> Option<String> {
        match &self.profiles {
            Some(p) => p.iter().find(|(p, _)| p.name == name).and_then(|(_, pw)| pw.clone()),
            None => profiles::password(name),
        }
    }

    fn profile(&self, args: &Value) -> anyhow::Result<Profile> {
        let all = self.all_profiles();
        let wanted = args["connection"]
            .as_str()
            .map(str::to_string)
            .or_else(|| self.default_connection.clone());
        let p = match wanted {
            Some(name) => all.into_iter().find(|p| p.name == name).ok_or_else(|| {
                anyhow!("no connection named {name:?} (call list_connections to see them)")
            })?,
            None if all.len() == 1 => all.into_iter().next().expect("one profile"),
            None if all.is_empty() => {
                bail!("no connections are shared with agents; add one in Tabula first")
            }
            None => bail!("say which `connection` to use (call list_connections to see them)"),
        };
        Ok(p)
    }

    fn session(&self, profile: &Profile) -> anyhow::Result<Arc<Session>> {
        if let Some(s) = self.sessions.borrow().get(&profile.name) {
            return Ok(s.clone());
        }
        let pw = self.password(&profile.name);
        let s = rt::block_on(Session::connect(profile.clone(), pw))?;
        self.sessions
            .borrow_mut()
            .insert(profile.name.clone(), s.clone());
        Ok(s)
    }

    /// Handle one incoming line. Returns the reply line, if any.
    pub fn handle(&self, line: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                return Some(
                    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("parse error: {e}")}})
                        .to_string(),
                );
            }
        };
        let method = msg["method"].as_str().unwrap_or("");
        // Notifications (no id) get no reply.
        let id = msg.get("id").cloned()?;
        let result: Result<Value, (i64, String)> = match method {
            "initialize" => {
                if let Some(n) = msg["params"]["clientInfo"]["name"].as_str() {
                    *self.agent.borrow_mut() = n.to_string();
                }
                let asked = msg["params"]["protocolVersion"].as_str().unwrap_or("");
                let version = if PROTOCOL_VERSIONS.contains(&asked) {
                    asked
                } else {
                    PROTOCOL_VERSIONS[0]
                };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "tabula", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_specs() })),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("");
                let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = match self.call(name, &args) {
                    Ok(t) => (t, false),
                    Err(e) => (e.to_string(), true),
                };
                Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
            }
            "resources/list" => Ok(json!({"resources": []})),
            "prompts/list" => Ok(json!({"prompts": []})),
            other => Err((-32601, format!("method not found: {other}"))),
        };
        Some(
            match result {
                Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
                Err((code, message)) => {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
                }
            }
            .to_string(),
        )
    }

    pub fn serve_stdio(&self) -> anyhow::Result<()> {
        let stdin = std::io::stdin();
        let mut out = std::io::stdout();
        for line in stdin.lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle(&line) {
                writeln!(out, "{reply}")?;
                out.flush()?;
            }
        }
        Ok(())
    }

    fn call(&self, tool: &str, args: &Value) -> anyhow::Result<String> {
        let out = self.call_inner(tool, args);
        if self.use_link && tool != "list_connections" {
            let detail = args["sql"]
                .as_str()
                .or(args["table"].as_str())
                .or(args["schema"].as_str())
                .unwrap_or("")
                .to_string();
            let connection = self.profile(args).map(|p| p.name).unwrap_or_default();
            // Best effort: nothing to log to when the app is closed.
            let _ = link::request(
                &LinkRequest::Activity {
                    agent: self.agent.borrow().clone(),
                    connection,
                    tool: tool.to_string(),
                    detail,
                    ok: out.is_ok(),
                },
                Duration::from_secs(2),
            );
        }
        out
    }

    fn call_inner(&self, tool: &str, args: &Value) -> anyhow::Result<String> {
        match tool {
            "list_connections" => {
                let list: Vec<Value> = self
                    .all_profiles()
                    .iter()
                    .map(|p| {
                        json!({
                            "connection": p.name,
                            "database": p.database,
                            "host": p.host,
                            "access": match p.ai {
                                AiAccess::ReadOnly => "read-only",
                                AiAccess::AskForWrites => "read-only; writes need the user's approval in Tabula",
                                AiAccess::ReadWrite => "read-write",
                                AiAccess::Off => "off",
                            },
                        })
                    })
                    .collect();
                Ok(serde_json::to_string_pretty(&list)?)
            }
            "list_schemas" => {
                let s = self.session(&self.profile(args)?)?;
                let schemas = rt::block_on(s.schemas())?;
                Ok(serde_json::to_string(&schemas)?)
            }
            "list_tables" => {
                let s = self.session(&self.profile(args)?)?;
                let schemas = match args["schema"].as_str() {
                    Some(sc) => vec![sc.to_string()],
                    None => rt::block_on(s.schemas())?
                        .into_iter()
                        .filter(|n| n != "pg_catalog" && n != "information_schema")
                        .collect(),
                };
                let mut out = Vec::new();
                for sc in schemas {
                    for o in rt::block_on(s.objects(&sc))? {
                        out.push(json!({
                            "name": format!("{}.{}", o.schema, o.name),
                            "kind": o.kind,
                            "rows_estimate": o.rows_estimate,
                            "comment": o.comment,
                            "arguments": o.signature,
                        }));
                    }
                }
                Ok(serde_json::to_string_pretty(&strip_nulls(Value::Array(out)))?)
            }
            "describe_table" => {
                let s = self.session(&self.profile(args)?)?;
                let table = args["table"]
                    .as_str()
                    .ok_or_else(|| anyhow!("`table` is required"))?;
                let (schema, name) = self.resolve_table(&s, table)?;
                let info = rt::block_on(s.table_info(&schema, &name))?;
                Ok(serde_json::to_string_pretty(&strip_nulls(
                    serde_json::to_value(&info)?,
                ))?)
            }
            "run_query" => {
                let p = self.profile(args)?;
                let s = self.session(&p)?;
                let sql = args["sql"].as_str().ok_or_else(|| anyhow!("`sql` is required"))?;
                let max = args["max_rows"].as_u64().unwrap_or(100).clamp(1, 5_000) as usize;
                let out = self.run_with_access(&p, &s, sql, max)?;
                Ok(format_outcome(&out))
            }
            "explain_query" => {
                let p = self.profile(args)?;
                let s = self.session(&p)?;
                let sql = args["sql"].as_str().ok_or_else(|| anyhow!("`sql` is required"))?;
                let analyze = args["analyze"].as_bool().unwrap_or(false);
                let explain = format!(
                    "EXPLAIN ({}FORMAT TEXT) {}",
                    if analyze { "ANALYZE, BUFFERS, " } else { "" },
                    sql.trim().trim_end_matches(';')
                );
                let out = rt::block_on(s.run_read_only(&explain, 2_000, QUERY_TIMEOUT))?;
                let plan = out
                    .main()
                    .map(|r| {
                        r.rows
                            .iter()
                            .filter_map(|row| row.first().cloned().flatten())
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                Ok(plan)
            }
            "sample_rows" => {
                let p = self.profile(args)?;
                let s = self.session(&p)?;
                let table = args["table"]
                    .as_str()
                    .ok_or_else(|| anyhow!("`table` is required"))?;
                let (schema, name) = self.resolve_table(&s, table)?;
                let n = args["limit"].as_u64().unwrap_or(10).clamp(1, 200);
                let sql = format!(
                    "SELECT * FROM {}.{} LIMIT {n}",
                    db::quote_ident(&schema),
                    db::quote_ident(&name)
                );
                let out = rt::block_on(s.run_read_only(&sql, n as usize, QUERY_TIMEOUT))?;
                Ok(format_outcome(&out))
            }
            "open_in_tabula" => {
                let p = self.profile(args)?;
                let sql = args["sql"].as_str().ok_or_else(|| anyhow!("`sql` is required"))?;
                if !self.use_link {
                    bail!("Tabula isn't open");
                }
                let reply = link::request(
                    &LinkRequest::Open {
                        connection: p.name.clone(),
                        sql: sql.to_string(),
                        title: args["title"].as_str().map(str::to_string),
                    },
                    Duration::from_secs(10),
                )
                .map_err(|_| anyhow!("Tabula isn't open, so the query can't be shown there. Ask the user to open Tabula, or use run_query instead."))?;
                if reply.ok {
                    Ok("Opened in a new query tab in Tabula.".into())
                } else {
                    bail!(reply.message.unwrap_or_else(|| "Tabula couldn't open it".into()))
                }
            }
            other => bail!("unknown tool: {other}"),
        }
    }

    fn run_with_access(
        &self,
        p: &Profile,
        s: &Session,
        sql: &str,
        max: usize,
    ) -> anyhow::Result<QueryOutcome> {
        if p.ai == AiAccess::ReadWrite {
            return rt::block_on(s.run_write(sql, max, QUERY_TIMEOUT));
        }
        match rt::block_on(s.run_read_only(sql, max, QUERY_TIMEOUT)) {
            Err(e) if db::needs_write_access(&e) => {
                if p.ai != AiAccess::AskForWrites {
                    bail!(
                        "{e}\nThis connection is read-only for agents. The user can change that in Tabula's connection settings."
                    );
                }
                if !self.use_link {
                    bail!("writes need the user's approval in Tabula, and Tabula isn't open");
                }
                let reply = link::request(
                    &LinkRequest::Approve {
                        agent: self.agent.borrow().clone(),
                        connection: p.name.clone(),
                        sql: sql.to_string(),
                    },
                    APPROVAL_TIMEOUT,
                )
                .map_err(|_| {
                    anyhow!("writes need the user's approval in Tabula, and Tabula isn't open")
                })?;
                if !reply.ok {
                    bail!(
                        "the user didn't allow this statement{}",
                        reply.message.map(|m| format!(": {m}")).unwrap_or_default()
                    );
                }
                rt::block_on(s.run_write(sql, max, QUERY_TIMEOUT))
            }
            other => other,
        }
    }

    /// `schema.table`, or a bare name looked up across schemas.
    fn resolve_table(&self, s: &Session, table: &str) -> anyhow::Result<(String, String)> {
        let unquote = |x: &str| x.trim().trim_matches('"').replace("\"\"", "\"");
        if let Some((sc, n)) = table.split_once('.') {
            return Ok((unquote(sc), unquote(n)));
        }
        let name = unquote(table);
        let schemas = rt::block_on(s.schemas())?;
        let mut found = Vec::new();
        for sc in schemas.iter().filter(|n| *n != "pg_catalog" && *n != "information_schema") {
            if rt::block_on(s.objects(sc))?
                .iter()
                .any(|o| o.name == name && o.kind != ObjectKind::Function)
            {
                found.push(sc.clone());
            }
        }
        match found.len() {
            0 => bail!("no table or view named {name:?}"),
            1 => Ok((found.remove(0), name)),
            _ => bail!(
                "{name:?} exists in several schemas ({}); use schema.table",
                found.join(", ")
            ),
        }
    }
}

/// Results as compact JSON: columns, rows and counts.
fn format_outcome(out: &QueryOutcome) -> String {
    let cap = |v: &Option<String>| -> Value {
        match v {
            None => Value::Null,
            Some(s) if s.chars().count() > MAX_CELL => {
                Value::String(format!("{}…", s.chars().take(MAX_CELL).collect::<String>()))
            }
            Some(s) => Value::String(s.clone()),
        }
    };
    let Some(r) = out.main() else {
        return json!({"ok": true, "elapsed_ms": out.elapsed.as_millis()}).to_string();
    };
    let mut v = json!({
        "row_count": r.row_count,
        "elapsed_ms": out.elapsed.as_millis(),
    });
    if r.has_rows() {
        v["columns"] = json!(r
            .columns
            .iter()
            .map(|c| match &c.type_name {
                Some(t) => format!("{} ({t})", c.name),
                None => c.name.clone(),
            })
            .collect::<Vec<_>>());
        v["rows"] = Value::Array(
            r.rows
                .iter()
                .map(|row| Value::Array(row.iter().map(cap).collect()))
                .collect(),
        );
        if r.truncated {
            v["truncated"] = json!(format!(
                "only the first {} rows are shown; raise max_rows or add LIMIT/aggregates",
                r.rows.len()
            ));
        }
    } else {
        v["rows_affected"] = json!(r.row_count);
    }
    v.to_string()
}

fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(_, v)| !v.is_null())
                .map(|(k, v)| (k, strip_nulls(v)))
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(strip_nulls).collect()),
        v => v,
    }
}

fn tool_specs() -> Value {
    let conn = json!({"type": "string", "description": "Connection name from list_connections. Optional when there is only one."});
    json!([
        {
            "name": "list_connections",
            "description": "List the database connections the user has shared with agents, and what each allows (read-only or read-write).",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "list_schemas",
            "description": "List the schemas in a database.",
            "inputSchema": {"type": "object", "properties": {"connection": conn}},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "list_tables",
            "description": "List tables, views, materialized views, sequences and functions, with row estimates and comments. Leave out `schema` for every non-system schema.",
            "inputSchema": {"type": "object", "properties": {
                "connection": conn,
                "schema": {"type": "string"}
            }},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "describe_table",
            "description": "Columns (types, nullability, defaults, primary key), indexes, constraints including foreign keys, size, and the CREATE statement for a table or view.",
            "inputSchema": {"type": "object", "properties": {
                "connection": conn,
                "table": {"type": "string", "description": "schema.table, or a table name that is unique across schemas"}
            }, "required": ["table"]},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "sample_rows",
            "description": "The first few rows of a table or view, to see what the data looks like.",
            "inputSchema": {"type": "object", "properties": {
                "connection": conn,
                "table": {"type": "string"},
                "limit": {"type": "integer", "default": 10, "maximum": 200}
            }, "required": ["table"]},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "run_query",
            "description": "Run one SQL statement and return its rows as JSON. On read-only connections it runs in a read-only transaction; writes fail, or wait for the user's approval in Tabula when the connection asks for it. 30 s timeout.",
            "inputSchema": {"type": "object", "properties": {
                "connection": conn,
                "sql": {"type": "string"},
                "max_rows": {"type": "integer", "default": 100, "maximum": 5000}
            }, "required": ["sql"]}
        },
        {
            "name": "explain_query",
            "description": "Show PostgreSQL's plan for a statement. With analyze=true it runs the statement (read-only) and reports actual timings and buffers.",
            "inputSchema": {"type": "object", "properties": {
                "connection": conn,
                "sql": {"type": "string"},
                "analyze": {"type": "boolean", "default": false}
            }, "required": ["sql"]},
            "annotations": {"readOnlyHint": true}
        },
        {
            "name": "open_in_tabula",
            "description": "Open a query in a new tab in the user's Tabula window and run it there, so they can see and work with the full results. Needs Tabula to be open.",
            "inputSchema": {"type": "object", "properties": {
                "connection": conn,
                "sql": {"type": "string"},
                "title": {"type": "string", "description": "Short tab title"}
            }, "required": ["sql"]}
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiles::SslMode;

    fn server(access: AiAccess) -> Option<Server> {
        let url = std::env::var("TABULA_TEST_URL").ok()?;
        let (p, pw) = Profile::from_url("demo", &url).unwrap();
        let p = Profile {
            ssl: SslMode::Disable,
            ai: access,
            ..p
        };
        let off = Profile {
            name: "hidden".into(),
            ai: AiAccess::Off,
            ..p.clone()
        };
        Some(Server::with_profiles(vec![(p, pw.clone()), (off, pw)]))
    }

    fn call(s: &Server, id: u64, method: &str, params: Value) -> Value {
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        serde_json::from_str(&s.handle(&line).unwrap()).unwrap()
    }

    fn tool(s: &Server, name: &str, args: Value) -> (String, bool) {
        let r = call(s, 9, "tools/call", json!({"name": name, "arguments": args}));
        (
            r["result"]["content"][0]["text"].as_str().unwrap().to_string(),
            r["result"]["isError"].as_bool().unwrap(),
        )
    }

    #[test]
    fn speaks_mcp_without_a_server() {
        let s = Server::with_profiles(vec![]);
        let init = call(&s, 1, "initialize", json!({"protocolVersion": "2025-06-18", "clientInfo": {"name": "claude-code"}}));
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(*s.agent.borrow(), "claude-code");
        assert!(s.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let tools = call(&s, 2, "tools/list", json!({}));
        assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 8);
        let (text, err) = tool(&s, "list_tables", json!({}));
        assert!(err && text.contains("no connections"), "{text}");
        assert_eq!(call(&s, 3, "nope", json!({}))["error"]["code"], -32601);
    }

    #[test]
    fn agents_explore_and_query_read_only() {
        let Some(s) = server(AiAccess::ReadOnly) else {
            eprintln!("TABULA_TEST_URL not set; skipped");
            return;
        };
        let (text, _) = tool(&s, "list_connections", json!({}));
        assert!(text.contains("demo") && !text.contains("hidden"), "{text}");
        assert!(!text.contains("tabula"), "no password leaks: {text}");

        let (text, err) = tool(&s, "list_tables", json!({"schema": "shop"}));
        assert!(!err && text.contains("shop.orders"), "{text}");
        // Only one shared connection, so `connection` can be left out.
        let (text, err) = tool(&s, "describe_table", json!({"table": "orders"}));
        assert!(!err && text.contains("orders_customer_idx") && text.contains("FOREIGN KEY"), "{text}");

        let (text, err) = tool(&s, "run_query", json!({"sql": "select status, count(*) from shop.orders group by 1 order by 1"}));
        assert!(!err, "{text}");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["rows"].as_array().unwrap().len(), 4);
        assert_eq!(v["rows"][0][1], "25000");

        let (text, err) = tool(&s, "run_query", json!({"sql": "delete from shop.order_items"}));
        assert!(err && text.contains("read-only for agents"), "{text}");
        let (text, err) = tool(&s, "run_query", json!({"sql": "select 1; delete from shop.order_items"}));
        assert!(err, "{text}");

        let (text, err) = tool(&s, "run_query", json!({"sql": "select id from shop.orders", "max_rows": 5}));
        assert!(!err && text.contains("truncated"), "{text}");

        let (text, err) = tool(&s, "explain_query", json!({"sql": "select * from shop.orders where customer_id = 7"}));
        assert!(!err && text.contains("orders_customer_idx"), "{text}");

        let (text, err) = tool(&s, "sample_rows", json!({"table": "shop.customers", "limit": 3}));
        assert!(!err && text.contains("user1@example.com"), "{text}");

        let (_, err) = tool(&s, "run_query", json!({"connection": "hidden", "sql": "select 1"}));
        assert!(err, "connections with AI off are invisible");
    }

    #[test]
    fn ask_for_writes_needs_the_app() {
        let Some(s) = server(AiAccess::AskForWrites) else {
            return;
        };
        let (text, err) = tool(&s, "run_query", json!({"sql": "update shop.orders set note = 'x' where id = 1"}));
        assert!(err && text.contains("approval"), "{text}");
        let (_, err) = tool(&s, "run_query", json!({"sql": "select 1"}));
        assert!(!err);
    }

    /// A write on an "ask" connection goes to the app over the live link and
    /// runs only if the app says yes.
    #[test]
    fn approvals_go_through_the_app() {
        let Some(mut s) = server(AiAccess::AskForWrites) else {
            return;
        };
        let home = tempfile::tempdir().unwrap();
        // SAFETY: the only test that sets TABULA_HOME.
        unsafe { std::env::set_var("TABULA_HOME", home.path()) };
        s.use_link = true;
        let mut rx = link::listen().unwrap();
        // The "app": allow statements that touch row 1 only.
        std::thread::spawn(move || {
            futures::executor::block_on(async {
                use futures::StreamExt as _;
                while let Some((req, reply)) = rx.next().await {
                    let r = match req {
                        LinkRequest::Approve { sql, .. } if sql.contains("id = 1") => link::LinkReply::yes(),
                        LinkRequest::Approve { .. } => link::LinkReply::no("declined"),
                        _ => link::LinkReply::yes(),
                    };
                    let _ = reply.send(r);
                }
            })
        });
        let (text, err) = tool(&s, "run_query", json!({"sql": "update shop.orders set note = 'approved' where id = 1"}));
        assert!(!err, "{text}");
        let (text, err) = tool(&s, "run_query", json!({"sql": "update shop.orders set note = 'nope' where id = 2"}));
        assert!(err && text.contains("didn't allow"), "{text}");
        let (text, _) = tool(&s, "run_query", json!({"sql": "select id, note from shop.orders where id in (1, 2) order by id"}));
        assert!(text.contains("approved") && !text.contains("nope"), "{text}");
        tool(&s, "run_query", json!({"sql": "select 1"}));
        // Put row 1 back (approved, since it touches id = 1).
        let (text, err) = tool(&s, "run_query", json!({"sql": "update shop.orders set note = null where id = 1"}));
        assert!(!err, "{text}");
    }
}
