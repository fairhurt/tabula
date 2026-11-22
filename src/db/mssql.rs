//! The SQL Server backend (Azure SQL too), on tiberius.
//!
//! A session holds a connection for the catalog and row edits, one for the
//! user's queries, and one for AI agents, opened the first time an agent
//! asks. Scripts split at `GO` lines into batches, as SSMS does.
//!
//! Stop and agent timeouts close the connection running the query, which
//! makes the server abandon it (and roll back anything left open); the next
//! query connects again. tiberius's attention signal would keep the session,
//! but it loses track of the reply and leaves the connection unusable.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use futures::TryStreamExt as _;
use tiberius::{AuthMethod, Client, ColumnData, ColumnType, Config, EncryptionLevel, FromSql as _, QueryItem, Row};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt as _};

use super::script;
use super::{
    Change, ColumnInfo, ConstraintInfo, DbObject, EditTarget, ForeignKey, IndexInfo, MapColumn, MapTable,
    ObjectKind, QueryOutcome, ResultColumn, ResultSet, TableInfo, WriteRefused, hex, is_binary_type,
};
use crate::profiles::{Engine, Profile, SslMode};

type Conn = Client<Compat<TcpStream>>;

/// Bytes shown of a binary value before it is cut short.
const MAX_BLOB: usize = 256;

/// What SSMS sets on every connection; indexed views, filtered indexes and
/// computed columns need most of them.
const SESSION_SETUP: &str = "SET ANSI_NULLS ON; SET ANSI_PADDING ON; SET ANSI_WARNINGS ON; SET ARITHABORT ON; \
     SET CONCAT_NULL_YIELDS_NULL ON; SET QUOTED_IDENTIFIER ON; SET NUMERIC_ROUNDABORT OFF; SET XACT_ABORT ON; \
     SET TEXTSIZE 2147483647";

fn quote(name: &str) -> String {
    Engine::SqlServer.quote_ident(name)
}

/// An error as SSMS shows it: `Msg 208, Level 16, State 1, Line 1`.
fn ms(e: tiberius::error::Error) -> anyhow::Error {
    match e {
        tiberius::error::Error::Server(t) => anyhow!(
            "Msg {}, Level {}, State {}, Line {}\n{}",
            t.code(),
            t.class(),
            t.state(),
            t.line(),
            t.message()
        ),
        e => anyhow::Error::new(Broken(e.to_string())),
    }
}

/// Where and how to connect.
fn config(profile: &Profile, password: Option<&str>) -> Config {
    let mut c = Config::new();
    // `host\INSTANCE` names an instance; the SQL Server Browser has its port.
    match profile.host.split_once('\\') {
        Some((host, instance)) => {
            c.host(if host.is_empty() || host == "." { "localhost" } else { host });
            c.instance_name(instance);
        }
        None => c.host(&profile.host),
    }
    c.port(profile.port);
    if !profile.database.is_empty() {
        c.database(&profile.database);
    }
    c.application_name("tabula");
    c.authentication(auth(&profile.user, password));
    // As with PostgreSQL: `prefer` and `require` encrypt without checking
    // the certificate; `verify` checks it against this computer's roots.
    // `disable` still encrypts the login, so the password never goes in
    // the clear.
    c.encryption(match profile.ssl {
        SslMode::Disable => EncryptionLevel::Off,
        SslMode::Prefer => EncryptionLevel::On,
        SslMode::Require | SslMode::Verify => EncryptionLevel::Required,
    });
    if profile.ssl != SslMode::Verify {
        c.trust_cert();
    }
    c
}

/// A SQL login, or on Windows with no user name, the signed-in Windows user.
fn auth(user: &str, password: Option<&str>) -> AuthMethod {
    #[cfg(windows)]
    if user.is_empty() {
        return AuthMethod::Integrated;
    }
    AuthMethod::sql_server(user, password.unwrap_or(""))
}

async fn open(config: &Config) -> anyhow::Result<Conn> {
    let connect = async {
        let mut config = config.clone();
        // Azure SQL's gateway may send us on to the server that has the
        // database; follow it once.
        for _ in 0..2 {
            let tcp = if config_has_instance(&config) {
                use tiberius::SqlBrowser as _;
                TcpStream::connect_named(&config).await.map_err(ms)?
            } else {
                TcpStream::connect(config.get_addr()).await?
            };
            tcp.set_nodelay(true)?;
            match Client::connect(config.clone(), tcp.compat_write()).await {
                Ok(client) => return Ok(client),
                Err(tiberius::error::Error::Routing { host, port }) => {
                    config.host(&host);
                    config.port(port);
                }
                Err(e) => return Err(ms(e)),
            }
        }
        bail!("the server redirected the connection more than once")
    };
    let mut client = match tokio::time::timeout(Duration::from_secs(10), connect).await {
        Ok(client) => client?,
        Err(_) => bail!("timed out connecting to the server"),
    };
    client.simple_query(SESSION_SETUP).await.map_err(ms)?.into_results().await.map_err(ms)?;
    Ok(client)
}

fn config_has_instance(config: &Config) -> bool {
    // tiberius keeps the instance to itself; `get_addr` is all it shows.
    format!("{config:?}").contains("instance_name: Some")
}

