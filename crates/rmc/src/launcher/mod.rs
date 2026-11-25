//! Launcher: favorites, new target, settings (global and per
//! favorite), session status, tray icon. A session runs as its own process
//! (`rmc` with the favorite's options) whose output goes to a log file; the launcher
//! follows the log for the status line.

use std::cell::RefCell;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::Result;
use eframe::egui;

mod history;
mod icons;
mod logview;
mod theme;
mod views;

use crate::profiles::{Drive, Favorite, Options, Overrides, Settings, log_dir};
use crate::reach::{Reach, Schedule};

/// Icon theme name (installed by tools/install-local.sh); the tray also gets pixmaps.
const ICON: &str = "rmc";
/// Window title and tray name.
pub const TITLE: &str = "RMC – Remote Machine Connect";
const LOG_LEVELS: &[&str] = &["ERROR", "WARN", "INFO", "DEBUG"];
const HELPERS: &[(&str, &str)] = &[
    ("login", "Eigenes Login (Anmeldung gespeichert)"),
    ("webview", "FreeRDP-WebView"),
    ("xdg", "Systembrowser"),
];

/// Abstract socket of the running launcher: a second start only asks it to show its window.
const INSTANCE_SOCKET: &str = "rmc-launcher";

/// Binds the instance socket, or tells the running launcher to show itself (`Ok(None)`).
fn single_instance() -> Result<Option<std::os::unix::net::UnixListener>> {
    use std::io::Write;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
    // RDPC_INSTANCE (tests): own instance name, so a running launcher is left alone.
    let base = std::env::var("RDPC_INSTANCE").unwrap_or_else(|_| INSTANCE_SOCKET.into());
    let name = format!("{base}-{}", unsafe { libc::getuid() });
    let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
    match UnixListener::bind_addr(&addr) {
        Ok(l) => Ok(Some(l)),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let mut s = UnixStream::connect_addr(&addr)?;
            s.write_all(b"show\n")?;
            Ok(None)
        }
        Err(e) => Err(e.into()),
    }
}

pub fn run() -> Result<()> {
    let Some(listener) = single_instance()? else {
        println!("Launcher läuft bereits: Fenster wird angezeigt");
        return Ok(());
    };
    let launcher = Rc::new(RefCell::new(Launcher::new(listener)));
    // The window can be closed into the tray: Wayland cannot hide a window, so it is really
    // closed and opened again later (eframe keeps the event loop for repeated runs).
    loop {
        let l = launcher.clone();
        eframe::run_native(
            "rmc",
            window_options(),
            Box::new(move |cc| {
                theme::apply(&cc.egui_ctx);
                l.borrow_mut().set_ctx(Some(cc.egui_ctx.clone()));
                Ok(Box::new(Window(l)))
            }),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut l = launcher.borrow_mut();
        l.set_ctx(None);
        // RDPC_NO_TRAY (tests): tray mode without an icon.
        let tray_ok = l.tray.is_some() || std::env::var_os("RDPC_NO_TRAY").is_some();
        if l.quit || !tray_ok || !l.settings.close_to_tray() {
            break;
        }
        if !l.in_tray() {
            break;
        }
    }
    if let Some(t) = launcher.borrow_mut().tray.take() {
        t.shutdown().wait();
    }
    Ok(())
}

fn window_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_app_id("rmc")
            .with_icon(window_icon())
            .with_inner_size([1200.0, 840.0])
            .with_min_inner_size([960.0, 660.0]),
        ..Default::default()
    }
}

fn window_icon() -> std::sync::Arc<egui::IconData> {
    let (width, height, rgba) =
        crate::icons::rgba(crate::icons::PNG_64).unwrap_or((1, 1, vec![0; 4]));
    std::sync::Arc::new(egui::IconData {
        rgba,
        width,
        height,
    })
}

/// The window's app: the launcher state outlives the window (tray mode in between).
struct Window(Rc<RefCell<Launcher>>);

impl eframe::App for Window {
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.0.borrow_mut().logic(ctx, frame);
    }
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.0.borrow_mut().ui(ui, frame);
    }
}

// ---- tray --------------------------------------------------------------------------------

enum TrayCmd {
    Show,
    Connect(String),
    /// Disconnect the session of this favorite.
    Disconnect(String),
    Quit,
}

/// The open window's context (None while the launcher lives in the tray only).
type Wake = Arc<Mutex<Option<egui::Context>>>;

fn wake(w: &Wake) {
    if let Some(c) = w.lock().expect("lock").as_ref() {
        c.request_repaint();
    }
}

struct Tray {
    tx: mpsc::Sender<TrayCmd>,
    wake: Wake,
    favorites: Vec<String>,
    /// Favorites with a running session.
    active: Vec<String>,
}

