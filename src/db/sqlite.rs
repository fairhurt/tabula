//! The SQLite backend, on rusqlite with SQLite built in.
//!
//! A session holds three connections to the file: one for the catalog and
//! agents' approved writes, one for the user's queries, and one opened
//! read-only for agents' reads, so SQLite itself refuses their writes.
//! rusqlite blocks, so every call runs on tokio's blocking pool.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use rusqlite::fallible_iterator::FallibleIterator as _;
use rusqlite::ffi::ErrorCode;
use rusqlite::limits::Limit;
use rusqlite::types::ValueRef;
use rusqlite::{Batch, Connection, InterruptHandle, OpenFlags, Statement};

use super::{
    Change, ColumnInfo, ConstraintInfo, DbObject, EditTarget, ForeignKey, IndexInfo, MapColumn, MapTable,
    ObjectKind, QueryOutcome, ResultColumn, ResultSet, TableInfo, WriteRefused, quote_ident,
};

/// How long to wait for another program's lock on the file.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Blobs longer than this are shown cut short.
const MAX_BLOB: usize = 256;

/// One connection, usable from async code.
struct Db {
    conn: Arc<Mutex<Connection>>,
    interrupt: Arc<InterruptHandle>,
    /// Set by [`Stop`]; ends a run between two statements, where there is
    /// nothing for `interrupt` to interrupt.
    stopped: Arc<AtomicBool>,
}

impl Db {
    fn new(conn: Connection) -> Db {
        Db {
            interrupt: Arc::new(conn.get_interrupt_handle()),
            conn: Arc::new(Mutex::new(conn)),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run `f` with the connection on the blocking pool.
    async fn call<T, F>(&self, f: F) -> anyhow::Result<T>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = conn.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut conn)
        })
        .await
        .map_err(|e| anyhow!("the database task failed: {e}"))?
    }

    async fn run(&self, sql: &str, max_rows: usize, mode: Mode) -> anyhow::Result<QueryOutcome> {
        let (sql, stopped) = (sql.to_string(), self.stopped.clone());
        self.call(move |conn| run(conn, &sql, max_rows, mode, &stopped)).await
    }
}

/// Stops the query running on a session's work connection.
pub struct Stop {
    interrupt: Arc<InterruptHandle>,
    stopped: Arc<AtomicBool>,
}

impl Stop {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.interrupt.interrupt();
    }
}

pub struct Backend {
    meta: Db,
    work: Db,
    read_only: Db,
}

impl Backend {
    /// Open the file at `path`, and report SQLite's version. The file has to
    /// exist: a viewer that creates an empty database on a typo helps nobody.
    pub async fn connect(path: &str) -> anyhow::Result<(Backend, String)> {
        let path = path.to_string();
        tokio::task::spawn_blocking(move || {
            let meta = open(&path, false)?;
            let version = meta.query_row("SELECT sqlite_version()", [], |r| r.get(0)).map_err(describe)?;
            let backend = Backend {
                meta: Db::new(meta),
                work: Db::new(open(&path, false)?),
                read_only: Db::new(open(&path, true)?),
            };
            Ok((backend, version))
        })
        .await
        .map_err(|e| anyhow!("the database task failed: {e}"))?
    }

    pub fn stop_handle(&self) -> Stop {
        Stop {
            interrupt: self.work.interrupt.clone(),
            stopped: self.work.stopped.clone(),
        }
    }

    /// The attached databases, which SQLite addresses like schemas. Only
    /// `main` unless something attaches more; `temp` is each connection's own.
    pub async fn schemas(&self) -> anyhow::Result<Vec<String>> {
        self.meta
            .call(|conn| {
                query(conn, "SELECT name FROM pragma_database_list WHERE name <> 'temp' ORDER BY seq", [], |r| {
                    r.get(0)
                })
            })
            .await
    }

    pub async fn objects(&self, schema: &str) -> anyhow::Result<Vec<DbObject>> {
        let schema = schema.to_string();
        self.meta
            .call(move |conn| {
                let estimates = row_estimates(conn, &schema);
                Ok(relations(conn, &schema)?
                    .into_iter()
                    .map(|(name, kind)| DbObject {
                        schema: schema.clone(),
                        kind: object_kind(&kind),
                        rows_estimate: estimates.get(&name).copied(),
                        comment: None,
                        signature: None,
                        engine: crate::profiles::Engine::Sqlite,
                        name,
                    })
                    .collect())
            })
            .await
    }

