//! Clipboard between the Linux desktop (Wayland, rdp-clipboard) and the session (cliprdr).
//!
//! - Linux -> Windows: a local selection is announced as Windows formats; when Windows pastes,
//!   the data is read from the local selection and converted (on a worker thread).
//! - Windows -> Linux: the server's formats are offered as MIME types; when a Linux app pastes,
//!   the provider requests the data from the server and waits for the answer.
//!
//! Text (CF_UNICODETEXT), images (PNG / CF_DIB) and files (FileGroupDescriptorW with file
//! contents requests; directories recursively) in both directions. Files from Windows land in
//! `~/.cache/rmc/clipboard/<time>/` and are handed over as `text/uri-list`.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rdp_clipboard::WaylandClipboard;
use rdp_clipboard::convert::{
    self, CF_DIB, CF_DIBV5, CF_TEXT, CF_UNICODETEXT, PNG_MIME, PNG_NAME, TEXT_MIMES,
};
use rdp_clipboard::files::{
    self, DROP_EFFECT_NAME, FILE_LIST_NAME, GNOME_FILES_MIME, URI_LIST_MIME,
};
use rdp_session::SessionHandle;

/// Ids of registered formats in our format list (ids >= 0xC000 are free for names).
const CLIENT_PNG_ID: u32 = 0xC0A0;
const CLIENT_FILES_ID: u32 = 0xC0B0;
const CLIENT_DROP_EFFECT_ID: u32 = 0xC0B1;
const FILECONTENTS_SIZE: u32 = 1;
const FILECONTENTS_RANGE: u32 = 2;
/// Chunk size when fetching file contents from the server.
/// (small enough that graphics and input interleave with the file data)
const CHUNK: u32 = 1024 * 1024;
/// No file data requested for this long: Windows cancelled (its copy dialog), display ends.
const STALL: Duration = Duration::from_secs(5);

/// What the server currently offers (format ids to request).
#[derive(Default, Clone, Copy)]
struct Remote {
    text: Option<u32>,
    png: Option<u32>,
    dib: Option<u32>,
    files: Option<u32>,
    /// Increments with every server format list (cache key for downloaded files).
    generation: u64,
}

/// Progress of the current file transfer (either direction).
#[derive(Default)]
struct Transfer {
    done: u64,
    total: u64,
    started: Option<Instant>,
    last: Option<Instant>,
    title: String,
    /// Cancelled from the bar: further file requests fail, downloads stop.
    cancelled: bool,
    /// KDE job view (percent, speed, remaining time), only while the session is minimized.
    job: Option<crate::jobview::JobView>,
}

/// Answer slot for one outstanding server data request.
#[derive(Default)]
struct Pending {
    answer: Option<Option<Vec<u8>>>,
    /// File contents answers by stream id.
    files: HashMap<u32, Option<Vec<u8>>>,
}

struct Inner {
    handle: SessionHandle,
    remote: Mutex<Remote>,
    /// Local selection MIME types (for answering server requests).
    local: Mutex<Vec<String>>,
    /// Local files of the last file list we sent (index = list index of the server).
    local_files: Mutex<Vec<PathBuf>>,
    /// Serializes server requests; the answer arrives via `on_data` / `on_file_data`.
    request: Mutex<()>,
    pending: Mutex<Pending>,
    answered: Condvar,
    next_stream: Mutex<u32>,
    /// Downloaded remote files: (generation, top-level paths) of the last download; the
    /// lock also serializes downloads.
    downloaded: Mutex<Option<(u64, Vec<PathBuf>)>>,
    /// When remote content was last offered locally: local changes right after it are
    /// echoes (e.g. Klipper taking over the selection) and are not sent back to Windows.
    last_offer: Mutex<Option<Instant>>,
    transfer: Mutex<Transfer>,
    /// Bandwidth for file data in bytes per second (0 = unlimited).
    limit: u64,
    minimized: std::sync::atomic::AtomicBool,
    /// Switched off from the bar's menu: nothing is exchanged until switched on again.
    enabled: std::sync::atomic::AtomicBool,
}

pub struct Clipboard {
    inner: Arc<Inner>,
    wl: Arc<WaylandClipboard>,
}

