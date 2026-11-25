//! AVC444 / AVC444v2 view split (encoder side) and combination (decoder side).
//!
//! An AVC444 picture travels as two YUV 4:2:0 pictures ("views") through one H.264 stream:
//! the main view carries luma and subsampled chroma, the auxiliary view carries the chroma
//! samples the main view leaves out (MS-RDPEGFX 2.2.4.5, 3.3.8.3.2, 3.3.8.3.3).
//!
//! Decoding keeps a [`Yuv444Frame`] per surface. [`apply_main`] and [`apply_aux`] update it
//! inside the update's region rectangles; luma-only or chroma-only updates combine with what
//! earlier updates left. The even-row/even-column chroma sample holds the main view's value;
//! [`resolve_reverse_filter`] (or the GPU shader) undoes the encoder's 2x2 averaging.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::color::{clamp_u8, yuv_to_rgb};
use crate::frame::{MACROBLOCK, Yuv420Frame, Yuv444Frame};
use crate::rect::Rect;

/// Largest change the reverse chroma filter may make before it is applied (MS-RDPEGFX 3.3.8.3.2).
pub const REVERSE_FILTER_THRESHOLD: i32 = 30;

/// How the auxiliary view packs the chroma samples the main view leaves out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Avc444Layout {
    /// `RFX_AVC444_BITMAP_STREAM`: odd chroma rows interleaved per macroblock row.
    V1,
    /// `RFX_AVC444V2_BITMAP_STREAM`: odd chroma columns side by side.
    V2,
}

/// How the encoder derives the main view's subsampled chroma.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChromaMode {
    /// Average of the 2x2 block (spec model, assumed for Windows); needs the reverse filter.
    Box,
    /// Top-left sample of the block (FreeRDP's encoder); exact without any filter.
    Sample,
}

/// Splits a 4:4:4 picture into main and auxiliary view (both padded to whole macroblocks).
pub fn split(
    src: &Yuv444Frame,
    layout: Avc444Layout,
    chroma: ChromaMode,
) -> (Yuv420Frame, Yuv420Frame) {
    let (pw, ph) = (src.stride, src.rows);
    let (hw, hh) = ((pw / 2) as usize, (ph / 2) as usize);
    let stride = pw as usize;

    let mut main = Yuv420Frame::new(pw, ph);
    main.y.copy_from_slice(&src.y);
    main.u
        .par_chunks_mut(hw)
        .zip(main.v.par_chunks_mut(hw))
        .enumerate()
        .for_each(|(r, (mu, mv))| {
            let top = 2 * r * stride;
            let bot = top + stride;
            for c in 0..hw {
                let a = top + 2 * c;
                let b = bot + 2 * c;
                match chroma {
                    ChromaMode::Sample => {
                        mu[c] = src.u[a];
                        mv[c] = src.v[a];
                    }
                    ChromaMode::Box => {
                        mu[c] = avg4(src.u[a], src.u[a + 1], src.u[b], src.u[b + 1]);
                        mv[c] = avg4(src.v[a], src.v[a + 1], src.v[b], src.v[b + 1]);
                    }
                }
            }
        });

    let mut aux = Yuv420Frame::new(pw, ph);
    match layout {
        Avc444Layout::V1 => {
            // B4/B5: in each macroblock row, aux luma rows 0..7 carry the odd U rows,
            // rows 8..15 the odd V rows.
            let mb = MACROBLOCK as usize;
            aux.y
                .par_chunks_mut(stride)
                .enumerate()
                .for_each(|(aux_row, line)| {
                    let base = aux_row - aux_row % mb;
                    let within = aux_row % mb;
                    let (plane, row) = if within < mb / 2 {
                        (&src.u, base + 2 * within + 1)
                    } else {
                        (&src.v, base + 2 * (within - mb / 2) + 1)
                    };
                    line.copy_from_slice(&plane[row * stride..(row + 1) * stride]);
                });
            // B6/B7: odd columns of even rows.
            aux.u
                .par_chunks_mut(hw)
                .zip(aux.v.par_chunks_mut(hw))
                .enumerate()
                .for_each(|(r, (au, av))| {
                    let top = 2 * r * stride;
                    for c in 0..hw {
                        au[c] = src.u[top + 2 * c + 1];
                        av[c] = src.v[top + 2 * c + 1];
                    }
                });
        }
        Avc444Layout::V2 => {
            // B4/B5: odd U columns (left half), then odd V columns (right half), every row.
            aux.y
                .par_chunks_mut(stride)
                .enumerate()
                .for_each(|(row, line)| {
                    let base = row * stride;
                    for c in 0..hw {
                        line[c] = src.u[base + 2 * c + 1];
                        line[hw + c] = src.v[base + 2 * c + 1];
                    }
                });
            // B6..B9: even columns 4c and 4c+2 of odd rows.
            let q = hw / 2;
            aux.u
                .par_chunks_mut(hw)
                .zip(aux.v.par_chunks_mut(hw))
                .enumerate()
                .for_each(|(r, (au, av))| {
                    let odd = (2 * r + 1) * stride;
                    for c in 0..q {
                        let at = odd + 4 * c;
                        au[c] = src.u[at];
                        au[c + q] = src.v[at];
                        av[c] = src.u[at + 2];
                        av[c + q] = src.v[at + 2];
                    }
                });
        }
    }
    let _ = hh;
    (main, aux)
}

