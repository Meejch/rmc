//! `rmc`: RMC – Remote Machine Connect. Without arguments the launcher; with connection
//! arguments a session: libfreerdp session -> GPU graphics core -> Wayland views.
//!
//! Graphics pipeline events are applied to `GpuCore`; every RDPGFX frame is acknowledged
//! only after the present that shows it has completed (or, headless, after its batch).
//! Input from the window goes through the same view transformation as the picture.
//!
//!   RDP_PASSWORD=... rmc --host 127.0.0.1 --port 3391 --user rdptest --ignore-cert --headless
//!
//! `--ignore-cert` is only accepted for loopback addresses. AAD (`--aad`) is a stub until the
//! Entra login can be tested against the target.

mod clip;
mod desktop;
mod icons;
mod jobview;
mod keymap;
mod launcher;
mod menu;
mod paths;
mod profiles;
mod reach;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use winit::application::ApplicationHandler;
use winit::event::{MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::raw_window_handle::HasDisplayHandle;
use winit::window::WindowId;

use rdp_session::{Auth, AvcData, Config, Event, Session, SessionHandle, mouse};
use rdp_viewer::{Action, CursorShape, CursorUpdate, Input, SessionOutput, Viewer, ViewerOptions};
use rdpgfx_gpu::core::{AvcView, CoreOptions, GpuCore};
use rdpgfx_model::outputs::{DesktopLayout, Monitor};
use rdpgfx_model::{Avc444Layout, Rect, SurfaceOp};

#[derive(Parser, Clone)]
#[command(
    name = "rmc",
    version,
    about = "RMC – Remote Machine Connect (ohne Argumente: Launcher; rmc connect <Favorit>; rmc install-desktop)"
)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 3389)]
    port: u16,
    #[arg(long)]
    user: Option<String>,
    #[arg(long)]
    domain: Option<String>,
    /// Environment variable holding the password (never on the command line).
    #[arg(long, default_value = "RDP_PASSWORD")]
    password_env: String,
    /// Read the password from the first line of stdin (the launcher; never in the
    /// environment or on the command line).
    #[arg(long)]
    password_stdin: bool,
    /// Ask before trusting an unknown or changed certificate (line "RDPC-CERT" on stdout,
    /// answer "yes"/"no" on stdin; the launcher shows a dialog).
    #[arg(long)]
    cert_prompt: bool,
    /// Entra ID login (RDS AAD, like FreeRDP's /sec:aad); --user is the UPN.
    #[arg(long)]
    aad: bool,
    /// AAD login helper: login (own WebView with stored sign-in, default), webview
    /// (FreeRDP's WebView, no stored sign-in), xdg (system browser) or a path.
    #[arg(long, default_value = "login")]
    aad_helper: String,
    /// Connection bar hidden until the top edge is touched (default: pinned, always visible).
    /// The pin button in the bar toggles it; the choice is remembered for the next start.
    #[arg(long)]
    bar_autohide: bool,
    /// Keep Windows active (small mouse moves every 50 s, against "away" in Teams and the
    /// idle lock); also switchable in the bar (cup button, Ctrl+Alt+A), remembered.
    #[arg(long)]
    keep_active: bool,
    /// Connection bar pinned, overriding a remembered choice.
    #[arg(long, conflicts_with = "bar_autohide")]
    bar_pinned: bool,
    /// Delete all stored sign-ins and passwords (secret store, profile of the login window)
    /// and exit.
    #[arg(long)]
    logout: bool,
    /// No audio playback.
    #[arg(long)]
    no_sound: bool,
    /// Arguments of the audio channel (FreeRDP /sound:), e.g. sys:pulse,dev:<sink>.
    #[arg(long, default_value = "sys:pulse")]
    sound_args: String,
    /// No microphone.
    #[arg(long)]
    no_mic: bool,
    /// Arguments of the microphone channel (FreeRDP /microphone:), e.g. sys:pulse,dev:<source>.
    #[arg(long, default_value = "sys:pulse")]
    mic_args: String,
    /// No camera redirection.
    #[arg(long)]
    no_camera: bool,
    /// No automatic reconnect after a network drop.
    #[arg(long)]
    no_reconnect: bool,
    /// No clipboard sharing.
    #[arg(long)]
    no_clipboard: bool,
    /// Bandwidth for files over the clipboard in Mbit/s (0 = unlimited): leaves room for
    /// picture and mouse on the same connection.
    #[arg(long, default_value_t = 50)]
    clipboard_limit: u32,
    /// Redirect a local folder as a drive in Windows: NAME=PATH (repeatable).
    #[arg(long = "drive", value_name = "NAME=PATH")]
    drives: Vec<String>,
    /// Redirect a CUPS printer (name as in `lpstat -e`); repeatable.
    #[arg(long = "printer", value_name = "NAME")]
    printers: Vec<String>,
    /// Arguments of the camera channel (FreeRDP /dvc:rdpecam,...), e.g. sys:v4l (default).
    #[arg(long, default_value = "")]
    camera_args: String,
    /// Requested desktop size.
    #[arg(long, default_value = "3440x1440")]
    size: String,
    /// Advertise AVC420 only (no AVC444).
    #[arg(long)]
    avc420_only: bool,
    /// Accept any certificate (local test servers only). Otherwise an unknown certificate is
    /// trusted on first use (or asked with --cert-prompt) and stored in ~/.config/rmc/freerdp;
    /// a changed one is rejected (or asked).
    #[arg(long)]
    ignore_cert: bool,
    /// No window: decode and acknowledge only.
    #[arg(long)]
    headless: bool,
    /// Window for automated tests (app id rdp-replay-test, never fullscreen, no focus).
    #[arg(long)]
    test_window: bool,
    /// Start in windowed mode (1:1 crop; Ctrl+Alt+Enter switches to fullscreen).
    #[arg(long)]
    windowed: bool,
    /// Monitors to use (connector names, comma separated; default: all).
    #[arg(long, value_delimiter = ',')]
    monitors: Vec<String>,
    /// Primary monitor (default: KDE's output order, else the largest).
    #[arg(long)]
    primary: Option<String>,
    /// No keyboard grab in fullscreen (on by default; toggle with Ctrl+Alt+K).
    #[arg(long)]
    no_grab: bool,
    /// Quit after this many seconds.
    #[arg(long)]
    duration: Option<u64>,
    /// Write every surface as PPM into this directory before quitting.
    #[arg(long)]
    dump_dir: Option<PathBuf>,
    /// After the first frame, send a fixed input sequence (end-to-end test).
    #[arg(long)]
    input_test: bool,
    /// Record the graphics pipeline as received (H.264 access units per surface and every
    /// command as JSON lines) into this directory, for offline analysis. Contains the screen
    /// content: keep it under local/.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Disable the reverse chroma filter of AVC444 (on by default; see docs/upstream/001).
    #[arg(long)]
    no_reverse_filter: bool,
    /// Graphics decoding: auto (NVIDIA NVDEC, else VA-API for AMD/Intel, else CPU), nvidia,
    /// vaapi or software.
    #[arg(long, default_value = "auto")]
    decoder: String,
    /// Check which graphics decoding works here, print it and exit (used by the launcher).
    #[arg(long)]
    probe_gpu: bool,
    /// FreeRDP log level (WLog), e.g. WARN, INFO, DEBUG.
    #[arg(long, default_value = "WARN")]
    log_level: String,
    /// Name shown in the connection bar (default: the host).
    #[arg(long)]
    name: Option<String>,
    /// The connection is no favorite yet: the bar offers to save it (the launcher asks).
    #[arg(long)]
    offer_favorite: bool,
}

