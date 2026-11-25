//! Session desktop layout: monitors, which surface pixels land on which output, and the
//! view transformation shared by presentation and input (docs/architektur.md).
//!
//! Coordinate assumption (to be confirmed by the live measurement, phase 0b): the monitor
//! rectangles of RDPGFX_RESET_GRAPHICS_PDU may be negative (virtual desktop around the
//! primary monitor); MapSurfaceToOutput origins are unsigned and refer to the same desktop
//! shifted so that the bounding box starts at (0, 0). `DesktopLayout::outputs` performs that
//! shift; if the live test shows otherwise, only this function changes.

use crate::rect::Rect;

/// Monitor rectangle in virtual desktop coordinates (TS_MONITOR_DEF, exclusive edges).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Monitor {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub primary: bool,
}

/// Monitor layout announced by the server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopLayout {
    pub monitors: Vec<Monitor>,
}

impl DesktopLayout {
    /// Top-left of the bounding box in virtual desktop coordinates.
    fn origin(&self) -> (i32, i32) {
        let l = self.monitors.iter().map(|m| m.left).min().unwrap_or(0);
        let t = self.monitors.iter().map(|m| m.top).min().unwrap_or(0);
        (l, t)
    }

    /// Bounding box of all monitors in output coordinates (always starts at 0, 0).
    pub fn bounding_box(&self) -> Rect {
        let (l, t) = self.origin();
        let r = self.monitors.iter().map(|m| m.right).max().unwrap_or(0);
        let b = self.monitors.iter().map(|m| m.bottom).max().unwrap_or(0);
        Rect::new(0, 0, (r - l).max(0) as u32, (b - t).max(0) as u32)
    }

    /// Clamps a session desktop position onto the nearest monitor, so a pointer that left
    /// all views never lands in a gap of the (non-rectangular) desktop.
    pub fn clamp_to_monitors(&self, x: f64, y: f64) -> Option<(u32, u32)> {
        self.outputs()
            .into_iter()
            .filter(|r| r.width() > 0 && r.height() > 0)
            .map(|r| {
                let cx = x.clamp(f64::from(r.left), f64::from(r.right - 1));
                let cy = y.clamp(f64::from(r.top), f64::from(r.bottom - 1));
                ((cx - x).powi(2) + (cy - y).powi(2), cx as u32, cy as u32)
            })
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, cx, cy)| (cx, cy))
    }

    /// Monitor rectangles in output coordinates (the space of MapSurfaceToOutput).
    pub fn outputs(&self) -> Vec<Rect> {
        let (l, t) = self.origin();
        self.monitors
            .iter()
            .map(|m| {
                Rect::new(
                    (m.left - l) as u32,
                    (m.top - t) as u32,
                    (m.right - l) as u32,
                    (m.bottom - t) as u32,
                )
            })
            .collect()
    }
}

/// A mapped surface as seen by the compositor: id, origin on the desktop, size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedSurface {
    pub id: u16,
    pub origin: (u32, u32),
    pub width: u32,
    pub height: u32,
}

/// Part of a surface that is visible in a viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub surface: u16,
    /// Source rectangle in surface coordinates.
    pub src: Rect,
    /// Destination rectangle relative to the viewport's top-left (same size as `src`).
    pub dst: Rect,
}

/// Pieces of the mapped surfaces (in drawing order, later on top) that intersect `viewport`
/// (output coordinates). Fully hidden surfaces still yield pieces; overdraw is cheap and
/// overlapping surfaces do not occur in practice.
pub fn compose(surfaces: &[MappedSurface], viewport: Rect) -> Vec<Piece> {
    surfaces
        .iter()
        .filter_map(|s| {
            let on_desktop = Rect::from_xywh(s.origin.0, s.origin.1, s.width, s.height);
            let vis = on_desktop.intersect(&viewport)?;
            Some(Piece {
                surface: s.id,
                src: Rect::new(
                    vis.left - s.origin.0,
                    vis.top - s.origin.1,
                    vis.right - s.origin.0,
                    vis.bottom - s.origin.1,
                ),
                dst: Rect::new(
                    vis.left - viewport.left,
                    vis.top - viewport.top,
                    vis.right - viewport.left,
                    vis.bottom - viewport.top,
                ),
            })
        })
        .collect()
}

/// Number of viewport pixels no piece covers (empty corners of the bounding box, missing
/// surfaces). Decides whether a view needs a background clear, so it runs every frame:
/// exact area via coordinate compression over the piece edges (no per-pixel work).
pub fn uncovered_pixels(pieces: &[Piece], viewport: Rect) -> u64 {
    let (w, h) = (viewport.width(), viewport.height());
    let rects: Vec<Rect> = pieces
        .iter()
        .filter_map(|p| p.dst.intersect(&Rect::new(0, 0, w, h)))
        .collect();
    let mut xs: Vec<u32> = rects.iter().flat_map(|r| [r.left, r.right]).collect();
    let mut ys: Vec<u32> = rects.iter().flat_map(|r| [r.top, r.bottom]).collect();
    xs.sort_unstable();
    xs.dedup();
    ys.sort_unstable();
    ys.dedup();
    let mut covered = 0u64;
    for xw in xs.windows(2) {
        for yw in ys.windows(2) {
            let cell = Rect::new(xw[0], yw[0], xw[1], yw[1]);
            if rects.iter().any(|r| cell.intersect(r) == Some(cell)) {
                covered += u64::from(cell.width()) * u64::from(cell.height());
            }
        }
    }
    u64::from(w) * u64::from(h) - covered
}

