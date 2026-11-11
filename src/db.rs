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

/// `"name"`, doubling any quotes inside.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
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
    limit: usize,
    offset: usize,
) -> String {
    let mut sql = format!("SELECT * FROM {}", object.qualified());
    if !filter.trim().is_empty() {
        sql.push_str(&format!(" WHERE {}", filter.trim()));
    }
    if let Some((col, asc)) = order {
        sql.push_str(&format!(
            " ORDER BY {} {}",
            quote_ident(col),
            if asc { "ASC" } else { "DESC" }
        ));
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
            browse_sql(&o, " quantity > 1 ", Some(("order_id", false)), 100, 200),
            "SELECT * FROM \"shop\".\"Order Items\" WHERE quantity > 1 ORDER BY \"order_id\" DESC LIMIT 100 OFFSET 200"
        );
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
    fn test_session() -> Option<Arc<Session>> {
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
        });
    }
}
