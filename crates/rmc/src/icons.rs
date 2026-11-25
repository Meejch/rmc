//! The RMC icon (assets/icons, logo on a dark tile), embedded for the window, the tray and
//! the desktop entry (no icon theme needed, e.g. in an AppImage).

pub const PNG_22: &[u8] = include_bytes!("../../../assets/icons/rmc-22.png");
pub const PNG_32: &[u8] = include_bytes!("../../../assets/icons/rmc-32.png");
pub const PNG_48: &[u8] = include_bytes!("../../../assets/icons/rmc-48.png");
pub const PNG_64: &[u8] = include_bytes!("../../../assets/icons/rmc-64.png");
pub const PNG_128: &[u8] = include_bytes!("../../../assets/icons/rmc-128.png");
pub const PNG_256: &[u8] = include_bytes!("../../../assets/icons/rmc-256.png");
/// The logo alone (transparent), for the launcher header (UI rebuild, block 3).
#[allow(dead_code)]
pub const LOGO: &[u8] = include_bytes!("../../../assets/rmc-logo.png");

/// Decodes a PNG into straight RGBA8 (width, height, pixels).
pub fn rgba(png_data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(png_data));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let px = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|c| [c[0], c[0], c[0], c[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None,
    };
    Some((info.width, info.height, px))
}

/// The icon as ARGB32 in network byte order (StatusNotifierItem pixmaps).
pub fn argb(png_data: &[u8]) -> Option<(i32, i32, Vec<u8>)> {
    let (w, h, px) = rgba(png_data)?;
    let data = px
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|c| [c[3], c[0], c[1], c[2]])
        .collect();
    Some((w as i32, h as i32, data))
}

#[cfg(test)]
mod tests {
    #[test]
    fn decode() {
        for (png, n) in [(super::PNG_22, 22), (super::PNG_256, 256)] {
            let (w, h, px) = super::rgba(png).unwrap();
            assert_eq!((w, h, px.len()), (n, n, (n * n * 4) as usize));
        }
        assert!(super::rgba(super::LOGO).is_some());
    }
}