impl Clipboard {
    /// `limit_mbit`: bandwidth for file transfers in Mbit/s (0 = unlimited), leaving room
    /// for graphics and input on the same connection.
    pub fn start(handle: SessionHandle, limit_mbit: u32) -> Result<Self> {
        prune_cache(Duration::from_secs(24 * 3600));
        let inner = Arc::new(Inner {
            handle,
            remote: Mutex::new(Remote::default()),
            local: Mutex::new(Vec::new()),
            local_files: Mutex::new(Vec::new()),
            request: Mutex::new(()),
            pending: Mutex::new(Pending::default()),
            answered: Condvar::new(),
            next_stream: Mutex::new(1),
            downloaded: Mutex::new(None),
            last_offer: Mutex::new(None),
            transfer: Mutex::new(Transfer::default()),
            limit: u64::from(limit_mbit) * 1_000_000 / 8,
            minimized: std::sync::atomic::AtomicBool::new(false),
            enabled: std::sync::atomic::AtomicBool::new(true),
        });
        let (on_local_inner, provider_inner) = (inner.clone(), inner.clone());
        let wl = WaylandClipboard::start(
            Box::new(move |mimes| on_local_inner.local_changed(mimes)),
            Arc::new(move |mime| provider_inner.provide(mime)),
        )?;
        Ok(Self {
            inner,
            wl: Arc::new(wl),
        })
    }

    /// Switches the exchange on or off (bar menu). Off: Windows' clipboard is not offered
    /// locally, local copies are not announced, requests get no data.
    pub fn set_enabled(&self, on: bool) {
        self.inner
            .enabled
            .store(on, std::sync::atomic::Ordering::Relaxed);
        println!("Zwischenablage: {}", if on { "an" } else { "aus" });
    }

    pub fn enabled(&self) -> bool {
        self.inner
            .enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Server clipboard changed: offer its content locally.
    pub fn remote_formats(&self, formats: &[(u32, String)]) {
        if !self.enabled() {
            return;
        }
        let mut r = Remote::default();
        for (id, name) in formats {
            match (*id, name.as_str()) {
                (CF_UNICODETEXT, _) => r.text = Some(CF_UNICODETEXT),
                (CF_TEXT, _) if r.text.is_none() => r.text = Some(CF_TEXT),
                (CF_DIB, _) => r.dib = Some(CF_DIB),
                (CF_DIBV5, _) if r.dib.is_none() => r.dib = Some(CF_DIBV5),
                (id, n) if n == PNG_NAME => r.png = Some(id),
                (id, n) if n == FILE_LIST_NAME => r.files = Some(id),
                _ => {}
            }
        }
        {
            let mut cur = self.inner.remote.lock().expect("lock");
            r.generation = cur.generation + 1;
            *cur = r;
        }
        let mut mimes: Vec<String> = Vec::new();
        if r.files.is_some() {
            mimes.push(URI_LIST_MIME.to_string());
            mimes.push(GNOME_FILES_MIME.to_string());
        }
        if r.text.is_some() {
            mimes.extend(TEXT_MIMES.iter().map(|m| m.to_string()));
        }
        if r.png.is_some() || r.dib.is_some() {
            mimes.push(PNG_MIME.to_string());
        }
        *self.inner.last_offer.lock().expect("lock") = Some(Instant::now());
        if let Err(e) = self.wl.offer(&mimes) {
            eprintln!("Zwischenablage: Angebot fehlgeschlagen: {e:#}");
        }
    }

    /// Windows pastes: read and convert the local selection on a worker thread.
    pub fn data_request(&self, format: u32) {
        if !self.enabled() {
            self.inner.handle.clip_respond(None);
            return;
        }
        let (inner, wl) = (self.inner.clone(), self.wl.clone());
        std::thread::spawn(move || {
            let data = inner.local_data(&wl, format);
            inner.handle.clip_respond(data.as_deref());
        });
    }

    /// Windows reads a file of our file list.
    pub fn file_request(&self, stream: u32, index: u32, flags: u32, position: u64, size: u32) {
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            let data = inner
                .local_file_contents(index, flags, position, size)
                .map_err(|e| eprintln!("Zwischenablage: Datei nicht lesbar: {e:#}"))
                .ok();
            inner.handle.clip_file_respond(stream, data.as_deref());
        });
    }

    /// Answer to our request (Linux pastes remote content).
    pub fn on_data(&self, ok: bool, data: Vec<u8>) {
        let mut p = self.inner.pending.lock().expect("lock");
        p.answer = Some(ok.then_some(data));
        self.inner.answered.notify_all();
    }

    /// Progress of a running file transfer for the bar: fraction and text ("42%  0:35"
    /// with the remaining time); None when idle. Shown until shortly after completion, or
    /// 60 s without progress (stalled or aborted).
    pub fn progress(&self) -> Option<(f32, String)> {
        let mut t = self.inner.transfer.lock().expect("lock");
        let last = t.last?;
        if t.total == 0 {
            return None;
        }
        let complete = t.done >= t.total;
        let idle = last.elapsed();
        if !complete && idle > STALL {
            // Windows stopped asking (copy cancelled there) or the transfer broke off
            if let Some(mut j) = t.job.take() {
                j.finish(Some("abgebrochen".into()));
            }
            t.last = None;
            return None;
        }
        if complete && idle > Duration::from_millis(1500) {
            return None;
        }
        let f = (t.done as f64 / t.total as f64).min(1.0);
        let elapsed = t.started.map_or(0.0, |s| s.elapsed().as_secs_f64());
        let mut text = format!("{}%", (f * 100.0) as u32);
        if !complete && elapsed > 2.0 && t.done > 0 {
            let rate = t.done as f64 / elapsed;
            let rest = ((t.total - t.done) as f64 / rate) as u64;
            text.push_str(&format!("  {}", hms(rest)));
        }
        Some((f as f32, text))
    }

    /// Session minimized: the transfer is shown in KDE's job view instead of the bar.
    pub fn set_minimized(&self, on: bool) {
        self.inner
            .minimized
            .store(on, std::sync::atomic::Ordering::Relaxed);
        if on {
            let mut t = self.inner.transfer.lock().expect("lock");
            if t.job.is_none() && t.last.is_some() && t.done < t.total && !t.cancelled {
                let (done, total) = (t.done, t.total);
                t.job = crate::jobview::JobView::start(&t.title);
                if let Some(j) = &mut t.job {
                    j.update(done, total, 0, true);
                }
            }
        }
    }

    /// Cancel button of the bar: stops the running transfer in both directions.
    pub fn cancel(&self) {
        let mut t = self.inner.transfer.lock().expect("lock");
        t.cancelled = true;
        t.last = None;
        if let Some(mut j) = t.job.take() {
            j.finish(Some("abgebrochen".into()));
        }
    }

    /// Answer to our file contents request.
    pub fn on_file_data(&self, stream: u32, ok: bool, data: Vec<u8>) {
        let mut p = self.inner.pending.lock().expect("lock");
        p.files.insert(stream, ok.then_some(data));
        self.inner.answered.notify_all();
    }
}

