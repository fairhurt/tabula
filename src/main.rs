//! Tabula: a PostgreSQL viewer and query tool built with GPUI Kit, that AI
//! agents can use too.
//!
//!   tabula                                  open the app
//!   tabula --url postgres://…               open the app and connect
//!   tabula mcp [--connection NAME]          MCP server on stdio, for agents
//!   tabula add NAME URL [--ai MODE]         save a connection (MODE: off,
//!                                           read-only, ask, read-write)
//!   tabula agents [connect|disconnect]      register `tabula mcp` with
//!                                           Claude Code and Copilot CLI
//!   tabula ask CONNECTION "question" [--copilot]
//!                                           ask an agent from the terminal
//!   tabula --screenshot DIR [--url …]       render the views headless to PNGs
// A GUI program on Windows: no console window (except for `mcp`, which
// agents start with pipes, so it doesn't need one).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod agents;
mod ai;
mod db;
mod link;
mod mcp;
mod profiles;
mod rt;
mod ui;

use anyhow::bail;

use crate::profiles::{AiAccess, Profile};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    match args.first().map(String::as_str) {
        Some("mcp") => mcp::Server::new(flag("--connection")).serve_stdio(),
        Some("add") => {
            let (Some(name), Some(url)) = (args.get(1), args.get(2)) else {
                bail!("usage: tabula add NAME postgres://user:pass@host/db [--ai off|read-only|ask|read-write]");
            };
            let (mut p, pw) = Profile::from_url(name, url)?;
            p.ai = match flag("--ai").as_deref() {
                None | Some("read-only") => AiAccess::ReadOnly,
                Some("off") => AiAccess::Off,
                Some("ask") => AiAccess::AskForWrites,
                Some("read-write") => AiAccess::ReadWrite,
                Some(o) => bail!("unknown --ai mode {o:?}"),
            };
            if pw.is_some() {
                profiles::set_password(&p.name, pw.as_deref())?;
            }
            println!("saved {} ({}, AI: {})", p.name, p.summary(), p.ai.label());
            profiles::upsert(p)?;
            Ok(())
        }
        Some("agents") => {
            let exe = std::env::current_exe()?;
            for a in agents::Agent::ALL {
                match args.get(1).map(String::as_str) {
                    Some("connect") => {
                        let p = a.connect(&exe)?;
                        println!("{}: registered in {}", a.label(), p.display());
                    }
                    Some("disconnect") => {
                        let removed = a.disconnect()?;
                        println!("{}: {}", a.label(), if removed { "removed" } else { "wasn't registered" });
                    }
                    _ => println!(
                        "{}: {} · MCP {:?}",
                        a.label(),
                        if a.installed() { "installed" } else { "not found" },
                        a.mcp_status(&exe)
                    ),
                }
            }
            Ok(())
        }
        Some("ask") => {
            let (Some(conn), Some(prompt)) = (args.get(1), args.get(2)) else {
                bail!("usage: tabula ask CONNECTION \"question\" [--copilot]");
            };
            let p = profiles::load()
                .into_iter()
                .find(|p| &p.name == conn)
                .ok_or_else(|| anyhow::anyhow!("no connection named {conn:?}"))?;
            let agent = if args.iter().any(|a| a == "--copilot") {
                agents::Agent::Copilot
            } else {
                agents::Agent::Claude
            };
            let (mut rx, _run) = ai::ask(ai::AskRequest {
                agent,
                connection: p.name.clone(),
                database: p.database.clone(),
                prompt: prompt.clone(),
                history: vec![],
                resume: None,
            });
            futures::executor::block_on(async {
                use futures::StreamExt as _;
                use std::io::Write as _;
                while let Some(ev) = rx.next().await {
                    match ev {
                        ai::AiEvent::Text(t) => {
                            print!("{t}");
                            let _ = std::io::stdout().flush();
                        }
                        ai::AiEvent::Tool { name, detail } => eprintln!("\n[tool] {name} {detail}"),
                        ai::AiEvent::Done { error, .. } => {
                            println!();
                            if let Some(e) = error {
                                eprintln!("error: {e}");
                            }
                            break;
                        }
                    }
                }
            });
            Ok(())
        }
        Some("-h" | "--help") => {
            println!("{}", include_str!("main.rs").lines().skip(3).take(11).map(|l| l.trim_start_matches("//!")).collect::<Vec<_>>().join("\n"));
            Ok(())
        }
        _ => {
            let url = flag("--url");
            match flag("--screenshot") {
                Some(dir) => {
                    ui::screenshots(dir.into(), url)?;
                    // Skip teardown: dropping the headless GPU context with
                    // the database runtime still up can crash on exit.
                    std::process::exit(0)
                }
                None => ui::run(url),
            }
        }
    }
}
