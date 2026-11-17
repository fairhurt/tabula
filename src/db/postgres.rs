//! The PostgreSQL backend, on tokio-postgres.
//!
//! Ad-hoc SQL goes through the simple-query protocol, so every value comes
//! back as text and any type displays without per-type decoding.

use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use futures::{StreamExt as _, pin_mut};
use tokio_postgres::error::SqlState;
use tokio_postgres::{CancelToken, Client, Config, NoTls, SimpleQueryMessage};

use super::{
    Change, ColumnInfo, ConstraintInfo, DbObject, EditTarget, ForeignKey, IndexInfo, MapColumn, MapTable,
    ObjectKind, QueryOutcome, ResultColumn, ResultSet, TableInfo, WriteRefused, quote_ident,
};
use crate::profiles::{Profile, SslMode};

/// A database error with the server's detail, hint and position.
fn describe_error(e: &tokio_postgres::Error) -> String {
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
    let text = describe_error(&e);
    if e.code() == Some(&SqlState::READ_ONLY_SQL_TRANSACTION) {
        anyhow::Error::new(WriteRefused(text))
    } else {
        anyhow!(text)
    }
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
            cfg.ssl_mode(if mode == SslMode::Prefer {
                tokio_postgres::config::SslMode::Prefer
            } else {
                tokio_postgres::config::SslMode::Require
            });
            // libpq's `prefer` and `require` encrypt without verifying the
            // certificate; match that. `verify` checks the certificate
            // against this computer's trusted roots, and the host name.
            let verify = mode == SslMode::Verify;
            let tls = native_tls::TlsConnector::builder()
                .danger_accept_invalid_certs(!verify)
                .danger_accept_invalid_hostnames(!verify)
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

/// One client for the catalog and one for user queries, so a long query
/// never blocks the object tree.
pub struct Backend {
    meta: Client,
    work: Client,
}

impl Backend {
    /// Connect, and report the server's version.
    pub async fn connect(profile: &Profile, password: Option<&str>) -> anyhow::Result<(Backend, String)> {
        let meta = open(profile, password).await?;
        let work = open(profile, password).await?;
        let version = meta
            .query_one("SELECT current_setting('server_version')", &[])
            .await
            .map_err(pg)?
            .get::<_, String>(0);
        Ok((Backend { meta, work }, version))
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
                params.iter().map(|(_, v)| v as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
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


    /// The plan as text. With `analyze` the statement runs (read-only) and
    /// the plan has actual timings and buffers.
    pub async fn plan(&self, sql: &str, analyze: bool, timeout: Duration) -> anyhow::Result<String> {
        let explain = format!(
            "EXPLAIN ({}FORMAT TEXT) {}",
            if analyze { "ANALYZE, BUFFERS, " } else { "" },
            sql.trim().trim_end_matches(';')
        );
        let out = self.run_read_only(&explain, 2_000, timeout).await?;
        Ok(out
            .main()
            .map(|r| {
                r.rows
                    .iter()
                    .filter_map(|row| row.first().cloned().flatten())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default())
    }
}

/// Numbers read best right-aligned.
fn is_numeric(type_name: &str) -> bool {
    matches!(
        type_name,
        "int2" | "int4" | "int8" | "float4" | "float8" | "numeric" | "oid" | "money"
    )
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
                    .map(|(i, c)| {
                        let type_name = types.as_ref().and_then(|t| t.get(i).cloned());
                        ResultColumn {
                            name: c.name().to_string(),
                            numeric: type_name.as_deref().is_some_and(is_numeric),
                            type_name,
                        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spots_transaction_control_and_writes() {
        assert!(is_transaction_control("  COMMIT"));
        assert!(is_transaction_control("set role postgres"));
        assert!(!is_transaction_control("select 1"));
    }
}
