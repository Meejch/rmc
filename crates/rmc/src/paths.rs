//! Directories of the application (XDG base directories):
//!
//! - `~/.config/rmc/`: `settings.toml` (launcher, defaults), `favorites.toml`, `freerdp/`
//!   (FreeRDP's files, e.g. the trusted server certificates)
//! - `~/.local/state/rmc/`: `session.toml` (remembered bar toggles), `logs/`
//! - `~/.local/share/rmc/`: `login/` (browser profile of the login window), `secrets/`
//!   (encrypted fallback store)
//! - `~/.cache/rmc/`: `clipboard/` (files pasted from Windows)

use std::path::PathBuf;

pub const APP: &str = "rmc";

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(fallback)))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_base() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}
fn state_base() -> PathBuf {
    xdg("XDG_STATE_HOME", ".local/state")
}
fn data_base() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}
fn cache_base() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache")
}

pub fn config_dir() -> PathBuf {
    config_base().join(APP)
}
pub fn state_dir() -> PathBuf {
    state_base().join(APP)
}
pub fn data_dir() -> PathBuf {
    data_base().join(APP)
}
pub fn cache_dir() -> PathBuf {
    cache_base().join(APP)
}
pub fn log_dir() -> PathBuf {
    state_dir().join("logs")
}
pub fn login_profile() -> PathBuf {
    data_dir().join("login")
}
