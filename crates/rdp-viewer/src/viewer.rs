//! Session views on Wayland (shared by `rmc` and `rdpgfx-replay`).
//!
//! Fullscreen: one toplevel per session output on its local monitor. Windowed: one window
//! with a 1:1 crop of the session desktop that keeps position and size across mode switches
//! (scaled overview as an option). Switching only exchanges views; surfaces and decoders
//! stay. The overlay layer (connection bar placeholder, scroll thumbs) sees pointer input
//! first; client hotkeys (Ctrl+Alt+…) are evaluated before anything goes to the session.
//!
//! Hotkeys: Enter = fullscreen <-> windowed, S = crop <-> overview, M = minimise all,
//! L = show/hide bar, P = pin bar, K = keyboard grab on/off, arrows / wheel / left drag =
//! move the crop (Shift+wheel horizontal), Space = pause (replay), Q = quit.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
use winit::monitor::MonitorHandle;
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::window::{CursorIcon, CustomCursor, Fullscreen, Window, WindowId};

use rdpgfx_gpu::core::{ButtonRow, GpuCore, OverlayImage, OverlayState, ViewId};
use rdpgfx_model::Rect;
use rdpgfx_model::outputs::{DesktopLayout, Monitor, ViewScale};

use crate::grab::KeyboardGrab;

/// One output of the session desktop and the local monitor that shows it in fullscreen.
#[derive(Clone, Debug)]
pub struct SessionOutput {
    pub name: String,
    /// Rectangle in output coordinates (bounding box starts at 0, 0).
    pub rect: Rect,
    pub primary: bool,
    pub monitor: Option<MonitorHandle>,
}

#[derive(Clone, Debug)]
pub struct ViewerOptions {
    /// Window title prefix.
    pub title: String,
    /// App id "rdp-replay-test" for automated tests (no focus, placement via a window rule);
    /// never fullscreen.
    pub test_window: bool,
    pub start_windowed: bool,
    /// Keyboard grab on entering fullscreen (off in windowed mode); toggle with Ctrl+Alt+K.
    pub grab_in_fullscreen: bool,
    /// Connection bar pinned (always visible) at start; the pin button toggles it.
    pub bar_pinned: bool,
    /// "Keep active" (small mouse moves so Windows/Teams do not go idle) at start.
    pub keep_active: bool,
    /// Shown in the bar (favorite name or host).
    pub label: String,
    /// The connection is no favorite yet: the bar offers a star to save it.
    pub offer_favorite: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Fullscreen,
    Windowed,
}

/// Input for the session, in session desktop coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Input {
    Key {
        code: KeyCode,
        down: bool,
    },
    Move {
        x: u32,
        y: u32,
    },
    Button {
        button: MouseButton,
        down: bool,
        x: u32,
        y: u32,
    },
    /// Wheel notches (positive = up / right).
    Wheel {
        dx: i32,
        dy: i32,
        x: u32,
        y: u32,
    },
    /// Focus lost: release every pressed key.
    ReleaseAll,
}

#[derive(Clone, Debug)]
pub enum Action {
    Input(Input),
    Quit,
    /// The cancel button of the transfer progress strip was clicked.
    CancelTransfer,
    /// The star in the bar was clicked: save this connection as a favorite.
    SaveFavorite,
    /// An entry of the settings menu (gear) was clicked: its id.
    Menu(u32),
    /// Ctrl+Alt+Space (used by the replay).
    Pause,
    /// Left click that went to the session (for logs): output name, view and session position.
    Clicked {
        output: String,
        view: (f64, f64),
        session: Option<(u32, u32)>,
    },
}

