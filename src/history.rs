//! Query history: every statement run from a query tab, kept as JSON lines
//! in `history.jsonl` beside `connections.json`.

use std::io::Write as _;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Entries kept when the file is trimmed, and the size that triggers it.
const KEEP: usize = 1000;
const TRIM_AT: usize = 2000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// When it ran, RFC 3339 in local time.
    pub at: String,
    pub connection: String,
    pub database: String,
    pub sql: String,
    pub elapsed_ms: u64,
    /// Rows returned or affected; `None` when it failed.
    #[serde(default)]
    pub rows: Option<u64>,
    #[serde(default)]
    pub error: Option<String>,
}

impl Entry {
    /// "14:02" for today, "Nov 10 14:02" for earlier.
    pub fn when(&self) -> String {
        match chrono::DateTime::parse_from_rfc3339(&self.at) {
            Ok(t) => {
                let t = t.with_timezone(&chrono::Local);
                if t.date_naive() == chrono::Local::now().date_naive() {
                    t.format("%H:%M").to_string()
                } else {
                    t.format("%b %-d %H:%M").to_string()
                }
            }
            Err(_) => self.at.clone(),
        }
    }

    /// Whether every word of `query` appears in the SQL, connection or database.
    pub fn matches(&self, query: &str) -> bool {
        let hay = format!("{} {} {}", self.sql, self.connection, self.database).to_lowercase();
        query.to_lowercase().split_whitespace().all(|w| hay.contains(w))
    }
}

/// The history, oldest first. Without a path nothing is written.
pub struct History {
    path: Option<PathBuf>,
    entries: Vec<Entry>,
}

impl History {
    /// The saved history in Tabula's config directory.
    pub fn open() -> History {
        History::at(crate::profiles::config_dir().join("history.jsonl"))
    }

    pub fn at(path: PathBuf) -> History {
        let mut entries: Vec<Entry> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        if entries.len() > KEEP {
            entries.drain(..entries.len() - KEEP);
        }
        History { path: Some(path), entries }
    }

    /// Kept for this run only (headless screenshots).
    pub fn in_memory() -> History {
        History { path: None, entries: vec![] }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Add a run. Running the same SQL on the same database again replaces
    /// the previous entry instead of piling up.
    pub fn record(&mut self, e: Entry) -> anyhow::Result<()> {
        let repeat = self.entries.last().is_some_and(|l| {
            l.sql == e.sql && l.connection == e.connection && l.database == e.database
        });
        if repeat {
            self.entries.pop();
        }
        self.entries.push(e);
        if repeat || self.entries.len() > TRIM_AT {
            if self.entries.len() > TRIM_AT {
                self.entries.drain(..self.entries.len() - KEEP);
            }
            return self.rewrite();
        }
        let Some(path) = &self.path else { return Ok(()) };
        std::fs::create_dir_all(path.parent().expect("history path has a parent"))?;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        let line = serde_json::to_string(self.entries.last().expect("just pushed"))?;
        writeln!(f, "{line}")?;
        Ok(())
    }

    pub fn clear(&mut self) -> anyhow::Result<()> {
        self.entries.clear();
        self.rewrite()
    }

    fn rewrite(&self) -> anyhow::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        std::fs::create_dir_all(path.parent().expect("history path has a parent"))?;
        let mut text = String::new();
        for e in &self.entries {
            text.push_str(&serde_json::to_string(e)?);
            text.push('\n');
        }
        std::fs::write(path, text)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(sql: &str) -> Entry {
        Entry {
            at: chrono::Local::now().to_rfc3339(),
            connection: "demo".into(),
            database: "shop".into(),
            sql: sql.into(),
            elapsed_ms: 3,
            rows: Some(1),
            error: None,
        }
    }

    #[test]
    fn records_reloads_and_collapses_repeats() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("history.jsonl");
        let mut h = History::at(path.clone());
        h.record(entry("select 1")).unwrap();
        h.record(entry("select\n  2")).unwrap();
        h.record(entry("select\n  2")).unwrap();
        assert_eq!(h.entries().len(), 2);

        let again = History::at(path.clone());
        assert_eq!(again.entries(), h.entries());
        assert_eq!(again.entries()[1].sql, "select\n  2");

        h.clear().unwrap();
        assert!(History::at(path).entries().is_empty());
    }

    #[test]
    fn trims_when_it_grows() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("history.jsonl");
        let mut h = History::at(path.clone());
        for i in 0..=TRIM_AT {
            h.record(entry(&format!("select {i}"))).unwrap();
        }
        assert_eq!(h.entries().len(), KEEP);
        assert_eq!(h.entries().last().unwrap().sql, format!("select {TRIM_AT}"));
        assert_eq!(History::at(path).entries().len(), KEEP);
    }

    #[test]
    fn search_matches_every_word() {
        let e = entry("SELECT * FROM shop.orders WHERE status = 'paid'");
        assert!(e.matches("orders PAID"));
        assert!(e.matches("demo"));
        assert!(e.matches(""));
        assert!(!e.matches("orders refunds"));
    }
}