/// Log line prefix of the graphics backend (the launcher shows it with the session).
pub const GPU_LINE: &str = "Grafik: ";

fn backend_choice(args: &Args) -> Result<rdpgfx_gpu::core::BackendChoice> {
    rdpgfx_gpu::core::BackendChoice::parse(&args.decoder).with_context(|| {
        format!(
            "unbekannte Dekodierung „{}“ (auto, nvidia, vaapi, software)",
            args.decoder
        )
    })
}

/// `--probe-gpu`: which decoding works with the chosen setting. Prints a summary line and a
/// machine-readable line "RMC-GPU\t<backend>\t<decoder device>\t<vulkan device>".
fn probe_gpu(args: &Args) -> Result<()> {
    let core = GpuCore::new(&CoreOptions {
        backend: backend_choice(args)?,
        ..Default::default()
    })?;
    let info = core.backend_info();
    println!("{GPU_LINE}{}", info.summary());
    let kind = match info.backend {
        rdpgfx_gpu::core::Backend::Cuda => "nvidia",
        rdpgfx_gpu::core::Backend::Vaapi => "vaapi",
        rdpgfx_gpu::core::Backend::Software => "software",
    };
    println!("RMC-GPU\t{kind}\t{}\t{}", info.decoder, info.vulkan);
    Ok(())
}

/// The password from the first stdin line (read once; later reconnects reuse it).
fn stdin_password() -> Option<String> {
    static PW: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PW.get_or_init(|| {
        // Unbuffered, byte by byte up to the newline: later stdin lines are the answers to
        // certificate prompts, read by the C layer.
        // (std's stdin is buffered and could take those lines, hence read(2) directly).
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        // SAFETY: reads one byte into a valid buffer from fd 0.
        while unsafe { libc::read(0, b.as_mut_ptr().cast(), 1) } == 1 {
            if b[0] == b'\n' {
                break;
            }
            line.push(b[0]);
        }
        let s = String::from_utf8(line).ok()?;
        (!s.is_empty()).then_some(s)
    })
    .clone()
}

/// Log line of the session process: the star in the bar was clicked (see launcher).
pub const SAVE_FAVORITE_LINE: &str = "Leiste: als Favorit speichern";

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// CPU time (user + system) of this process in seconds.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
    let after = stat.rsplit(')').next().unwrap_or("");
    let f: Vec<&str> = after.split_whitespace().collect();
    let ticks: f64 = f.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0)
        + f.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    ticks / 100.0
}

/// Records the graphics pipeline: `surface-<id>.h264` (access units in decode order) and
/// `commands.jsonl` (one JSON object per command, AU offsets into the stream files).
struct Recorder {
    dir: PathBuf,
    log: std::io::BufWriter<std::fs::File>,
    streams: HashMap<u16, (std::fs::File, u64)>,
    seq: u64,
}

impl Recorder {
    fn new(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            dir: dir.to_path_buf(),
            log: std::io::BufWriter::new(std::fs::File::create(dir.join("commands.jsonl"))?),
            streams: HashMap::new(),
            seq: 0,
        })
    }

    fn au(&mut self, id: u16, data: &[u8]) -> Result<serde_json::Value> {
        use std::io::Write;
        if !self.streams.contains_key(&id) {
            let f = std::fs::File::create(self.dir.join(format!("surface-{id}.h264")))?;
            self.streams.insert(id, (f, 0));
        }
        let (f, off) = self.streams.get_mut(&id).expect("inserted");
        f.write_all(data)?;
        let v = serde_json::json!({ "offset": *off, "len": data.len() });
        *off += data.len() as u64;
        Ok(v)
    }

    fn rects(r: &[Rect]) -> serde_json::Value {
        serde_json::Value::Array(
            r.iter()
                .map(|r| serde_json::json!([r.left, r.top, r.right, r.bottom]))
                .collect(),
        )
    }

    fn record(&mut self, ev: &Event) -> Result<()> {
        use std::io::Write;
        let mut v = match ev {
            Event::ResetGraphics {
                width,
                height,
                monitors,
            } => serde_json::json!({
                "cmd": "reset", "width": width, "height": height,
                "monitors": monitors.iter().map(|m| [m.left, m.top, m.right, m.bottom, i32::from(m.primary)]).collect::<Vec<_>>() }),
            Event::CreateSurface {
                id,
                width,
                height,
                format,
            } => serde_json::json!({
                "cmd": "create", "id": id, "width": width, "height": height, "format": format!("{format:?}") }),
            Event::DeleteSurface { id } => serde_json::json!({ "cmd": "delete", "id": id }),
            Event::MapSurface { id, x, y } => {
                serde_json::json!({ "cmd": "map", "id": id, "x": x, "y": y })
            }
            Event::StartFrame { id } => serde_json::json!({ "cmd": "start", "frame": id }),
            Event::EndFrame { id } => serde_json::json!({ "cmd": "end", "frame": id }),
            Event::SolidFill { id, color, rects } => serde_json::json!({
                "cmd": "fill", "id": id, "rgb": [color.r, color.g, color.b], "rects": Self::rects(rects) }),
            Event::SurfaceToSurface {
                src,
                rect,
                dst,
                points,
            } => serde_json::json!({
                "cmd": "s2s", "src": src, "rect": Self::rects(&[*rect]), "dst": dst, "points": points }),
            Event::SurfaceToCache { id, rect, slot } => serde_json::json!({
                "cmd": "s2c", "id": id, "rect": Self::rects(&[*rect]), "slot": slot }),
            Event::CacheToSurface { slot, id, points } => serde_json::json!({
                "cmd": "c2s", "slot": slot, "id": id, "points": points }),
            Event::EvictCache { slot } => serde_json::json!({ "cmd": "evict", "slot": slot }),
            Event::Avc420 { id, dest, main } => {
                let au = self.au(*id, &main.data)?;
                serde_json::json!({ "cmd": "avc420", "id": id, "dest": Self::rects(&[*dest]),
                    "main": { "rects": Self::rects(&main.rects), "au": au } })
            }
            Event::Avc444 {
                id,
                v2,
                dest,
                main,
                aux,
            } => {
                let m = match main {
                    Some(m) => Some(
                        serde_json::json!({ "rects": Self::rects(&m.rects), "au": self.au(*id, &m.data)? }),
                    ),
                    None => None,
                };
                let a = match aux {
                    Some(a) => Some(
                        serde_json::json!({ "rects": Self::rects(&a.rects), "au": self.au(*id, &a.data)? }),
                    ),
                    None => None,
                };
                serde_json::json!({ "cmd": "avc444", "id": id, "v2": v2, "dest": Self::rects(&[*dest]),
                    "main": m, "aux": a })
            }
            Event::Pixels { id, rect, bgra } => serde_json::json!({
                "cmd": "pixels", "id": id, "rect": Self::rects(&[*rect]), "bytes": bgra.len() }),
            Event::UnsupportedCodec { id, codec, rect } => serde_json::json!({
                "cmd": "unsupported", "id": id, "codec": codec, "rect": Self::rects(&[*rect]) }),
            Event::GfxClosed => serde_json::json!({ "cmd": "gfx_closed" }),
            _ => return Ok(()),
        };
        v["seq"] = serde_json::json!(self.seq);
        self.seq += 1;
        writeln!(self.log, "{v}")?;
        Ok(())
    }
}

