//! CPU reference of the RDPGFX surface model (MS-RDPEGFX 3.3.5): surfaces, solid fills,
//! surface-to-surface copies, the bitmap cache, CPU-decoded pixel uploads and AVC updates.
//!
//! The GPU core implements the same operations; its tests run identical command sequences
//! against this model and compare the surfaces byte for byte.
//!
//! Semantics (aligned with FreeRDP 3.33.1 `libfreerdp/gdi/gfx.c` unless noted):
//! - Pixels are stored as BGRA bytes (`PIXEL_FORMAT_XRGB_8888`/`ARGB_8888` in memory order).
//!   For XRGB surfaces the alpha byte carries no meaning; presentation treats them as opaque.
//! - New surfaces start opaque black (the spec leaves the content undefined).
//! - SolidFill: rectangles are clipped to the surface; the alpha of the fill colour is
//!   ignored and written as 0xFF (MS-RDPEGFX 3.3.5.4).
//! - SurfaceToSurface, SurfaceToCache, CacheToSurface: rectangles outside the surface are a
//!   protocol error. Unlike FreeRDP, the whole command is validated before anything changes.
//! - SurfaceToSurface reads the source as it was before the command (snapshot), also when a
//!   destination overlaps the source or several destinations are given. FreeRDP copies point
//!   by point; both agree for the common single-destination scroll.
//! - AVC: region rectangles are in surface coordinates. The codec state (YUV 4:4:4) lives per
//!   surface; only pixels inside the region rectangles are converted to BGRA.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use crate::avc444::{self, Avc444Layout, sample_rgb};
use crate::frame::{Yuv420Frame, Yuv444Frame};
use crate::rect::Rect;

/// Pixel formats of RDPGFX_CREATE_SURFACE_PDU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Xrgb8888,
    Argb8888,
}

/// Fill colour (RDPGFX_COLOR32 without the ignored alpha).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    /// BGRA bytes as stored in a surface (alpha always 0xFF).
    pub const fn bgra(self) -> [u8; 4] {
        [self.b, self.g, self.r, 0xFF]
    }

    /// The same bytes as a little-endian `u32` (what the GPU writes per texel).
    pub const fn packed(self) -> u32 {
        u32::from_le_bytes(self.bgra())
    }
}

/// Initial pixel of a new surface: opaque black.
pub const INITIAL_PIXEL: [u8; 4] = [0, 0, 0, 0xFF];

/// Default number of cache slots (MS-RDPEGFX 2.2.2.4: 4096 for the v8.x capability sets).
pub const DEFAULT_CACHE_SLOTS: u16 = 4096;

/// What a surface-model command can do wrong. The model is left unchanged in every case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceError {
    UnknownSurface(u16),
    SurfaceExists(u16),
    EmptySurface(u16),
    UnknownCacheSlot(u16),
    InvalidCacheSlot(u16),
    OutOfBounds { surface: u16, rect: Rect },
    PixelDataSize { expected: usize, got: usize },
    ViewSize,
}

impl fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownSurface(id) => write!(f, "unknown surface {id}"),
            Self::SurfaceExists(id) => write!(f, "surface {id} already exists"),
            Self::EmptySurface(id) => write!(f, "surface {id} has zero size"),
            Self::UnknownCacheSlot(s) => write!(f, "cache slot {s} is empty"),
            Self::InvalidCacheSlot(s) => write!(f, "cache slot {s} out of range"),
            Self::OutOfBounds { surface, rect } => {
                write!(f, "rect {rect:?} outside surface {surface}")
            }
            Self::PixelDataSize { expected, got } => {
                write!(f, "pixel data has {got} bytes, expected {expected}")
            }
            Self::ViewSize => write!(f, "AVC view does not match the padded surface size"),
        }
    }
}

impl std::error::Error for SurfaceError {}

pub type SurfaceResult<T> = Result<T, SurfaceError>;

/// Which views an AVC444 update carries (the LC field of RFX_AVC444_BITMAP_STREAM).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LumaChroma {
    /// LC = 0: main and auxiliary view.
    Both,
    /// LC = 1: main view only.
    Luma,
    /// LC = 2: auxiliary view only.
    Chroma,
}

impl LumaChroma {
    pub const fn has_main(self) -> bool {
        matches!(self, Self::Both | Self::Luma)
    }
    pub const fn has_aux(self) -> bool {
        matches!(self, Self::Both | Self::Chroma)
    }
}