impl Tray {
    fn send(&self, cmd: TrayCmd) {
        let _ = self.tx.send(cmd);
        wake(&self.wake);
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "rmc".into()
    }
    fn title(&self) -> String {
        match self.active.as_slice() {
            [] => "RMC".into(),
            [n] => format!("RMC: {n}"),
            v => format!("RMC: {} Verbindungen", v.len()),
        }
    }
    fn icon_name(&self) -> String {
        ICON.into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        [
            crate::icons::PNG_22,
            crate::icons::PNG_32,
            crate::icons::PNG_48,
            crate::icons::PNG_64,
        ]
        .iter()
        .filter_map(|p| crate::icons::argb(p))
        .map(|(width, height, data)| ksni::Icon {
            width,
            height,
            data,
        })
        .collect()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCmd::Show);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        let mut m: Vec<ksni::MenuItem<Self>> = Vec::new();
        for n in &self.active {
            let name = n.clone();
            m.push(
                StandardItem {
                    label: format!("Trennen: {n}"),
                    icon_name: "network-disconnect".into(),
                    activate: Box::new(move |t: &mut Self| {
                        t.send(TrayCmd::Disconnect(name.clone()))
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        if !self.active.is_empty() {
            m.push(ksni::MenuItem::Separator);
        }
        for f in &self.favorites {
            let name = f.clone();
            m.push(
                StandardItem {
                    label: format!("Verbinden: {f}"),
                    enabled: !self.active.contains(f),
                    activate: Box::new(move |t: &mut Self| t.send(TrayCmd::Connect(name.clone()))),
                    ..Default::default()
                }
                .into(),
            );
        }
        if !self.favorites.is_empty() {
            m.push(ksni::MenuItem::Separator);
        }
        m.push(
            StandardItem {
                label: "Launcher öffnen".into(),
                activate: Box::new(|t: &mut Self| t.send(TrayCmd::Show)),
                ..Default::default()
            }
            .into(),
        );
        m.push(
            StandardItem {
                label: "Beenden".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| t.send(TrayCmd::Quit)),
                ..Default::default()
            }
            .into(),
        );
        m
    }
}

// ---- session process ----------------------------------------------------------------------

#[derive(Clone, PartialEq)]
enum State {
    Starting,
    Connected,
    Reconnecting,
    Ended(String),
}

struct Running {
    fav: Favorite,
    adhoc: bool,
    child: Child,
    log: PathBuf,
    offset: u64,
    partial: String,
    state: State,
    was_connected: bool,
    started: Instant,
    /// The star in the session's bar was clicked (save as favorite).
    save_requested: bool,
    /// Answers to certificate prompts (the password went first).
    stdin: Option<ChildStdin>,
    /// Certificate question of the session, waiting for the user.
    cert: Option<CertPrompt>,
    /// The server refused the user name or password.
    auth_failed: bool,
    /// Password to store once the session is connected ("Passwort speichern").
    save_password: Option<String>,
    /// Folders added from the bar menu during the session (name, path).
    drives_added: Vec<(String, String)>,
    /// "Im Launcher bearbeiten" from the bar menu.
    edit_requested: bool,
    /// Graphics backend the session reported ("NVIDIA NVDEC auf ...").
    gpu: Option<String>,
}

/// "RDPC-CERT" line of the session process (csrc/shim.c, ask_certificate).
#[derive(Clone, Debug)]
struct CertPrompt {
    changed: bool,
    host: String,
    common_name: String,
    subject: String,
    issuer: String,
    fingerprint: String,
    old_fingerprint: String,
}

impl CertPrompt {
    fn parse(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.strip_prefix("RDPC-CERT\t")?.split('\t').collect();
        let get = |i: usize| f.get(i).copied().unwrap_or("").to_string();
        Some(Self {
            changed: f.first() == Some(&"changed"),
            host: get(1),
            common_name: get(2),
            subject: get(3),
            issuer: get(4),
            fingerprint: get(5),
            old_fingerprint: get(6),
        })
    }
}

/// FreeRDP's messages for a refused login (logon failure, wrong password, account problems).
fn is_auth_failure(msg: &str) -> bool {
    const CODES: &[&str] = &[
        "0x00020009",
        "0x00020014",
        "0x00020015",
        "0x00020016",
        "0x00020018",
        "0x0002000E",
        "0x0002000F",
        "0x00020011",
        "0x00020012",
    ];
    CODES.iter().any(|c| msg.contains(c))
        || msg.contains("Logon failed")
        || msg.contains("Authentication failure")
}

/// Password dialog before connecting (no stored password, or the last one was refused).
struct PwPrompt {
    fav: Favorite,
    adhoc: bool,
    password: String,
    save: bool,
    error: Option<String>,
}

impl Running {
    /// Follows the log: new lines -> state.
    fn poll(&mut self) {
        if let Ok(mut f) = std::fs::File::open(&self.log)
            && f.seek(SeekFrom::Start(self.offset)).is_ok()
        {
            let mut buf = String::new();
            if let Ok(n) = f.read_to_string(&mut buf) {
                self.offset += n as u64;
                self.partial.push_str(&buf);
                while let Some(i) = self.partial.find('\n') {
                    let line: String = self.partial.drain(..=i).collect();
                    self.line(line.trim_end());
                }
            }
        }
        if let Ok(Some(status)) = self.child.try_wait()
            && !matches!(self.state, State::Ended(_))
        {
            self.state = State::Ended(if status.success() {
                "Sitzung beendet".into()
            } else {
                format!("Sitzung beendet ({status})")
            });
        }
    }

    fn line(&mut self, l: &str) {
        if l == crate::SAVE_FAVORITE_LINE {
            self.save_requested = true;
        } else if let Some(c) = CertPrompt::parse(l) {
            self.cert = Some(c);
        } else if let Some(g) = l.strip_prefix(crate::GPU_LINE) {
            self.gpu = Some(g.to_string());
        } else if l == crate::menu::EDIT_FAVORITE_LINE {
            self.edit_requested = true;
        } else if let Some(rest) = l
            .strip_prefix(crate::menu::DRIVE_ADDED_LINE)
            .and_then(|r| r.strip_prefix('\t'))
        {
            if let Some((name, path)) = rest.split_once('\t') {
                self.drives_added.push((name.to_string(), path.to_string()));
            }
        } else if l.starts_with("Verbunden:") || l.starts_with("Sitzung: reconnected") {
            self.state = State::Connected;
            self.was_connected = true;
        } else if l.starts_with("Sitzung: connection lost") {
            self.state = State::Reconnecting;
        } else if let Some(msg) = l.strip_prefix("Getrennt: ") {
            self.auth_failed |= !self.was_connected && is_auth_failure(msg);
            self.state = State::Ended(format!("Getrennt: {msg}"));
        } else if l.starts_with("Error: ") {
            self.state = State::Ended(l.to_string());
        }
    }

    fn running(&self) -> bool {
        !matches!(self.state, State::Ended(_))
    }

    /// Answers the pending certificate question.
    fn answer_cert(&mut self, trust: bool) {
        use std::io::Write;
        self.cert = None;
        if let Some(i) = &mut self.stdin {
            let _ = i.write_all(if trust { b"yes\n" } else { b"no\n" });
            let _ = i.flush();
        }
    }

    fn stop(&mut self) {
        // SIGTERM: the session process ends, the server sees a disconnect.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
    }
}

/// Starts a session process for the favorite; output into a new log file.
fn spawn(
    fav: &Favorite,
    defaults: &Options,
    adhoc: bool,
    password: Option<&str>,
) -> Result<(Child, PathBuf)> {
    let dir = log_dir();
    std::fs::create_dir_all(&dir)?;
    prune_logs(&dir, 20);
    let stamp = chrono_like_stamp();
    let safe: String = fav
        .name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let log = dir.join(format!("{safe}-{stamp}.log"));
    let file = std::fs::File::create(&log)?;
    let err = file.try_clone()?;
    let exe = std::env::current_exe()?;
    let mut args = fav.session_args(defaults);
    if adhoc {
        args.push("--offer-favorite".into());
    }
    // stdin: first the password line, later the answers to certificate questions.
    args.push("--cert-prompt".into());
    if password.is_some() {
        args.push("--password-stdin".into());
    }
    let mut child = Command::new(exe)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(file)
        .stderr(err)
        .spawn()?;
    if let (Some(pw), Some(stdin)) = (password, child.stdin.as_mut()) {
        use std::io::Write;
        stdin.write_all(pw.as_bytes())?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
    }
    Ok((child, log))
}

/// Keeps the newest `keep` log files.
fn prune_logs(dir: &std::path::Path, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<_> = rd
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "log"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    files.sort();
    let n = files.len().saturating_sub(keep);
    for (_, p) in files.into_iter().take(n) {
        let _ = std::fs::remove_file(p);
    }
}

/// Local time "YYYYMMDD-HHMMSS" without a date crate.
fn chrono_like_stamp() -> String {
    local_time("%Y%m%d-%H%M%S")
}

fn local_time(fmt: &str) -> String {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    let mut buf = [0u8; 64];
    let cfmt = std::ffi::CString::new(fmt).expect("format");
    let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), cfmt.as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

/// Forgets the stored sign-in (refresh token) of one account/host.
fn forget_token(fav: &Favorite) -> bool {
    rdp_session::secret::clear(&[
        ("application", rdp_session::secret::APP),
        ("account", &fav.user),
        ("host", &fav.host),
    ]);
    let a = fav.password_attrs();
    rdp_session::secret::clear(&attr_refs(&a));
    true
}

fn attr_refs(a: &[(String, String)]) -> Vec<(&str, &str)> {
    a.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

fn stored_password(fav: &Favorite) -> Option<String> {
    rdp_session::secret::lookup(&attr_refs(&fav.password_attrs()))
}

// ---- editor --------------------------------------------------------------------------------

#[derive(Clone, PartialEq)]
enum Target {
    Defaults,
    /// Index in the favorites (None = new favorite).
    Favorite(Option<usize>),
}

struct Editor {
    target: Target,
    fav: Favorite,
    /// Defaults being edited (Target::Defaults).
    defaults: Options,
    close_to_tray: bool,
    error: Option<String>,
    /// Selected category in the settings page.
    category: usize,
    /// Text of the check intervals while editing ("5, 15, 25").
    reach_text: Option<String>,
}

// ---- reachability ---------------------------------------------------------------------------

/// Reachability of one favorite (status light).
struct ReachEntry {
    /// "host:port" the status belongs to (a changed favorite starts over).
    key: String,
    status: Reach,
    checked: Option<Instant>,
    in_flight: bool,
    next: Instant,
}

/// Result of a check thread: favorite name, key, result.
type ReachResult = (String, String, Reach);

// ---- launcher -------------------------------------------------------------------------------

struct Launcher {
    settings: Settings,
    outputs: Vec<rdp_viewer::wl_outputs::LocalOutput>,
    new_host: String,
    new_user: String,
    /// Session processes (running, and ended ones until dismissed or restarted).
    sessions: Vec<Running>,
    history: Vec<history::Entry>,
    editor: Option<Editor>,
    confirm_delete: Option<usize>,
    offer_save: Option<Favorite>,
    /// A folder added during a favorite's session: keep it in the favorite? (name, drive)
    offer_drive: Option<(String, Drive)>,
    /// Bring the window up (e.g. the bar's star asks to save the favorite).
    show_requested: bool,
    pw_prompt: Option<PwPrompt>,
    page: views::Page,
    fav_search: String,
    selected_fav: Option<String>,
    /// 0 = current connections, 1 = recent connections.
    conn_tab: usize,
    new_show_pw: bool,
    logview: logview::LogView,
    logo: Option<egui::TextureHandle>,
    /// Password field of "Neues Ziel" and its options.
    new_password: String,
    new_domain: String,
    new_entra: bool,
    message: Option<String>,
    tray: Option<ksni::blocking::Handle<Tray>>,
    tray_rx: mpsc::Receiver<TrayCmd>,
    wake: Wake,
    reach: std::collections::HashMap<String, ReachEntry>,
    /// Start of the check schedule (launcher start, window back from the tray).
    reach_epoch: Instant,
    reach_tx: mpsc::Sender<ReachResult>,
    reach_rx: mpsc::Receiver<ReachResult>,
    quit: bool,
    test_closed: bool,
}

impl Launcher {
    fn new(listener: std::os::unix::net::UnixListener) -> Self {
        let wake_ctx: Wake = Arc::new(Mutex::new(None));
        let (settings, message) = match Settings::load() {
            Ok(s) => (s, None),
            Err(e) => (
                Settings::default(),
                Some(format!("Einstellungen nicht lesbar: {e:#}")),
            ),
        };
        let (tx, tray_rx) = mpsc::channel();
        // Second starts (e.g. from the KDE menu) show this launcher.
        {
            let (tx, w) = (tx.clone(), wake_ctx.clone());
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    drop(conn);
                    let _ = tx.send(TrayCmd::Show);
                    wake(&w);
                }
            });
        }
        // RDPC_NO_TRAY: tests in a virtual display without touching the desktop's tray.
        let tray = if std::env::var_os("RDPC_NO_TRAY").is_some() {
            None
        } else {
            use ksni::blocking::TrayMethods;
            Tray {
                tx,
                wake: wake_ctx.clone(),
                favorites: settings.favorites.iter().map(|f| f.name.clone()).collect(),
                active: Vec::new(),
            }
            .spawn()
            .map_err(|e| eprintln!("Tray-Symbol nicht verfügbar: {e}"))
            .ok()
        };
        let (reach_tx, reach_rx) = mpsc::channel();
        let mut l = Self {
            reach: std::collections::HashMap::new(),
            reach_epoch: Instant::now(),
            reach_tx,
            reach_rx,
            settings,
            outputs: rdp_viewer::wl_outputs::query().unwrap_or_default(),
            new_host: String::new(),
            new_user: String::new(),
            sessions: Vec::new(),
            history: history::load(),
            editor: None,
            confirm_delete: None,
            offer_save: None,
            offer_drive: None,
            show_requested: false,
            pw_prompt: None,
            page: views::Page::Connect,
            fav_search: String::new(),
            selected_fav: None,
            conn_tab: 0,
            new_show_pw: false,
            logview: logview::LogView::default(),
            logo: None,
            new_password: String::new(),
            new_domain: String::new(),
            new_entra: true,
            message,
            tray,
            tray_rx,
            wake: wake_ctx,
            quit: false,
            test_closed: false,
        };
        // "Verbinden beim Start"
        if let Some(f) = l
            .settings
            .favorites
            .iter()
            .find(|f| f.connect_on_start)
            .cloned()
        {
            l.connect(f, false);
        }
        // RDPC_TEST_PAGE=history|log|settings: start page (screenshots in tests).
        l.page = match std::env::var("RDPC_TEST_PAGE").as_deref() {
            Ok("history") => views::Page::History,
            Ok("log") => views::Page::Log,
            Ok("settings") => views::Page::Settings,
            _ => views::Page::Connect,
        };
        // RDPC_TEST_EDITOR=defaults|<index>: opens an editor at start (screenshots in tests).
        if let Ok(v) = std::env::var("RDPC_TEST_EDITOR") {
            // "<index or defaults>[:<category>]"
            let (v, cat) = v.split_once(':').unwrap_or((v.as_str(), ""));
            let target = match v.parse::<usize>() {
                Ok(i) if i < l.settings.favorites.len() => Target::Favorite(Some(i)),
                _ => Target::Defaults,
            };
            let fav = match target {
                Target::Favorite(Some(i)) => l.settings.favorites[i].clone(),
                _ => Favorite::default(),
            };
            if target == Target::Defaults {
                l.page = views::Page::Settings;
            }
            l.open_editor(target, fav);
            if let (Ok(c), Some(ed)) = (cat.parse::<usize>(), l.editor.as_mut()) {
                ed.category = c;
            }
        }
        if l.settings.favorites.is_empty() && l.message.is_none() {
            l.message = Some(
                "Noch keine Favoriten: links ein Ziel eingeben oder „Neuer Favorit“ wählen.".into(),
            );
        }
        l
    }

    fn open_editor(&mut self, target: Target, fav: Favorite) {
        let category = 0;
        self.editor = Some(Editor {
            target,
            fav,
            defaults: self.settings.defaults.clone(),
            close_to_tray: self.settings.close_to_tray(),
            error: None,
            category,
            reach_text: None,
        });
    }

    fn set_ctx(&mut self, ctx: Option<egui::Context>) {
        if ctx.is_some() {
            self.restart_reach();
        }
        *self.wake.lock().expect("lock") = ctx;
    }

    /// The window opened: check every favorite now and start the fast intervals again.
    fn restart_reach(&mut self) {
        let now = Instant::now();
        self.reach_epoch = now;
        for e in self.reach.values_mut() {
            e.next = now;
        }
    }

    /// Takes check results and starts due checks; returns the time until the next one.
    fn reach_tick(&mut self) -> Option<Duration> {
        let now = Instant::now();
        while let Ok((name, key, r)) = self.reach_rx.try_recv() {
            if let Some(e) = self.reach.get_mut(&name).filter(|e| e.key == key) {
                e.status = r;
                e.checked = Some(now);
                e.in_flight = false;
            }
        }
        let names: Vec<&str> = self
            .settings
            .favorites
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        self.reach.retain(|n, _| names.contains(&n.as_str()));
        let mut wait: Option<Duration> = None;
        for f in &self.settings.favorites {
            let o = f.resolve(&self.settings.defaults);
            let (host, port) = f.host_port();
            if !o.reach_check || host.is_empty() {
                self.reach.remove(&f.name);
                continue;
            }
            let key = format!("{host}:{port}");
            let e = self
                .reach
                .entry(f.name.clone())
                .or_insert_with(|| ReachEntry {
                    key: key.clone(),
                    status: Reach::Unknown,
                    checked: None,
                    in_flight: false,
                    next: now,
                });
            if e.key != key {
                *e = ReachEntry {
                    key: key.clone(),
                    status: Reach::Unknown,
                    checked: None,
                    in_flight: false,
                    next: now,
                };
            }
            if !e.in_flight && now >= e.next {
                let sched = Schedule {
                    intervals: o.reach_intervals.clone(),
                    stage: o.reach_stage,
                };
                e.in_flight = true;
                e.next = now + sched.interval_at(now - self.reach_epoch);
                let (tx, w, name) = (self.reach_tx.clone(), self.wake.clone(), f.name.clone());
                std::thread::spawn(move || {
                    let r = crate::reach::probe(&host, port);
                    let _ = tx.send((name, key, r));
                    wake(&w);
                });
            }
            let d = e.next.saturating_duration_since(now);
            wait = Some(wait.map_or(d, |w| w.min(d)));
        }
        wait
    }

    /// Tray only (window closed): serves tray commands until the window is wanted again
    /// (true) or the app quits (false).
    fn in_tray(&mut self) -> bool {
        loop {
            match self.tray_rx.recv_timeout(Duration::from_millis(500)) {
                Ok(TrayCmd::Show) => return true,
                Ok(TrayCmd::Connect(n)) => self.connect_by_name(&n),
                Ok(TrayCmd::Disconnect(n)) => self.disconnect(&n),
                Ok(TrayCmd::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => return false,
                Err(mpsc::RecvTimeoutError::Timeout) => self.poll_session(),
            }
            if std::mem::take(&mut self.show_requested) {
                return true;
            }
        }
    }

    fn save(&mut self) {
        if let Err(e) = self.settings.save() {
            self.message = Some(format!("Speichern fehlgeschlagen: {e:#}"));
        }
        self.update_tray();
    }

    fn update_tray(&self) {
        if let Some(t) = &self.tray {
            let favorites = self
                .settings
                .favorites
                .iter()
                .map(|f| f.name.clone())
                .collect();
            let active = self
                .sessions
                .iter()
                .filter(|s| s.running())
                .map(|s| s.fav.name.clone())
                .collect();
            t.update(|tray| {
                tray.favorites = favorites;
                tray.active = active;
            });
        }
    }

    /// Any session running.
    fn active(&self) -> bool {
        self.sessions.iter().any(Running::running)
    }

    /// The running session of a favorite (by name).
    fn running_of(&self, name: &str) -> Option<&Running> {
        self.sessions
            .iter()
            .find(|s| s.running() && s.fav.name == name)
    }

    fn disconnect(&mut self, name: &str) {
        for s in self.sessions.iter_mut().filter(|s| s.fav.name == name) {
            s.stop();
        }
    }

    /// Removes ended sessions of `name` from the list (dismissed, or started again).
    fn dismiss(&mut self, name: &str) {
        self.sessions.retain(|s| s.running() || s.fav.name != name);
    }

    /// Connects a favorite: Entra directly, password logins with the stored password or after
    /// asking for it.
    fn connect(&mut self, fav: Favorite, adhoc: bool) {
        if !fav.entra {
            match stored_password(&fav) {
                Some(pw) => self.start(fav, adhoc, Some(pw), None),
                None => self.ask_password(fav, adhoc, None),
            }
            return;
        }
        self.start(fav, adhoc, None, None);
    }

    fn ask_password(&mut self, fav: Favorite, adhoc: bool, error: Option<String>) {
        self.pw_prompt = Some(PwPrompt {
            save: !adhoc,
            fav,
            adhoc,
            password: String::new(),
            error,
        });
        self.show_requested = true;
    }

    /// Starts the session process; `save_password` is stored once connected.
    fn start(
        &mut self,
        fav: Favorite,
        adhoc: bool,
        password: Option<String>,
        save_password: Option<String>,
    ) {
        if self.running_of(&fav.name).is_some() {
            self.message = Some(format!("„{}“ ist bereits verbunden.", fav.name));
            return;
        }
        if fav.host.trim().is_empty() {
            self.message = Some("Kein Host angegeben.".into());
            return;
        }
        match spawn(&fav, &self.settings.defaults, adhoc, password.as_deref()) {
            Ok((mut child, log)) => {
                self.message = None;
                let stdin = child.stdin.take();
                self.dismiss(&fav.name);
                self.history.push(history::Entry {
                    name: fav.name.clone(),
                    host: fav.host.clone(),
                    user: fav.user.clone(),
                    domain: fav.domain.clone(),
                    entra: fav.entra,
                    adhoc,
                    started: local_time("%Y-%m-%d %H:%M:%S"),
                    log: log.display().to_string(),
                    ..Default::default()
                });
                history::save(&self.history);
                self.sessions.push(Running {
                    fav,
                    adhoc,
                    child,
                    log,
                    offset: 0,
                    partial: String::new(),
                    state: State::Starting,
                    was_connected: false,
                    started: Instant::now(),
                    save_requested: false,
                    stdin,
                    cert: None,
                    auth_failed: false,
                    save_password,
                    drives_added: Vec::new(),
                    edit_requested: false,
                    gpu: None,
                });
                self.update_tray();
            }
            Err(e) => self.message = Some(format!("Start fehlgeschlagen: {e:#}")),
        }
    }

    fn connect_by_name(&mut self, name: &str) {
        if let Some(f) = self.settings.find(name).cloned() {
            self.connect(f, false);
        }
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        while let Ok(cmd) = self.tray_rx.try_recv() {
            match cmd {
                TrayCmd::Show => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayCmd::Connect(n) => self.connect_by_name(&n),
                TrayCmd::Disconnect(n) => self.disconnect(&n),
                TrayCmd::Quit => {
                    self.quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    /// Session status: logs, history, success bookkeeping (all sessions).
    fn poll_session(&mut self) {
        let mut changed = false;
        let mut connected_now = Vec::new();
        let mut save_now = None;
        let mut refused = None;
        let mut hist_changed = false;
        let mut edit_now = None;
        for s in &mut self.sessions {
            let before = (s.state.clone(), s.was_connected);
            let had_cert = s.cert.is_some();
            s.poll();
            if s.cert.is_some() && !had_cert {
                self.show_requested = true;
            }
            if s.was_connected
                && !before.1
                && let Some(pw) = s.save_password.take()
            {
                let a = s.fav.password_attrs();
                let label = format!("RMC: Passwort {} auf {}", s.fav.account(), s.fav.host);
                if rdp_session::secret::store(&attr_refs(&a), &label, &pw)
                    == rdp_session::secret::Backend::None
                {
                    self.message = Some("Passwort konnte nicht gespeichert werden.".into());
                }
            }
            if !s.running() && std::mem::take(&mut s.auth_failed) && !s.fav.entra {
                refused = Some((s.fav.clone(), s.adhoc));
            }
            if std::mem::take(&mut s.save_requested) {
                save_now = Some(s.fav.clone());
            }
            if std::mem::take(&mut s.edit_requested) && !s.adhoc {
                edit_now = Some(s.fav.name.clone());
            }
            if !s.adhoc {
                for (name, path) in s.drives_added.drain(..) {
                    self.offer_drive = Some((s.fav.name.clone(), Drive { name, path }));
                    self.show_requested = true;
                }
            }
            if s.was_connected && !before.1 {
                connected_now.push((s.fav.clone(), s.adhoc));
            }
            if s.state != before.0 {
                changed = true;
                let log = s.log.display().to_string();
                if let Some(h) = self.history.iter_mut().rev().find(|h| h.log == log) {
                    h.connected |= s.was_connected;
                    if let State::Ended(m) = &s.state {
                        h.ended = Some(local_time("%Y-%m-%d %H:%M:%S"));
                        h.result = m.clone();
                    }
                    hist_changed = true;
                }
            }
        }
        if hist_changed {
            history::save(&self.history);
        }
        for (fav, adhoc) in connected_now {
            let now = local_time("%Y-%m-%d %H:%M");
            if adhoc {
                if self.settings.favorites.iter().all(|f| f.host != fav.host) {
                    self.offer_save = Some(fav);
                }
            } else if let Some(f) = self
                .settings
                .favorites
                .iter_mut()
                .find(|f| f.name == fav.name)
            {
                f.last_connected = Some(now);
                self.save();
            }
        }
        if let Some((fav, adhoc)) = refused {
            self.dismiss(&fav.name);
            self.ask_password(
                fav,
                adhoc,
                Some("Anmeldung abgelehnt: Benutzer oder Passwort falsch.".into()),
            );
        }
        if let Some(n) = edit_now
            && let Some(i) = self.settings.favorites.iter().position(|f| f.name == n)
        {
            let f = self.settings.favorites[i].clone();
            self.open_editor(Target::Favorite(Some(i)), f);
            self.show_requested = true;
        }
        if let Some(fav) = save_now {
            // The editor shows name, host and user for review; saving adds the favorite.
            self.offer_save = None;
            self.open_editor(Target::Favorite(None), fav);
            self.show_requested = true;
        }
        if changed {
            self.update_tray();
        }
    }

    fn delete_dialog(&mut self, ctx: &egui::Context) {
        let Some(i) = self.confirm_delete else { return };
        let Some(name) = self.settings.favorites.get(i).map(|f| f.name.clone()) else {
            self.confirm_delete = None;
            return;
        };
        let mut close = false;
        egui::Window::new("Favorit löschen")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("„{name}“ wirklich löschen?"));
                ui.horizontal(|ui| {
                    if ui.button("Löschen").clicked() {
                        let f = self.settings.favorites.remove(i);
                        forget_token(&f);
                        close = true;
                    }
                    if ui.button("Abbrechen").clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.confirm_delete = None;
            self.save();
        }
    }

    fn password_dialog(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.pw_prompt else { return };
        let mut done: Option<bool> = None;
        egui::Window::new("Anmeldung")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!("{} auf {}", p.fav.account(), p.fav.host));
                if let Some(e) = &p.error {
                    ui.colored_label(egui::Color32::from_rgb(230, 120, 80), e);
                }
                let r = ui.add(
                    egui::TextEdit::singleline(&mut p.password)
                        .password(true)
                        .hint_text("Passwort"),
                );
                if !r.has_focus() && p.password.is_empty() {
                    r.request_focus();
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    done = Some(true);
                }
                ui.checkbox(&mut p.save, "Passwort speichern")
                    .on_hover_text("Im Schlüsselbund (KWallet, GNOME Keyring, KeePassXC), sonst verschlüsselt mit systemd-creds. Gespeichert wird erst nach erfolgreicher Anmeldung.");
                ui.horizontal(|ui| {
                    if ui.button("Verbinden").clicked() {
                        done = Some(true);
                    }
                    if ui.button("Abbrechen").clicked() {
                        done = Some(false);
                    }
                });
            });
        match done {
            Some(true) => {
                let p = self.pw_prompt.take().expect("open");
                let save = p.save.then(|| p.password.clone());
                self.start(p.fav, p.adhoc, Some(p.password), save);
            }
            Some(false) => self.pw_prompt = None,
            None => {}
        }
    }

    fn cert_dialog(&mut self, ctx: &egui::Context) {
        let Some((idx, c)) = self
            .sessions
            .iter()
            .enumerate()
            .find_map(|(i, s)| s.cert.clone().map(|c| (i, c)))
        else {
            return;
        };
        let mut answer: Option<bool> = None;
        let title = if c.changed {
            "Zertifikat geändert"
        } else {
            "Unbekanntes Zertifikat"
        };
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(520.0);
                if c.changed {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 120, 80),
                        format!("Das Zertifikat von {} hat sich seit der letzten Verbindung geändert. Das passiert, wenn Windows es erneuert, kann aber auch auf einen Angriff hindeuten. Nur vertrauen, wenn die Änderung erwartet ist.", c.host),
                    );
                } else {
                    ui.label(format!("{} zeigt ein Zertifikat, das noch nicht bekannt ist (bei Rechnern ohne Entra ID normal: Windows stellt es selbst aus). Vertrauen speichert es, danach wird nicht mehr gefragt.", c.host));
                }
                ui.add_space(6.0);
                egui::Grid::new("cert").num_columns(2).spacing([10.0, 4.0]).show(ui, |ui| {
                    for (k, v) in [
                        ("Name", &c.common_name),
                        ("Inhaber", &c.subject),
                        ("Aussteller", &c.issuer),
                        ("Fingerabdruck", &c.fingerprint),
                    ] {
                        if v.is_empty() {
                            continue;
                        }
                        ui.weak(k);
                        ui.add(egui::Label::new(egui::RichText::new(v).monospace()).wrap());
                        ui.end_row();
                    }
                    if c.changed {
                        ui.weak("Bisher");
                        ui.add(
                            egui::Label::new(egui::RichText::new(&c.old_fingerprint).monospace())
                                .wrap(),
                        );
                        ui.end_row();
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Vertrauen und verbinden").clicked() {
                        answer = Some(true);
                    }
                    if ui.button("Abbrechen").clicked() {
                        answer = Some(false);
                    }
                });
            });
        if let Some(a) = answer {
            self.sessions[idx].answer_cert(a);
        }
    }

    fn apply_editor(&mut self, ed: &Editor) -> std::result::Result<(), String> {
        match ed.target {
            Target::Defaults => {
                self.settings.defaults = ed.defaults.clone();
                self.settings.close_to_tray = Some(ed.close_to_tray);
            }
            Target::Favorite(idx) => {
                let mut f = ed.fav.clone();
                f.name = f.name.trim().to_string();
                f.host = f.host.trim().to_string();
                f.user = f.user.trim().to_string();
                if f.name.is_empty() {
                    f.name = f.host.clone();
                }
                if f.host.is_empty() {
                    return Err("Bitte einen Host angeben.".into());
                }
                let clash = self
                    .settings
                    .favorites
                    .iter()
                    .enumerate()
                    .any(|(i, o)| o.name == f.name && Some(i) != idx);
                if clash {
                    return Err(format!("Es gibt schon einen Favoriten „{}“.", f.name));
                }
                // A running ad-hoc session saved from its bar now belongs to this favorite.
                if idx.is_none()
                    && let Some(s) = self
                        .sessions
                        .iter_mut()
                        .find(|s| s.running() && s.adhoc && s.fav.host == f.host)
                {
                    s.fav = f.clone();
                    s.adhoc = false;
                }
                match idx {
                    Some(i) => self.settings.favorites[i] = f,
                    None => self.settings.favorites.push(f),
                }
            }
        }
        Ok(())
    }
}

