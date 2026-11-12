//! Updates from GitHub Releases.
//!
//! At startup Tabula asks GitHub for the latest release. If it is newer, the
//! status bar offers it; on Windows one click downloads that release's
//! installer, checks it against the published SHA-256, and runs it. Elsewhere
//! the click opens the release page.
//!
//! The check sends nothing about the user: one anonymous GET, with Tabula's
//! version as the user agent.

use std::path::PathBuf;

use anyhow::{Context as _, anyhow, bail};
use serde_json::Value;

/// Where releases are published.
pub const REPO: &str = "fairhurt/tabula";

/// Downloads are only taken from here.
fn download_prefix() -> String {
    format!("https://github.com/{REPO}/releases/download/")
}

/// The version this build is.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// A release newer than this build.
#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    pub version: String,
    /// The release's page, for people to read or download from by hand.
    pub page: String,
    /// The Windows installer and its checksum file, when the release has them.
    pub installer: Option<Installer>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Installer {
    pub name: String,
    pub url: String,
    pub sha256_url: String,
}

/// Release builds check; development builds don't, unless asked to with
/// `TABULA_UPDATE_CHECK=1`.
pub fn enabled() -> bool {
    !cfg!(debug_assertions) || std::env::var_os("TABULA_UPDATE_CHECK").is_some()
}

/// `1.2.3` (a leading `v` is fine) as numbers. Anything after the patch
/// number, such as `-beta.1`, makes it a pre-release, which is never offered.
fn parse(version: &str) -> Option<[u64; 3]> {
    let v = version.trim().trim_start_matches('v');
    let mut parts = v.split('.');
    let out = [parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?];
    parts.next().is_none().then_some(out)
}

/// Whether `other` is a later version than `current`.
pub fn newer(current: &str, other: &str) -> bool {
    match (parse(current), parse(other)) {
        (Some(c), Some(o)) => o > c,
        _ => false,
    }
}

/// Read GitHub's "latest release" answer; `Some` when it is newer than
/// `current`.
pub fn parse_latest(json: &str, current: &str) -> Option<Release> {
    let v: Value = serde_json::from_str(json).ok()?;
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        return None;
    }
    let version = v["tag_name"].as_str()?.trim_start_matches('v').to_string();
    if !newer(current, &version) {
        return None;
    }
    let asset = |name: &str| -> Option<String> {
        v["assets"]
            .as_array()?
            .iter()
            .find(|a| a["name"].as_str() == Some(name))
            .and_then(|a| a["browser_download_url"].as_str())
            .filter(|u| u.starts_with(&download_prefix()))
            .map(str::to_string)
    };
    let name = format!("Tabula-Setup-{version}.exe");
    let installer = match (asset(&name), asset(&format!("{name}.sha256"))) {
        (Some(url), Some(sha256_url)) => Some(Installer { name, url, sha256_url }),
        _ => None,
    };
    Some(Release {
        page: v["html_url"]
            .as_str()
            .filter(|u| u.starts_with("https://github.com/"))
            .map_or_else(|| format!("https://github.com/{REPO}/releases/latest"), str::to_string),
        version,
        installer,
    })
}

fn get(url: &str) -> anyhow::Result<ureq::http::Response<ureq::Body>> {
    ureq::get(url)
        .header("User-Agent", &format!("tabula/{}", current()))
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| anyhow!("{e}"))
}

/// Ask GitHub for a newer release. Blocks; run it off the UI thread.
pub fn check() -> anyhow::Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = get(&url)?.body_mut().read_to_string().map_err(|e| anyhow!("{e}"))?;
    Ok(parse_latest(&body, current()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `data` has the SHA-256 that a `.sha256` file (`<hash>  <name>`)
/// names.
fn matches_checksum(data: &[u8], checksum_file: &str) -> bool {
    use sha2::Digest as _;
    let want = checksum_file.split_whitespace().next().unwrap_or("").to_lowercase();
    want.len() == 64 && want == hex(&sha2::Sha256::digest(data))
}

/// Download a release's installer and check it against its published
/// checksum. Blocks. Returns where the installer was saved.
pub fn download(installer: &Installer) -> anyhow::Result<PathBuf> {
    if !installer.url.starts_with(&download_prefix()) || !installer.sha256_url.starts_with(&download_prefix()) {
        bail!("the update isn't hosted on {REPO}'s releases");
    }
    let checksum = get(&installer.sha256_url)?.body_mut().read_to_string().map_err(|e| anyhow!("{e}"))?;
    let mut data = Vec::new();
    std::io::copy(&mut get(&installer.url)?.into_body().into_reader(), &mut data).context("downloading the update")?;
    if !matches_checksum(&data, &checksum) {
        bail!("the downloaded update doesn't match its published checksum, so it wasn't run");
    }
    let dir = std::env::temp_dir().join("tabula-update");
    std::fs::create_dir_all(&dir)?;
    // Only ever a plain file name inside that folder.
    let name: String = installer.name.chars().filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c)).collect();
    let path = dir.join(name);
    std::fs::write(&path, data)?;
    Ok(path)
}

