//! Text for the overlay (bar label, menus): rendered on the CPU into opaque RGBA pictures
//! that the GPU core copies into the view (`OverlayImage`). Font: the desktop's sans-serif
//! (via fontconfig), else the Ubuntu font that ships with egui.

use std::sync::{Arc, OnceLock};

use ab_glyph::{Font, FontArc, PxScale, ScaleFont, point};

fn font() -> &'static FontArc {
    static FONT: OnceLock<FontArc> = OnceLock::new();
    FONT.get_or_init(|| {
        system_font().unwrap_or_else(|| {
            FontArc::try_from_slice(epaint_default_fonts::UBUNTU_LIGHT).expect("bundled font")
        })
    })
}

fn system_font() -> Option<FontArc> {
    let out = std::process::Command::new("fc-match")
        .args(["-f", "%{file}", "sans-serif:weight=medium"])
        .output()
        .ok()?;
    let path = String::from_utf8(out.stdout).ok()?;
    if !(path.ends_with(".ttf") || path.ends_with(".otf")) {
        return None;
    }
    FontArc::try_from_vec(std::fs::read(path.trim()).ok()?).ok()
}

pub type Rgb = [u8; 3];

/// Packs a colour as the GPU core expects it (R in the lowest byte, opaque).
pub fn pack(c: Rgb) -> u32 {
    u32::from(c[0]) | (u32::from(c[1]) << 8) | (u32::from(c[2]) << 16) | 0xFF00_0000
}

/// Horizontal placement of the text in its box.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Align {
    Left,
    Center,
}

/// Width of `text` in pixels at `size`.
pub fn width(text: &str, size: f32) -> f32 {
    let f = font().as_scaled(PxScale::from(size));
    let mut w = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        let id = f.glyph_id(ch);
        if let Some(p) = prev {
            w += f.kern(p, id);
        }
        w += f.h_advance(id);
        prev = Some(id);
    }
    w
}

/// Shortens `text` with "…" until it fits into `max` pixels.
pub fn fit(text: &str, size: f32, max: f32) -> String {
    if width(text, size) <= max {
        return text.to_string();
    }
    let mut chars: Vec<char> = text.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let s: String = chars.iter().collect::<String>().trim_end().to_string() + "…";
        if width(&s, size) <= max {
            return s;
        }
    }
    String::new()
}

/// Draws `text` into a `w`×`h` box filled with `bg`, vertically centred. Too long text is
/// shortened with "…".
pub fn render(text: &str, size: f32, w: u32, h: u32, fg: Rgb, bg: Rgb, align: Align) -> Arc<[u32]> {
    let mut px = vec![pack(bg); (w * h) as usize];
    draw(&mut px, w, h, 0, 0, w, h, text, size, fg, bg, align);
    px.into()
}

/// Draws `text` into the box (`x`, `y`, `bw`, `bh`) of an existing picture `w`×`h` whose
/// background there is `bg`.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    px: &mut [u32],
    w: u32,
    h: u32,
    x: u32,
    y: u32,
    bw: u32,
    bh: u32,
    text: &str,
    size: f32,
    fg: Rgb,
    bg: Rgb,
    align: Align,
) {
    let f = font().as_scaled(PxScale::from(size));
    let text = fit(text, size, bw as f32);
    let tw = width(&text, size);
    let x0 = x as f32
        + match align {
            Align::Left => 0.0,
            Align::Center => ((bw as f32 - tw) / 2.0).max(0.0).floor(),
        };
    // baseline: centre the ascent/descent box
    let line = f.ascent() - f.descent();
    let baseline = y as f32 + ((bh as f32 - line) / 2.0).floor() + f.ascent();
    let mut pen = x0;
    let mut prev = None;
    let clip = (x + bw).min(w);
    let clip_y = (y + bh).min(h);
    for ch in text.chars() {
        let id = f.glyph_id(ch);
        if let Some(p) = prev {
            pen += f.kern(p, id);
        }
        let g = id.with_scale_and_position(PxScale::from(size), point(pen, baseline.round()));
        pen += f.h_advance(id);
        prev = Some(id);
        let Some(o) = font().outline_glyph(g) else {
            continue;
        };
        let b = o.px_bounds();
        o.draw(|gx, gy, cov| {
            let xx = b.min.x as i32 + gx as i32;
            let yy = b.min.y as i32 + gy as i32;
            if xx < x as i32 || yy < y as i32 || xx >= clip as i32 || yy >= clip_y as i32 {
                return;
            }
            // slight boost: thin strokes stay readable on dark backgrounds
            let a = cov.clamp(0.0, 1.0).powf(0.8);
            let mix = |i: usize| {
                (f32::from(bg[i]) + (f32::from(fg[i]) - f32::from(bg[i])) * a).round() as u8
            };
            px[(yy as u32 * w + xx as u32) as usize] = pack([mix(0), mix(1), mix(2)]);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_and_fits() {
        let bg = [43, 43, 43];
        let px = render("Arbeit", 15.0, 120, 24, [235, 235, 235], bg, Align::Center);
        assert_eq!(px.len(), 120 * 24);
        // some pixels are text, the corners stay background
        assert!(px.iter().any(|&p| p != pack(bg)));
        assert_eq!(px[0], pack(bg));
        let long = "Ein sehr langer Favoritenname, der nicht passt";
        let s = fit(long, 15.0, 100.0);
        assert!(s.ends_with('…') && width(&s, 15.0) <= 100.0);
        assert_eq!(fit("kurz", 15.0, 100.0), "kurz");
    }
}