/// Entry of the settings menu that opens below the bar (gear button).
#[derive(Clone, Debug, PartialEq)]
pub struct MenuItem {
    pub id: u32,
    pub label: String,
    pub kind: MenuKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuKind {
    /// On/off switch (stays open when clicked).
    Toggle(bool),
    /// Command (closes the menu).
    Action,
    /// Already done, not clickable (e.g. a drive that was added).
    Done,
    /// Small caption.
    Header,
    Separator,
}

impl MenuItem {
    pub fn new(id: u32, label: impl Into<String>, kind: MenuKind) -> Self {
        Self {
            id,
            label: label.into(),
            kind,
        }
    }
}

/// Rendered menu: the entries and hover it shows, and its pixels.
type MenuImage = (Vec<MenuItem>, Option<usize>, std::sync::Arc<[u32]>);

/// Cursor image (RGBA, straight alpha).
#[derive(Clone, Debug)]
pub struct CursorShape {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum CursorUpdate {
    /// New shape, cached under `index` (RDP pointer cache).
    Set {
        index: u16,
        shape: CursorShape,
    },
    Cached {
        index: u16,
    },
    Hidden,
    Default,
}

#[derive(Clone)]
enum CursorState {
    Default,
    Hidden,
    Custom(CustomCursor),
}

struct Win {
    window: Window,
    view: ViewId,
    /// Session output shown in fullscreen (`None` = windowed).
    output: Option<usize>,
    cursor: Option<PhysicalPosition<f64>>,
    /// Diagnostics: pointer moves since the last `pointer_stats` and the last session point.
    moves: u32,
    last_point: Option<(u32, u32)>,
}

#[derive(Default)]
struct Bar {
    shown: bool,
    hide_at: Option<Instant>,
    pinned: bool,
    hover: i32,
}

#[derive(Clone, Copy)]
enum DragKind {
    Pan,
    Thumb(usize),
}

#[derive(Clone, Copy)]
struct Drag {
    kind: DragKind,
    start: (f64, f64),
    origin: (u32, u32),
}

// Bar button ids (as in overlay.comp).
const B_PIN: i32 = 0;
const B_MINIMISE: i32 = 1;
const B_WINDOW: i32 = 2;
const B_GRAB: i32 = 3;
const B_CLOSE: i32 = 4;
const B_KEEP: i32 = 5;
const B_STAR: i32 = 6;
const B_GEAR: i32 = 7;

/// Settings menu: width, row heights (item, separator, caption), padding.
const MENU_W: u32 = 300;
const MENU_ROW: u32 = 32;
const MENU_SEP: u32 = 11;
const MENU_HEAD: u32 = 26;
const MENU_PAD: u32 = 6;

/// Bar colours shared with overlay.comp (0.17 background, 0.92 glyphs).
const BAR_BG: crate::text::Rgb = [43, 43, 43];
const BAR_FG: crate::text::Rgb = [235, 235, 235];
/// Name label: font size and box (left of the right button group).
const LABEL_SIZE: f32 = 15.0;
const LABEL_X: u32 = 2 * GAP + BUTTON + 8;

const CROP_INITIAL: (u32, u32) = (1280, 800);
const THUMB: u32 = 8;
const PAN_STEP: f64 = 120.0;
const BAR_W: u32 = 560;
const BAR_H: u32 = 32;
/// Height of the transfer progress strip below the bar.
const PROGRESS_H: u32 = 16;
/// Width of the cancel button at the right end of the progress strip.
const PROGRESS_CANCEL_W: u32 = 20;
const BAR_HIDE: Duration = Duration::from_secs(3);
const BUTTON: u32 = 24;
const GAP: u32 = 6;

/// Same hit test as `overlay.comp`: the button id at a bar position (-1 none). `right` is
/// the right group, left to right.
fn bar_button(local_x: u32, local_y: u32, right: &[i32]) -> i32 {
    if !(4..4 + BUTTON).contains(&local_y) {
        return -1;
    }
    if (GAP..GAP + BUTTON).contains(&local_x) {
        return B_PIN;
    }
    let n = right.len() as u32;
    for (s, &b) in right.iter().enumerate() {
        let x0 = BAR_W - (n - s as u32) * (BUTTON + GAP);
        if (x0..x0 + BUTTON).contains(&local_x) {
            return b;
        }
    }
    -1
}

/// Width of the name label between the pin and the right group.
fn label_width(right: usize) -> u32 {
    (BAR_W - right as u32 * (BUTTON + GAP)).saturating_sub(LABEL_X + 8)
}

pub struct Viewer {
    opts: ViewerOptions,
    outputs: Vec<SessionOutput>,
    layout: DesktopLayout,
    primary: usize,
    mode: Mode,
    windows: Vec<Win>,
    modifiers: ModifiersState,
    bar: Bar,
    minimized: bool,
    crop: Rect,
    win_scale: ViewScale,
    drag: Option<Drag>,
    dirty: bool,
    grab: Option<KeyboardGrab>,
    grab_tried: bool,
    grab_on: bool,
    cursors: HashMap<u16, CustomCursor>,
    cursor: CursorState,
    focus_events: u32,
    /// Session mouse buttons currently held (sent down, not yet up).
    held: Vec<MouseButton>,
    /// Clipboard transfer progress shown below the bar, and where it was drawn.
    transfer: Option<(f32, String)>,
    progress_rect: Option<Rect>,
    /// "Keep active" toggle (button 5, Ctrl+Alt+A); the client sends the moves.
    keep_active: bool,
    /// The star was clicked (saved as favorite): it disappears.
    favorite_saved: bool,
    /// Rendered name label and the width it was rendered for.
    label: Option<(u32, std::sync::Arc<[u32]>)>,
    /// Settings menu below the bar: entries (empty = no gear), open, entry under the pointer.
    menu: Vec<MenuItem>,
    menu_open: bool,
    menu_hover: Option<usize>,
    /// Rendered menu and what it was rendered from.
    menu_img: Option<MenuImage>,
}

impl Viewer {
    pub fn new(opts: ViewerOptions, outputs: Vec<SessionOutput>) -> Self {
        let pinned = opts.bar_pinned;
        let keep = opts.keep_active;
        let mode = if opts.start_windowed || opts.test_window {
            Mode::Windowed
        } else {
            Mode::Fullscreen
        };
        let mut v = Self {
            opts,
            outputs: Vec::new(),
            layout: DesktopLayout {
                monitors: Vec::new(),
            },
            primary: 0,
            mode,
            windows: Vec::new(),
            modifiers: ModifiersState::empty(),
            bar: Bar {
                hover: -1,
                pinned,
                shown: pinned,
                ..Default::default()
            },
            minimized: false,
            crop: Rect::new(0, 0, 0, 0),
            win_scale: ViewScale::Native,
            drag: None,
            dirty: true,
            grab: None,
            grab_tried: false,
            grab_on: false,
            cursors: HashMap::new(),
            cursor: CursorState::Default,
            focus_events: 0,
            held: Vec::new(),
            transfer: None,
            progress_rect: None,
            keep_active: keep,
            favorite_saved: false,
            label: None,
            menu: Vec::new(),
            // RDPC_TEST_MENU (screenshots in tests): the settings menu starts open.
            menu_open: std::env::var_os("RDPC_TEST_MENU").is_some(),
            menu_hover: None,
            menu_img: None,
        };
        v.adopt_outputs(outputs);
        v
    }