/// How a view maps its viewport.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ViewScale {
    /// 1:1 session pixels (fullscreen, and the windowed crop).
    #[default]
    Native,
    /// Uniformly scaled to fit (optional windowed overview).
    Fit,
}

/// Mapping between a view (window or fullscreen surface of `view_w`x`view_h` pixels) and a
/// viewport on the session desktop: uniform scale, centred (letterbox in windowed mode).
/// Presentation and input use the same transformation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewTransform {
    pub viewport: Rect,
    pub scale: f64,
    /// Offset of the scaled viewport inside the view (letterbox bars).
    pub offset: (f64, f64),
}

impl ViewTransform {
    pub fn fit(viewport: Rect, view_w: u32, view_h: u32) -> Self {
        let (vw, vh) = (f64::from(viewport.width()), f64::from(viewport.height()));
        let scale = (f64::from(view_w) / vw).min(f64::from(view_h) / vh);
        let offset = (
            (f64::from(view_w) - vw * scale) / 2.0,
            (f64::from(view_h) - vh * scale) / 2.0,
        );
        Self {
            viewport,
            scale,
            offset,
        }
    }

    /// 1:1 mapping; a viewport smaller than the view is centred on whole pixels.
    pub fn native(viewport: Rect, view_w: u32, view_h: u32) -> Self {
        Self {
            viewport,
            scale: 1.0,
            offset: (
                f64::from(view_w.saturating_sub(viewport.width()) / 2),
                f64::from(view_h.saturating_sub(viewport.height()) / 2),
            ),
        }
    }

    pub fn new(mode: ViewScale, viewport: Rect, view_w: u32, view_h: u32) -> Self {
        match mode {
            ViewScale::Native => Self::native(viewport, view_w, view_h),
            ViewScale::Fit => Self::fit(viewport, view_w, view_h),
        }
    }

    /// View pixel position -> session desktop pixel, `None` on the letterbox bars.
    pub fn view_to_session(&self, x: f64, y: f64) -> Option<(u32, u32)> {
        let sx = (x - self.offset.0) / self.scale;
        let sy = (y - self.offset.1) / self.scale;
        let (w, h) = (
            f64::from(self.viewport.width()),
            f64::from(self.viewport.height()),
        );
        if sx < 0.0 || sy < 0.0 || sx >= w || sy >= h {
            return None;
        }
        Some((
            self.viewport.left + sx as u32,
            self.viewport.top + sy as u32,
        ))
    }

    /// View position -> session desktop position without bounds (may lie outside the
    /// viewport, e.g. while a button drag leaves the window under an implicit pointer grab).
    pub fn view_to_session_unbounded(&self, x: f64, y: f64) -> (f64, f64) {
        (
            f64::from(self.viewport.left) + (x - self.offset.0) / self.scale,
            f64::from(self.viewport.top) + (y - self.offset.1) / self.scale,
        )
    }