/// Shared with the acknowledgement thread.
#[derive(Default)]
struct AckState {
    /// Frames decoded since the last CapsConfirm (MS-RDPEGFX `totalFramesDecoded`).
    total_decoded: AtomicU32,
    acked: AtomicU64,
    /// Ended, not yet acknowledged frames: end time and `totalFramesDecoded` as of that
    /// frame. The acknowledgement must carry the count up to and including its own frame,
    /// not the current one (live test 24.11.2026: Windows' encoder failed on the mismatch
    /// as soon as Teams started the camera).
    ended: Mutex<HashMap<u32, (Instant, u32)>>,
    latency_ms: Mutex<Vec<f64>>,
    latency_all: Mutex<Vec<f64>>,
}

/// Session logic shared by headless and windowed mode.
struct Client {
    core: GpuCore,
    handle: SessionHandle,
    acks: Arc<AckState>,
    desktop: (u32, u32),
    connected: bool,
    disconnected: Option<String>,
    frame_ended: bool,
    first_frame: bool,
    counts: BTreeMap<&'static str, u64>,
    unsupported: BTreeMap<u32, u64>,
    errors: u64,
    warned: HashSet<&'static str>,
    /// H.264 bytes received (all AVC views): effective bitrate for statistics.
    avc_bytes: u64,
    avc_bytes_window: u64,
    /// AVC444 updates by LC field (0 = both views, 1 = luma only, 2 = chroma only) and how
    /// often main and aux region lists differ with LC 0.
    lc: [u64; 3],
    lc0_rects_differ: u64,
    recorder: Option<Recorder>,
}

impl Client {
    /// New graphics channel or capability exchange: frame count from zero, pending
    /// acknowledgements of the old channel are dropped.
    fn reset_acks(&self) {
        self.acks.total_decoded.store(0, Ordering::Relaxed);
        if let Ok(mut m) = self.acks.ended.lock() {
            m.clear();
        }
    }

    fn new(mut core: GpuCore, handle: SessionHandle, desktop: (u32, u32)) -> Self {
        let acks = Arc::new(AckState::default());
        let (h, a) = (handle.clone(), acks.clone());
        core.on_presented(move |id| {
            let id = id as u32;
            // Frames of a closed graphics channel are no longer in the map: no ack.
            let Some((t0, total)) = a.ended.lock().ok().and_then(|mut m| m.remove(&id)) else {
                return;
            };
            h.frame_ack(id, total, 0);
            a.acked.fetch_add(1, Ordering::Relaxed);
            {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                if let Ok(mut l) = a.latency_ms.lock() {
                    l.push(ms);
                }
                if let Ok(mut l) = a.latency_all.lock() {
                    l.push(ms);
                }
            }
        });
        Self {
            core,
            handle,
            acks,
            desktop,
            connected: false,
            disconnected: None,
            frame_ended: false,
            first_frame: false,
            counts: BTreeMap::new(),
            unsupported: BTreeMap::new(),
            errors: 0,
            warned: HashSet::new(),
            avc_bytes: 0,
            avc_bytes_window: 0,
            lc: [0; 3],
            lc0_rects_differ: 0,
            recorder: None,
        }
    }

    fn warn_once(&mut self, key: &'static str, msg: String) {
        if self.warned.insert(key) {
            eprintln!("Hinweis: {msg}");
        }
    }

    /// Applies one session event. Protocol errors of single commands are counted and logged,
    /// they do not end the session.
    fn handle(&mut self, ev: Event) -> Result<()> {
        let name = match &ev {
            Event::Connected { .. } => "Connected",
            Event::Disconnected { .. } => "Disconnected",
            Event::Error(_) => "Error",
            Event::Info(_) => "Info",
            Event::GfxClosed => "GfxClosed",
            Event::ClipReady
            | Event::ClipRemoteFormats(_)
            | Event::ClipDataRequest(_)
            | Event::ClipData { .. }
            | Event::ClipFileRequest { .. }
            | Event::ClipFileData { .. } => "Zwischenablage",
            Event::CapsConfirm { .. } => "CapsConfirm",
            Event::ResetGraphics { .. } => "ResetGraphics",
            Event::CreateSurface { .. } => "CreateSurface",
            Event::DeleteSurface { .. } => "DeleteSurface",
            Event::MapSurface { .. } => "MapSurfaceToOutput",
            Event::StartFrame { .. } => "StartFrame",
            Event::EndFrame { .. } => "EndFrame",
            Event::SolidFill { .. } => "SolidFill",
            Event::SurfaceToSurface { .. } => "SurfaceToSurface",
            Event::SurfaceToCache { .. } => "SurfaceToCache",
            Event::CacheToSurface { .. } => "CacheToSurface",
            Event::EvictCache { .. } => "EvictCacheEntry",
            Event::Avc420 { .. } => "AVC420",
            Event::Avc444 { .. } => "AVC444",
            Event::Pixels { .. } => "CPU-Codec",
            Event::UnsupportedCodec { .. } => "nicht unterstützt",
            Event::PointerSet { .. } => "Zeigerform",
            Event::PointerCached { .. } => "Zeiger aus Cache",
            Event::PointerSystem { .. } => "Systemzeiger",
        };
        *self.counts.entry(name).or_default() += 1;
        if let Some(r) = &mut self.recorder
            && let Err(e) = r.record(&ev)
        {
            eprintln!("Aufnahme abgebrochen: {e:#}");
            self.recorder = None;
        }
        let bytes = match &ev {
            Event::Avc420 { main, .. } => main.data.len(),
            Event::Avc444 { main, aux, .. } => {
                main.as_ref().map_or(0, |m| m.data.len()) + aux.as_ref().map_or(0, |a| a.data.len())
            }
            _ => 0,
        } as u64;
        self.avc_bytes += bytes;
        self.avc_bytes_window += bytes;
        if let Event::Avc444 { main, aux, .. } = &ev {
            match (main, aux) {
                (Some(m), Some(a)) => {
                    self.lc[0] += 1;
                    if m.rects != a.rects {
                        self.lc0_rects_differ += 1;
                    }
                }
                (Some(_), None) => self.lc[1] += 1,
                (None, Some(_)) => self.lc[2] += 1,
                (None, None) => {}
            }
        }
        let r = self.apply(ev);
        if let Err(e) = r {
            self.errors += 1;
            if self.errors <= 10 {
                eprintln!("Fehler bei {name}: {e:#}");
            }
        }
        Ok(())
    }

