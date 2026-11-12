//! The AI agents Tabula works with: Claude Code and GitHub Copilot CLI.
//!
//! Two jobs:
//! - Register `tabula mcp` in each agent's MCP config, so the agent can use
//!   your databases from its own terminal session.
//! - Find the agent's command, so the Ask AI panel can run it headless on
//!   your own subscription (see [`crate::ai`]).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The name Tabula's server is registered under.
pub const SERVER_NAME: &str = "tabula";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Copilot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpStatus {
    /// Not registered.
    Missing,
    /// Registered with this executable.
    Current,
    /// Registered with another executable (an older install).
    Outdated,
}

impl Agent {
    pub const ALL: [Agent; 2] = [Agent::Claude, Agent::Copilot];

    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Copilot => "GitHub Copilot CLI",
        }
    }

    /// A name short enough for a button.
    pub fn short_label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Copilot => "Copilot",
        }
    }

    /// The key this agent's preferences are saved under.
    pub fn key(self) -> &'static str {
        self.command()
    }

    pub fn from_key(key: &str) -> Option<Agent> {
        Agent::ALL.into_iter().find(|a| a.key() == key)
    }

    /// Models offered in the Ask AI panel, as (label, `--model` value). The
    /// agent's own default is always offered too, and any other id can be typed.
    pub fn models(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Agent::Claude => &[("Fable", "fable"), ("Opus", "opus"), ("Sonnet", "sonnet"), ("Haiku", "haiku")],
            Agent::Copilot => &[("Auto", "auto")],
        }
    }

    /// What to show for a `--model` value.
    pub fn model_label(self, model: Option<&str>) -> String {
        match model {
            None => "Default".into(),
            Some(m) => self
                .models()
                .iter()
                .find(|(_, id)| *id == m)
                .map_or_else(|| m.to_string(), |(label, _)| label.to_string()),
        }
    }

    pub fn command(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Copilot => "copilot",
        }
    }

    pub fn install_hint(self) -> &'static str {
        match self {
            Agent::Claude => "npm install -g @anthropic-ai/claude-code, then run `claude` once to sign in",
            Agent::Copilot => "npm install -g @github/copilot, then run `copilot` once to sign in",
        }
    }

    /// The file the agent reads its user-level MCP servers from.
    pub fn mcp_config(self) -> Option<PathBuf> {
        match self {
            Agent::Claude => match std::env::var_os("CLAUDE_CONFIG_DIR") {
                Some(d) => Some(PathBuf::from(d).join(".claude.json")),
                None => home().map(|h| h.join(".claude.json")),
            },
            Agent::Copilot => std::env::var_os("COPILOT_HOME")
                .map(PathBuf::from)
                .or_else(|| home().map(|h| h.join(".copilot")))
                .map(|d| d.join("mcp-config.json")),
        }
    }

    pub fn installed(self) -> bool {
        find_on_path(self.command()).is_some()
    }

    fn entry(self, exe: &str) -> Value {
        match self {
            Agent::Claude => json!({"type": "stdio", "command": exe, "args": ["mcp"], "env": {}}),
            Agent::Copilot => {
                json!({"type": "local", "command": exe, "args": ["mcp"], "tools": ["*"]})
            }
        }
    }

    pub fn mcp_status(self, exe: &Path) -> McpStatus {
        let Some(text) = self.mcp_config().and_then(|c| std::fs::read_to_string(c).ok()) else {
            return McpStatus::Missing;
        };
        let command = serde_json::from_str::<Value>(&strip_json_comments(&text))
            .ok()
            .and_then(|v| v["mcpServers"][SERVER_NAME]["command"].as_str().map(String::from));
        match command {
            None => McpStatus::Missing,
            Some(c) if same_path(&c, &exe.to_string_lossy()) => McpStatus::Current,
            Some(_) => McpStatus::Outdated,
        }
    }

    /// Register `exe mcp` with this agent, keeping everything else in its
    /// config. The first time, a copy of the original is kept next to it.
    pub fn connect(self, exe: &Path) -> std::io::Result<PathBuf> {
        let cfg = self.mcp_config().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "no home folder")
        })?;
        let original = std::fs::read_to_string(&cfg).ok();
        let mut v: Value = match original.as_deref().map(str::trim) {
            None | Some("") => json!({}),
            Some(s) => serde_json::from_str(&strip_json_comments(s)).map_err(|e| bad_data(&cfg, e))?,
        };
        let obj = v
            .as_object_mut()
            .ok_or_else(|| bad_data(&cfg, "not a JSON object"))?;
        let servers = obj.entry("mcpServers").or_insert_with(|| json!({}));
        if !servers.is_object() {
            *servers = json!({});
        }
        servers[SERVER_NAME] = self.entry(&exe.to_string_lossy());
        if let Some(o) = &original {
            let backup = cfg.with_extension("json.before-tabula");
            if !backup.exists() {
                std::fs::write(&backup, o)?;
            }
        }
        if let Some(d) = cfg.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = cfg.with_extension("tabula.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default())?;
        std::fs::rename(&tmp, &cfg)?;
        Ok(cfg)
    }

    /// Remove Tabula from this agent's MCP config. False if it wasn't there.
    pub fn disconnect(self) -> std::io::Result<bool> {
        let Some(cfg) = self.mcp_config() else {
            return Ok(false);
        };
        let Ok(text) = std::fs::read_to_string(&cfg) else {
            return Ok(false);
        };
        let mut v: Value =
            serde_json::from_str(&strip_json_comments(&text)).map_err(|e| bad_data(&cfg, e))?;
        let removed = v["mcpServers"]
            .as_object_mut()
            .and_then(|o| o.remove(SERVER_NAME))
            .is_some();
        if removed {
            let tmp = cfg.with_extension("tabula.tmp");
            std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default())?;
            std::fs::rename(&tmp, &cfg)?;
        }
        Ok(removed)
    }
}