/// Surface commands without codec payload. AVC updates carry decoded views on the CPU and
/// bitstreams on the GPU, so they are separate methods on both sides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SurfaceOp {
    Create {
        id: u16,
        width: u32,
        height: u32,
        format: PixelFormat,
    },
    Delete {
        id: u16,
    },
    MapToOutput {
        id: u16,
        x: u32,
        y: u32,
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
        dst: u16,
        points: Vec<(u32, u32)>,
    },
    EvictCache {
        slot: u16,
    },
    /// Pixels decoded on the CPU (ClearCodec, Planar, uncompressed): tight BGRA rows.
    Upload {
        id: u16,
        rect: Rect,
        bgra: Vec<u8>,
    },
}

/// One surface of the CPU model.
#[derive(Clone, Debug)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    /// Tight BGRA rows.
    pub pixels: Vec<u8>,
    /// Position on the session desktop once mapped (RDPGFX_MAP_SURFACE_TO_OUTPUT_PDU).
    pub output_origin: Option<(u32, u32)>,
    /// AVC codec state, created by the first AVC update.
    pub yuv: Option<Yuv444Frame>,
}

impl Surface {
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    fn contains(&self, r: &Rect) -> bool {
        within(r, self.width, self.height)
    }

    fn read(&self, r: &Rect) -> Vec<u8> {
        let mut out = Vec::with_capacity((r.width() * r.height() * 4) as usize);
        for row in r.top..r.bottom {
            let a = ((row * self.width + r.left) * 4) as usize;
            out.extend_from_slice(&self.pixels[a..a + (r.width() * 4) as usize]);
        }
        out
    }

    fn write(&mut self, x: u32, y: u32, w: u32, h: u32, src: &[u8]) {
        let n = (w * 4) as usize;
        for row in 0..h {
            let a = (((y + row) * self.width + x) * 4) as usize;
            self.pixels[a..a + n].copy_from_slice(&src[row as usize * n..][..n]);
        }
    }
}

/// A cache entry: a copied block of pixels.
#[derive(Clone, Debug)]
pub struct CacheEntry {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// CPU model of all surfaces and the bitmap cache of one graphics pipeline.
#[derive(Clone, Debug)]
pub struct SurfaceModel {
    pub surfaces: BTreeMap<u16, Surface>,
    pub cache: HashMap<u16, CacheEntry>,
    pub max_cache_slots: u16,
    /// Mapping order: later mapped surfaces are drawn on top (see `outputs`).
    pub map_order: Vec<u16>,
}

impl Default for SurfaceModel {
    fn default() -> Self {
        Self::new(DEFAULT_CACHE_SLOTS)
    }
}

/// Whether `r` lies inside a `width`x`height` surface (empty rectangles included).
pub fn within(r: &Rect, width: u32, height: u32) -> bool {
    r.left <= r.right && r.top <= r.bottom && r.right <= width && r.bottom <= height
}

/// Destination rectangle of a copy of size `w`x`h` to `point`.
pub fn dest_rect(point: (u32, u32), w: u32, h: u32) -> Rect {
    Rect::new(point.0, point.1, point.0 + w, point.1 + h)
}

/// Checks a 1-based cache slot index.
pub fn check_slot(slot: u16, max: u16) -> SurfaceResult<()> {
    if slot == 0 || slot > max {
        Err(SurfaceError::InvalidCacheSlot(slot))
    } else {
        Ok(())
    }
}

impl SurfaceModel {
    pub fn new(max_cache_slots: u16) -> Self {
        Self {
            surfaces: BTreeMap::new(),
            cache: HashMap::new(),
            max_cache_slots,
            map_order: Vec::new(),
        }
    }

    pub fn surface(&self, id: u16) -> SurfaceResult<&Surface> {
        self.surfaces
            .get(&id)
            .ok_or(SurfaceError::UnknownSurface(id))
    }

    fn surface_mut(&mut self, id: u16) -> SurfaceResult<&mut Surface> {
        self.surfaces
            .get_mut(&id)
            .ok_or(SurfaceError::UnknownSurface(id))
    }

