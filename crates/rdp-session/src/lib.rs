//! RDP session on top of libfreerdp 3, through the thin C layer in `csrc/shim.c`.
//!
//! The session runs FreeRDP's event loop on its own thread and delivers the graphics
//! pipeline (MS-RDPEGFX) as owned [`Event`]s; all data is copied out of FreeRDP's buffers.
//! [`SessionHandle`] sends input and frame acknowledgements from any thread; the C layer
//! queues them and sends them from the session thread.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail};
use rdpgfx_model::outputs::Monitor;
use rdpgfx_model::{Color, PixelFormat, Rect};

mod ffi {
    use std::ffi::{c_char, c_void};

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct RdpcRect {
        pub left: u16,
        pub top: u16,
        pub right: u16,
        pub bottom: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct RdpcPoint {
        pub x: u16,
        pub y: u16,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct RdpcMonitor {
        pub left: i32,
        pub top: i32,
        pub right: i32,
        pub bottom: i32,
        pub primary: u32,
    }

    #[repr(C)]
    pub struct RdpcConfig {
        pub host: *const c_char,
        pub port: u16,
        pub username: *const c_char,
        pub domain: *const c_char,
        pub password: *const c_char,
        pub auth: u32,
        pub width: u32,
        pub height: u32,
        pub monitors: *const RdpcMonitor,
        pub monitor_count: u32,
        pub avc444: u32,
        pub ignore_certificate: u32,
        pub log_level: *const c_char,
        pub aad_helper: *const c_char,
        pub cert_tofu: u32,
        pub sound: *const c_char,
        pub microphone: *const c_char,
        pub camera: *const c_char,
        pub auto_reconnect: u32,
        pub clipboard: u32,
        pub drives: *const c_char,
        pub printers: *const c_char,
        pub cert_prompt: u32,
    }

    #[repr(C)]
    pub struct RdpcEvent {
        pub kind: u32,
        pub surface: u32,
        pub surface2: u32,
        pub a: u32,
        pub b: u32,
        pub c: u32,
        pub d: u32,
        pub rect: RdpcRect,
        pub rects: *const RdpcRect,
        pub rect_count: u32,
        pub rects2: *const RdpcRect,
        pub rect_count2: u32,
        pub points: *const RdpcPoint,
        pub point_count: u32,
        pub data: *const u8,
        pub length: u32,
        pub data2: *const u8,
        pub length2: u32,
        pub monitors: *const RdpcMonitor,
        pub monitor_count: u32,
        pub message: *const c_char,
    }

    pub type EventFn = unsafe extern "C" fn(user: *mut c_void, ev: *const RdpcEvent);

    #[repr(C)]
    pub struct RdpcSession {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct RdpcAttr {
        pub key: *const c_char,
        pub value: *const c_char,
    }

    unsafe extern "C" {
        pub fn rdpc_session_new(
            cfg: *const RdpcConfig,
            f: EventFn,
            user: *mut c_void,
        ) -> *mut RdpcSession;
        pub fn rdpc_session_run(s: *mut RdpcSession) -> i32;
        pub fn rdpc_session_stop(s: *mut RdpcSession);
        pub fn rdpc_session_free(s: *mut RdpcSession);
        pub fn rdpc_send_key(s: *mut RdpcSession, scancode: u32, extended: u32, down: u32) -> i32;
        pub fn rdpc_send_mouse(s: *mut RdpcSession, flags: u32, x: u32, y: u32) -> i32;
        pub fn rdpc_send_sync(s: *mut RdpcSession, flags: u32) -> i32;
        pub fn rdpc_send_frame_ack(
            s: *mut RdpcSession,
            frame_id: u32,
            total_decoded: u32,
            queue_depth: u32,
        ) -> i32;
        pub fn rdpc_add_device(s: *mut RdpcSession, kind: u32, spec: *const c_char) -> i32;
        pub fn rdpc_secret_lookup(attrs: *const RdpcAttr, n: usize) -> *mut c_char;
        pub fn rdpc_secret_store(
            attrs: *const RdpcAttr,
            n: usize,
            label: *const c_char,
            value: *const c_char,
        ) -> i32;
        pub fn rdpc_secret_clear(attrs: *const RdpcAttr, n: usize);
        pub fn rdpc_secret_clear_all();
        pub fn rdpc_secret_free(s: *mut c_char);
        pub fn rdpc_clip_set_formats(s: *mut RdpcSession, list: *const u8, len: usize) -> i32;
        pub fn rdpc_clip_request(s: *mut RdpcSession, format_id: u32) -> i32;
        pub fn rdpc_clip_respond(s: *mut RdpcSession, ok: u32, data: *const u8, len: usize) -> i32;
        pub fn rdpc_clip_file_request(
            s: *mut RdpcSession,
            stream: u32,
            index: u32,
            flags: u32,
            position: u64,
            size: u32,
        ) -> i32;
        pub fn rdpc_clip_file_respond(
            s: *mut RdpcSession,
            stream: u32,
            ok: u32,
            data: *const u8,
            len: usize,
        ) -> i32;
    }

    pub const EV_CONNECTED: u32 = 1;
    pub const EV_DISCONNECTED: u32 = 2;
    pub const EV_ERROR: u32 = 3;
    pub const EV_CAPS_CONFIRM: u32 = 4;
    pub const EV_RESET_GRAPHICS: u32 = 5;
    pub const EV_CREATE_SURFACE: u32 = 6;
    pub const EV_DELETE_SURFACE: u32 = 7;
    pub const EV_MAP_SURFACE: u32 = 8;
    pub const EV_START_FRAME: u32 = 9;
    pub const EV_END_FRAME: u32 = 10;
    pub const EV_SOLID_FILL: u32 = 11;
    pub const EV_SURFACE_TO_SURFACE: u32 = 12;
    pub const EV_SURFACE_TO_CACHE: u32 = 13;
    pub const EV_CACHE_TO_SURFACE: u32 = 14;
    pub const EV_EVICT_CACHE: u32 = 15;
    pub const EV_AVC420: u32 = 16;
    pub const EV_AVC444: u32 = 17;
    pub const EV_PIXELS: u32 = 18;
    pub const EV_UNSUPPORTED_CODEC: u32 = 19;
    pub const EV_POINTER_SET: u32 = 20;
    pub const EV_POINTER_CACHED: u32 = 21;
    pub const EV_POINTER_SYSTEM: u32 = 22;
    pub const EV_INFO: u32 = 23;
    pub const EV_GFX_CLOSED: u32 = 24;
    pub const EV_CLIP_READY: u32 = 25;
    pub const EV_CLIP_REMOTE_FORMATS: u32 = 26;
    pub const EV_CLIP_DATA_REQUEST: u32 = 27;
    pub const EV_CLIP_DATA: u32 = 28;
    pub const EV_CLIP_FILE_REQUEST: u32 = 29;
    pub const EV_CLIP_FILE_DATA: u32 = 30;
}

/// How the client authenticates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    /// Username/password (NLA); used against the local test server.
    Password,
    /// Entra ID (RDS AAD): access token from the out-of-process login helper
    /// (`Config::aad_helper`, system browser or WebView), as FreeRDP's `/sec:aad`.
    Aad,
}

/// Connection settings.
#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub domain: Option<String>,
    pub password: Option<String>,
    pub auth: Auth,
    pub width: u32,
    pub height: u32,
    /// Monitor layout sent to the server (virtual desktop coordinates); empty = one monitor.
    pub monitors: Vec<Monitor>,
    pub avc444: bool,
    /// Accept any certificate. Only for local test servers.
    pub ignore_certificate: bool,
    pub log_level: Option<String>,
    /// Path of FreeRDP's AAD login helper (`freerdp-xdg-aad-helper` or `-webview-`).
    pub aad_helper: Option<String>,
    /// Unknown certificate: accept and store on first use (fingerprint reported as `Info`);
    /// a changed certificate is always rejected.
    pub cert_tofu: bool,
    /// Channel arguments as for FreeRDP's `/sound:`, `/microphone:`, `/dvc:rdpecam,`
    /// (e.g. `"sys:pulse"`; empty string = defaults); `None` = channel off.
    pub sound: Option<String>,
    pub microphone: Option<String>,
    pub camera: Option<String>,
    /// Reconnect automatically after a network drop (up to 5 tries).
    pub auto_reconnect: bool,
    /// Clipboard channel (cliprdr).
    pub clipboard: bool,
    /// Redirected folders (name shown in Windows, local path).
    pub drives: Vec<(String, String)>,
    /// Redirected CUPS printers (names as in `lpstat -e`).
    pub printers: Vec<String>,
    /// Unknown or changed certificate: ask on stdout/stdin ("RDPC-CERT" line, answer
    /// "yes"/"no"), used by the launcher. Entra renewals stay automatic.
    pub cert_prompt: bool,
}

/// AVC view: region rectangles (surface coordinates) and the H.264 access unit.
#[derive(Clone, Debug)]
pub struct AvcData {
    pub rects: Vec<Rect>,
    pub data: Vec<u8>,
}

/// Session events. Graphics events mirror MS-RDPEGFX.
#[derive(Clone, Debug)]
pub enum Event {
    Connected {
        width: u32,
        height: u32,
    },
    Disconnected {
        code: u32,
        message: String,
    },
    Error(String),
    /// Notable information (certificate accepted on first use, ...).
    Info(String),
    /// The graphics channel closed (e.g. before an auto-reconnect): all surfaces and cache
    /// entries of the server are gone; a new channel starts from scratch.
    GfxClosed,
    /// Clipboard channel ready; the local formats were (re)announced.
    ClipReady,
    /// The server clipboard changed: its formats (id, registered name or empty).
    ClipRemoteFormats(Vec<(u32, String)>),
    /// The server pastes: data of this format is wanted (answer with `clip_respond`).
    ClipDataRequest(u32),
    /// Answer to `clip_request`.
    ClipData {
        ok: bool,
        data: Vec<u8>,
    },
    /// The server reads a file of our file list (flags: 1 = size, 2 = range).
    ClipFileRequest {
        stream: u32,
        index: u32,
        flags: u32,
        position: u64,
        size: u32,
    },
    /// Answer to `clip_file_request`.
    ClipFileData {
        stream: u32,
        ok: bool,
        data: Vec<u8>,
    },
    CapsConfirm {
        version: u32,
        flags: u32,
    },
    ResetGraphics {
        width: u32,
        height: u32,
        monitors: Vec<Monitor>,
    },
    CreateSurface {
        id: u16,
        width: u32,
        height: u32,
        format: PixelFormat,
    },
    DeleteSurface {
        id: u16,
    },
    MapSurface {
        id: u16,
        x: u32,
        y: u32,
    },
    StartFrame {
        id: u32,
    },
    EndFrame {
        id: u32,
    },
    SolidFill {
        id: u16,
        color: Color,
        rects: Vec<Rect>,
    },
    SurfaceToSurface {
        src: u16,
        rect: Rect,
        dst: u16,
        points: Vec<(u32, u32)>,
    },
    SurfaceToCache {
        id: u16,
        rect: Rect,
        slot: u16,
    },
    CacheToSurface {
        slot: u16,
        id: u16,
        points: Vec<(u32, u32)>,
    },
    EvictCache {
        slot: u16,
    },
    Avc420 {
        id: u16,
        dest: Rect,
        main: AvcData,
    },
    /// `v2`: AVC444v2 layout. `main`/`aux` are `None` when the LC field leaves them out.
    Avc444 {
        id: u16,
        v2: bool,
        dest: Rect,
        main: Option<AvcData>,
        aux: Option<AvcData>,
    },
    /// CPU-decoded pixels (uncompressed, planar, ClearCodec): tight BGRA rows.
    Pixels {
        id: u16,
        rect: Rect,
        bgra: Vec<u8>,
    },
    UnsupportedCodec {
        id: u16,
        codec: u32,
        rect: Rect,
    },
    /// New pointer shape (tight BGRA rows), cached by the server under `index`.
    PointerSet {
        index: u16,
        width: u16,
        height: u16,
        hot_x: u16,
        hot_y: u16,
        bgra: Vec<u8>,
    },
    PointerCached {
        index: u16,
    },
    /// System pointer: hidden (`true`) or the default arrow.
    PointerSystem {
        hidden: bool,
    },
}

fn rect(r: &ffi::RdpcRect) -> Rect {
    Rect::new(
        u32::from(r.left),
        u32::from(r.top),
        u32::from(r.right),
        u32::from(r.bottom),
    )
}

/// # Safety
/// `ptr` must point to `n` valid elements or be null.
unsafe fn slice<'a, T>(ptr: *const T, n: u32) -> &'a [T] {
    if ptr.is_null() || n == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(ptr, n as usize) }
    }
}

