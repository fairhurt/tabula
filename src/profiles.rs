//! Saved connection profiles (`<config>/tabula/connections.json`) and their
//! passwords. A profile is a PostgreSQL, MySQL or SQL Server server, or a
//! SQLite file.
//!
//! Passwords never go in the profiles file. On Windows and macOS they live in
//! the OS credential store; on Linux in `secrets.json` next to the profiles,
//! readable only by the user. AI agents only ever see profile *names*.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

/// The kind of database a profile points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    #[default]
    Postgres,
    /// MySQL, and MariaDB, which speaks the same protocol.
    #[serde(rename = "mysql")]
    MySql,
    #[serde(rename = "sqlserver")]
    SqlServer,
    Sqlite,
}

impl Engine {
    pub const ALL: [Engine; 4] = [Engine::Postgres, Engine::MySql, Engine::SqlServer, Engine::Sqlite];

    pub fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::MySql => "MySQL",
            Engine::SqlServer => "SQL Server",
            Engine::Sqlite => "SQLite",
        }
    }

    /// A server to connect to, rather than a file.
    pub fn is_server(self) -> bool {
        self != Engine::Sqlite
    }

    /// The port a server listens on unless told otherwise.
    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::MySql => 3306,
            Engine::SqlServer => 1433,
            Engine::Sqlite => 0,
        }
    }

    /// The administrator a fresh install comes with.
    pub fn default_user(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::MySql => "root",
            Engine::SqlServer => "sa",
            Engine::Sqlite => "",
        }
    }

    /// The schema unqualified names resolve to, so completion can leave it off.
    pub fn default_schema(self) -> &'static str {
        match self {
            Engine::Postgres => "public",
            Engine::SqlServer => "dbo",
            Engine::Sqlite => "main",
            // A MySQL database is its own (only) schema.
            Engine::MySql => "",
        }
    }

    /// An identifier quoted in the engine's own syntax: `"name"`, MySQL's
    /// `` `name` ``, or SQL Server's `[name]`. Quotes inside are doubled.
    pub fn quote_ident(self, s: &str) -> String {
        match self {
            Engine::Postgres | Engine::Sqlite => format!("\"{}\"", s.replace('"', "\"\"")),
            Engine::MySql => format!("`{}`", s.replace('`', "``")),
            Engine::SqlServer => format!("[{}]", s.replace(']', "]]")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    Disable,
    #[default]
    Prefer,
    Require,
    /// Encrypt, and refuse a server whose certificate isn't trusted by this
    /// computer or doesn't match the host name (libpq's `verify-full`).
    Verify,
}

impl SslMode {
    pub const ALL: [SslMode; 4] = [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::Verify];

    pub fn label(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::Verify => "verify",
        }
    }

    /// What the mode protects against, in a sentence.
    pub fn hint(self) -> &'static str {
        match self {
            SslMode::Disable => "Not encrypted. Fine for a database on this computer.",
            SslMode::Prefer => "Encrypted if the server offers it; the server's identity isn't checked.",
            SslMode::Require => "Always encrypted, but the server's identity isn't checked, so the connection could be intercepted.",
            SslMode::Verify => "Always encrypted, and the server must prove who it is with a trusted certificate for this host. Use this over the internet.",
        }
    }

    fn is_default(&self) -> bool {
        *self == SslMode::default()
    }
}

/// What AI agents (through `tabula mcp`) may do on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AiAccess {
    /// Not visible to agents at all.
    Off,
    /// Catalog and read-only queries (the default).
    #[default]
    ReadOnly,
    /// Read-only, and writes are allowed once you approve each one in Tabula.
    AskForWrites,
    /// Any statement. For throwaway databases.
    ReadWrite,
}

impl AiAccess {
    pub const ALL: [AiAccess; 4] = [
        AiAccess::Off,
        AiAccess::ReadOnly,
        AiAccess::AskForWrites,
        AiAccess::ReadWrite,
    ];

    pub fn label(self) -> &'static str {
        match self {
            AiAccess::Off => "Off",
            AiAccess::ReadOnly => "Read only",
            AiAccess::AskForWrites => "Ask for writes",
            AiAccess::ReadWrite => "Read & write",
        }
    }
}

/// The server fields (`host`, `port`, `user`, `ssl`) are for PostgreSQL,
/// MySQL and SQL Server; a SQLite profile leaves them empty and keeps the
/// file's path in `database`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    #[serde(default)]
    pub engine: Engine,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub port: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    /// The database name, or for SQLite the absolute path of the file.
    pub database: String,
    #[serde(default, skip_serializing_if = "SslMode::is_default")]
    pub ssl: SslMode,
    #[serde(default)]
    pub ai: AiAccess,
}

