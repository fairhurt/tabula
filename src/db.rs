//! Database access: connecting, the catalog, and running SQL.
//!
//! [`Session`] is what the rest of Tabula talks to. Behind it is one of four
//! backends, [`postgres`], [`mysql`], [`mssql`] or [`sqlite`], picked by the
//! profile's engine.
//! Everything here is async on tokio (see [`crate::rt`]), and every value
//! comes back as text, so any type displays without per-type decoding.

mod mssql;
mod mysql;
mod postgres;
mod script;
pub(crate) mod sqlite;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::profiles::{Engine, Profile};

/// One column of a result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultColumn {
    pub name: String,
    /// The type name, when known: PostgreSQL's for a single statement,
    /// SQLite's declared type for a column that comes straight from a table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
    /// Holds numbers, which read best right-aligned.
    #[serde(skip)]
    pub numeric: bool,
}

impl ResultColumn {
    pub fn is_numeric(&self) -> bool {
        self.numeric
    }
}


/// The outcome of one statement.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ResultSet {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Option<String>>>,
    /// Rows returned or affected, as the server counted them.
    pub row_count: u64,
    /// More rows came back than were kept.
    pub truncated: bool,
}

impl ResultSet {
    pub fn has_rows(&self) -> bool {
        !self.columns.is_empty()
    }
}

/// All statements in one run.
#[derive(Debug, Clone, Default)]
pub struct QueryOutcome {
    pub results: Vec<ResultSet>,
    pub elapsed: Duration,
}

impl QueryOutcome {
    /// The last statement that returned rows, else the last one.
    pub fn main(&self) -> Option<&ResultSet> {
        self.results
            .iter()
            .rev()
            .find(|r| r.has_rows())
            .or(self.results.last())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    ForeignTable,
    Function,
    Sequence,
}

impl ObjectKind {
    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Table => "Tables",
            ObjectKind::View => "Views",
            ObjectKind::MaterializedView => "Materialized views",
            ObjectKind::ForeignTable => "Foreign tables",
            ObjectKind::Function => "Functions",
            ObjectKind::Sequence => "Sequences",
        }
    }

    /// Has rows you can browse.
    pub fn has_data(self) -> bool {
        matches!(
            self,
            ObjectKind::Table
                | ObjectKind::View
                | ObjectKind::MaterializedView
                | ObjectKind::ForeignTable
        )
    }

    pub const ORDER: [ObjectKind; 6] = [
        ObjectKind::Table,
        ObjectKind::View,
        ObjectKind::MaterializedView,
        ObjectKind::ForeignTable,
        ObjectKind::Function,
        ObjectKind::Sequence,
    ];
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DbObject {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    /// The planner's estimate (PostgreSQL's `reltuples`, SQLite's count as
    /// of the last `ANALYZE`), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_estimate: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// For functions: the argument list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Whose syntax the name is quoted in.
    #[serde(skip)]
    pub engine: Engine,
}

impl DbObject {
    /// `schema.name`, each quoted the engine's way.
    pub fn qualified(&self) -> String {
        format!("{}.{}", self.engine.quote_ident(&self.schema), self.engine.quote_ident(&self.name))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    pub primary_key: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexInfo {
    pub name: String,
    pub definition: String,
    pub unique: bool,
    pub primary: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConstraintInfo {
    pub name: String,
    /// p, f, u, c, x
    pub kind: String,
    pub definition: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: ObjectKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_estimate: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_size: Option<String>,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<IndexInfo>,
    pub constraints: Vec<ConstraintInfo>,
    /// CREATE statement (PostgreSQL tables are reconstructed; everything
    /// else is the database's own text).
    pub ddl: String,
}

/// A table or view on the schema map.
#[derive(Debug, Clone, PartialEq)]
pub struct MapTable {
    pub name: String,
    pub kind: ObjectKind,
    pub rows_estimate: Option<i64>,
    pub columns: Vec<MapColumn>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MapColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
}

/// A foreign key: `from_table(from_cols)` references `to_table(to_cols)`.
#[derive(Debug, Clone, PartialEq)]
pub struct ForeignKey {
    pub name: String,
    pub from_table: String,
    pub from_cols: Vec<String>,
    pub to_schema: String,
    pub to_table: String,
    pub to_cols: Vec<String>,
}

/// `"name"`, doubling any quotes inside: PostgreSQL's and SQLite's quoting.
/// See [`Engine::quote_ident`] for the others.
pub fn quote_ident(s: &str) -> String {
    Engine::Postgres.quote_ident(s)
}


/// `'text'`, doubling any quotes inside.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A column type that holds bytes, which Tabula shows as `0x…` hex.
pub(crate) fn is_binary_type(declared: &str) -> bool {
    let t = declared.to_ascii_lowercase();
    t.contains("binary") || t.contains("blob") || t == "image"
}

/// Bytes as `0x…` hex, cut short after `max` bytes with the full length noted.
pub(crate) fn hex(bytes: &[u8], max: usize) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(2 + 2 * bytes.len().min(max) + 16);
    s.push_str("0x");
    for b in &bytes[..bytes.len().min(max)] {
        let _ = write!(s, "{b:02X}");
    }
    if bytes.len() > max {
        let _ = write!(s, "… ({} bytes)", bytes.len());
    }
    s
}

/// The bytes a `0x…` value in a binary column stands for. A value the grid
/// showed cut short can't be read back, and saving it would lose data.
pub(crate) fn parse_hex(value: &str) -> anyhow::Result<Vec<u8>> {
    if value.contains('…') {
        anyhow::bail!("this value was cut short for display, so it can't be saved as it is");
    }
    let digits = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(value);
    if !digits.len().is_multiple_of(2) || !digits.bytes().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("binary values are written as hex, like 0x00FF");
    }
    (0..digits.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).map_err(anyhow::Error::from))
        .collect()
}

/// A column name and its value as text (`None` is NULL).
pub type Cell = (String, Option<String>);

/// One edit to a table's rows, made in the Data tab.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Set columns on the row with this primary key.
    Update { key: Vec<Cell>, set: Vec<Cell> },
    /// A new row; columns left out take their defaults.
    Insert { values: Vec<Cell> },
    /// Delete the row with this primary key.
    Delete { key: Vec<Cell> },
}

impl Change {
    /// Every cell the change names: the key's, then the new values.
    pub(crate) fn cells(&self) -> impl Iterator<Item = &Cell> {
        let (key, values): (&[Cell], &[Cell]) = match self {
            Change::Update { key, set } => (key, set),
            Change::Insert { values } => (&[], values),
            Change::Delete { key } => (key, &[]),
        };
        key.iter().chain(values)
    }
}

/// The table edits apply to: its quoted name and each column's type.
#[derive(Debug, Clone, Default)]
pub struct EditTarget {
    pub engine: Engine,
    pub qualified: String,
    pub types: HashMap<String, String>,
}

impl EditTarget {
    pub fn new(engine: Engine, qualified: String, columns: &[ColumnInfo]) -> EditTarget {
        EditTarget {
            engine,
            qualified,
            types: columns.iter().map(|c| (c.name.clone(), c.data_type.clone())).collect(),
        }
    }

    /// The type `column` was declared with.
    pub(crate) fn declared(&self, column: &str) -> Option<&str> {
        self.types.get(column).map(String::as_str)
    }

    /// The value written by `value` (a literal or a placeholder), as the
    /// column's type. PostgreSQL needs the cast, `value::type`; SQL Server
    /// converts text to anything but bytes on its own; in MySQL and SQLite
    /// the column converts what it is given.
    fn typed(&self, column: &str, value: String) -> String {
        match (self.engine, self.types.get(column)) {
            (Engine::Postgres, Some(t)) => format!("{value}::{t}"),
            // Style 1 reads the `0x…` text the grid shows.
            (Engine::SqlServer, Some(t)) if is_binary_type(t) && value.starts_with('@') => {
                format!("CONVERT(varbinary(max), {value}, 1)")
            }
            _ => value,
        }
    }

    /// The statement for `change`. `value` renders each column's value in
    /// turn: a placeholder when it will be bound, or a literal to show it.
    fn sql(&self, change: &Change, mut value: impl FnMut(&str, &Option<String>) -> String) -> String {
        let mut assign = |cells: &[Cell], sep: &str, null_safe: bool| -> String {
            cells
                .iter()
                .map(|(c, v)| {
                    let name = self.engine.quote_ident(c);
                    if null_safe && v.is_none() {
                        format!("{name} IS NULL")
                    } else {
                        format!("{name} = {}", self.typed(c, value(c, v)))
                    }
                })
                .collect::<Vec<_>>()
                .join(sep)
        };
        match change {
            Change::Update { key, set } => {
                let set_sql = assign(set, ", ", false);
                format!("UPDATE {} SET {set_sql} WHERE {}", self.qualified, assign(key, " AND ", true))
            }
            Change::Delete { key } => {
                format!("DELETE FROM {} WHERE {}", self.qualified, assign(key, " AND ", true))
            }
            // MySQL has no DEFAULT VALUES, but an empty list means the same.
            Change::Insert { values } if values.is_empty() && self.engine == Engine::MySql => {
                format!("INSERT INTO {} () VALUES ()", self.qualified)
            }
            Change::Insert { values } if values.is_empty() => {
                format!("INSERT INTO {} DEFAULT VALUES", self.qualified)
            }
            Change::Insert { values } => {
                let cols: Vec<String> = values.iter().map(|(c, _)| self.engine.quote_ident(c)).collect();
                let vals: Vec<String> = values.iter().map(|(c, v)| self.typed(c, value(c, v))).collect();
                format!("INSERT INTO {} ({}) VALUES ({})", self.qualified, cols.join(", "), vals.join(", "))
            }
        }
    }

