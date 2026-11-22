//! The MySQL backend (MariaDB too), on mysql_async.
//!
//! The user's queries run on a connection of their own, so a transaction
//! they open stays theirs. The catalog, row edits and agents share a small
//! pool, so a slow agent query never holds up the object tree. Ad-hoc SQL
//! goes through the text protocol, where every value already arrives as text.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::Queryable as _;
use mysql_async::{Column, Conn, Opts, OptsBuilder, Pool, PoolConstraints, PoolOpts, Row, SslOpts, Value};
use tokio::sync::Mutex;

use super::script;
use super::{
    Change, ColumnInfo, ConstraintInfo, DbObject, EditTarget, ForeignKey, IndexInfo, MapColumn, MapTable,
    ObjectKind, QueryOutcome, ResultColumn, ResultSet, TableInfo, WriteRefused, hex, is_binary_type, parse_hex,
};
use crate::profiles::{Engine, Profile, SslMode};

/// Bytes shown of a binary value before it is cut short.
const MAX_BLOB: usize = 256;
/// Databases the server keeps for itself, listed after the user's.
const SYSTEM_DATABASES: [&str; 4] = ["information_schema", "mysql", "performance_schema", "sys"];

/// ER_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION, and the server's --read-only.
const READ_ONLY_ERRORS: [u16; 2] = [1792, 1290];
/// ER_QUERY_INTERRUPTED: what `KILL QUERY` leaves behind.
const INTERRUPTED: u16 = 1317;

fn quote(name: &str) -> String {
    Engine::MySql.quote_ident(name)
}

/// The server's error as the mysql client prints it.
fn my(e: mysql_async::Error) -> anyhow::Error {
    match e {
        mysql_async::Error::Server(s) if READ_ONLY_ERRORS.contains(&s.code) => {
            anyhow::Error::new(WriteRefused(format!("ERROR {} ({}): {}", s.code, s.state, s.message)))
        }
        mysql_async::Error::Server(s) if s.code == INTERRUPTED => anyhow!("the query was stopped"),
        mysql_async::Error::Server(s) => anyhow!("ERROR {} ({}): {}", s.code, s.state, s.message),
        e => anyhow!("{e}"),
    }
}

/// A catalog value as text, whatever type it came back as.
fn text(row: &Row, i: usize) -> Option<String> {
    match row.as_ref(i)? {
        Value::NULL => None,
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::Int(n) => Some(n.to_string()),
        Value::UInt(n) => Some(n.to_string()),
        v => Some(v.as_sql(true).trim_matches('\'').to_string()),
    }
}

fn string(row: &Row, i: usize) -> String {
    text(row, i).unwrap_or_default()
}

fn number(row: &Row, i: usize) -> Option<i64> {
    text(row, i)?.parse().ok()
}

fn flag(row: &Row, i: usize) -> bool {
    number(row, i).is_some_and(|n| n != 0)
}

/// Connection options for `profile`, with `tls` as given.
fn options(profile: &Profile, password: Option<&str>, database: Option<&str>, tls: Option<SslOpts>) -> OptsBuilder {
    OptsBuilder::default()
        .ip_or_hostname(profile.host.clone())
        .tcp_port(profile.port)
        .user((!profile.user.is_empty()).then(|| profile.user.clone()))
        .pass(password.map(str::to_string))
        .db_name(database.filter(|d| !d.is_empty()).map(str::to_string))
        .prefer_socket(false)
        .tcp_nodelay(true)
        .ssl_opts(tls)
        .connect_attribute("program_name", "tabula")
}

/// The TLS settings to try first for `mode`, and whether to fall back to a
/// plain connection when the server has no TLS.
fn tls(mode: SslMode) -> (Option<SslOpts>, bool) {
    // As with PostgreSQL: `prefer` and `require` encrypt without checking
    // the certificate; `verify` checks it against this computer's roots.
    let unverified = || SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true);
    match mode {
        SslMode::Disable => (None, false),
        SslMode::Prefer => (Some(unverified()), true),
        SslMode::Require => (Some(unverified()), false),
        SslMode::Verify => (Some(SslOpts::default()), false),
    }
}

async fn open(opts: OptsBuilder) -> anyhow::Result<Conn> {
    match tokio::time::timeout(Duration::from_secs(10), Conn::new(opts)).await {
        Ok(conn) => conn.map_err(my),
        Err(_) => bail!("timed out connecting to the server"),
    }
}

