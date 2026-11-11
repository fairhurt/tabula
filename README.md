# Tabula

A PostgreSQL viewer and query tool in the spirit of pgAdmin and HeidiSQL, built
with [GPUI Kit](https://gpui-kit.com), with AI agents as first-class users:
Claude Code and GitHub Copilot CLI can explore and query your databases through
Tabula on your own subscription.

![query](docs/query-dark.png)

## Features

- Object tree: connections → databases → schemas → tables, views, materialized
  views, functions, sequences, with row estimates. Right-click for actions.
- Data tab: browse 500 rows a page, `WHERE` filter, sort on the server by
  clicking a header.
- Structure tab: columns, indexes, constraints and the `CREATE` statement.
- Query tabs: SQL editor with highlighting, Ctrl+Enter runs the selection or
  everything, several statements at once, Stop, Plan (`EXPLAIN`), copy as TSV,
  save as CSV.
- Saved connections; passwords in the OS credential store (Windows, macOS) or a
  user-only file (Linux).

## AI

**Agents use Tabula** (`tabula mcp`, an MCP server). Tools: `list_connections`,
`list_schemas`, `list_tables`, `describe_table`, `sample_rows`, `run_query`,
`explain_query`, `open_in_tabula`. Register it from the app (status bar →
Agents → Connect) or `tabula agents connect`, which writes `~/.claude.json` and
`~/.copilot/mcp-config.json` (keeping everything else; a backup is kept).

Each connection sets what agents may do:

| AI access      | What agents can do |
| -------------- | ------------------ |
| Off            | Nothing; the connection is hidden from them |
| Read only      | Default. One statement at a time, in a read-only transaction, 30 s timeout |
| Ask for writes | Read only, and a write pops up in Tabula for you to allow once or decline |
| Read & write   | Anything. For throwaway databases |

Agents only ever see connection names, never passwords. Read-only is enforced
by PostgreSQL itself, not by parsing SQL; for production, also give the
connection a read-only database role (a read-only transaction still lets
functions like `pg_terminate_backend` run).

**Tabula uses agents** (the Ask AI panel, Ctrl+Shift+A). It runs `claude -p` or
`copilot -p` headless with only Tabula's tools for that run (no shell, no file
edits, no other MCP servers, no hooks), so it reads your real schema before
answering. SQL in answers gets Insert / Run buttons. Query tabs have Explain,
Optimize and (after an error) Fix with AI. From a terminal:
`tabula ask <connection> "question" [--copilot]`.

## Command line

```
tabula                                  open the app
tabula --url postgres://…               open the app and connect
tabula add NAME URL [--ai MODE]         save a connection (off, read-only, ask, read-write)
tabula agents [connect|disconnect]      register `tabula mcp` with Claude Code and Copilot CLI
tabula mcp [--connection NAME]          the MCP server (agents start this)
tabula ask CONNECTION "question"        ask an agent from the terminal
tabula --screenshot DIR [--url …]       render the views headless to PNGs
```

## Building

Rust 1.97+. On Linux GPUI needs the fontconfig, xkbcommon and OpenSSL dev
packages (`libfontconfig-dev libxkbcommon-x11-dev libssl-dev`) and Vulkan.

```
cargo build --release
```

## Testing

```
docker run -d --name tabula-pg -e POSTGRES_PASSWORD=tabula -p 5432:5432 postgres:17
docker exec -i tabula-pg psql -U postgres < scripts/seed.sql
TABULA_TEST_URL=postgres://postgres:tabula@localhost/postgres cargo test
cargo run -- --screenshot shots --url postgres://postgres:tabula@localhost/postgres
```

Database and MCP tests skip without `TABULA_TEST_URL`.