fn bad_data(p: &Path, e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("{} isn't valid JSON ({e}); not changing it", p.display()),
    )
}

fn home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn same_path(a: &str, b: &str) -> bool {
    let n = |s: &str| s.replace('\\', "/").to_lowercase();
    n(a) == n(b)
}

/// JSON with `//` and `/* */` comments removed.
pub fn strip_json_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut in_str = false;
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The program to run for `command`. On Windows, npm installs tools as
/// `.cmd` shims, which mangle multi-line arguments; the shim's Node script is
/// run directly instead.
pub fn resolve_command(command: &str) -> (PathBuf, Vec<String>) {
    let found = find_on_path(command).unwrap_or_else(|| PathBuf::from(command));
    let ext = found
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if ext == "cmd" || ext == "bat" {
        if let Some((node, script)) = node_script_of_shim(&found) {
            return (node, vec![script.to_string_lossy().to_string()]);
        }
        return (
            PathBuf::from("cmd.exe"),
            vec!["/C".into(), found.to_string_lossy().to_string()],
        );
    }
    (found, vec![])
}

/// Look `command` up on PATH (with Windows extensions), and in the usual
/// install folders a GUI app's PATH may lack.
pub fn find_on_path(command: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat", ""]
    } else {
        &[""]
    };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(h) = home() {
        dirs.push(h.join(".local/bin"));
        dirs.push(h.join(".claude/local"));
        dirs.push(h.join(".npm-global/bin"));
        if cfg!(windows) {
            if let Some(appdata) = std::env::var_os("APPDATA") {
                dirs.push(PathBuf::from(appdata).join("npm"));
            }
        }
    }
    dirs.iter().find_map(|dir| {
        exts.iter()
            .map(|e| dir.join(format!("{command}{e}")))
            .find(|c| c.is_file())
    })
}

/// The Node binary and script an npm `.cmd` shim runs.
fn node_script_of_shim(shim: &Path) -> Option<(PathBuf, PathBuf)> {
    let text = std::fs::read_to_string(shim).ok()?;
    let dir = shim.parent()?;
    let script = text
        .split('"')
        .filter(|s| {
            let l = s.to_lowercase();
            l.ends_with(".js") || l.ends_with(".cjs") || l.ends_with(".mjs")
        })
        .find_map(|s| {
            let rel = s
                .trim_start_matches("%dp0%")
                .trim_start_matches("%~dp0")
                .trim_start_matches(['\\', '/']);
            let full = dir.join(rel.replace('\\', std::path::MAIN_SEPARATOR_STR));
            full.exists().then_some(full)
        })?;
    let local = dir.join("node.exe");
    let node = if local.exists() {
        local
    } else {
        PathBuf::from("node")
    };
    Some((node, script))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_and_removes_itself_keeping_other_servers() {
        let d = tempfile::tempdir().unwrap();
        // SAFETY: tests touching these variables run only here.
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", d.path());
            std::env::set_var("COPILOT_HOME", d.path().join("copilot"));
        }
        let exe = Path::new("/opt/tabula/tabula");
        let claude = d.path().join(".claude.json");
        std::fs::write(
            &claude,
            r#"{"numStartups": 3, // a comment
               "mcpServers": {"docs": {"command": "docs"}}}"#,
        )
        .unwrap();
        assert_eq!(Agent::Claude.mcp_status(exe), McpStatus::Missing);
        Agent::Claude.connect(exe).unwrap();
        assert_eq!(Agent::Claude.mcp_status(exe), McpStatus::Current);
        assert_eq!(Agent::Claude.mcp_status(Path::new("/other")), McpStatus::Outdated);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(v["numStartups"], 3);
        assert_eq!(v["mcpServers"]["docs"]["command"], "docs");
        assert_eq!(v["mcpServers"]["tabula"]["args"][0], "mcp");
        assert!(d.path().join(".claude.json.before-tabula").exists());

        Agent::Copilot.connect(exe).unwrap();
        let v: Value = serde_json::from_str(
            &std::fs::read_to_string(d.path().join("copilot/mcp-config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["mcpServers"]["tabula"]["type"], "local");

        assert!(Agent::Claude.disconnect().unwrap());
        assert!(!Agent::Claude.disconnect().unwrap());
        assert_eq!(Agent::Claude.mcp_status(exe), McpStatus::Missing);
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["docs"]["command"], "docs");
    }
}