/// Run `sql` (any number of statements) and keep at most `max_rows` rows of
/// each result.
async fn run_on(conn: &mut Conn, sql: &str, max_rows: usize) -> anyhow::Result<QueryOutcome> {
    let started = Instant::now();
    let mut result = conn.query_iter(sql).await.map_err(my)?;
    let mut results = vec![];
    // Each pass reads one statement's result; `next` moves on to the next
    // statement once this one's rows run out.
    while let Some(columns) = result.columns() {
        let mut set = ResultSet { columns: columns.iter().map(result_column).collect(), ..ResultSet::default() };
        if columns.is_empty() {
            set.row_count = result.affected_rows();
            set.columns.clear();
            result.next().await.map_err(my)?;
        } else {
            let binary: Vec<Shown> = columns.iter().map(shown).collect();
            while let Some(row) = result.next().await.map_err(my)? {
                set.row_count += 1;
                if set.rows.len() < max_rows {
                    set.rows.push(row.unwrap_raw().into_iter().zip(&binary).map(|(v, b)| cell(v, *b)).collect());
                } else {
                    set.truncated = true;
                }
            }
        }
        results.push(set);
    }
    Ok(QueryOutcome { results, elapsed: started.elapsed() })
}

/// How a column's bytes become text.
#[derive(Clone, Copy, PartialEq)]
enum Shown {
    Text,
    /// Bytes, as `0x…` hex.
    Hex,
    /// A BIT(n), as its number.
    Bits,
}

fn shown(c: &Column) -> Shown {
    use ColumnType::*;
    match c.column_type() {
        MYSQL_TYPE_BIT => Shown::Bits,
        // Charset 63 is `binary`: BINARY, VARBINARY and BLOB columns.
        MYSQL_TYPE_STRING | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR | MYSQL_TYPE_BLOB | MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB
            if c.character_set() == 63 =>
        {
            Shown::Hex
        }
        MYSQL_TYPE_GEOMETRY => Shown::Hex,
        _ => Shown::Text,
    }
}

fn cell(v: Option<Value>, shown: Shown) -> Option<String> {
    match v? {
        Value::NULL => None,
        Value::Bytes(b) => Some(match shown {
            Shown::Hex => hex(&b, MAX_BLOB),
            Shown::Bits => b.iter().fold(0u64, |n, byte| (n << 8) | u64::from(*byte)).to_string(),
            // Text that isn't UTF-8 (a latin1 column read raw, say) shows as hex
            // rather than being mangled.
            Shown::Text => String::from_utf8(b).unwrap_or_else(|e| hex(e.as_bytes(), MAX_BLOB)),
        }),
        Value::Int(n) => Some(n.to_string()),
        Value::UInt(n) => Some(n.to_string()),
        Value::Float(n) => Some(n.to_string()),
        Value::Double(n) => Some(n.to_string()),
        v => Some(v.as_sql(true).trim_matches('\'').to_string()),
    }
}

fn result_column(c: &Column) -> ResultColumn {
    use ColumnType::*;
    let t = c.column_type();
    let numeric = matches!(
        t,
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_INT24
            | MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE | MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL | MYSQL_TYPE_YEAR
            | MYSQL_TYPE_BIT
    );
    let unsigned = c.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let binary = c.character_set() == 63;
    let name = match t {
        MYSQL_TYPE_TINY => "tinyint",
        MYSQL_TYPE_SHORT => "smallint",
        MYSQL_TYPE_INT24 => "mediumint",
        MYSQL_TYPE_LONG => "int",
        MYSQL_TYPE_LONGLONG => "bigint",
        MYSQL_TYPE_FLOAT => "float",
        MYSQL_TYPE_DOUBLE => "double",
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
        MYSQL_TYPE_YEAR => "year",
        MYSQL_TYPE_BIT => "bit",
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
        MYSQL_TYPE_JSON => "json",
        MYSQL_TYPE_GEOMETRY => "geometry",
        MYSQL_TYPE_ENUM => "enum",
        MYSQL_TYPE_SET => "set",
        MYSQL_TYPE_BLOB | MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB => {
            if binary { "blob" } else { "text" }
        }
        MYSQL_TYPE_STRING => {
            if binary { "binary" } else { "char" }
        }
        MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => {
            if binary { "varbinary" } else { "varchar" }
        }
        _ => "",
    };
    ResultColumn {
        name: c.name_str().into_owned(),
        type_name: (!name.is_empty()).then(|| if unsigned && numeric { format!("{name} unsigned") } else { name.to_string() }),
        numeric,
    }
}