    fn apply(&mut self, ev: Event) -> Result<()> {
        match ev {
            Event::Connected { width, height } => {
                self.connected = true;
                self.desktop = (width, height);
                println!("Verbunden: Desktop {width}x{height}");
            }
            Event::Disconnected { code, message } => {
                println!("Getrennt: {message}");
                self.disconnected = Some(format!("{message} (0x{code:08x})"));
            }
            Event::Error(m) => eprintln!("Sitzung: {m}"),
            Event::Info(m) => println!("Sitzung: {m}"),
            Event::ClipReady
            | Event::ClipRemoteFormats(_)
            | Event::ClipDataRequest(_)
            | Event::ClipData { .. }
            | Event::ClipFileRequest { .. }
            | Event::ClipFileData { .. } => {}
            Event::GfxClosed => {
                println!("Grafikkanal geschlossen: Surfaces und Cache verworfen");
                self.reset_acks();
                self.core.clear_all()?;
            }
            Event::CapsConfirm { version, flags } => {
                println!("GFX-Caps bestätigt: Version 0x{version:05x}, Flags 0x{flags:x}");
                // totalFramesDecoded counts from the CapsConfirm (MS-RDPEGFX 2.2.2.13).
                self.reset_acks();
            }
            Event::ResetGraphics {
                width,
                height,
                monitors,
            } => {
                println!(
                    "ResetGraphics: {width}x{height}, {} Monitor(e) {:?}",
                    monitors.len(),
                    monitors
                        .iter()
                        .map(|m| (m.left, m.top, m.right - m.left, m.bottom - m.top))
                        .collect::<Vec<_>>()
                );
                self.desktop = (width, height);
                self.core.reset_graphics()?;
            }
            Event::CreateSurface {
                id,
                width,
                height,
                format,
            } => {
                println!("CreateSurface {id}: {width}x{height} {format:?}");
                self.core.create_surface(id, width, height, format)?;
            }
            Event::DeleteSurface { id } => {
                // Some servers delete a surface before creating it (the FreeRDP shadow server
                // does on every connection); that is harmless.
                if self.core.surface_ids().contains(&id) {
                    self.core.delete_surface(id)?;
                }
            }
            Event::MapSurface { id, x, y } => {
                println!("MapSurfaceToOutput {id}: {x},{y}");
                self.core.apply(&SurfaceOp::MapToOutput { id, x, y })?;
            }
            Event::StartFrame { .. } => {}
            Event::EndFrame { id } => {
                let total = self.acks.total_decoded.fetch_add(1, Ordering::Relaxed) + 1;
                if let Ok(mut m) = self.acks.ended.lock() {
                    m.insert(id, (Instant::now(), total));
                }
                self.core.end_frame_ack(u64::from(id))?;
                self.frame_ended = true;
                self.first_frame = true;
            }
            Event::SolidFill { id, color, rects } => self.core.solid_fill(id, color, &rects)?,
            Event::SurfaceToSurface {
                src,
                rect,
                dst,
                points,
            } => self.core.surface_to_surface(src, rect, dst, &points)?,
            Event::SurfaceToCache { id, rect, slot } => {
                self.core.surface_to_cache(id, rect, slot)?
            }
            Event::CacheToSurface { slot, id, points } => {
                self.core.cache_to_surface(slot, id, &points)?
            }
            Event::EvictCache { slot } => self.core.evict(slot)?,
            Event::Avc420 { id, dest, main } => {
                self.check_dest(dest);
                self.core
                    .avc(id, Avc444Layout::V1, Some(view(&main)), None)?;
            }
            Event::Avc444 {
                id,
                v2,
                dest,
                main,
                aux,
            } => {
                self.check_dest(dest);
                let layout = if v2 {
                    Avc444Layout::V2
                } else {
                    Avc444Layout::V1
                };
                self.core
                    .avc(id, layout, main.as_ref().map(view), aux.as_ref().map(view))?;
            }
            Event::Pixels { id, rect, bgra } => self.core.upload(id, rect, &bgra)?,
            Event::UnsupportedCodec { codec, .. } => {
                *self.unsupported.entry(codec).or_default() += 1;
            }
            Event::PointerSet {
                index,
                width,
                height,
                hot_x,
                hot_y,
                bgra,
            } => {
                if self.warned.insert("pointer") {
                    println!("Zeigerform {index}: {width}x{height}, Hotspot {hot_x},{hot_y}");
                    // Small shapes as text: '#' bright, '.' dark, ' ' transparent.
                    if width <= 32 && height <= 32 {
                        for row in bgra.chunks(usize::from(width) * 4) {
                            let line: String = row
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .map(|p| {
                                    match (
                                        p[3],
                                        u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2]),
                                    ) {
                                        (0..=127, _) => ' ',
                                        (_, 384..) => '#',
                                        _ => '.',
                                    }
                                })
                                .collect();
                            println!("  |{line}|");
                        }
                    }
                }
            }
            // Shown by the viewer in windowed mode.
            Event::PointerCached { .. } | Event::PointerSystem { .. } => {}
        }
        Ok(())
    }

    /// The GPU core assumes AVC pictures start at the surface origin (to be confirmed live).
    fn check_dest(&mut self, dest: Rect) {
        if dest.left != 0 || dest.top != 0 {
            self.warn_once(
                "dest",
                format!(
                    "AVC-Zielrechteck beginnt nicht bei 0,0: {dest:?} (wird als 0,0 behandelt)"
                ),
            );
        }
    }

    /// Presents once after a batch of events if a frame ended (acknowledgements follow).
    fn after_batch(&mut self) -> Result<()> {
        if self.frame_ended {
            self.core.present()?;
            self.frame_ended = false;
        }
        Ok(())
    }

    fn stats_line(&mut self, secs: f64, cpu: f64) -> String {
        let mbit = std::mem::take(&mut self.avc_bytes_window) as f64 * 8.0 / secs / 1e6;
        let mut lat = std::mem::take(&mut *self.acks.latency_ms.lock().expect("latency lock"));
        lat.sort_by(|a, b| a.total_cmp(b));
        let mean = if lat.is_empty() {
            0.0
        } else {
            lat.iter().sum::<f64>() / lat.len() as f64
        };
        let p95 = lat
            .get(((lat.len() as f64 * 0.95) as usize).min(lat.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0.0);
        format!(
            "{} Frames quittiert ({:.1}/s), Quittung nach EndFrame Ø {mean:.1} / p95 {p95:.1} ms, H.264 {mbit:.2} Mbit/s, CPU {:.1} % eines Kerns",
            lat.len(),
            lat.len() as f64 / secs,
            cpu / secs * 100.0
        )
    }

    fn summary(&self) {
        println!("Ereignisse:");
        for (k, v) in &self.counts {
            println!("  {k:<20} {v}");
        }
        if !self.unsupported.is_empty() {
            println!("Nicht unterstützte Codecs: {:?}", self.unsupported);
        }
        let mut lat = self
            .acks
            .latency_all
            .lock()
            .map(|l| l.clone())
            .unwrap_or_default();
        lat.sort_by(|a, b| a.total_cmp(b));
        let mean = lat.iter().sum::<f64>() / lat.len().max(1) as f64;
        let max = lat.last().copied().unwrap_or(0.0);
        println!("H.264 empfangen: {:.1} MB", self.avc_bytes as f64 / 1e6);
        println!(
            "AVC444 nach LC: beide Ansichten {} (davon mit verschiedenen Regionen {}), nur Luma {}, nur Chroma {}",
            self.lc[0], self.lc0_rects_differ, self.lc[1], self.lc[2]
        );
        println!(
            "Frames dekodiert {}, quittiert {}, Fehler {}, Quittung nach EndFrame Ø {mean:.2} ms (max {max:.2} ms)",
            self.acks.total_decoded.load(Ordering::Relaxed),
            self.acks.acked.load(Ordering::Relaxed),
            self.errors
        );
    }

    /// Every surface as binary PPM (`surface-<id>.ppm`).
    fn dump(&mut self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let ids: Vec<_> = self.core.surface_ids();
        for id in ids {
            let (w, h) = self.core.surface_size(id)?;
            let bgra = self.core.read_surface(id)?;
            let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
            for px in bgra.as_chunks::<4>().0 {
                out.extend_from_slice(&[px[2], px[1], px[0]]);
            }
            let path = dir.join(format!("surface-{id}.ppm"));
            std::fs::write(&path, out)?;
            println!("Surface {id} ({w}x{h}) -> {}", path.display());
        }
        Ok(())
    }
}

fn view(d: &AvcData) -> AvcView<'_> {
    AvcView {
        au: &d.data,
        rects: &d.rects,
    }
}