impl Launcher {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_tray(ctx);
        self.poll_session();
        if let Some(d) = self.reach_tick() {
            ctx.request_repaint_after(d.max(Duration::from_millis(200)));
        }
        if std::mem::take(&mut self.show_requested) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        // RDPC_TEST_CLOSE=<s>: closes the window after s seconds (tray mode test).
        if std::env::var_os("RDPC_TEST_CLOSE").is_some() && !self.test_closed {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if let Some(secs) = std::env::var("RDPC_TEST_CLOSE")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            && ctx.input(|i| i.time) > secs
            && !self.test_closed
        {
            self.test_closed = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // Closing the window ends this window only; run() then keeps the app in the tray
        // (setting "Fenster schließen = Tray") or quits.
        // Follow the session log while one runs.
        if self.active() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

/// Status light of a favorite: green = RDP answers, yellow = reachable without RDP answer,
/// grey = not reachable, ring (None) = not checked yet or check off; with a tooltip text.
fn reach_color(e: Option<&ReachEntry>) -> (Option<egui::Color32>, String) {
    let (c, tip) = match e.map(|e| &e.status) {
        None => (None, "Erreichbarkeit wird nicht geprüft".to_string()),
        Some(Reach::Unknown) => (None, "wird geprüft …".to_string()),
        Some(Reach::Rdp) => (Some(theme::GREEN), "erreichbar, RDP antwortet".to_string()),
        Some(Reach::NoRdp(why)) => (Some(theme::YELLOW), why.clone()),
        Some(Reach::Down(why)) => (Some(theme::GREY), format!("nicht erreichbar: {why}")),
    };
    let when = e
        .and_then(|e| e.checked)
        .map(|t| format!("\ngeprüft vor {} s", t.elapsed().as_secs()))
        .unwrap_or_default();
    (c, format!("{tip}{when}"))
}

// ---- settings widgets -----------------------------------------------------------------------

fn categories(target: &Target) -> Vec<&'static str> {
    let mut c = vec![
        "Allgemein",
        "Anzeige",
        "Audio & Geräte",
        "Freigaben",
        "Leistung",
        "Erweitert",
    ];
    if *target == Target::Defaults {
        c[0] = "Launcher";
    }
    c
}

/// One category of the settings page. Favorites edit overrides (with "Standard verwenden"),
/// the global settings edit the values themselves.
fn editor_ui(
    ui: &mut egui::Ui,
    ed: &mut Editor,
    category: &str,
    outputs: &[rdp_viewer::wl_outputs::LocalOutput],
) {
    let defaults_mode = ed.target == Target::Defaults;
    let mut ov = if defaults_mode {
        full_overrides(&ed.defaults)
    } else {
        ed.fav.overrides.clone()
    };
    let base = if defaults_mode {
        Options::default()
    } else {
        ed.defaults.clone()
    };
    let custom = !defaults_mode;
    match category {
        "Allgemein" => {
            card(
                ui,
                "Verbindung",
                "Name in der Liste, Rechnername und Konto (bei Entra ID die E-Mail-Adresse).",
                |ui| {
                    egui::Grid::new("general")
                        .num_columns(2)
                        .spacing([12.0, 6.0])
                        .show(ui, |ui| {
                            ui.label("Name");
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.fav.name).desired_width(260.0),
                            );
                            ui.end_row();
                            ui.label("Host");
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.fav.host)
                                    .hint_text("rechner oder rechner:port")
                                    .desired_width(260.0),
                            );
                            ui.end_row();
                            ui.label("Benutzer");
                            ui.add(
                                egui::TextEdit::singleline(&mut ed.fav.user)
                                    .hint_text(if ed.fav.entra {
                                        "name@firma.de"
                                    } else {
                                        "Benutzername"
                                    })
                                    .desired_width(260.0),
                            );
                            ui.end_row();
                            ui.label("Domäne");
                            ui.add_enabled(
                                !ed.fav.entra,
                                egui::TextEdit::singleline(&mut ed.fav.domain)
                                    .hint_text("leer = lokales Konto")
                                    .desired_width(260.0),
                            );
                            ui.end_row();
                        });
                },
            );
            card(
                ui,
                "Anmeldung mit Entra ID",
                "Für Entra-ID-Rechner (Firmenkonto mit 2FA). Aus: Anmeldung mit Benutzer und Passwort, für Rechner in einer Domäne oder mit lokalem Konto. Das Passwort wird beim Verbinden abgefragt und auf Wunsch sicher gespeichert (Schlüsselbund, sonst systemd-creds).",
                |ui| {
                    ui.checkbox(&mut ed.fav.entra, "Entra ID verwenden");
                },
            );
            card(
                ui,
                "Beim Start verbinden",
                "Der Launcher verbindet sich beim Öffnen sofort mit diesem Favoriten.",
                |ui| {
                    ui.checkbox(&mut ed.fav.connect_on_start, "Automatisch verbinden");
                },
            );
            reach_ui(ui, custom, &base, &mut ov, &mut ed.reach_text);
        }
        "Launcher" => {
            card(
                ui,
                "Fenster schließen",
                "Das X schließt nur das Fenster, der Launcher bleibt als Symbol im Tray (Verbinden, Trennen, Launcher öffnen, Beenden). Aus: Das X beendet den Launcher, eine laufende Sitzung bleibt bestehen.",
                |ui| {
                    ui.checkbox(&mut ed.close_to_tray, "Beim Schließen ins Tray");
                },
            );
            reach_ui(ui, custom, &base, &mut ov, &mut ed.reach_text);
            card(
                ui,
                "Logs",
                "Jede Sitzung schreibt ein Log; die letzten 20 werden aufbewahrt.",
                |ui| {
                    ui.horizontal(|ui| {
                        ui.monospace(log_dir().display().to_string());
                        if ui.small_button("Öffnen").clicked() {
                            let _ = std::fs::create_dir_all(log_dir());
                            let _ = Command::new("xdg-open").arg(log_dir()).spawn();
                        }
                    });
                },
            );
            ui.weak("Die übrigen Kategorien legen die Standardwerte für alle Favoriten fest. Einzelne Favoriten können sie überschreiben.");
        }
        "Anzeige" => {
            card(
                ui,
                "Monitore",
                "Welche Monitore die Windows-Sitzung nutzt (Klick auf die Kästchen) und welcher davon Primärmonitor mit der Windows-Taskleiste ist.",
                |ui| {
                    monitors_ui(ui, custom, &base, &mut ov, outputs);
                },
            );
            row_bool(
                ui,
                custom,
                "Im Fenstermodus starten",
                "Startet als Fenster mit einem 1:1-Ausschnitt statt im Vollbild. Umschalten jederzeit mit Strg+Alt+Enter.",
                base.start_windowed,
                &mut ov.start_windowed,
            );
            row_bool(
                ui,
                custom,
                "Verbindungsleiste angepinnt",
                "Die Leiste oben am Primärmonitor bleibt sichtbar. Aus: Sie blendet sich aus und erscheint, wenn die Maus an den oberen Rand stößt.",
                base.bar_pinned,
                &mut ov.bar_pinned,
            );
        }
        "Audio & Geräte" => {
            row_bool(
                ui,
                custom,
                "Tonwiedergabe",
                "Ton von Windows über die Lautsprecher bzw. das Headset dieses Rechners.",
                base.sound,
                &mut ov.sound,
            );
            row_bool(
                ui,
                custom,
                "Mikrofon",
                "Das Mikrofon dieses Rechners steht in Windows zur Verfügung (z. B. für Teams).",
                base.microphone,
                &mut ov.microphone,
            );
            row_bool(
                ui,
                custom,
                "Webcam",
                "Die Webcam dieses Rechners steht in Windows zur Verfügung (z. B. für Teams-Videoanrufe).",
                base.camera,
                &mut ov.camera,
            );
        }
        "Freigaben" => {
            row_bool(
                ui,
                custom,
                "Zwischenablage",
                "Kopieren und Einfügen zwischen Linux und Windows: Text, Bilder und Dateien (auch Ordner).",
                base.clipboard,
                &mut ov.clipboard,
            );
            card(
                ui,
                "Bandbreite für Dateien",
                "Dateien über die Zwischenablage teilen sich die Verbindung mit Bild und Maus. Eine Grenze hält die Sitzung beim Kopieren großer Dateien flüssig. 0 = unbegrenzt.",
                |ui| {
                    let d = base.clipboard_limit;
                    let shown = if d == 0 {
                        "unbegrenzt".to_string()
                    } else {
                        format!("{d} Mbit/s")
                    };
                    let own = use_default(ui, custom, &d, &shown, &mut ov.clipboard_limit);
                    let mut v = ov.clipboard_limit.unwrap_or(d);
                    ui.add_enabled_ui(own, |ui| {
                        if ui
                            .add(
                                egui::DragValue::new(&mut v)
                                    .range(0..=10_000)
                                    .suffix(" Mbit/s"),
                            )
                            .changed()
                        {
                            ov.clipboard_limit = Some(v);
                        }
                    });
                },
            );
            drives_ui(ui, custom, &base.drives, &mut ov.drives);
            printers_ui(ui, custom, &base.printers, &mut ov.printers);
        }
        "Leistung" => {
            decoder_ui(ui, custom, &base.decoder, &mut ov.decoder);
            row_bool(
                ui,
                custom,
                "Nur AVC420 anbieten",
                "Einfachere Bildkodierung ohne volle Farbauflösung. Nur zur Fehlersuche: Windows weicht dann auf ältere, langsamere Verfahren aus.",
                base.avc420_only,
                &mut ov.avc420_only,
            );
            row_bool(
                ui,
                custom,
                "Chroma-Rückfilter",
                "Macht farbige Kanten bei AVC444 etwas schärfer. Normalerweise an lassen.",
                base.reverse_filter,
                &mut ov.reverse_filter,
            );
        }
        "Erweitert" => {
            row_bool(
                ui,
                custom,
                "Automatisch neu verbinden",
                "Nach einem kurzen Netzaussetzer verbindet sich die Sitzung von selbst wieder, ohne neue Anmeldung.",
                base.auto_reconnect,
                &mut ov.auto_reconnect,
            );
            row_bool(
                ui,
                custom,
                "Aktiv halten",
                "Hält die Sitzung aktiv und verhindert die automatische Bildschirmsperre von Windows.",
                base.keep_active,
                &mut ov.keep_active,
            );
            row_bool(
                ui,
                custom,
                "Tastatur-Grab im Vollbild",
                "Tastenkombinationen wie Alt+Tab oder die Windows-Taste gehen im Vollbild an Windows statt an KDE. Lösen mit Strg+Alt+K.",
                base.keyboard_grab,
                &mut ov.keyboard_grab,
            );
            row_choice(
                ui,
                custom,
                "Anmeldung",
                "Wie die Entra-Anmeldung abläuft. Standard ist das eigene Fenster: einmal mit Passwort und 2FA, danach still.",
                &base.login_helper,
                &mut ov.login_helper,
                HELPERS,
            );
            let levels: Vec<(&str, &str)> = LOG_LEVELS.iter().map(|l| (*l, *l)).collect();
            row_choice(
                ui,
                custom,
                "Log-Stufe",
                "Wie ausführlich das Sitzungs-Log ist. DEBUG nur zur Fehlersuche, die Logs werden dann groß.",
                &base.log_level,
                &mut ov.log_level,
                &levels,
            );
        }
        _ => {}
    }
    if defaults_mode {
        ed.defaults = Favorite {
            overrides: ov,
            ..Default::default()
        }
        .resolve(&Options::default());
    } else {
        ed.fav.overrides = ov;
    }
}