/// Stops the query running on a session's work connection.
pub struct Stop {
    pool: Pool,
    id: u32,
}

impl Stop {
    pub async fn stop(&self) -> anyhow::Result<()> {
        kill_query(&self.pool, self.id).await
    }
}

/// `KILL QUERY` from another connection. The login may always stop its own.
async fn kill_query(pool: &Pool, id: u32) -> anyhow::Result<()> {
    let mut conn = pool.get_conn().await.map_err(my)?;
    conn.query_drop(format!("KILL QUERY {id}")).await.map_err(my)
}

pub struct Backend {
    /// The catalog, row edits and agents.
    pool: Pool,
    /// The user's queries.
    work: Arc<Mutex<Conn>>,
    work_id: u32,
    database: String,
}

impl Backend {
    /// Connect, and report the server's version. With no database in the
    /// profile, the first of the user's databases is picked and written back.
    pub async fn connect(profile: &mut Profile, password: Option<&str>) -> anyhow::Result<(Backend, String)> {
        let (first_tls, fallback) = tls(profile.ssl);
        let database = (!profile.database.is_empty()).then(|| profile.database.clone());
        let mut tls = first_tls;
        let mut work = match open(options(profile, password, database.as_deref(), tls.clone())).await {
            Err(e) if fallback && e.to_string().to_ascii_lowercase().contains("ssl") => {
                tls = None;
                open(options(profile, password, database.as_deref(), None)).await?
            }
            r => r?,
        };
        let version: String = work.query_first("SELECT VERSION()").await.map_err(my)?.unwrap_or_default();
        if database.is_none() {
            let names: Vec<String> = work.query("SHOW DATABASES").await.map_err(my)?;
            profile.database = sort_databases(names).into_iter().next().unwrap_or_else(|| "information_schema".into());
            work.query_drop(format!("USE {}", quote(&profile.database))).await.map_err(my)?;
        }
        let pool_opts = PoolOpts::default()
            .with_constraints(PoolConstraints::new(1, 4).expect("1 <= 4"))
            // Nothing here leaves session state behind, so skip the reset's
            // round trip on every checkout.
            .with_reset_connection(false)
            .with_inactive_connection_ttl(Duration::from_secs(60));
        let pool_opts = options(profile, password, Some(&profile.database), tls)
            // Row edits count the rows they matched, not just those they changed.
            .client_found_rows(true)
            .pool_opts(pool_opts);
        let pool = Pool::new(Opts::from(pool_opts));
        let backend = Backend {
            work_id: work.id(),
            work: Arc::new(Mutex::new(work)),
            pool,
            database: profile.database.clone(),
        };
        Ok((backend, version))
    }

    pub fn stop_handle(&self) -> Stop {
        Stop { pool: self.pool.clone(), id: self.work_id }
    }

    async fn conn(&self) -> anyhow::Result<mysql_async::Conn> {
        self.pool.get_conn().await.map_err(my)
    }

    pub async fn databases(&self) -> anyhow::Result<Vec<String>> {
        let names: Vec<String> = self.conn().await?.query("SHOW DATABASES").await.map_err(my)?;
        Ok(sort_databases(names))
    }

    /// A MySQL database is a schema: this one is the session's only one.
    pub async fn schemas(&self) -> anyhow::Result<Vec<String>> {
        Ok(vec![self.database.clone()])
    }