    /// Applies one command. On error nothing changes.
    pub fn apply(&mut self, op: &SurfaceOp) -> SurfaceResult<()> {
        match op {
            SurfaceOp::Create {
                id,
                width,
                height,
                format,
            } => {
                if self.surfaces.contains_key(id) {
                    return Err(SurfaceError::SurfaceExists(*id));
                }
                if *width == 0 || *height == 0 {
                    return Err(SurfaceError::EmptySurface(*id));
                }
                let n = (*width * *height) as usize;
                self.surfaces.insert(
                    *id,
                    Surface {
                        width: *width,
                        height: *height,
                        format: *format,
                        pixels: INITIAL_PIXEL.repeat(n),
                        output_origin: None,
                        yuv: None,
                    },
                );
            }
            SurfaceOp::Delete { id } => {
                self.surfaces
                    .remove(id)
                    .ok_or(SurfaceError::UnknownSurface(*id))?;
                self.map_order.retain(|s| s != id);
            }
            SurfaceOp::MapToOutput { id, x, y } => {
                self.surface_mut(*id)?.output_origin = Some((*x, *y));
                self.map_order.retain(|s| s != id);
                self.map_order.push(*id);
            }
            SurfaceOp::SolidFill { id, color, rects } => {
                let s = self.surface_mut(*id)?;
                let px = color.bgra();
                for r in rects {
                    if let Some(c) = r.intersect(&s.bounds()) {
                        let line = px.repeat(c.width() as usize);
                        for row in c.top..c.bottom {
                            let a = ((row * s.width + c.left) * 4) as usize;
                            s.pixels[a..a + line.len()].copy_from_slice(&line);
                        }
                    }
                }
            }
            SurfaceOp::SurfaceToSurface {
                src,
                rect,
                dst,
                points,
            } => {
                let s = self.surface(*src)?;
                if !s.contains(rect) {
                    return Err(SurfaceError::OutOfBounds {
                        surface: *src,
                        rect: *rect,
                    });
                }
                let d = self.surface(*dst)?;
                let (w, h) = (rect.width(), rect.height());
                for p in points {
                    let r = dest_rect(*p, w, h);
                    if !d.contains(&r) {
                        return Err(SurfaceError::OutOfBounds {
                            surface: *dst,
                            rect: r,
                        });
                    }
                }
                let block = s.read(rect);
                let d = self.surface_mut(*dst)?;
                for p in points {
                    d.write(p.0, p.1, w, h, &block);
                }
            }
            SurfaceOp::SurfaceToCache { id, rect, slot } => {
                check_slot(*slot, self.max_cache_slots)?;
                let s = self.surface(*id)?;
                if !s.contains(rect) {
                    return Err(SurfaceError::OutOfBounds {
                        surface: *id,
                        rect: *rect,
                    });
                }
                let entry = CacheEntry {
                    width: rect.width(),
                    height: rect.height(),
                    pixels: s.read(rect),
                };
                self.cache.insert(*slot, entry);
            }
            SurfaceOp::CacheToSurface { slot, dst, points } => {
                check_slot(*slot, self.max_cache_slots)?;
                let e = self
                    .cache
                    .get(slot)
                    .ok_or(SurfaceError::UnknownCacheSlot(*slot))?;
                let d = self.surface(*dst)?;
                for p in points {
                    let r = dest_rect(*p, e.width, e.height);
                    if !d.contains(&r) {
                        return Err(SurfaceError::OutOfBounds {
                            surface: *dst,
                            rect: r,
                        });
                    }
                }
                let e = e.clone();
                let d = self.surface_mut(*dst)?;
                for p in points {
                    d.write(p.0, p.1, e.width, e.height, &e.pixels);
                }
            }
            SurfaceOp::EvictCache { slot } => {
                check_slot(*slot, self.max_cache_slots)?;
                self.cache
                    .remove(slot)
                    .ok_or(SurfaceError::UnknownCacheSlot(*slot))?;
            }
            SurfaceOp::Upload { id, rect, bgra } => {
                let s = self.surface_mut(*id)?;
                if !s.contains(rect) {
                    return Err(SurfaceError::OutOfBounds {
                        surface: *id,
                        rect: *rect,
                    });
                }
                let expected = (rect.width() * rect.height() * 4) as usize;
                if bgra.len() != expected {
                    return Err(SurfaceError::PixelDataSize {
                        expected,
                        got: bgra.len(),
                    });
                }
                s.write(rect.left, rect.top, rect.width(), rect.height(), bgra);
            }
        }
        Ok(())
    }