    pub async fn table_info(&self, schema: &str, name: &str) -> anyhow::Result<TableInfo> {
        let (schema, name) = (schema.to_string(), name.to_string());
        self.meta.call(move |conn| table_info(conn, &schema, &name)).await
    }

    /// Every table and view in a schema with its columns, and the foreign
    /// keys between them: what the schema map draws.
    pub async fn schema_map(&self, schema: &str) -> anyhow::Result<(Vec<MapTable>, Vec<ForeignKey>)> {
        let schema = schema.to_string();
        self.meta.call(move |conn| schema_map(conn, &schema)).await
    }

    /// Apply row edits in one transaction: all of them, or none. An update
    /// or delete that doesn't hit exactly one row stops and rolls back. Runs
    /// on the catalog connection, so it never joins a transaction the user
    /// opened in a query tab. Returns how many rows changed.
    pub async fn apply(&self, target: &EditTarget, changes: &[Change]) -> anyhow::Result<u64> {
        if changes.is_empty() {
            return Ok(0);
        }
        let (target, changes) = (target.clone(), changes.to_vec());
        self.meta.call(move |conn| apply(conn, &target, &changes)).await
    }

    /// Run `sql` (any number of statements) on the work connection.
    pub async fn run(&self, sql: &str, max_rows: usize) -> anyhow::Result<QueryOutcome> {
        self.work.run(sql, max_rows, Mode::User).await
    }

    /// Run one statement on the read-only connection with a timeout. For AI
    /// agents: a write fails, and so does anything that is not exactly one
    /// statement.
    pub async fn run_read_only(
        &self,
        sql: &str,
        max_rows: usize,
        timeout: Duration,
    ) -> anyhow::Result<QueryOutcome> {
        self.read_only.run(sql, max_rows, Mode::AgentRead(timeout)).await
    }

    /// Run `sql` on the catalog connection with a timeout, for AI agents
    /// with write access.
    pub async fn run_write(&self, sql: &str, max_rows: usize, timeout: Duration) -> anyhow::Result<QueryOutcome> {
        self.meta.run(sql, max_rows, Mode::AgentWrite(timeout)).await
    }

    /// The query plan as an indented tree. SQLite can't measure a run the
    /// way `EXPLAIN ANALYZE` does, so `analyze` only adds a note.
    pub async fn plan(&self, sql: &str, analyze: bool, timeout: Duration) -> anyhow::Result<String> {
        let explain = format!("EXPLAIN QUERY PLAN {}", sql.trim().trim_end_matches(';'));
        let out = self.run_read_only(&explain, 2_000, timeout).await?;
        // Rows are (id, parent, notused, detail), parents before children.
        let mut depth: HashMap<String, usize> = HashMap::new();
        let mut plan = String::new();
        for row in out.main().map(|r| r.rows.as_slice()).unwrap_or_default() {
            let cell = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
            let d = depth.get(&cell(1)).map_or(0, |d| d + 1);
            depth.insert(cell(0), d);
            plan.push_str(&format!("{}{}\n", "  ".repeat(d), cell(3)));
        }
        if analyze {
            plan.push_str("(SQLite has no EXPLAIN ANALYZE; this is the plan, not a measured run.)\n");
        }
        Ok(plan.trim_end().to_string())
    }
}

/// Who is running SQL, which decides what a run may do.
#[derive(Clone, Copy)]
enum Mode {
    /// The user, on the work connection: any number of statements, no time
    /// limit, and a transaction may stay open for the next run.
    User,
    /// An agent, on the read-only connection: exactly one statement.
    AgentRead(Duration),
    /// An agent allowed to write.
    AgentWrite(Duration),
}