/// A setting as a card: title, explanation, controls.
fn card(ui: &mut egui::Ui, title: &str, desc: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.strong(title);
            ui.add(egui::Label::new(egui::RichText::new(desc).weak()).wrap());
            ui.add_space(4.0);
            add(ui);
        });
    ui.add_space(6.0);
}

fn full_overrides(o: &Options) -> Overrides {
    Overrides {
        monitors: Some(o.monitors.clone()),
        primary: Some(o.primary.clone()),
        start_windowed: Some(o.start_windowed),
        bar_pinned: Some(o.bar_pinned),
        sound: Some(o.sound),
        microphone: Some(o.microphone),
        camera: Some(o.camera),
        clipboard: Some(o.clipboard),
        drives: Some(o.drives.clone()),
        clipboard_limit: Some(o.clipboard_limit),
        printers: Some(o.printers.clone()),
        decoder: Some(o.decoder.clone()),
        avc420_only: Some(o.avc420_only),
        reverse_filter: Some(o.reverse_filter),
        auto_reconnect: Some(o.auto_reconnect),
        keyboard_grab: Some(o.keyboard_grab),
        keep_active: Some(o.keep_active),
        login_helper: Some(o.login_helper.clone()),
        log_level: Some(o.log_level.clone()),
        reach_check: Some(o.reach_check),
        reach_intervals: Some(o.reach_intervals.clone()),
        reach_stage: Some(o.reach_stage),
    }
}