/// Fixed input sequence for the end-to-end test (checked against the X11 probe's log).
fn input_test(h: SessionHandle, desktop: (u32, u32)) {
    let pause = |ms| std::thread::sleep(Duration::from_millis(ms));
    let click = |x, y, button| {
        h.send_mouse(mouse::MOVE, x, y);
        pause(80);
        h.send_mouse(button | mouse::DOWN, x, y);
        pause(50);
        h.send_mouse(button, x, y);
        pause(120);
    };
    let key = |sc: u8, ext: bool| {
        h.send_key(sc, ext, true);
        pause(30);
        h.send_key(sc, ext, false);
        pause(60);
    };
    println!("Eingabetest: Start");
    click(100, 100, mouse::BUTTON1);
    click(desktop.0 - 440, desktop.1 - 140, mouse::BUTTON2);
    click(desktop.0 / 2, desktop.1 / 2, mouse::BUTTON3);
    h.send_mouse(mouse::wheel(1, false), desktop.0 / 2, desktop.1 / 2);
    pause(100);
    h.send_mouse(mouse::wheel(-1, false), desktop.0 / 2, desktop.1 / 2);
    pause(100);
    // "rdp", Shift+A, Enter, Right arrow (extended), keypad 6 (not extended)
    for sc in [0x13, 0x20, 0x19] {
        key(sc, false);
    }
    h.send_key(0x2A, false, true);
    pause(30);
    key(0x1E, false);
    h.send_key(0x2A, false, false);
    pause(60);
    key(0x1C, false);
    key(0x4D, true);
    key(0x4D, false);
    println!("Eingabetest: gesendet");
}

fn config(args: &Args) -> Result<Config> {
    let (w, h) = args
        .size
        .split_once('x')
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))
        .context("--size WxH")?;
    if args.ignore_cert && !is_loopback(&args.host) {
        bail!("--ignore-cert ist nur für Loopback-Adressen erlaubt");
    }
    let aad_helper = args.aad.then(|| match args.aad_helper.as_str() {
        "xdg" => freerdp_helper("freerdp-xdg-aad-helper"),
        "webview" => freerdp_helper("freerdp-webview-aad-helper"),
        "login" if !webkit_available() => {
            println!(
                "Login-Fenster nicht verfügbar (WebKitGTK 4.1 fehlt), Anmeldung im Systembrowser"
            );
            freerdp_helper("freerdp-xdg-aad-helper")
        }
        "login" => login_helper_path(),
        path => path.to_string(),
    });
    if let Some(h) = &aad_helper
        && !Path::new(h).exists()
    {
        bail!("AAD-Login-Helfer nicht gefunden: {h}");
    }
    Ok(Config {
        host: args.host.clone(),
        port: args.port,
        username: args.user.clone(),
        domain: args.domain.clone(),
        password: if args.password_stdin {
            stdin_password()
        } else {
            std::env::var(&args.password_env).ok()
        },
        auth: if args.aad { Auth::Aad } else { Auth::Password },
        width: w,
        height: h,
        monitors: Vec::new(),
        avc444: !args.avc420_only,
        ignore_certificate: args.ignore_cert,
        log_level: Some(args.log_level.clone()),
        aad_helper,
        cert_tofu: !args.ignore_cert,
        cert_prompt: args.cert_prompt && !args.ignore_cert,
        sound: (!args.no_sound).then(|| args.sound_args.clone()),
        microphone: (!args.no_mic).then(|| args.mic_args.clone()),
        camera: (!args.no_camera).then(|| args.camera_args.clone()),
        auto_reconnect: !args.no_reconnect,
        clipboard: !args.no_clipboard,
        printers: args.printers.clone(),
        drives: args
            .drives
            .iter()
            .filter_map(|d| d.split_once('='))
            .map(|(n, p)| (n.trim().to_string(), expand_home(p.trim())))
            .collect(),
    })
}

