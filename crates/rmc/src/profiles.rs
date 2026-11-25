//! Favorites and settings: global defaults, per favorite
//! overridable. Stored in `~/.config/rmc/settings.toml` and `favorites.toml` (never secrets;
//! tokens and passwords go to the secret store). A favorite becomes the command line of a
//! session process (`rmc connect <name>`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A local folder shown in Windows as a drive.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Drive {
    /// Name in Windows (e.g. "Linux-Home").
    pub name: String,
    /// Local path (`~` allowed).
    pub path: String,
}

/// Settings with a value for every option (the global defaults, or a resolved favorite).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Options {
    // Anzeige
    /// Monitors (connector names); empty = all.
    pub monitors: Vec<String>,
    /// Primary monitor (connector name); empty = KDE's order.
    pub primary: String,
    pub start_windowed: bool,
    pub bar_pinned: bool,
    // Audio & Geräte
    pub sound: bool,
    pub microphone: bool,
    pub camera: bool,
    // Freigaben
    pub clipboard: bool,
    /// Redirected folders.
    pub drives: Vec<Drive>,
    /// Bandwidth for files over the clipboard in Mbit/s (0 = unlimited).
    pub clipboard_limit: u32,
    /// Redirected CUPS printers (names as in `lpstat -e`).
    pub printers: Vec<String>,
    // Leistung
    /// Graphics decoding: auto, nvidia, vaapi, software.
    pub decoder: String,
    pub avc420_only: bool,
    pub reverse_filter: bool,
    // Erweitert
    pub auto_reconnect: bool,
    pub keyboard_grab: bool,
    /// Keep Windows active (small mouse moves) from the start.
    pub keep_active: bool,
    /// login (own WebView with stored sign-in), webview, xdg
    pub login_helper: String,
    pub log_level: String,
    // Erreichbarkeit (launcher)
    /// Check whether the favorite answers RDP (status light in the launcher).
    pub reach_check: bool,
    /// Check intervals in seconds, one stage each (the last one repeats).
    pub reach_intervals: Vec<u32>,
    /// Duration of one stage in seconds.
    pub reach_stage: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            monitors: Vec::new(),
            primary: String::new(),
            start_windowed: false,
            bar_pinned: true,
            sound: true,
            microphone: true,
            camera: true,
            clipboard: true,
            drives: Vec::new(),
            clipboard_limit: 50,
            printers: Vec::new(),
            decoder: "auto".into(),
            avc420_only: false,
            reverse_filter: true,
            auto_reconnect: true,
            keyboard_grab: true,
            keep_active: false,
            login_helper: "login".into(),
            log_level: "INFO".into(),
            reach_check: true,
            reach_intervals: vec![5, 15, 25, 40, 60],
            reach_stage: 60,
        }
    }
}

/// Per favorite overrides (None = global default).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Overrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitors: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_windowed: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bar_pinned: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub microphone: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub camera: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub drives: Option<Vec<Drive>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clipboard_limit: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub printers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decoder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avc420_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverse_filter: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_reconnect: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyboard_grab: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login_helper: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reach_check: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reach_intervals: Option<Vec<u32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reach_stage: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Favorite {
    pub name: String,
    pub host: String,
    /// UPN for Entra ID; user name (or user@domain) otherwise.
    pub user: String,
    /// Windows domain for password logins (empty = local account or UPN in `user`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub domain: String,
    /// Entra ID login (RDS AAD); false = user name and password (NLA).
    pub entra: bool,
    /// The launcher connects to this favorite when it starts.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub connect_on_start: bool,
    /// Last successful connection (local time, "YYYY-MM-DD HH:MM").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<String>,
    pub overrides: Overrides,
}

impl Favorite {
    pub fn new(name: &str, host: &str, user: &str) -> Self {
        Self {
            name: name.into(),
            host: host.into(),
            user: user.into(),
            entra: true,
            ..Default::default()
        }
    }

