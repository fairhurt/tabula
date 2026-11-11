<div align="center">

# Tabula

**A fast, native PostgreSQL client — that your AI agents can use too.**

In the spirit of pgAdmin and HeidiSQL, built in Rust with [GPUI Kit](https://gpui-kit.com).
Claude Code and GitHub Copilot CLI can explore and query your databases through Tabula, on your own subscription.

![Rust](https://img.shields.io/badge/Rust-1.97+-b7410e?logo=rust&logoColor=white)
![PostgreSQL](https://img.shields.io/badge/PostgreSQL-tested%20on%2017-336791?logo=postgresql&logoColor=white)
![GPUI Kit](https://img.shields.io/badge/UI-GPUI%20Kit-2563eb)
![MCP](https://img.shields.io/badge/MCP-server-6d28d9)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/query-dark.png">
  <img src="docs/query-light.png" alt="A query tab: SQL editor over a results grid">
</picture>

</div>

<br>

## ✨ Highlights

<table>
<tr>
<td width="50%" valign="top">

### 🗂️ Browse everything
Connections → databases → schemas → tables, views, materialized views, functions and sequences, with row estimates. Double-click a table to page through its rows; right-click anything for more.

</td>
<td width="50%" valign="top">

### ⚡ Query without friction
A highlighted SQL editor, <kbd>Ctrl</kbd>+<kbd>Enter</kbd> to run the selection or everything, several statements at once, Stop, Plan, copy as TSV and save as CSV.

</td>
</tr>
<tr>
<td width="50%" valign="top">

### 🤖 AI that reads your real schema
The Ask AI panel runs Claude Code or Copilot CLI with Tabula's tools, so answers are grounded in your actual tables. SQL in answers gets <b>Insert</b> and <b>Run</b> buttons.

</td>
<td width="50%" valign="top">

### 🛡️ Safe by default
Agents get read-only access, enforced by PostgreSQL itself. Writes can wait for your one-click approval. Agents never see passwords.

</td>
</tr>
</table>

## 📊 Data

Page through 500 rows at a time, filter with any `WHERE` clause, and click a header to sort on the server. NULLs look like NULLs, numbers line up on the right, and the grid stays smooth on big tables.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/data-dark.png">
  <img src="docs/data-light.png" alt="The Data tab browsing shop.orders">
</picture>

## 🧱 Structure

Columns with types, defaults and keys; indexes; constraints including foreign keys; and the full `CREATE` statement, ready to copy.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/structure-dark.png">
  <img src="docs/structure-light.png" alt="The Structure tab for shop.orders">
</picture>

## 🤖 Ask AI

Press <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>A</kbd>. The agent looks things up with Tabula's tools (you see each call) before it answers. Query tabs add **Explain**, **Optimize** and, after an error, **Fix with AI**. <kbd>Ctrl</kbd>+<kbd>K</kbd> in the editor jumps straight to the panel.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/ai-dark.png">
  <img src="docs/ai-light.png" alt="The Ask AI panel answering with a SQL block and Insert / Run buttons">
</picture>

No API keys: Tabula runs `claude -p` or `copilot -p` headless, signed in as you, with **only Tabula's tools** for that run — no shell, no file edits, no other MCP servers, no hooks.

## 🔌 Let your agents use Tabula

`tabula mcp` is an MCP server. Connect it from the app (status bar → **Agents** → **Connect**) or with `tabula agents connect`, and Claude Code or Copilot CLI can work with your databases from their own sessions.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/agents-dark.png">
  <img src="docs/agents-light.png" alt="The AI agents dialog with connect buttons and recent activity">
</picture>

| Tool | What it does |
| --- | --- |
| `list_connections` | Connections shared with agents, and what each allows |
| `list_schemas` · `list_tables` | The catalog, with row estimates and comments |
| `describe_table` | Columns, keys, indexes, foreign keys, size, DDL |
| `sample_rows` | A peek at the data |
| `run_query` | One statement, results as JSON |
| `explain_query` | The plan, optionally with `ANALYZE` (read-only) |
| `open_in_tabula` | Show a query's results in your Tabula window |

### What agents may do

Set per connection, in the connection dialog:

| AI access | Agents can… |
| --- | --- |
| **Off** | Nothing — the connection is hidden from them |
| **Read only** *(default)* | Read, one statement at a time, in a read-only transaction with a 30 s timeout |
| **Ask for writes** | Read, and propose writes that you allow once or decline in Tabula |
| **Read & write** | Run anything — for throwaway databases |

<table>
<tr>
<td width="50%"><picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/connect-dark.png">
  <img src="docs/connect-light.png" alt="The connection dialog with SSL and AI access settings">
</picture></td>
<td width="50%"><picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/approve-dark.png">
  <img src="docs/approve-light.png" alt="An agent asking for approval to run an UPDATE">
</picture></td>
</tr>
<tr>
<td align="center"><sub>Each connection chooses its AI access</sub></td>
<td align="center"><sub>“Ask for writes” in action</sub></td>
</tr>
</table>

> [!NOTE]
> Read-only is enforced by PostgreSQL, not by parsing SQL, and a multi-statement escape like `SELECT 1; COMMIT; DELETE …` is rejected. A read-only transaction still lets functions such as `pg_terminate_backend` run, so for production also give the connection a read-only database role.

## ⌨️ Shortcuts

| Keys | Action |
| --- | --- |
| <kbd>Ctrl</kbd>+<kbd>Enter</kbd> / <kbd>F5</kbd> | Run the selection, or everything |
| <kbd>Ctrl</kbd>+<kbd>T</kbd> | New query tab |
| <kbd>Ctrl</kbd>+<kbd>W</kbd> | Close tab |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>A</kbd> | Toggle Ask AI |
| <kbd>Ctrl</kbd>+<kbd>K</kbd> | Ask AI from the editor |
| <kbd>Ctrl</kbd>+<kbd>R</kbd> | Refresh the object tree |
| <kbd>Ctrl</kbd>+<kbd>C</kbd> | Copy the selected cell, row or column |

On macOS, use <kbd>Cmd</kbd> instead of <kbd>Ctrl</kbd>.

## 💻 Command line

```text
tabula                                  open the app
tabula --url postgres://…               open the app and connect
tabula add NAME URL [--ai MODE]         save a connection (off, read-only, ask, read-write)
tabula agents [connect|disconnect]      register `tabula mcp` with Claude Code and Copilot CLI
tabula mcp [--connection NAME]          the MCP server (agents start this)
tabula ask CONNECTION "question"        ask an agent from the terminal [--copilot]
tabula --screenshot DIR [--url …]       render the views headless to PNGs
```

Or register by hand: `claude mcp add tabula -- /path/to/tabula mcp`.

## 🛠️ Building

Rust 1.97 or newer.

```sh
cargo build --release
./target/release/tabula
```

On Linux, GPUI needs Vulkan and the fontconfig, xkbcommon and OpenSSL dev packages:

```sh
sudo apt install libfontconfig-dev libxkbcommon-x11-dev libssl-dev
```

Saved connections live in your config folder (`tabula/connections.json`). Passwords go in the OS credential store on Windows and macOS, and in a file only you can read on Linux.

## 🧪 Testing

```sh
docker run -d --name tabula-pg -e POSTGRES_PASSWORD=tabula -p 5432:5432 postgres:17
docker exec -i tabula-pg psql -U postgres < scripts/seed.sql
TABULA_TEST_URL=postgres://postgres:tabula@localhost/postgres cargo test
cargo run -- --screenshot shots --url postgres://postgres:tabula@localhost/postgres
```

Database and MCP tests skip without `TABULA_TEST_URL`. The screenshots above come from `--screenshot` against `scripts/seed.sql`.
