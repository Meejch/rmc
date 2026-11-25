//! "Log" page: a session log, split into time, level, source and message, coloured and
//! filtered. Follows a running session's log as it grows.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, RichText};

use super::theme;

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Level {
    /// FreeRDP details (TRACE/DEBUG) and statistics.
    Detail,
    Info,
    /// Lines of RMC itself (connection steps, results).
    App,
    Warn,
    Error,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub time: String,
    pub level: Level,
    pub source: String,
    pub text: String,
}

/// Splits one log line. FreeRDP: `[hh:mm:ss:ms] [pid:tid] [LEVEL][tag] - [func]: message`.
pub fn parse(raw: &str) -> Option<Line> {
    let raw = raw.trim_end();
    if raw.is_empty() || raw.starts_with("RDPC-CERT\t") {
        return None;
    }
    if let Some(rest) = raw.strip_prefix('[')
        && let Some((time, rest)) = rest.split_once("] [")
        && time.len() >= 8
        && time.as_bytes()[2] == b':'
    {
        let rest = rest.split_once("] ").map_or(rest, |(_, r)| r);
        let (lvl, rest) = rest
            .strip_prefix('[')
            .and_then(|r| r.split_once("]["))
            .unwrap_or(("INFO", rest));
        let (tag, msg) = rest.split_once("] - ").unwrap_or(("", rest));
        let msg = msg
            .strip_prefix('[')
            .and_then(|m| m.split_once("]: "))
            .map_or(msg, |(_, m)| m);
        let level = match lvl {
            "ERROR" | "FATAL" => Level::Error,
            "WARN" => Level::Warn,
            "INFO" => Level::Info,
            _ => Level::Detail,
        };
        let source = tag
            .trim_start_matches("com.freerdp.")
            .trim_start_matches("com.winpr.")
            .to_string();
        return Some(Line {
            time: time[..8].to_string(),
            level,
            source,
            text: msg.to_string(),
        });
    }
    // RMC's own lines
    let (level, source) = if raw.starts_with("Getrennt:") || raw.starts_with("Error:") {
        (Level::Error, "RMC")
    } else if raw.starts_with("Fehler") || raw.contains("fehlgeschlagen") {
        (Level::Warn, "RMC")
    } else if raw.starts_with('[') || raw.contains("Frames quittiert") || raw.starts_with("  ") {
        (Level::Detail, "")
    } else {
        (Level::App, "RMC")
    };
    Some(Line {
        time: String::new(),
        level,
        source: source.into(),
        text: raw.to_string(),
    })
}

/// State of the log page.
pub struct LogView {
    pub file: Option<PathBuf>,
    lines: Vec<Line>,
    offset: u64,
    partial: String,
    pub show_detail: bool,
    pub show_info: bool,
    pub filter: String,
    pub follow: bool,
}

impl Default for LogView {
    fn default() -> Self {
        Self {
            file: None,
            lines: Vec::new(),
            offset: 0,
            partial: String::new(),
            show_detail: false,
            show_info: true,
            filter: String::new(),
            follow: true,
        }
    }
}

impl LogView {
    pub fn open(&mut self, p: &Path) {
        if self.file.as_deref() != Some(p) {
            self.file = Some(p.to_path_buf());
            self.lines.clear();
            self.offset = 0;
            self.partial.clear();
        }
    }

    /// Reads what was appended since the last call.
    pub fn refresh(&mut self) {
        let Some(p) = &self.file else { return };
        let Ok(mut f) = std::fs::File::open(p) else {
            return;
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            self.lines.clear();
            self.offset = 0;
            self.partial.clear();
        }
        if len == self.offset || f.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_ok() {
            self.offset += buf.len() as u64;
            self.partial.push_str(&String::from_utf8_lossy(&buf));
            while let Some(i) = self.partial.find('\n') {
                let l: String = self.partial.drain(..=i).collect();
                if let Some(line) = parse(&l) {
                    self.lines.push(line);
                }
            }
        }
    }

    fn visible(&self, l: &Line) -> bool {
        let lvl_ok = match l.level {
            Level::Detail => self.show_detail,
            Level::Info => self.show_info,
            _ => true,
        };
        lvl_ok
            && (self.filter.is_empty()
                || l.text.to_lowercase().contains(&self.filter.to_lowercase())
                || l.source
                    .to_lowercase()
                    .contains(&self.filter.to_lowercase()))
    }

    pub fn counts(&self) -> (usize, usize) {
        let e = self
            .lines
            .iter()
            .filter(|l| l.level == Level::Error)
            .count();
        let w = self.lines.iter().filter(|l| l.level == Level::Warn).count();
        (e, w)
    }