fn run_headless(args: &Args) -> Result<()> {
    let cfg = config(args)?;
    let core = GpuCore::new(&CoreOptions {
        reverse_filter: !args.no_reverse_filter,
        backend: backend_choice(args)?,
        ..Default::default()
    })?;
    println!("{GPU_LINE}{}", core.backend_info().summary());
    let (tx, rx) = mpsc::channel();
    let session = Session::start(&cfg, move |ev| {
        let _ = tx.send(ev);
    })?;
    let mut client = Client::new(core, session.handle(), (cfg.width, cfg.height));
    if let Some(dir) = &args.record {
        client.recorder = Some(Recorder::new(dir)?);
        println!("Aufnahme nach {}", dir.display());
    }
    let start = Instant::now();
    let (mut stats_at, mut cpu_at) = (Instant::now(), cpu_seconds());
    let mut input_started = false;
    let until = args.duration.map(|d| start + Duration::from_secs(d));
    loop {
        if until.is_some_and(|u| Instant::now() >= u) || client.disconnected.is_some() {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(ev) => {
                client.handle(ev)?;
                while let Ok(ev) = rx.try_recv() {
                    client.handle(ev)?;
                }
                client.after_batch()?;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if args.input_test && client.first_frame && !input_started {
            input_started = true;
            let (h, d) = (client.handle.clone(), client.desktop);
            std::thread::spawn(move || input_test(h, d));
        }
        if stats_at.elapsed() >= Duration::from_secs(5) {
            let cpu = cpu_seconds();
            if client.acks.latency_ms.lock().is_ok_and(|l| !l.is_empty()) {
                println!(
                    "{}",
                    client.stats_line(stats_at.elapsed().as_secs_f64(), cpu - cpu_at)
                );
            }
            stats_at = Instant::now();
            cpu_at = cpu;
        }
    }
    client.core.flush()?;
    if let Some(dir) = &args.dump_dir {
        client.dump(dir)?;
    }
    client.summary();
    let disconnected = client.disconnected.clone();
    drop(client);
    session.stop();
    if let Some(reason) = disconnected
        && !reason.contains("0x00000000")
    {
        bail!("Verbindung beendet: {reason}");
    }
    Ok(())
}

// ---- windowed mode ------------------------------------------------------------------------

/// Local monitors used for the session and the layout sent to the server.
struct LocalLayout {
    sel: rdp_viewer::layout::Selection,
    sent: Vec<Monitor>,
}

impl LocalLayout {
    fn query(args: &Args) -> Result<Self> {
        let all = rdp_viewer::wl_outputs::query()?;
        let sel = rdp_viewer::layout::select(&all, &args.monitors, args.primary.as_deref())?;
        let sent = rdp_viewer::layout::rdp_monitors(&sel);
        println!("Monitor-Layout für den Server:");
        for (o, m) in sel.outputs.iter().zip(&sent) {
            println!(
                "  {:<10} {:>6},{:<6} {}x{}{}",
                o.name,
                m.left,
                m.top,
                m.right - m.left,
                m.bottom - m.top,
                if m.primary { " (primär)" } else { "" }
            );
        }
        Ok(Self { sel, sent })
    }

    /// Session outputs (server layout) with the local monitors that show them.
    fn session_outputs(&self, el: &ActiveEventLoop, session: &[Monitor]) -> Vec<SessionOutput> {
        let rects = DesktopLayout {
            monitors: session.to_vec(),
        }
        .outputs();
        let matched = rdp_viewer::layout::match_outputs(&self.sent, session);
        rects
            .into_iter()
            .zip(session)
            .zip(matched)
            .map(|((rect, m), local)| {
                let name = self.sel.outputs[local].name.clone();
                SessionOutput {
                    monitor: rdp_viewer::viewer::monitor_by_name(el, &name),
                    name,
                    rect,
                    primary: m.primary,
                }
            })
            .collect()
    }
}

struct App {
    args: Args,
    cfg: Config,
    local: Option<LocalLayout>,
    proxy: EventLoopProxy<Event>,
    session: Option<Session>,
    client: Option<Client>,
    viewer: Option<Viewer>,
    clipboard: Option<clip::Clipboard>,
    /// Settings menu of the bar (gear).
    menu: Option<menu::SessionMenu>,
    /// Last pointer position sent (session coordinates) and the last keep-active move.
    last_move: Option<(u32, u32)>,
    keep_at: Instant,
    pressed: HashSet<(u8, bool)>,
    started: Instant,
    input_started: bool,
    error: Option<anyhow::Error>,
    stats_at: Instant,
    cpu_at: f64,
}

impl App {
    fn init(&mut self, el: &ActiveEventLoop) -> Result<()> {
        let display = el.display_handle()?.as_raw();
        let core = GpuCore::new(&CoreOptions {
            display: Some(display),
            reverse_filter: !self.args.no_reverse_filter,
            backend: backend_choice(&self.args)?,
            ..Default::default()
        })?;
        println!("{GPU_LINE}{}", core.backend_info().summary());
        let sent = self
            .local
            .as_ref()
            .map(|l| l.sent.clone())
            .unwrap_or_default();
        let outputs = match &self.local {
            Some(l) => l.session_outputs(el, &sent),
            None => vec![SessionOutput {
                name: "Session".into(),
                rect: Rect::new(0, 0, self.cfg.width, self.cfg.height),
                primary: true,
                monitor: None,
            }],
        };
        let proxy = self.proxy.clone();
        let session = Session::start(&self.cfg, move |ev| {
            let _ = proxy.send_event(ev);
        })?;
        if self.cfg.clipboard {
            self.clipboard = clip::Clipboard::start(session.handle(), self.args.clipboard_limit)
                .map_err(|e| eprintln!("Zwischenablage nicht verfügbar: {e:#}"))
                .ok();
        }
        let mut client = Client::new(core, session.handle(), (self.cfg.width, self.cfg.height));
        if let Some(dir) = &self.args.record {
            client.recorder = Some(Recorder::new(dir)?);
            println!("Aufnahme nach {}", dir.display());
        }
        let viewer = Viewer::new(
            ViewerOptions {
                title: "RMC".into(),
                test_window: self.args.test_window,
                start_windowed: self.args.windowed,
                grab_in_fullscreen: !self.args.no_grab,
                bar_pinned: if self.args.bar_autohide {
                    false
                } else if self.args.bar_pinned {
                    true
                } else {
                    settings::bar_pinned().unwrap_or(true)
                },
                keep_active: self.args.keep_active || settings::keep_active().unwrap_or(false),
                label: self
                    .args
                    .name
                    .clone()
                    .unwrap_or_else(|| self.args.host.clone()),
                offer_favorite: self.args.offer_favorite,
            },
            outputs,
        );
        let menu = menu::SessionMenu::new(
            !self.args.no_sound,
            !self.args.no_mic,
            self.clipboard.is_some(),
            &self.cfg.drives,
            &self.cfg.printers,
            self.args.name.is_some() && !self.args.offer_favorite,
        );
        let mut viewer = viewer;
        viewer.set_menu(menu.items(self.clipboard.as_ref().is_some_and(|c| c.enabled())));
        self.menu = Some(menu);
        // Windows open with the server's layout (first ResetGraphics), not with the one we
        // sent: a server with a fixed screen would otherwise flash windows on every monitor.
        self.viewer = Some(viewer);
        self.client = Some(client);
        self.session = Some(session);
        self.started = Instant::now();
        Ok(())
    }

    fn release_keys(&mut self) {
        if let Some(c) = &self.client {
            for (sc, ext) in self.pressed.drain() {
                c.handle.send_key(sc, ext, false);
            }
        }
    }

    /// Input from the viewer (already past hotkeys and overlay) to the session.
    fn send_input(&mut self, input: Input) {
        let Some(c) = &self.client else { return };
        match input {
            Input::Key { code, down } => {
                if let Some((sc, ext)) = keymap::scancode(code) {
                    c.handle.send_key(sc, ext, down);
                    if down {
                        self.pressed.insert((sc, ext));
                    } else {
                        self.pressed.remove(&(sc, ext));
                    }
                }
            }
            Input::Move { x, y } => {
                self.last_move = Some((x, y));
                c.handle.send_mouse(mouse::MOVE, x, y);
            }
            Input::Button { button, down, x, y } => {
                let flags = match button {
                    MouseButton::Left => Some(mouse::BUTTON1),
                    MouseButton::Right => Some(mouse::BUTTON2),
                    MouseButton::Middle => Some(mouse::BUTTON3),
                    MouseButton::Back => Some(mouse::EXTENDED | mouse::XBUTTON1),
                    MouseButton::Forward => Some(mouse::EXTENDED | mouse::XBUTTON2),
                    MouseButton::Other(_) => None,
                };
                if let Some(f) = flags {
                    c.handle
                        .send_mouse(f | if down { mouse::DOWN } else { 0 }, x, y);
                }
            }
            Input::Wheel { dx, dy, x, y } => {
                if dy != 0 {
                    c.handle.send_mouse(mouse::wheel(dy, false), x, y);
                }
                if dx != 0 {
                    c.handle.send_mouse(mouse::wheel(dx, true), x, y);
                }
            }
            Input::ReleaseAll => self.release_keys(),
        }
    }

    fn session_event(&mut self, el: &ActiveEventLoop, ev: Event) -> Result<()> {
        // Clipboard events go to the clipboard bridge only.
        match ev {
            Event::ClipReady => Ok(()),
            Event::ClipRemoteFormats(f) => {
                if let Some(c) = &self.clipboard {
                    c.remote_formats(&f);
                }
                Ok(())
            }
            Event::ClipDataRequest(format) => {
                match &self.clipboard {
                    Some(c) => c.data_request(format),
                    None => {
                        if let Some(s) = &self.session {
                            s.handle().clip_respond(None);
                        }
                    }
                }
                Ok(())
            }
            Event::ClipData { ok, data } => {
                if let Some(c) = &self.clipboard {
                    c.on_data(ok, data);
                }
                Ok(())
            }
            Event::ClipFileRequest {
                stream,
                index,
                flags,
                position,
                size,
            } => {
                match &self.clipboard {
                    Some(c) => c.file_request(stream, index, flags, position, size),
                    None => {
                        if let Some(s) = &self.session {
                            s.handle().clip_file_respond(stream, None);
                        }
                    }
                }
                Ok(())
            }
            Event::ClipFileData { stream, ok, data } => {
                if let Some(c) = &self.clipboard {
                    c.on_file_data(stream, ok, data);
                }
                Ok(())
            }
            ev => self.session_event_gfx(el, ev),
        }
    }

    fn session_event_gfx(&mut self, el: &ActiveEventLoop, ev: Event) -> Result<()> {
        let (Some(client), Some(viewer)) = (self.client.as_mut(), self.viewer.as_mut()) else {
            return Ok(());
        };
        match &ev {
            Event::PointerSet {
                index,
                width,
                height,
                hot_x,
                hot_y,
                bgra,
            } => {
                let mut rgba = bgra.clone();
                for px in rgba.as_chunks_mut::<4>().0 {
                    px.swap(0, 2);
                }
                viewer.set_cursor(
                    el,
                    CursorUpdate::Set {
                        index: *index,
                        shape: CursorShape {
                            width: *width,
                            height: *height,
                            hot_x: *hot_x,
                            hot_y: *hot_y,
                            rgba,
                        },
                    },
                )?;
            }
            Event::PointerCached { index } => {
                viewer.set_cursor(el, CursorUpdate::Cached { index: *index })?;
            }
            Event::PointerSystem { hidden } => viewer.set_cursor(
                el,
                if *hidden {
                    CursorUpdate::Hidden
                } else {
                    CursorUpdate::Default
                },
            )?,
            _ => {}
        }
        let layout = match &ev {
            Event::ResetGraphics { monitors, .. } => Some(monitors.clone()),
            _ => None,
        };
        client.handle(ev)?;
        if let Some(monitors) = layout
            && !monitors.is_empty()
        {
            let outputs = match &self.local {
                Some(l) => l.session_outputs(el, &monitors),
                None => DesktopLayout {
                    monitors: monitors.clone(),
                }
                .outputs()
                .into_iter()
                .zip(&monitors)
                .map(|(rect, m)| SessionOutput {
                    name: "Session".into(),
                    rect,
                    primary: m.primary,
                    monitor: None,
                })
                .collect(),
            };
            for o in &outputs {
                println!(
                    "Session-Ausgang {}x{} bei {},{} -> Monitor {}",
                    o.rect.width(),
                    o.rect.height(),
                    o.rect.left,
                    o.rect.top,
                    o.name
                );
            }
            viewer.set_outputs(el, &mut client.core, outputs)?;
            if !viewer.is_open() {
                viewer.open(el, &mut client.core)?;
            }
        }
        Ok(())
    }

    fn window_event(
        &mut self,
        el: &ActiveEventLoop,
        id: WindowId,
        event: WindowEvent,
    ) -> Result<()> {
        let (Some(client), Some(viewer)) = (self.client.as_mut(), self.viewer.as_mut()) else {
            return Ok(());
        };
        let actions = viewer.handle(el, &mut client.core, id, event)?;
        for action in actions {
            match action {
                Action::Quit => el.exit(),
                Action::CancelTransfer => {
                    if let Some(c) = &self.clipboard {
                        c.cancel();
                    }
                }
                Action::Input(i) => self.send_input(i),
                // The launcher follows this log line and offers to save the favorite.
                Action::SaveFavorite => println!("{SAVE_FAVORITE_LINE}"),
                Action::Menu(id) => {
                    if let (Some(m), Some(c), Some(v)) =
                        (&mut self.menu, &self.client, &mut self.viewer)
                    {
                        m.handle(id, &c.handle, self.clipboard.as_ref());
                        let on = self.clipboard.as_ref().is_some_and(|c| c.enabled());
                        v.set_menu(m.items(on));
                    }
                }
                Action::Pause | Action::Clicked { .. } => {}
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        self.release_keys();
        if let Some(v) = &self.viewer {
            settings::remember(v.bar_pinned(), v.keep_active());
        }
        if let (Some(c), Some(v)) = (self.client.as_mut(), self.viewer.as_mut()) {
            v.shutdown(&mut c.core);
        }
        self.viewer = None;
        if let Some(c) = &mut self.client {
            if let Some(dir) = &self.args.dump_dir {
                let _ = c.core.flush().and_then(|()| c.dump(dir));
            }
            c.summary();
        }
        self.client = None;
        if let Some(s) = self.session.take() {
            s.stop();
        }
    }
}

impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.client.is_none()
            && let Err(e) = self.init(el)
        {
            self.error = Some(e);
            el.exit();
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, ev: Event) {
        if let Err(e) = self.session_event(el, ev) {
            self.error = Some(e);
            el.exit();
        }
        if self
            .client
            .as_ref()
            .is_some_and(|c| c.disconnected.is_some())
        {
            el.exit();
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Err(e) = self.window_event(el, id, event) {
            self.error = Some(e);
            el.exit();
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // SIGTERM/SIGINT (launcher "Trennen"): leave the loop, shutdown() disconnects cleanly.
        if STOP_REQUESTED.load(Ordering::Relaxed) {
            el.exit();
            return;
        }
        let (Some(c), Some(v)) = (self.client.as_mut(), self.viewer.as_mut()) else {
            return;
        };
        // Fallback: no layout from the server 5 s after connecting -> open with ours.
        if !v.is_open()
            && c.connected
            && self.started.elapsed() > Duration::from_secs(5)
            && let Err(e) = v.open(el, &mut c.core)
        {
            self.error = Some(e);
            el.exit();
            return;
        }
        if let Some(clip) = &self.clipboard {
            clip.set_minimized(v.minimized());
            v.set_transfer(clip.progress());
        }
        // RDPC_TEST_MENU_ACTION=<id> (tests): clicks a menu entry 3 s after connecting.
        if c.connected
            && self.started.elapsed() > Duration::from_secs(3)
            && let Some(m) = &mut self.menu
            && let Some(id) = std::env::var("RDPC_TEST_MENU_ACTION")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
            && !TEST_ACTION_DONE.swap(true, Ordering::Relaxed)
        {
            m.handle(id, &c.handle, self.clipboard.as_ref());
        }
        if let Some(m) = &mut self.menu
            && m.tick(&c.handle)
        {
            v.set_menu(m.items(self.clipboard.as_ref().is_some_and(|c| c.enabled())));
        }
        // "Keep active": a 1-pixel move and back every 50 s (counts as input for Windows'
        // idle detection, e.g. Teams presence), also while the window is in the background.
        if v.keep_active() && self.keep_at.elapsed() >= Duration::from_secs(50) {
            self.keep_at = Instant::now();
            let (x, y) = self.last_move.unwrap_or((c.desktop.0 / 2, c.desktop.1 / 2));
            let nx = if x > 0 { x - 1 } else { x + 1 };
            c.handle.send_mouse(mouse::MOVE, nx, y);
            c.handle.send_mouse(mouse::MOVE, x, y);
        }
        let r = v.tick(&mut c.core).and_then(|()| {
            if c.frame_ended || v.needs_present() {
                // Minimised views draw nothing, but frames are still acknowledged.
                let was_dirty = v.needs_present();
                c.core.present()?;
                c.frame_ended = false;
                v.presented();
                if was_dirty {
                    v.debug_views(&c.core);
                }
            }
            Ok(())
        });
        if let Err(e) = r {
            self.error = Some(e);
            el.exit();
            return;
        }
        if self.args.input_test && c.first_frame && !self.input_started {
            self.input_started = true;
            let (h, d) = (c.handle.clone(), c.desktop);
            std::thread::spawn(move || input_test(h, d));
        }
        if self.stats_at.elapsed() >= Duration::from_secs(5) {
            let cpu = cpu_seconds();
            let line = c.stats_line(self.stats_at.elapsed().as_secs_f64(), cpu - self.cpu_at);
            println!(
                "{line}, Modus {:?}{}, Grab {}",
                v.mode(),
                if v.minimized() { " (minimiert)" } else { "" },
                if v.grab_on() { "an" } else { "aus" }
            );
            if std::env::var_os("RDP_VIEWER_DEBUG").is_some() {
                println!("[viewer] Zeigerbewegungen: {}", v.pointer_stats());
                println!("[viewer] Ausgaben: {}", v.present_stats(&c.core));
            }
            self.stats_at = Instant::now();
            self.cpu_at = cpu;
        }
        // Wake up regularly for the bar's auto-hide and the duration limit.
        let mut next = Instant::now() + Duration::from_millis(250);
        if let Some(d) = self.args.duration {
            let end = self.started + Duration::from_secs(d);
            if Instant::now() >= end {
                el.exit();
                return;
            }
            next = next.min(end);
        }
        el.set_control_flow(ControlFlow::WaitUntil(next));
    }

    fn exiting(&mut self, _el: &ActiveEventLoop) {
        self.shutdown();
    }
}

fn run_windowed(args: &Args) -> Result<()> {
    let mut cfg = config(args)?;
    let local = match LocalLayout::query(args) {
        Ok(l) => {
            let (w, h) = rdp_viewer::layout::desktop_size(&l.sent);
            cfg.width = w;
            cfg.height = h;
            cfg.monitors = l.sent.clone();
            Some(l)
        }
        Err(e) => {
            eprintln!("Lokale Monitore nicht ermittelbar ({e:#}), nutze --size");
            None
        }
    };
    let el = EventLoop::<Event>::with_user_event().build()?;
    let mut app = App {
        args: args.clone(),
        cfg,
        local,
        proxy: el.create_proxy(),
        session: None,
        client: None,
        clipboard: None,
        menu: None,
        last_move: None,
        keep_at: Instant::now(),
        viewer: None,
        pressed: HashSet::new(),
        started: Instant::now(),
        input_started: false,
        error: None,
        stats_at: Instant::now(),
        cpu_at: cpu_seconds(),
    };
    el.run_app(&mut app)?;
    app.shutdown();
    app.error.map_or(Ok(()), Err)
}

mod settings {
    //! Remembered session toggles (bar pinned, keep active): `~/.local/state/rmc/session.toml`.
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
    #[serde(default)]
    pub struct SessionState {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub bar_pinned: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub keep_active: Option<bool>,
    }

    fn path() -> std::path::PathBuf {
        crate::paths::state_dir().join("session.toml")
    }

    pub fn load() -> SessionState {
        std::fs::read_to_string(path())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(s: &SessionState) -> anyhow::Result<()> {
        let p = path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = p.with_extension("toml.tmp");
        std::fs::write(&tmp, toml::to_string_pretty(s)?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// Remembered pin state of the connection bar.
    pub fn bar_pinned() -> Option<bool> {
        load().bar_pinned
    }

    /// Remembered "keep active" toggle.
    pub fn keep_active() -> Option<bool> {
        load().keep_active
    }

    pub fn remember(bar_pinned: bool, keep_active: bool) {
        let s = SessionState {
            bar_pinned: Some(bar_pinned),
            keep_active: Some(keep_active),
        };
        if let Err(e) = save(&s) {
            eprintln!("Sitzungszustand nicht gespeichert: {e:#}");
        }
    }
}

/// RDPC_TEST_MENU_ACTION ran.
static TEST_ACTION_DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set by SIGTERM/SIGINT; the event loop ends and the session disconnects cleanly.
static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_stop_signal(_: libc::c_int) {
    STOP_REQUESTED.store(true, Ordering::Relaxed);
}

/// "~/x" -> "$HOME/x".
fn expand_home(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(h)) => format!("{h}/{rest}"),
        _ if p == "~" => std::env::var("HOME").unwrap_or_else(|_| p.into()),
        _ => p.to_string(),
    }
}

/// A FreeRDP helper program: next to the client (AppImage, installed layout), in
/// ../libexec, or in the FreeRDP prefix the client was built against.
fn freerdp_helper(name: &str) -> String {
    let prefix = env!("RDPC_FREERDP_PREFIX");
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf));
    let mut candidates = Vec::new();
    if let Some(d) = &exe_dir {
        candidates.push(d.join(name));
        candidates.push(d.join("../libexec").join(name));
    }
    candidates.push(Path::new(prefix).join("libexec").join(name));
    candidates.push(Path::new(prefix).join("bin").join(name));
    candidates
        .iter()
        .find(|p| p.exists())
        .unwrap_or(&candidates[candidates.len() - 2])
        .display()
        .to_string()
}

/// WebKitGTK 4.1 can be loaded (the own login window needs it; the AppImage does not bundle
/// it, the desktop provides it).
fn webkit_available() -> bool {
    let name = c"libwebkit2gtk-4.1.so.0";
    // SAFETY: dlopen/dlclose with a valid C string; the handle is only checked and released.
    unsafe {
        let h = libc::dlopen(name.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL);
        if h.is_null() {
            return false;
        }
        libc::dlclose(h);
    }
    true
}

/// The own login helper: next to the client binary (installed layout), else where the
/// build put it.
fn login_helper_path() -> String {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("rmc-login")))
        .filter(|p| p.exists());
    match (beside, option_env!("RDPC_LOGIN_HELPER")) {
        (Some(p), _) => p.display().to_string(),
        (None, Some(p)) => p.to_string(),
        (None, None) => "rmc-login".to_string(),
    }
}

/// WebKit profile of the login helper (cookies, stored sign-in), as in rmc-login.c.
fn login_profile() -> PathBuf {
    paths::login_profile()
}

fn main() -> Result<()> {
    // No arguments: the launcher. `connect <favorite>`: a session with the favorite's options.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("install-desktop") {
        // Menu entry for this binary (tools/install-local.sh; AppImages do it at start). In an
        // AppImage the binary lives in a temporary mount: the entry must name the AppImage.
        let exe = match std::env::var_os("APPIMAGE") {
            Some(img) => std::path::PathBuf::from(img),
            None => std::env::current_exe()?,
        };
        let f = desktop::install(&exe)?;
        println!(
            "Startmenü-Eintrag: {} (Exec={})",
            f.display(),
            exe.display()
        );
        return Ok(());
    }
    if argv.len() == 1 {
        desktop::ensure_for_appimage();
        return launcher::run();
    }
    let mut args = if argv.get(1).map(String::as_str) == Some("connect") {
        let name = argv.get(2).context("usage: rmc connect <favorite>")?;
        let settings = profiles::Settings::load()?;
        let fav = settings
            .find(name)
            .with_context(|| format!("Favorit „{name}“ nicht gefunden"))?;
        let mut full = vec![argv[0].clone()];
        full.extend(fav.session_args(&settings.defaults));
        full.extend(argv.iter().skip(3).cloned());
        Args::parse_from(full)
    } else {
        Args::parse()
    };
    // RDPC_TEST_HEADLESS (launcher tests): sessions without windows.
    if std::env::var_os("RDPC_TEST_HEADLESS").is_some() {
        args.headless = true;
        args.size = "1600x896".into();
    }
    if args.probe_gpu {
        return probe_gpu(&args);
    }
    if args.logout {
        let dir = login_profile();
        if dir.exists() {
            std::fs::remove_dir_all(&dir).with_context(|| format!("{}", dir.display()))?;
            println!("Login-Profil gelöscht ({})", dir.display());
        }
        // Refresh tokens and passwords of all accounts/hosts (csrc/secret.c).
        rdp_session::secret::clear_all();
        println!("Gespeicherte Anmeldungen und Passwörter gelöscht");
        return Ok(());
    }
    if args.aad && (args.aad_helper == "webview" || args.aad_helper == "login") {
        // WebKitGTK workarounds for the WebView login helper (inherited by the helper process).
        // Set before any thread starts.
        for (k, v) in [
            ("GDK_BACKEND", "x11"),
            ("WEBKIT_DISABLE_DMABUF_RENDERER", "1"),
            ("WEBKIT_DISABLE_COMPOSITING_MODE", "1"),
        ] {
            // SAFETY: single-threaded at this point (no session, no event loop yet).
            unsafe { std::env::set_var(k, v) };
        }
    }
    // SAFETY: the handler only stores into an atomic.
    unsafe {
        libc::signal(
            libc::SIGTERM,
            on_stop_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            on_stop_signal as *const () as libc::sighandler_t,
        );
    }
    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    let r = if args.headless {
        run_headless(&args)
    } else {
        run_windowed(&args)
    };
    println!(
        "Laufzeit {:.1} s, CPU {:.1} % eines Kerns im Mittel",
        t0.elapsed().as_secs_f64(),
        (cpu_seconds() - cpu0) / t0.elapsed().as_secs_f64() * 100.0
    );
    r
}