/// Copies an event out of the C layer's buffers.
///
/// # Safety
/// `ev` must be a valid event as described in `shim.h` (pointers valid for the call).
unsafe fn convert(ev: &ffi::RdpcEvent) -> Option<Event> {
    let id = ev.surface as u16;
    let rects = |p, n| unsafe { slice(p, n) }.iter().map(rect).collect::<Vec<_>>();
    let points = || {
        unsafe { slice(ev.points, ev.point_count) }
            .iter()
            .map(|p| (u32::from(p.x), u32::from(p.y)))
            .collect::<Vec<_>>()
    };
    let bytes = |p, n| unsafe { slice(p, n) }.to_vec();
    let message = || {
        if ev.message.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(ev.message) }
                .to_string_lossy()
                .into_owned()
        }
    };
    Some(match ev.kind {
        ffi::EV_CONNECTED => Event::Connected {
            width: ev.a,
            height: ev.b,
        },
        ffi::EV_DISCONNECTED => Event::Disconnected {
            code: ev.a,
            message: message(),
        },
        ffi::EV_ERROR => Event::Error(message()),
        ffi::EV_INFO => Event::Info(message()),
        ffi::EV_GFX_CLOSED => Event::GfxClosed,
        ffi::EV_CLIP_READY => Event::ClipReady,
        ffi::EV_CLIP_REMOTE_FORMATS => {
            let text = String::from_utf8_lossy(unsafe { slice(ev.data, ev.length) }).into_owned();
            Event::ClipRemoteFormats(
                text.lines()
                    .filter_map(|l| {
                        let (id, name) = l.split_once('\t').unwrap_or((l, ""));
                        Some((id.parse().ok()?, name.to_string()))
                    })
                    .collect(),
            )
        }
        ffi::EV_CLIP_DATA_REQUEST => Event::ClipDataRequest(ev.a),
        ffi::EV_CLIP_DATA => Event::ClipData {
            ok: ev.a != 0,
            data: unsafe { slice(ev.data, ev.length) }.to_vec(),
        },
        ffi::EV_CLIP_FILE_REQUEST => Event::ClipFileRequest {
            stream: ev.a,
            index: ev.b,
            flags: ev.c,
            position: u64::from(ev.surface) | (u64::from(ev.surface2) << 32),
            size: ev.d,
        },
        ffi::EV_CLIP_FILE_DATA => Event::ClipFileData {
            stream: ev.a,
            ok: ev.b != 0,
            data: unsafe { slice(ev.data, ev.length) }.to_vec(),
        },
        ffi::EV_CAPS_CONFIRM => Event::CapsConfirm {
            version: ev.a,
            flags: ev.b,
        },
        ffi::EV_RESET_GRAPHICS => Event::ResetGraphics {
            width: ev.a,
            height: ev.b,
            monitors: unsafe { slice(ev.monitors, ev.monitor_count) }
                .iter()
                .map(|m| Monitor {
                    left: m.left,
                    top: m.top,
                    right: m.right,
                    bottom: m.bottom,
                    primary: m.primary != 0,
                })
                .collect(),
        },
        ffi::EV_CREATE_SURFACE => Event::CreateSurface {
            id,
            width: ev.a,
            height: ev.b,
            format: if ev.c == 0x21 {
                PixelFormat::Argb8888
            } else {
                PixelFormat::Xrgb8888
            },
        },
        ffi::EV_DELETE_SURFACE => Event::DeleteSurface { id },
        ffi::EV_MAP_SURFACE => Event::MapSurface {
            id,
            x: ev.a,
            y: ev.b,
        },
        ffi::EV_START_FRAME => Event::StartFrame { id: ev.a },
        ffi::EV_END_FRAME => Event::EndFrame { id: ev.a },
        ffi::EV_SOLID_FILL => Event::SolidFill {
            id,
            color: Color {
                b: ev.a as u8,
                g: (ev.a >> 8) as u8,
                r: (ev.a >> 16) as u8,
            },
            rects: rects(ev.rects, ev.rect_count),
        },
        ffi::EV_SURFACE_TO_SURFACE => Event::SurfaceToSurface {
            src: id,
            rect: rect(&ev.rect),
            dst: ev.surface2 as u16,
            points: points(),
        },
        ffi::EV_SURFACE_TO_CACHE => Event::SurfaceToCache {
            id,
            rect: rect(&ev.rect),
            slot: ev.a as u16,
        },
        ffi::EV_CACHE_TO_SURFACE => Event::CacheToSurface {
            slot: ev.a as u16,
            id,
            points: points(),
        },
        ffi::EV_EVICT_CACHE => Event::EvictCache { slot: ev.a as u16 },
        ffi::EV_AVC420 => Event::Avc420 {
            id,
            dest: rect(&ev.rect),
            main: AvcData {
                rects: rects(ev.rects, ev.rect_count),
                data: bytes(ev.data, ev.length),
            },
        },
        ffi::EV_AVC444 => Event::Avc444 {
            id,
            v2: ev.a == 2,
            dest: rect(&ev.rect),
            main: (!ev.data.is_null()).then(|| AvcData {
                rects: rects(ev.rects, ev.rect_count),
                data: bytes(ev.data, ev.length),
            }),
            aux: (!ev.data2.is_null()).then(|| AvcData {
                rects: rects(ev.rects2, ev.rect_count2),
                data: bytes(ev.data2, ev.length2),
            }),
        },
        ffi::EV_PIXELS => Event::Pixels {
            id,
            rect: rect(&ev.rect),
            bgra: bytes(ev.data, ev.length),
        },
        ffi::EV_UNSUPPORTED_CODEC => Event::UnsupportedCodec {
            id,
            codec: ev.a,
            rect: rect(&ev.rect),
        },
        ffi::EV_POINTER_SET => Event::PointerSet {
            index: ev.a as u16,
            width: ev.b as u16,
            height: ev.c as u16,
            hot_x: (ev.d & 0xFFFF) as u16,
            hot_y: (ev.d >> 16) as u16,
            bgra: bytes(ev.data, ev.length),
        },
        ffi::EV_POINTER_CACHED => Event::PointerCached { index: ev.a as u16 },
        ffi::EV_POINTER_SYSTEM => Event::PointerSystem { hidden: ev.a == 0 },
        _ => return None,
    })
}