    /// The favorite's effective options.
    pub fn resolve(&self, d: &Options) -> Options {
        let o = &self.overrides;
        Options {
            monitors: o.monitors.clone().unwrap_or_else(|| d.monitors.clone()),
            primary: o.primary.clone().unwrap_or_else(|| d.primary.clone()),
            start_windowed: o.start_windowed.unwrap_or(d.start_windowed),
            bar_pinned: o.bar_pinned.unwrap_or(d.bar_pinned),
            sound: o.sound.unwrap_or(d.sound),
            microphone: o.microphone.unwrap_or(d.microphone),
            camera: o.camera.unwrap_or(d.camera),
            clipboard: o.clipboard.unwrap_or(d.clipboard),
            drives: o.drives.clone().unwrap_or_else(|| d.drives.clone()),
            clipboard_limit: o.clipboard_limit.unwrap_or(d.clipboard_limit),
            printers: o.printers.clone().unwrap_or_else(|| d.printers.clone()),
            decoder: o.decoder.clone().unwrap_or_else(|| d.decoder.clone()),
            avc420_only: o.avc420_only.unwrap_or(d.avc420_only),
            reverse_filter: o.reverse_filter.unwrap_or(d.reverse_filter),
            auto_reconnect: o.auto_reconnect.unwrap_or(d.auto_reconnect),
            keyboard_grab: o.keyboard_grab.unwrap_or(d.keyboard_grab),
            keep_active: o.keep_active.unwrap_or(d.keep_active),
            login_helper: o
                .login_helper
                .clone()
                .unwrap_or_else(|| d.login_helper.clone()),
            log_level: o.log_level.clone().unwrap_or_else(|| d.log_level.clone()),
            reach_check: o.reach_check.unwrap_or(d.reach_check),
            reach_intervals: o
                .reach_intervals
                .clone()
                .unwrap_or_else(|| d.reach_intervals.clone()),
            reach_stage: o.reach_stage.unwrap_or(d.reach_stage),
        }
    }

    /// Login name as Windows shows it: DOMAIN\\user, or the user name alone.
    pub fn account(&self) -> String {
        match self.domain.trim() {
            "" => self.user.trim().to_string(),
            d => format!("{d}\\{}", self.user.trim()),
        }
    }

    /// Attributes of the stored password (secret store).
    pub fn password_attrs(&self) -> [(String, String); 4] {
        [
            ("application".into(), rdp_session::secret::APP.into()),
            ("kind".into(), "password".into()),
            ("account".into(), self.account()),
            ("host".into(), self.host.trim().to_string()),
        ]
    }

    /// Host and port ("host:port", default 3389; IPv6 literals in brackets keep their colons).
    pub fn host_port(&self) -> (String, u16) {
        let h = self.host.trim();
        match h.rsplit_once(':') {
            Some((host, p)) if !host.contains(':') => match p.parse::<u16>() {
                Ok(port) => (host.to_string(), port),
                Err(_) => (h.to_string(), 3389),
            },
            _ => (h.trim_matches(['[', ']']).to_string(), 3389),
        }
    }

    /// Command line (after the program name) of the session process.
    pub fn session_args(&self, d: &Options) -> Vec<String> {
        let o = self.resolve(d);
        // "host:port" (not for IPv6 literals in brackets)
        let (host, port) = match self.host.rsplit_once(':') {
            Some((h, p)) if !h.contains(':') && p.parse::<u16>().is_ok() => (h, Some(p)),
            _ => (self.host.as_str(), None),
        };
        let mut a = vec![
            "--host".into(),
            host.to_string(),
            "--name".into(),
            self.name.clone(),
        ];
        if let Some(p) = port {
            a.extend(["--port".into(), p.to_string()]);
        }
        if !self.user.is_empty() {
            a.extend(["--user".into(), self.user.clone()]);
        }
        if !self.entra && !self.domain.trim().is_empty() {
            a.extend(["--domain".into(), self.domain.trim().to_string()]);
        }
        if self.entra {
            a.extend([
                "--aad".into(),
                "--aad-helper".into(),
                o.login_helper.clone(),
            ]);
        }
        if !o.monitors.is_empty() {
            a.extend(["--monitors".into(), o.monitors.join(",")]);
        }
        if !o.primary.is_empty() {
            a.extend(["--primary".into(), o.primary.clone()]);
        }
        let flags = [
            (o.start_windowed, "--windowed"),
            (!o.sound, "--no-sound"),
            (!o.microphone, "--no-mic"),
            (!o.camera, "--no-camera"),
            (!o.clipboard, "--no-clipboard"),
            (o.avc420_only, "--avc420-only"),
            (!o.reverse_filter, "--no-reverse-filter"),
            (!o.auto_reconnect, "--no-reconnect"),
            (!o.keyboard_grab, "--no-grab"),
            (o.keep_active, "--keep-active"),
        ];
        a.extend(flags.iter().filter(|f| f.0).map(|f| f.1.to_string()));
        a.extend(["--clipboard-limit".into(), o.clipboard_limit.to_string()]);
        if o.decoder != "auto" && !o.decoder.is_empty() {
            a.extend(["--decoder".into(), o.decoder.clone()]);
        }
        for p in &o.printers {
            a.extend(["--printer".into(), p.clone()]);
        }
        for dr in &o.drives {
            if !dr.name.trim().is_empty() && !dr.path.trim().is_empty() {
                a.extend([
                    "--drive".into(),
                    format!("{}={}", dr.name.trim(), dr.path.trim()),
                ]);
            }
        }
        a.push(
            if o.bar_pinned {
                "--bar-pinned"
            } else {
                "--bar-autohide"
            }
            .into(),
        );
        a.extend(["--log-level".into(), o.log_level.clone()]);
        a
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub defaults: Options,
    /// Closing the launcher window keeps the app in the tray.
    pub close_to_tray: Option<bool>,
    #[serde(rename = "favorite")]
    pub favorites: Vec<Favorite>,
}

impl Settings {
    /// `~/.config/rmc/settings.toml`: launcher settings and defaults.
    pub fn settings_path() -> PathBuf {
        crate::paths::config_dir().join("settings.toml")
    }