/// Rect clipped to the padded picture and grown to whole 2x2 chroma blocks.
fn chroma_area(dst: &Yuv444Frame, rect: &Rect) -> Option<Rect> {
    rect.align_to_chroma_blocks()
        .intersect(&Rect::new(0, 0, dst.stride, dst.rows))
}

/// Takes luma (inside `rects`) and subsampled chroma (for every 2x2 block the rects touch)
/// from a main view. The block's four chroma samples all get the main value, so a main-only
/// update shows the 4:2:0 picture until an auxiliary view refines it.
pub fn apply_main(dst: &mut Yuv444Frame, main: &Yuv420Frame, rects: &[Rect]) {
    assert_eq!(
        (main.width, main.height),
        (dst.stride, dst.rows),
        "main view size"
    );
    let stride = dst.stride as usize;
    let hw = (main.width / 2) as usize;
    for rect in rects {
        let Some(area) = chroma_area(dst, rect) else {
            continue;
        };
        let luma = rect.intersect(&Rect::new(0, 0, dst.stride, dst.rows));
        let (l, r) = (area.left as usize, area.right as usize);
        rows_mut(dst, area.top, area.bottom).for_each(|(row, (y, u, v))| {
            if let Some(lr) = luma
                && (lr.top as usize..lr.bottom as usize).contains(&row)
            {
                let (a, b) = (lr.left as usize, lr.right as usize);
                y[a..b].copy_from_slice(&main.y[row * stride + a..row * stride + b]);
            }
            let crow = (row / 2) * hw;
            for col in l..r {
                u[col] = main.u[crow + col / 2];
                v[col] = main.v[crow + col / 2];
            }
        });
    }
}

/// Takes the chroma samples the main view leaves out from an auxiliary view, inside `rects`.
pub fn apply_aux(dst: &mut Yuv444Frame, layout: Avc444Layout, aux: &Yuv420Frame, rects: &[Rect]) {
    assert_eq!(
        (aux.width, aux.height),
        (dst.stride, dst.rows),
        "aux view size"
    );
    let stride = dst.stride as usize;
    let hw = (aux.width / 2) as usize;
    let mb = MACROBLOCK as usize;
    for rect in rects {
        let Some(area) = chroma_area(dst, rect) else {
            continue;
        };
        let (l, r) = (area.left as usize, area.right as usize);
        rows_mut(dst, area.top, area.bottom).for_each(|(row, (_y, u, v))| match layout {
            Avc444Layout::V1 => {
                if row % 2 == 1 {
                    let base = row - row % mb;
                    let within = (row % mb) / 2;
                    let ur = (base + within) * stride;
                    let vr = (base + mb / 2 + within) * stride;
                    u[l..r].copy_from_slice(&aux.y[ur + l..ur + r]);
                    v[l..r].copy_from_slice(&aux.y[vr + l..vr + r]);
                } else {
                    let crow = (row / 2) * hw;
                    for col in (l + 1..r).step_by(2) {
                        u[col] = aux.u[crow + col / 2];
                        v[col] = aux.v[crow + col / 2];
                    }
                }
            }
            Avc444Layout::V2 => {
                let yrow = row * stride;
                for col in (l + 1..r).step_by(2) {
                    u[col] = aux.y[yrow + col / 2];
                    v[col] = aux.y[yrow + hw + col / 2];
                }
                if row % 2 == 1 {
                    let crow = (row / 2) * hw;
                    let q = hw / 2;
                    for col in (l..r).step_by(2) {
                        let c = col / 4;
                        if col % 4 == 0 {
                            u[col] = aux.u[crow + c];
                            v[col] = aux.u[crow + q + c];
                        } else {
                            u[col] = aux.v[crow + c];
                            v[col] = aux.v[crow + q + c];
                        }
                    }
                }
            }
        });
    }
}

/// Undoes the encoder's 2x2 averaging for one even/even sample.
#[inline]
pub fn reverse_filter(average: u8, right: u8, below: u8, diagonal: u8) -> u8 {
    let reversed = clamp_u8(
        4 * i32::from(average) - i32::from(right) - i32::from(below) - i32::from(diagonal),
    );
    if (i32::from(reversed) - i32::from(average)).abs() < REVERSE_FILTER_THRESHOLD {
        average
    } else {
        reversed
    }
}