type Sink = Mutex<Box<dyn FnMut(Event) + Send>>;

unsafe extern "C" fn trampoline(user: *mut c_void, ev: *const ffi::RdpcEvent) {
    // SAFETY: `user` is the `Sink` owned by `Inner`, alive while the session runs; `ev` is
    // valid for the duration of the call (shim.h).
    let sink = unsafe { &*(user as *const Sink) };
    if let Some(event) = unsafe { convert(&*ev) } {
        // A panic must not unwind into C.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Ok(mut f) = sink.lock() {
                f(event);
            }
        }));
    }
}

struct Inner {
    raw: *mut ffi::RdpcSession,
    sink: *mut Sink,
}

// SAFETY: the C functions used through `Inner` from other threads (send_*, stop) are
// documented thread-safe in shim.h; `run` is only called on the session thread.
unsafe impl Send for Inner {}
unsafe impl Sync for Inner {}

impl Drop for Inner {
    fn drop(&mut self) {
        unsafe {
            ffi::rdpc_session_free(self.raw);
            drop(Box::from_raw(self.sink));
        }
    }
}

/// Mouse button / wheel flags (MS-RDPBCGR 2.2.8.1.1.3.1.1.3).
pub mod mouse {
    pub const MOVE: u32 = 0x0800;
    pub const DOWN: u32 = 0x8000;
    pub const BUTTON1: u32 = 0x1000; // left
    pub const BUTTON2: u32 = 0x2000; // right
    pub const BUTTON3: u32 = 0x4000; // middle
    pub const WHEEL: u32 = 0x0200;
    pub const HWHEEL: u32 = 0x0400;
    pub const WHEEL_NEGATIVE: u32 = 0x0100;
    /// Extended mouse event (buttons 4/5: XBUTTON1 0x0001, XBUTTON2 0x0002, DOWN 0x8000).
    pub const EXTENDED: u32 = 0x8000_0000;
    pub const XBUTTON1: u32 = 0x0001;
    pub const XBUTTON2: u32 = 0x0002;