fn open(path: &str, read_only: bool) -> anyhow::Result<Connection> {
    let access = if read_only {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } else {
        // No SQLITE_OPEN_CREATE: a missing file is an error.
        OpenFlags::SQLITE_OPEN_READ_WRITE
    };
    let open = || -> rusqlite::Result<Connection> {
        let conn = Connection::open_with_flags(path, access | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        if read_only {
            // Belt and braces. Without the limit, `ATTACH` could open another
            // file writable, and `VACUUM INTO` could write a new one.
            conn.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
            conn.pragma_update(None, "query_only", true)?;
        } else {
            conn.pragma_update(None, "foreign_keys", true)?;
        }
        // Opening is lazy; this is what notices a file that isn't a database.
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
        Ok(conn)
    };
    open().map_err(describe).with_context(|| format!("couldn't open {path}"))
}

/// A SQLite error as text, marking a refused write.
fn describe(e: rusqlite::Error) -> anyhow::Error {
    describe_in(e, "")
}

/// [`describe`], for an error from running `whole`: a syntax error's position
/// counts from the start of `whole`, not of the statement it is in.
fn describe_in(e: rusqlite::Error, whole: &str) -> anyhow::Error {
    let code = e.sqlite_error_code();
    let text = match e {
        rusqlite::Error::SqlInputError { msg, sql, offset, .. } => {
            // SQLite saw only the statement and what follows it.
            let (text, before) = match whole.len().checked_sub(sql.len()) {
                Some(before) if whole.ends_with(&sql) => (whole, before),
                _ => (sql.as_str(), 0),
            };
            match usize::try_from(offset).map(|at| before + at) {
                Ok(at) if text.is_char_boundary(at) => {
                    format!("{msg}\nat character {}", text[..at].chars().count() + 1)
                }
                _ => msg,
            }
        }
        rusqlite::Error::SqliteFailure(_, Some(msg)) => msg,
        other => other.to_string(),
    };
    match code {
        Some(ErrorCode::ReadOnly) => anyhow::Error::new(WriteRefused(text)),
        Some(ErrorCode::OperationInterrupted) => anyhow!("the query was stopped"),
        _ => anyhow!(text),
    }
}

/// Collect the rows of a catalog query.
fn query<T, P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
    row: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> anyhow::Result<Vec<T>> {
    let read = || conn.prepare(sql)?.query_map(params, row)?.collect::<rusqlite::Result<Vec<T>>>();
    read().map_err(describe)
}

fn object_kind(kind: &str) -> ObjectKind {
    if kind == "view" { ObjectKind::View } else { ObjectKind::Table }
}

/// Row counts as of the last `ANALYZE`, by table. Empty if it never ran.
fn row_estimates(conn: &Connection, schema: &str) -> HashMap<String, i64> {
    // `stat` starts with the number of rows in the table or index.
    let sql = format!("SELECT tbl, stat FROM {}.sqlite_stat1", quote_ident(schema));
    let rows: Vec<(String, String)> = query(conn, &sql, [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap_or_default();
    let mut out = HashMap::new();
    for (table, stat) in rows {
        if let Some(n) = stat.split(' ').next().and_then(|n| n.parse::<i64>().ok()) {
            let e = out.entry(table).or_insert(n);
            *e = n.max(*e);
        }
    }
    out
}

/// A name as it reads in a definition: quoted only if it has to be.
fn ident(name: &str) -> String {
    let plain = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain { name.to_string() } else { quote_ident(name) }
}

fn idents(names: &[String]) -> String {
    names.iter().map(|n| ident(n)).collect::<Vec<_>>().join(", ")
}

/// A table's columns and the names of its primary key's, in key order.
fn columns(conn: &Connection, schema: &str, table: &str) -> anyhow::Result<(Vec<ColumnInfo>, Vec<String>)> {
    // (name, type, notnull, default, position in the primary key, hidden)
    let cols: Vec<(String, String, bool, Option<String>, i64, i64)> = query(
        conn,
        "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1, ?2) ORDER BY cid",
        [table, schema],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    )?;
    let mut pk: Vec<(i64, String)> = cols.iter().filter(|c| c.4 > 0).map(|c| (c.4, c.0.clone())).collect();
    pk.sort();
    let pk: Vec<String> = pk.into_iter().map(|(_, n)| n).collect();
    let single_key = pk.len() == 1;
    let columns = cols
        .into_iter()
        // 1 marks a virtual table's hidden columns, which `SELECT *` skips.
        .filter(|c| c.5 != 1)
        .map(|(name, data_type, notnull, default, pk, _)| {
            // An INTEGER PRIMARY KEY is the rowid, which is never NULL even
            // though the pragma doesn't say so.
            let rowid = pk > 0 && single_key && data_type.eq_ignore_ascii_case("INTEGER");
            ColumnInfo {
                name,
                data_type,
                nullable: !notnull && !rowid,
                default,
                primary_key: pk > 0,
                comment: None,
            }
        })
        .collect();
    Ok((columns, pk))
}

/// A foreign key as SQLite's pragma describes it.
struct Reference {
    from: Vec<String>,
    table: String,
    /// Empty when the key names no columns, meaning the parent's primary key.
    to: Vec<String>,
    on_update: String,
    on_delete: String,
}

fn references(conn: &Connection, schema: &str, table: &str) -> anyhow::Result<Vec<Reference>> {
    // One row per column of each key: (id, table, from, to, on update, on delete)
    let rows: Vec<(i64, String, String, Option<String>, String, String)> = query(
        conn,
        "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete
         FROM pragma_foreign_key_list(?1, ?2) ORDER BY id, seq",
        [table, schema],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    )?;
    let mut out: Vec<Reference> = Vec::new();
    let mut last = None;
    for (id, table, from, to, on_update, on_delete) in rows {
        if last != Some(id) {
            last = Some(id);
            out.push(Reference {
                from: vec![],
                table,
                to: vec![],
                on_update,
                on_delete,
            });
        }
        let key = out.last_mut().expect("just pushed");
        key.from.push(from);
        key.to.extend(to);
    }
    Ok(out)
}

/// The tables and views in a schema, as (name, type).
fn relations(conn: &Connection, schema: &str) -> anyhow::Result<Vec<(String, String)>> {
    let sql = format!(
        "SELECT name, type FROM {}.sqlite_master
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
         ORDER BY name",
        quote_ident(schema)
    );
    query(conn, &sql, [], |r| Ok((r.get(0)?, r.get(1)?)))
}

/// Every table and view with its columns, and the foreign keys between them.
fn schema_map(conn: &Connection, schema: &str) -> anyhow::Result<(Vec<MapTable>, Vec<ForeignKey>)> {
    let estimates = row_estimates(conn, schema);
    let mut tables = Vec::new();
    let mut keys_of = HashMap::new();
    for (name, kind) in relations(conn, schema)? {
        let (cols, pk) = columns(conn, schema, &name)?;
        tables.push(MapTable {
            kind: object_kind(&kind),
            rows_estimate: estimates.get(&name).copied(),
            columns: cols
                .into_iter()
                .map(|c| MapColumn {
                    name: c.name,
                    data_type: c.data_type,
                    nullable: c.nullable,
                    primary_key: c.primary_key,
                })
                .collect(),
            name: name.clone(),
        });
        // SQLite matches names without regard to case.
        keys_of.insert(name.to_lowercase(), (name, pk));
    }
    let mut keys = Vec::new();
    for table in tables.iter().filter(|t| t.kind == ObjectKind::Table) {
        for r in references(conn, schema, &table.name)? {
            let parent = keys_of.get(&r.table.to_lowercase());
            keys.push(ForeignKey {
                // Unnamed in SQLite's catalog; this is the name PostgreSQL would give it.
                name: format!("{}_{}_fkey", table.name, r.from.join("_")),
                from_table: table.name.clone(),
                from_cols: r.from,
                to_schema: schema.to_string(),
                to_table: parent.map_or(r.table, |(name, _)| name.clone()),
                to_cols: match parent {
                    Some((_, pk)) if r.to.is_empty() => pk.clone(),
                    _ => r.to,
                },
            });
        }
    }
    Ok((tables, keys))
}

/// Columns, indexes, constraints and DDL for a table or view. SQLite keeps
/// the CREATE statements as written; constraints are rebuilt from pragmas and
/// have no names, so they go by their columns. CHECKs show only in the DDL.
fn table_info(conn: &Connection, schema: &str, name: &str) -> anyhow::Result<TableInfo> {
    let master = format!("{}.sqlite_master", quote_ident(schema));
    let (kind, create_sql) = query(
        conn,
        &format!("SELECT type, sql FROM {master} WHERE name = ?1 AND type IN ('table', 'view')"),
        [name],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
    )?
    .into_iter()
    .next()
    .ok_or_else(|| anyhow!("no table or view named {schema}.{name}"))?;

    let (columns, pk) = columns(conn, schema, name)?;
    let mut constraints = Vec::new();
    if !pk.is_empty() {
        constraints.push(ConstraintInfo {
            name: pk.join(", "),
            kind: "p".into(),
            definition: format!("PRIMARY KEY ({})", idents(&pk)),
        });
    }

    // (name, unique, origin: c = CREATE INDEX, u = UNIQUE, pk = PRIMARY KEY)
    let index_list: Vec<(String, bool, String)> = query(
        conn,
        "SELECT name, \"unique\", origin FROM pragma_index_list(?1, ?2)",
        [name, schema],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    // Each index, and whether its definition is a statement of its own.
    let mut indexes: Vec<(IndexInfo, bool)> = Vec::new();
    for (index, unique, origin) in index_list {
        let on: Vec<String> = query(
            conn,
            "SELECT coalesce(name, '<expression>') FROM pragma_index_info(?1, ?2) ORDER BY seqno",
            [index.as_str(), schema],
            |r| r.get(0),
        )?;
        let written: Option<String> = query(
            conn,
            &format!("SELECT sql FROM {master} WHERE type = 'index' AND name = ?1"),
            [index.as_str()],
            |r| r.get(0),
        )?
        .into_iter()
        .next()
        .flatten();
        if origin == "u" {
            constraints.push(ConstraintInfo {
                name: on.join(", "),
                kind: "u".into(),
                definition: format!("UNIQUE ({})", idents(&on)),
            });
        }
        let standalone = written.is_some();
        // SQLite makes a constraint's index itself; there is no statement.
        let definition = written.unwrap_or_else(|| {
            format!("{} ({})", if origin == "pk" { "PRIMARY KEY" } else { "UNIQUE" }, idents(&on))
        });
        let info = IndexInfo {
            name: index,
            definition,
            unique,
            primary: origin == "pk",
        };
        indexes.push((info, standalone));
    }
    indexes.sort_by(|(a, _), (b, _)| b.primary.cmp(&a.primary).then_with(|| a.name.cmp(&b.name)));
    let index_sql: Vec<String> =
        indexes.iter().filter(|(_, standalone)| *standalone).map(|(i, _)| i.definition.clone()).collect();
    let indexes: Vec<IndexInfo> = indexes.into_iter().map(|(i, _)| i).collect();

    for r in references(conn, schema, name)? {
        let mut definition = format!("FOREIGN KEY ({}) REFERENCES {}", idents(&r.from), ident(&r.table));
        if !r.to.is_empty() {
            definition.push_str(&format!(" ({})", idents(&r.to)));
        }
        for (event, action) in [("UPDATE", &r.on_update), ("DELETE", &r.on_delete)] {
            if action != "NO ACTION" {
                definition.push_str(&format!(" ON {event} {action}"));
            }
        }
        constraints.push(ConstraintInfo {
            name: r.from.join(", "),
            kind: "f".into(),
            definition,
        });
    }
    let order = |k: &ConstraintInfo| match k.kind.as_str() {
        "p" => 0,
        "u" => 1,
        _ => 2,
    };
    constraints.sort_by_key(order);

    let triggers: Vec<String> = query(
        conn,
        &format!("SELECT sql FROM {master} WHERE type = 'trigger' AND tbl_name = ?1 AND sql IS NOT NULL ORDER BY name"),
        [name],
        |r| r.get(0),
    )?;
    let ddl = create_sql
        .into_iter()
        .chain(index_sql)
        .chain(triggers)
        .map(|sql| format!("{};", sql.trim_end().trim_end_matches(';')))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(TableInfo {
        schema: schema.to_string(),
        name: name.to_string(),
        kind: object_kind(&kind),
        comment: None,
        rows_estimate: row_estimates(conn, schema).get(name).copied(),
        total_size: None,
        columns,
        indexes,
        constraints,
        ddl,
    })
}

/// How SQLite treats values stored in a column, from its declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    Integer,
    Text,
    /// Stored as given: a column declared BLOB, or with no type at all.
    Blob,
    Real,
    Numeric,
}

impl Affinity {
    /// SQLite's own rules, in its order.
    pub fn of(declared: &str) -> Affinity {
        let t = declared.to_ascii_uppercase();
        let has = |words: &[&str]| words.iter().any(|w| t.contains(w));
        if has(&["INT"]) {
            Affinity::Integer
        } else if has(&["CHAR", "CLOB", "TEXT"]) {
            Affinity::Text
        } else if t.is_empty() || has(&["BLOB"]) {
            Affinity::Blob
        } else if has(&["REAL", "FLOA", "DOUB"]) {
            Affinity::Real
        } else {
            Affinity::Numeric
        }
    }
}

/// A grid cell's text on its way back into the database.
#[derive(Debug, PartialEq)]
pub enum EditValue<'a> {
    Null,
    /// Bound as text; the column's affinity turns `42` into a number.
    Text(&'a str),
    Blob(Vec<u8>),
}

impl EditValue<'_> {
    /// What `text` means in a column declared `declared`. A blob comes back
    /// from the way [`display`] shows it; one shown cut short can't.
    pub fn read<'a>(declared: Option<&str>, text: Option<&'a str>) -> anyhow::Result<EditValue<'a>> {
        let Some(text) = text else { return Ok(EditValue::Null) };
        if declared.map(Affinity::of) != Some(Affinity::Blob) {
            return Ok(EditValue::Text(text));
        }
        let Some(rest) = text.strip_prefix("x'") else { return Ok(EditValue::Text(text)) };
        if let Some(hex) = rest.strip_suffix('\'')
            && hex.len() % 2 == 0
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("checked hex");
            return Ok(EditValue::Blob((0..hex.len()).step_by(2).map(byte).collect()));
        }
        if rest.contains("…' (") && rest.ends_with(" bytes)") {
            bail!("a BLOB longer than {MAX_BLOB} bytes is shown cut short, so it can't be saved from the grid");
        }
        Ok(EditValue::Text(text))
    }

    /// As a literal, for showing the statement. A number bound for a numeric
    /// column reads better (and means the same) without quotes.
    pub fn literal(&self, declared: Option<&str>) -> String {
        match self {
            EditValue::Null => "NULL".into(),
            EditValue::Blob(b) => format!("x'{}'", b.iter().map(|byte| format!("{byte:02X}")).collect::<String>()),
            EditValue::Text(t) => {
                let numeric = matches!(
                    declared.map(Affinity::of),
                    Some(Affinity::Integer | Affinity::Real | Affinity::Numeric)
                );
                let digits = t.strip_prefix('-').unwrap_or(t);
                let plain = !digits.is_empty()
                    && digits.split('.').count() <= 2
                    && digits.split('.').all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
                if numeric && plain { t.to_string() } else { super::quote_literal(t) }
            }
        }
    }
}