    /// A value as a literal in the engine's own syntax.
    fn literal(&self, column: &str, value: &Option<String>) -> String {
        let quoted = || value.as_deref().map_or("NULL".to_string(), quote_literal);
        let binary = self.declared(column).is_some_and(is_binary_type);
        match self.engine {
            Engine::Postgres => quoted(),
            // Hex is already a literal in both; a value cut short for the
            // grid stays quoted text, and saving refuses it.
            Engine::MySql | Engine::SqlServer if binary && value.as_deref().is_some_and(|v| parse_hex(v).is_ok()) => {
                value.clone().unwrap_or_default()
            }
            // MySQL reads a backslash in a string as an escape.
            Engine::MySql => value
                .as_deref()
                .map_or("NULL".to_string(), |v| quote_literal(&v.replace('\\', "\\\\"))),
            // N'…' keeps characters outside the database's code page.
            Engine::SqlServer => value.as_deref().map_or("NULL".to_string(), |v| format!("N{}", quote_literal(v))),
            Engine::Sqlite => {
                let declared = self.declared(column);
                // A blob shown cut short can't be read back; saving refuses
                // it, so here it only has to show what the grid shows.
                sqlite::EditValue::read(declared, value.as_deref()).map_or_else(|_| quoted(), |v| v.literal(declared))
            }
        }
    }

    /// `change` as SQL with the values written in, to show before saving.
    pub fn preview(&self, change: &Change) -> String {
        let sql = self.sql(change, |column, v| self.literal(column, v));
        format!("{sql};")
    }

    /// `change` with placeholders (`$n` for PostgreSQL, `?` for MySQL, `@Pn`
    /// for SQL Server, `?n` for SQLite), and
    /// the cells to bind to them. A NULL in a key becomes `IS NULL` and binds
    /// nothing.
    pub(crate) fn bound(&self, change: &Change) -> (String, Vec<Cell>) {
        let mut params: Vec<Cell> = vec![];
        let key_nulls: usize = match change {
            Change::Update { key, .. } | Change::Delete { key } => key.iter().filter(|(_, v)| v.is_none()).count(),
            Change::Insert { .. } => 0,
        };
        let sql = self.sql(change, |column, v| {
            params.push((column.to_string(), v.clone()));
            match self.engine {
                Engine::Postgres => format!("${}::text", params.len()),
                Engine::MySql => "?".to_string(),
                Engine::SqlServer => format!("@P{}", params.len()),
                Engine::Sqlite => format!("?{}", params.len()),
            }
        });
        if key_nulls == 0 {
            return (sql, params);
        }
        // Rare (a NULL in a key): fall back to literals for this statement.
        (self.preview(change).trim_end_matches(';').to_string(), vec![])
    }
}

/// The error for a write that a read-only session turned down: PostgreSQL's
/// or MySQL's read-only transaction, SQLite's read-only connection, or a
/// statement Tabula's own check found writing (SQL Server).
#[derive(Debug)]
pub struct WriteRefused(pub String);

impl std::fmt::Display for WriteRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for WriteRefused {}

/// Whether `e` is a write that failed only for want of write access.
pub fn needs_write_access(e: &anyhow::Error) -> bool {
    e.is::<WriteRefused>()
}

enum Backend {
    // Boxed: their clients make them many times the size of the others.
    Postgres(Box<postgres::Backend>),
    MySql(mysql::Backend),
    SqlServer(Box<mssql::Backend>),
    Sqlite(sqlite::Backend),
}

/// Stops the query running on a session's work connection.
pub enum Canceller {
    Postgres(tokio_postgres::CancelToken),
    MySql(mysql::Stop),
    SqlServer(mssql::Stop),
    Sqlite(sqlite::Stop),
}

impl Canceller {
    pub async fn cancel(self) -> anyhow::Result<()> {
        match self {
            Canceller::Postgres(token) => token.cancel_query(tokio_postgres::NoTls).await?,
            Canceller::MySql(stop) => stop.stop().await?,
            Canceller::SqlServer(stop) => stop.stop(),
            Canceller::Sqlite(stop) => stop.stop(),
        }
        Ok(())
    }
}

/// An open connection to one database. Each backend keeps the catalog and
/// the user's queries on separate connections, so a long query never blocks
/// the object tree.
pub struct Session {
    pub profile: Profile,
    password: Option<String>,
    backend: Backend,
    /// The server's version, or the version of the built-in SQLite.
    pub server_version: String,
}

/// Call the same method on whichever backend the session has.
macro_rules! backend {
    ($session:expr, $b:ident => $call:expr) => {
        match &$session.backend {
            Backend::Postgres($b) => $call,
            Backend::MySql($b) => $call,
            Backend::SqlServer($b) => $call,
            Backend::Sqlite($b) => $call,
        }
    };
}

impl Session {
    /// Connect. A MySQL or SQL Server profile that names no database gets the
    /// one the server picks for the login, and the session's profile says
    /// which.
    pub async fn connect(mut profile: Profile, password: Option<String>) -> anyhow::Result<Arc<Session>> {
        let (backend, server_version) = match profile.engine {
            Engine::Postgres => {
                let (b, v) = postgres::Backend::connect(&profile, password.as_deref()).await?;
                (Backend::Postgres(Box::new(b)), v)
            }
            Engine::MySql => {
                let (b, v) = mysql::Backend::connect(&mut profile, password.as_deref()).await?;
                (Backend::MySql(b), v)
            }
            Engine::SqlServer => {
                let (b, v) = mssql::Backend::connect(&mut profile, password.as_deref()).await?;
                (Backend::SqlServer(Box::new(b)), v)
            }
            Engine::Sqlite => {
                let (b, v) = sqlite::Backend::connect(&profile.database).await?;
                (Backend::Sqlite(b), v)
            }
        };
        Ok(Arc::new(Session {
            profile,
            password,
            backend,
            server_version,
        }))
    }

    /// The same server, another database.
    pub async fn reconnect_to(&self, database: &str) -> anyhow::Result<Arc<Session>> {
        Session::connect(self.profile.with_database(database), self.password.clone()).await
    }

    /// Something that stops the query running on the work connection.
    pub fn canceller(&self) -> Canceller {
        match &self.backend {
            Backend::Postgres(b) => Canceller::Postgres(b.cancel_token()),
            Backend::MySql(b) => Canceller::MySql(b.stop_handle()),
            Backend::SqlServer(b) => Canceller::SqlServer(b.stop_handle()),
            Backend::Sqlite(b) => Canceller::Sqlite(b.stop_handle()),
        }
    }

    /// The databases to offer next to this one. A SQLite file is the only one.
    pub async fn databases(&self) -> anyhow::Result<Vec<String>> {
        match &self.backend {
            Backend::Postgres(b) => b.databases().await,
            Backend::MySql(b) => b.databases().await,
            Backend::SqlServer(b) => b.databases().await,
            Backend::Sqlite(_) => Ok(vec![self.profile.database.clone()]),
        }
    }

    /// User schemas first, then the system ones. SQLite has `main`; a MySQL
    /// database is its own only schema.
    pub async fn schemas(&self) -> anyhow::Result<Vec<String>> {
        backend!(self, b => b.schemas().await)
    }

    pub async fn objects(&self, schema: &str) -> anyhow::Result<Vec<DbObject>> {
        backend!(self, b => b.objects(schema).await)
    }

    /// Every table and view in a schema with its columns, and the foreign
    /// keys between them: what the schema map draws.
    pub async fn schema_map(&self, schema: &str) -> anyhow::Result<(Vec<MapTable>, Vec<ForeignKey>)> {
        backend!(self, b => b.schema_map(schema).await)
    }

    /// The names SQL completion suggests: every table and view in the
    /// user's own schemas, with their columns.
    pub async fn completion_catalog(&self) -> anyhow::Result<crate::complete::Catalog> {
        let schemas: Vec<String> = self
            .schemas()
            .await?
            .into_iter()
            .filter(|s| !is_system_schema(s))
            .take(40)
            .collect();
        let mut tables = vec![];
        for schema in &schemas {
            let (found, _) = self.schema_map(schema).await?;
            tables.extend(found.into_iter().map(|t| crate::complete::Table {
                schema: schema.clone(),
                name: t.name,
                view: !matches!(t.kind, ObjectKind::Table | ObjectKind::ForeignTable),
                columns: t.columns.into_iter().map(|c| (c.name, c.data_type)).collect(),
            }));
        }
        Ok(crate::complete::Catalog { schemas, tables, engine: self.profile.engine })
    }

    /// Columns, indexes, constraints and DDL for a table or view.
    pub async fn table_info(&self, schema: &str, name: &str) -> anyhow::Result<TableInfo> {
        backend!(self, b => b.table_info(schema, name).await)
    }

    /// The `CREATE FUNCTION` (or procedure) statement for a routine. SQLite's
    /// catalog has no functions to list.
    pub async fn function_ddl(&self, schema: &str, name: &str, args: &str) -> anyhow::Result<String> {
        match &self.backend {
            Backend::Postgres(b) => b.function_ddl(schema, name, args).await,
            Backend::MySql(b) => b.function_ddl(schema, name).await,
            Backend::SqlServer(b) => b.function_ddl(schema, name).await,
            Backend::Sqlite(_) => anyhow::bail!("SQLite has no stored functions"),
        }
    }

    /// Apply row edits in one transaction: all of them, or none. An update
    /// or delete that doesn't hit exactly one row stops and rolls back. Runs
    /// off the work connection, so it never joins a transaction the user
    /// opened in a query tab. Returns how many rows changed.
    pub async fn apply(&self, target: &EditTarget, changes: &[Change]) -> anyhow::Result<u64> {
        backend!(self, b => b.apply(target, changes).await)
    }

    /// Run `sql` (any number of statements) on the work connection, keeping
    /// at most `max_rows` rows per statement.
    pub async fn run(&self, sql: &str, max_rows: usize) -> anyhow::Result<QueryOutcome> {
        backend!(self, b => b.run(sql, max_rows).await)
    }

    /// Run one statement read-only with a timeout. For AI agents: a write
    /// fails (see [`needs_write_access`]), and so does anything that is not
    /// exactly one statement. PostgreSQL and MySQL enforce it with a
    /// read-only transaction, SQLite with a connection opened read-only, and
    /// SQL Server (which has no such thing) with Tabula's own check of the
    /// statement inside a transaction that is always rolled back.
    pub async fn run_read_only(
        &self,
        sql: &str,
        max_rows: usize,
        timeout: Duration,
    ) -> anyhow::Result<QueryOutcome> {
        backend!(self, b => b.run_read_only(sql, max_rows, timeout).await)
    }

