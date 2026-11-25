//! Keyboard grab (docs/architektur.md): `zwp_keyboard_shortcuts_inhibit_v1`
//! on the session windows, so Alt+Tab, the Meta key and other compositor shortcuts go to the
//! remote session. Works on winit's own Wayland connection (foreign display and surfaces)
//! with a separate event queue. Hotkeys of the client are still seen first by the client.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1;
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::{
    self, ZwpKeyboardShortcutsInhibitorV1,
};
use winit::window::WindowId;

#[derive(Default)]
struct State {
    /// Inhibitor object id -> active (the compositor may refuse or end the grab).
    active: HashMap<ObjectId, bool>,
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

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwpKeyboardShortcutsInhibitManagerV1,
        _: <ZwpKeyboardShortcutsInhibitManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        s: &mut Self,
        inh: &ZwpKeyboardShortcutsInhibitorV1,
        ev: zwp_keyboard_shortcuts_inhibitor_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let on = matches!(ev, zwp_keyboard_shortcuts_inhibitor_v1::Event::Active);
        s.active.insert(inh.id(), on);
    }
}

pub struct KeyboardGrab {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    manager: ZwpKeyboardShortcutsInhibitManagerV1,
    seat: WlSeat,
    inhibitors: HashMap<WindowId, ZwpKeyboardShortcutsInhibitorV1>,
}

impl KeyboardGrab {
    /// Uses the display of the given (winit) window system connection.
    pub fn new(display: RawDisplayHandle) -> Result<Self> {
        let RawDisplayHandle::Wayland(h) = display else {
            bail!("keyboard grab needs Wayland");
        };
        // SAFETY: winit keeps the wl_display alive for as long as its event loop exists, which
        // outlives the viewer (the grab is dropped with the viewer's windows).
        let backend = unsafe { Backend::from_foreign_display(h.display.as_ptr() as *mut _) };
        let conn = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();
        let manager: ZwpKeyboardShortcutsInhibitManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .context("compositor offers no keyboard-shortcuts-inhibit")?;
        let seat: WlSeat = globals.bind(&qh, 1..=7, ()).context("no wl_seat")?;
        Ok(Self {
            conn,
            queue,
            state: State::default(),
            manager,
            seat,
            inhibitors: HashMap::new(),
        })
    }

    /// Turns the grab for one window on or off.
    pub fn set(&mut self, id: WindowId, window: RawWindowHandle, on: bool) -> Result<()> {
        if on && !self.inhibitors.contains_key(&id) {
            let RawWindowHandle::Wayland(w) = window else {
                bail!("keyboard grab needs a Wayland window");
            };
            // SAFETY: the surface belongs to a live winit window; the inhibitor is destroyed
            // (`set(.., false)` / `forget`) before that window is dropped.
            let sid = unsafe {
                ObjectId::from_ptr(WlSurface::interface(), w.surface.as_ptr() as *mut _)
            }?;
            let surface = WlSurface::from_id(&self.conn, sid)?;
            let qh = self.queue.handle();
            let inh = self
                .manager
                .inhibit_shortcuts(&surface, &self.seat, &qh, ());
            self.inhibitors.insert(id, inh);
        } else if !on && let Some(inh) = self.inhibitors.remove(&id) {
            self.state.active.remove(&inh.id());
            inh.destroy();
        }
        self.conn.flush()?;
        Ok(())
    }

    /// Releases the grab of a window that is about to close.
    pub fn forget(&mut self, id: WindowId) {
        if let Some(inh) = self.inhibitors.remove(&id) {
            self.state.active.remove(&inh.id());
            inh.destroy();
            let _ = self.conn.flush();
        }
    }

    /// Processes compositor answers already read from the socket (winit reads it; reading
    /// here could leave events for winit's queue without waking winit). Returns how many
    /// inhibitors are active.
    pub fn poll(&mut self) -> usize {
        let _ = self.queue.dispatch_pending(&mut self.state);
        self.state.active.values().filter(|a| **a).count()
    }
}