/// Apply row edits in one transaction: all of them, or none.
fn apply(conn: &mut Connection, target: &EditTarget, changes: &[Change]) -> anyhow::Result<u64> {
    let failed = |i: usize, why: String, change: &Change| {
        anyhow!("change {} of {} {why}, nothing was saved:\n{}", i + 1, changes.len(), target.preview(change))
    };
    // IMMEDIATE takes the write lock now, not at the first change.
    conn.execute_batch("BEGIN IMMEDIATE").map_err(describe)?;
    let mut total = 0;
    for (i, change) in changes.iter().enumerate() {
        let run = || -> anyhow::Result<usize> {
            // Every cell has to read back, bound here or not.
            for (column, text) in change.cells() {
                EditValue::read(target.declared(column), text.as_deref())?;
            }
            let (sql, cells) = target.bound(change);
            let mut values = Vec::with_capacity(cells.len());
            for (column, text) in &cells {
                values.push(match EditValue::read(target.declared(column), text.as_deref())? {
                    EditValue::Null => rusqlite::types::Value::Null,
                    EditValue::Text(t) => rusqlite::types::Value::Text(t.to_string()),
                    EditValue::Blob(b) => rusqlite::types::Value::Blob(b),
                });
            }
            conn.execute(&sql, rusqlite::params_from_iter(values)).map_err(describe)
        };
        let done = match run() {
            Ok(n) => n,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(failed(i, format!("failed ({e})"), change));
            }
        };
        if done != 1 && !matches!(change, Change::Insert { .. }) {
            let _ = conn.execute_batch("ROLLBACK");
            let why = format!("matched {done} rows instead of 1 (the row may have changed or gone)");
            return Err(failed(i, why, change));
        }
        total += done as u64;
    }
    conn.execute_batch("COMMIT").map_err(describe)?;
    Ok(total)
}

