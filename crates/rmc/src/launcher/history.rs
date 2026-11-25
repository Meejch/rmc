//! Connection history (`~/.local/state/rmc/history.toml`): one entry per session start, for
//! the "Verlauf" page and "Letzte Verbindungen". Never contains passwords.

use serde::{Deserialize, Serialize};

const KEEP: usize = 200;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Entry {
    /// Favorite name (ad-hoc: the host).
    pub name: String,
    pub host: String,
    pub user: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub domain: String,
    pub entra: bool,
    /// Started via "Neue Verbindung" (no favorite).
    pub adhoc: bool,
    /// Local time "YYYY-MM-DD HH:MM:SS".
    pub started: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    /// The session was connected at least once.
    pub connected: bool,
    /// How it ended ("Sitzung beendet", "Getrennt: …"); empty while running.
    pub result: String,
    /// Path of the session log.
    pub log: String,
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    entry: Vec<Entry>,
}

fn path() -> std::path::PathBuf {
    crate::paths::state_dir().join("history.toml")
}

/// Entries, oldest first.
pub fn load() -> Vec<Entry> {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| toml::from_str::<File>(&t).ok())
        .map(|f| f.entry)
        .unwrap_or_default()
}

pub fn save(entries: &[Entry]) {
    let start = entries.len().saturating_sub(KEEP);
    let f = File {
        entry: entries[start..].to_vec(),
    };
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(t) = toml::to_string_pretty(&f) {
        let tmp = p.with_extension("toml.tmp");
        if std::fs::write(&tmp, t).is_ok() {
            let _ = std::fs::rename(&tmp, &p);
        }
    }
}

/// The most recent distinct targets (host + user), newest first.
pub fn recent(entries: &[Entry], n: usize) -> Vec<&Entry> {
    let mut out: Vec<&Entry> = Vec::new();
    for e in entries.iter().rev() {
        if !out
            .iter()
            .any(|o| o.host.eq_ignore_ascii_case(&e.host) && o.user == e.user)
        {
            out.push(e);
            if out.len() == n {
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(host: &str, user: &str, t: &str) -> Entry {
        Entry {
            host: host.into(),
            user: user.into(),
            started: t.into(),
            ..Default::default()
        }
    }

    #[test]
    fn recent_distinct() {
        let v = vec![
            e("a", "u", "1"),
            e("b", "u", "2"),
            e("A", "u", "3"),
            e("c", "x", "4"),
        ];
        let r: Vec<&str> = recent(&v, 5).iter().map(|e| e.started.as_str()).collect();
        assert_eq!(r, ["4", "3", "2"]);
        assert_eq!(recent(&v, 1).len(), 1);
    }
}