    /// Wheel flags for `clicks` notches (positive = away from the user / right).
    pub fn wheel(clicks: i32, horizontal: bool) -> u32 {
        let units = (clicks.unsigned_abs() * 120).min(255);
        let mut f = if horizontal { HWHEEL } else { WHEEL } | units;
        // MS-RDPBCGR: negative rotation is sent as a 9-bit two's complement value.
        if clicks < 0 {
            f = (if horizontal { HWHEEL } else { WHEEL }) | WHEEL_NEGATIVE | ((256 - units) & 0xFF);
        }
        f
    }
}

/// Lock key flags of the synchronize event (MS-RDPBCGR 2.2.8.1.1.3.1.1.5).
pub mod sync {
    pub const SCROLL_LOCK: u32 = 0x01;
    pub const NUM_LOCK: u32 = 0x02;
    pub const CAPS_LOCK: u32 = 0x04;
}

/// Sends input and acknowledgements; cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct SessionHandle(Arc<Inner>);

impl SessionHandle {
    pub fn send_key(&self, scancode: u8, extended: bool, down: bool) {
        unsafe {
            ffi::rdpc_send_key(
                self.0.raw,
                u32::from(scancode),
                u32::from(extended),
                u32::from(down),
            )
        };
    }

    /// `flags` from [`mouse`]; coordinates on the session desktop.
    pub fn send_mouse(&self, flags: u32, x: u32, y: u32) {
        unsafe { ffi::rdpc_send_mouse(self.0.raw, flags, x.min(0xFFFF), y.min(0xFFFF)) };
    }