/// Why a run ended early.
enum Failure {
    Sqlite {
        error: rusqlite::Error,
        /// The statement that failed is one SQLite counts as a write.
        writes: bool,
    },
    Other(anyhow::Error),
}

impl From<rusqlite::Error> for Failure {
    fn from(error: rusqlite::Error) -> Self {
        Failure::Sqlite { error, writes: false }
    }
}

fn run(
    conn: &mut Connection,
    sql: &str,
    max_rows: usize,
    mode: Mode,
    stopped: &AtomicBool,
) -> anyhow::Result<QueryOutcome> {
    let started = Instant::now();
    stopped.store(false, Ordering::SeqCst);
    let timeout = match mode {
        Mode::User => None,
        Mode::AgentRead(t) | Mode::AgentWrite(t) => Some(t),
    };
    if let Some(t) = timeout {
        // SQLite has no statement timeout; have it ask as it works.
        let deadline = started + t;
        conn.progress_handler(1_000, Some(move || Instant::now() >= deadline)).map_err(describe)?;
    }
    let out = run_statements(conn, sql, max_rows, matches!(mode, Mode::AgentRead(_)), stopped);
    if timeout.is_some() {
        let _ = conn.progress_handler(0, None::<fn() -> bool>);
    }
    // An agent's connection is shared by its later calls (and a write
    // transaction locks the file), so nothing may stay open on it.
    let left_open = !matches!(mode, Mode::User) && !conn.is_autocommit();
    if left_open {
        let _ = conn.execute_batch("ROLLBACK");
    }
    let results = out.map_err(|f| match f {
        Failure::Other(e) => e,
        Failure::Sqlite { error, writes } => {
            let code = error.sqlite_error_code();
            let e = describe_in(error, sql);
            if code == Some(ErrorCode::OperationInterrupted) && timeout.is_some_and(|t| started.elapsed() >= t) {
                anyhow!("the query timed out after {} s", timeout.unwrap_or_default().as_secs())
            } else if !matches!(mode, Mode::AgentRead(_)) || e.is::<WriteRefused>() {
                e
            } else if writes {
                // VACUUM, say, fails on the read-only connection in its own way.
                anyhow::Error::new(WriteRefused(e.to_string()))
            } else if e.to_string().contains("too many attached databases") {
                anyhow!("other databases can't be attached on a read-only connection")
            } else {
                e
            }
        }
    })?;
    if left_open {
        bail!(match mode {
            Mode::AgentRead(_) => "a transaction can't stay open between an agent's calls, so it was rolled back",
            _ => "a transaction can't stay open between an agent's calls, so it was rolled back; send BEGIN … COMMIT as one call",
        });
    }
    Ok(QueryOutcome {
        results,
        elapsed: started.elapsed(),
    })
}

