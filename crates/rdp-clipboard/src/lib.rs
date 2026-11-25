//! Wayland clipboard for the RDP client: reads and offers the selection through
//! `ext-data-control-v1` (KWin, like a clipboard manager), so it works without window focus,
//! plus the format conversions between Linux MIME types and Windows clipboard formats.
//!
//! - Local changes are reported with their MIME types (`on_local`).
//! - [`WaylandClipboard::read`] fetches the current local selection in one MIME type.
//! - [`WaylandClipboard::offer`] puts remote content on offer; the data is produced lazily by
//!   the provider when a Linux application pastes.

pub mod convert;
pub mod files;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

/// Marker MIME type on our own offers: the selection event that follows our own
/// `set_selection` is not a local change.
const MARKER: &str = "application/x-rmc-owned";

/// Produces the data of a MIME type when a Linux application pastes remote content.
pub type Provider = Arc<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;
/// Called with the MIME types of a new local selection (empty: cleared).
pub type OnLocal = Box<dyn Fn(Vec<String>) + Send>;

#[derive(Default)]
struct Shared {
    /// Current foreign selection (not ours) and its MIME types.
    current: Option<(ExtDataControlOfferV1, Vec<String>)>,
    /// Our offer currently set as selection.
    own: Option<ExtDataControlSourceV1>,
}

struct State {
    shared: Arc<Mutex<Shared>>,
    offers: HashMap<ObjectId, Vec<String>>,
    on_local: OnLocal,
    provider: Provider,
}

pub struct WaylandClipboard {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    shared: Arc<Mutex<Shared>>,
}

impl WaylandClipboard {
    /// Connects to the compositor and starts the event thread.
    pub fn start(on_local: OnLocal, provider: Provider) -> Result<Self> {
        let conn = Connection::connect_to_env().context("Wayland connection")?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let manager: ExtDataControlManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("compositor offers no ext_data_control_manager_v1")?;
        let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=9, ()).context("no wl_seat")?;
        let device = manager.get_data_device(&seat, &qh, ());
        let shared = Arc::new(Mutex::new(Shared::default()));
        let mut state = State {
            shared: shared.clone(),
            offers: HashMap::new(),
            on_local,
            provider,
        };
        conn.flush()?;
        std::thread::Builder::new()
            .name("wl-clipboard".into())
            .spawn(move || while queue.blocking_dispatch(&mut state).is_ok() {})?;
        Ok(Self {
            conn,
            qh,
            manager,
            device,
            shared,
        })
    }

    /// Reads the current local selection in `mime` (blocking, bounded by a timeout).
    pub fn read(&self, mime: &str) -> Result<Vec<u8>> {
        let offer = {
            let s = self.shared.lock().expect("clipboard lock");
            match &s.current {
                Some((o, mimes)) if mimes.iter().any(|m| m == mime) => o.clone(),
                _ => bail!("no local selection in {mime}"),
            }
        };
        let (r, w) = pipe()?;
        offer.receive(mime.to_string(), w.as_fd());
        self.conn.flush()?;
        drop(w);
        read_all(r, Duration::from_secs(5))
    }

    /// Offers remote content in these MIME types (empty: nothing); data via the provider.
    pub fn offer(&self, mimes: &[String]) -> Result<()> {
        let mut s = self.shared.lock().expect("clipboard lock");
        if mimes.is_empty() {
            if let Some(old) = s.own.take() {
                old.destroy();
            }
            drop(s);
            self.conn.flush()?;
            return Ok(());
        }
        // Set the new offer before dropping the old one: no moment with an empty selection
        // (that would read as "cleared locally" and wipe the server's clipboard).
        let source = self.manager.create_data_source(&self.qh, ());
        for m in mimes {
            source.offer(m.clone());
        }
        source.offer(MARKER.to_string());
        self.device.set_selection(Some(&source));
        if let Some(old) = s.own.replace(source) {
            old.destroy();
        }
        drop(s);
        self.conn.flush()?;
        Ok(())
    }
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        bail!("pipe2: {}", std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Reads until EOF; gives up after `timeout` without data.
fn read_all(fd: OwnedFd, timeout: Duration) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let mut file = std::fs::File::from(fd);
    let mut out = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let mut pfd = libc::pollfd {
            fd: file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let r = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) };
        if r == 0 {
            bail!("clipboard read timed out");
        }
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        match file.read(&mut buf) {
            Ok(0) => return Ok(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                let mut shared = state.shared.lock().expect("clipboard lock");
                if let Some((old, _)) = shared.current.take() {
                    old.destroy();
                }
                match id {
                    Some(offer) => {
                        let mimes = state.offers.remove(&offer.id()).unwrap_or_default();
                        if mimes.iter().any(|m| m == MARKER) {
                            // our own offer: not a local change
                            offer.destroy();
                        } else {
                            shared.current = Some((offer, mimes.clone()));
                            drop(shared);
                            (state.on_local)(mimes);
                        }
                    }
                    None => {
                        // A cleared selection is not reported: it happens when an owner goes
                        // away (also our own offer) and must not empty the server's clipboard.
                    }
                }
            }
            ext_data_control_device_v1::Event::PrimarySelection { id: Some(offer) } => {
                state.offers.remove(&offer.id());
                offer.destroy();
            }
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offers.entry(offer.id()).or_default().push(mime_type);
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_source_v1::Event::Send { mime_type, fd } => {
                // The data may have to come from the server first: write on a thread.
                let provider = state.provider.clone();
                std::thread::spawn(move || {
                    if let Some(data) = provider(&mime_type) {
                        let mut f = std::fs::File::from(fd);
                        let _ = f.write_all(&data);
                    }
                });
            }
            ext_data_control_source_v1::Event::Cancelled => {
                let mut shared = state.shared.lock().expect("clipboard lock");
                if shared.own.as_ref().is_some_and(|s| s.id() == source.id()) {
                    shared.own = None;
                }
                source.destroy();
            }
            _ => {}
        }
    }
}