    /// Run `sql` with a timeout, off the work connection, for AI agents with
    /// write access.
    pub async fn run_write(
        &self,
        sql: &str,
        max_rows: usize,
        timeout: Duration,
    ) -> anyhow::Result<QueryOutcome> {
        backend!(self, b => b.run_write(sql, max_rows, timeout).await)
    }

    /// The plan for `sql` as text, run read-only. With `analyze`, PostgreSQL
    /// runs the statement and reports actual timings.
    pub async fn plan(&self, sql: &str, analyze: bool, timeout: Duration) -> anyhow::Result<String> {
        backend!(self, b => b.plan(sql, analyze, timeout).await)
    }

    /// The statement that shows the plan for `sql` as rows.
    pub fn explain_sql(&self, sql: &str) -> String {
        match self.profile.engine {
            Engine::Postgres | Engine::MySql => format!("EXPLAIN {sql}"),
            // SHOWPLAN has to be alone in its batch; `run` splits at GO.
            Engine::SqlServer => format!("SET SHOWPLAN_TEXT ON\nGO\n{sql}\nGO\nSET SHOWPLAN_TEXT OFF"),
            Engine::Sqlite => format!("EXPLAIN QUERY PLAN {sql}"),
        }
    }
}

/// A schema of the database's own, listed after the user's.
pub fn is_system_schema(schema: &str) -> bool {
    matches!(schema, "pg_catalog" | "information_schema" | "INFORMATION_SCHEMA" | "sys")
}

/// Rows of `object`, a page at a time, for the Data tab.
pub fn browse_sql(
    object: &DbObject,
    filter: &str,
    order: Option<(&str, bool)>,
    // The primary key: the order when none is chosen, and the tie-breaker
    // when one is, so pages don't shuffle.
    key: &[String],
    limit: usize,
    offset: usize,
) -> String {
    let quote = |name: &str| object.engine.quote_ident(name);
    let mut sql = format!("SELECT * FROM {}", object.qualified());
    if !filter.trim().is_empty() {
        sql.push_str(&format!(" WHERE {}", filter.trim()));
    }
    let mut by: Vec<String> = order
        .iter()
        .map(|(col, asc)| format!("{} {}", quote(col), if *asc { "ASC" } else { "DESC" }))
        .collect();
    by.extend(key.iter().filter(|k| order.is_none_or(|(c, _)| c != k.as_str())).map(|k| quote(k)));
    if object.engine == Engine::SqlServer {
        // OFFSET … FETCH needs an ORDER BY; with no key, any order will do.
        if by.is_empty() {
            by.push("(SELECT NULL)".into());
        }
        sql.push_str(&format!(" ORDER BY {} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY", by.join(", ")));
        return sql;
    }
    if !by.is_empty() {
        sql.push_str(&format!(" ORDER BY {}", by.join(", ")));
    }
    sql.push_str(&format!(" LIMIT {limit} OFFSET {offset}"));
    sql
}