    pub async fn objects(&self, schema: &str) -> anyhow::Result<Vec<DbObject>> {
        let mut conn = self.conn().await?;
        let rels: Vec<Row> = conn
            .exec(
                "SELECT TABLE_NAME, TABLE_TYPE, TABLE_ROWS, TABLE_COMMENT
                 FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
                (schema,),
            )
            .await
            .map_err(my)?;
        let mut out: Vec<DbObject> = rels
            .iter()
            .map(|r| {
                let kind = relation_kind(&string(r, 1));
                DbObject {
                    schema: schema.to_string(),
                    name: string(r, 0),
                    kind,
                    // Views have no count of their own.
                    rows_estimate: (kind == ObjectKind::Table).then(|| number(r, 2)).flatten(),
                    comment: text(r, 3).filter(|c| !c.is_empty() && c != "VIEW"),
                    signature: None,
                    engine: Engine::MySql,
                }
            })
            .collect();
        let routines: Vec<Row> = conn
            .exec(
                "SELECT r.ROUTINE_NAME, r.ROUTINE_COMMENT,
                        (SELECT GROUP_CONCAT(
                                    CONCAT_WS(' ', IF(r.ROUTINE_TYPE = 'PROCEDURE', p.PARAMETER_MODE, NULL),
                                              p.PARAMETER_NAME, p.DTD_IDENTIFIER)
                                    ORDER BY p.ORDINAL_POSITION SEPARATOR ', ')
                         FROM information_schema.PARAMETERS p
                         WHERE p.SPECIFIC_SCHEMA = r.ROUTINE_SCHEMA AND p.SPECIFIC_NAME = r.SPECIFIC_NAME
                           AND p.ORDINAL_POSITION > 0)
                 FROM information_schema.ROUTINES r WHERE r.ROUTINE_SCHEMA = ? ORDER BY r.ROUTINE_NAME",
                (schema,),
            )
            .await
            .map_err(my)?;
        out.extend(routines.iter().map(|r| DbObject {
            schema: schema.to_string(),
            name: string(r, 0),
            kind: ObjectKind::Function,
            rows_estimate: None,
            comment: text(r, 1).filter(|c| !c.is_empty()),
            signature: Some(string(r, 2)),
            engine: Engine::MySql,
        }));
        Ok(out)
    }

    /// Every table and view with its columns, and the foreign keys between
    /// them: what the schema map draws.
    pub async fn schema_map(&self, schema: &str) -> anyhow::Result<(Vec<MapTable>, Vec<ForeignKey>)> {
        let mut conn = self.conn().await?;
        let rows: Vec<Row> = conn
            .exec(
                "SELECT c.TABLE_NAME, t.TABLE_TYPE, t.TABLE_ROWS, c.COLUMN_NAME, c.COLUMN_TYPE,
                        c.IS_NULLABLE = 'YES', c.COLUMN_KEY = 'PRI'
                 FROM information_schema.COLUMNS c
                 JOIN information_schema.TABLES t ON t.TABLE_SCHEMA = c.TABLE_SCHEMA AND t.TABLE_NAME = c.TABLE_NAME
                 WHERE c.TABLE_SCHEMA = ?
                 ORDER BY c.TABLE_NAME, c.ORDINAL_POSITION",
                (schema,),
            )
            .await
            .map_err(my)?;
        let mut tables: Vec<MapTable> = vec![];
        for r in &rows {
            let name = string(r, 0);
            if tables.last().is_none_or(|t| t.name != name) {
                let kind = relation_kind(&string(r, 1));
                tables.push(MapTable {
                    name,
                    kind,
                    rows_estimate: (kind == ObjectKind::Table).then(|| number(r, 2)).flatten(),
                    columns: vec![],
                });
            }
            if let Some(t) = tables.last_mut() {
                t.columns.push(MapColumn {
                    name: string(r, 3),
                    data_type: string(r, 4),
                    nullable: flag(r, 5),
                    primary_key: flag(r, 6),
                });
            }
        }
        let rows: Vec<Row> = conn
            .exec(
                "SELECT CONSTRAINT_NAME, TABLE_NAME, REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME,
                        COLUMN_NAME, REFERENCED_COLUMN_NAME
                 FROM information_schema.KEY_COLUMN_USAGE
                 WHERE TABLE_SCHEMA = ? AND REFERENCED_TABLE_NAME IS NOT NULL
                 ORDER BY TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION",
                (schema,),
            )
            .await
            .map_err(my)?;
        let mut keys: Vec<ForeignKey> = vec![];
        for r in &rows {
            let (name, from_table) = (string(r, 0), string(r, 1));
            if keys.last().is_none_or(|k| k.name != name || k.from_table != from_table) {
                keys.push(ForeignKey {
                    name,
                    from_table,
                    to_schema: string(r, 2),
                    to_table: string(r, 3),
                    from_cols: vec![],
                    to_cols: vec![],
                });
            }
            if let Some(k) = keys.last_mut() {
                k.from_cols.push(string(r, 4));
                k.to_cols.push(string(r, 5));
            }
        }
        Ok((tables, keys))
    }