/// "Standard verwenden (aktuell: …)" for favorites; returns whether an own value is set.
fn use_default<T: Clone>(
    ui: &mut egui::Ui,
    custom: bool,
    d: &T,
    shown: &str,
    o: &mut Option<T>,
) -> bool {
    if !custom {
        if o.is_none() {
            *o = Some(d.clone());
        }
        return true;
    }
    let mut std = o.is_none();
    if ui
        .checkbox(&mut std, format!("Standard verwenden (aktuell: {shown})"))
        .changed()
    {
        *o = (!std).then(|| d.clone());
    }
    !std
}

fn on_off(b: bool) -> &'static str {
    if b { "an" } else { "aus" }
}

fn row_bool(
    ui: &mut egui::Ui,
    custom: bool,
    title: &str,
    desc: &str,
    d: bool,
    o: &mut Option<bool>,
) {
    card(ui, title, desc, |ui| {
        let own = use_default(ui, custom, &d, on_off(d), o);
        let mut v = o.unwrap_or(d);
        ui.add_enabled_ui(own, |ui| {
            if ui.checkbox(&mut v, "Aktiv").changed() {
                *o = Some(v);
            }
        });
    });
}

fn row_choice(
    ui: &mut egui::Ui,
    custom: bool,
    title: &str,
    desc: &str,
    d: &str,
    o: &mut Option<String>,
    choices: &[(&str, &str)],
) {
    let label_of = |k: &str| {
        choices
            .iter()
            .find(|c| c.0 == k)
            .map_or(k.to_string(), |c| c.1.to_string())
    };
    card(ui, title, desc, |ui| {
        let own = use_default(ui, custom, &d.to_string(), &label_of(d), o);
        let mut v = o.clone().unwrap_or_else(|| d.to_string());
        ui.add_enabled_ui(own, |ui| {
            egui::ComboBox::from_id_salt(title)
                .selected_text(label_of(&v))
                .width(280.0)
                .show_ui(ui, |ui| {
                    for (key, text) in choices {
                        if ui.selectable_label(v == *key, *text).clicked() {
                            v = key.to_string();
                            *o = Some(v.clone());
                        }
                    }
                });
        });
    });
}