/// Returns a copy with the reverse chroma filter applied to every even/even sample.
pub fn resolve_reverse_filter(f: &Yuv444Frame) -> Yuv444Frame {
    let mut out = f.clone();
    let stride = f.stride as usize;
    for (src, dst) in [(&f.u, &mut out.u), (&f.v, &mut out.v)] {
        dst.par_chunks_mut(stride)
            .enumerate()
            .filter(|(row, _)| row % 2 == 0)
            .for_each(|(row, line)| {
                let at = row * stride;
                let below = at + stride;
                for col in (0..stride).step_by(2) {
                    line[col] = reverse_filter(
                        src[at + col],
                        src[at + col + 1],
                        src[below + col],
                        src[below + col + 1],
                    );
                }
            });
    }
    out
}

/// Converts to tightly packed RGB, optionally after the reverse chroma filter.
pub fn to_rgb(f: &Yuv444Frame, reverse: bool) -> Vec<u8> {
    if reverse {
        resolve_reverse_filter(f).to_rgb()
    } else {
        f.to_rgb()
    }
}

/// Converts one RGB pixel of a combined picture (shader reference, single sample).
pub fn sample_rgb(f: &Yuv444Frame, row: u32, col: u32, reverse: bool) -> (u8, u8, u8) {
    let at = f.idx(row, col);
    let (mut u, mut v) = (f.u[at], f.v[at]);
    if reverse && row.is_multiple_of(2) && col.is_multiple_of(2) {
        let below = at + f.stride as usize;
        u = reverse_filter(u, f.u[at + 1], f.u[below], f.u[below + 1]);
        v = reverse_filter(v, f.v[at + 1], f.v[below], f.v[below + 1]);
    }
    yuv_to_rgb(f.y[at], u, v)
}

#[inline]
fn avg4(a: u8, b: u8, c: u8, d: u8) -> u8 {
    ((u32::from(a) + u32::from(b) + u32::from(c) + u32::from(d) + 2) >> 2) as u8
}

/// Mutable `(y, u, v)` row slices of one picture row.
type RowsMut<'a> = (&'a mut [u8], &'a mut [u8], &'a mut [u8]);