    /// Columns, indexes, constraints and DDL for a table or view.
    pub async fn table_info(&self, schema: &str, name: &str) -> anyhow::Result<TableInfo> {
        let mut conn = self.conn().await?;
        let rel: Row = conn
            .exec_first(
                "SELECT TABLE_TYPE, TABLE_COMMENT, TABLE_ROWS, DATA_LENGTH + INDEX_LENGTH
                 FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                (schema, name),
            )
            .await
            .map_err(my)?
            .ok_or_else(|| anyhow!("no table or view named {schema}.{name}"))?;
        let kind = relation_kind(&string(&rel, 0));
        let table = kind == ObjectKind::Table;

        let cols: Vec<Row> = conn
            .exec(
                "SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE = 'YES', COLUMN_DEFAULT, COLUMN_KEY = 'PRI',
                        COLUMN_COMMENT, EXTRA, DATA_TYPE
                 FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
                 ORDER BY ORDINAL_POSITION",
                (schema, name),
            )
            .await
            .map_err(my)?;
        let columns = cols
            .iter()
            .map(|r| {
                let extra = string(r, 6).to_ascii_lowercase();
                let default = text(r, 3).map(|d| {
                    // MySQL 8 reports expressions as written and literals bare.
                    let expression = extra.contains("default_generated");
                    let textual = matches!(
                        string(r, 7).as_str(),
                        "char" | "varchar" | "text" | "tinytext" | "mediumtext" | "longtext" | "enum" | "set"
                    );
                    if textual && !expression && !d.starts_with('\'') { super::quote_literal(&d) } else { d }
                });
                ColumnInfo {
                    name: string(r, 0),
                    data_type: string(r, 1),
                    nullable: flag(r, 2),
                    default: default.or_else(|| extra.contains("auto_increment").then(|| "auto_increment".into())),
                    primary_key: flag(r, 4),
                    comment: text(r, 5).filter(|c| !c.is_empty()),
                }
            })
            .collect::<Vec<_>>();
        if columns.is_empty() {
            bail!("no table or view named {schema}.{name}");
        }

        let index_rows: Vec<Row> = conn
            .exec(
                "SELECT INDEX_NAME, NON_UNIQUE = 0, INDEX_TYPE,
                        GROUP_CONCAT(
                            CONCAT(COALESCE(CONCAT('`', REPLACE(COLUMN_NAME, '`', '``'), '`'), '(expression)'),
                                   IF(SUB_PART IS NULL, '', CONCAT('(', SUB_PART, ')')),
                                   IF(COLLATION = 'D', ' DESC', ''))
                            ORDER BY SEQ_IN_INDEX SEPARATOR ', ')
                 FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?
                 GROUP BY INDEX_NAME, NON_UNIQUE, INDEX_TYPE
                 ORDER BY INDEX_NAME = 'PRIMARY' DESC, INDEX_NAME",
                (schema, name),
            )
            .await
            .map_err(my)?;
        let indexes: Vec<IndexInfo> = index_rows
            .iter()
            .map(|r| {
                let (index, unique, kind, cols) = (string(r, 0), flag(r, 1), string(r, 2), string(r, 3));
                let primary = index == "PRIMARY";
                let definition = if primary {
                    format!("PRIMARY KEY ({cols})")
                } else {
                    let what = match kind.as_str() {
                        "FULLTEXT" => "FULLTEXT ",
                        "SPATIAL" => "SPATIAL ",
                        _ if unique => "UNIQUE ",
                        _ => "",
                    };
                    format!("CREATE {what}INDEX {} ON {}.{} ({cols})", quote(&index), quote(schema), quote(name))
                };
                IndexInfo { name: index, definition, unique, primary }
            })
            .collect();

        let key_rows: Vec<Row> = conn
            .exec(
                "SELECT k.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE,
                        GROUP_CONCAT(CONCAT('`', REPLACE(k.COLUMN_NAME, '`', '``'), '`') ORDER BY k.ORDINAL_POSITION SEPARATOR ', '),
                        MAX(k.REFERENCED_TABLE_SCHEMA), MAX(k.REFERENCED_TABLE_NAME),
                        GROUP_CONCAT(CONCAT('`', REPLACE(k.REFERENCED_COLUMN_NAME, '`', '``'), '`') ORDER BY k.ORDINAL_POSITION SEPARATOR ', '),
                        MAX(rc.UPDATE_RULE), MAX(rc.DELETE_RULE)
                 FROM information_schema.KEY_COLUMN_USAGE k
                 JOIN information_schema.TABLE_CONSTRAINTS tc
                   ON tc.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND tc.TABLE_NAME = k.TABLE_NAME
                  AND tc.CONSTRAINT_NAME = k.CONSTRAINT_NAME
                 LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS rc
                   ON rc.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND rc.TABLE_NAME = k.TABLE_NAME
                  AND rc.CONSTRAINT_NAME = k.CONSTRAINT_NAME
                 WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ?
                 GROUP BY k.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE
                 ORDER BY FIELD(tc.CONSTRAINT_TYPE, 'PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY'), k.CONSTRAINT_NAME",
                (schema, name),
            )
            .await
            .map_err(my)?;
        let mut constraints: Vec<ConstraintInfo> = key_rows
            .iter()
            .map(|r| {
                let (cname, ctype, cols) = (string(r, 0), string(r, 1), string(r, 2));
                let (kind, definition) = match ctype.as_str() {
                    "PRIMARY KEY" => ("p", format!("PRIMARY KEY ({cols})")),
                    "UNIQUE" => ("u", format!("UNIQUE ({cols})")),
                    _ => {
                        let mut d = format!(
                            "FOREIGN KEY ({cols}) REFERENCES {}.{} ({})",
                            quote(&string(r, 3)),
                            quote(&string(r, 4)),
                            string(r, 5)
                        );
                        for (rule, at) in [("ON UPDATE", 6), ("ON DELETE", 7)] {
                            let action = string(r, at);
                            if !action.is_empty() && action != "NO ACTION" && action != "RESTRICT" {
                                d.push_str(&format!(" {rule} {action}"));
                            }
                        }
                        ("f", d)
                    }
                };
                ConstraintInfo { name: cname, kind: kind.into(), definition }
            })
            .collect();
        // CHECK constraints: MySQL 8.0.16 and MariaDB 10.2 on; older servers
        // have no table to ask, and no checks.
        let checks: Result<Vec<Row>, _> = conn
            .exec(
                "SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE
                 FROM information_schema.CHECK_CONSTRAINTS cc
                 JOIN information_schema.TABLE_CONSTRAINTS tc
                   ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME
                 WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'CHECK'
                 ORDER BY cc.CONSTRAINT_NAME",
                (schema, name),
            )
            .await;
        if let Ok(checks) = checks {
            constraints.extend(checks.iter().map(|r| ConstraintInfo {
                name: string(r, 0),
                kind: "c".into(),
                definition: format!("CHECK {}", string(r, 1)),
            }));
        }

        let qualified = format!("{}.{}", quote(schema), quote(name));
        let ddl: Option<Row> = conn.query_first(format!("SHOW CREATE TABLE {qualified}")).await.map_err(my)?;
        let ddl = ddl.and_then(|r| text(&r, 1)).map(|d| format!("{d};")).unwrap_or_default();

        Ok(TableInfo {
            schema: schema.to_string(),
            name: name.to_string(),
            kind,
            comment: text(&rel, 1).filter(|c| !c.is_empty() && c != "VIEW"),
            rows_estimate: table.then(|| number(&rel, 2)).flatten(),
            total_size: table.then(|| number(&rel, 3)).flatten().map(pretty_size),
            columns,
            indexes,
            constraints,
            ddl,
        })
    }