fn run_statements(
    conn: &Connection,
    sql: &str,
    max_rows: usize,
    single: bool,
    stopped: &AtomicBool,
) -> Result<Vec<ResultSet>, Failure> {
    let mut batch = Batch::new(conn, sql);
    let mut results = Vec::new();
    let mut next = batch.next()?;
    if single && next.is_some() {
        // Look ahead before running anything: one statement means one.
        if !matches!(batch.next(), Ok(None)) {
            return Err(Failure::Other(anyhow!("agents run one statement at a time")));
        }
    }
    while let Some(mut stmt) = next {
        if stopped.load(Ordering::SeqCst) {
            return Err(Failure::Other(anyhow!("the query was stopped")));
        }
        let writes = !stmt.readonly();
        results.push(run_statement(conn, &mut stmt, max_rows).map_err(|error| Failure::Sqlite { error, writes })?);
        drop(stmt);
        next = if single { None } else { batch.next()? };
    }
    Ok(results)
}

fn run_statement(conn: &Connection, stmt: &mut Statement<'_>, max_rows: usize) -> rusqlite::Result<ResultSet> {
    let width = stmt.column_count();
    let mut columns: Vec<ResultColumn> = stmt
        .columns()
        .map(|c| ResultColumn {
            name: c.name().to_string(),
            type_name: c.decl_type().map(str::to_string),
            numeric: false,
        })
        .collect();
    // Per column: did any value show up, and were all of them numbers?
    let mut seen = vec![false; width];
    let mut numbers = vec![true; width];
    let changes_before = conn.total_changes();
    let mut set = ResultSet::default();
    let mut rows = stmt.raw_query();
    while let Some(row) = rows.next()? {
        set.row_count += 1;
        if set.rows.len() >= max_rows {
            set.truncated = true;
            continue;
        }
        let mut cells = Vec::with_capacity(width);
        for i in 0..width {
            let value = row.get_ref(i)?;
            if value != ValueRef::Null {
                seen[i] = true;
                numbers[i] &= matches!(value, ValueRef::Integer(_) | ValueRef::Real(_));
            }
            cells.push(display(value));
        }
        set.rows.push(cells);
    }
    drop(rows);
    for (i, c) in columns.iter_mut().enumerate() {
        // SQLite types values, not columns, so go by the values; the
        // declared type only speaks for a column with none to show.
        c.numeric = if seen[i] { numbers[i] } else { c.type_name.as_deref().is_some_and(declared_numeric) };
    }
    if width == 0 {
        // `changes()` keeps the last INSERT, UPDATE or DELETE's count, so
        // only trust it when this statement changed something.
        set.row_count = if conn.total_changes() != changes_before { conn.changes() } else { 0 };
    }
    set.columns = columns;
    Ok(set)
}