/// Start the downloaded installer. It closes Tabula, replaces it and starts
/// the new version; the caller should quit straight after.
#[cfg(windows)]
pub fn run_installer(path: &std::path::Path) -> anyhow::Result<()> {
    std::process::Command::new(path)
        .args(["/SILENT", "/CLOSEAPPLICATIONS", "/NORESTART"])
        .spawn()
        .context("starting the installer")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.2.0", "0.2.1"));
        assert!(newer("0.2.9", "v0.10.0"));
        assert!(newer("0.9.9", "1.0.0"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.2.0", "0.1.9"));
        // Not offered: pre-releases and anything that isn't a version.
        assert!(!newer("0.2.0", "0.3.0-beta.1"));
        assert!(!newer("0.2.0", "nightly"));
    }

    fn latest(tag: &str, extra: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/fairhurt/tabula/releases/tag/{tag}","draft":false,"prerelease":false,
            "assets":[
              {{"name":"Tabula-Setup-0.3.0.exe","browser_download_url":"https://github.com/fairhurt/tabula/releases/download/{tag}/Tabula-Setup-0.3.0.exe"}},
              {{"name":"Tabula-Setup-0.3.0.exe.sha256","browser_download_url":"https://github.com/fairhurt/tabula/releases/download/{tag}/Tabula-Setup-0.3.0.exe.sha256"}}
              {extra}
            ]}}"#
        )
    }

    #[test]
    fn reads_the_latest_release() {
        let r = parse_latest(&latest("v0.3.0", ""), "0.2.0").expect("newer");
        assert_eq!(r.version, "0.3.0");
        assert_eq!(r.page, "https://github.com/fairhurt/tabula/releases/tag/v0.3.0");
        let i = r.installer.expect("an installer");
        assert_eq!(i.name, "Tabula-Setup-0.3.0.exe");
        assert!(i.sha256_url.ends_with("/Tabula-Setup-0.3.0.exe.sha256"));

        // Already on it, or ahead of it.
        assert_eq!(parse_latest(&latest("v0.3.0", ""), "0.3.0"), None);
        assert_eq!(parse_latest(&latest("v0.3.0", ""), "0.4.0"), None);
        // Not a release at all (a private repo answers 404 with a message).
        assert_eq!(parse_latest(r#"{"message":"Not Found"}"#, "0.2.0"), None);
        assert_eq!(parse_latest(&latest("v0.3.0", "").replace(r#""prerelease":false"#, r#""prerelease":true"#), "0.2.0"), None);
    }

    #[test]
    fn an_installer_from_elsewhere_is_ignored() {
        let json = latest("v0.3.0", "").replace(
            "https://github.com/fairhurt/tabula/releases/download/v0.3.0/Tabula-Setup-0.3.0.exe\"",
            "https://example.com/Tabula-Setup-0.3.0.exe\"",
        );
        let r = parse_latest(&json, "0.2.0").expect("newer");
        assert_eq!(r.installer, None);
        let bad = Installer {
            name: "x.exe".into(),
            url: "https://example.com/x.exe".into(),
            sha256_url: "https://example.com/x.exe.sha256".into(),
        };
        assert!(download(&bad).is_err());
    }

    #[test]
    fn checks_the_download_against_its_checksum() {
        // Too short to be a SHA-256 at all.
        assert!(!matches_checksum(b"tabula", "0d5f4f8f1a4ad3ae7a5d1b6b4e3d0a2c"));
        use sha2::Digest as _;
        let real = hex(&sha2::Sha256::digest(b"tabula"));
        assert!(matches_checksum(b"tabula", &format!("{real}  Tabula-Setup-0.3.0.exe\n")));
        assert!(matches_checksum(b"tabula", &real.to_uppercase()));
        assert!(!matches_checksum(b"tabulb", &real));
        assert!(!matches_checksum(b"tabula", ""));
    }
}