/// One value as text. Dates read as ISO 8601, bytes as `0x…` hex, a GUID in
/// upper case: how SSMS shows them.
fn cell(data: ColumnData<'static>) -> Option<String> {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    let stamp = |dt: NaiveDateTime, digits: u8| {
        let mut s = dt.format("%Y-%m-%d %H:%M:%S").to_string();
        push_fraction(&mut s, dt.and_utc().timestamp_subsec_nanos(), digits);
        s
    };
    Some(match data {
        ColumnData::U8(v) => v?.to_string(),
        ColumnData::I16(v) => v?.to_string(),
        ColumnData::I32(v) => v?.to_string(),
        ColumnData::I64(v) => v?.to_string(),
        ColumnData::F32(v) => v?.to_string(),
        ColumnData::F64(v) => v?.to_string(),
        ColumnData::Bit(v) => if v? { "1" } else { "0" }.to_string(),
        ColumnData::String(v) => v?.into_owned(),
        ColumnData::Guid(v) => v?.to_string().to_ascii_uppercase(),
        ColumnData::Binary(v) => hex(&v?, MAX_BLOB),
        ColumnData::Numeric(v) => v?.to_string(),
        ColumnData::Xml(v) => v?.to_string(),
        ColumnData::DateTime(None)
        | ColumnData::SmallDateTime(None)
        | ColumnData::Date(None)
        | ColumnData::Time(None)
        | ColumnData::DateTime2(None)
        | ColumnData::DateTimeOffset(None) => return None,
        ref d @ ColumnData::DateTime(_) => stamp(NaiveDateTime::from_sql(d).ok()??, 3),
        ref d @ ColumnData::SmallDateTime(_) => stamp(NaiveDateTime::from_sql(d).ok()??, 0),
        ref d @ ColumnData::DateTime2(Some(v)) => stamp(NaiveDateTime::from_sql(d).ok()??, v.time().scale()),
        ref d @ ColumnData::Date(_) => NaiveDate::from_sql(d).ok()??.format("%Y-%m-%d").to_string(),
        ref d @ ColumnData::Time(Some(v)) => {
            let t = NaiveTime::from_sql(d).ok()??;
            let mut s = t.format("%H:%M:%S").to_string();
            push_fraction(&mut s, chrono::Timelike::nanosecond(&t), v.scale());
            s
        }
        ref d @ ColumnData::DateTimeOffset(Some(v)) => {
            let dt = DateTime::<FixedOffset>::from_sql(d).ok()??;
            let mut s = dt.format("%Y-%m-%d %H:%M:%S").to_string();
            push_fraction(&mut s, dt.timestamp_subsec_nanos(), v.datetime2().time().scale());
            s.push_str(&dt.format(" %:z").to_string());
            s
        }
    })
}

/// `.1234567`: the fraction of a second to the column's precision.
fn push_fraction(s: &mut String, nanos: u32, digits: u8) {
    if digits > 0 {
        let all = format!("{nanos:09}");
        s.push('.');
        s.push_str(&all[..usize::from(digits.min(9))]);
    }
}

fn result_column(c: &tiberius::Column) -> ResultColumn {
    use ColumnType::*;
    let t = c.column_type();
    let (name, numeric) = match t {
        Bit | Bitn => ("bit", false),
        Int1 => ("tinyint", true),
        Int2 => ("smallint", true),
        Int4 => ("int", true),
        Int8 => ("bigint", true),
        Intn => ("int", true),
        Float4 => ("real", true),
        Float8 | Floatn => ("float", true),
        Money | Money4 => ("money", true),
        Decimaln => ("decimal", true),
        Numericn => ("numeric", true),
        Datetime | Datetimen => ("datetime", false),
        Datetime4 => ("smalldatetime", false),
        Datetime2 => ("datetime2", false),
        Daten => ("date", false),
        Timen => ("time", false),
        DatetimeOffsetn => ("datetimeoffset", false),
        Guid => ("uniqueidentifier", false),
        BigVarChar => ("varchar", false),
        BigChar => ("char", false),
        NVarchar => ("nvarchar", false),
        NChar => ("nchar", false),
        BigVarBin => ("varbinary", false),
        BigBinary => ("binary", false),
        Text => ("text", false),
        NText => ("ntext", false),
        Image => ("image", false),
        Xml => ("xml", false),
        SSVariant => ("sql_variant", false),
        _ => ("", false),
    };
    ResultColumn {
        name: c.name().to_string(),
        type_name: (!name.is_empty()).then(|| name.to_string()),
        numeric,
    }
}

/// Run one batch, adding its results to `results`. A batch that returns no
/// rows reports how many it changed.
async fn run_batch(conn: &mut Conn, batch: &str, max_rows: usize, results: &mut Vec<ResultSet>) -> anyhow::Result<()> {
    let mut stream = conn.simple_query(batch).await.map_err(ms)?;
    let before = results.len();
    while let Some(item) = stream.try_next().await.map_err(ms)? {
        match item {
            QueryItem::Metadata(meta) => results.push(ResultSet {
                columns: meta.columns().iter().map(result_column).collect(),
                ..ResultSet::default()
            }),
            QueryItem::Row(row) => {
                let Some(set) = results.last_mut() else { continue };
                set.row_count += 1;
                if set.rows.len() < max_rows {
                    set.rows.push(row.into_iter().map(cell).collect());
                } else {
                    set.truncated = true;
                }
            }
        }
    }
    drop(stream);
    if results.len() == before {
        // tiberius keeps the server's row counts to itself, but @@ROWCOUNT
        // outlives the batch: it is the last statement's count.
        let count = conn.simple_query("SELECT @@ROWCOUNT").await.map_err(ms)?.into_row().await.map_err(ms)?;
        // Under SET SHOWPLAN_TEXT the answer is a plan instead; count nothing.
        let row_count = count.and_then(|r| number(&r, 0)).map_or(0, |n| u64::try_from(n).unwrap_or(0));
        results.push(ResultSet { row_count, ..ResultSet::default() });
    }
    Ok(())
}