/// A value as text, the way the sqlite3 shell would print it.
fn display(value: ValueRef<'_>) -> Option<String> {
    Some(match value {
        ValueRef::Null => return None,
        ValueRef::Integer(n) => n.to_string(),
        // Debug keeps the `.0` on whole numbers.
        ValueRef::Real(f) => format!("{f:?}"),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(b) => {
            let hex: String = b.iter().take(MAX_BLOB).map(|byte| format!("{byte:02X}")).collect();
            if b.len() > MAX_BLOB {
                format!("x'{hex}…' ({} bytes)", b.len())
            } else {
                format!("x'{hex}'")
            }
        }
    })
}

/// Whether a declared type says "numbers" clearly enough to right-align a
/// column with no values to go by. (Plain NUMERIC affinity doesn't: it is
/// also what DATETIME and BOOLEAN get.)
fn declared_numeric(decl: &str) -> bool {
    let t = decl.to_ascii_uppercase();
    match Affinity::of(decl) {
        Affinity::Integer | Affinity::Real => true,
        Affinity::Numeric => t.contains("NUMERIC") || t.contains("DECIMAL"),
        Affinity::Text | Affinity::Blob => false,
    }
}

/// A copy of the demo shop (`scripts/seed-sqlite.sql`) in a temporary folder.
#[cfg(test)]
pub fn demo_database() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("shop.db");
    Connection::open(&path)
        .and_then(|c| c.execute_batch(include_str!("../../scripts/seed-sqlite.sql")))
        .expect("seed the demo database");
    let path = path.to_string_lossy().into_owned();
    (dir, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_display_like_the_shell() {
        assert_eq!(display(ValueRef::Null), None);
        assert_eq!(display(ValueRef::Integer(-7)).as_deref(), Some("-7"));
        assert_eq!(display(ValueRef::Real(2.0)).as_deref(), Some("2.0"));
        assert_eq!(display(ValueRef::Real(0.25)).as_deref(), Some("0.25"));
        assert_eq!(display(ValueRef::Text(b"caf\xc3\xa9")).as_deref(), Some("café"));
        assert_eq!(display(ValueRef::Blob(&[0, 255])).as_deref(), Some("x'00FF'"));
        let long = display(ValueRef::Blob(&[1; 300])).unwrap();
        assert!(long.ends_with("…' (300 bytes)") && long.len() < 2 * 300, "{long}");
    }

    #[test]
    fn edited_cells_go_back_as_what_they_were() {
        let read = |declared: Option<&str>, text: &'static str| EditValue::read(declared, Some(text));
        assert_eq!(EditValue::read(Some("TEXT"), None).unwrap(), EditValue::Null);
        assert_eq!(read(Some("INTEGER"), "42").unwrap(), EditValue::Text("42"));
        // A blob round-trips through the way it is displayed, in a blob column only.
        assert_eq!(read(Some("BLOB"), "x'00FF'").unwrap(), EditValue::Blob(vec![0, 255]));
        assert_eq!(read(Some(""), "x''").unwrap(), EditValue::Blob(vec![]));
        assert_eq!(read(Some("TEXT"), "x'00FF'").unwrap(), EditValue::Text("x'00FF'"));
        assert_eq!(read(Some("BLOB"), "x'0G'").unwrap(), EditValue::Text("x'0G'"));
        assert_eq!(read(Some("BLOB"), "hello").unwrap(), EditValue::Text("hello"));
        let long = display(ValueRef::Blob(&[7; 300])).unwrap();
        assert!(EditValue::read(Some("BLOB"), Some(&long)).is_err());

        let literal = |declared: &str, text: &'static str| read(Some(declared), text).unwrap().literal(Some(declared));
        assert_eq!(literal("INTEGER", "42"), "42");
        assert_eq!(literal("NUMERIC(10,2)", "-4.50"), "-4.50");
        assert_eq!(literal("INTEGER", "4 2"), "'4 2'");
        assert_eq!(literal("INTEGER", "1."), "'1.'");
        assert_eq!(literal("TEXT", "42"), "'42'");
        assert_eq!(literal("TEXT", "it's"), "'it''s'");
        assert_eq!(literal("BLOB", "x'00ff'"), "x'00FF'");
        assert_eq!(EditValue::Null.literal(Some("TEXT")), "NULL");
    }

    #[test]
    fn declared_types_follow_affinity() {
        assert_eq!(Affinity::of("VARCHAR(20)"), Affinity::Text);
        assert_eq!(Affinity::of("POINT"), Affinity::Integer, "SQLite's rule: anything with INT in it");
        assert_eq!(Affinity::of(""), Affinity::Blob);
        assert_eq!(Affinity::of("DATETIME"), Affinity::Numeric);
        for t in ["INTEGER", "bigint", "REAL", "double precision", "NUMERIC(10,2)", "decimal"] {
            assert!(declared_numeric(t), "{t}");
        }
        for t in ["TEXT", "varchar(20)", "BLOB", "DATETIME", "boolean", ""] {
            assert!(!declared_numeric(t), "{t}");
        }
    }
}