impl Inner {
    /// New local selection: announce the matching Windows formats.
    fn local_changed(&self, mimes: Vec<String>) {
        // Never announce an empty list (would clear the Windows clipboard), and no echo of
        // what Windows just put on offer.
        if mimes.is_empty() || !self.enabled.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        if self
            .last_offer
            .lock()
            .expect("lock")
            .is_some_and(|t| t.elapsed() < Duration::from_millis(1500))
        {
            return;
        }
        let mut formats: Vec<(u32, &str)> = Vec::new();
        if mimes.iter().any(|m| m == URI_LIST_MIME) {
            formats.push((CLIENT_FILES_ID, FILE_LIST_NAME));
            formats.push((CLIENT_DROP_EFFECT_ID, DROP_EFFECT_NAME));
        }
        if convert::local_text_mime(&mimes).is_some() {
            formats.push((CF_UNICODETEXT, ""));
        }
        if mimes.iter().any(|m| m == PNG_MIME) {
            formats.push((CF_DIB, ""));
            formats.push((CLIENT_PNG_ID, PNG_NAME));
        }
        *self.local.lock().expect("lock") = mimes;
        self.handle.clip_set_formats(&formats);
    }

    /// Local selection in a Windows format.
    fn local_data(&self, wl: &WaylandClipboard, format: u32) -> Option<Vec<u8>> {
        let mimes = self.local.lock().expect("lock").clone();
        let r = match format {
            CF_UNICODETEXT | CF_TEXT => {
                let mime = convert::local_text_mime(&mimes)?;
                wl.read(mime).map(|t| {
                    let text = String::from_utf8_lossy(&t);
                    if format == CF_UNICODETEXT {
                        convert::text_to_utf16(&text)
                    } else {
                        let mut b = text.replace('\n', "\r\n").into_bytes();
                        b.push(0);
                        b
                    }
                })
            }
            CF_DIB | CF_DIBV5 => wl.read(PNG_MIME).and_then(|png| convert::png_to_dib(&png)),
            CLIENT_PNG_ID => wl.read(PNG_MIME),
            CLIENT_DROP_EFFECT_ID => Ok(1u32.to_le_bytes().to_vec()), // DROPEFFECT_COPY
            CLIENT_FILES_ID => wl.read(URI_LIST_MIME).and_then(|list| {
                let paths = files::parse_uri_list(&String::from_utf8_lossy(&list));
                let entries = files::collect(&paths)?;
                let (list, local): (Vec<_>, Vec<_>) = entries.into_iter().unzip();
                *self.local_files.lock().expect("lock") = local;
                self.start_transfer(
                    list.iter().map(|e| e.size).sum(),
                    "Zwischenablage: Dateien nach Windows",
                );
                Ok(files::build_descriptor(&list))
            }),
            _ => return None,
        };
        r.map_err(|e| eprintln!("Zwischenablage: lokales Lesen fehlgeschlagen: {e:#}"))
            .ok()
    }