    /// The `CREATE FUNCTION` or `CREATE PROCEDURE` statement for a routine.
    pub async fn function_ddl(&self, schema: &str, name: &str) -> anyhow::Result<String> {
        let mut conn = self.conn().await?;
        let kind: Option<String> = conn
            .exec_first(
                "SELECT ROUTINE_TYPE FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA = ? AND ROUTINE_NAME = ?",
                (schema, name),
            )
            .await
            .map_err(my)?;
        let kind = kind.ok_or_else(|| anyhow!("no function or procedure named {schema}.{name}"))?;
        let what = if kind == "PROCEDURE" { "PROCEDURE" } else { "FUNCTION" };
        let row: Option<Row> = conn
            .query_first(format!("SHOW CREATE {what} {}.{}", quote(schema), quote(name)))
            .await
            .map_err(my)?;
        row.and_then(|r| text(&r, 2))
            .ok_or_else(|| anyhow!("the server didn't show the definition of {schema}.{name}; it may need more privileges"))
    }

    /// Apply row edits in one transaction: all of them, or none. An update
    /// or delete that doesn't hit exactly one row stops and rolls back. Runs
    /// off the work connection, so it never joins a transaction the user
    /// opened in a query tab. Returns how many rows changed.
    pub async fn apply(&self, target: &EditTarget, changes: &[Change]) -> anyhow::Result<u64> {
        if changes.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn().await?;
        conn.query_drop("START TRANSACTION").await.map_err(my)?;
        let mut total = 0;
        for (i, change) in changes.iter().enumerate() {
            let fail = |why: String| anyhow!("change {} of {} failed, nothing was saved: {why}\n{}", i + 1, changes.len(), target.preview(change));
            let (sql, cells) = target.bound(change);
            let mut params = Vec::with_capacity(cells.len());
            for (column, value) in &cells {
                params.push(match value {
                    None => Value::NULL,
                    Some(v) if target.declared(column).is_some_and(is_binary_type) => {
                        Value::Bytes(parse_hex(v).map_err(|e| fail(e.to_string()))?)
                    }
                    Some(v) => Value::Bytes(v.clone().into_bytes()),
                });
            }
            let done = match conn.exec_drop(sql.as_str(), params).await {
                Ok(()) => conn.affected_rows(),
                Err(e) => {
                    rollback(&mut conn).await;
                    return Err(fail(my(e).to_string()));
                }
            };
            if done != 1 && !matches!(change, Change::Insert { .. }) {
                rollback(&mut conn).await;
                bail!(
                    "change {} of {} matched {done} rows instead of 1 (the row may have changed or gone), nothing was saved:\n{}",
                    i + 1,
                    changes.len(),
                    target.preview(change)
                );
            }
            total += done;
        }
        conn.query_drop("COMMIT").await.map_err(my)?;
        Ok(total)
    }