fn is_zero(n: &u16) -> bool {
    *n == 0
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            name: String::new(),
            engine: Engine::Postgres,
            host: "localhost".into(),
            port: 5432,
            user: "postgres".into(),
            database: "postgres".into(),
            ssl: SslMode::default(),
            ai: AiAccess::default(),
        }
    }
}

impl Profile {
    /// A profile for the SQLite database file at `path`.
    pub fn sqlite(name: &str, path: &str) -> Profile {
        let path = absolute(path);
        Profile {
            name: if name.is_empty() { file_name(&path) } else { name.to_string() },
            engine: Engine::Sqlite,
            host: String::new(),
            port: 0,
            user: String::new(),
            database: path,
            ssl: SslMode::default(),
            ai: AiAccess::default(),
        }
    }

    /// The empty form for a server of `engine`: localhost, its usual port and
    /// administrator. MySQL and SQL Server start with no database, which
    /// means the login's default one.
    pub fn server(engine: Engine) -> Profile {
        Profile {
            engine,
            port: engine.default_port(),
            user: engine.default_user().into(),
            database: if engine == Engine::Postgres { "postgres".into() } else { String::new() },
            ..Profile::default()
        }
    }

    /// Parse `postgres://user:pass@host:port/db?sslmode=…` (or `mysql://`,
    /// `mariadb://`, `sqlserver://`, `mssql://` in the same shape), or for SQLite
    /// `sqlite:/path/to/file.db` or just the path of an existing file.
    /// Returns the profile and the password, if the URL has one.
    pub fn from_url(name: &str, s: &str) -> anyhow::Result<(Profile, Option<String>)> {
        if let Some(path) = sqlite_path(s) {
            if path.is_empty() {
                bail!("the URL needs a file, like sqlite:/path/to/app.db");
            }
            return Ok((Profile::sqlite(name, &path), None));
        }
        let u = url::Url::parse(s).context("not a valid URL")?;
        let engine = match u.scheme() {
            "postgres" | "postgresql" => Engine::Postgres,
            "mysql" | "mariadb" => Engine::MySql,
            "sqlserver" | "mssql" => Engine::SqlServer,
            _ => bail!("the URL should start with postgres://, mysql://, sqlserver:// or sqlite:"),
        };
        let decode = |v: &str| {
            url::form_urlencoded::parse(format!("x={v}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        let mut p = Profile {
            name: name.to_string(),
            host: u.host_str().unwrap_or("localhost").to_string(),
            port: u.port().unwrap_or(engine.default_port()),
            ..Profile::server(engine)
        };
        if !u.username().is_empty() {
            p.user = decode(u.username());
        }
        let db = u.path().trim_start_matches('/');
        if !db.is_empty() {
            p.database = decode(db);
        }
        for (k, v) in u.query_pairs() {
            // libpq's sslmode, MySQL's ssl-mode, and SQL Server's encrypt.
            let k = k.to_ascii_lowercase().replace('_', "-");
            if matches!(k.as_str(), "sslmode" | "ssl-mode" | "encrypt") {
                p.ssl = match v.to_ascii_lowercase().as_str() {
                    "disable" | "disabled" | "false" | "no" => SslMode::Disable,
                    "require" | "required" | "true" | "yes" | "mandatory" => SslMode::Require,
                    "verify-ca" | "verify-full" | "verify" | "verify-identity" | "strict" => SslMode::Verify,
                    _ => SslMode::Prefer,
                };
            }
        }
        if p.name.is_empty() {
            p.name = p.default_name();
        }
        Ok((p, u.password().map(decode)))
    }

    /// `database@host`, or just the host when no database is named.
    pub fn default_name(&self) -> String {
        if self.database.is_empty() {
            self.host.clone()
        } else {
            format!("{}@{}", self.database, self.host)
        }
    }

    pub fn summary(&self) -> String {
        match self.engine {
            Engine::Sqlite => self.database.clone(),
            _ => format!("{}@{}:{}/{}", self.user, self.host, self.port, self.database),
        }
    }

    /// The database as shown next to the connection name: short enough for
    /// a toolbar, so a SQLite file goes by its file name.
    pub fn database_label(&self) -> String {
        match self.engine {
            Engine::Sqlite => file_name(&self.database),
            _ => self.database.clone(),
        }
    }

    /// The connection and its database, for toolbars: `name · database`, or
    /// just the name when the two would say the same thing.
    pub fn title(&self) -> String {
        let database = self.database_label();
        if database == self.name {
            database
        } else {
            format!("{} · {database}", self.name)
        }
    }

    pub fn with_database(&self, database: &str) -> Profile {
        Profile {
            database: database.to_string(),
            ..self.clone()
        }
    }
}

/// The file a SQLite URL names: `sqlite:app.db`, `sqlite:/abs/app.db`,
/// `sqlite:///abs/app.db`, or the bare path of a file that exists. The path
/// is taken as written (no percent-decoding).
fn sqlite_path(s: &str) -> Option<String> {
    let Some(rest) = s.strip_prefix("sqlite:") else {
        let bare = !s.is_empty() && !s.contains("://") && Path::new(&expand_home(s)).is_file();
        return bare.then(|| s.to_string());
    };
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    // `sqlite:///C:/data/app.db` leaves `/C:/data/app.db`.
    let b = rest.as_bytes();
    let drive = b.len() > 2 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b':';
    Some(if drive { &rest[1..] } else { rest }.to_string())
}

/// `~/x` with the home folder filled in.
fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest).to_string_lossy().into_owned(),
        _ => path.to_string(),
    }
}