    fn start_transfer(&self, total: u64, title: &str) {
        let now = Instant::now();
        let minimized = self.minimized.load(std::sync::atomic::Ordering::Relaxed);
        let mut job = (total > 0 && minimized)
            .then(|| crate::jobview::JobView::start(title))
            .flatten();
        if let Some(j) = &mut job {
            j.update(0, total, 0, true);
        }
        // the previous job (if still open) ends with its drop
        *self.transfer.lock().expect("lock") = Transfer {
            done: 0,
            total,
            started: Some(now),
            last: Some(now),
            title: title.to_string(),
            cancelled: false,
            job,
        };
    }

    fn cancelled(&self) -> bool {
        self.transfer.lock().expect("lock").cancelled
    }

    /// Bandwidth limit: waits until `done` bytes are within the allowed rate.
    fn pace(&self) {
        if self.limit == 0 {
            return;
        }
        let (done, started) = {
            let t = self.transfer.lock().expect("lock");
            (t.done, t.started)
        };
        if let Some(s) = started {
            let due = Duration::from_secs_f64(done as f64 / self.limit as f64);
            if let Some(wait) = due.checked_sub(s.elapsed()) {
                std::thread::sleep(wait.min(Duration::from_secs(2)));
            }
        }
    }

    fn advance(&self, n: u64) {
        let mut t = self.transfer.lock().expect("lock");
        t.done += n;
        t.last = Some(Instant::now());
        let (done, total) = (t.done, t.total);
        let speed = t.started.map_or(0, |s| {
            let secs = s.elapsed().as_secs_f64();
            if secs > 0.0 {
                (done as f64 / secs) as u64
            } else {
                0
            }
        });
        let complete = done >= total;
        if let Some(j) = &mut t.job {
            j.update(done, total, speed, complete);
            if complete {
                j.finish(None);
            }
        }
    }

    /// Size (8 bytes) or a range of a file from our last file list.
    fn local_file_contents(&self, index: u32, flags: u32, pos: u64, size: u32) -> Result<Vec<u8>> {
        let path = self
            .local_files
            .lock()
            .expect("lock")
            .get(index as usize)
            .cloned()
            .context("unknown list index")?;
        if flags & FILECONTENTS_SIZE != 0 {
            let len = std::fs::metadata(&path)?.len();
            return Ok(len.to_le_bytes().to_vec());
        }
        if flags & FILECONTENTS_RANGE == 0 {
            bail!("unknown file contents request {flags}");
        }
        if self.cancelled() {
            bail!("transfer cancelled");
        }
        self.pace();
        let mut f = std::fs::File::open(&path)?;
        f.seek(SeekFrom::Start(pos))?;
        let mut buf = Vec::with_capacity(size as usize);
        f.take(u64::from(size)).read_to_end(&mut buf)?;
        self.advance(buf.len() as u64);
        Ok(buf)
    }

    /// Requests a format from the server and waits for the answer.
    fn fetch(&self, format: u32) -> Option<Vec<u8>> {
        let _one = self.request.lock().expect("lock");
        self.pending.lock().expect("lock").answer = None;
        self.handle.clip_request(format);
        let p = self.pending.lock().expect("lock");
        let (mut p, timeout) = self
            .answered
            .wait_timeout_while(p, Duration::from_secs(10), |p| p.answer.is_none())
            .expect("lock");
        if timeout.timed_out() {
            eprintln!("Zwischenablage: keine Antwort vom Server");
            return None;
        }
        p.answer.take().flatten()
    }

    /// One file contents request to the server, waiting for its answer.
    fn fetch_file(&self, index: u32, flags: u32, pos: u64, size: u32) -> Option<Vec<u8>> {
        let stream = {
            let mut s = self.next_stream.lock().expect("lock");
            *s = s.wrapping_add(1).max(1);
            *s
        };
        self.handle
            .clip_file_request(stream, index, flags, pos, size);
        let p = self.pending.lock().expect("lock");
        let (mut p, timeout) = self
            .answered
            .wait_timeout_while(p, Duration::from_secs(30), |p| {
                !p.files.contains_key(&stream)
            })
            .expect("lock");
        if timeout.timed_out() {
            eprintln!("Zwischenablage: keine Dateidaten vom Server");
            return None;
        }
        p.files.remove(&stream).flatten()
    }

