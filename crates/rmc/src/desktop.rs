//! Start menu entry and icons for the current user (`rmc install-desktop`): the desktop file
//! `~/.local/share/applications/rmc.desktop` and the icon in `~/.local/share/icons/hicolor`.
//! An AppImage creates (or corrects) them at start, so the first start puts RMC into the
//! menu and a moved AppImage keeps working.

use std::path::{Path, PathBuf};

use anyhow::Result;

const ICONS: &[(u32, &[u8])] = &[
    (22, crate::icons::PNG_22),
    (32, crate::icons::PNG_32),
    (48, crate::icons::PNG_48),
    (64, crate::icons::PNG_64),
    (128, crate::icons::PNG_128),
    (256, crate::icons::PNG_256),
];

fn data_base() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn desktop_file() -> PathBuf {
    data_base().join("applications").join("rmc.desktop")
}

/// Exec value for a path (quoted as the desktop entry spec wants it).
fn exec_value(exe: &Path) -> String {
    let s = exe.display().to_string();
    if s.chars().any(|c| c.is_whitespace() || "\"`$\\".contains(c)) {
        let escaped: String = s
            .chars()
            .flat_map(|c| match c {
                '"' | '`' | '$' | '\\' => vec!['\\', c],
                c => vec![c],
            })
            .collect();
        format!("\"{escaped}\"")
    } else {
        s
    }
}

fn entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]
Type=Application
Name=RMC
GenericName=Remotedesktop
Comment=Remote Machine Connect: Verbindung zu Windows-Rechnern per RDP
Exec={}
Icon=rmc
Terminal=false
Categories=Network;RemoteAccess;
StartupWMClass=rmc
Keywords=RDP;Remote;Windows;Remotedesktop;RMC;
",
        exec_value(exe)
    )
}

/// Program path of a desktop entry's Exec line (quotes removed).
fn exec_of(entry: &str) -> Option<String> {
    let v = entry.lines().find_map(|l| l.strip_prefix("Exec="))?.trim();
    let v = v
        .strip_prefix('"')
        .and_then(|r| r.split('"').next())
        .unwrap_or(v.split_whitespace().next().unwrap_or(v));
    Some(v.replace("\\ ", " "))
}

/// Writes icons and desktop entry for `exe`; refreshes the menu cache (KDE) if possible.
pub fn install(exe: &Path) -> Result<PathBuf> {
    let base = data_base();
    for (n, png) in ICONS {
        let dir = base.join(format!("icons/hicolor/{n}x{n}/apps"));
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("rmc.png"), png)?;
    }
    let file = desktop_file();
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("desktop.tmp");
    std::fs::write(&tmp, entry(exe))?;
    std::fs::rename(&tmp, &file)?;
    for cmd in ["kbuildsycoca6", "update-desktop-database"] {
        let mut c = std::process::Command::new(cmd);
        if cmd == "update-desktop-database" {
            c.arg(base.join("applications"));
        }
        let _ = c
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    Ok(file)
}

/// Started from an AppImage ($APPIMAGE): creates the menu entry, or fixes it after the
/// AppImage was moved. An entry of another installation (e.g. built from source) is left
/// alone, unless its program no longer exists.
pub fn ensure_for_appimage() {
    let Some(img) = std::env::var_os("APPIMAGE").map(PathBuf::from) else {
        return;
    };
    let want = entry(&img);
    let current = std::fs::read_to_string(desktop_file()).ok();
    if current.as_deref() == Some(want.as_str()) {
        return;
    }
    if let Some(exec) = current.as_deref().and_then(exec_of)
        && !exec.to_lowercase().ends_with(".appimage")
        && Path::new(&exec).exists()
    {
        return;
    }
    match install(&img) {
        Ok(f) => println!("Startmenü-Eintrag angelegt: {}", f.display()),
        Err(e) => eprintln!("Startmenü-Eintrag nicht angelegt: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting() {
        assert_eq!(exec_value(Path::new("/opt/rmc/rmc")), "/opt/rmc/rmc");
        assert_eq!(
            exec_value(Path::new("/home/a b/RMC.AppImage")),
            "\"/home/a b/RMC.AppImage\""
        );
        assert!(entry(Path::new("/x/rmc")).contains("Exec=/x/rmc\nIcon=rmc\n"));
        assert_eq!(
            exec_of(&entry(Path::new("/opt/rmc/rmc"))).as_deref(),
            Some("/opt/rmc/rmc")
        );
        assert_eq!(
            exec_of(&entry(Path::new("/home/a b/RMC.AppImage"))).as_deref(),
            Some("/home/a b/RMC.AppImage")
        );
    }
}
