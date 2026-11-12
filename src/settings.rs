//! App preferences, kept in `settings.json` beside `connections.json`.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::profiles::config_dir;

pub const DEFAULT_FONT_SIZE: f32 = 15.;
pub const MIN_FONT_SIZE: f32 = 11.;
pub const MAX_FONT_SIZE: f32 = 22.;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// UI text size in pixels; everything else scales from it.
    pub font_size: f32,
    /// The Ask AI agent last used ("claude" or "copilot").
    pub agent: Option<String>,
    /// The model picked for each agent, by agent key. Absent: the CLI's default.
    pub models: HashMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            font_size: DEFAULT_FONT_SIZE,
            agent: None,
            models: HashMap::new(),
        }
    }
}

impl Settings {
    pub fn font_size(&self) -> f32 {
        if self.font_size.is_finite() {
            self.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
        } else {
            DEFAULT_FONT_SIZE
        }
    }
}

fn path() -> PathBuf {
    config_dir().join("settings.json")
}

pub fn load() -> Settings {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(settings: &Settings) -> anyhow::Result<()> {
    let p = path();
    std::fs::create_dir_all(p.parent().expect("config path has a parent"))?;
    std::fs::write(&p, serde_json::to_string_pretty(settings)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let s: Settings = serde_json::from_str(r#"{"font_size": 40}"#).unwrap();
        assert_eq!(s.font_size(), MAX_FONT_SIZE);
        assert!(s.models.is_empty());
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.font_size(), DEFAULT_FONT_SIZE);
    }
}