    /// Downloads the server's file list into a new cache directory; returns the top-level
    /// paths. Cached per server format list (Klipper and the pasting app ask both).
    fn download_files(&self, format: u32, generation: u64) -> Result<Vec<PathBuf>> {
        let mut cached = self.downloaded.lock().expect("lock");
        if let Some((g, paths)) = &*cached
            && *g == generation
        {
            return Ok(paths.clone());
        }
        let list = self.fetch(format).context("no file list from the server")?;
        let entries = files::parse_descriptor(&list)?;
        self.start_transfer(
            entries.iter().map(|e| e.size).sum(),
            "Zwischenablage: Dateien von Windows",
        );
        let dir = cache_dir().join(crate_stamp());
        std::fs::create_dir_all(&dir)?;
        let mut top = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            let Some(rel) = files::safe_relative(&e.name) else {
                eprintln!("Zwischenablage: Dateiname abgelehnt: {}", e.name);
                continue;
            };
            let target = dir.join(&rel);
            if rel.components().count() == 1 {
                top.push(target.clone());
            }
            if e.dir {
                std::fs::create_dir_all(&target)?;
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut size = e.size;
            if size == 0 {
                // size not in the descriptor: ask for it
                if let Some(b) = self.fetch_file(i as u32, FILECONTENTS_SIZE, 0, 8)
                    && b.len() >= 8
                {
                    size = u64::from_le_bytes(b[..8].try_into().expect("8 bytes"));
                }
            }
            let mut f = std::fs::File::create(&target)?;
            let mut pos = 0u64;
            while pos < size {
                if self.cancelled() {
                    drop(f);
                    let _ = std::fs::remove_dir_all(&dir);
                    bail!("cancelled");
                }
                self.pace();
                let want = (size - pos).min(u64::from(CHUNK)) as u32;
                let chunk = self
                    .fetch_file(i as u32, FILECONTENTS_RANGE, pos, want)
                    .with_context(|| format!("contents of {}", e.name))?;
                if chunk.is_empty() {
                    break;
                }
                f.write_all(&chunk)?;
                pos += chunk.len() as u64;
                self.advance(chunk.len() as u64);
            }
        }
        *cached = Some((generation, top.clone()));
        Ok(top)
    }

    /// A Linux app pastes remote content in `mime`.
    fn provide(&self, mime: &str) -> Option<Vec<u8>> {
        let r = *self.remote.lock().expect("lock");
        if mime == URI_LIST_MIME || mime == GNOME_FILES_MIME {
            let paths = self
                .download_files(r.files?, r.generation)
                .map_err(|e| eprintln!("Zwischenablage: Dateien nicht übertragbar: {e:#}"))
                .ok()?;
            let uris: Vec<String> = paths.iter().map(|p| files::file_uri(p)).collect();
            return Some(if mime == URI_LIST_MIME {
                uris.iter()
                    .map(|u| format!("{u}\r\n"))
                    .collect::<String>()
                    .into_bytes()
            } else {
                format!("copy\n{}", uris.join("\n")).into_bytes()
            });
        }
        if TEXT_MIMES.contains(&mime) {
            let id = r.text?;
            let data = self.fetch(id)?;
            return Some(if id == CF_UNICODETEXT {
                convert::utf16_to_text(&data).into_bytes()
            } else {
                // CF_TEXT: ANSI, NUL-terminated; treat as Latin-1 compatible bytes
                let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
                String::from_utf8_lossy(&data[..end])
                    .replace("\r\n", "\n")
                    .into_bytes()
            });
        }
        if mime == PNG_MIME {
            if let Some(id) = r.png
                && let Some(png) = self.fetch(id)
            {
                return Some(png);
            }
            let dib = self.fetch(r.dib?)?;
            return convert::dib_to_png(&dib)
                .map_err(|e| eprintln!("Zwischenablage: Bild nicht umwandelbar: {e:#}"))
                .ok();
        }
        None
    }
}

/// `~/.cache/rmc/clipboard`.
fn cache_dir() -> PathBuf {
    crate::paths::cache_dir().join("clipboard")
}

/// Seconds as "m:ss" or "h:mm:ss".
fn hms(s: u64) -> String {
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Removes downloads older than `age` from the cache directory.
fn prune_cache(age: Duration) {
    let Ok(rd) = std::fs::read_dir(cache_dir()) else {
        return;
    };
    for e in rd.flatten() {
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|el| el > age);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Unique directory name per download.
fn crate_stamp() -> String {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{:03}", t.as_secs(), t.subsec_millis())
}
