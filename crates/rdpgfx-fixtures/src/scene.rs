//! Deterministic synthetic "desktop" pictures in session desktop coordinates.
//!
//! The content imitates what an RDP desktop shows and what makes AVC444 necessary:
//! sharp coloured text on light windows, flat areas, a moving window, a scrolling text
//! pane and (optionally) a noisy video area. Everything derives from (seed, frame, x, y),
//! so the verifier can re-render the exact source frames instead of storing them.

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use rdpgfx_model::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Content {
    /// Windows, coloured text, a moving window and a scrolling pane.
    Desktop,
    /// Desktop plus a large full-motion noise area (worst case for the encoder).
    Video,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scene {
    pub seed: u64,
    pub content: Content,
    /// Desktop bounding box.
    pub width: u32,
    pub height: u32,
}

struct Window {
    rect: Rect,
    title: [u8; 3],
    ink: [u8; 3],
}

const BG_TOP: [i32; 3] = [32, 58, 96];
const BG_BOTTOM: [i32; 3] = [14, 24, 44];

impl Scene {
    /// Static windows, laid out relative to the desktop size.
    fn windows(&self) -> Vec<Window> {
        let (w, h) = (self.width, self.height);
        let mut v = Vec::new();
        let inks = [[200, 20, 20], [20, 40, 200], [0, 120, 40], [150, 0, 150]];
        let titles = [
            [60, 120, 200],
            [200, 90, 40],
            [40, 160, 120],
            [120, 120, 120],
        ];
        let mut s = self.seed;
        for i in 0..6u32 {
            let rw = (w / 4).max(200) + (next(&mut s) % (w / 6 + 1) as u64) as u32;
            let rh = (h / 3).max(150) + (next(&mut s) % (h / 4 + 1) as u64) as u32;
            let x = (next(&mut s) % u64::from(w.saturating_sub(rw).max(1))) as u32;
            let y = (next(&mut s) % u64::from(h.saturating_sub(rh).max(1))) as u32;
            v.push(Window {
                rect: Rect::from_xywh(x, y, rw.min(w), rh.min(h)),
                title: titles[i as usize % 4],
                ink: inks[i as usize % 4],
            });
        }
        v
    }

    /// Rect of the moving window in `frame` (sweeps horizontally across the whole desktop).
    pub fn moving_window(&self, frame: u32) -> Rect {
        let (ww, wh) = ((self.width / 5).max(320), (self.height / 3).max(240));
        let span = self.width.saturating_sub(ww).max(1);
        let t = (frame * 23) % (2 * span);
        let x = if t < span { t } else { 2 * span - t };
        let y = self.height / 3;
        Rect::from_xywh(x, y.min(self.height - wh), ww, wh)
    }

    /// The scrolling text pane (fixed position, content moves up 3 px per frame).
    fn scroll_pane(&self) -> Rect {
        let (pw, ph) = ((self.width / 6).max(240), (self.height / 2).max(200));
        Rect::from_xywh(self.width - pw - 16, 16, pw, ph.min(self.height - 32))
    }

    fn video_area(&self) -> Option<Rect> {
        (self.content == Content::Video).then(|| {
            Rect::from_xywh(
                self.width / 8,
                self.height / 2,
                self.width / 3,
                self.height / 3,
            )
        })
    }

    /// Renders the desktop crop `area` of `frame` as tightly packed RGB.
    pub fn render(&self, frame: u32, area: Rect) -> Vec<u8> {
        let windows = self.windows();
        let moving = self.moving_window(frame);
        let pane = self.scroll_pane();
        let video = self.video_area();
        let (aw, ah) = (area.width() as usize, area.height() as usize);
        let mut out = vec![0u8; aw * ah * 3];
        out.par_chunks_mut(aw * 3)
            .enumerate()
            .for_each(|(r, line)| {
                let y = area.top + r as u32;
                for c in 0..aw {
                    let x = area.left + c as u32;
                    let px = self.pixel(frame, x, y, &windows, moving, pane, video);
                    line[c * 3..c * 3 + 3].copy_from_slice(&px);
                }
            });
        out
    }

    #[allow(clippy::too_many_arguments)]
    fn pixel(
        &self,
        frame: u32,
        x: u32,
        y: u32,
        windows: &[Window],
        moving: Rect,
        pane: Rect,
        video: Option<Rect>,
    ) -> [u8; 3] {
        if let Some(v) = video
            && contains(&v, x, y)
        {
            let n = hash(
                self.seed ^ 0x5bd1e995,
                u64::from(frame),
                u64::from(x / 2),
                u64::from(y / 2),
            );
            return [(n >> 8) as u8, (n >> 16) as u8, (n >> 24) as u8];
        }
        if contains(&moving, x, y) {
            return window_pixel(
                x - moving.left,
                y - moving.top,
                [180, 40, 160],
                [10, 90, 20],
                self.seed ^ 0xabc,
                0,
            );
        }
        if contains(&pane, x, y) {
            return window_pixel(
                x - pane.left,
                y - pane.top,
                [90, 90, 90],
                [30, 30, 180],
                self.seed ^ 0x77,
                frame * 3,
            );
        }
        for w in windows.iter().rev() {
            if contains(&w.rect, x, y) {
                return window_pixel(
                    x - w.rect.left,
                    y - w.rect.top,
                    w.title,
                    w.ink,
                    self.seed ^ u64::from(w.rect.left),
                    0,
                );
            }
        }
        // Background: vertical gradient with a faint diagonal pattern.
        let t = y as i32 * 256 / self.height.max(1) as i32;
        let d = if (x + y) % 64 < 2 { 6 } else { 0 };
        std::array::from_fn(|i| (BG_TOP[i] + (BG_BOTTOM[i] - BG_TOP[i]) * t / 256 + d) as u8)
    }
}

/// A light window with a coloured title bar and lines of pseudo text in `ink`.
/// `scroll` shifts the text vertically (scrolling pane).
fn window_pixel(x: u32, y: u32, title: [u8; 3], ink: [u8; 3], seed: u64, scroll: u32) -> [u8; 3] {
    const TITLE_H: u32 = 28;
    if y < TITLE_H {
        return if x < 2 || y < 2 {
            [230, 230, 230]
        } else {
            title
        };
    }
    let ty = y - TITLE_H + scroll;
    let (cell_w, cell_h) = (9u32, 18u32);
    let (col, line) = (x / cell_w, ty / cell_h);
    let (gx, gy) = (x % cell_w, ty % cell_h);
    // Some lines and word gaps stay empty.
    let line_len = 20 + (hash(seed, u64::from(line), 1, 2) % 120) as u32;
    let gap = hash(seed, u64::from(line), u64::from(col), 3).is_multiple_of(7);
    if col < 1 || col > line_len || gap || !(3..15).contains(&gy) || gx >= 7 {
        return [250, 250, 248];
    }
    // 7x12 pseudo glyph from a hash: about 35 % of the cells are set.
    let glyph = hash(seed, u64::from(line), u64::from(col), 4);
    let bit = (gy - 3) * 7 + gx;
    if (glyph.rotate_left(bit) & 0b111) < 3 {
        // Alternate ink colours per word to create chroma edges.
        if hash(seed, u64::from(line), u64::from(col / 6), 5).is_multiple_of(3) {
            [ink[2], ink[0], ink[1]]
        } else {
            ink
        }
    } else {
        [250, 250, 248]
    }
}

fn contains(r: &Rect, x: u32, y: u32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn next(s: &mut u64) -> u64 {
    *s = s.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = *s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn hash(seed: u64, a: u64, b: u64, c: u64) -> u64 {
    let mut s = seed
        ^ a.wrapping_mul(0x9E3779B97F4A7C15)
        ^ b.wrapping_mul(0xC2B2AE3D27D4EB4F)
        ^ c.wrapping_mul(0x165667B19E3779F9);
    next(&mut s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_is_deterministic_and_crops_consistently() {
        let s = Scene {
            seed: 42,
            content: Content::Video,
            width: 800,
            height: 600,
        };
        let full = s.render(5, Rect::new(0, 0, 800, 600));
        assert_eq!(full, s.render(5, Rect::new(0, 0, 800, 600)));
        let crop = s.render(5, Rect::new(100, 50, 300, 150));
        for r in 0..100usize {
            let a = &full[((50 + r) * 800 + 100) * 3..((50 + r) * 800 + 300) * 3];
            assert_eq!(a, &crop[r * 200 * 3..(r + 1) * 200 * 3]);
        }
        assert_ne!(
            full,
            s.render(6, Rect::new(0, 0, 800, 600)),
            "frames must differ"
        );
    }
}
