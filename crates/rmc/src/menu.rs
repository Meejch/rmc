//! Settings menu of the connection bar (gear): things that can be
//! changed during the session. Sound and microphone are muted locally (PulseAudio/PipeWire
//! streams of this process), the clipboard is switched in the bridge, folders and printers
//! are announced to Windows through rdpdr.

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rdp_session::SessionHandle;
use rdp_viewer::viewer::{MenuItem, MenuKind};

const ID_SOUND: u32 = 1;
const ID_MIC: u32 = 2;
const ID_CLIPBOARD: u32 = 3;
const ID_FOLDER: u32 = 10; // + index of the quick folder
const ID_OTHER_FOLDER: u32 = 40;
const ID_PRINTER: u32 = 60; // + index of the printer
const ID_EDIT: u32 = 90;

/// Log line for the launcher: a folder was added during the session.
pub const DRIVE_ADDED_LINE: &str = "Leiste: Laufwerk verbunden";
/// Log line for the launcher: open the favorite's settings.
pub const EDIT_FAVORITE_LINE: &str = "Leiste: Favorit bearbeiten";

pub struct SessionMenu {
    sound: bool,
    mic: bool,
    clipboard: bool,
    sound_muted: bool,
    mic_muted: bool,
    /// Quick folders (name in Windows, path).
    folders: Vec<(String, String)>,
    /// Drive names in Windows (configured at start and added since).
    drives: Vec<String>,
    printers: Vec<String>,
    printers_on: Vec<String>,
    /// The session belongs to a favorite (not ad hoc): "Im Launcher bearbeiten".
    favorite: bool,
    picker: Option<mpsc::Receiver<Option<PathBuf>>>,
    mute_at: Instant,
}