/// Absolute, so a saved profile works whatever folder Tabula (or an agent's
/// `tabula mcp`) is started from.
fn absolute(path: &str) -> String {
    let path = expand_home(path.trim());
    std::path::absolute(&path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(path)
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

pub fn config_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("TABULA_HOME") {
        return PathBuf::from(d);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("tabula")
}

fn profiles_path() -> PathBuf {
    config_dir().join("connections.json")
}

pub fn load() -> Vec<Profile> {
    std::fs::read_to_string(profiles_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(profiles: &[Profile]) -> anyhow::Result<()> {
    let p = profiles_path();
    std::fs::create_dir_all(p.parent().expect("config path has a parent"))?;
    std::fs::write(&p, serde_json::to_string_pretty(profiles)?)?;
    Ok(())
}

/// Insert or replace (by name).
pub fn upsert(profile: Profile) -> anyhow::Result<Vec<Profile>> {
    let mut all = load();
    match all.iter_mut().find(|p| p.name == profile.name) {
        Some(p) => *p = profile,
        None => all.push(profile),
    }
    save(&all)?;
    Ok(all)
}

pub fn remove(name: &str) -> anyhow::Result<Vec<Profile>> {
    let mut all = load();
    all.retain(|p| p.name != name);
    save(&all)?;
    let _ = set_password(name, None);
    Ok(all)
}


// ------------------------------------------------------------ passwords

const SERVICE: &str = "tabula";

#[cfg(any(windows, target_os = "macos"))]
pub fn password(name: &str) -> Option<String> {
    keyring::Entry::new(SERVICE, name)
        .ok()?
        .get_password()
        .ok()
}

#[cfg(any(windows, target_os = "macos"))]
pub fn set_password(name: &str, password: Option<&str>) -> anyhow::Result<()> {
    let e = keyring::Entry::new(SERVICE, name)?;
    match password {
        Some(pw) => e.set_password(pw)?,
        None => {
            let _ = e.delete_credential();
        }
    }
    Ok(())
}

#[cfg(not(any(windows, target_os = "macos")))]
fn secrets_path() -> PathBuf {
    config_dir().join("secrets.json")
}

#[cfg(not(any(windows, target_os = "macos")))]
fn load_secrets() -> std::collections::BTreeMap<String, String> {
    let _ = SERVICE;
    std::fs::read_to_string(secrets_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn password(name: &str) -> Option<String> {
    load_secrets().remove(name)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn set_password(name: &str, password: Option<&str>) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut all = load_secrets();
    match password {
        Some(pw) => all.insert(name.to_string(), pw.to_string()),
        None => all.remove(name),
    };
    let p = secrets_path();
    std::fs::create_dir_all(p.parent().expect("config path has a parent"))?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&p)?;
    f.write_all(serde_json::to_string_pretty(&all)?.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls() {
        let (p, pw) =
            Profile::from_url("", "postgres://alice:p%40ss@db.example.com:6543/shop?sslmode=require")
                .unwrap();
        assert_eq!(p.user, "alice");
        assert_eq!(pw.as_deref(), Some("p@ss"));
        assert_eq!(p.host, "db.example.com");
        assert_eq!(p.port, 6543);
        assert_eq!(p.database, "shop");
        assert_eq!(p.ssl, SslMode::Require);
        let (p, _) = Profile::from_url("", "postgres://alice@db.example.com/shop?sslmode=verify-full").unwrap();
        assert_eq!(p.ssl, SslMode::Verify);
        // Saved as a plain word, and older files without it still load.
        assert_eq!(serde_json::to_string(&SslMode::Verify).unwrap(), "\"verify\"");
        assert_eq!(p.name, "shop@db.example.com");
        assert!(Profile::from_url("x", "oracle://a@b/c").is_err());
    }

    #[test]
    fn parses_mysql_and_sql_server_urls() {
        let (p, pw) = Profile::from_url("", "mysql://app:s%3Acret@db.internal/shop?ssl-mode=REQUIRED").unwrap();
        assert_eq!((p.engine, p.port, p.user.as_str(), p.database.as_str()), (Engine::MySql, 3306, "app", "shop"));
        assert_eq!((p.ssl, pw.as_deref()), (SslMode::Require, Some("s:cret")));
        assert_eq!(p.name, "shop@db.internal");
        let (p, _) = Profile::from_url("", "mariadb://db.internal:3307").unwrap();
        assert_eq!((p.engine, p.port, p.user.as_str(), p.database.as_str()), (Engine::MySql, 3307, "root", ""));
        assert_eq!(p.name, "db.internal");

        let (p, _) = Profile::from_url("", "sqlserver://sa@sql.example.com/Sales?encrypt=strict").unwrap();
        assert_eq!((p.engine, p.port, p.database.as_str(), p.ssl), (Engine::SqlServer, 1433, "Sales", SslMode::Verify));
        let (p, _) = Profile::from_url("x", "mssql://localhost").unwrap();
        assert_eq!((p.engine, p.user.as_str(), p.ssl), (Engine::SqlServer, "sa", SslMode::Prefer));

        // Saved under short, stable names.
        assert_eq!(serde_json::to_string(&Engine::MySql).unwrap(), "\"mysql\"");
        assert_eq!(serde_json::to_string(&Engine::SqlServer).unwrap(), "\"sqlserver\"");
    }

    #[test]
    fn quotes_identifiers_per_engine() {
        assert_eq!(Engine::Postgres.quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(Engine::MySql.quote_ident("a`b"), "`a``b`");
        assert_eq!(Engine::SqlServer.quote_ident("a]b"), "[a]]b]");
    }

    #[test]
    fn parses_sqlite_urls_and_paths() {
        // Whatever an absolute path looks like here (`/data/…` or `C:\data\…`).
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("shop.db");
        let path = file.to_str().unwrap();
        for url in [format!("sqlite:{path}"), format!("sqlite:///{}", path.trim_start_matches('/'))] {
            let (p, pw) = Profile::from_url("", &url).unwrap();
            assert_eq!(p.engine, Engine::Sqlite, "{url}");
            assert_eq!(p.database, path, "{url}");
            assert_eq!(p.name, "shop.db");
            assert_eq!(p.summary(), path);
            assert_eq!(p.database_label(), "shop.db");
            assert_eq!(p.title(), "shop.db");
            assert_eq!(Profile { name: "Shop".into(), ..p.clone() }.title(), "Shop · shop.db");
            assert!(pw.is_none());
        }
        // Relative paths are pinned to the current folder.
        let (p, _) = Profile::from_url("mine", "sqlite:shop.db").unwrap();
        assert_eq!(p.name, "mine");
        assert_eq!(Path::new(&p.database), std::env::current_dir().unwrap().join("shop.db"));
        assert!(Profile::from_url("", "sqlite:").is_err());

        // A bare path counts only when the file is there.
        assert!(Profile::from_url("", path).is_err());
        std::fs::write(&file, b"").unwrap();
        let (p, _) = Profile::from_url("", path).unwrap();
        assert_eq!((p.engine, p.database.as_str()), (Engine::Sqlite, path));
    }

    #[test]
    fn profiles_round_trip_and_old_files_still_load() {
        // Saved before SQLite support: no `engine`.
        let old = r#"[{"name":"a","host":"h","port":5432,"user":"u","database":"d","ssl":"require","ai":"off"}]"#;
        let p: Vec<Profile> = serde_json::from_str(old).unwrap();
        assert_eq!((p[0].engine, p[0].ssl, p[0].port), (Engine::Postgres, SslMode::Require, 5432));

        // A SQLite profile saves none of the server's fields.
        let lite = Profile::sqlite("s", "shop.db");
        let saved: serde_json::Value = serde_json::to_value(&lite).unwrap();
        let mut fields: Vec<&str> = saved.as_object().unwrap().keys().map(String::as_str).collect();
        fields.sort();
        assert_eq!(fields, vec!["ai", "database", "engine", "name"]);
        assert_eq!(saved["engine"], "sqlite");
        assert_eq!(serde_json::from_value::<Profile>(saved).unwrap(), lite);
        let pg = Profile { name: "p".into(), ..Profile::default() };
        assert_eq!(serde_json::from_str::<Profile>(&serde_json::to_string(&pg).unwrap()).unwrap(), pg);
    }
}