/// Run every batch of `sql` on the connection in `slot`, connecting first
/// if it is empty. `stop` ends it early: the connection is closed, so the
/// server gives up on the query, and `stopped` says why. A connection that
/// breaks is let go too, so the next run starts afresh.
async fn run_script(
    slot: &mut Option<Conn>,
    config: &Config,
    sql: &str,
    max_rows: usize,
    stop: impl std::future::Future<Output = ()>,
    stopped: &str,
) -> anyhow::Result<QueryOutcome> {
    let started = Instant::now();
    if slot.is_none() {
        *slot = Some(open(config).await?);
    }
    let mut results = vec![];
    tokio::pin!(stop);
    for batch in script::batches(sql) {
        let Some(conn) = slot.as_mut() else { break };
        let done = tokio::select! {
            r = run_batch(conn, batch, max_rows, &mut results) => Some(r),
            () = &mut stop => None,
        };
        match done {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                if e.downcast_ref::<Broken>().is_some() {
                    *slot = None;
                }
                return Err(e);
            }
            None => {
                *slot = None;
                bail!("{stopped}");
            }
        }
    }
    Ok(QueryOutcome { results, elapsed: started.elapsed() })
}

/// The connection itself failed (not just the statement), so it can't be
/// used again.
#[derive(Debug)]
struct Broken(String);

impl std::fmt::Display for Broken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Broken {}

/// Stops the query running on a session's work connection.
pub struct Stop(Arc<Notify>);

impl Stop {
    pub fn stop(&self) {
        self.0.notify_waiters();
    }
}

/// Catalog queries, with `@P1`… bound.
async fn rows(conn: &mut Conn, sql: &str, params: &[&str]) -> anyhow::Result<Vec<Row>> {
    let params: Vec<&dyn tiberius::ToSql> = params.iter().map(|p| p as &dyn tiberius::ToSql).collect();
    conn.query(sql, &params).await.map_err(ms)?.into_first_result().await.map_err(ms)
}

fn text(row: &Row, i: usize) -> Option<String> {
    row.cells().nth(i).and_then(|(_, d)| cell(d.clone()))
}

fn string(row: &Row, i: usize) -> String {
    text(row, i).unwrap_or_default()
}

fn number(row: &Row, i: usize) -> Option<i64> {
    text(row, i)?.parse().ok()
}

fn flag(row: &Row, i: usize) -> bool {
    matches!(text(row, i).as_deref(), Some("1"))
}

/// A column's type as it is declared: `nvarchar(50)`, `decimal(10,2)`.
/// Expects `sys.columns c` joined to `sys.types ty`.
const TYPE_SQL: &str = "CASE
    WHEN ty.name IN ('varchar', 'char', 'varbinary', 'binary')
        THEN ty.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length AS varchar(10)) END + ')'
    WHEN ty.name IN ('nvarchar', 'nchar')
        THEN ty.name + '(' + CASE WHEN c.max_length = -1 THEN 'max' ELSE CAST(c.max_length / 2 AS varchar(10)) END + ')'
    WHEN ty.name IN ('decimal', 'numeric')
        THEN ty.name + '(' + CAST(c.precision AS varchar(3)) + ',' + CAST(c.scale AS varchar(3)) + ')'
    WHEN ty.name IN ('datetime2', 'time', 'datetimeoffset') AND c.scale <> 7
        THEN ty.name + '(' + CAST(c.scale AS varchar(3)) + ')'
    ELSE ty.name END";

/// The object types the tree shows, and what Tabula calls them.
fn object_kind(code: &str) -> Option<ObjectKind> {
    Some(match code.trim() {
        "U" => ObjectKind::Table,
        "V" => ObjectKind::View,
        "FN" | "IF" | "TF" | "FS" | "FT" | "P" | "PC" => ObjectKind::Function,
        "SO" => ObjectKind::Sequence,
        _ => return None,
    })
}

pub struct Backend {
    config: Config,
    /// The catalog and row edits.
    meta: Mutex<Conn>,
    /// The user's queries; empty after Stop, until the next query.
    work: Mutex<Option<Conn>>,
    /// AI agents, connected when first needed.
    agent: Mutex<Option<Conn>>,
    stop: Arc<Notify>,
}

impl Backend {
    /// Connect, and report the server's version. With no database in the
    /// profile, the login's default database is used and written back.
    pub async fn connect(profile: &mut Profile, password: Option<&str>) -> anyhow::Result<(Backend, String)> {
        let mut config = config(profile, password);
        let (mut meta, work) = tokio::try_join!(open(&config), open(&config))?;
        let row = meta
            .simple_query(
                "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), DB_NAME(),
                        CAST(SERVERPROPERTY('Edition') AS nvarchar(128))",
            )
            .await
            .map_err(ms)?
            .into_row()
            .await
            .map_err(ms)?
            .ok_or_else(|| anyhow!("the server didn't say which version it is"))?;
        let (version, database, edition) = (string(&row, 0), string(&row, 1), string(&row, 2));
        if profile.database.is_empty() {
            profile.database = database;
            config.database(&profile.database);
        }
        let version = if edition.is_empty() { version } else { format!("{version} ({edition})") };
        let backend = Backend {
            config,
            meta: Mutex::new(meta),
            work: Mutex::new(Some(work)),
            agent: Mutex::new(None),
            stop: Arc::new(Notify::new()),
        };
        Ok((backend, version))
    }

