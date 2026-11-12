//! PostgreSQL access: connecting, the catalog, and running SQL.
//!
//! Everything here is async on tokio (see [`crate::rt`]). Ad-hoc SQL goes
//! through the simple-query protocol, so every value comes back as text and
//! any type displays without per-type decoding.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use futures::{StreamExt as _, pin_mut};
use serde::Serialize;
use tokio_postgres::{CancelToken, Client, Config, NoTls, SimpleQueryMessage};

use crate::profiles::{Profile, SslMode};

/// One column of a result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultColumn {
    pub name: String,
    /// The type name, when known (single statements only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_name: Option<String>,
}

impl ResultColumn {
    /// Numbers read best right-aligned.
    pub fn is_numeric(&self) -> bool {
        matches!(
            self.type_name.as_deref(),
            Some(
                "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "oid" | "money"
            )
        )
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
    /// The planner's estimate (`reltuples`), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows_estimate: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// For functions: the argument list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

impl DbObject {
    pub fn qualified(&self) -> String {
        format!("{}.{}", quote_ident(&self.schema), quote_ident(&self.name))
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
    /// CREATE statement (tables are reconstructed; views and functions come
    /// from the server).
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

/// `"name"`, doubling any quotes inside.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}


/// `'text'`, doubling any quotes inside.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
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

/// The table edits apply to: its quoted name and each column's type.
#[derive(Debug, Clone, Default)]
pub struct EditTarget {
    pub qualified: String,
    pub types: std::collections::HashMap<String, String>,
}

impl EditTarget {
    pub fn new(qualified: String, columns: &[ColumnInfo]) -> EditTarget {
        EditTarget {
            qualified,
            types: columns.iter().map(|c| (c.name.clone(), c.data_type.clone())).collect(),
        }
    }

    /// `value::type`, with the value written by `value` (a literal or `$n`).
    fn typed(&self, column: &str, value: String) -> String {
        match self.types.get(column) {
            Some(t) => format!("{value}::{t}"),
            None => value,
        }
    }

    /// The statement for `change`. `value` renders each value in turn: a
    /// `$n::text` placeholder when it will be bound, or a literal to show it.
    fn sql(&self, change: &Change, mut value: impl FnMut(&Option<String>) -> String) -> String {
        let mut assign = |cells: &[Cell], sep: &str, null_safe: bool| -> String {
            cells
                .iter()
                .map(|(c, v)| {
                    let rhs = self.typed(c, value(v));
                    if null_safe && v.is_none() {
                        format!("{} IS NULL", quote_ident(c))
                    } else {
                        format!("{} = {rhs}", quote_ident(c))
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
            Change::Insert { values } if values.is_empty() => {
                format!("INSERT INTO {} DEFAULT VALUES", self.qualified)
            }
            Change::Insert { values } => {
                let cols: Vec<String> = values.iter().map(|(c, _)| quote_ident(c)).collect();
                let vals: Vec<String> = values.iter().map(|(c, v)| self.typed(c, value(v))).collect();
                format!("INSERT INTO {} ({}) VALUES ({})", self.qualified, cols.join(", "), vals.join(", "))
            }
        }
    }

    /// `change` as SQL with the values written in, to show before saving.
    pub fn preview(&self, change: &Change) -> String {
        let sql = self.sql(change, |v| match v {
            Some(v) => quote_literal(v),
            None => "NULL".into(),
        });
        format!("{sql};")
    }

    /// `change` with `$n` placeholders, and the values to bind to them. A
    /// NULL in a key becomes `IS NULL` and binds nothing.
    fn bound(&self, change: &Change) -> (String, Vec<Option<String>>) {
        let mut params: Vec<Option<String>> = vec![];
        let key_nulls: usize = match change {
            Change::Update { key, .. } | Change::Delete { key } => key.iter().filter(|(_, v)| v.is_none()).count(),
            Change::Insert { .. } => 0,
        };
        let sql = self.sql(change, |v| {
            params.push(v.clone());
            format!("${}::text", params.len())
        });
        if key_nulls == 0 {
            return (sql, params);
        }
        // Rare (a NULL in a key): fall back to literals for this statement.
        (self.preview(change).trim_end_matches(';').to_string(), vec![])
    }
}

/// A database error with the server's detail, hint and position.
pub fn describe_error(e: &tokio_postgres::Error) -> String {
    let Some(db) = e.as_db_error() else {
        let mut s = e.to_string();
        let mut src = std::error::Error::source(e);
        while let Some(inner) = src {
            s.push_str(&format!(": {inner}"));
            src = inner.source();
        }
        return s;
    };
    let mut s = format!("{}: {}", db.severity(), db.message());
    if let Some(d) = db.detail() {
        s.push_str(&format!("\nDETAIL: {d}"));
    }
    if let Some(h) = db.hint() {
        s.push_str(&format!("\nHINT: {h}"));
    }
    if let Some(tokio_postgres::error::ErrorPosition::Original(p)) = db.position() {
        s.push_str(&format!("\nat character {p}"));
    }
    s.push_str(&format!(" (SQLSTATE {})", db.code().code()));
    s
}

fn pg(e: tokio_postgres::Error) -> anyhow::Error {
    anyhow!(describe_error(&e))
}

async fn open(profile: &Profile, password: Option<&str>) -> anyhow::Result<Client> {
    let mut cfg = Config::new();
    cfg.host(&profile.host)
        .port(profile.port)
        .user(&profile.user)
        .dbname(&profile.database)
        .application_name("tabula")
        .connect_timeout(Duration::from_secs(10));
    if let Some(pw) = password {
        cfg.password(pw);
    }
    let client = match profile.ssl {
        SslMode::Disable => {
            let (client, conn) = cfg.connect(NoTls).await.map_err(pg)?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            client
        }
        mode => {
            cfg.ssl_mode(if mode == SslMode::Require {
                tokio_postgres::config::SslMode::Require
            } else {
                tokio_postgres::config::SslMode::Prefer
            });
            // libpq's `prefer` and `require` encrypt without verifying the
            // certificate; match that.
            let tls = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true)
                .build()
                .context("TLS setup failed")?;
            let tls = postgres_native_tls::MakeTlsConnector::new(tls);
            let (client, conn) = cfg.connect(tls).await.map_err(pg)?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            client
        }
    };
    Ok(client)
}

/// An open connection to one database: one client for the catalog and one
/// for user queries, so a long query never blocks the object tree.
pub struct Session {
    pub profile: Profile,
    password: Option<String>,
    meta: Client,
    work: Client,
    pub server_version: String,
}

impl Session {
    pub async fn connect(profile: Profile, password: Option<String>) -> anyhow::Result<Arc<Session>> {
        let meta = open(&profile, password.as_deref()).await?;
        let work = open(&profile, password.as_deref()).await?;
        let server_version = meta
            .query_one("SELECT current_setting('server_version')", &[])
            .await
            .map_err(pg)?
            .get::<_, String>(0);
        Ok(Arc::new(Session {
            profile,
            password,
            meta,
            work,
            server_version,
        }))
    }

    /// The same server, another database.
    pub async fn reconnect_to(&self, database: &str) -> anyhow::Result<Arc<Session>> {
        Session::connect(self.profile.with_database(database), self.password.clone()).await
    }

    /// A token that stops the query running on the work connection.
    pub fn cancel_token(&self) -> CancelToken {
        self.work.cancel_token()
    }

    pub async fn databases(&self) -> anyhow::Result<Vec<String>> {
        let rows = self
            .meta
            .query(
                "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname",
                &[],
            )
            .await
            .map_err(pg)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// User schemas first, then the system ones.
    pub async fn schemas(&self) -> anyhow::Result<Vec<String>> {
        let rows = self
            .meta
            .query(
                "SELECT nspname FROM pg_namespace
                 WHERE nspname NOT LIKE 'pg_toast%' AND nspname NOT LIKE 'pg_temp%'
                 ORDER BY nspname IN ('pg_catalog', 'information_schema'), nspname",
                &[],
            )
            .await
            .map_err(pg)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    pub async fn objects(&self, schema: &str) -> anyhow::Result<Vec<DbObject>> {
        let mut out = Vec::new();
        let rels = self
            .meta
            .query(
                "SELECT c.relname, c.relkind::text,
                        CASE WHEN c.reltuples >= 0 THEN c.reltuples::int8 END,
                        obj_description(c.oid, 'pg_class')
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND c.relkind IN ('r','p','v','m','f','S')
                 ORDER BY c.relname",
                &[&schema],
            )
            .await
            .map_err(pg)?;
        for r in rels {
            let kind = match r.get::<_, String>(1).as_str() {
                "r" | "p" => ObjectKind::Table,
                "v" => ObjectKind::View,
                "m" => ObjectKind::MaterializedView,
                "f" => ObjectKind::ForeignTable,
                _ => ObjectKind::Sequence,
            };
            out.push(DbObject {
                schema: schema.to_string(),
                name: r.get(0),
                kind,
                rows_estimate: r.get(2),
                comment: r.get(3),
                signature: None,
            });
        }
        let funcs = self
            .meta
            .query(
                "SELECT p.proname, pg_get_function_identity_arguments(p.oid),
                        obj_description(p.oid, 'pg_proc')
                 FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                 WHERE n.nspname = $1 AND p.prokind IN ('f', 'p')
                 ORDER BY p.proname",
                &[&schema],
            )
            .await
            .map_err(pg)?;
        for r in funcs {
            out.push(DbObject {
                schema: schema.to_string(),
                name: r.get(0),
                kind: ObjectKind::Function,
                rows_estimate: None,
                comment: r.get(2),
                signature: Some(r.get(1)),
            });
        }
        Ok(out)
    }

    /// Every table and view in a schema with its columns, and the foreign
    /// keys between them: what the schema map draws.
    pub async fn schema_map(&self, schema: &str) -> anyhow::Result<(Vec<MapTable>, Vec<ForeignKey>)> {
        let rows = self
            .meta
            .query(
                "SELECT c.relname, c.relkind::text,
                        CASE WHEN c.reltuples >= 0 THEN c.reltuples::int8 END,
                        a.attname, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
                        COALESCE(a.attnum = ANY(i.indkey), false)
                 FROM pg_class c
                 JOIN pg_namespace n ON n.oid = c.relnamespace
                 JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
                 LEFT JOIN pg_index i ON i.indrelid = c.oid AND i.indisprimary
                 WHERE n.nspname = $1 AND c.relkind IN ('r','p','v','m','f')
                 ORDER BY c.relname, a.attnum",
                &[&schema],
            )
            .await
            .map_err(pg)?;
        let mut tables: Vec<MapTable> = vec![];
        for r in rows {
            let name: String = r.get(0);
            if tables.last().is_none_or(|t| t.name != name) {
                tables.push(MapTable {
                    name,
                    kind: match r.get::<_, String>(1).as_str() {
                        "v" => ObjectKind::View,
                        "m" => ObjectKind::MaterializedView,
                        "f" => ObjectKind::ForeignTable,
                        _ => ObjectKind::Table,
                    },
                    rows_estimate: r.get(2),
                    columns: vec![],
                });
            }
            tables.last_mut().expect("just pushed").columns.push(MapColumn {
                name: r.get(3),
                data_type: r.get(4),
                nullable: r.get(5),
                primary_key: r.get(6),
            });
        }
        let cols = |rel: &str, keys: &str| {
            format!(
                "ARRAY(SELECT a.attname::text FROM unnest(con.{keys}) WITH ORDINALITY k(attnum, ord)
                       JOIN pg_attribute a ON a.attrelid = con.{rel} AND a.attnum = k.attnum ORDER BY k.ord)"
            )
        };
        let rows = self
            .meta
            .query(
                &format!(
                    "SELECT con.conname::text, c.relname::text, fn.nspname::text, fc.relname::text, {}, {}
                     FROM pg_constraint con
                     JOIN pg_class c ON c.oid = con.conrelid
                     JOIN pg_namespace n ON n.oid = c.relnamespace
                     JOIN pg_class fc ON fc.oid = con.confrelid
                     JOIN pg_namespace fn ON fn.oid = fc.relnamespace
                     WHERE con.contype = 'f' AND n.nspname = $1
                     ORDER BY c.relname, con.conname",
                    cols("conrelid", "conkey"),
                    cols("confrelid", "confkey")
                ),
                &[&schema],
            )
            .await
            .map_err(pg)?;
        let keys = rows
            .into_iter()
            .map(|r| ForeignKey {
                name: r.get(0),
                from_table: r.get(1),
                to_schema: r.get(2),
                to_table: r.get(3),
                from_cols: r.get(4),
                to_cols: r.get(5),
            })
            .collect();
        Ok((tables, keys))
    }

    /// The names SQL completion suggests: every table and view in the
    /// user's own schemas, with their columns.
    pub async fn completion_catalog(&self) -> anyhow::Result<crate::complete::Catalog> {
        let schemas: Vec<String> = self
            .schemas()
            .await?
            .into_iter()
            .filter(|s| s != "pg_catalog" && s != "information_schema")
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
        Ok(crate::complete::Catalog { schemas, tables })
    }

    /// Columns, indexes, constraints and DDL for a table or view.
    pub async fn table_info(&self, schema: &str, name: &str) -> anyhow::Result<TableInfo> {
        let rel = self
            .meta
            .query_opt(
                "SELECT c.oid, c.relkind::text, obj_description(c.oid, 'pg_class'),
                        CASE WHEN c.reltuples >= 0 THEN c.reltuples::int8 END,
                        pg_size_pretty(pg_total_relation_size(c.oid))
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[&schema, &name],
            )
            .await
            .map_err(pg)?
            .ok_or_else(|| anyhow!("no table or view named {schema}.{name}"))?;
        let oid: u32 = rel.get(0);
        let kind = match rel.get::<_, String>(1).as_str() {
            "v" => ObjectKind::View,
            "m" => ObjectKind::MaterializedView,
            "f" => ObjectKind::ForeignTable,
            "S" => ObjectKind::Sequence,
            _ => ObjectKind::Table,
        };
        let cols = self
            .meta
            .query(
                "SELECT a.attname, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
                        pg_get_expr(d.adbin, d.adrelid),
                        COALESCE(a.attnum = ANY(i.indkey), false),
                        col_description(a.attrelid, a.attnum)
                 FROM pg_attribute a
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
                 LEFT JOIN pg_index i ON i.indrelid = a.attrelid AND i.indisprimary
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY a.attnum",
                &[&oid],
            )
            .await
            .map_err(pg)?;
        let columns: Vec<ColumnInfo> = cols
            .iter()
            .map(|r| ColumnInfo {
                name: r.get(0),
                data_type: r.get(1),
                nullable: r.get(2),
                default: r.get(3),
                primary_key: r.get(4),
                comment: r.get(5),
            })
            .collect();
        let indexes = self
            .meta
            .query(
                "SELECT ic.relname, pg_get_indexdef(i.indexrelid), i.indisunique, i.indisprimary
                 FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid
                 WHERE i.indrelid = $1 ORDER BY i.indisprimary DESC, ic.relname",
                &[&oid],
            )
            .await
            .map_err(pg)?
            .iter()
            .map(|r| IndexInfo {
                name: r.get(0),
                definition: r.get(1),
                unique: r.get(2),
                primary: r.get(3),
            })
            .collect::<Vec<_>>();
        let constraints = self
            .meta
            .query(
                "SELECT conname, contype::text, pg_get_constraintdef(oid)
                 FROM pg_constraint WHERE conrelid = $1
                 ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 ELSE 3 END, conname",
                &[&oid],
            )
            .await
            .map_err(pg)?
            .iter()
            .map(|r| ConstraintInfo {
                name: r.get(0),
                kind: r.get(1),
                definition: r.get(2),
            })
            .collect::<Vec<_>>();
        let qualified = format!("{}.{}", quote_ident(schema), quote_ident(name));
        let ddl = match kind {
            ObjectKind::View | ObjectKind::MaterializedView => {
                let def: String = self
                    .meta
                    .query_one("SELECT pg_get_viewdef($1::oid, true)", &[&oid])
                    .await
                    .map_err(pg)?
                    .get(0);
                let what = if kind == ObjectKind::View {
                    "VIEW"
                } else {
                    "MATERIALIZED VIEW"
                };
                format!("CREATE {what} {qualified} AS\n{}", def.trim_end())
            }
            _ => table_ddl(&qualified, &columns, &constraints, &indexes),
        };
        Ok(TableInfo {
            schema: schema.to_string(),
            name: name.to_string(),
            kind,
            comment: rel.get(2),
            rows_estimate: rel.get(3),
            total_size: rel.get(4),
            columns,
            indexes,
            constraints,
            ddl,
        })
    }

    /// The `CREATE FUNCTION` statement for a function.
    pub async fn function_ddl(&self, schema: &str, name: &str, args: &str) -> anyhow::Result<String> {
        let sig = format!("{}.{}({args})", quote_ident(schema), quote_ident(name));
        let row = self
            .meta
            .query_one(
                "SELECT pg_get_functiondef(to_regprocedure($1))",
                &[&sig],
            )
            .await
            .map_err(pg)?;
        row.get::<_, Option<String>>(0)
            .ok_or_else(|| anyhow!("function {sig} not found"))
    }

    /// Apply row edits in one transaction: all of them, or none. An update
    /// or delete that doesn't hit exactly one row stops and rolls back. Runs
    /// on the catalog connection, so it never joins a transaction the user
    /// opened in a query tab. Returns how many rows changed.
    pub async fn apply(&self, target: &EditTarget, changes: &[Change]) -> anyhow::Result<u64> {
        if changes.is_empty() {
            return Ok(0);
        }
        self.meta.batch_execute("BEGIN").await.map_err(pg)?;
        let mut total = 0;
        for (i, change) in changes.iter().enumerate() {
            let (sql, params) = target.bound(change);
            let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                params.iter().map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
            let done = match self.meta.execute(sql.as_str(), &refs).await {
                Ok(n) => n,
                Err(e) => {
                    let _ = self.meta.batch_execute("ROLLBACK").await;
                    return Err(anyhow!("change {} of {} failed, nothing was saved: {}\n{}", i + 1, changes.len(), describe_error(&e), target.preview(change)));
                }
            };
            if done != 1 && !matches!(change, Change::Insert { .. }) {
                let _ = self.meta.batch_execute("ROLLBACK").await;
                return Err(anyhow!(
                    "change {} of {} matched {done} rows instead of 1 (the row may have changed or gone), nothing was saved:\n{}",
                    i + 1,
                    changes.len(),
                    target.preview(change)
                ));
            }
            total += done;
        }
        self.meta.batch_execute("COMMIT").await.map_err(pg)?;
        Ok(total)
    }

    /// Run `sql` (any number of statements) on the work connection, keeping
    /// at most `max_rows` rows per statement.
    pub async fn run(&self, sql: &str, max_rows: usize) -> anyhow::Result<QueryOutcome> {
        run_on(&self.work, sql, max_rows).await
    }

    /// Run one statement inside a read-only transaction with a timeout, on
    /// the catalog connection. For AI agents: a write fails, and so does
    /// anything that is not exactly one statement.
    pub async fn run_read_only(
        &self,
        sql: &str,
        max_rows: usize,
        timeout: Duration,
    ) -> anyhow::Result<QueryOutcome> {
        // The extended protocol takes exactly one statement, so this is what
        // stops `SELECT 1; COMMIT; DELETE …` from escaping the transaction.
        self.meta
            .prepare(sql)
            .await
            .map_err(|e| anyhow!("{} (agents run one statement at a time)", describe_error(&e)))?;
        if is_transaction_control(sql) {
            bail!("transaction control statements aren't allowed in read-only mode");
        }
        self.meta
            .batch_execute(&format!(
                "BEGIN READ ONLY; SET LOCAL statement_timeout = {}; SELECT 1",
                timeout.as_millis()
            ))
            .await
            .map_err(pg)?;
        let out = run_on(&self.meta, sql, max_rows).await;
        let _ = self.meta.batch_execute("ROLLBACK").await;
        out
    }

    /// Run any single statement on the catalog connection with a timeout, for
    /// AI agents with write access.
    pub async fn run_write(
        &self,
        sql: &str,
        max_rows: usize,
        timeout: Duration,
    ) -> anyhow::Result<QueryOutcome> {
        self.meta
            .batch_execute(&format!("SET statement_timeout = {}", timeout.as_millis()))
            .await
            .map_err(pg)?;
        let out = run_on(&self.meta, sql, max_rows).await;
        let _ = self.meta.batch_execute("RESET statement_timeout").await;
        out
    }

}

/// The error a read-only transaction gives for a write (SQLSTATE 25006).
pub fn needs_write_access(e: &anyhow::Error) -> bool {
    e.to_string().contains("SQLSTATE 25006")
}

fn is_transaction_control(sql: &str) -> bool {
    let first = sql
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == ';')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        first.as_str(),
        "begin" | "commit" | "rollback" | "end" | "abort" | "start" | "savepoint" | "release" | "prepare" | "set"
            | "reset" | "discard"
    )
}

async fn run_on(client: &Client, sql: &str, max_rows: usize) -> anyhow::Result<QueryOutcome> {
    let started = Instant::now();
    // Column types, when the text is a single statement the server can plan.
    let types: Option<Vec<String>> = match client.prepare(sql).await {
        Ok(stmt) => Some(stmt.columns().iter().map(|c| c.type_().name().to_string()).collect()),
        Err(_) => None,
    };
    let stream = client.simple_query_raw(sql).await.map_err(pg)?;
    pin_mut!(stream);
    let mut results = Vec::new();
    let mut cur = ResultSet::default();
    while let Some(msg) = stream.next().await {
        match msg.map_err(pg)? {
            SimpleQueryMessage::RowDescription(cols) => {
                cur.columns = cols
                    .iter()
                    .enumerate()
                    .map(|(i, c)| ResultColumn {
                        name: c.name().to_string(),
                        type_name: types.as_ref().and_then(|t| t.get(i).cloned()),
                    })
                    .collect();
            }
            SimpleQueryMessage::Row(row) => {
                if cur.rows.len() < max_rows {
                    cur.rows
                        .push((0..row.len()).map(|i| row.get(i).map(str::to_string)).collect());
                } else {
                    cur.truncated = true;
                }
            }
            SimpleQueryMessage::CommandComplete(n) => {
                cur.row_count = n;
                results.push(std::mem::take(&mut cur));
            }
            _ => {}
        }
    }
    Ok(QueryOutcome {
        results,
        elapsed: started.elapsed(),
    })
}

/// A readable CREATE TABLE built from the catalog.
fn table_ddl(
    qualified: &str,
    columns: &[ColumnInfo],
    constraints: &[ConstraintInfo],
    indexes: &[IndexInfo],
) -> String {
    let mut lines: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut l = format!("    {} {}", quote_ident(&c.name), c.data_type);
            if let Some(d) = &c.default {
                l.push_str(&format!(" DEFAULT {d}"));
            }
            if !c.nullable {
                l.push_str(" NOT NULL");
            }
            l
        })
        .collect();
    for k in constraints.iter().filter(|k| k.kind != "n") {
        lines.push(format!(
            "    CONSTRAINT {} {}",
            quote_ident(&k.name),
            k.definition
        ));
    }
    let mut s = format!("CREATE TABLE {qualified} (\n{}\n);", lines.join(",\n"));
    // Indexes that back a constraint are already covered above.
    for i in indexes {
        if constraints.iter().any(|k| k.name == i.name) {
            continue;
        }
        s.push_str(&format!("\n{};", i.definition));
    }
    s
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
    let mut sql = format!("SELECT * FROM {}", object.qualified());
    if !filter.trim().is_empty() {
        sql.push_str(&format!(" WHERE {}", filter.trim()));
    }
    let mut by: Vec<String> = order
        .iter()
        .map(|(col, asc)| format!("{} {}", quote_ident(col), if *asc { "ASC" } else { "DESC" }))
        .collect();
    by.extend(key.iter().filter(|k| order.is_none_or(|(c, _)| c != k.as_str())).map(|k| quote_ident(k)));
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
        assert_eq!(params, vec![Some("It's new".to_string()), None, Some("7".to_string())]);

        assert_eq!(t.preview(&Change::Delete { key }), "DELETE FROM \"shop\".\"products\" WHERE \"id\" = '7'::integer;");
        assert_eq!(t.preview(&Change::Insert { values: vec![] }), "INSERT INTO \"shop\".\"products\" DEFAULT VALUES;");
        assert_eq!(
            t.preview(&Change::Insert { values: vec![("title".into(), Some("Mug".into()))] }),
            "INSERT INTO \"shop\".\"products\" (\"title\") VALUES ('Mug'::text);"
        );
        assert_eq!(quote_literal("a'b"), "'a''b'");
    }

    #[test]
    fn csv_and_tsv() {
        let cols = vec![
            ResultColumn { name: "a".into(), type_name: None },
            ResultColumn { name: "b".into(), type_name: None },
        ];
        let rows = vec![vec![Some("x,y".into()), None], vec![Some("q\"".into()), Some("t\tu".into())]];
        assert_eq!(to_csv(&cols, &rows), "a,b\r\n\"x,y\",\r\n\"q\"\"\",t\tu\r\n");
        assert_eq!(to_tsv(&cols, &rows), "a\tb\nx,y\t\nq\"\tt u");
    }

    #[test]
    fn spots_transaction_control_and_writes() {
        assert!(is_transaction_control("  COMMIT"));
        assert!(is_transaction_control("set role postgres"));
        assert!(!is_transaction_control("select 1"));
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
        let p = Profile { ssl: SslMode::Disable, ..p };
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
            s.meta
                .batch_execute("create temp table tabula_edit (id serial primary key, title text not null, price numeric(10,2), tags text[])")
                .await
                .unwrap();
            let t = EditTarget {
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
            let rows = s.meta.query("select id, title, price::text, tags::text from tabula_edit", &[]).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get::<_, String>(1), "It's a mug");
            assert_eq!(rows[0].get::<_, Option<String>>(2), None);
            assert_eq!(rows[0].get::<_, String>(3), "{a,b}");

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
            let row = s.meta.query_one("select title from tabula_edit", &[]).await.unwrap();
            assert_eq!(row.get::<_, String>(0), "It's a mug");
        });
    }
}