    fn adopt_outputs(&mut self, outputs: Vec<SessionOutput>) {
        // A new layout restarts the crop at the primary output (same size).
        let size = (self.crop.width(), self.crop.height());
        self.primary = outputs.iter().position(|o| o.primary).unwrap_or(0);
        self.layout = DesktopLayout {
            monitors: outputs
                .iter()
                .map(|o| Monitor {
                    left: o.rect.left as i32,
                    top: o.rect.top as i32,
                    right: o.rect.right as i32,
                    bottom: o.rect.bottom as i32,
                    primary: o.primary,
                })
                .collect(),
        };
        self.outputs = outputs;
        let bb = self.layout.bounding_box();
        let o = self.outputs.get(self.primary).map_or(bb, |o| o.rect);
        let (w, h) = if size.0 == 0 || size.1 == 0 {
            CROP_INITIAL
        } else {
            size
        };
        self.crop = Rect::from_xywh(o.left, o.top, w, h);
        // Keep the crop inside the (possibly smaller) desktop.
        let (w, h) = (
            self.crop.width().min(bb.width()),
            self.crop.height().min(bb.height()),
        );
        let x = self.crop.left.min(bb.width() - w);
        let y = self.crop.top.min(bb.height() - h);
        self.crop = Rect::from_xywh(x, y, w, h);
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn focus_events(&self) -> u32 {
        self.focus_events
    }

    pub fn minimized(&self) -> bool {
        self.minimized
    }

    /// Views of the open windows (for statistics).
    pub fn views(&self) -> Vec<ViewId> {
        self.windows.iter().map(|w| w.view).collect()
    }

    pub fn desktop(&self) -> Rect {
        self.layout.bounding_box()
    }

    /// Whether windows are open.
    pub fn is_open(&self) -> bool {
        !self.windows.is_empty()
    }

    /// Opens the windows of the current mode.
    pub fn open(&mut self, el: &ActiveEventLoop, core: &mut GpuCore) -> Result<()> {
        match self.mode {
            Mode::Fullscreen => {
                let mut order: Vec<usize> = (0..self.outputs.len()).collect();
                order.sort_by_key(|&i| i != self.primary);
                for i in order {
                    self.create_window(el, core, Some(i))?;
                }
            }
            Mode::Windowed => self.create_window(el, core, None)?,
        }
        self.apply_grab_default();
        self.sync_grab()?;
        self.dirty = true;
        Ok(())
    }

    /// New session layout (ResetGraphics): windows are rebuilt in the current mode.
    pub fn set_outputs(
        &mut self,
        el: &ActiveEventLoop,
        core: &mut GpuCore,
        outputs: Vec<SessionOutput>,
    ) -> Result<()> {
        let had_windows = !self.windows.is_empty();
        self.close_all(core);
        self.adopt_outputs(outputs);
        if had_windows {
            self.open(el, core)?;
        }
        Ok(())
    }

    fn close_all(&mut self, core: &mut GpuCore) {
        for w in std::mem::take(&mut self.windows) {
            if let Some(g) = &mut self.grab {
                g.forget(w.window.id());
            }
            let _ = core.destroy_view(w.view);
        }
    }

    pub fn shutdown(&mut self, core: &mut GpuCore) {
        self.close_all(core);
        self.grab = None;
    }

    fn title(&self, output: Option<usize>) -> String {
        let base = match self.opts.label.as_str() {
            "" => self.opts.title.clone(),
            l => format!("{} – {l}", self.opts.title),
        };
        match output {
            Some(i) => format!("{base} {}", self.outputs[i].name),
            None => format!("{base} (Fenstermodus)"),
        }
    }

    fn create_window(
        &mut self,
        el: &ActiveEventLoop,
        core: &mut GpuCore,
        output: Option<usize>,
    ) -> Result<()> {
        let mut attrs = Window::default_attributes().with_title(self.title(output));
        match output {
            Some(i) => {
                let o = &self.outputs[i];
                attrs = attrs
                    .with_fullscreen(Some(Fullscreen::Borderless(o.monitor.clone())))
                    .with_inner_size(PhysicalSize::new(o.rect.width(), o.rect.height()));
            }
            None => {
                let (w, h) = self.windowed_size();
                attrs = attrs.with_inner_size(PhysicalSize::new(w, h));
            }
        }
        {
            use winit::platform::wayland::WindowAttributesExtWayland;
            attrs = if self.opts.test_window {
                attrs.with_name("rdp-replay-test", "").with_active(false)
            } else {
                // app id = desktop entry rmc.desktop (taskbar icon)
                attrs.with_name("rmc", "")
            };
        }
        let window = el.create_window(attrs)?;
        self.apply_cursor_to(&window);
        let viewport = self.viewport_of(output);
        let size = window.inner_size();
        let view = core.create_view(
            window.display_handle()?.as_raw(),
            window.window_handle()?.as_raw(),
            (size.width.max(1), size.height.max(1)),
            viewport,
        )?;
        let scale = if output.is_some() {
            ViewScale::Native
        } else {
            self.win_scale
        };
        core.set_view_scale(view, scale)?;
        self.windows.push(Win {
            window,
            view,
            output,
            cursor: None,
            moves: 0,
            last_point: None,
        });
        Ok(())
    }

    fn viewport_of(&self, output: Option<usize>) -> Rect {
        match output {
            Some(i) => self.outputs[i].rect,
            None if self.win_scale == ViewScale::Native => self.crop,
            None => self.layout.bounding_box(),
        }
    }

    fn windowed_size(&self) -> (u32, u32) {
        match self.win_scale {
            ViewScale::Native => (self.crop.width(), self.crop.height()),
            ViewScale::Fit => {
                let bb = self.layout.bounding_box();
                (bb.width() / 4, bb.height() / 4)
            }
        }
    }

    fn windowed(&self) -> Option<usize> {
        self.windows.iter().position(|w| w.output.is_none())
    }

    fn set_crop_origin(&mut self, core: &mut GpuCore, x: f64, y: f64) -> Result<()> {
        let bb = self.layout.bounding_box();
        let (w, h) = (self.crop.width(), self.crop.height());
        let x = x.clamp(0.0, f64::from(bb.width() - w)) as u32;
        let y = y.clamp(0.0, f64::from(bb.height() - h)) as u32;
        let new = Rect::from_xywh(x, y, w, h);
        if new != self.crop {
            self.crop = new;
            self.apply_windowed_viewport(core)?;
        }
        Ok(())
    }

    fn pan(&mut self, core: &mut GpuCore, dx: f64, dy: f64) -> Result<()> {
        if self.win_scale != ViewScale::Native || self.windowed().is_none() {
            return Ok(());
        }
        let (x, y) = (f64::from(self.crop.left), f64::from(self.crop.top));
        self.set_crop_origin(core, x + dx, y + dy)
    }

    fn resize_crop(&mut self, core: &mut GpuCore, w: u32, h: u32) -> Result<()> {
        let bb = self.layout.bounding_box();
        let (w, h) = (w.min(bb.width()), h.min(bb.height()));
        let x = self.crop.left.min(bb.width() - w);
        let y = self.crop.top.min(bb.height() - h);
        self.crop = Rect::from_xywh(x, y, w, h);
        self.apply_windowed_viewport(core)
    }

    fn apply_windowed_viewport(&mut self, core: &mut GpuCore) -> Result<()> {
        if let Some(i) = self.windowed() {
            let (view, vp, scale) = (self.windows[i].view, self.viewport_of(None), self.win_scale);
            core.set_view_viewport(view, vp)?;
            core.set_view_scale(view, scale)?;
            self.dirty = true;
        }
        Ok(())
    }

    fn thumbs(&self, win: &Win) -> [Option<Rect>; 2] {
        if win.output.is_some() || self.win_scale != ViewScale::Native {
            return [None, None];
        }
        let bb = self.layout.bounding_box();
        let size = win.window.inner_size();
        let (vw, vh) = (size.width, size.height);
        let c = self.crop;
        let h = (c.width() < bb.width() && vw > 3 * THUMB).then(|| {
            let track = f64::from(vw - THUMB);
            let x0 = f64::from(c.left) / f64::from(bb.width()) * track;
            let len = (f64::from(c.width()) / f64::from(bb.width()) * track).max(24.0);
            Rect::new(x0 as u32, vh - THUMB, (x0 + len) as u32, vh)
        });
        let v = (c.height() < bb.height() && vh > 3 * THUMB).then(|| {
            let track = f64::from(vh - THUMB);
            let y0 = f64::from(c.top) / f64::from(bb.height()) * track;
            let len = (f64::from(c.height()) / f64::from(bb.height()) * track).max(24.0);
            Rect::new(vw - THUMB, y0 as u32, vw, (y0 + len) as u32)
        });
        [h, v]
    }

    /// Fullscreen <-> windowed without touching the session.
    pub fn toggle_mode(&mut self, el: &ActiveEventLoop, core: &mut GpuCore) -> Result<()> {
        debug(|| {
            format!(
                "toggle_mode aus {:?}: Fenstergrößen {:?}, Modifikatoren {:?}",
                self.mode,
                self.windows
                    .iter()
                    .map(|w| w.window.inner_size())
                    .collect::<Vec<_>>(),
                self.modifiers
            )
        });
        if self.opts.test_window {
            println!("Testfenster: Vollbild ist gesperrt");
            return Ok(());
        }
        let t = Instant::now();
        let keep = self.bar_window().unwrap_or(0);
        let mut kept = None;
        for (i, w) in std::mem::take(&mut self.windows).into_iter().enumerate() {
            if i == keep {
                kept = Some(w);
            } else {
                if let Some(g) = &mut self.grab {
                    g.forget(w.window.id());
                }
                core.destroy_view(w.view)?;
                drop(w);
            }
        }
        let mut w = kept.context("no window to keep")?;
        match self.mode {
            Mode::Fullscreen => {
                self.mode = Mode::Windowed;
                let (ww, wh) = self.windowed_size();
                w.window.set_fullscreen(None);
                // KWin otherwise restores a maximised geometry and ignores the size request.
                w.window.set_maximized(false);
                let _ = w.window.request_inner_size(PhysicalSize::new(ww, wh));
                w.window.set_title(&self.title(None));
                w.output = None;
                self.windows.push(w);
                self.apply_windowed_viewport(core)?;
            }
            Mode::Windowed => {
                self.mode = Mode::Fullscreen;
                let p = self.primary;
                w.window.set_fullscreen(Some(Fullscreen::Borderless(
                    self.outputs[p].monitor.clone(),
                )));
                w.window.set_title(&self.title(Some(p)));
                w.output = Some(p);
                core.set_view_viewport(w.view, self.outputs[p].rect)?;
                core.set_view_scale(w.view, ViewScale::Native)?;
                self.windows.push(w);
                for i in (0..self.outputs.len()).filter(|&i| i != p) {
                    self.create_window(el, core, Some(i))?;
                }
            }
        }
        self.apply_grab_default();
        self.sync_grab()?;
        self.dirty = true;
        println!(
            "Wechsel zu {:?} in {:.1} ms (Session unverändert)",
            self.mode,
            t.elapsed().as_secs_f64() * 1000.0
        );
        Ok(())
    }

    // ---- keyboard grab ---------------------------------------------------------------------

    fn apply_grab_default(&mut self) {
        if self.opts.grab_in_fullscreen && !self.opts.test_window {
            self.grab_on = self.mode == Mode::Fullscreen;
        }
    }

    fn sync_grab(&mut self) -> Result<()> {
        if self.grab.is_none() && self.grab_on && !self.grab_tried {
            self.grab_tried = true;
            if let Some(w) = self.windows.first() {
                match KeyboardGrab::new(w.window.display_handle()?.as_raw()) {
                    Ok(g) => self.grab = Some(g),
                    Err(e) => eprintln!("Tastatur-Grab nicht verfügbar: {e:#}"),
                }
            }
        }
        if let Some(g) = &mut self.grab {
            for w in &self.windows {
                g.set(
                    w.window.id(),
                    w.window.window_handle()?.as_raw(),
                    self.grab_on,
                )?;
            }
        }
        self.dirty = true;
        Ok(())
    }

    pub fn grab_active(&mut self) -> usize {
        self.grab.as_mut().map_or(0, |g| g.poll())
    }

    pub fn grab_on(&self) -> bool {
        self.grab_on
    }

    fn toggle_grab(&mut self) -> Result<()> {
        self.grab_on = !self.grab_on;
        self.sync_grab()?;
        println!("Tastatur-Grab {}", if self.grab_on { "an" } else { "aus" });
        Ok(())
    }

    // ---- cursor ------------------------------------------------------------------------------

    /// Pointer shape from the server.
    pub fn set_cursor(&mut self, el: &ActiveEventLoop, update: CursorUpdate) -> Result<()> {
        self.cursor = match update {
            CursorUpdate::Set { index, shape } => {
                let src = CustomCursor::from_rgba(
                    shape.rgba,
                    shape.width,
                    shape.height,
                    shape.hot_x.min(shape.width.saturating_sub(1)),
                    shape.hot_y.min(shape.height.saturating_sub(1)),
                )?;
                let c = el.create_custom_cursor(src);
                self.cursors.insert(index, c.clone());
                CursorState::Custom(c)
            }
            CursorUpdate::Cached { index } => match self.cursors.get(&index) {
                Some(c) => CursorState::Custom(c.clone()),
                None => return Ok(()),
            },
            CursorUpdate::Hidden => CursorState::Hidden,
            CursorUpdate::Default => CursorState::Default,
        };
        for w in &self.windows {
            self.apply_cursor_to(&w.window);
        }
        Ok(())
    }

    fn apply_cursor_to(&self, window: &Window) {
        match &self.cursor {
            CursorState::Default => {
                window.set_cursor_visible(true);
                window.set_cursor(CursorIcon::Default);
            }
            CursorState::Hidden => window.set_cursor_visible(false),
            CursorState::Custom(c) => {
                window.set_cursor_visible(true);
                window.set_cursor(c.clone());
            }
        }
    }

    // ---- overlay, present ------------------------------------------------------------------

    fn bar_window(&self) -> Option<usize> {
        self.windows
            .iter()
            .position(|w| w.output.is_none_or(|o| o == self.primary))
    }

    fn bar_rect(&self, win: &Win) -> Rect {
        let w = win.window.inner_size().width;
        Rect::from_xywh(w.saturating_sub(BAR_W) / 2, 0, BAR_W, BAR_H)
    }

    fn show_bar(&mut self) {
        self.bar.shown = true;
        self.bar.hide_at = Some(Instant::now() + BAR_HIDE);
        self.dirty = true;
    }

    /// Progress of a clipboard transfer (0..1 and a short text such as "42%  0:35"),
    /// None = no transfer: shown below the bar.
    pub fn set_transfer(&mut self, progress: Option<(f32, String)>) {
        let changed = match (&self.transfer, &progress) {
            (None, None) => false,
            (Some(a), Some(b)) => (a.0 - b.0).abs() >= 0.005 || a.1 != b.1,
            _ => true,
        };
        if changed {
            self.transfer = progress;
            self.dirty = true;
        }
    }

    /// "Keep active" on: the client sends small mouse moves regularly.
    pub fn keep_active(&self) -> bool {
        self.keep_active
    }

    fn toggle_keep_active(&mut self) {
        self.keep_active = !self.keep_active;
        self.dirty = true;
        println!(
            "Aktiv halten: {}",
            if self.keep_active { "an" } else { "aus" }
        );
    }

    /// Whether the connection bar is pinned (for remembering the choice).
    pub fn bar_pinned(&self) -> bool {
        self.bar.pinned
    }

    /// Right button group of the bar, left to right: star (only while the connection is no
    /// favorite), keep active, keyboard grab, then the window buttons.
    fn right_buttons(&self) -> Vec<i32> {
        let mut b = Vec::with_capacity(7);
        if self.opts.offer_favorite && !self.favorite_saved {
            b.push(B_STAR);
        }
        if !self.menu.is_empty() {
            b.push(B_GEAR);
        }
        b.extend([B_KEEP, B_GRAB, B_MINIMISE, B_WINDOW, B_CLOSE]);
        b
    }

    /// Replaces the entries of the settings menu (empty = no gear button).
    pub fn set_menu(&mut self, items: Vec<MenuItem>) {
        if self.menu != items {
            if items.is_empty() {
                self.menu_open = false;
            }
            self.menu = items;
            self.label = None; // the label width depends on the button count
            self.dirty = true;
        }
    }

    pub fn menu_open(&self) -> bool {
        self.menu_open
    }

    fn close_menu(&mut self) {
        if self.menu_open {
            self.menu_open = false;
            self.menu_hover = None;
            self.dirty = true;
        }
    }

    /// Heights of the menu rows.
    fn menu_rows(&self) -> Vec<u32> {
        self.menu
            .iter()
            .map(|m| match m.kind {
                MenuKind::Separator => MENU_SEP,
                MenuKind::Header => MENU_HEAD,
                _ => MENU_ROW,
            })
            .collect()
    }

    fn menu_height(&self) -> u32 {
        self.menu_rows().iter().sum::<u32>() + 2 * MENU_PAD
    }

    /// Where the menu is drawn in the bar's window: below the bar (and the transfer strip),
    /// right edge under the gear button.
    fn menu_rect(&self, win: &Win) -> Rect {
        let bar = self.bar_rect(win);
        let right = self.right_buttons();
        let n = right.len() as u32;
        let slot = right.iter().position(|&b| b == B_GEAR).unwrap_or(0) as u32;
        let gear_right = bar.left + BAR_W - (n - slot) * (BUTTON + GAP) + BUTTON;
        let w = win.window.inner_size().width;
        let left = gear_right
            .saturating_sub(MENU_W)
            .min(w.saturating_sub(MENU_W));
        let top = match self.progress_rect {
            Some(p) => p.bottom + 4,
            None => bar.bottom + 4,
        };
        Rect::from_xywh(left, top, MENU_W, self.menu_height())
    }

    /// Menu entry at a position inside the menu rectangle.
    fn menu_item_at(&self, r: Rect, x: u32, y: u32) -> Option<usize> {
        if !(r.left..r.right).contains(&x) || !(r.top..r.bottom).contains(&y) {
            return None;
        }
        let mut top = r.top + MENU_PAD;
        for (i, h) in self.menu_rows().into_iter().enumerate() {
            if (top..top + h).contains(&y) {
                return Some(i);
            }
            top += h;
        }
        None
    }

    fn menu_image(&mut self) -> std::sync::Arc<[u32]> {
        if let Some((items, hover, px)) = &self.menu_img
            && *items == self.menu
            && *hover == self.menu_hover
        {
            return px.clone();
        }
        let px = render_menu(
            &self.menu,
            &self.menu_rows(),
            self.menu_hover,
            self.menu_height(),
        );
        self.menu_img = Some((self.menu.clone(), self.menu_hover, px.clone()));
        px
    }

    /// The name label picture for the current button group (rendered once per width).
    fn label_image(&mut self) -> (u32, std::sync::Arc<[u32]>) {
        let w = label_width(self.right_buttons().len());
        match &self.label {
            Some((lw, px)) if *lw == w => (w, px.clone()),
            _ => {
                let px = crate::text::render(
                    &self.opts.label,
                    LABEL_SIZE,
                    w,
                    BUTTON,
                    BAR_FG,
                    BAR_BG,
                    crate::text::Align::Center,
                );
                self.label = Some((w, px.clone()));
                (w, px)
            }
        }
    }

    /// Bar auto-hide and overlay state; call once per loop iteration.
    pub fn tick(&mut self, core: &mut GpuCore) -> Result<()> {
        if self.bar.shown
            && !self.bar.pinned
            && !self.menu_open
            && self.bar.hide_at.is_some_and(|t| Instant::now() >= t)
        {
            self.bar.shown = false;
            self.dirty = true;
        }
        let bw = self.bar_window();
        let right = self.right_buttons();
        let row = ButtonRow::new(&right.iter().map(|&b| b as u32).collect::<Vec<_>>());
        let label = (!self.opts.label.is_empty()).then(|| self.label_image());
        let menu = (self.menu_open && self.bar.shown).then(|| self.menu_image());
        for i in 0..self.windows.len() {
            let thumbs = self.thumbs(&self.windows[i]);
            // transfer progress: a strip right below the bar (at the top edge if hidden)
            let progress = (Some(i) == bw)
                .then(|| self.transfer.clone())
                .flatten()
                .map(|(f, text)| {
                    let r = self.bar_rect(&self.windows[i]);
                    let top = if self.bar.shown { r.bottom + 2 } else { 0 };
                    let mut t = [b' '; 16];
                    for (d, s) in t.iter_mut().zip(text.bytes()) {
                        *d = s;
                    }
                    rdpgfx_gpu::core::OverlayProgress {
                        rect: Rect::new(r.left, top, r.right, top + PROGRESS_H),
                        permyriad: (f.clamp(0.0, 1.0) * 10000.0) as u32,
                        text: t,
                    }
                });
            if Some(i) == bw {
                self.progress_rect = progress.map(|p| p.rect);
            }
            let bar = (Some(i) == bw && self.bar.shown).then(|| self.bar_rect(&self.windows[i]));
            let mut images = match (bar, &label) {
                (Some(r), Some((w, px))) => vec![OverlayImage {
                    x: r.left + LABEL_X,
                    y: r.top + 4,
                    width: *w,
                    height: BUTTON,
                    pixels: px.clone(),
                }],
                _ => Vec::new(),
            };
            if bar.is_some()
                && let Some(px) = &menu
            {
                let r = self.menu_rect(&self.windows[i]);
                images.push(OverlayImage {
                    x: r.left,
                    y: r.top,
                    width: r.width(),
                    height: r.height(),
                    pixels: px.clone(),
                });
            }
            core.set_view_images(self.windows[i].view, images)?;
            let state = OverlayState {
                bar,
                pinned: self.bar.pinned,
                hover: if Some(i) == bw { self.bar.hover } else { -1 },
                grab: self.grab_on,
                keep_active: self.keep_active,
                thumbs,
                progress,
                buttons: row,
            };
            core.set_view_overlay(self.windows[i].view, state)?;
        }
        Ok(())
    }

    /// Something visible changed (overlay, window, layout) and needs a present.
    pub fn needs_present(&self) -> bool {
        self.dirty && !self.minimized
    }

    pub fn presented(&mut self) {
        self.dirty = false;
    }

    /// Diagnostics: pointer moves per window since the last call and the last session point.
    pub fn pointer_stats(&mut self) -> String {
        let mut parts = Vec::new();
        for (i, w) in self.windows.iter_mut().enumerate() {
            let name = w
                .output
                .map_or("Fenster".to_string(), |o| self.outputs[o].name.clone());
            parts.push(format!("{i} {name}: {} {:?}", w.moves, w.last_point));
            w.moves = 0;
        }
        parts.join(", ")
    }

    /// Diagnostics: presents per window since the last call (count, not acquired, present
    /// results other than success), viewport, minimised/stale flags.
    pub fn present_stats(&self, core: &GpuCore) -> String {
        self.windows
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let name = w
                    .output
                    .map_or("Fenster".to_string(), |o| self.outputs[o].name.clone());
                match core.take_view_diag(w.view) {
                    Some(d) => format!(
                        "{i} {name}: {} Ausgaben, {} nicht erhalten, {} Rückmeldungen, \
                         Ausschnitt {},{}-{},{}{}{}",
                        d.presents,
                        d.not_acquired,
                        d.present_errors,
                        d.viewport.left,
                        d.viewport.top,
                        d.viewport.right,
                        d.viewport.bottom,
                        if d.minimized { " minimiert" } else { "" },
                        if d.stale { " veraltet" } else { "" }
                    ),
                    None => format!("{i} {name}: keine Ansicht"),
                }
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Diagnostics: swapchain sizes of the views (after a present).
    pub fn debug_views(&self, core: &GpuCore) {
        debug(|| {
            format!(
                "Ansichten: {:?}",
                self.windows
                    .iter()
                    .map(|w| (
                        core.view_info(w.view).map(|i| i.0),
                        w.window.inner_size(),
                        w.window.fullscreen().is_some()
                    ))
                    .collect::<Vec<_>>()
            )
        });
    }

    fn set_minimized(&mut self, core: &mut GpuCore, on: bool) -> Result<()> {
        self.minimized = on;
        for w in &self.windows {
            if on {
                w.window.set_minimized(true);
            }
            core.set_view_minimized(w.view, on)?;
        }
        self.dirty = true;
        Ok(())
    }

    fn session_point(
        &self,
        core: &GpuCore,
        wi: usize,
        pos: PhysicalPosition<f64>,
    ) -> Option<(u32, u32)> {
        core.view_transform(self.windows[wi].view)
            .ok()?
            .view_to_session(pos.x, pos.y)
    }

    /// Like `session_point`, but a position outside the view (a button drag that left the
    /// window keeps the implicit grab on Wayland) continues into the neighbouring monitor
    /// and is clamped onto the nearest monitor instead of being dropped.
    fn session_point_free(
        &self,
        core: &GpuCore,
        wi: usize,
        pos: PhysicalPosition<f64>,
    ) -> Option<(u32, u32)> {
        let t = core.view_transform(self.windows[wi].view).ok()?;
        let (x, y) = t.view_to_session_unbounded(pos.x, pos.y);
        self.layout.clamp_to_monitors(x, y)
    }

    fn output_name(&self, wi: usize) -> String {
        self.windows[wi]
            .output
            .map_or("Fenstermodus".to_string(), |o| self.outputs[o].name.clone())
    }

    /// Handles a window event; returns input for the session and other actions.
    pub fn handle(
        &mut self,
        el: &ActiveEventLoop,
        core: &mut GpuCore,
        id: WindowId,
        event: WindowEvent,
    ) -> Result<Vec<Action>> {
        let mut out = Vec::new();
        let Some(wi) = self.windows.iter().position(|w| w.window.id() == id) else {
            return Ok(out);
        };
        let hot = self.modifiers.control_key() && self.modifiers.alt_key();
        match event {
            WindowEvent::CloseRequested => out.push(Action::Quit),
            WindowEvent::Resized(size) => {
                let (w, h) = (size.width.max(1), size.height.max(1));
                debug(|| {
                    format!(
                        "Resized {w}x{h}, Fenster {} ({:?}), Modus {:?}, Ausschnitt {:?}",
                        wi,
                        self.windows[wi]
                            .output
                            .map(|o| self.outputs[o].name.clone()),
                        self.mode,
                        self.crop
                    )
                });
                core.set_view_size(self.windows[wi].view, w, h)?;
                if self.windows[wi].output.is_none() && self.win_scale == ViewScale::Native {
                    self.resize_crop(core, w, h)?;
                }
                self.dirty = true;
            }
            WindowEvent::Focused(focused) => {
                if focused && self.minimized {
                    self.set_minimized(core, false)?;
                }
                if focused && self.opts.test_window {
                    self.focus_events += 1;
                    println!("Hinweis: Testfenster hat den Fokus bekommen");
                }
                if !focused {
                    out.push(Action::Input(Input::ReleaseAll));
                }
            }
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                let mut consumed = false;
                if self.menu_open && event.logical_key == Key::Named(NamedKey::Escape) {
                    if pressed {
                        self.close_menu();
                    }
                    return Ok(out);
                }
                if hot && pressed && !event.repeat {
                    consumed = true;
                    match event.logical_key.as_ref() {
                        Key::Named(NamedKey::Enter) => self.toggle_mode(el, core)?,
                        Key::Named(NamedKey::ArrowLeft) => self.pan(core, -PAN_STEP, 0.0)?,
                        Key::Named(NamedKey::ArrowRight) => self.pan(core, PAN_STEP, 0.0)?,
                        Key::Named(NamedKey::ArrowUp) => self.pan(core, 0.0, -PAN_STEP)?,
                        Key::Named(NamedKey::ArrowDown) => self.pan(core, 0.0, PAN_STEP)?,
                        Key::Named(NamedKey::Space) => out.push(Action::Pause),
                        Key::Character(c) => match c.to_lowercase().as_str() {
                            "m" => self.set_minimized(core, true)?,
                            "s" if self.mode == Mode::Windowed => {
                                self.win_scale = match self.win_scale {
                                    ViewScale::Native => ViewScale::Fit,
                                    ViewScale::Fit => ViewScale::Native,
                                };
                                let (w, h) = self.windowed_size();
                                if let Some(i) = self.windowed() {
                                    let _ = self.windows[i]
                                        .window
                                        .request_inner_size(PhysicalSize::new(w, h));
                                }
                                self.apply_windowed_viewport(core)?;
                                println!("Fenstermodus: {:?}", self.win_scale);
                            }
                            "l" => {
                                if self.bar.shown {
                                    self.bar.shown = false;
                                    self.dirty = true;
                                } else {
                                    self.show_bar();
                                }
                            }
                            "p" => {
                                self.bar.pinned = !self.bar.pinned;
                                self.show_bar();
                            }
                            "k" => self.toggle_grab()?,
                            "a" => self.toggle_keep_active(),
                            "q" => out.push(Action::Quit),
                            _ => consumed = false,
                        },
                        _ => consumed = false,
                    }
                }
                if !consumed && let PhysicalKey::Code(code) = event.physical_key {
                    out.push(Action::Input(Input::Key {
                        code,
                        down: pressed,
                    }));
                }
            }
            WindowEvent::CursorEntered { .. } | WindowEvent::CursorLeft { .. } => {
                let entered = matches!(event, WindowEvent::CursorEntered { .. });
                debug(|| {
                    format!(
                        "Zeiger {} Fenster {wi} ({})",
                        if entered { "betritt" } else { "verlässt" },
                        self.output_name(wi)
                    )
                });
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.windows[wi].cursor = Some(position);
                self.windows[wi].moves += 1;
                if let Some(d) = self.drag {
                    let (dx, dy) = (position.x - d.start.0, position.y - d.start.1);
                    let (ox, oy) = (f64::from(d.origin.0), f64::from(d.origin.1));
                    let size = self.windows[wi].window.inner_size();
                    let bb = self.layout.bounding_box();
                    match d.kind {
                        DragKind::Pan => self.set_crop_origin(core, ox - dx, oy - dy)?,
                        DragKind::Thumb(0) => {
                            let k = f64::from(bb.width()) / f64::from(size.width - THUMB);
                            self.set_crop_origin(core, ox + dx * k, oy)?;
                        }
                        DragKind::Thumb(_) => {
                            let k = f64::from(bb.height()) / f64::from(size.height - THUMB);
                            self.set_crop_origin(core, ox, oy + dy * k)?;
                        }
                    }
                    return Ok(out);
                }
                let mut over_bar = false;
                if Some(wi) == self.bar_window() {
                    let r = self.bar_rect(&self.windows[wi]);
                    let (x, y) = (position.x.max(0.0) as u32, position.y.max(0.0) as u32);
                    over_bar = self.bar.shown
                        && (r.left..r.right).contains(&x)
                        && (r.top..r.bottom).contains(&y);
                    if self.menu_open {
                        let mr = self.menu_rect(&self.windows[wi]);
                        let hover = self.menu_item_at(mr, x, y).filter(|&i| {
                            matches!(self.menu[i].kind, MenuKind::Toggle(_) | MenuKind::Action)
                        });
                        if hover != self.menu_hover {
                            self.menu_hover = hover;
                            self.dirty = true;
                        }
                        over_bar |=
                            (mr.left..mr.right).contains(&x) && (mr.top..mr.bottom).contains(&y);
                    }
                    if y == 0 && !self.bar.shown {
                        self.show_bar(); // edge push at the top of the primary output
                    }
                    let hover = if over_bar {
                        self.bar.hide_at = Some(Instant::now() + BAR_HIDE);
                        bar_button(x - r.left, y - r.top, &self.right_buttons())
                    } else {
                        -1
                    };
                    if hover != self.bar.hover {
                        self.bar.hover = hover;
                        self.dirty = true;
                    }
                }
                let p = if self.held.is_empty() {
                    self.session_point(core, wi, position)
                } else {
                    self.session_point_free(core, wi, position)
                };
                self.windows[wi].last_point = p;
                if !over_bar && let Some((x, y)) = p {
                    out.push(Action::Input(Input::Move { x, y }));
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(pos) = self.windows[wi].cursor else {
                    return Ok(out);
                };
                let down = state == ElementState::Pressed;
                if button == MouseButton::Left && !down && self.drag.take().is_some() {
                    return Ok(out);
                }
                if button == MouseButton::Left && down {
                    // Cancel button of the transfer progress strip (its right end).
                    if Some(wi) == self.bar_window()
                        && let Some(r) = self.progress_rect
                    {
                        let (x, y) = (pos.x.max(0.0) as u32, pos.y.max(0.0) as u32);
                        if (r.right.saturating_sub(PROGRESS_CANCEL_W)..r.right).contains(&x)
                            && (r.top..r.bottom).contains(&y)
                        {
                            out.push(Action::CancelTransfer);
                            return Ok(out);
                        }
                    }
                    // Open settings menu: clicks inside choose an entry, clicks outside the menu
                    // and the bar only close it.
                    if self.menu_open {
                        let (x, y) = (pos.x.max(0.0) as u32, pos.y.max(0.0) as u32);
                        let on_bar = Some(wi) == self.bar_window() && {
                            let r = self.bar_rect(&self.windows[wi]);
                            (r.left..r.right).contains(&x) && (r.top..r.bottom).contains(&y)
                        };
                        if !on_bar {
                            let mr = (Some(wi) == self.bar_window())
                                .then(|| self.menu_rect(&self.windows[wi]));
                            match mr.and_then(|r| self.menu_item_at(r, x, y)) {
                                Some(i) => match self.menu[i].kind {
                                    MenuKind::Toggle(_) => out.push(Action::Menu(self.menu[i].id)),
                                    MenuKind::Action => {
                                        out.push(Action::Menu(self.menu[i].id));
                                        self.close_menu();
                                    }
                                    _ => {}
                                },
                                None => {
                                    let inside = mr.is_some_and(|r| {
                                        (r.left..r.right).contains(&x)
                                            && (r.top..r.bottom).contains(&y)
                                    });
                                    if !inside {
                                        self.close_menu();
                                    }
                                }
                            }
                            return Ok(out);
                        }
                    }
                    // Overlay first (input routing).
                    if Some(wi) == self.bar_window() && self.bar.shown {
                        let r = self.bar_rect(&self.windows[wi]);
                        let (x, y) = (pos.x.max(0.0) as u32, pos.y.max(0.0) as u32);
                        if (r.left..r.right).contains(&x) && (r.top..r.bottom).contains(&y) {
                            match bar_button(x - r.left, y - r.top, &self.right_buttons()) {
                                B_PIN => {
                                    self.bar.pinned = !self.bar.pinned;
                                    self.dirty = true;
                                }
                                B_MINIMISE => self.set_minimized(core, true)?,
                                B_WINDOW => self.toggle_mode(el, core)?,
                                B_GRAB => self.toggle_grab()?,
                                B_CLOSE => out.push(Action::Quit),
                                B_KEEP => self.toggle_keep_active(),
                                B_STAR => {
                                    self.favorite_saved = true;
                                    self.bar.hover = -1;
                                    self.label = None;
                                    self.dirty = true;
                                    out.push(Action::SaveFavorite);
                                }
                                B_GEAR => {
                                    self.menu_open = !self.menu_open;
                                    self.menu_hover = None;
                                    self.dirty = true;
                                }
                                _ => {}
                            }
                            return Ok(out);
                        }
                    }
                    let origin = (self.crop.left, self.crop.top);
                    let start = (pos.x, pos.y);
                    let (px, py) = (pos.x.max(0.0) as u32, pos.y.max(0.0) as u32);
                    let on_thumb = self.thumbs(&self.windows[wi]).iter().position(|t| {
                        t.is_some_and(|r| {
                            (r.left..r.right).contains(&px) && (r.top..r.bottom).contains(&py)
                        })
                    });
                    if let Some(k) = on_thumb {
                        self.drag = Some(Drag {
                            kind: DragKind::Thumb(k),
                            start,
                            origin,
                        });
                        return Ok(out);
                    }
                    if hot && self.windows[wi].output.is_none() {
                        self.drag = Some(Drag {
                            kind: DragKind::Pan,
                            start,
                            origin,
                        });
                        return Ok(out);
                    }
                    out.push(Action::Clicked {
                        output: self.output_name(wi),
                        view: (pos.x, pos.y),
                        session: self.session_point(core, wi, pos),
                    });
                }
                // A release must always reach the session, even outside every view, or the
                // button stays down in Windows (window drags then snap at the monitor edge).
                let p = if down {
                    self.session_point(core, wi, pos)
                } else {
                    self.held.retain(|b| *b != button);
                    self.session_point_free(core, wi, pos)
                };
                if let Some((x, y)) = p {
                    if down && !self.held.contains(&button) {
                        self.held.push(button);
                    }
                    out.push(Action::Input(Input::Button { button, down, x, y }));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (fx, fy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (f64::from(x), f64::from(y)),
                    MouseScrollDelta::PixelDelta(p) => (p.x / 40.0, p.y / 40.0),
                };
                if hot {
                    let (mut dx, mut dy) = (-fx * PAN_STEP, -fy * PAN_STEP);
                    if self.modifiers.shift_key() {
                        std::mem::swap(&mut dx, &mut dy);
                    }
                    self.pan(core, dx, dy)?;
                } else if let Some(pos) = self.windows[wi].cursor
                    && let Some((x, y)) = self.session_point(core, wi, pos)
                {
                    let (dx, dy) = (fx.round() as i32, fy.round() as i32);
                    if dx != 0 || dy != 0 {
                        out.push(Action::Input(Input::Wheel { dx, dy, x, y }));
                    }
                }
            }
            _ => {}
        }
        Ok(out)
    }
}

/// Diagnostics on stdout when `RDP_VIEWER_DEBUG` is set.
/// Draws the settings menu (opaque RGBA, see OverlayImage).
fn render_menu(
    items: &[MenuItem],
    rows: &[u32],
    hover: Option<usize>,
    h: u32,
) -> std::sync::Arc<[u32]> {
    use crate::text::{self, Align, pack};
    const BG: text::Rgb = [38, 38, 41];
    const EDGE: text::Rgb = [78, 78, 84];
    const HOVER: text::Rgb = [60, 60, 66];
    const FG: text::Rgb = [235, 235, 235];
    const WEAK: text::Rgb = [150, 150, 158];
    const ACCENT: text::Rgb = [61, 140, 252];
    const OFF: text::Rgb = [92, 92, 98];
    const DONE: text::Rgb = [80, 190, 110];
    let w = MENU_W;
    let mut px = vec![pack(BG); (w * h) as usize];
    let fill = |px: &mut [u32], x0: u32, y0: u32, x1: u32, y1: u32, c: text::Rgb| {
        for y in y0..y1.min(h) {
            for x in x0..x1.min(w) {
                px[(y * w + x) as usize] = pack(c);
            }
        }
    };
    // border
    fill(&mut px, 0, 0, w, 1, EDGE);
    fill(&mut px, 0, h - 1, w, h, EDGE);
    fill(&mut px, 0, 0, 1, h, EDGE);
    fill(&mut px, w - 1, 0, w, h, EDGE);
    // rounded discs and pills, antialiased against `bg`
    let blend = |a: text::Rgb, b: text::Rgb, t: f32| -> text::Rgb {
        std::array::from_fn(|i| {
            (f32::from(a[i]) + (f32::from(b[i]) - f32::from(a[i])) * t).round() as u8
        })
    };
    let pill = |px: &mut [u32], x0: f32, y0: f32, pw: f32, ph: f32, c: text::Rgb, bg: text::Rgb| {
        let r = ph / 2.0;
        for y in y0.floor() as u32..(y0 + ph).ceil() as u32 {
            for x in x0.floor() as u32..(x0 + pw).ceil() as u32 {
                let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
                let cx = fx.clamp(x0 + r, x0 + pw - r);
                let d = ((fx - cx).powi(2) + (fy - (y0 + r)).powi(2)).sqrt();
                let cov = (r - d + 0.5).clamp(0.0, 1.0);
                if cov > 0.0 && x < w && y < h {
                    px[(y * w + x) as usize] = pack(blend(bg, c, cov));
                }
            }
        }
    };
    let mut top = MENU_PAD;
    for (i, (m, &rh)) in items.iter().zip(rows).enumerate() {
        let row_bg = if hover == Some(i) { HOVER } else { BG };
        if hover == Some(i) {
            fill(&mut px, 4, top, w - 4, top + rh, HOVER);
        }
        let tx = 14;
        match m.kind {
            MenuKind::Separator => fill(&mut px, 10, top + rh / 2, w - 10, top + rh / 2 + 1, EDGE),
            MenuKind::Header => text::draw(
                &mut px,
                w,
                h,
                tx,
                top + 6,
                w - 2 * tx,
                rh - 6,
                &m.label,
                12.5,
                WEAK,
                row_bg,
                Align::Left,
            ),
            MenuKind::Action => text::draw(
                &mut px,
                w,
                h,
                tx,
                top,
                w - 2 * tx,
                rh,
                &m.label,
                14.5,
                FG,
                row_bg,
                Align::Left,
            ),
            MenuKind::Done => {
                text::draw(
                    &mut px,
                    w,
                    h,
                    tx,
                    top,
                    w - 2 * tx - 20,
                    rh,
                    &m.label,
                    14.5,
                    WEAK,
                    row_bg,
                    Align::Left,
                );
                pill(
                    &mut px,
                    (w - 24) as f32,
                    (top + rh / 2) as f32 - 4.0,
                    8.0,
                    8.0,
                    DONE,
                    row_bg,
                );
            }
            MenuKind::Toggle(on) => {
                text::draw(
                    &mut px,
                    w,
                    h,
                    tx,
                    top,
                    w - 2 * tx - 44,
                    rh,
                    &m.label,
                    14.5,
                    FG,
                    row_bg,
                    Align::Left,
                );
                let (sw, sh) = (34.0, 18.0);
                let sx = (w - 14) as f32 - sw;
                let sy = (top + rh / 2) as f32 - sh / 2.0;
                pill(
                    &mut px,
                    sx,
                    sy,
                    sw,
                    sh,
                    if on { ACCENT } else { OFF },
                    row_bg,
                );
                let knob_x = if on { sx + sw - sh + 2.0 } else { sx + 2.0 };
                pill(
                    &mut px,
                    knob_x,
                    sy + 2.0,
                    sh - 4.0,
                    sh - 4.0,
                    FG,
                    if on { ACCENT } else { OFF },
                );
            }
        }
        top += rh;
    }
    px.into()
}

fn debug(msg: impl FnOnce() -> String) {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    if *ON.get_or_init(|| std::env::var_os("RDP_VIEWER_DEBUG").is_some()) {
        println!("[viewer] {}", msg());
    }
}

/// Local monitor handle by connector name (winit reports e.g. "DP-5" on Wayland).
pub fn monitor_by_name(el: &ActiveEventLoop, name: &str) -> Option<MonitorHandle> {
    el.available_monitors()
        .find(|m| m.name().as_deref() == Some(name))
}