    /// RDPGFX_RESET_GRAPHICS_PDU: codec states are dropped (like FreeRDP's codec reset);
    /// surfaces, mappings and the cache stay. The new monitor layout is handled by `outputs`.
    pub fn reset_graphics(&mut self) {
        for s in self.surfaces.values_mut() {
            s.yuv = None;
        }
    }

    /// AVC420 or AVC444 update. `main`/`aux` are the decoded views with their region
    /// rectangles (`None` when the LC field leaves the view out; AVC420 = main only).
    /// Pixels inside the union of both rectangle sets are converted to BGRA.
    pub fn avc(
        &mut self,
        id: u16,
        layout: Avc444Layout,
        main: Option<(&Yuv420Frame, &[Rect])>,
        aux: Option<(&Yuv420Frame, &[Rect])>,
        reverse: bool,
    ) -> SurfaceResult<()> {
        let s = self.surface_mut(id)?;
        let (w, h) = (s.width, s.height);
        let padded = (crate::frame::pad(w), crate::frame::pad(h));
        for v in [main.map(|m| m.0), aux.map(|a| a.0)].into_iter().flatten() {
            if (v.width, v.height) != padded {
                return Err(SurfaceError::ViewSize);
            }
        }
        let yuv = s.yuv.get_or_insert_with(|| Yuv444Frame::new(w, h));
        if let Some((m, rects)) = main {
            avc444::apply_main(yuv, m, rects);
        }
        if let Some((a, rects)) = aux {
            avc444::apply_aux(yuv, layout, a, rects);
        }
        let bounds = s.bounds();
        let rects = main
            .iter()
            .chain(aux.iter())
            .flat_map(|(_, r)| r.iter())
            .filter_map(|r| r.intersect(&bounds));
        let yuv = s.yuv.as_ref().expect("created above");
        for r in rects {
            for row in r.top..r.bottom {
                for col in r.left..r.right {
                    let (red, g, b) = sample_rgb(yuv, row, col, reverse);
                    let a = ((row * w + col) * 4) as usize;
                    s.pixels[a..a + 4].copy_from_slice(&[b, g, red, 0xFF]);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(s: &Surface, x: u32, y: u32) -> [u8; 4] {
        let a = ((y * s.width + x) * 4) as usize;
        s.pixels[a..a + 4].try_into().unwrap()
    }

    fn model_with(id: u16, w: u32, h: u32) -> SurfaceModel {
        let mut m = SurfaceModel::default();
        m.apply(&SurfaceOp::Create {
            id,
            width: w,
            height: h,
            format: PixelFormat::Xrgb8888,
        })
        .unwrap();
        m
    }

    /// Writes a distinct value into every pixel (x, y encoded).
    fn gradient(m: &mut SurfaceModel, id: u16) {
        let s = m.surface(id).unwrap();
        let (w, h) = (s.width, s.height);
        let mut data = Vec::new();
        for y in 0..h {
            for x in 0..w {
                data.extend_from_slice(&[x as u8, y as u8, (x ^ y) as u8, 0xFF]);
            }
        }
        m.apply(&SurfaceOp::Upload {
            id,
            rect: Rect::new(0, 0, w, h),
            bgra: data,
        })
        .unwrap();
    }

    #[test]
    fn create_starts_black_and_rejects_duplicates() {
        let mut m = model_with(1, 4, 3);
        assert_eq!(px(m.surface(1).unwrap(), 3, 2), INITIAL_PIXEL);
        let dup = SurfaceOp::Create {
            id: 1,
            width: 2,
            height: 2,
            format: PixelFormat::Argb8888,
        };
        assert_eq!(m.apply(&dup), Err(SurfaceError::SurfaceExists(1)));
    }

    #[test]
    fn solid_fill_clips_and_ignores_alpha() {
        let mut m = model_with(1, 8, 8);
        let color = Color {
            r: 10,
            g: 20,
            b: 30,
        };
        m.apply(&SurfaceOp::SolidFill {
            id: 1,
            color,
            rects: vec![Rect::new(6, 6, 20, 20), Rect::new(0, 0, 0, 5)],
        })
        .unwrap();
        let s = m.surface(1).unwrap();
        assert_eq!(px(s, 7, 7), [30, 20, 10, 0xFF]);
        assert_eq!(px(s, 5, 7), INITIAL_PIXEL);
        assert_eq!(px(s, 0, 0), INITIAL_PIXEL);
    }

    #[test]
    fn overlapping_scroll_reads_snapshot() {
        let mut m = model_with(1, 6, 6);
        gradient(&mut m, 1);
        let before = m.surface(1).unwrap().clone();
        // Scroll up by one row and, with a second point, also duplicate it downwards.
        m.apply(&SurfaceOp::SurfaceToSurface {
            src: 1,
            rect: Rect::new(0, 1, 6, 4),
            dst: 1,
            points: vec![(0, 0), (0, 2)],
        })
        .unwrap();
        let s = m.surface(1).unwrap();
        for y in 0..3 {
            for x in 0..6 {
                assert_eq!(px(s, x, y + 2), px(&before, x, y + 1));
            }
        }
        assert_eq!(px(s, 3, 0), px(&before, 3, 1));
        assert_eq!(px(s, 3, 1), px(&before, 3, 2));
    }

    #[test]
    fn out_of_bounds_copy_changes_nothing() {
        let mut m = model_with(1, 6, 6);
        gradient(&mut m, 1);
        let before = m.surface(1).unwrap().pixels.clone();
        let op = SurfaceOp::SurfaceToSurface {
            src: 1,
            rect: Rect::new(0, 0, 2, 2),
            dst: 1,
            points: vec![(0, 0), (5, 5)],
        };
        assert!(matches!(
            m.apply(&op),
            Err(SurfaceError::OutOfBounds { surface: 1, .. })
        ));
        assert_eq!(m.surface(1).unwrap().pixels, before);
    }

    #[test]
    fn cache_roundtrip_survives_surface_delete() {
        let mut m = model_with(1, 6, 6);
        gradient(&mut m, 1);
        let src = m.surface(1).unwrap().clone();
        m.apply(&SurfaceOp::SurfaceToCache {
            id: 1,
            rect: Rect::new(1, 2, 4, 5),
            slot: 7,
        })
        .unwrap();
        m.apply(&SurfaceOp::Delete { id: 1 }).unwrap();
        m.apply(&SurfaceOp::Create {
            id: 2,
            width: 8,
            height: 8,
            format: PixelFormat::Argb8888,
        })
        .unwrap();
        m.apply(&SurfaceOp::CacheToSurface {
            slot: 7,
            dst: 2,
            points: vec![(5, 5), (0, 0)],
        })
        .unwrap();
        let d = m.surface(2).unwrap();
        assert_eq!(px(d, 5, 5), px(&src, 1, 2));
        assert_eq!(px(d, 7, 7), px(&src, 3, 4));
        assert_eq!(px(d, 0, 0), px(&src, 1, 2));
        assert_eq!(
            m.apply(&SurfaceOp::CacheToSurface {
                slot: 8,
                dst: 2,
                points: vec![(0, 0)]
            }),
            Err(SurfaceError::UnknownCacheSlot(8))
        );
        assert_eq!(
            m.apply(&SurfaceOp::EvictCache { slot: 0 }),
            Err(SurfaceError::InvalidCacheSlot(0))
        );
        m.apply(&SurfaceOp::EvictCache { slot: 7 }).unwrap();
        assert!(m.cache.is_empty());
    }

    #[test]
    fn avc_converts_only_region_pixels() {
        let mut m = model_with(1, 20, 18);
        // A flat grey main view: Y=128, U=V=128.
        let mut main = Yuv420Frame::new(32, 32);
        main.y.fill(128);
        m.avc(
            1,
            Avc444Layout::V1,
            Some((&main, &[Rect::new(3, 3, 7, 5)])),
            None,
            false,
        )
        .unwrap();
        let s = m.surface(1).unwrap();
        assert_eq!(px(s, 3, 3), [128, 128, 128, 0xFF]);
        assert_eq!(px(s, 6, 4), [128, 128, 128, 0xFF]);
        assert_eq!(px(s, 2, 3), INITIAL_PIXEL);
        assert_eq!(px(s, 7, 3), INITIAL_PIXEL);
        let wrong = Yuv420Frame::new(16, 16);
        assert_eq!(
            m.avc(1, Avc444Layout::V1, Some((&wrong, &[])), None, false),
            Err(SurfaceError::ViewSize)
        );
    }
}