    /// Sets the server's lock keys (flags from [`sync`]).
    pub fn sync_locks(&self, flags: u32) {
        unsafe { ffi::rdpc_send_sync(self.0.raw, flags) };
    }

    /// RDPGFX_FRAME_ACKNOWLEDGE_PDU.
    pub fn frame_ack(&self, frame_id: u32, total_decoded: u32, queue_depth: u32) {
        unsafe { ffi::rdpc_send_frame_ack(self.0.raw, frame_id, total_decoded, queue_depth) };
    }

    /// Announces the local clipboard formats (id, registered name or empty for predefined).
    pub fn clip_set_formats(&self, formats: &[(u32, &str)]) {
        let list: String = formats
            .iter()
            .map(|(id, n)| format!("{id}\t{n}\n"))
            .collect();
        unsafe { ffi::rdpc_clip_set_formats(self.0.raw, list.as_ptr(), list.len()) };
    }

    /// Requests the server's clipboard data in a format (answer: `Event::ClipData`).
    pub fn clip_request(&self, format_id: u32) {
        unsafe { ffi::rdpc_clip_request(self.0.raw, format_id) };
    }

    /// Answers `Event::ClipDataRequest` (`None` = failure).
    pub fn clip_respond(&self, data: Option<&[u8]>) {
        let (ok, p, n) = data.map_or((0, std::ptr::null(), 0), |d| (1, d.as_ptr(), d.len()));
        unsafe { ffi::rdpc_clip_respond(self.0.raw, ok, p, n) };
    }