/// Results as tab-separated text (header line first), for the clipboard.
pub fn to_tsv(columns: &[ResultColumn], rows: &[Vec<Option<String>>]) -> String {
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    let mut out = columns
        .iter()
        .map(|c| clean(&c.name))
        .collect::<Vec<_>>()
        .join("\t");
    for r in rows {
        out.push('\n');
        out.push_str(
            &r.iter()
                .map(|v| v.as_deref().map(clean).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    out
}

/// Results as RFC 4180 CSV.
pub fn to_csv(columns: &[ResultColumn], rows: &[Vec<Option<String>>]) -> String {
    let field = |s: &str| {
        if s.contains([',', '"', '\n', '\r']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    };
    let mut out = columns
        .iter()
        .map(|c| field(&c.name))
        .collect::<Vec<_>>()
        .join(",");
    out.push_str("\r\n");
    for r in rows {
        out.push_str(
            &r.iter()
                .map(|v| v.as_deref().map(field).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn browse_sql_quotes_and_pages() {
        let o = DbObject {
            schema: "shop".into(),
            name: "Order Items".into(),
            kind: ObjectKind::Table,
            rows_estimate: None,
            comment: None,
            signature: None,
            engine: Engine::Postgres,
        };
        assert_eq!(
            browse_sql(&o, " quantity > 1 ", Some(("order_id", false)), &[], 100, 200),
            "SELECT * FROM \"shop\".\"Order Items\" WHERE quantity > 1 ORDER BY \"order_id\" DESC LIMIT 100 OFFSET 200"
        );
        let key = ["order_id".to_string(), "product_id".to_string()];
        assert_eq!(
            browse_sql(&o, "", None, &key, 500, 0),
            "SELECT * FROM \"shop\".\"Order Items\" ORDER BY \"order_id\", \"product_id\" LIMIT 500 OFFSET 0"
        );
        assert_eq!(
            browse_sql(&o, "", Some(("order_id", true)), &key, 500, 0),
            "SELECT * FROM \"shop\".\"Order Items\" ORDER BY \"order_id\" ASC, \"product_id\" LIMIT 500 OFFSET 0"
        );
    }

    fn target() -> EditTarget {
        EditTarget {
            engine: Engine::Postgres,
            qualified: "\"shop\".\"products\"".into(),
            types: [("id", "integer"), ("title", "text"), ("price", "numeric(10,2)")]
                .into_iter()
                .map(|(c, t)| (c.to_string(), t.to_string()))
                .collect(),
        }
    }

    #[test]
    fn changes_become_sql() {
        let t = target();
        let key = vec![("id".to_string(), Some("7".to_string()))];
        let update = Change::Update {
            key: key.clone(),
            set: vec![("title".into(), Some("It's new".into())), ("price".into(), None)],
        };
        assert_eq!(
            t.preview(&update),
            "UPDATE \"shop\".\"products\" SET \"title\" = 'It''s new'::text, \"price\" = NULL::numeric(10,2) WHERE \"id\" = '7'::integer;"
        );
        let (sql, params) = t.bound(&update);
        assert_eq!(
            sql,
            "UPDATE \"shop\".\"products\" SET \"title\" = $1::text::text, \"price\" = $2::text::numeric(10,2) WHERE \"id\" = $3::text::integer"
        );
        let values: Vec<Option<&str>> = params.iter().map(|(_, v)| v.as_deref()).collect();
        assert_eq!(values, vec![Some("It's new"), None, Some("7")]);
        assert_eq!(params[2].0, "id");

        assert_eq!(t.preview(&Change::Delete { key }), "DELETE FROM \"shop\".\"products\" WHERE \"id\" = '7'::integer;");
        assert_eq!(t.preview(&Change::Insert { values: vec![] }), "INSERT INTO \"shop\".\"products\" DEFAULT VALUES;");
        assert_eq!(
            t.preview(&Change::Insert { values: vec![("title".into(), Some("Mug".into()))] }),
            "INSERT INTO \"shop\".\"products\" (\"title\") VALUES ('Mug'::text);"
        );
        assert_eq!(quote_literal("a'b"), "'a''b'");
    }

    #[test]
    fn changes_become_sqlite_sql() {
        let t = EditTarget {
            engine: Engine::Sqlite,
            qualified: "\"main\".\"products\"".into(),
            types: [("id", "INTEGER"), ("title", "TEXT"), ("price", "NUMERIC"), ("photo", "BLOB")]
                .into_iter()
                .map(|(c, t)| (c.to_string(), t.to_string()))
                .collect(),
        };
        let cell = |c: &str, v: Option<&str>| (c.to_string(), v.map(str::to_string));
        let update = Change::Update {
            key: vec![cell("id", Some("7"))],
            set: vec![cell("title", Some("It's new")), cell("price", None), cell("photo", Some("x'00FF'"))],
        };
        // No casts: SQLite has none to give, and a column's affinity does the converting.
        assert_eq!(
            t.preview(&update),
            "UPDATE \"main\".\"products\" SET \"title\" = 'It''s new', \"price\" = NULL, \"photo\" = x'00FF' WHERE \"id\" = 7;"
        );
        let (sql, params) = t.bound(&update);
        assert_eq!(
            sql,
            "UPDATE \"main\".\"products\" SET \"title\" = ?1, \"price\" = ?2, \"photo\" = ?3 WHERE \"id\" = ?4"
        );
        assert_eq!(params.len(), 4);
        assert_eq!(
            t.preview(&Change::Insert { values: vec![cell("title", Some("Mug")), cell("price", Some("4.50"))] }),
            "INSERT INTO \"main\".\"products\" (\"title\", \"price\") VALUES ('Mug', 4.50);"
        );
        let (sql, params) = t.bound(&Change::Delete { key: vec![cell("id", None)] });
        assert_eq!(sql, "DELETE FROM \"main\".\"products\" WHERE \"id\" IS NULL");
        assert!(params.is_empty());
    }

    #[test]
    fn csv_and_tsv() {
        let cols = vec![
            ResultColumn { name: "a".into(), type_name: None, numeric: false },
            ResultColumn { name: "b".into(), type_name: None, numeric: false },
        ];
        let rows = vec![vec![Some("x,y".into()), None], vec![Some("q\"".into()), Some("t\tu".into())]];
        assert_eq!(to_csv(&cols, &rows), "a,b\r\n\"x,y\",\r\n\"q\"\"\",t\tu\r\n");
        assert_eq!(to_tsv(&cols, &rows), "a\tb\nx,y\t\nq\"\tt u");
    }

    /// Needs a server: `TABULA_TEST_URL=postgres://postgres:tabula@localhost/postgres`,
    /// seeded with `scripts/seed.sql`.
    /// Or `TABULA_TEST_CONNECTION=name`, a saved connection (its password
    /// comes from the keyring).
    fn test_session() -> Option<Arc<Session>> {
        if let Ok(name) = std::env::var("TABULA_TEST_CONNECTION") {
            let p = crate::profiles::load().into_iter().find(|p| p.name == name).expect("saved connection");
            let pw = crate::profiles::password(&p.name);
            return Some(crate::rt::block_on(Session::connect(p, pw)).expect("connect"));
        }
        let url = std::env::var("TABULA_TEST_URL").ok()?;
        let (p, pw) = Profile::from_url("test", &url).unwrap();
        let p = Profile { ssl: crate::profiles::SslMode::Disable, ..p };
        Some(crate::rt::block_on(Session::connect(p, pw)).expect("connect"))
    }

    #[test]
    fn catalog_and_queries_against_a_server() {
        let Some(s) = test_session() else {
            eprintln!("TABULA_TEST_URL not set; skipped");
            return;
        };
        crate::rt::block_on(async {
            assert!(s.schemas().await.unwrap().contains(&"shop".to_string()));
            let objs = s.objects("shop").await.unwrap();
            let names: Vec<_> = objs.iter().map(|o| (o.name.as_str(), o.kind)).collect();
            assert!(names.contains(&("orders", ObjectKind::Table)));
            assert!(names.contains(&("order_totals", ObjectKind::View)));
            assert!(names.contains(&("customer_order_count", ObjectKind::Function)));

            let info = s.table_info("shop", "orders").await.unwrap();
            assert!(info.columns.iter().any(|c| c.name == "id" && c.primary_key));
            assert!(info.indexes.iter().any(|i| i.name == "orders_customer_idx"));
            assert!(info.ddl.starts_with("CREATE TABLE \"shop\".\"orders\""));

            let out = s.run("select 1 as a, null::text as b; select id from shop.orders", 10).await.unwrap();
            assert_eq!(out.results.len(), 2);
            assert_eq!(out.results[0].rows, vec![vec![Some("1".into()), None]]);
            assert!(out.results[1].truncated);
            assert_eq!(out.results[1].rows.len(), 10);
            assert_eq!(out.results[1].row_count, 100_000);

            let typed = s.run("select 1::int4 as n", 10).await.unwrap();
            assert!(typed.results[0].columns[0].is_numeric());

            let t = Duration::from_secs(5);
            assert!(s.run_read_only("select count(*) from shop.orders", 10, t).await.is_ok());
            let err = s.run_read_only("delete from shop.orders", 10, t).await.unwrap_err();
            assert!(err.to_string().contains("read-only"), "{err}");
            let err = s
                .run_read_only("select 1; commit; delete from shop.orders", 10, t)
                .await
                .unwrap_err();
            assert!(err.to_string().contains("one statement"), "{err}");
            assert!(s.run_read_only("commit", 10, t).await.is_err());
            // Still there.
            let n = s.run("select count(*) from shop.orders", 1).await.unwrap();
            assert_eq!(n.results[0].rows[0][0].as_deref(), Some("100000"));

            let err = s
                .run_read_only("with d as (delete from shop.orders returning 1) select * from d", 10, t)
                .await
                .unwrap_err();
            assert!(needs_write_access(&err), "{err}");

            let err = s.run("select * from nope", 10).await.unwrap_err();
            assert!(err.to_string().contains("42P01"), "{err}");

            let catalog = s.completion_catalog().await.unwrap();
            assert!(catalog.schemas.contains(&"shop".to_string()) && !catalog.schemas.contains(&"pg_catalog".to_string()));
            let (_, found) = crate::complete::suggest(&catalog, "select o.cu from shop.orders o", 11, None);
            assert_eq!(found[0].insert, "customer_id");

            let (tables, keys) = s.schema_map("shop").await.unwrap();
            let orders = tables.iter().find(|t| t.name == "orders").unwrap();
            assert!(orders.columns.iter().any(|c| c.name == "id" && c.primary_key && !c.nullable));
            assert!(tables.iter().any(|t| t.name == "order_totals" && t.kind == ObjectKind::View));
            let fk = |from: &str, to: &str| keys.iter().find(|k| k.from_table == from && k.to_table == to).unwrap();
            assert_eq!(fk("orders", "customers").from_cols, vec!["customer_id"]);
            assert_eq!(fk("orders", "customers").to_cols, vec!["id"]);
            assert_eq!(fk("order_items", "orders").to_schema, "shop");
            assert_eq!(fk("order_items", "products").from_cols, vec!["product_id"]);

            // Row edits, on a table only this connection sees.
            // (`run_write` and `apply` share the connection the temp table lives on.)
            let wait = Duration::from_secs(5);
            s.run_write("create temp table tabula_edit (id serial primary key, title text not null, price numeric(10,2), tags text[])", 1, wait)
                .await
                .unwrap();
            let t = EditTarget {
                engine: Engine::Postgres,
                qualified: "tabula_edit".into(),
                types: [("id", "integer"), ("title", "text"), ("price", "numeric(10,2)"), ("tags", "text[]")]
                    .into_iter()
                    .map(|(c, t)| (c.to_string(), t.to_string()))
                    .collect(),
            };
            let cell = |c: &str, v: Option<&str>| (c.to_string(), v.map(str::to_string));
            let n = s
                .apply(&t, &[
                    Change::Insert { values: vec![cell("title", Some("Mug")), cell("price", Some("4.50")), cell("tags", Some("{a,b}"))] },
                    Change::Insert { values: vec![cell("title", Some("Cup"))] },
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", Some("It's a mug")), cell("price", None)] },
                    Change::Delete { key: vec![cell("id", Some("2"))] },
                ])
                .await
                .unwrap();
            assert_eq!(n, 4);
            let rows = s.run_write("select id, title, price, tags from tabula_edit", 10, wait).await.unwrap();
            assert_eq!(
                rows.results[0].rows,
                vec![vec![Some("1".into()), Some("It's a mug".into()), None, Some("{a,b}".into())]]
            );

            // One bad change undoes the good ones before it.
            let err = s
                .apply(&t, &[
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", Some("changed"))] },
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("price", Some("not a number"))] },
                ])
                .await
                .unwrap_err();
            assert!(err.to_string().contains("nothing was saved"), "{err}");
            let err = s
                .apply(&t, &[
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", Some("changed"))] },
                    Change::Delete { key: vec![cell("id", Some("99"))] },
                ])
                .await
                .unwrap_err();
            assert!(err.to_string().contains("matched 0 rows"), "{err}");
            let row = s.run_write("select title from tabula_edit", 10, wait).await.unwrap();
            assert_eq!(row.results[0].rows, vec![vec![Some("It's a mug".into())]]);
        });
    }

    /// A session from a URL in `var`, or None to skip. The servers are
    /// seeded with `scripts/seed-mysql.sql` / `scripts/seed-sqlserver.sql`.
    fn server_session(var: &str) -> Option<Arc<Session>> {
        let url = std::env::var(var).ok()?;
        let (p, pw) = Profile::from_url("test", &url).unwrap();
        Some(crate::rt::block_on(Session::connect(p, pw)).expect("connect"))
    }

    fn cell(c: &str, v: Option<&str>) -> Cell {
        (c.to_string(), v.map(str::to_string))
    }

    /// Needs `TABULA_TEST_MYSQL_URL=mysql://root:tabula@localhost/shop`.
    #[test]
    fn mysql_against_a_server() {
        let Some(s) = server_session("TABULA_TEST_MYSQL_URL") else {
            eprintln!("TABULA_TEST_MYSQL_URL not set; skipped");
            return;
        };
        crate::rt::block_on(async {
            assert_eq!(s.schemas().await.unwrap(), vec!["shop"]);
            let dbs = s.databases().await.unwrap();
            assert_eq!(dbs.first().map(String::as_str), Some("shop"), "{dbs:?}");
            assert!(dbs.last().is_some_and(|d| d == "sys"), "{dbs:?}");

            let objs = s.objects("shop").await.unwrap();
            let names: Vec<_> = objs.iter().map(|o| (o.name.as_str(), o.kind)).collect();
            assert!(names.contains(&("orders", ObjectKind::Table)));
            assert!(names.contains(&("order_totals", ObjectKind::View)));
            let f = objs.iter().find(|o| o.name == "customer_order_count").unwrap();
            assert_eq!((f.kind, f.signature.as_deref()), (ObjectKind::Function, Some("customer int")));
            assert!(objs.iter().find(|o| o.name == "orders").unwrap().rows_estimate.is_some_and(|n| n > 50_000));
            assert_eq!(objs[0].qualified(), "`shop`.`customers`");
            assert!(s.function_ddl("shop", "customer_order_count", "").await.unwrap().contains("CREATE DEFINER"));

            let info = s.table_info("shop", "orders").await.unwrap();
            let col = |n: &str| info.columns.iter().find(|c| c.name == n).unwrap();
            assert!(col("id").primary_key && col("id").data_type == "bigint" && !col("id").nullable);
            assert_eq!(col("id").default.as_deref(), Some("auto_increment"));
            assert_eq!(col("status").default.as_deref(), Some("'pending'"));
            assert_eq!(col("placed_at").default.as_deref(), Some("CURRENT_TIMESTAMP"));
            assert!(info.indexes[0].primary && info.indexes[0].definition == "PRIMARY KEY (`id`)");
            let idx = info.indexes.iter().find(|i| i.name == "orders_placed_idx").unwrap();
            assert_eq!(idx.definition, "CREATE INDEX `orders_placed_idx` ON `shop`.`orders` (`placed_at` DESC)");
            let defs: Vec<_> = info.constraints.iter().map(|k| (k.kind.as_str(), k.definition.as_str())).collect();
            assert!(defs.contains(&("p", "PRIMARY KEY (`id`)")), "{defs:?}");
            assert!(defs.contains(&("f", "FOREIGN KEY (`customer_id`) REFERENCES `shop`.`customers` (`id`)")), "{defs:?}");
            assert!(info.ddl.starts_with("CREATE TABLE `orders`"), "{}", info.ddl);
            assert!(info.total_size.as_deref().is_some_and(|t| t.ends_with("MB")), "{:?}", info.total_size);
            let items = s.table_info("shop", "order_items").await.unwrap();
            assert!(items.constraints.iter().any(|k| k.definition.ends_with("ON DELETE CASCADE")), "{:?}", items.constraints);
            let products = s.table_info("shop", "products").await.unwrap();
            assert!(products.constraints.iter().any(|k| k.kind == "c" && k.definition.contains("price")), "{:?}", products.constraints);
            let view = s.table_info("shop", "order_totals").await.unwrap();
            assert!(view.kind == ObjectKind::View && view.ddl.contains("VIEW `order_totals`"), "{}", view.ddl);
            assert!(s.table_info("shop", "nope").await.is_err());

            // Several statements, NULLs, numbers, bytes and bits.
            let out = s
                .run("select 1 as a, null as b, 2.50 as c; select id from orders; select x'00ff' as raw, b'101' as bits, 'naïve' as t", 10)
                .await
                .unwrap();
            assert_eq!(out.results.len(), 3);
            assert_eq!(out.results[0].rows, vec![vec![Some("1".into()), None, Some("2.50".into())]]);
            assert!(out.results[0].columns[0].is_numeric() && out.results[0].columns[2].is_numeric());
            assert!(out.results[1].truncated && out.results[1].rows.len() == 10 && out.results[1].row_count == 100_000);
            assert_eq!(out.results[2].rows[0], vec![Some("0x00FF".into()), Some("0x05".into()), Some("naïve".into())]);
            let photo = s.run("select photo from products where id = 50", 1).await.unwrap();
            assert!(photo.results[0].rows[0][0].as_deref().is_some_and(|p| p.starts_with("0x") && p.len() == 66));

            // Writes report what they changed; DDL changes nothing.
            let out = s
                .run("create temporary table t (x int); insert into t values (1), (2), (3); update t set x = 0 where x > 1; drop temporary table t", 10)
                .await
                .unwrap();
            let counts: Vec<u64> = out.results.iter().map(|r| r.row_count).collect();
            assert_eq!(counts, vec![0, 3, 2, 0]);

            // The Data tab's pages, and a transaction that spans runs.
            let orders = DbObject { schema: "shop".into(), name: "orders".into(), kind: ObjectKind::Table, rows_estimate: None, comment: None, signature: None, engine: Engine::MySql };
            let page = s.run(&browse_sql(&orders, "status = 'paid'", Some(("id", false)), &[], 3, 3), 3).await.unwrap();
            let ids: Vec<_> = page.results[0].rows.iter().map(|r| r[0].clone().unwrap()).collect();
            assert_eq!(ids, vec!["99985", "99981", "99977"]);
            s.run("begin; update orders set note = 'x' where id = 1", 1).await.unwrap();
            s.run("rollback", 1).await.unwrap();
            assert_eq!(s.run("select note from orders where id = 1", 1).await.unwrap().results[0].rows, vec![vec![None]]);

            let err = s.run("select * from nope", 10).await.unwrap_err();
            assert!(err.to_string().starts_with("ERROR 1146 (42S02)"), "{err}");

            // Agents: reads only, one statement, and the server refuses writes.
            let t = Duration::from_secs(5);
            assert!(s.run_read_only("select count(*) from orders", 10, t).await.is_ok());
            for sql in ["delete from orders", "create table x (y int)", "with d as (select 1) update orders set note = 'x'", "select 1 into outfile '/tmp/x'"] {
                let err = s.run_read_only(sql, 10, t).await.unwrap_err();
                assert!(needs_write_access(&err), "{sql}: {err}");
            }
            let err = s.run_read_only("select 1; delete from orders", 10, t).await.unwrap_err();
            assert!(err.to_string().contains("one statement"), "{err}");
            // A function that writes, called from a SELECT, meets the read-only transaction.
            s.run("create function tabula_writes() returns int deterministic modifies sql data begin update orders set note = 'w' where id = 1; return 1; end", 1).await.unwrap();
            let err = s.run_read_only("select tabula_writes()", 10, t).await.unwrap_err();
            assert!(needs_write_access(&err), "{err}");
            s.run("drop function tabula_writes", 1).await.unwrap();
            assert_eq!(s.run("select count(*) from orders where note = 'w'", 1).await.unwrap().results[0].rows[0][0].as_deref(), Some("0"));

            let out = s.run_write("update orders set note = 'agent' where id <= 3", 10, t).await.unwrap();
            assert_eq!(out.results[0].row_count, 3);
            let err = s.run_write("begin", 10, t).await.unwrap_err();
            assert!(err.to_string().contains("rolled back"), "{err}");
            s.run("update orders set note = null where id <= 3", 1).await.unwrap();

            let started = std::time::Instant::now();
            let err = s.run_read_only("select sleep(5)", 10, Duration::from_millis(300)).await.unwrap_err();
            assert!(err.to_string().contains("timed out"), "{err}");
            assert!(started.elapsed() < Duration::from_secs(3));
            assert!(s.run_read_only("select 1", 10, t).await.is_ok());

            let stop = s.canceller();
            let running = tokio::spawn({
                let s = s.clone();
                async move { s.run("select sleep(10)", 10).await }
            });
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop.cancel().await.unwrap();
            // SLEEP returns 1 when interrupted rather than failing.
            let out = running.await.unwrap();
            assert!(out.as_ref().is_err_and(|e| e.to_string() == "the query was stopped") || out.as_ref().is_ok_and(|o| o.results[0].rows[0][0].as_deref() == Some("1")), "{out:?}");
            assert!(s.run("select 1", 1).await.is_ok());

            let plan = s.plan("select * from orders where customer_id = 7", false, t).await.unwrap();
            assert!(plan.contains("orders_customer_idx"), "{plan}");
            let plan = s.plan("select * from orders where customer_id = 7", true, t).await.unwrap();
            assert!(plan.contains("actual time"), "{plan}");
            let explain = s.run(&s.explain_sql("select * from orders where id = 1"), 10).await.unwrap();
            assert!(explain.results[0].has_rows());

            let (tables, keys) = s.schema_map("shop").await.unwrap();
            assert!(tables.iter().find(|t| t.name == "orders").unwrap().columns.iter().any(|c| c.name == "id" && c.primary_key));
            let fk = keys.iter().find(|k| k.from_table == "order_items" && k.to_table == "orders").unwrap();
            assert_eq!((fk.from_cols.clone(), fk.to_schema.as_str()), (vec!["order_id".to_string()], "shop"));
            let catalog = s.completion_catalog().await.unwrap();
            let (_, found) = crate::complete::suggest(&catalog, "select o.cu from orders o", 11, None);
            assert_eq!(found[0].insert, "customer_id");

            // Row edits, on a table made for them.
            s.run("drop table if exists tabula_edit; create table tabula_edit (id int auto_increment primary key, title varchar(50) not null, price decimal(10,2), raw varbinary(8), note text)", 1).await.unwrap();
            let info = s.table_info("shop", "tabula_edit").await.unwrap();
            let target = EditTarget::new(Engine::MySql, "`shop`.`tabula_edit`".into(), &info.columns);
            let n = s
                .apply(&target, &[
                    Change::Insert { values: vec![cell("title", Some("Mug")), cell("price", Some("4.50")), cell("raw", Some("0x00FF"))] },
                    Change::Insert { values: vec![] },
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", Some("It's a \\ mug")), cell("price", None)] },
                    // Unchanged still counts as the one row it matched.
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("note", None)] },
                ])
                .await;
            // The empty insert has no title, which is NOT NULL: nothing is saved.
            let err = n.unwrap_err();
            assert!(err.to_string().contains("change 2 of 4") && err.to_string().contains("nothing was saved"), "{err}");
            assert_eq!(s.run("select count(*) from tabula_edit", 1).await.unwrap().results[0].rows[0][0].as_deref(), Some("0"));
            let n = s
                .apply(&target, &[
                    Change::Insert { values: vec![cell("title", Some("Mug")), cell("price", Some("4.50")), cell("raw", Some("0x00FF"))] },
                    Change::Update { key: vec![cell("id", Some("2"))], set: vec![cell("title", Some("It's a \\ mug")), cell("price", None)] },
                    Change::Update { key: vec![cell("id", Some("2"))], set: vec![cell("note", None)] },
                ])
                .await
                .unwrap();
            assert_eq!(n, 3);
            s.run("alter table tabula_edit add bits bit(3) not null default b'101'", 1).await.unwrap();
            let row = s.run("select bits from tabula_edit", 1).await.unwrap();
            assert_eq!(row.results[0].rows[0][0].as_deref(), Some("5"));
            let row = s.run("select title, price, raw from tabula_edit", 1).await.unwrap();
            assert_eq!(row.results[0].rows[0], vec![Some("It's a \\ mug".into()), None, Some("0x00FF".into())]);
            assert_eq!(
                target.preview(&Change::Update { key: vec![cell("id", Some("2"))], set: vec![cell("title", Some("a\\b")), cell("raw", Some("0x01"))] }),
                "UPDATE `shop`.`tabula_edit` SET `title` = 'a\\\\b', `raw` = 0x01 WHERE `id` = '2';"
            );
            let err = s.apply(&target, &[Change::Delete { key: vec![cell("id", Some("99"))] }]).await.unwrap_err();
            assert!(err.to_string().contains("matched 0 rows"), "{err}");
            s.run("drop table tabula_edit", 1).await.unwrap();

            // Another database on the same server.
            let other = s.reconnect_to("mysql").await.unwrap();
            assert_eq!(other.schemas().await.unwrap(), vec!["mysql"]);
        });
    }

    /// Needs `TABULA_TEST_SQLSERVER_URL=sqlserver://sa:<password>@localhost/shop`.
    #[test]
    fn sql_server_against_a_server() {
        let Some(s) = server_session("TABULA_TEST_SQLSERVER_URL") else {
            eprintln!("TABULA_TEST_SQLSERVER_URL not set; skipped");
            return;
        };
        crate::rt::block_on(async {
            let schemas = s.schemas().await.unwrap();
            assert_eq!(&schemas[..2], ["dbo", "shop"], "{schemas:?}");
            assert_eq!(schemas[schemas.len() - 2..], ["INFORMATION_SCHEMA", "sys"], "{schemas:?}");
            assert!(s.databases().await.unwrap().contains(&"master".to_string()));

            let objs = s.objects("shop").await.unwrap();
            let names: Vec<_> = objs.iter().map(|o| (o.name.as_str(), o.kind)).collect();
            assert!(names.contains(&("orders", ObjectKind::Table)));
            assert!(names.contains(&("order_totals", ObjectKind::View)));
            assert!(names.contains(&("invoice_numbers", ObjectKind::Sequence)));
            let f = objs.iter().find(|o| o.name == "customer_order_count").unwrap();
            assert_eq!(f.signature.as_deref(), Some("@customer int"));
            assert_eq!(objs.iter().find(|o| o.name == "orders").unwrap().rows_estimate, Some(100_000));
            assert_eq!(objs.iter().find(|o| o.name == "customers").unwrap().comment.as_deref(), Some("People who have an account"));
            assert!(s.function_ddl("shop", "customer_order_count", "").await.unwrap().starts_with("CREATE FUNCTION"));

            let info = s.table_info("shop", "orders").await.unwrap();
            let col = |n: &str| info.columns.iter().find(|c| c.name == n).unwrap();
            assert!(col("id").primary_key && col("id").data_type == "bigint" && !col("id").nullable);
            assert_eq!(col("id").default.as_deref(), Some("IDENTITY(1,1)"));
            assert_eq!(col("status").data_type, "varchar(10)");
            assert_eq!(col("placed_at").data_type, "datetime2(3)");
            assert_eq!(col("status").default.as_deref(), Some("('pending')"));
            assert_eq!(info.indexes[0].definition, "PRIMARY KEY CLUSTERED ([id])");
            let idx = info.indexes.iter().find(|i| i.name == "orders_placed_idx").unwrap();
            assert_eq!(idx.definition, "CREATE NONCLUSTERED INDEX [orders_placed_idx] ON [shop].[orders] ([placed_at] DESC) INCLUDE ([status])");
            let defs: Vec<_> = info.constraints.iter().map(|k| (k.kind.as_str(), k.definition.as_str())).collect();
            assert!(defs.contains(&("f", "FOREIGN KEY ([customer_id]) REFERENCES [shop].[customers] ([id])")), "{defs:?}");
            assert!(defs.iter().any(|(k, d)| *k == "c" && d.starts_with("CHECK (")), "{defs:?}");
            assert!(info.ddl.starts_with("CREATE TABLE [shop].[orders] (\n    [id] bigint IDENTITY(1,1) NOT NULL,"), "{}", info.ddl);
            assert!(info.ddl.contains("CONSTRAINT [orders_pkey] PRIMARY KEY CLUSTERED ([id])"), "{}", info.ddl);
            assert!(info.ddl.ends_with("INCLUDE ([status]);"), "{}", info.ddl);
            assert!(info.total_size.is_some());
            let items = s.table_info("shop", "order_items").await.unwrap();
            assert!(items.constraints.iter().any(|k| k.definition.ends_with("ON DELETE CASCADE")), "{:?}", items.constraints);
            let view = s.table_info("shop", "order_totals").await.unwrap();
            assert!(view.kind == ObjectKind::View && view.ddl.starts_with("CREATE VIEW shop.order_totals"), "{}", view.ddl);

            // Batches, several result sets, and how values read.
            let out = s
                .run("select 1 as a, null as b, cast(2.5 as decimal(5,2)) as c\nselect id from shop.orders\nGO\nselect cast('2024-01-02 03:04:05.1234567' as datetime2) as d2, cast('2024-01-02 03:04:05.123' as datetime) as dt, cast('2024-01-02' as date) as d, cast(0x00FF as varbinary(2)) as raw, cast(1 as bit) as b, N'naïve' as t, cast('6F9619FF-8B86-D011-B42D-00C04FC964FF' as uniqueidentifier) as g, cast('2024-01-02 03:04:05 +02:00' as datetimeoffset(0)) as o", 10)
                .await
                .unwrap();
            assert_eq!(out.results.len(), 3);
            assert_eq!(out.results[0].rows, vec![vec![Some("1".into()), None, Some("2.50".into())]]);
            assert!(out.results[0].columns[0].is_numeric() && out.results[0].columns[2].is_numeric());
            assert!(out.results[1].truncated && out.results[1].rows.len() == 10 && out.results[1].row_count == 100_000);
            let expect = ["2024-01-02 03:04:05.1234567", "2024-01-02 03:04:05.123", "2024-01-02", "0x00FF", "1", "naïve", "6F9619FF-8B86-D011-B42D-00C04FC964FF", "2024-01-02 03:04:05 +02:00"];
            assert_eq!(out.results[2].rows[0], expect.iter().map(|v| Some(v.to_string())).collect::<Vec<_>>());

            let out = s
                .run("create table #t (x int); insert #t values (1), (2), (3)\nGO\nupdate #t set x = 0 where x > 1\nGO\ndrop table #t", 10)
                .await
                .unwrap();
            let counts: Vec<u64> = out.results.iter().map(|r| r.row_count).collect();
            assert_eq!(counts, vec![3, 2, 0]);

            let orders = DbObject { schema: "shop".into(), name: "orders".into(), kind: ObjectKind::Table, rows_estimate: None, comment: None, signature: None, engine: Engine::SqlServer };
            let sql = browse_sql(&orders, "status = 'paid'", Some(("id", false)), &[], 3, 3);
            assert_eq!(sql, "SELECT * FROM [shop].[orders] WHERE status = 'paid' ORDER BY [id] DESC OFFSET 3 ROWS FETCH NEXT 3 ROWS ONLY");
            let page = s.run(&sql, 3).await.unwrap();
            let ids: Vec<_> = page.results[0].rows.iter().map(|r| r[0].clone().unwrap()).collect();
            assert_eq!(ids, vec!["99985", "99981", "99977"]);
            assert!(s.run(&browse_sql(&orders, "", None, &[], 5, 0), 5).await.is_ok());
            s.run("begin transaction; update shop.orders set note = 'x' where id = 1", 1).await.unwrap();
            s.run("rollback", 1).await.unwrap();
            assert_eq!(s.run("select note from shop.orders where id = 1", 1).await.unwrap().results[0].rows, vec![vec![None]]);

            let err = s.run("select * from nope", 10).await.unwrap_err();
            assert!(err.to_string().starts_with("Msg 208, Level 16"), "{err}");

            let t = Duration::from_secs(5);
            assert!(s.run_read_only("select count(*) from shop.orders", 10, t).await.is_ok());
            for sql in ["delete from shop.orders", "select 1 delete from shop.orders", "exec sp_who", "select * into #x from shop.orders", "select next value for shop.invoice_numbers"] {
                let err = s.run_read_only(sql, 10, t).await.unwrap_err();
                assert!(needs_write_access(&err), "{sql}: {err}");
            }
            assert!(s.run_read_only("select 1; select 2", 10, t).await.unwrap_err().to_string().contains("one statement"));
            assert_eq!(s.run("select count(*) from shop.orders", 1).await.unwrap().results[0].rows[0][0].as_deref(), Some("100000"));

            let out = s.run_write("update shop.orders set note = 'agent' where id <= 3", 10, t).await.unwrap();
            assert_eq!(out.results[0].row_count, 3);
            let err = s.run_write("begin transaction; delete from shop.order_items", 10, t).await.unwrap_err();
            assert!(err.to_string().contains("rolled back"), "{err}");
            assert_eq!(s.run("select count(*) from shop.order_items", 1).await.unwrap().results[0].rows[0][0].as_deref(), Some("100000"));
            s.run("update shop.orders set note = null where id <= 3", 1).await.unwrap();

            let started = std::time::Instant::now();
            let err = s.run_read_only("with g(n) as (select 1 union all select n + 1 from g where n < 100000000) select count(*) from g option (maxrecursion 0)", 10, Duration::from_millis(300)).await.unwrap_err();
            assert!(err.to_string().contains("timed out"), "{err}");
            assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
            assert!(s.run_read_only("select 1", 10, t).await.is_ok());

            // Stop ends the query by closing its connection.
            s.run("begin transaction; update shop.orders set note = 'kept' where id = 5", 1).await.unwrap();
            let started = std::time::Instant::now();
            let stop = s.canceller();
            let running = tokio::spawn({
                let s = s.clone();
                async move { s.run("waitfor delay '00:00:10'", 10).await }
            });
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop.cancel().await.unwrap();
            let err = running.await.unwrap().unwrap_err();
            assert!(err.to_string().starts_with("the query was stopped"), "{err}");
            assert!(started.elapsed() < Duration::from_secs(5));
            // The next query connects again; what was left open is gone.
            let after = s.run("select @@trancount, note from shop.orders where id = 5", 1).await.unwrap();
            assert_eq!(after.results[0].rows[0], vec![Some("0".into()), None]);

            let plan = s.plan("select * from shop.orders where customer_id = 7", false, t).await.unwrap();
            assert!(plan.contains("orders_customer_idx"), "{plan}");
            let plan = s.plan("select count(*) from shop.orders where customer_id = 7", true, t).await.unwrap();
            assert!(plan.contains("actual rows="), "{plan}");
            let explain = s.run(&s.explain_sql("select * from shop.orders where id = 1"), 10).await.unwrap();
            assert!(explain.results.iter().any(|r| r.has_rows()), "{explain:?}");

            let (tables, keys) = s.schema_map("shop").await.unwrap();
            assert!(tables.iter().find(|t| t.name == "orders").unwrap().columns.iter().any(|c| c.name == "id" && c.primary_key && !c.nullable));
            let fk = keys.iter().find(|k| k.from_table == "order_items" && k.to_table == "products").unwrap();
            assert_eq!(fk.from_cols, vec!["product_id"]);
            let catalog = s.completion_catalog().await.unwrap();
            assert!(!catalog.schemas.contains(&"sys".to_string()));
            let (_, found) = crate::complete::suggest(&catalog, "select o.cu from shop.orders o", 11, None);
            assert_eq!(found[0].insert, "customer_id");

            s.run("drop table if exists dbo.tabula_edit; create table dbo.tabula_edit (id int identity primary key, title nvarchar(50) not null, price decimal(10,2), raw varbinary(8), at datetime2, flag bit)", 1).await.unwrap();
            let info = s.table_info("dbo", "tabula_edit").await.unwrap();
            let target = EditTarget::new(Engine::SqlServer, "[dbo].[tabula_edit]".into(), &info.columns);
            let n = s
                .apply(&target, &[
                    Change::Insert { values: vec![cell("title", Some("Mug ☕")), cell("price", Some("4.50")), cell("raw", Some("0x00FF")), cell("at", Some("2024-01-02 03:04:05.5")), cell("flag", Some("1"))] },
                    Change::Insert { values: vec![cell("title", Some("Cup"))] },
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", Some("It's a mug")), cell("price", None)] },
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("price", None)] },
                    Change::Delete { key: vec![cell("id", Some("2"))] },
                ])
                .await
                .unwrap();
            assert_eq!(n, 5);
            let row = s.run("select title, price, raw, at, flag from dbo.tabula_edit", 5).await.unwrap();
            assert_eq!(row.results[0].rows, vec![vec![Some("It's a mug".into()), None, Some("0x00FF".into()), Some("2024-01-02 03:04:05.5000000".into()), Some("1".into())]]);
            let err = s
                .apply(&target, &[
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", Some("changed"))] },
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("price", Some("not a number"))] },
                ])
                .await
                .unwrap_err();
            assert!(err.to_string().contains("nothing was saved"), "{err}");
            let err = s.apply(&target, &[Change::Delete { key: vec![cell("id", Some("99"))] }]).await.unwrap_err();
            assert!(err.to_string().contains("matched 0 rows"), "{err}");
            assert_eq!(s.run("select title from dbo.tabula_edit", 1).await.unwrap().results[0].rows[0][0].as_deref(), Some("It's a mug"));
            assert_eq!(
                target.preview(&Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", Some("ü")), cell("raw", Some("0x01"))] }),
                "UPDATE [dbo].[tabula_edit] SET [title] = N'ü', [raw] = 0x01 WHERE [id] = N'1';"
            );
            s.run("drop table dbo.tabula_edit", 1).await.unwrap();

            let other = s.reconnect_to("master").await.unwrap();
            assert_eq!(other.profile.database, "master");
        });
    }

    fn sqlite_session() -> (tempfile::TempDir, Arc<Session>) {
        let (dir, path) = sqlite::demo_database();
        let s = crate::rt::block_on(Session::connect(Profile::sqlite("shop", &path), None)).expect("connect");
        (dir, s)
    }

    #[test]
    fn sqlite_catalog() {
        let (_dir, s) = sqlite_session();
        crate::rt::block_on(async {
            assert!(s.server_version.starts_with("3."), "{}", s.server_version);
            assert_eq!(s.databases().await.unwrap(), vec![s.profile.database.clone()]);
            assert_eq!(s.schemas().await.unwrap(), vec!["main"]);

            let objs = s.objects("main").await.unwrap();
            let names: Vec<_> = objs.iter().map(|o| (o.name.as_str(), o.kind)).collect();
            assert_eq!(
                names,
                vec![
                    ("customers", ObjectKind::Table),
                    ("order_items", ObjectKind::Table),
                    ("order_totals", ObjectKind::View),
                    ("orders", ObjectKind::Table),
                    ("products", ObjectKind::Table),
                ],
                "sqlite_stat1 and friends stay hidden"
            );
            // The seed ends with ANALYZE, so there are counts to show.
            assert_eq!(objs[3].rows_estimate, Some(100_000));
            assert_eq!(objs[2].rows_estimate, None);

            let info = s.table_info("main", "orders").await.unwrap();
            assert_eq!(info.kind, ObjectKind::Table);
            assert_eq!(info.rows_estimate, Some(100_000));
            let col = |n: &str| info.columns.iter().find(|c| c.name == n).unwrap();
            assert!(col("id").primary_key && col("id").data_type == "INTEGER" && !col("id").nullable);
            assert!(!col("customer_id").nullable && col("note").nullable);
            assert_eq!(col("status").default.as_deref(), Some("'pending'"));
            let index = |n: &str| info.indexes.iter().find(|i| i.name == n).unwrap();
            assert_eq!(
                index("orders_customer_idx").definition,
                "CREATE INDEX orders_customer_idx ON orders(customer_id)"
            );
            let kinds: Vec<_> = info.constraints.iter().map(|k| (k.kind.as_str(), k.definition.as_str())).collect();
            assert_eq!(
                kinds,
                vec![("p", "PRIMARY KEY (id)"), ("f", "FOREIGN KEY (customer_id) REFERENCES customers (id)")]
            );
            assert!(info.ddl.starts_with("CREATE TABLE orders ("), "{}", info.ddl);
            assert!(info.ddl.contains("CHECK (status IN"), "{}", info.ddl);
            assert!(info.ddl.ends_with("ON orders(placed_at DESC);"), "{}", info.ddl);

            // Keys SQLite indexes on its own, and a cascading composite key.
            let info = s.table_info("main", "order_items").await.unwrap();
            assert!(info.indexes[0].primary);
            assert_eq!(info.indexes[0].definition, "PRIMARY KEY (order_id, product_id)");
            let defs: Vec<_> = info.constraints.iter().map(|k| k.definition.as_str()).collect();
            assert!(defs.contains(&"PRIMARY KEY (order_id, product_id)"), "{defs:?}");
            assert!(defs.contains(&"FOREIGN KEY (order_id) REFERENCES orders (id) ON DELETE CASCADE"), "{defs:?}");
            let info = s.table_info("main", "customers").await.unwrap();
            assert!(info.constraints.iter().any(|k| k.kind == "u" && k.definition == "UNIQUE (email)"));

            let view = s.table_info("main", "order_totals").await.unwrap();
            assert_eq!(view.kind, ObjectKind::View);
            assert!(view.ddl.starts_with("CREATE VIEW order_totals AS"), "{}", view.ddl);
            assert_eq!(view.columns.len(), 4);

            assert!(s.table_info("main", "nope").await.is_err());
        });
    }

    #[test]
    fn sqlite_queries() {
        let (_dir, s) = sqlite_session();
        crate::rt::block_on(async {
            let out = s.run("select 1 as a, null as b; select id from orders", 10).await.unwrap();
            assert_eq!(out.results.len(), 2);
            assert_eq!(out.results[0].rows, vec![vec![Some("1".into()), None]]);
            assert!(out.results[1].truncated);
            assert_eq!(out.results[1].rows.len(), 10);
            assert_eq!(out.results[1].row_count, 100_000);

            // Numbers are spotted by value; text is not, whatever the column says.
            let typed = s
                .run("select id, price, title, count(*) over () as n, price * 2 as twice, sku || 'x' from products", 5)
                .await
                .unwrap();
            let cols = &typed.results[0].columns;
            assert_eq!(cols[1].type_name.as_deref(), Some("NUMERIC"));
            assert_eq!(cols[3].type_name, None);
            let numeric: Vec<bool> = cols.iter().map(ResultColumn::is_numeric).collect();
            assert_eq!(numeric, vec![true, true, false, true, true, false]);
            let empty = s.run("select id, title from products where 0", 5).await.unwrap();
            assert!(empty.results[0].has_rows() && empty.results[0].rows.is_empty());
            assert!(empty.results[0].columns[0].is_numeric() && !empty.results[0].columns[1].is_numeric());

            // Writes report what they changed; DDL changes no rows.
            let out = s
                .run("create table t (x); insert into t values (1), (2), (3); update t set x = 0 where x > 1; drop table t", 10)
                .await
                .unwrap();
            let counts: Vec<u64> = out.results.iter().map(|r| r.row_count).collect();
            assert_eq!(counts, vec![0, 3, 2, 0]);
            assert!(out.results.iter().all(|r| !r.has_rows()));

            // The page of rows the Data tab asks for.
            let orders = DbObject {
                schema: "main".into(),
                name: "orders".into(),
                kind: ObjectKind::Table,
                rows_estimate: None,
                comment: None,
                signature: None,
                engine: Engine::Sqlite,
            };
            let page = s.run(&browse_sql(&orders, "status = 'paid'", Some(("id", false)), &[], 3, 3), 3).await.unwrap();
            let ids: Vec<_> = page.results[0].rows.iter().map(|r| r[0].clone().unwrap()).collect();
            assert_eq!(ids, vec!["99985", "99981", "99977"]);

            // Foreign keys are enforced, and a transaction can span runs.
            let err = s.run("delete from customers where id = 1", 1).await.unwrap_err();
            assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
            s.run("begin; update orders set note = 'x' where id = 1", 1).await.unwrap();
            s.run("rollback", 1).await.unwrap();
            let note = s.run("select note from orders where id = 1", 1).await.unwrap();
            assert_eq!(note.results[0].rows, vec![vec![None]]);

            let err = s.run("select * from nope", 10).await.unwrap_err();
            assert_eq!(err.to_string(), "no such table: nope");
            let err = s.run("select 1;\nselect * frm orders", 10).await.unwrap_err();
            assert_eq!(err.to_string(), "near \"frm\": syntax error\nat character 20");

            let plan = s.run(&s.explain_sql("select * from orders where customer_id = 7"), 10).await.unwrap();
            let detail = plan.results[0].rows[0].last().cloned().flatten().unwrap();
            assert!(detail.contains("orders_customer_idx"), "{detail}");
        });
    }

    #[test]
    fn sqlite_read_only_is_enforced_by_sqlite() {
        let (dir, s) = sqlite_session();
        crate::rt::block_on(async {
            let t = Duration::from_secs(5);
            let n = s.run_read_only("select count(*) from orders", 10, t).await.unwrap();
            assert_eq!(n.results[0].rows[0][0].as_deref(), Some("100000"));

            // Writes of every shape are refused, in the way that can ask for approval.
            for sql in [
                "delete from orders",
                "insert into orders (customer_id) values (1) returning id",
                "with d as (select 1) update orders set note = 'x'",
                "create table t (x)",
                "drop view order_totals",
                "pragma journal_mode = wal",
                "vacuum",
            ] {
                let err = s.run_read_only(sql, 10, t).await.unwrap_err();
                assert!(needs_write_access(&err), "{sql}: {err}");
            }

            // One statement means one, and nothing of a batch runs.
            let err = s.run_read_only("select 1; delete from orders", 10, t).await.unwrap_err();
            assert!(err.to_string().contains("one statement"), "{err}");
            let err = s.run_read_only("select 1; select 2", 10, t).await.unwrap_err();
            assert!(err.to_string().contains("one statement"), "{err}");
            assert!(s.run_read_only("select 1; -- just a comment", 10, t).await.is_ok());

            // No way out through another file, and no switching the guard off.
            let other = dir.path().join("other.db");
            let attach = format!("attach database 'file:{}?mode=rwc' as o", other.display());
            let err = s.run_read_only(&attach, 10, t).await.unwrap_err();
            assert!(err.to_string().contains("can't be attached"), "{err}");
            let into = format!("vacuum into '{}'", other.display());
            assert!(s.run_read_only(&into, 10, t).await.is_err());
            assert!(!other.exists());
            assert!(s.run_read_only("select load_extension('x')", 10, t).await.is_err());
            s.run_read_only("pragma query_only = off", 10, t).await.unwrap();
            let err = s.run_read_only("delete from orders", 10, t).await.unwrap_err();
            assert!(needs_write_access(&err), "{err}");

            // A transaction can't be left open to pin an old snapshot.
            let err = s.run_read_only("begin", 10, t).await.unwrap_err();
            assert!(err.to_string().contains("rolled back"), "{err}");
            s.run("update orders set note = 'seen' where id = 2", 1).await.unwrap();
            let seen = s.run_read_only("select note from orders where id = 2", 1, t).await.unwrap();
            assert_eq!(seen.results[0].rows[0][0].as_deref(), Some("seen"));

            // Still all there.
            let n = s.run("select count(*) from orders", 1).await.unwrap();
            assert_eq!(n.results[0].rows[0][0].as_deref(), Some("100000"));

            let join = "select * from orders o join customers c on c.id = o.customer_id where c.id = 7";
            let plan = s.plan(join, true, t).await.unwrap();
            assert!(plan.contains("orders_customer_idx") && plan.contains("no EXPLAIN ANALYZE"), "{plan}");
        });
    }

    #[test]
    fn sqlite_agent_writes_stops_and_timeouts() {
        let (_dir, s) = sqlite_session();
        crate::rt::block_on(async {
            let t = Duration::from_secs(5);
            let out = s.run_write("update orders set note = 'agent' where id <= 3", 10, t).await.unwrap();
            assert_eq!(out.results[0].row_count, 3);
            // The user's connection sees it at once.
            let n = s.run("select count(*) from orders where note = 'agent'", 1).await.unwrap();
            assert_eq!(n.results[0].rows[0][0].as_deref(), Some("3"));

            // A transaction left open would lock the file for everyone else.
            let err = s.run_write("begin; delete from order_items", 10, t).await.unwrap_err();
            assert!(err.to_string().contains("rolled back"), "{err}");
            s.run("update orders set note = null where id <= 3", 1).await.unwrap();
            let n = s.run("select count(*) from order_items", 1).await.unwrap();
            assert_eq!(n.results[0].rows[0][0].as_deref(), Some("100000"));

            let forever = "with recursive g(n) as (select 1 union all select n + 1 from g) select count(*) from g";
            let started = std::time::Instant::now();
            let err = s.run_read_only(forever, 10, Duration::from_millis(200)).await.unwrap_err();
            assert!(err.to_string().contains("timed out"), "{err}");
            assert!(started.elapsed() < Duration::from_secs(3));
            // The limit goes with the call, not the connection.
            assert!(s.run_read_only("select count(*) from orders", 10, t).await.is_ok());

            let stop = s.canceller();
            let running = tokio::spawn({
                let s = s.clone();
                async move { s.run(forever, 10).await }
            });
            tokio::time::sleep(Duration::from_millis(200)).await;
            stop.cancel().await.unwrap();
            let err = running.await.unwrap().unwrap_err();
            assert_eq!(err.to_string(), "the query was stopped");
            assert!(s.run("select 1", 1).await.is_ok());
        });
    }

    #[test]
    fn sqlite_schema_map_and_completion() {
        let (_dir, s) = sqlite_session();
        crate::rt::block_on(async {
            s.run(
                "create table Refunds (id integer primary key, order_ref integer references ORDERS, item_order, item_product,
                   foreign key (item_order, item_product) references order_items)",
                1,
            )
            .await
            .unwrap();
            let (tables, keys) = s.schema_map("main").await.unwrap();
            let orders = tables.iter().find(|t| t.name == "orders").unwrap();
            assert_eq!(orders.rows_estimate, Some(100_000));
            assert!(orders.columns.iter().any(|c| c.name == "id" && c.primary_key && !c.nullable));
            assert!(tables.iter().any(|t| t.name == "order_totals" && t.kind == ObjectKind::View));
            let fk = |from: &str, to: &str| keys.iter().find(|k| k.from_table == from && k.to_table == to).unwrap();
            assert_eq!(fk("orders", "customers").from_cols, vec!["customer_id"]);
            assert_eq!(fk("orders", "customers").to_cols, vec!["id"]);
            assert_eq!(fk("orders", "customers").name, "orders_customer_id_fkey");
            assert_eq!(fk("order_items", "orders").to_schema, "main");
            assert_eq!(fk("order_items", "products").from_cols, vec!["product_id"]);
            // A key that names no columns means the parent's primary key, and
            // the parent is found whatever case it was written in.
            assert_eq!(fk("Refunds", "orders").to_cols, vec!["id"]);
            assert_eq!(fk("Refunds", "order_items").from_cols, vec!["item_order", "item_product"]);
            assert_eq!(fk("Refunds", "order_items").to_cols, vec!["order_id", "product_id"]);

            let catalog = s.completion_catalog().await.unwrap();
            assert_eq!(catalog.schemas, vec!["main"]);
            let (_, found) = crate::complete::suggest(&catalog, "select o.cu from orders o", 11, None);
            assert_eq!(found[0].insert, "customer_id");
        });
    }

    #[test]
    fn sqlite_row_edits() {
        let (_dir, s) = sqlite_session();
        crate::rt::block_on(async {
            s.run("create table edit (id integer primary key, title text not null, price numeric, photo blob, extra)", 1)
                .await
                .unwrap();
            let info = s.table_info("main", "edit").await.unwrap();
            let t = EditTarget::new(Engine::Sqlite, "\"main\".\"edit\"".into(), &info.columns);
            let cell = |c: &str, v: Option<&str>| (c.to_string(), v.map(str::to_string));
            let n = s
                .apply(&t, &[
                    Change::Insert {
                        values: vec![cell("title", Some("Mug")), cell("price", Some("4.50")), cell("photo", Some("x'00FF'"))],
                    },
                    Change::Insert { values: vec![cell("title", Some("Cup"))] },
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", Some("It's a mug")), cell("extra", Some("7"))] },
                    Change::Delete { key: vec![cell("id", Some("2"))] },
                ])
                .await
                .unwrap();
            assert_eq!(n, 4);
            // Numbers went in as numbers, the blob as a blob, and text stayed text.
            let rows = s
                .run("select id, title, price, typeof(price), photo, typeof(photo), extra, typeof(extra) from edit", 10)
                .await
                .unwrap();
            let expect = ["1", "It's a mug", "4.5", "real", "x'00FF'", "blob", "7", "text"];
            assert_eq!(rows.results[0].rows, vec![expect.iter().map(|v| Some(v.to_string())).collect::<Vec<_>>()]);

            // One bad change undoes the good ones before it.
            let err = s
                .apply(&t, &[
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", Some("changed"))] },
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", None)] },
                ])
                .await
                .unwrap_err();
            assert!(err.to_string().contains("change 2 of 2 failed") && err.to_string().contains("NOT NULL"), "{err}");
            assert!(err.to_string().contains("nothing was saved"), "{err}");
            let err = s
                .apply(&t, &[
                    Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("title", Some("changed"))] },
                    Change::Delete { key: vec![cell("id", Some("99"))] },
                ])
                .await
                .unwrap_err();
            assert!(err.to_string().contains("matched 0 rows"), "{err}");
            // A blob too long for the grid is refused, not saved as its preview.
            let long = format!("x'{}…' (300 bytes)", "07".repeat(256));
            let err = s
                .apply(&t, &[Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("photo", Some(&long))] }])
                .await
                .unwrap_err();
            assert!(err.to_string().contains("cut short"), "{err}");
            let row = s.run("select title, photo from edit", 10).await.unwrap();
            assert_eq!(row.results[0].rows, vec![vec![Some("It's a mug".into()), Some("x'00FF'".into())]]);
            // The connection is left ready for the next save.
            let ok = [Change::Delete { key: vec![cell("id", Some("1"))] }];
            assert_eq!(s.apply(&t, &ok).await.unwrap(), 1);

            // Edits respect foreign keys, like the user's own statements.
            let orders = s.table_info("main", "orders").await.unwrap();
            let t = EditTarget::new(Engine::Sqlite, "\"main\".\"orders\"".into(), &orders.columns);
            let orphan = [Change::Update { key: vec![cell("id", Some("1"))], set: vec![cell("customer_id", Some("999999"))] }];
            let err = s.apply(&t, &orphan).await.unwrap_err();
            assert!(err.to_string().contains("FOREIGN KEY"), "{err}");
        });
    }

    #[test]
    fn sqlite_wants_an_existing_database() {
        let dir = tempfile::tempdir().unwrap();
        let connect = |path: &std::path::Path| {
            crate::rt::block_on(Session::connect(Profile::sqlite("x", path.to_str().unwrap()), None))
        };
        let missing = dir.path().join("missing.db");
        let err = connect(&missing).err().unwrap();
        assert!(format!("{err:#}").contains("unable to open database file"), "{err:#}");
        assert!(!missing.exists(), "connecting must not create the file");

        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "this is not a database, it is a text file that is long enough").unwrap();
        let err = connect(&text).err().unwrap();
        assert!(format!("{err:#}").contains("not a database"), "{err:#}");
    }
}