    /// Session desktop pixel (its top-left corner) -> view position.
    pub fn session_to_view(&self, x: u32, y: u32) -> (f64, f64) {
        (
            self.offset.0 + f64::from(x - self.viewport.left) * self.scale,
            self.offset.1 + f64::from(y - self.viewport.top) * self.scale,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The target desktop: portrait monitor left, ultrawide primary in the middle, QHD right.
    fn desktop() -> DesktopLayout {
        DesktopLayout {
            monitors: vec![
                Monitor {
                    left: -1440,
                    top: -560,
                    right: 0,
                    bottom: 2000,
                    primary: false,
                },
                Monitor {
                    left: 0,
                    top: 0,
                    right: 3440,
                    bottom: 1440,
                    primary: true,
                },
                Monitor {
                    left: 3440,
                    top: 0,
                    right: 6000,
                    bottom: 1440,
                    primary: false,
                },
            ],
        }
    }

    #[test]
    fn outputs_are_shifted_to_the_bounding_box() {
        let d = desktop();
        assert_eq!(d.bounding_box(), Rect::new(0, 0, 7440, 2560));
        assert_eq!(
            d.outputs(),
            vec![
                Rect::new(0, 0, 1440, 2560),
                Rect::new(1440, 560, 4880, 2000),
                Rect::new(4880, 560, 7440, 2000),
            ]
        );
    }

    #[test]
    fn surface_per_output_covers_each_output_exactly() {
        let d = desktop();
        let outs = d.outputs();
        let surfaces: Vec<_> = outs
            .iter()
            .enumerate()
            .map(|(i, o)| MappedSurface {
                id: i as u16 + 1,
                origin: (o.left, o.top),
                width: o.width(),
                height: o.height(),
            })
            .collect();
        for (i, o) in outs.iter().enumerate() {
            let p = compose(&surfaces, *o);
            assert_eq!(p.len(), 1, "output {i}");
            assert_eq!(p[0].surface, i as u16 + 1);
            assert_eq!(p[0].src, Rect::new(0, 0, o.width(), o.height()));
            assert_eq!(p[0].dst, p[0].src);
            assert_eq!(uncovered_pixels(&p, *o), 0);
        }
        // Windowed view of the whole desktop: the corners above/below the landscape
        // monitors stay uncovered.
        let bb = d.bounding_box();
        let p = compose(&surfaces, bb);
        assert_eq!(p.len(), 3);
        assert_eq!(uncovered_pixels(&p, bb), 2 * (6000 * 560));
    }

    #[test]
    fn single_surface_is_cut_per_output() {
        let d = desktop();
        let bb = d.bounding_box();
        let s = [MappedSurface {
            id: 9,
            origin: (0, 0),
            width: bb.width(),
            height: bb.height(),
        }];
        let outs = d.outputs();
        let p = compose(&s, outs[1]);
        assert_eq!(
            p,
            vec![Piece {
                surface: 9,
                src: outs[1],
                dst: Rect::new(0, 0, 3440, 1440)
            }]
        );
    }

    #[test]
    fn view_transform_roundtrip_and_letterbox() {
        let bb = Rect::new(0, 0, 7440, 2560);
        // Window 1860x1000: scale 0.25 horizontally, bars above and below.
        let t = ViewTransform::fit(bb, 1860, 1000);
        assert!((t.scale - 0.25).abs() < 1e-12);
        assert_eq!(t.offset, (0.0, (1000.0 - 640.0) / 2.0));
        assert_eq!(t.view_to_session(10.0, 5.0), None);
        assert_eq!(t.view_to_session(0.0, 180.0), Some((0, 0)));
        assert_eq!(t.view_to_session(1859.9, 819.9), Some((7439, 2559)));
        let (x, y) = t.session_to_view(4000, 1000);
        assert_eq!(t.view_to_session(x, y), Some((4000, 1000)));
        // Fullscreen on the portrait output: identity apart from the viewport origin.
        let o = Rect::new(0, 0, 1440, 2560);
        let t = ViewTransform::fit(o, 1440, 2560);
        assert_eq!(t.scale, 1.0);
        assert_eq!(t.view_to_session(1439.5, 2559.5), Some((1439, 2559)));
    }

    #[test]
    fn uncovered_area_with_overlaps() {
        let vp = Rect::new(0, 0, 100, 100);
        let piece = |l, t, r, b| Piece {
            surface: 1,
            src: Rect::new(0, 0, 1, 1),
            dst: Rect::new(l, t, r, b),
        };
        assert_eq!(uncovered_pixels(&[], vp), 10_000);
        // Two overlapping rects (60x60 each, 20x20 overlap) and one sticking out.
        let p = [
            piece(0, 0, 60, 60),
            piece(40, 40, 100, 100),
            piece(90, 0, 150, 10),
        ];
        assert_eq!(uncovered_pixels(&p, vp), 10_000 - (3600 + 3600 - 400) - 100);
    }

    #[test]
    fn native_crop_is_one_to_one() {
        // Windowed crop of 1280x800 out of the centre output.
        let crop = Rect::from_xywh(2000, 700, 1280, 800);
        let t = ViewTransform::new(ViewScale::Native, crop, 1280, 800);
        assert_eq!((t.scale, t.offset), (1.0, (0.0, 0.0)));
        assert_eq!(t.view_to_session(0.0, 0.0), Some((2000, 700)));
        assert_eq!(t.view_to_session(1279.9, 799.9), Some((3279, 1499)));
        // Window larger than the viewport: centred on whole pixels, bars around it.
        let t = ViewTransform::native(Rect::new(0, 0, 100, 50), 301, 70);
        assert_eq!(t.offset, (100.0, 10.0));
        assert_eq!(t.view_to_session(99.0, 30.0), None);
        assert_eq!(t.view_to_session(100.0, 10.0), Some((0, 0)));
    }

    #[test]
    fn drag_outside_a_fullscreen_view_continues_on_the_neighbour() {
        let d = desktop();
        let primary = d.outputs()[1];
        let t = ViewTransform::native(primary, primary.width(), primary.height());
        let free = |x, y| {
            let (sx, sy) = t.view_to_session_unbounded(x, y);
            d.clamp_to_monitors(sx, sy)
        };
        assert_eq!(free(10.0, 100.0), Some((1450, 660)));
        // Past the left edge onto the portrait monitor, past the right edge onto the QHD one.
        assert_eq!(free(-10.0, 100.0), Some((1430, 660)));
        assert_eq!(free(3450.0, 100.0), Some((4890, 660)));
        // Below the ultrawide (a gap in the desktop): nearest monitor edge.
        assert_eq!(free(100.0, 1500.0), Some((1540, 1999)));
        // Far outside the desktop: clamped onto the portrait monitor.
        assert_eq!(free(-5000.0, -900.0), Some((0, 0)));
    }
}