    /// The lines as a table (virtualised: only visible rows are laid out).
    pub fn body(&self, ui: &mut egui::Ui) {
        let rows: Vec<&Line> = self.lines.iter().filter(|l| self.visible(l)).collect();
        if rows.is_empty() {
            ui.add_space(20.0);
            ui.label(theme::weak(if self.file.is_none() {
                "Noch kein Log vorhanden."
            } else {
                "Keine Zeilen für diesen Filter."
            }));
            return;
        }
        let row_h = 20.0;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .stick_to_bottom(self.follow)
            .show_rows(ui, row_h, rows.len(), |ui, range| {
                for l in &rows[range] {
                    ui.horizontal(|ui| {
                        ui.set_height(row_h);
                        ui.add_sized(
                            [64.0, row_h],
                            egui::Label::new(RichText::new(&l.time).monospace().color(theme::WEAK)),
                        );
                        let (badge, color) = match l.level {
                            Level::Error => ("FEHLER", theme::RED),
                            Level::Warn => ("WARNUNG", theme::YELLOW),
                            Level::App => ("RMC", theme::ACCENT_HI),
                            Level::Info => ("INFO", theme::WEAK),
                            Level::Detail => ("DETAIL", theme::GREY),
                        };
                        ui.add_sized(
                            [72.0, row_h],
                            egui::Label::new(RichText::new(badge).small().strong().color(color)),
                        );
                        ui.add_sized(
                            [190.0, row_h],
                            egui::Label::new(
                                RichText::new(short(&l.source, 28))
                                    .small()
                                    .color(theme::WEAK),
                            )
                            .truncate(),
                        );
                        let text_color = match l.level {
                            Level::Error => Color32::from_rgb(255, 160, 145),
                            Level::Warn => Color32::from_rgb(240, 210, 140),
                            Level::Detail => theme::WEAK,
                            _ => theme::TEXT,
                        };
                        ui.add(
                            egui::Label::new(RichText::new(&l.text).monospace().color(text_color))
                                .extend(),
                        );
                    });
                }
            });
    }
}

fn short(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let tail: String = s
            .chars()
            .rev()
            .take(n - 1)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("…{tail}")
    }
}

/// Log files, newest first: (path, favorite name, "YYYY-MM-DD HH:MM").
pub fn files(dir: &Path) -> Vec<(PathBuf, String, String)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<(PathBuf, String, String)> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_string_lossy().to_string();
            // <name>-YYYYMMDD-HHMMSS
            let (rest, time) = stem.rsplit_once('-')?;
            let (name, date) = rest.rsplit_once('-')?;
            if date.len() != 8 || time.len() != 6 {
                return None;
            }
            let label = format!(
                "{}-{}-{} {}:{}",
                &date[..4],
                &date[4..6],
                &date[6..],
                &time[..2],
                &time[2..4]
            );
            Some((p.clone(), name.replace('_', " "), label))
        })
        .collect();
    v.sort_by(|a, b| b.2.cmp(&a.2));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_freerdp_line() {
        let l = parse("[11:19:24:535] [455815:0006f522] [WARN][com.freerdp.channels.rdpsnd.client] - [rdpsnd_check_pulse]: pulse->stream=(nil)").unwrap();
        assert_eq!(l.time, "11:19:24");
        assert_eq!(l.level, Level::Warn);
        assert_eq!(l.source, "channels.rdpsnd.client");
        assert_eq!(l.text, "pulse->stream=(nil)");
    }

    #[test]
    fn parse_own_lines() {
        assert_eq!(
            parse("Verbunden: Desktop 7440x2560").unwrap().level,
            Level::App
        );
        assert_eq!(
            parse("Getrennt: connection failed").unwrap().level,
            Level::Error
        );
        assert_eq!(
            parse("10 Frames quittiert (2.0/s)").unwrap().level,
            Level::Detail
        );
        assert!(parse("RDPC-CERT\tnew\th").is_none());
    }

    #[test]
    fn file_names() {
        let dir = std::env::temp_dir().join(format!("rmc-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for n in [
            "Arbeit-20261125-083544.log",
            "Test_Server-20261125-111917.log",
            "x.txt",
        ] {
            std::fs::write(dir.join(n), b"").unwrap();
        }
        let f = files(&dir);
        assert_eq!(f.len(), 2);
        assert_eq!(
            (f[0].1.as_str(), f[0].2.as_str()),
            ("Test Server", "2026-11-25 11:19")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