const DECODERS: &[(&str, &str)] = &[
    ("auto", "Automatisch (empfohlen)"),
    ("nvidia", "NVIDIA (NVDEC)"),
    ("vaapi", "AMD / Intel (VA-API)"),
    ("software", "CPU (ohne Hardware-Dekodierung)"),
];

/// Result of `rmc --probe-gpu` per decoder setting, checked once in the background.
fn gpu_probe(choice: &str) -> Option<Result<(String, String), String>> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    type Probes = Mutex<HashMap<String, Option<Result<(String, String), String>>>>;
    static CACHE: OnceLock<Probes> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut c = cache.lock().expect("lock");
    if let Some(v) = c.get(choice) {
        return v.clone();
    }
    c.insert(choice.to_string(), None);
    let key = choice.to_string();
    std::thread::spawn(move || {
        let out = std::env::current_exe().and_then(|exe| {
            Command::new(exe)
                .args(["--probe-gpu", "--decoder", &key])
                .output()
        });
        let r = match out {
            Ok(o) => {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                let line = text
                    .lines()
                    .find_map(|l| l.strip_prefix("RMC-GPU\t"))
                    .map(str::to_string);
                let summary = text
                    .lines()
                    .find_map(|l| l.strip_prefix(crate::GPU_LINE))
                    .unwrap_or_default()
                    .to_string();
                match line {
                    Some(l) => Ok((l.split('\t').next().unwrap_or("").to_string(), summary)),
                    None => {
                        let err = String::from_utf8_lossy(&o.stderr);
                        Err(err
                            .lines()
                            .rfind(|l| l.starts_with("Error"))
                            .unwrap_or("nicht verfügbar")
                            .trim_start_matches("Error: ")
                            .to_string())
                    }
                }
            }
            Err(e) => Err(e.to_string()),
        };
        if let Some(c) = CACHE.get() {
            c.lock().expect("lock").insert(key, Some(r));
        }
    });
    None
}