impl SessionMenu {
    pub fn new(
        sound: bool,
        mic: bool,
        clipboard: bool,
        drives: &[(String, String)],
        printers_on: &[String],
        favorite: bool,
    ) -> Self {
        let mut folders = vec![("Home".to_string(), home())];
        for (key, fallback) in [
            ("DOWNLOAD", "Downloads"),
            ("DOCUMENTS", "Dokumente"),
            ("DESKTOP", "Desktop"),
        ] {
            if let Some(p) = user_dir(key) {
                let name = PathBuf::from(&p)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| fallback.to_string());
                folders.push((name, p));
            }
        }
        Self {
            sound,
            mic,
            clipboard,
            sound_muted: false,
            mic_muted: false,
            folders,
            drives: drives.iter().map(|d| d.0.clone()).collect(),
            printers: cups_printers(),
            printers_on: printers_on.to_vec(),
            favorite,
            picker: None,
            mute_at: Instant::now(),
        }
    }

    pub fn items(&self, clipboard_on: bool) -> Vec<MenuItem> {
        let mut v = Vec::new();
        if self.sound || self.mic || self.clipboard {
            v.push(MenuItem::new(0, "Diese Verbindung", MenuKind::Header));
        }
        if self.sound {
            v.push(MenuItem::new(
                ID_SOUND,
                "Ton",
                MenuKind::Toggle(!self.sound_muted),
            ));
        }
        if self.mic {
            v.push(MenuItem::new(
                ID_MIC,
                "Mikrofon",
                MenuKind::Toggle(!self.mic_muted),
            ));
        }
        if self.clipboard {
            v.push(MenuItem::new(
                ID_CLIPBOARD,
                "Zwischenablage",
                MenuKind::Toggle(clipboard_on),
            ));
        }
        v.push(MenuItem::new(0, "", MenuKind::Separator));
        v.push(MenuItem::new(
            0,
            "Ordner als Laufwerk verbinden",
            MenuKind::Header,
        ));
        for (i, (name, path)) in self.folders.iter().enumerate() {
            let done = self.drives.iter().any(|d| d.eq_ignore_ascii_case(name));
            let label = format!("{name}  ({})", short_path(path));
            v.push(MenuItem::new(
                ID_FOLDER + i as u32,
                label,
                if done {
                    MenuKind::Done
                } else {
                    MenuKind::Action
                },
            ));
        }
        v.push(MenuItem::new(
            ID_OTHER_FOLDER,
            if self.picker.is_some() {
                "Ordnerauswahl ist offen …"
            } else {
                "Anderer Ordner …"
            },
            if self.picker.is_some() {
                MenuKind::Done
            } else {
                MenuKind::Action
            },
        ));
        if !self.printers.is_empty() {
            v.push(MenuItem::new(0, "", MenuKind::Separator));
            v.push(MenuItem::new(0, "Drucker verbinden", MenuKind::Header));
            for (i, p) in self.printers.iter().enumerate() {
                let on = self.printers_on.iter().any(|x| x == p);
                v.push(MenuItem::new(
                    ID_PRINTER + i as u32,
                    p.replace('_', " "),
                    if on { MenuKind::Done } else { MenuKind::Action },
                ));
            }
        }
        if self.favorite {
            v.push(MenuItem::new(0, "", MenuKind::Separator));
            v.push(MenuItem::new(
                ID_EDIT,
                "Im Launcher bearbeiten …",
                MenuKind::Action,
            ));
        }
        v
    }

    /// A menu entry was clicked.
    pub fn handle(
        &mut self,
        id: u32,
        session: &SessionHandle,
        clip: Option<&crate::clip::Clipboard>,
    ) {
        match id {
            ID_SOUND => {
                self.sound_muted = !self.sound_muted;
                mute_async("sink-input", self.sound_muted);
                println!("Ton: {}", if self.sound_muted { "stumm" } else { "an" });
            }
            ID_MIC => {
                self.mic_muted = !self.mic_muted;
                mute_async("source-output", self.mic_muted);
                println!("Mikrofon: {}", if self.mic_muted { "stumm" } else { "an" });
            }
            ID_CLIPBOARD => {
                if let Some(c) = clip {
                    c.set_enabled(!c.enabled());
                }
            }
            ID_OTHER_FOLDER => {
                if self.picker.is_none() {
                    self.picker = Some(pick_folder());
                }
            }
            ID_EDIT => println!("{EDIT_FAVORITE_LINE}"),
            id if (ID_FOLDER..ID_FOLDER + 20).contains(&id) => {
                if let Some((name, path)) = self.folders.get((id - ID_FOLDER) as usize).cloned() {
                    self.add_drive(session, &name, &path);
                }
            }
            id if (ID_PRINTER..ID_PRINTER + 30).contains(&id) => {
                if let Some(p) = self.printers.get((id - ID_PRINTER) as usize).cloned()
                    && !self.printers_on.contains(&p)
                    && session.add_printer(&p)
                {
                    println!("Drucker angefordert: {p}");
                    self.printers_on.push(p);
                }
            }
            _ => {}
        }
    }

    fn add_drive(&mut self, session: &SessionHandle, name: &str, path: &str) {
        // a free name in Windows (the same name twice would be confusing)
        let mut n = name.to_string();
        let mut k = 2;
        while self.drives.iter().any(|d| d.eq_ignore_ascii_case(&n)) {
            n = format!("{name} {k}");
            k += 1;
        }
        if session.add_drive(&n, path) {
            println!("{DRIVE_ADDED_LINE}\t{n}\t{path}");
            self.drives.push(n);
        }
    }

    /// Folder chooser results; mute again streams that were reopened. Returns true when the
    /// menu entries changed.
    pub fn tick(&mut self, session: &SessionHandle) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.picker {
            match rx.try_recv() {
                Ok(choice) => {
                    self.picker = None;
                    changed = true;
                    if let Some(p) = choice {
                        let name = p
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| "Ordner".into());
                        self.add_drive(session, &name, &p.display().to_string());
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.picker = None;
                    changed = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        // rdpsnd/audin may open new streams (format change, reconnect): keep them muted
        if (self.sound_muted || self.mic_muted) && self.mute_at.elapsed() > Duration::from_secs(3) {
            self.mute_at = Instant::now();
            if self.sound_muted {
                mute_async("sink-input", true);
            }
            if self.mic_muted {
                mute_async("source-output", true);
            }
        }
        changed
    }
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

fn user_dir(key: &str) -> Option<String> {
    let out = Command::new("xdg-user-dir").arg(key).output().ok()?;
    let p = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!p.is_empty() && p != home() && std::path::Path::new(&p).is_dir()).then_some(p)
}

fn short_path(p: &str) -> String {
    match p.strip_prefix(&home()) {
        Some("") => "~".into(),
        Some(rest) => format!("~{rest}"),
        None => p.to_string(),
    }
}

/// CUPS destinations for the settings page: looked up once in the background (the UI
/// redraws on every mouse move; running lpstat each frame froze it). None = still looking.
pub fn cups_printers_cached() -> Option<Vec<String>> {
    use std::sync::{Mutex, OnceLock};
    static LIST: OnceLock<Mutex<Option<Vec<String>>>> = OnceLock::new();
    let mut started = false;
    let cell = LIST.get_or_init(|| {
        started = true;
        Mutex::new(None)
    });
    if started {
        std::thread::spawn(|| {
            let v = cups_printers();
            if let Some(c) = LIST.get() {
                *c.lock().expect("lock") = Some(v);
            }
        });
    }
    cell.lock().expect("lock").clone()
}

/// CUPS destinations (`lpstat -e`); empty without CUPS.
pub fn cups_printers() -> Vec<String> {
    Command::new("lpstat")
        .arg("-e")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Mutes or unmutes this process' playback ("sink-input") or recording ("source-output")
/// streams via pactl (PulseAudio or PipeWire), in the background.
fn mute_async(kind: &'static str, on: bool) {
    std::thread::spawn(move || {
        let pid = std::process::id().to_string();
        let Ok(out) = Command::new("pactl")
            .args(["-f", "json", "list", &format!("{kind}s")])
            .output()
        else {
            eprintln!("pactl nicht verfügbar: Stummschalten nicht möglich");
            return;
        };
        let Ok(list) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            return;
        };
        for s in list.as_array().into_iter().flatten() {
            let owner = s
                .pointer("/properties/application.process.id")
                .and_then(|v| v.as_str());
            if owner == Some(pid.as_str())
                && let Some(idx) = s.get("index").and_then(|v| v.as_u64())
            {
                let _ = Command::new("pactl")
                    .arg(format!("set-{kind}-mute"))
                    .arg(idx.to_string())
                    .arg(if on { "1" } else { "0" })
                    .status();
            }
        }
    });
}

/// Folder chooser (kdialog, else zenity) on a thread; the channel gets the choice.
fn pick_folder() -> mpsc::Receiver<Option<PathBuf>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let home = home();
        let title = "Ordner als Laufwerk verbinden";
        let tries: [(&str, Vec<&str>); 2] = [
            (
                "kdialog",
                vec!["--title", title, "--getexistingdirectory", &home],
            ),
            (
                "zenity",
                vec![
                    "--file-selection",
                    "--directory",
                    "--title",
                    title,
                    "--filename",
                    &home,
                ],
            ),
        ];
        for (cmd, args) in tries {
            match Command::new(cmd).args(&args).output() {
                Ok(o) => {
                    let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    let _ =
                        tx.send((o.status.success() && !p.is_empty()).then(|| PathBuf::from(p)));
                    return;
                }
                Err(_) => continue,
            }
        }
        eprintln!("Kein Ordnerdialog gefunden (kdialog oder zenity)");
        let _ = tx.send(None);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_and_states() {
        let mut m = SessionMenu::new(
            true,
            true,
            true,
            &[("Home".into(), "/home/x".into())],
            &[],
            true,
        );
        m.printers = vec!["Drucker_A".into()];
        let items = m.items(true);
        assert!(
            items
                .iter()
                .any(|i| i.id == ID_SOUND && i.kind == MenuKind::Toggle(true))
        );
        // Home is already redirected -> Done
        assert!(
            items
                .iter()
                .any(|i| i.id == ID_FOLDER && i.kind == MenuKind::Done)
        );
        assert!(
            items
                .iter()
                .any(|i| i.label == "Drucker A" && i.kind == MenuKind::Action)
        );
        assert!(items.iter().any(|i| i.id == ID_EDIT));
        m.sound_muted = true;
        assert!(
            m.items(false)
                .iter()
                .any(|i| i.id == ID_SOUND && i.kind == MenuKind::Toggle(false))
        );
        assert!(
            m.items(false)
                .iter()
                .any(|i| i.id == ID_CLIPBOARD && i.kind == MenuKind::Toggle(false))
        );
    }
}