    /// Requests a file's size (flags 1) or a range (flags 2) from the server's file list.
    pub fn clip_file_request(&self, stream: u32, index: u32, flags: u32, position: u64, size: u32) {
        unsafe { ffi::rdpc_clip_file_request(self.0.raw, stream, index, flags, position, size) };
    }

    /// Answers `Event::ClipFileRequest` (`None` = failure).
    pub fn clip_file_respond(&self, stream: u32, data: Option<&[u8]>) {
        let (ok, p, n) = data.map_or((0, std::ptr::null(), 0), |d| (1, d.as_ptr(), d.len()));
        unsafe { ffi::rdpc_clip_file_respond(self.0.raw, stream, ok, p, n) };
    }

    /// Adds a folder as a drive during the session (the result comes as Info/Error event).
    pub fn add_drive(&self, name: &str, path: &str) -> bool {
        self.add_device(8, &format!("{name}\t{path}"))
    }

    /// Redirects a CUPS printer during the session.
    pub fn add_printer(&self, name: &str) -> bool {
        self.add_device(4, name)
    }

    fn add_device(&self, kind: u32, spec: &str) -> bool {
        let Ok(c) = CString::new(spec) else {
            return false;
        };
        unsafe { ffi::rdpc_add_device(self.0.raw, kind, c.as_ptr()) == 0 }
    }

    pub fn stop(&self) {
        unsafe { ffi::rdpc_session_stop(self.0.raw) };
    }
}

/// A running session (FreeRDP event loop on its own thread).
pub struct Session {
    handle: SessionHandle,
    thread: Option<JoinHandle<i32>>,
    /// AAD login helper (out-of-process, started by FreeRDP for every login).
    aad_helper: Option<String>,
}