    pub fn stop_handle(&self) -> Stop {
        Stop(self.stop.clone())
    }

    pub async fn databases(&self) -> anyhow::Result<Vec<String>> {
        let mut meta = self.meta.lock().await;
        let rows = rows(
            &mut meta,
            "SELECT name FROM sys.databases WHERE state = 0 AND HAS_DBACCESS(name) = 1
             ORDER BY CASE WHEN database_id <= 4 THEN 1 ELSE 0 END, name",
            &[],
        )
        .await?;
        Ok(rows.iter().map(|r| string(r, 0)).collect())
    }

    /// The database's schemas: the user's, then `sys` and
    /// `INFORMATION_SCHEMA`. The fixed roles' empty schemas are left out.
    pub async fn schemas(&self) -> anyhow::Result<Vec<String>> {
        let mut meta = self.meta.lock().await;
        let rows = rows(
            &mut meta,
            "SELECT s.name FROM sys.schemas s
             WHERE s.schema_id < 16384 AND (s.name <> 'guest' OR EXISTS (SELECT 1 FROM sys.objects o WHERE o.schema_id = s.schema_id))
             ORDER BY CASE WHEN s.name IN ('sys', 'INFORMATION_SCHEMA') THEN 1 ELSE 0 END,
                      CASE WHEN s.name = 'dbo' THEN 0 ELSE 1 END, s.name",
            &[],
        )
        .await?;
        Ok(rows.iter().map(|r| string(r, 0)).collect())
    }

    pub async fn objects(&self, schema: &str) -> anyhow::Result<Vec<DbObject>> {
        let mut meta = self.meta.lock().await;
        let found = rows(
            &mut meta,
            "SELECT o.name, o.type,
                    CASE WHEN o.type = 'U' THEN
                        (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1))
                    END,
                    CAST(ep.value AS nvarchar(4000)),
                    CASE WHEN o.type IN ('FN', 'IF', 'TF', 'FS', 'FT', 'P', 'PC') THEN
                        ISNULL(STUFF((SELECT ', ' + pr.name + ' ' + TYPE_NAME(pr.user_type_id)
                                             + CASE WHEN pr.is_output = 1 THEN ' OUTPUT' ELSE '' END
                                      FROM sys.parameters pr WHERE pr.object_id = o.object_id AND pr.parameter_id > 0
                                      ORDER BY pr.parameter_id
                                      FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, ''), '')
                    END
             FROM sys.all_objects o
             JOIN sys.schemas s ON s.schema_id = o.schema_id
             LEFT JOIN sys.extended_properties ep
               ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = 'MS_Description'
             WHERE s.name = @P1 AND o.type IN ('U', 'V', 'FN', 'IF', 'TF', 'FS', 'FT', 'P', 'PC', 'SO')
               AND (o.is_ms_shipped = 0 OR s.name IN ('sys', 'INFORMATION_SCHEMA'))
             ORDER BY o.name",
            &[schema],
        )
        .await?;
        Ok(found
            .iter()
            .filter_map(|r| {
                let kind = object_kind(&string(r, 1))?;
                Some(DbObject {
                    schema: schema.to_string(),
                    name: string(r, 0),
                    kind,
                    rows_estimate: number(r, 2),
                    comment: text(r, 3),
                    signature: text(r, 4),
                    engine: Engine::SqlServer,
                })
            })
            .collect())
    }