    /// Run `sql` (any number of statements) on the work connection.
    pub async fn run(&self, sql: &str, max_rows: usize) -> anyhow::Result<QueryOutcome> {
        let mut work = self.work.lock().await;
        run_on(&mut work, sql, max_rows).await
    }

    /// Run one statement in a read-only transaction with a timeout. For AI
    /// agents: a write fails, and so does anything that is not exactly one
    /// statement.
    ///
    /// A read-only transaction refuses every change to data, but DDL commits
    /// it first and then runs; so only statements that read are let in.
    pub async fn run_read_only(&self, sql: &str, max_rows: usize, timeout: Duration) -> anyhow::Result<QueryOutcome> {
        let statement = one_statement(sql)?;
        check_reads_only(statement)?;
        let mut conn = self.conn().await?;
        conn.query_drop("START TRANSACTION READ ONLY").await.map_err(my)?;
        let out = self.timed(&mut conn, statement, max_rows, timeout).await;
        rollback(&mut conn).await;
        out
    }

    /// Run `sql` off the work connection with a timeout, for AI agents with
    /// write access.
    pub async fn run_write(&self, sql: &str, max_rows: usize, timeout: Duration) -> anyhow::Result<QueryOutcome> {
        let mut conn = self.conn().await?;
        let out = self.timed(&mut conn, sql, max_rows, timeout).await;
        // A transaction left open would hold its locks for as long as the
        // pooled connection lives.
        if in_transaction(&mut conn).await {
            rollback(&mut conn).await;
            if out.is_ok() {
                bail!("the transaction was left open, so it was rolled back; agents run one statement at a time");
            }
        }
        out
    }

    async fn timed(&self, conn: &mut Conn, sql: &str, max_rows: usize, timeout: Duration) -> anyhow::Result<QueryOutcome> {
        let id = conn.id();
        match tokio::time::timeout(timeout, run_on(conn, sql, max_rows)).await {
            Ok(out) => out,
            Err(_) => {
                kill_query(&self.pool, id).await?;
                // Read what's left of the interrupted result, so the
                // connection is fit for the next caller.
                let _ = conn.query_drop("DO 0").await;
                bail!("the query timed out after {:.1}s and was stopped", timeout.as_secs_f64())
            }
        }
    }

    /// The plan as text. With `analyze` (MySQL 8.0.18 on), the statement runs
    /// read-only and the plan has actual timings.
    pub async fn plan(&self, sql: &str, analyze: bool, timeout: Duration) -> anyhow::Result<String> {
        let sql = sql.trim().trim_end_matches(';');
        let tree = if analyze { format!("EXPLAIN ANALYZE {sql}") } else { format!("EXPLAIN FORMAT=TREE {sql}") };
        match self.run_read_only(&tree, 2_000, timeout).await {
            Ok(out) => Ok(first_column(&out)),
            Err(e) if super::needs_write_access(&e) => Err(e),
            // MariaDB and older MySQL: the classic table.
            Err(_) => {
                let out = self.run_read_only(&format!("EXPLAIN {sql}"), 2_000, timeout).await?;
                Ok(out.main().map(|r| super::to_tsv(&r.columns, &r.rows)).unwrap_or_default())
            }
        }
    }
}

