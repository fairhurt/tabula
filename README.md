<div align="center">

<img src="docs/icon.png" width="112" alt="Tabula's icon: a table whose first cell is a ringed planet">

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
A highlighted SQL editor that completes your tables, columns, aliases and SQL keywords as you type, <kbd>Ctrl</kbd>+<kbd>Enter</kbd> to run the selection or everything, several statements at once, Stop, Plan, copy as TSV and save as CSV.

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

Page through 500 rows at a time, filter with any `WHERE` clause (the filter box completes the table's columns), and click a header to sort on the server. NULLs look like NULLs, numbers line up on the right, and the grid stays smooth on big tables.

Tables with a primary key can be edited in place. Double-click a cell (or press <kbd>F2</kbd>) to change it, **+ Row** to add one, right-click for **Set to NULL**, **Duplicate row** and **Delete row**. Nothing touches the database until you press **Save** (<kbd>Ctrl</kbd>+<kbd>S</kbd>): edits are highlighted, **Review SQL** shows exactly what will run, and everything is applied in one transaction, so either all of it is saved or none of it. An update or delete that no longer matches exactly one row stops the save.

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

## 🪐 Schema map

Right-click a schema and pick **Schema map** (or use the orbit button above the tree) to see every table as a card with its columns and keys, and every foreign key as a link from the column that holds it to the column it references. A small physics simulation lays it out: linked tables pull together, all tables push apart, and when you drag one its neighbours follow. Drag the background to pan, scroll to zoom, double-click a table for its rows, and search to fly to a table. Tables you drag stay pinned; **Lay out again** frees them. In the dark theme the map sits on a starfield; in the light theme on a dotted grid.

## 🕘 Query history

Every statement you run from a query tab is remembered, with when it ran, how long it took and whether it failed. <kbd>Ctrl</kbd>+<kbd>H</kbd> opens the history: type to search, <kbd>Enter</kbd> puts a query back in the editor, <kbd>Ctrl</kbd>+<kbd>Enter</kbd> runs it again. It is kept in `tabula/history.jsonl` beside your connections (the latest 1,000).

## 🤖 Ask AI

Press <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>A</kbd>. The agent looks things up with Tabula's tools (you see each call) before it answers. Query tabs add **Explain**, **Optimize** and, after an error, **Fix with AI**. <kbd>Ctrl</kbd>+<kbd>K</kbd> in the editor jumps straight to the panel.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/ai-dark.png">
  <img src="docs/ai-light.png" alt="The Ask AI panel answering with a SQL block and Insert / Run buttons">
</picture>

Pick the model from the menu beside the agent buttons: the agent's own default, a listed model, or any id you type. The choice is remembered per agent.

No API keys: both agents run signed in as you, with **only Tabula's tools** — no shell, no file edits, no other MCP servers. Claude Code runs headless with `claude -p`. Copilot runs through [GitHub's Copilot SDK](https://github.com/github/copilot-sdk): Tabula keeps one Copilot CLI server for the life of the app, so a conversation stays in one session, text streams as it is written, you see each tool call, and the model menu lists what your account offers. If the installed Copilot CLI is too old for the SDK, Tabula falls back to `copilot -p`.

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
| <kbd>Ctrl</kbd>+<kbd>H</kbd> | Query history |
| <kbd>F2</kbd> / double-click | Edit a cell in a Data tab |
| <kbd>Ctrl</kbd>+<kbd>S</kbd> | Save a Data tab's edits |
| <kbd>Ctrl</kbd>+<kbd>=</kbd> / <kbd>Ctrl</kbd>+<kbd>-</kbd> / <kbd>Ctrl</kbd>+<kbd>0</kbd> | Larger text, smaller text, reset |

On macOS, use <kbd>Cmd</kbd> instead of <kbd>Ctrl</kbd>.

## 💻 Command line

```text
tabula                                  open the app
tabula --url postgres://…               open the app and connect
tabula --connect NAME                   open the app on a saved connection
tabula add NAME URL [--ai MODE]         save a connection (off, read-only, ask, read-write)
tabula agents [connect|disconnect]      register `tabula mcp` with Claude Code and Copilot CLI
tabula mcp [--connection NAME]          the MCP server (agents start this)
tabula ask CONNECTION "question"        ask an agent from the terminal [--copilot] [--model M]
tabula --screenshot DIR [--url …]       render the views headless to PNGs (macOS and Linux)
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

The app icon is drawn in `assets/icon.svg`; the build renders it and, on Windows, embeds it in the executable.

Saved connections live in your config folder (`tabula/connections.json`), next to `settings.json` (text size, Ask AI agent and model) and `history.jsonl`. Passwords go in the OS credential store on Windows and macOS, and in a file only you can read on Linux.

## 🐘 Demo database

A small seeded PostgreSQL (a shop: 2,000 customers, 500 products, 100,000 orders, a view, a function and an enum):

```sh
docker compose up -d
tabula --url postgres://demo:tabula@localhost:5433/demo
```

Set `TABULA_DEMO_PASSWORD` in `.env` to change the password. It also creates `ai_readonly` (password `TABULA_AI_PASSWORD`, default `tabula-ai`), a read-only login you can give agents.

## 🧪 Testing

```sh
docker run -d --name tabula-pg -e POSTGRES_PASSWORD=tabula -p 5432:5432 postgres:17
docker exec -i tabula-pg psql -U postgres < scripts/seed.sql
TABULA_TEST_URL=postgres://postgres:tabula@localhost/postgres cargo test
cargo run -- --screenshot shots --url postgres://postgres:tabula@localhost/postgres
```

`cargo test -- --ignored copilot` asks a signed-in Copilot for its model list, and `TABULA_AI_DEBUG=1 tabula ask NAME "question" --copilot` prints every event the SDK sends.

Database and MCP tests skip without `TABULA_TEST_URL`. `TABULA_TEST_CONNECTION=name` runs the database test on a saved connection instead, taking its password from the credential store. The screenshots above come from `--screenshot` against `scripts/seed.sql`; headless rendering needs macOS or Linux.