impl Session {
    /// Connects in the background; `sink` receives every event on a FreeRDP thread.
    pub fn start(cfg: &Config, sink: impl FnMut(Event) + Send + 'static) -> Result<Self> {
        if cfg.username.is_none() {
            bail!("a user name is required (UPN for AAD)");
        }
        if cfg.auth == Auth::Aad && cfg.aad_helper.is_none() {
            bail!("AAD needs the path of the login helper");
        }
        let cs = |s: &Option<String>| -> Result<Option<CString>> {
            s.as_deref()
                .map(CString::new)
                .transpose()
                .context("NUL in string")
        };
        let host = CString::new(cfg.host.as_str())?;
        let user = cs(&cfg.username)?;
        let domain = cs(&cfg.domain)?;
        let password = cs(&cfg.password)?;
        let level = cs(&cfg.log_level)?;
        let aad_helper = cs(&cfg.aad_helper)?;
        let sound = cs(&cfg.sound)?;
        let microphone = cs(&cfg.microphone)?;
        let camera = cs(&cfg.camera)?;
        let drives = (!cfg.drives.is_empty()).then(|| {
            cfg.drives
                .iter()
                .map(|(n, p)| format!("{n}\t{p}\n"))
                .collect::<String>()
        });
        let drives = cs(&drives)?;
        let printers = (!cfg.printers.is_empty()).then(|| {
            cfg.printers
                .iter()
                .map(|p| format!("{p}\n"))
                .collect::<String>()
        });
        let printers = cs(&printers)?;
        let monitors: Vec<ffi::RdpcMonitor> = cfg
            .monitors
            .iter()
            .map(|m| ffi::RdpcMonitor {
                left: m.left,
                top: m.top,
                right: m.right,
                bottom: m.bottom,
                primary: u32::from(m.primary),
            })
            .collect();
        let ptr = |c: &Option<CString>| c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let raw_cfg = ffi::RdpcConfig {
            host: host.as_ptr(),
            port: cfg.port,
            username: ptr(&user),
            domain: ptr(&domain),
            password: ptr(&password),
            auth: match cfg.auth {
                Auth::Password => 0,
                Auth::Aad => 1,
            },
            width: cfg.width,
            height: cfg.height,
            monitors: monitors.as_ptr(),
            monitor_count: monitors.len() as u32,
            avc444: u32::from(cfg.avc444),
            ignore_certificate: u32::from(cfg.ignore_certificate),
            log_level: level
                .as_ref()
                .map_or(std::ptr::null(), |c| c.as_ptr() as *const c_char),
            aad_helper: ptr(&aad_helper),
            cert_tofu: u32::from(cfg.cert_tofu),
            sound: ptr(&sound),
            microphone: ptr(&microphone),
            camera: ptr(&camera),
            auto_reconnect: u32::from(cfg.auto_reconnect),
            clipboard: u32::from(cfg.clipboard),
            drives: ptr(&drives),
            printers: ptr(&printers),
            cert_prompt: u32::from(cfg.cert_prompt),
        };
        let sink: *mut Sink = Box::into_raw(Box::new(Mutex::new(Box::new(sink))));
        // The C layer copies all strings and the monitor array into FreeRDP's settings.
        let raw = unsafe { ffi::rdpc_session_new(&raw_cfg, trampoline, sink as *mut c_void) };
        if raw.is_null() {
            unsafe { drop(Box::from_raw(sink)) };
            bail!("rdpc_session_new failed (FreeRDP context or settings)");
        }
        let inner = Arc::new(Inner { raw, sink });
        let runner = inner.clone();
        let thread = std::thread::Builder::new()
            .name("rdp-session".into())
            .spawn(move || unsafe { ffi::rdpc_session_run(runner.raw) })?;
        Ok(Self {
            handle: SessionHandle(inner),
            thread: Some(thread),
            aad_helper: cfg.aad_helper.clone(),
        })
    }

    pub fn handle(&self) -> SessionHandle {
        self.handle.clone()
    }

    /// Stops the session and waits for the session thread, at most [`STOP_WAIT`]: a
    /// reconnect blocked in the AAD login helper (window still open) does not react to the
    /// stop request. The thread then stays detached; it holds its own reference to the
    /// session, and process exit ends it.
    pub fn stop(mut self) -> i32 {
        self.handle.stop();
        let rc = self.thread.take().map_or(0, join_bounded);
        self.end_login_helpers();
        rc
    }

    /// Ends login helpers still running as our children (a reconnect waiting for a login
    /// leaves its window open; it would outlive the client and keep its stdout pipe).
    fn end_login_helpers(&self) {
        let Some(helper) = &self.aad_helper else {
            return;
        };
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        const SIGTERM: i32 = 15;
        let me = std::process::id().to_string();
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return;
        };
        for e in dir.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|p| p.parse::<i32>().ok()) else {
                continue;
            };
            let path = e.path();
            // stat: "pid (comm) state ppid ..."; comm may contain spaces, so split after ')'.
            let ppid = std::fs::read_to_string(path.join("stat"))
                .ok()
                .and_then(|st| {
                    st.rsplit_once(')')
                        .and_then(|(_, rest)| rest.split_whitespace().nth(1).map(str::to_string))
                });
            if ppid.as_deref() != Some(me.as_str()) {
                continue;
            }
            let argv0 = std::fs::read(path.join("cmdline"))
                .ok()
                .and_then(|c| c.split(|b| *b == 0).next().map(|a| a.to_vec()));
            if argv0.as_deref() == Some(helper.as_bytes()) {
                unsafe { kill(pid, SIGTERM) };
            }
        }
    }
}

/// Longest wait for the session thread when stopping.
pub const STOP_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