/// Graphics decoding: choice and what this machine actually uses with it.
fn decoder_ui(ui: &mut egui::Ui, custom: bool, d: &str, o: &mut Option<String>) {
    let label_of = |k: &str| {
        DECODERS
            .iter()
            .find(|c| c.0 == k)
            .map_or(k.to_string(), |c| c.1.to_string())
    };
    card(
        ui,
        "Grafikbeschleunigung",
        "Womit das Bild der Sitzung dekodiert wird. „Automatisch“ nimmt eine NVIDIA-Grafikkarte (NVDEC), sonst AMD oder Intel über VA-API, sonst die CPU. Zusammengesetzt und dargestellt wird immer über Vulkan. Eine feste Wahl hilft bei Treiberproblemen oder zum Vergleichen.",
        |ui| {
            let own = use_default(ui, custom, &d.to_string(), &label_of(d), o);
            let mut v = o.clone().unwrap_or_else(|| d.to_string());
            ui.add_enabled_ui(own, |ui| {
                egui::ComboBox::from_id_salt("decoder")
                    .selected_text(label_of(&v))
                    .width(280.0)
                    .show_ui(ui, |ui| {
                        for (key, text) in DECODERS {
                            if ui.selectable_label(v == *key, *text).clicked() {
                                v = key.to_string();
                                *o = Some(v.clone());
                            }
                        }
                    });
            });
            ui.add_space(4.0);
            match gpu_probe(&v) {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(theme::weak("Grafik wird geprüft …"));
                    });
                    ui.ctx().request_repaint_after(Duration::from_millis(200));
                }
                Some(Ok((kind, summary))) => {
                    let (color, head) = match kind.as_str() {
                        "software" => (theme::YELLOW, "Ohne Hardware-Dekodierung:"),
                        _ => (theme::GREEN, "Erkannt:"),
                    };
                    ui.horizontal_wrapped(|ui| {
                        theme::dot(ui, Some(color), 10.0);
                        ui.label(egui::RichText::new(head).strong());
                        ui.label(summary);
                    });
                }
                Some(Err(e)) => {
                    ui.horizontal_wrapped(|ui| {
                        theme::dot(ui, Some(theme::RED), 10.0);
                        ui.label(egui::RichText::new("Nicht verfügbar:").strong());
                        ui.label(e);
                    });
                }
            }
        },
    );
}

/// Reachability check: on/off and the check intervals (one group for "Standard verwenden").
fn reach_ui(
    ui: &mut egui::Ui,
    custom: bool,
    base: &Options,
    ov: &mut Overrides,
    text: &mut Option<String>,
) {
    use crate::reach::{format_intervals, parse_intervals};
    card(
        ui,
        "Erreichbarkeit prüfen",
        "Der Launcher prüft, ob der Rechner per RDP antwortet (kurzer Verbindungsaufbau ohne Anmeldung), und zeigt das mit der Statusleuchte am Favoriten: grün = erreichbar, gelb = Rechner antwortet, aber kein RDP (z. B. noch beim Hochfahren), grau = nicht erreichbar. Nach dem Öffnen des Launchers wird oft geprüft, danach seltener: jede Stufe gilt für die angegebene Dauer, die letzte bleibt.",
        |ui| {
            let own = use_default(
                ui,
                custom,
                &base.reach_check,
                on_off(base.reach_check),
                &mut ov.reach_check,
            );
            let mut v = ov.reach_check.unwrap_or(base.reach_check);
            ui.add_enabled_ui(own, |ui| {
                if ui.checkbox(&mut v, "Aktiv").changed() {
                    ov.reach_check = Some(v);
                }
            });
            ui.add_space(6.0);
            let shown = format!(
                "{} s, je {} s",
                format_intervals(&base.reach_intervals),
                base.reach_stage
            );
            // intervals and stage duration share one "Standard verwenden"
            let mut both = ov.reach_intervals.clone().zip(ov.reach_stage);
            let own = use_default(
                ui,
                custom,
                &(base.reach_intervals.clone(), base.reach_stage),
                &shown,
                &mut both,
            );
            if !own {
                *text = None;
            }
            let (mut list, mut stage) =
                both.unwrap_or_else(|| (base.reach_intervals.clone(), base.reach_stage));
            ui.add_enabled_ui(own, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Abstände (Sekunden)");
                    let t = text.get_or_insert_with(|| format_intervals(&list));
                    let r = ui.add(egui::TextEdit::singleline(t).desired_width(160.0));
                    match parse_intervals(t) {
                        Some(v) => list = v,
                        None if r.lost_focus() => *t = format_intervals(&list),
                        None => {
                            ui.colored_label(
                                egui::Color32::from_rgb(220, 120, 60),
                                "z. B. 5, 15, 25, 40, 60",
                            );
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Dauer je Stufe");
                    ui.add(
                        egui::DragValue::new(&mut stage)
                            .range(5..=3600)
                            .suffix(" s"),
                    );
                });
            });
            if own {
                ov.reach_intervals = Some(list);
                ov.reach_stage = Some(stage);
            } else {
                ov.reach_intervals = None;
                ov.reach_stage = None;
            }
        },
    );
}