/// Whether `conn` has a transaction open. MySQL has no variable that says
/// so (MariaDB's `@@in_transaction` is its own), but it refuses to change
/// the next transaction's characteristics while one is open.
async fn in_transaction(conn: &mut Conn) -> bool {
    const CANT_CHANGE_TX_CHARACTERISTICS: u16 = 1568;
    matches!(
        conn.query_drop("SET TRANSACTION READ WRITE").await,
        Err(mysql_async::Error::Server(e)) if e.code == CANT_CHANGE_TX_CHARACTERISTICS
    )
}

async fn rollback(conn: &mut Conn) {
    if let Err(e) = conn.query_drop("ROLLBACK").await {
        eprintln!("tabula: ROLLBACK failed: {e}");
    }
}

fn first_column(out: &QueryOutcome) -> String {
    out.main()
        .map(|r| r.rows.iter().filter_map(|row| row.first().cloned().flatten()).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

fn relation_kind(table_type: &str) -> ObjectKind {
    match table_type {
        "VIEW" | "SYSTEM VIEW" => ObjectKind::View,
        "SEQUENCE" => ObjectKind::Sequence,
        _ => ObjectKind::Table,
    }
}

/// The user's databases by name, then the server's own.
fn sort_databases(mut names: Vec<String>) -> Vec<String> {
    names.sort_by_key(|n| (SYSTEM_DATABASES.contains(&n.to_ascii_lowercase().as_str()), n.clone()));
    names
}

/// The one statement in `sql`, or why there isn't one.
pub(super) fn one_statement(sql: &str) -> anyhow::Result<&str> {
    match script::statements(sql, Engine::MySql)[..] {
        [one] => Ok(one),
        [] => bail!("there is no statement to run"),
        _ => bail!("agents run one statement at a time"),
    }
}

/// Statements that only read. Anything else is refused as needing write
/// access, so Tabula can ask the user to approve it.
fn check_reads_only(statement: &str) -> anyhow::Result<()> {
    let words = script::words(statement, Engine::MySql);
    let first = words.first().map(String::as_str).unwrap_or("");
    let reads = matches!(first, "SELECT" | "WITH" | "SHOW" | "DESC" | "DESCRIBE" | "EXPLAIN" | "TABLE" | "VALUES");
    // `SELECT … INTO OUTFILE` writes a file on the server.
    let to_file = words.iter().any(|w| w == "OUTFILE" || w == "DUMPFILE");
    if !reads || to_file {
        return Err(anyhow::Error::new(WriteRefused(format!(
            "{} isn't allowed in read-only mode: agents may run SELECT, WITH, SHOW, DESCRIBE, EXPLAIN, TABLE and VALUES",
            if to_file { "writing to a file" } else if first.is_empty() { "this statement" } else { first }
        ))));
    }
    Ok(())
}

/// Bytes the way people read them: `16 kB`, `3.2 MB`.
pub(super) fn pretty_size(bytes: i64) -> String {
    let units = ["bytes", "kB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < units.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 || size >= 10.0 { format!("{size:.0} {}", units[unit]) } else { format!("{size:.1} {}", units[unit]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_reads_get_through() {
        for ok in ["select 1", "WITH x AS (SELECT 1) SELECT * FROM x", "show tables", "describe t", "explain select 1", "(select 1)"] {
            assert!(check_reads_only(ok).is_ok(), "{ok}");
        }
        for (bad, why) in [
            ("create table t (x int)", "CREATE"),
            ("drop table orders", "DROP"),
            ("set autocommit = 1", "SET"),
            ("commit", "COMMIT"),
            ("select * from t into outfile '/tmp/x'", "file"),
            ("/* hi */ delete from t", "DELETE"),
        ] {
            let err = check_reads_only(bad).unwrap_err();
            assert!(super::super::needs_write_access(&err) && err.to_string().contains(why), "{bad}: {err}");
        }
        assert!(one_statement("select 1; delete from t").unwrap_err().to_string().contains("one statement"));
        assert_eq!(one_statement("select ';' ; -- done").unwrap(), "select ';'");
    }

    #[test]
    fn sizes_read_like_postgres() {
        assert_eq!(pretty_size(512), "512 bytes");
        assert_eq!(pretty_size(16_384), "16 kB");
        assert_eq!(pretty_size(3_500_000), "3.3 MB");
    }
}