    /// `~/.config/rmc/favorites.toml`: the favorites.
    pub fn favorites_path() -> PathBuf {
        crate::paths::config_dir().join("favorites.toml")
    }

    pub fn load() -> Result<Self> {
        let app: AppFile = read_toml(&Self::settings_path())?.unwrap_or_default();
        let favs: FavFile = read_toml(&Self::favorites_path())?.unwrap_or_default();
        Ok(Self {
            defaults: app.defaults,
            close_to_tray: app.close_to_tray,
            favorites: favs.favorite,
        })
    }

    pub fn save(&self) -> Result<()> {
        write_toml(
            &Self::settings_path(),
            &AppFile {
                close_to_tray: self.close_to_tray,
                defaults: self.defaults.clone(),
            },
        )?;
        write_toml(
            &Self::favorites_path(),
            &FavFile {
                favorite: self.favorites.clone(),
            },
        )
    }

    pub fn close_to_tray(&self) -> bool {
        self.close_to_tray.unwrap_or(true)
    }

    pub fn find(&self, name: &str) -> Option<&Favorite> {
        self.favorites.iter().find(|f| f.name == name)
    }
}

/// Content of settings.toml.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct AppFile {
    #[serde(skip_serializing_if = "Option::is_none")]
    close_to_tray: Option<bool>,
    defaults: Options,
}

/// Content of favorites.toml.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
struct FavFile {
    favorite: Vec<Favorite>,
}

fn read_toml<T: serde::de::DeserializeOwned>(p: &std::path::Path) -> Result<Option<T>> {
    match std::fs::read_to_string(p) {
        Ok(t) => Ok(Some(
            toml::from_str(&t).with_context(|| format!("{}", p.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("{}", p.display())),
    }
}

fn write_toml<T: Serialize>(p: &std::path::Path, v: &T) -> Result<()> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = p.with_extension("toml.tmp");
    std::fs::write(&tmp, toml::to_string_pretty(v)?)?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

/// Session logs: `~/.local/state/rmc/logs`.
pub fn log_dir() -> PathBuf {
    crate::paths::log_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides_and_args() {
        let mut f = Favorite::new("Arbeit", "host.example", "user@example.com");
        f.overrides.camera = Some(false);
        f.overrides.monitors = Some(vec!["DP-5".into()]);
        let d = Options::default();
        let o = f.resolve(&d);
        assert!(!o.camera && o.sound && o.bar_pinned);
        let a = f.session_args(&d);
        assert!(a.windows(2).any(|w| w == ["--monitors", "DP-5"]));
        assert!(a.contains(&"--no-camera".to_string()));
        assert!(a.contains(&"--aad".to_string()) && a.contains(&"login".to_string()));
        assert!(!a.contains(&"--no-sound".to_string()));
    }

    #[test]
    fn toml_roundtrip() {
        let mut s = Settings::default();
        let mut f = Favorite::new("Arbeit", "h", "u");
        f.overrides.sound = Some(false);
        s.favorites.push(f);
        let t = toml::to_string_pretty(&s).unwrap();
        assert!(t.contains("[[favorite]]"));
        assert_eq!(toml::from_str::<Settings>(&t).unwrap(), s);
    }
}