/// Redirected printers: the CUPS printers of this machine with checkboxes.
fn printers_ui(ui: &mut egui::Ui, custom: bool, d: &[String], o: &mut Option<Vec<String>>) {
    card(
        ui,
        "Drucker",
        "Ausgewählte Drucker dieses Rechners erscheinen in Windows (als „umgeleitet“). Gedruckt wird über CUPS. In der Sitzung lassen sich weitere über das Zahnrad in der Leiste dazunehmen.",
        |ui| {
            let shown = if d.is_empty() {
                "keine".to_string()
            } else {
                d.join(", ")
            };
            let own = use_default(ui, custom, &d.to_vec(), &shown, o);
            let mut list = o.clone().unwrap_or_else(|| d.to_vec());
            let mut changed = false;
            let found = crate::menu::cups_printers_cached();
            if found.is_none() {
                ui.ctx().request_repaint_after(Duration::from_millis(200));
            }
            let all = found.clone().unwrap_or_default();
            ui.add_enabled_ui(own, |ui| {
                if found.is_none() {
                    ui.weak("Drucker werden gesucht …");
                } else if all.is_empty() {
                    ui.weak("Keine Drucker gefunden (CUPS: lpstat -e).");
                }
                for p in &all {
                    let mut on = list.contains(p);
                    if ui.checkbox(&mut on, p.replace('_', " ")).changed() {
                        if on {
                            list.push(p.clone());
                        } else {
                            list.retain(|x| x != p);
                        }
                        changed = true;
                    }
                }
            });
            if changed {
                *o = Some(list);
            }
        },
    );
}

/// Redirected folders: list with name and path, add/remove.
fn drives_ui(ui: &mut egui::Ui, custom: bool, d: &[Drive], o: &mut Option<Vec<Drive>>) {
    card(
        ui,
        "Laufwerke (Ordner-Mappings)",
        "Lokale Ordner erscheinen in Windows im Explorer unter „Dieser PC“ als Laufwerk „<Name> auf <Rechner>“. Name frei wählbar, Pfad z. B. ~/Dokumente.",
        |ui| {
            let shown = if d.is_empty() {
                "keine".to_string()
            } else {
                d.iter()
                    .map(|x| x.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let own = use_default(ui, custom, &d.to_vec(), &shown, o);
            let mut list = o.clone().unwrap_or_else(|| d.to_vec());
            let mut changed = false;
            ui.add_enabled_ui(own, |ui| {
                let mut remove = None;
                egui::Grid::new("drives")
                    .num_columns(3)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        if !list.is_empty() {
                            ui.weak("Name in Windows");
                            ui.weak("Lokaler Ordner");
                            ui.end_row();
                        }
                        for (i, dr) in list.iter_mut().enumerate() {
                            changed |= ui
                                .add(egui::TextEdit::singleline(&mut dr.name).desired_width(140.0))
                                .changed();
                            changed |= ui
                                .add(egui::TextEdit::singleline(&mut dr.path).desired_width(260.0))
                                .changed();
                            if ui.small_button("Entfernen").clicked() {
                                remove = Some(i);
                            }
                            ui.end_row();
                        }
                    });
                if let Some(i) = remove {
                    list.remove(i);
                    changed = true;
                }
                ui.horizontal(|ui| {
                    let mut add = |name: &str, path: &str| {
                        list.push(Drive {
                            name: name.into(),
                            path: path.into(),
                        });
                        changed = true;
                    };
                    if ui.button("Ordner hinzufügen").clicked() {
                        add("Ordner", "~/");
                    }
                    if ui.small_button("+ Persönlicher Ordner").clicked() {
                        add("Linux-Home", "~");
                    }
                    if ui.small_button("+ Downloads").clicked() {
                        add("Downloads", "~/Downloads");
                    }
                });
            });
            if own && (changed || o.is_none()) {
                *o = Some(list);
            }
        },
    );
}

/// Monitor selection with a small topology preview (portrait shown upright).
fn monitors_ui(
    ui: &mut egui::Ui,
    custom: bool,
    base: &Options,
    ov: &mut Overrides,
    outputs: &[rdp_viewer::wl_outputs::LocalOutput],
) {
    let shown = if base.monitors.is_empty() {
        "alle".to_string()
    } else {
        base.monitors.join(", ")
    };
    let own = use_default(ui, custom, &base.monitors, &shown, &mut ov.monitors);
    if custom {
        if ov.primary.is_some() != own {
            ov.primary = own.then(|| base.primary.clone());
        }
    } else if ov.primary.is_none() {
        ov.primary = Some(base.primary.clone());
    }
    let mut sel = ov.monitors.clone().unwrap_or_else(|| base.monitors.clone());
    let mut primary = ov.primary.clone().unwrap_or_else(|| base.primary.clone());
    ui.add_enabled_ui(own, |ui| {
        if outputs.is_empty() {
            ui.weak("Keine Monitore erkannt (alle werden genutzt).");
            return;
        }
        let (minx, miny) = outputs
            .iter()
            .fold((i32::MAX, i32::MAX), |(a, b), o| (a.min(o.x), b.min(o.y)));
        let maxx = outputs
            .iter()
            .map(|o| o.x + o.width as i32)
            .max()
            .unwrap_or(1);
        let maxy = outputs
            .iter()
            .map(|o| o.y + o.height as i32)
            .max()
            .unwrap_or(1);
        let (w, h) = ((maxx - minx) as f32, (maxy - miny) as f32);
        let avail = ui.available_width().min(460.0);
        let scale = (avail / w).min(120.0 / h);
        let (rect, _) =
            ui.allocate_exact_size(egui::vec2(w * scale, h * scale), egui::Sense::hover());
        for o in outputs {
            let on = sel.is_empty() || sel.contains(&o.name);
            let r = egui::Rect::from_min_size(
                rect.min + egui::vec2((o.x - minx) as f32 * scale, (o.y - miny) as f32 * scale),
                egui::vec2(o.width as f32 * scale, o.height as f32 * scale),
            )
            .shrink(2.0);
            // click on a monitor toggles it
            let resp = ui.interact(
                r,
                ui.scope_id().with(("mon", &o.name)),
                egui::Sense::click(),
            );
            if resp.clicked() {
                if sel.is_empty() {
                    sel = outputs.iter().map(|x| x.name.clone()).collect();
                }
                if on {
                    sel.retain(|n| n != &o.name);
                } else {
                    sel.push(o.name.clone());
                }
                if sel.len() == outputs.len() {
                    sel.clear();
                }
            }
            let is_primary = if primary.is_empty() {
                o.order == Some(0)
            } else {
                primary == o.name
            };
            let painter = ui.painter();
            let fill = if on {
                ui.visuals().selection.bg_fill
            } else {
                ui.visuals().faint_bg_color
            };
            painter.rect_filled(r, 3.0, fill);
            painter.rect_stroke(
                r,
                3.0,
                ui.visuals().widgets.noninteractive.fg_stroke,
                egui::StrokeKind::Inside,
            );
            painter.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                format!(
                    "{}\n{}×{}{}",
                    o.name,
                    o.width,
                    o.height,
                    if is_primary { "\nprimär" } else { "" }
                ),
                egui::FontId::proportional(11.0),
                ui.visuals().text_color(),
            );
        }
        ui.horizontal(|ui| {
            ui.label("Primärmonitor");
            let shown = if primary.is_empty() {
                "wie in KDE".to_string()
            } else {
                primary.clone()
            };
            egui::ComboBox::from_id_salt("primary")
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(primary.is_empty(), "wie in KDE")
                        .clicked()
                    {
                        primary.clear();
                    }
                    for o in outputs {
                        if ui.selectable_label(primary == o.name, &o.name).clicked() {
                            primary = o.name.clone();
                        }
                    }
                });
        });
    });
    if own {
        ov.monitors = Some(sel);
        ov.primary = Some(primary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cert_line() {
        let c =
            CertPrompt::parse("RDPC-CERT\tchanged\thost:3389\tPC1\tCN=PC1\tCN=PC1\taa:bb\tcc:dd")
                .unwrap();
        assert!(c.changed);
        assert_eq!(
            (c.host.as_str(), c.common_name.as_str()),
            ("host:3389", "PC1")
        );
        assert_eq!(
            (c.fingerprint.as_str(), c.old_fingerprint.as_str()),
            ("aa:bb", "cc:dd")
        );
        assert!(
            !CertPrompt::parse("RDPC-CERT\tnew\th:1\t\t\t\tf\t")
                .unwrap()
                .changed
        );
        assert!(CertPrompt::parse("Verbunden: x").is_none());
    }

    #[test]
    fn auth_failures() {
        assert!(is_auth_failure(
            "connection failed: Logon failed. (0x00020014)"
        ));
        assert!(!is_auth_failure(
            "connection failed: The connection failed at TLS connect. (0x00020008)"
        ));
    }
}
