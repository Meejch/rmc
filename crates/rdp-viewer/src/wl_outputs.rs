//! Local monitors as the Wayland compositor sees them: name, logical position, pixel size
//! after rotation, scale, refresh, and KDE's output order (the first one is the primary).
//!
//! winit reports the unrotated mode size for rotated outputs (portrait monitor: 2560x1440
//! instead of 1440x2560), so the layout sent to the server comes from here instead.

use std::collections::HashMap;

use anyhow::{Context, Result};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_output::{self, Transform, WlOutput};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_manager_v1::ZxdgOutputManagerV1;
use wayland_protocols::xdg::xdg_output::zv1::client::zxdg_output_v1::{self, ZxdgOutputV1};
use wayland_protocols_plasma::output_order::v1::client::kde_output_order_v1::{
    self, KdeOutputOrderV1,
};

/// One local output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalOutput {
    pub name: String,
    /// Logical position in the compositor's global space.
    pub x: i32,
    pub y: i32,
    /// Size in pixels after rotation (what the monitor shows).
    pub width: u32,
    pub height: u32,
    pub scale: i32,
    pub refresh_mhz: i32,
    /// Rotated by 90° or 270°.
    pub portrait: bool,
    /// Position in KDE's output order (0 = primary), if the compositor reports it.
    pub order: Option<usize>,
}

#[derive(Default)]
struct Out {
    name: Option<String>,
    transform: Option<Transform>,
    mode: (i32, i32),
    refresh: i32,
    scale: i32,
    logical_pos: Option<(i32, i32)>,
}

#[derive(Default)]
struct State {
    outs: HashMap<usize, Out>,
    order: Vec<String>,
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

impl Dispatch<WlOutput, usize> for State {
    fn event(
        s: &mut Self,
        _: &WlOutput,
        ev: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let o = s.outs.entry(*idx).or_default();
        match ev {
            wl_output::Event::Geometry {
                transform: WEnum::Value(t),
                ..
            } => o.transform = Some(t),
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                let current =
                    matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current));
                if current || o.mode == (0, 0) {
                    o.mode = (width, height);
                    o.refresh = refresh;
                }
            }
            wl_output::Event::Scale { factor } => o.scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, usize> for State {
    fn event(
        s: &mut Self,
        _: &ZxdgOutputV1,
        ev: zxdg_output_v1::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let o = s.outs.entry(*idx).or_default();
        match ev {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_pos = Some((x, y)),
            zxdg_output_v1::Event::Name { name } if o.name.is_none() => o.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<KdeOutputOrderV1, ()> for State {
    fn event(
        s: &mut Self,
        _: &KdeOutputOrderV1,
        ev: kde_output_order_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match ev {
            kde_output_order_v1::Event::Output { output_name } => s.order.push(output_name),
            kde_output_order_v1::Event::Done => {}
            _ => {}
        }
    }
}

fn rotated(t: Option<Transform>) -> bool {
    matches!(
        t,
        Some(Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270)
    )
}

/// Queries the outputs of the Wayland session (own short-lived connection).
pub fn query() -> Result<Vec<LocalOutput>> {
    let conn = Connection::connect_to_env().context("connect to the Wayland display")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let outputs: Vec<(u32, u32)> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == "wl_output")
            .map(|g| (g.name, g.version))
            .collect()
    });
    let mut wl = Vec::new();
    for (i, (name, version)) in outputs.iter().enumerate() {
        let o: WlOutput = globals.registry().bind(*name, (*version).min(4), &qh, i);
        wl.push(o);
    }
    let xdg: Option<ZxdgOutputManagerV1> = globals.bind(&qh, 1..=3, ()).ok();
    if let Some(m) = &xdg {
        for (i, o) in wl.iter().enumerate() {
            let _: ZxdgOutputV1 = m.get_xdg_output(o, &qh, i);
        }
    }
    let _order: Option<KdeOutputOrderV1> = globals.bind(&qh, 1..=1, ()).ok();
    let mut state = State::default();
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;

    let mut out: Vec<LocalOutput> = state
        .outs
        .iter()
        .map(|(i, o)| {
            let portrait = rotated(o.transform);
            let (mw, mh) = (o.mode.0.max(0) as u32, o.mode.1.max(0) as u32);
            let (width, height) = if portrait { (mh, mw) } else { (mw, mh) };
            let name = o.name.clone().unwrap_or_else(|| format!("output-{i}"));
            let scale = o.scale.max(1);
            let (lx, ly) = o.logical_pos.unwrap_or((0, 0));
            LocalOutput {
                order: state.order.iter().position(|n| *n == name),
                name,
                x: lx * scale,
                y: ly * scale,
                width,
                height,
                scale,
                refresh_mhz: o.refresh,
                portrait,
            }
        })
        .collect();
    out.sort_by_key(|o| (o.x, o.y));
    Ok(out)
}
