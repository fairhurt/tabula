//! Saved connection profiles (`<config>/tabula/connections.json`) and their
//! passwords.
//!
//! Passwords never go in the profiles file. On Windows and macOS they live in
//! the OS credential store; on Linux in `secrets.json` next to the profiles,
//! readable only by the user. AI agents only ever see profile *names*.

use std::path::PathBuf;

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    pub database: String,
    #[serde(default)]
    pub ssl: SslMode,
    #[serde(default)]
    pub ai: AiAccess,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            name: String::new(),
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
    /// Parse `postgres://user:pass@host:port/db?sslmode=…`. Returns the
    /// profile and the password, if the URL has one.
    pub fn from_url(name: &str, s: &str) -> anyhow::Result<(Profile, Option<String>)> {
        let u = url::Url::parse(s).context("not a valid URL")?;
        if !matches!(u.scheme(), "postgres" | "postgresql") {
            bail!("the URL should start with postgres://");
        }
        let decode = |v: &str| {
            url::form_urlencoded::parse(format!("x={v}").as_bytes())
                .next()
                .map(|(_, v)| v.into_owned())
                .unwrap_or_default()
        };
        let mut p = Profile {
            name: name.to_string(),
            host: u.host_str().unwrap_or("localhost").to_string(),
            port: u.port().unwrap_or(5432),
            ..Profile::default()
        };
        if !u.username().is_empty() {
            p.user = decode(u.username());
        }
        let db = u.path().trim_start_matches('/');
        if !db.is_empty() {
            p.database = decode(db);
        }
        for (k, v) in u.query_pairs() {
            if k == "sslmode" {
                p.ssl = match v.as_ref() {
                    "disable" => SslMode::Disable,
                    "require" => SslMode::Require,
                    "verify-ca" | "verify-full" | "verify" => SslMode::Verify,
                    _ => SslMode::Prefer,
                };
            }
        }
        if p.name.is_empty() {
            p.name = format!("{}@{}", p.database, p.host);
        }
        Ok((p, u.password().map(decode)))
    }

    pub fn summary(&self) -> String {
        format!("{}@{}:{}/{}", self.user, self.host, self.port, self.database)
    }

    pub fn with_database(&self, database: &str) -> Profile {
        Profile {
            database: database.to_string(),
            ..self.clone()
        }
    }
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
        assert!(Profile::from_url("x", "mysql://a@b/c").is_err());
    }
}