/// Parallel iterator over rows `top..bottom` yielding `(row, (y, u, v))` row slices.
fn rows_mut(
    f: &mut Yuv444Frame,
    top: u32,
    bottom: u32,
) -> impl ParallelIterator<Item = (usize, RowsMut<'_>)> {
    let stride = f.stride as usize;
    let (a, b) = (top as usize * stride, bottom as usize * stride);
    f.y[a..b]
        .par_chunks_mut(stride)
        .zip(f.u[a..b].par_chunks_mut(stride))
        .zip(f.v[a..b].par_chunks_mut(stride))
        .enumerate()
        .map(move |(i, ((y, u), v))| (top as usize + i, (y, u, v)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random 4:4:4 picture with independent chroma per sample.
    fn noise_frame(w: u32, h: u32, seed: u64) -> Yuv444Frame {
        let mut f = Yuv444Frame::new(w, h);
        let mut s = seed;
        for p in [&mut f.y, &mut f.u, &mut f.v] {
            for b in p.iter_mut() {
                s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *b = (s >> 56) as u8;
            }
        }
        f
    }

    fn full(f: &Yuv444Frame) -> Vec<Rect> {
        vec![Rect::new(0, 0, f.stride, f.rows)]
    }

    #[test]
    fn sample_mode_round_trip_is_exact() {
        for layout in [Avc444Layout::V1, Avc444Layout::V2] {
            for (w, h) in [(64, 48), (1440, 64), (35, 19)] {
                let src = noise_frame(w, h, 7);
                let (main, aux) = split(&src, layout, ChromaMode::Sample);
                let mut dst = Yuv444Frame::new(w, h);
                let all = full(&dst);
                apply_main(&mut dst, &main, &all);
                apply_aux(&mut dst, layout, &aux, &all);
                assert_eq!(dst, src, "{layout:?} {w}x{h}");
            }
        }
    }

    #[test]
    fn box_mode_reconstructs_smooth_chroma_exactly() {
        // With constant chroma per 2x2 block, average == sample, so the filter must keep it.
        let mut src = noise_frame(64, 32, 3);
        for row in 0..src.rows {
            for col in 0..src.stride {
                let at = src.idx(row, col);
                let blk = src.idx(row & !1, col & !1);
                src.u[at] = src.u[blk];
                src.v[at] = src.v[blk];
            }
        }
        for layout in [Avc444Layout::V1, Avc444Layout::V2] {
            let (main, aux) = split(&src, layout, ChromaMode::Box);
            let mut dst = Yuv444Frame::new(64, 32);
            let all = full(&dst);
            apply_main(&mut dst, &main, &all);
            apply_aux(&mut dst, layout, &aux, &all);
            assert_eq!(resolve_reverse_filter(&dst), src, "{layout:?}");
        }
    }

    #[test]
    fn box_mode_reverse_filter_restores_strong_edges() {
        // A sharp chroma edge inside a block: the filter must recover the top-left sample.
        let mut src = Yuv444Frame::new(16, 16);
        for row in 0..16 {
            for col in 0..16 {
                let at = src.idx(row, col);
                src.u[at] = if (row % 2 == 0) && (col % 2 == 0) {
                    250
                } else {
                    20
                };
            }
        }
        let (main, aux) = split(&src, Avc444Layout::V1, ChromaMode::Box);
        let mut dst = Yuv444Frame::new(16, 16);
        let all = full(&dst);
        apply_main(&mut dst, &main, &all);
        apply_aux(&mut dst, Avc444Layout::V1, &aux, &all);
        let r = resolve_reverse_filter(&dst);
        // The averaged value is rounded, so the reversed sample is exact only to +-3.
        let worst = crate::metrics::max_abs_diff(&r.u, &src.u);
        assert!(worst <= 3, "worst {worst}");
        assert!(r.u[0] > 240, "edge not restored: {}", r.u[0]);
    }

    #[test]
    fn main_only_update_upsamples_chroma() {
        let src = noise_frame(32, 32, 11);
        let (main, _) = split(&src, Avc444Layout::V1, ChromaMode::Sample);
        let mut dst = Yuv444Frame::new(32, 32);
        let all = full(&dst);
        apply_main(&mut dst, &main, &all);
        assert_eq!(dst.y, src.y);
        for row in 0..32 {
            for col in 0..32 {
                assert_eq!(dst.u[dst.idx(row, col)], src.u[src.idx(row & !1, col & !1)]);
            }
        }
    }

    #[test]
    fn region_updates_touch_only_their_blocks() {
        let a = noise_frame(64, 64, 1);
        let b = noise_frame(64, 64, 2);
        for layout in [Avc444Layout::V1, Avc444Layout::V2] {
            let (am, aa) = split(&a, layout, ChromaMode::Sample);
            let (bm, ba) = split(&b, layout, ChromaMode::Sample);
            let mut dst = Yuv444Frame::new(64, 64);
            let all = full(&dst);
            apply_main(&mut dst, &am, &all);
            apply_aux(&mut dst, layout, &aa, &all);
            let region = Rect::new(10, 6, 30, 21); // odd edges on purpose
            apply_main(&mut dst, &bm, &[region]);
            apply_aux(&mut dst, layout, &ba, &[region]);
            let blocks = region.align_to_chroma_blocks();
            for row in 0..64 {
                for col in 0..64 {
                    let at = dst.idx(row, col);
                    let inside_luma = region.intersect(&Rect::from_xywh(col, row, 1, 1)).is_some();
                    let inside_chroma =
                        blocks.intersect(&Rect::from_xywh(col, row, 1, 1)).is_some();
                    let ey = if inside_luma { b.y[at] } else { a.y[at] };
                    let (eu, ev) = if inside_chroma {
                        (b.u[at], b.v[at])
                    } else {
                        (a.u[at], a.v[at])
                    };
                    assert_eq!(
                        (dst.y[at], dst.u[at], dst.v[at]),
                        (ey, eu, ev),
                        "{layout:?} at {row},{col}"
                    );
                }
            }
        }
    }

    #[test]
    fn v1_layout_matches_spec_rows() {
        // Aux luma row 0 = U row 1, row 7 = U row 15, row 8 = V row 1, row 16 = U row 17.
        let src = noise_frame(32, 32, 5);
        let (_, aux) = split(&src, Avc444Layout::V1, ChromaMode::Sample);
        let s = src.stride as usize;
        let row = |p: &Vec<u8>, r: usize| p[r * s..(r + 1) * s].to_vec();
        assert_eq!(row(&aux.y, 0), row(&src.u, 1));
        assert_eq!(row(&aux.y, 7), row(&src.u, 15));
        assert_eq!(row(&aux.y, 8), row(&src.v, 1));
        assert_eq!(row(&aux.y, 16), row(&src.u, 17));
    }

    #[test]
    fn sample_rgb_matches_bulk_conversion() {
        let src = noise_frame(32, 16, 9);
        let rgb = to_rgb(&src, true);
        for (row, col) in [(0, 0), (1, 3), (14, 30), (15, 31)] {
            let i = ((row * 32 + col) * 3) as usize;
            assert_eq!(
                sample_rgb(&src, row, col, true),
                (rgb[i], rgb[i + 1], rgb[i + 2])
            );
        }
    }
}