fn join_bounded(t: JoinHandle<i32>) -> i32 {
    let end = std::time::Instant::now() + STOP_WAIT;
    while !t.is_finished() {
        if std::time::Instant::now() >= end {
            eprintln!(
                "Sitzung reagiert nicht auf Stopp (wartet z. B. auf den Login), beende trotzdem"
            );
            return -1;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    t.join().unwrap_or(-1)
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(t) = self.thread.take() {
            self.handle.stop();
            join_bounded(t);
            self.end_login_helpers();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mouse;

    #[test]
    fn wheel_flags() {
        assert_eq!(mouse::wheel(1, false), mouse::WHEEL | 120);
        assert_eq!(
            mouse::wheel(-1, false),
            mouse::WHEEL | mouse::WHEEL_NEGATIVE | (256 - 120)
        );
        assert_eq!(mouse::wheel(1, true), mouse::HWHEEL | 120);
    }
}

/// Secret storage (csrc/secret.c): freedesktop Secret Service (KWallet, GNOME Keyring,
/// KeePassXC), else a file encrypted with systemd-creds. Attributes identify a secret.
pub mod secret {
    use std::ffi::{CStr, CString};

    use super::ffi;

    /// Value of the "application" attribute of all stored secrets (as `RDPC_SECRET_APP` in
    /// csrc/secret.h); kept stable so stored sign-ins stay valid across versions.
    pub const APP: &str = "rdp-client";

    /// Where `store` put the secret.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Backend {
        None,
        SecretService,
        SystemdCreds,
    }

    fn c_attrs(attrs: &[(&str, &str)]) -> Option<(Vec<CString>, Vec<ffi::RdpcAttr>)> {
        let mut owned = Vec::with_capacity(attrs.len() * 2);
        for (k, v) in attrs {
            owned.push(CString::new(*k).ok()?);
            owned.push(CString::new(*v).ok()?);
        }
        let list = owned
            .chunks(2)
            .map(|c| ffi::RdpcAttr {
                key: c[0].as_ptr(),
                value: c[1].as_ptr(),
            })
            .collect();
        Some((owned, list))
    }

    pub fn lookup(attrs: &[(&str, &str)]) -> Option<String> {
        let (_owned, list) = c_attrs(attrs)?;
        // SAFETY: valid attribute array; the result is freed with rdpc_secret_free.
        unsafe {
            let p = ffi::rdpc_secret_lookup(list.as_ptr(), list.len());
            if p.is_null() {
                return None;
            }
            let v = CStr::from_ptr(p).to_string_lossy().into_owned();
            ffi::rdpc_secret_free(p);
            Some(v)
        }
    }

    pub fn store(attrs: &[(&str, &str)], label: &str, value: &str) -> Backend {
        let (Some((_owned, list)), Ok(label), Ok(value)) =
            (c_attrs(attrs), CString::new(label), CString::new(value))
        else {
            return Backend::None;
        };
        // SAFETY: valid attribute array and strings.
        match unsafe {
            ffi::rdpc_secret_store(list.as_ptr(), list.len(), label.as_ptr(), value.as_ptr())
        } {
            1 => Backend::SecretService,
            2 => Backend::SystemdCreds,
            _ => Backend::None,
        }
    }

    pub fn clear(attrs: &[(&str, &str)]) {
        if let Some((_owned, list)) = c_attrs(attrs) {
            // SAFETY: valid attribute array.
            unsafe { ffi::rdpc_secret_clear(list.as_ptr(), list.len()) }
        }
    }

    /// Removes every stored sign-in and password of the application.
    pub fn clear_all() {
        // SAFETY: no arguments.
        unsafe { ffi::rdpc_secret_clear_all() }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Fallback store only (no Secret Service in tests): round trip in a temp dir.
        #[test]
        fn creds_roundtrip() {
            let dir = std::env::temp_dir().join(format!("rdpc-secret-test-{}", std::process::id()));
            // SAFETY: test-only environment changes before any secret call of this process.
            unsafe {
                std::env::set_var("RDPC_SECRET_BACKEND", "creds");
                std::env::set_var("XDG_DATA_HOME", &dir);
            }
            let a = [("application", APP), ("account", "test"), ("host", "h")];
            if std::process::Command::new("systemd-creds")
                .arg("--version")
                .output()
                .is_err()
            {
                return; // no systemd-creds on this machine
            }
            assert_eq!(store(&a, "test", "geheim 123"), Backend::SystemdCreds);
            let file = std::fs::read_dir(dir.join("rmc/secrets"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            assert!(
                !std::fs::read(&file)
                    .unwrap()
                    .windows(6)
                    .any(|w| w == b"geheim")
            );
            assert_eq!(lookup(&a).as_deref(), Some("geheim 123"));
            clear(&a);
            assert_eq!(lookup(&a), None);
            assert_eq!(store(&a, "test", "x"), Backend::SystemdCreds);
            clear_all();
            assert_eq!(lookup(&a), None);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