    /// Every table and view with its columns, and the foreign keys between
    /// them: what the schema map draws.
    pub async fn schema_map(&self, schema: &str) -> anyhow::Result<(Vec<MapTable>, Vec<ForeignKey>)> {
        let mut meta = self.meta.lock().await;
        let found = rows(
            &mut meta,
            &format!(
                "SELECT o.name, o.type,
                        CASE WHEN o.type = 'U' THEN
                            (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1))
                        END,
                        c.name, {TYPE_SQL}, c.is_nullable,
                        CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i
                                          JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
                                          WHERE i.object_id = o.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id)
                             THEN 1 ELSE 0 END
                 FROM sys.all_objects o
                 JOIN sys.schemas s ON s.schema_id = o.schema_id
                 JOIN sys.all_columns c ON c.object_id = o.object_id
                 JOIN sys.types ty ON ty.user_type_id = c.user_type_id
                 WHERE s.name = @P1 AND o.type IN ('U', 'V')
                   AND (o.is_ms_shipped = 0 OR s.name IN ('sys', 'INFORMATION_SCHEMA'))
                 ORDER BY o.name, c.column_id"
            ),
            &[schema],
        )
        .await?;
        let mut tables: Vec<MapTable> = vec![];
        for r in &found {
            let name = string(r, 0);
            if tables.last().is_none_or(|t| t.name != name) {
                tables.push(MapTable {
                    name,
                    kind: object_kind(&string(r, 1)).unwrap_or(ObjectKind::Table),
                    rows_estimate: number(r, 2),
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
        let found = rows(
            &mut meta,
            "SELECT fk.name, tp.name, rs.name, rt.name, pc.name, rc.name
             FROM sys.foreign_keys fk
             JOIN sys.tables tp ON tp.object_id = fk.parent_object_id
             JOIN sys.schemas s ON s.schema_id = tp.schema_id
             JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id
             JOIN sys.schemas rs ON rs.schema_id = rt.schema_id
             JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id
             JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id
             JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
             WHERE s.name = @P1
             ORDER BY tp.name, fk.name, fkc.constraint_column_id",
            &[schema],
        )
        .await?;
        let mut keys: Vec<ForeignKey> = vec![];
        for r in &found {
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
        let mut meta = self.meta.lock().await;
        let qualified = format!("{}.{}", quote(schema), quote(name));
        let rel = rows(
            &mut meta,
            "SELECT o.object_id, o.type, CAST(ep.value AS nvarchar(4000)),
                    (SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)),
                    (SELECT SUM(a.total_pages) * 8192 FROM sys.partitions p
                     JOIN sys.allocation_units a ON a.container_id IN (p.hobt_id, p.partition_id)
                     WHERE p.object_id = o.object_id),
                    OBJECT_DEFINITION(o.object_id)
             FROM sys.all_objects o
             JOIN sys.schemas s ON s.schema_id = o.schema_id
             LEFT JOIN sys.extended_properties ep
               ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = 'MS_Description'
             WHERE s.name = @P1 AND o.name = @P2 AND o.type IN ('U', 'V')",
            &[schema, name],
        )
        .await?;
        let rel = rel.first().ok_or_else(|| anyhow!("no table or view named {schema}.{name}"))?;
        let id = string(rel, 0);
        let id = id.as_str();
        let kind = object_kind(&string(rel, 1)).unwrap_or(ObjectKind::Table);
        let table = kind == ObjectKind::Table;

        let cols = rows(
            &mut meta,
            &format!(
                "SELECT c.name, {TYPE_SQL}, c.is_nullable, dc.definition,
                        CASE WHEN EXISTS (SELECT 1 FROM sys.indexes i
                                          JOIN sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id
                                          WHERE i.object_id = c.object_id AND i.is_primary_key = 1 AND ic.column_id = c.column_id)
                             THEN 1 ELSE 0 END,
                        CAST(ep.value AS nvarchar(4000)),
                        CASE WHEN ic.column_id IS NOT NULL
                             THEN 'IDENTITY(' + CAST(ic.seed_value AS nvarchar(40)) + ',' + CAST(ic.increment_value AS nvarchar(40)) + ')'
                        END,
                        cc.definition
                 FROM sys.all_columns c
                 JOIN sys.types ty ON ty.user_type_id = c.user_type_id
                 LEFT JOIN sys.default_constraints dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id
                 LEFT JOIN sys.identity_columns ic ON ic.object_id = c.object_id AND ic.column_id = c.column_id
                 LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id
                 LEFT JOIN sys.extended_properties ep
                   ON ep.class = 1 AND ep.major_id = c.object_id AND ep.minor_id = c.column_id AND ep.name = 'MS_Description'
                 WHERE c.object_id = CAST(@P1 AS int)
                 ORDER BY c.column_id"
            ),
            &[id],
        )
        .await?;
        // (column, identity, computed) for the CREATE TABLE.
        let mut extras: Vec<(Option<String>, Option<String>)> = vec![];
        let columns: Vec<ColumnInfo> = cols
            .iter()
            .map(|r| {
                let (identity, computed) = (text(r, 6), text(r, 7));
                extras.push((identity.clone(), computed.clone()));
                ColumnInfo {
                    name: string(r, 0),
                    data_type: string(r, 1),
                    nullable: flag(r, 2),
                    default: text(r, 3).or(identity).or(computed.map(|c| format!("AS {c}"))),
                    primary_key: flag(r, 4),
                    comment: text(r, 5),
                }
            })
            .collect();

        let index_rows = rows(
            &mut meta,
            "SELECT i.name, i.is_unique, i.is_primary_key, i.is_unique_constraint, i.type_desc, i.filter_definition,
                    STUFF((SELECT ', ' + QUOTENAME(c.name) + CASE WHEN ic.is_descending_key = 1 THEN ' DESC' ELSE '' END
                           FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                           WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 0
                           ORDER BY ic.key_ordinal FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, ''),
                    STUFF((SELECT ', ' + QUOTENAME(c.name)
                           FROM sys.index_columns ic JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
                           WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = 1
                           ORDER BY ic.index_column_id FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, '')
             FROM sys.indexes i
             WHERE i.object_id = CAST(@P1 AS int) AND i.type > 0 AND i.is_hypothetical = 0
             ORDER BY i.is_primary_key DESC, i.name",
            &[id],
        )
        .await?;
        let mut indexes = vec![];
        let mut constraints = vec![];
        for r in &index_rows {
            let (index, unique, primary, unique_key) = (string(r, 0), flag(r, 1), flag(r, 2), flag(r, 3));
            // CLUSTERED, NONCLUSTERED, CLUSTERED COLUMNSTORE, …
            let clustering = string(r, 4);
            let cols = text(r, 6).unwrap_or_default();
            let definition = if primary || unique_key {
                let what = if primary { "PRIMARY KEY" } else { "UNIQUE" };
                let definition = format!("{what} {clustering} ({cols})");
                constraints.push(ConstraintInfo {
                    name: index.clone(),
                    kind: if primary { "p" } else { "u" }.into(),
                    definition: definition.clone(),
                });
                definition
            } else {
                let mut d = format!(
                    "CREATE {}{clustering} INDEX {} ON {qualified} ({cols})",
                    if unique { "UNIQUE " } else { "" },
                    quote(&index)
                );
                if let Some(include) = text(r, 7) {
                    d.push_str(&format!(" INCLUDE ({include})"));
                }
                if let Some(filter) = text(r, 5) {
                    d.push_str(&format!(" WHERE {filter}"));
                }
                d
            };
            indexes.push(IndexInfo { name: index, definition, unique, primary });
        }

        let keys = rows(
            &mut meta,
            "SELECT fk.name, rs.name, rt.name,
                    STUFF((SELECT ', ' + QUOTENAME(pc.name) FROM sys.foreign_key_columns fkc
                           JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id
                           WHERE fkc.constraint_object_id = fk.object_id ORDER BY fkc.constraint_column_id
                           FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, ''),
                    STUFF((SELECT ', ' + QUOTENAME(rc.name) FROM sys.foreign_key_columns fkc
                           JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
                           WHERE fkc.constraint_object_id = fk.object_id ORDER BY fkc.constraint_column_id
                           FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, ''),
                    fk.update_referential_action_desc, fk.delete_referential_action_desc
             FROM sys.foreign_keys fk
             JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id
             JOIN sys.schemas rs ON rs.schema_id = rt.schema_id
             WHERE fk.parent_object_id = CAST(@P1 AS int)
             ORDER BY fk.name",
            &[id],
        )
        .await?;
        for r in &keys {
            let mut d = format!(
                "FOREIGN KEY ({}) REFERENCES {}.{} ({})",
                string(r, 3),
                quote(&string(r, 1)),
                quote(&string(r, 2)),
                string(r, 4)
            );
            for (rule, at) in [("ON UPDATE", 5), ("ON DELETE", 6)] {
                let action = string(r, at).replace('_', " ");
                if !action.is_empty() && action != "NO ACTION" {
                    d.push_str(&format!(" {rule} {action}"));
                }
            }
            constraints.push(ConstraintInfo { name: string(r, 0), kind: "f".into(), definition: d });
        }
        let checks = rows(
            &mut meta,
            "SELECT name, definition FROM sys.check_constraints WHERE parent_object_id = CAST(@P1 AS int) ORDER BY name",
            &[id],
        )
        .await?;
        constraints.extend(checks.iter().map(|r| ConstraintInfo {
            name: string(r, 0),
            kind: "c".into(),
            definition: format!("CHECK {}", string(r, 1)),
        }));

        let ddl = if table {
            table_ddl(&qualified, &columns, &extras, &constraints, &indexes)
        } else {
            text(rel, 5).map(|d| d.trim().to_string()).unwrap_or_default()
        };
        Ok(TableInfo {
            schema: schema.to_string(),
            name: name.to_string(),
            kind,
            comment: text(rel, 2),
            rows_estimate: table.then(|| number(rel, 3)).flatten(),
            total_size: table.then(|| number(rel, 4)).flatten().map(super::mysql::pretty_size),
            columns,
            indexes,
            constraints,
            ddl,
        })
    }

    /// The `CREATE FUNCTION` or `CREATE PROCEDURE` statement for a routine.
    pub async fn function_ddl(&self, schema: &str, name: &str) -> anyhow::Result<String> {
        let mut meta = self.meta.lock().await;
        let qualified = format!("{}.{}", quote(schema), quote(name));
        let found = rows(&mut meta, "SELECT OBJECT_DEFINITION(OBJECT_ID(@P1))", &[&qualified]).await?;
        found
            .first()
            .and_then(|r| text(r, 0))
            .map(|d| d.trim().to_string())
            .ok_or_else(|| anyhow!("no definition for {qualified}: it may be encrypted, or need VIEW DEFINITION"))
    }

    /// Apply row edits in one transaction: all of them, or none. An update
    /// or delete that doesn't hit exactly one row stops and rolls back. Runs
    /// off the work connection, so it never joins a transaction the user
    /// opened in a query tab. Returns how many rows changed.
    pub async fn apply(&self, target: &EditTarget, changes: &[Change]) -> anyhow::Result<u64> {
        if changes.is_empty() {
            return Ok(0);
        }
        let mut meta = self.meta.lock().await;
        meta.simple_query("BEGIN TRANSACTION").await.map_err(ms)?.into_results().await.map_err(ms)?;
        let mut total = 0;
        for (i, change) in changes.iter().enumerate() {
            let (sql, cells) = target.bound(change);
            for (column, value) in &cells {
                // The grid shows long binary values cut short; saving one
                // would lose data.
                if let Some(v) = value
                    && target.declared(column).is_some_and(is_binary_type)
                    && let Err(e) = super::parse_hex(v)
                {
                    rollback(&mut meta).await;
                    bail!("change {} of {} failed, nothing was saved: {e}\n{}", i + 1, changes.len(), target.preview(change));
                }
            }
            let values: Vec<Option<&str>> = cells.iter().map(|(_, v)| v.as_deref()).collect();
            let params: Vec<&dyn tiberius::ToSql> = values.iter().map(|v| v as &dyn tiberius::ToSql).collect();
            let done = match meta.execute(sql.as_str(), &params).await {
                // Triggers add counts of their own; the statement's comes last.
                Ok(r) => r.rows_affected().last().copied().unwrap_or(0),
                Err(e) => {
                    rollback(&mut meta).await;
                    bail!("change {} of {} failed, nothing was saved: {}\n{}", i + 1, changes.len(), ms(e), target.preview(change));
                }
            };
            if done != 1 && !matches!(change, Change::Insert { .. }) {
                rollback(&mut meta).await;
                bail!(
                    "change {} of {} matched {done} rows instead of 1 (the row may have changed or gone), nothing was saved:\n{}",
                    i + 1,
                    changes.len(),
                    target.preview(change)
                );
            }
            total += done;
        }
        meta.simple_query("COMMIT TRANSACTION").await.map_err(ms)?.into_results().await.map_err(ms)?;
        Ok(total)
    }

    /// Run `sql` (any number of statements, and `GO` batches) on the work
    /// connection.
    pub async fn run(&self, sql: &str, max_rows: usize) -> anyhow::Result<QueryOutcome> {
        let mut work = self.work.lock().await;
        let stopped = self.stop.notified();
        tokio::pin!(stopped);
        // Listen from now, so a Stop pressed before the first row counts.
        stopped.as_mut().enable();
        let why = "the query was stopped (SQL Server stops a query by closing its connection, so a transaction left open was rolled back)";
        run_script(&mut work, &self.config, sql, max_rows, stopped, why).await
    }

    /// The agents' connection, opened on first use (and after a timeout).
    async fn agent(&self) -> anyhow::Result<tokio::sync::MutexGuard<'_, Option<Conn>>> {
        let mut agent = self.agent.lock().await;
        if agent.is_none() {
            *agent = Some(open(&self.config).await?);
        }
        Ok(agent)
    }

    /// Run `sql` on the agents' connection, stopping it after `timeout`.
    async fn timed(&self, slot: &mut Option<Conn>, sql: &str, max_rows: usize, timeout: Duration) -> anyhow::Result<QueryOutcome> {
        let why = format!("the query timed out after {:.1}s and was stopped", timeout.as_secs_f64());
        run_script(slot, &self.config, sql, max_rows, tokio::time::sleep(timeout), &why).await
    }

    /// Run one SELECT for an agent, inside a transaction that is always
    /// rolled back. SQL Server has no read-only transactions, so Tabula
    /// checks the statement first: anything that could write is refused as
    /// needing write access. A login with only read rights is the real
    /// guarantee.
    pub async fn run_read_only(&self, sql: &str, max_rows: usize, timeout: Duration) -> anyhow::Result<QueryOutcome> {
        let statement = one_statement(sql)?;
        check_reads_only(statement)?;
        let mut agent = self.agent().await?;
        let conn = agent.as_mut().expect("just opened");
        conn.simple_query("BEGIN TRANSACTION").await.map_err(ms)?.into_results().await.map_err(ms)?;
        let out = self.timed(&mut agent, statement, max_rows, timeout).await;
        // After a timeout the connection, and its transaction, are gone.
        if let Some(conn) = agent.as_mut() {
            rollback(conn).await;
        }
        out
    }

    /// Run `sql` with a timeout on the agents' connection, for AI agents with
    /// write access.
    pub async fn run_write(&self, sql: &str, max_rows: usize, timeout: Duration) -> anyhow::Result<QueryOutcome> {
        let mut agent = self.agent().await?;
        let out = self.timed(&mut agent, sql, max_rows, timeout).await;
        let Some(conn) = agent.as_mut() else { return out };
        // A transaction left open would hold its locks until the session ends.
        let open = conn.simple_query("SELECT @@TRANCOUNT").await.map_err(ms)?.into_row().await.map_err(ms)?;
        if open.and_then(|r| number(&r, 0)).unwrap_or(0) > 0 {
            rollback(conn).await;
            if out.is_ok() {
                bail!("the transaction was left open, so it was rolled back; agents run one statement at a time");
            }
        }
        out
    }

    /// The estimated plan as text (SHOWPLAN_TEXT: nothing runs). With
    /// `analyze`, the statement runs (read-only, as for agents) under
    /// STATISTICS PROFILE, and each step shows its actual rows and runs.
    pub async fn plan(&self, sql: &str, analyze: bool, timeout: Duration) -> anyhow::Result<String> {
        let statement = one_statement(sql)?;
        if analyze {
            check_reads_only(statement)?;
        }
        let mut agent = self.agent().await?;
        let conn = agent.as_mut().expect("just opened");
        let (on, off) = if analyze {
            ("BEGIN TRANSACTION; SET STATISTICS PROFILE ON", "SET STATISTICS PROFILE OFF; IF @@TRANCOUNT > 0 ROLLBACK")
        } else {
            ("SET SHOWPLAN_TEXT ON", "SET SHOWPLAN_TEXT OFF")
        };
        conn.simple_query(on).await.map_err(ms)?.into_results().await.map_err(ms)?;
        let out = self.timed(&mut agent, statement, 2_000, timeout).await;
        if let Some(conn) = agent.as_mut() {
            conn.simple_query(off).await.map_err(ms)?.into_results().await.map_err(ms)?;
        }
        let out = out?;
        let mut lines = vec![];
        for set in out.results.iter().filter(|r| r.has_rows()) {
            // SHOWPLAN_TEXT: one column of steps. STATISTICS PROFILE: Rows,
            // Executes, StmtText, …
            let at = |name: &str| set.columns.iter().position(|c| c.name == name);
            match (at("Rows"), at("Executes"), at("StmtText")) {
                (Some(r), Some(e), Some(s)) => lines.extend(set.rows.iter().map(|row| {
                    let get = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
                    format!("{} (actual rows={} runs={})", get(s), get(r), get(e))
                })),
                _ if analyze => {}
                _ => lines.extend(set.rows.iter().filter_map(|row| row.first().cloned().flatten())),
            }
        }
        Ok(lines.join("\n"))
    }
}

async fn rollback(conn: &mut Conn) {
    let done = match conn.simple_query("IF @@TRANCOUNT > 0 ROLLBACK").await {
        Ok(stream) => stream.into_results().await.map(|_| ()),
        Err(e) => Err(e),
    };
    if let Err(e) = done {
        eprintln!("tabula: ROLLBACK failed: {e}");
    }
}

/// The one statement (and batch) in `sql`, or why there isn't one.
fn one_statement(sql: &str) -> anyhow::Result<&str> {
    let batches = script::batches(sql);
    let [batch] = batches[..] else {
        if batches.is_empty() {
            bail!("there is no statement to run");
        }
        bail!("agents run one statement at a time (this has {} GO batches)", batches.len());
    };
    match script::statements(batch, Engine::SqlServer)[..] {
        [one] => Ok(one),
        [] => bail!("there is no statement to run"),
        _ => bail!("agents run one statement at a time"),
    }
}

/// Words that write, run code, or start another statement. T-SQL needs no
/// semicolon between statements, so `SELECT 1 DELETE FROM t` is two; the
/// second one's first word is caught here.
const WRITES: &[&str] = &[
    "INSERT", "UPDATE", "DELETE", "MERGE", "INTO", "CREATE", "ALTER", "DROP", "TRUNCATE", "EXEC", "EXECUTE",
    "GRANT", "REVOKE", "DENY", "BACKUP", "RESTORE", "DBCC", "KILL", "SHUTDOWN", "BULK", "OPENROWSET",
    "OPENDATASOURCE", "OPENQUERY", "DECLARE", "SET", "BEGIN", "COMMIT", "ROLLBACK", "SAVE", "USE", "RECONFIGURE",
    "WRITETEXT", "UPDATETEXT", "CHECKPOINT", "SETUSER", "REVERT", "TRIGGER", "RAISERROR", "THROW", "WAITFOR",
];

/// Only a SELECT (or WITH … SELECT) that names nothing in [`WRITES`], and
/// takes no value from a sequence (which no rollback undoes).
fn check_reads_only(statement: &str) -> anyhow::Result<()> {
    let words = script::words(statement, Engine::SqlServer);
    let first = words.first().map(String::as_str).unwrap_or("");
    let refuse = |what: &str| {
        Err(anyhow::Error::new(WriteRefused(format!(
            "{what} isn't allowed in read-only mode: agents may run one SELECT (SQL Server has no read-only transactions, so Tabula checks)"
        ))))
    };
    if first != "SELECT" && first != "WITH" {
        return refuse(if first.is_empty() { "this statement" } else { first });
    }
    if let Some(w) = words.iter().find(|w| WRITES.contains(&w.as_str())) {
        return refuse(w);
    }
    if words.windows(3).any(|w| w[0] == "NEXT" && w[1] == "VALUE" && w[2] == "FOR") {
        return refuse("NEXT VALUE FOR");
    }
    Ok(())
}

/// A readable CREATE TABLE built from the catalog.
fn table_ddl(
    qualified: &str,
    columns: &[ColumnInfo],
    extras: &[(Option<String>, Option<String>)],
    constraints: &[ConstraintInfo],
    indexes: &[IndexInfo],
) -> String {
    let mut lines: Vec<String> = columns
        .iter()
        .zip(extras)
        .map(|(c, (identity, computed))| {
            if let Some(computed) = computed {
                return format!("    {} AS {computed}", quote(&c.name));
            }
            let mut l = format!("    {} {}", quote(&c.name), c.data_type);
            if let Some(identity) = identity {
                l.push_str(&format!(" {identity}"));
            }
            l.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
            if let Some(d) = c.default.as_ref().filter(|_| identity.is_none()) {
                l.push_str(&format!(" DEFAULT {d}"));
            }
            l
        })
        .collect();
    lines.extend(constraints.iter().map(|k| format!("    CONSTRAINT {} {}", quote(&k.name), k.definition)));
    let mut s = format!("CREATE TABLE {qualified} (\n{}\n);", lines.join(",\n"));
    for i in indexes.iter().filter(|i| !constraints.iter().any(|k| k.name == i.name)) {
        s.push_str(&format!("\n{};", i.definition));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_selects_get_through() {
        for ok in [
            "select * from dbo.orders where [update] = 'delete'",
            "WITH t AS (SELECT 1 AS x) SELECT x FROM t",
            "select * from t order by id offset 10 rows fetch next 10 rows only",
        ] {
            assert!(check_reads_only(ok).is_ok(), "{ok}");
        }
        for (bad, why) in [
            ("update t set x = 1", "UPDATE"),
            ("select 1 delete from t", "DELETE"),
            ("select * into #copy from t", "INTO"),
            ("with d as (select 1 as x) delete from d", "DELETE"),
            ("exec sp_who", "EXEC"),
            ("select next value for dbo.seq", "NEXT VALUE FOR"),
            ("select * from openrowset('x', 'y', 'z')", "OPENROWSET"),
        ] {
            let err = check_reads_only(bad).unwrap_err();
            assert!(super::super::needs_write_access(&err) && err.to_string().contains(why), "{bad}: {err}");
        }
        assert!(one_statement("select 1\nGO\nselect 2").unwrap_err().to_string().contains("one statement"));
        assert_eq!(one_statement("select 1;\nGO").unwrap(), "select 1");
    }

    #[test]
    fn fractions_keep_the_column_precision() {
        let mut s = String::from("12:00:00");
        push_fraction(&mut s, 123_456_700, 7);
        assert_eq!(s, "12:00:00.1234567");
        let mut s = String::from("x");
        push_fraction(&mut s, 5_000_000, 3);
        assert_eq!(s, "x.005");
    }
}

