//! Conversions between Windows clipboard formats and Linux MIME types.

use anyhow::{Result, bail};

/// Windows predefined clipboard formats.
pub const CF_TEXT: u32 = 1;
pub const CF_DIB: u32 = 8;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_DIBV5: u32 = 17;
/// Registered format name for PNG images (Windows 10+ apps put it next to CF_DIB).
pub const PNG_NAME: &str = "PNG";

/// MIME types offered for text.
pub const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
pub const PNG_MIME: &str = "image/png";

/// The best local text MIME type of a selection.
pub fn local_text_mime(mimes: &[String]) -> Option<&str> {
    TEXT_MIMES
        .iter()
        .find(|t| mimes.iter().any(|m| m == *t))
        .copied()
}

/// CF_UNICODETEXT (UTF-16LE, NUL-terminated, CRLF) -> UTF-8 with LF.
pub fn utf16_to_text(data: &[u8]) -> String {
    let units: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units).replace("\r\n", "\n")
}

/// UTF-8 (LF or CRLF) -> CF_UNICODETEXT (UTF-16LE, CRLF, NUL-terminated).
pub fn text_to_utf16(text: &str) -> Vec<u8> {
    let crlf = text.replace("\r\n", "\n").replace('\n', "\r\n");
    let mut out: Vec<u8> = crlf.encode_utf16().flat_map(u16::to_le_bytes).collect();
    out.extend_from_slice(&[0, 0]);
    out
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// CF_DIB / CF_DIBV5 (BITMAPINFOHEADER or V4/V5 + pixels, 24/32 bpp) -> PNG.
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    if dib.len() < 40 {
        bail!("DIB too short");
    }
    let header = u32_at(dib, 0) as usize;
    let width = u32_at(dib, 4) as i32;
    let height = u32_at(dib, 8) as i32;
    let bits = u16_at(dib, 14);
    let compression = u32_at(dib, 16);
    let colors = u32_at(dib, 32) as usize;
    if width <= 0 || height == 0 || header < 40 || header > dib.len() {
        bail!("unsupported DIB header");
    }
    if !(bits == 24 || bits == 32) {
        bail!("unsupported DIB depth {bits}");
    }
    // BI_RGB = 0, BI_BITFIELDS = 3, BI_ALPHABITFIELDS = 6
    if !matches!(compression, 0 | 3 | 6) {
        bail!("unsupported DIB compression {compression}");
    }
    let masks = if header == 40 && compression == 3 {
        12
    } else if header == 40 && compression == 6 {
        16
    } else {
        0
    };
    let offset = header + masks + colors * 4;
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    let bpp = bits as usize / 8;
    let stride = (w * bpp).div_ceil(4) * 4;
    if dib.len() < offset + stride * h {
        bail!("DIB pixel data truncated");
    }
    let mut rgba = vec![0u8; w * h * 4];
    let mut any_alpha = false;
    for y in 0..h {
        // bottom-up unless the height is negative
        let src_row = if height > 0 { h - 1 - y } else { y };
        let row = &dib[offset + src_row * stride..][..w * bpp];
        for x in 0..w {
            let p = &row[x * bpp..];
            let o = (y * w + x) * 4;
            rgba[o] = p[2];
            rgba[o + 1] = p[1];
            rgba[o + 2] = p[0];
            rgba[o + 3] = if bpp == 4 { p[3] } else { 255 };
            any_alpha |= bpp == 4 && p[3] != 0;
        }
    }
    if bpp == 4 && !any_alpha {
        // 32 bpp BI_RGB usually leaves the alpha byte at 0: opaque
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()?;
        writer.write_image_data(&rgba)?;
    }
    Ok(out)
}

/// PNG -> CF_DIB (BITMAPINFOHEADER, 32 bpp BGRA, bottom-up).
pub fn png_to_dib(png_data: &[u8]) -> Result<Vec<u8>> {
    let mut dec = png::Decoder::new(png_data);
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf)?;
    let (w, h) = (info.width as usize, info.height as usize);
    let px = &buf[..info.buffer_size()];
    let channels = match info.color_type {
        png::ColorType::Rgba => 4,
        png::ColorType::Rgb => 3,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Grayscale => 1,
        png::ColorType::Indexed => bail!("indexed PNG not expanded"),
    };
    let line = info.line_size;
    let mut out = Vec::with_capacity(40 + w * h * 4);
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // positive: bottom-up
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&((w * h * 4) as u32).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes()); // 72 dpi
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        let row = &px[y * line..];
        for x in 0..w {
            let p = &row[x * channels..];
            let (r, g, b, a) = match channels {
                4 => (p[0], p[1], p[2], p[3]),
                3 => (p[0], p[1], p[2], 255),
                2 => (p[0], p[0], p[0], p[1]),
                _ => (p[0], p[0], p[0], 255),
            };
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_roundtrip() {
        let t = "Zeile 1\nÄrger mit Umlauten ✓\n";
        let w = text_to_utf16(t);
        assert_eq!(&w[w.len() - 2..], &[0, 0]);
        assert_eq!(utf16_to_text(&w), t);
        // CRLF from Windows becomes LF
        let win: Vec<u8> = "a\r\nb\0rest"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(utf16_to_text(&win), "a\nb");
    }

    #[test]
    fn image_roundtrip() {
        // 3x2 RGBA image through PNG -> DIB -> PNG
        let mut src = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut src, 3, 2);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[
                255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, //
                10, 20, 30, 128, 40, 50, 60, 255, 70, 80, 90, 0,
            ])
            .unwrap();
        }
        let dib = png_to_dib(&src).unwrap();
        assert_eq!(dib.len(), 40 + 3 * 2 * 4);
        // first stored row is the bottom row (bottom-up), BGRA
        assert_eq!(&dib[40..44], &[30, 20, 10, 128]);
        let png2 = png_to_dib(&dib_to_png(&dib).unwrap()).unwrap();
        assert_eq!(dib, png2);
    }

    #[test]
    fn dib24_padding() {
        // 1x1 24 bpp BI_RGB, row padded to 4 bytes
        let mut dib = Vec::new();
        for v in [40u32, 1, 1] {
            dib.extend_from_slice(&v.to_le_bytes());
        }
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&[0; 24]);
        dib.extend_from_slice(&[1, 2, 3, 0]);
        let png = dib_to_png(&dib).unwrap();
        let back = png_to_dib(&png).unwrap();
        assert_eq!(&back[40..44], &[1, 2, 3, 255]);
    }
}
