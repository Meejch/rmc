//! Colour conversion of MS-RDPEGFX 3.3.8.3.1 (full range, fixed point /256).
//!
//! These exact integer formulas are what the GPU shader must reproduce for AVC444.

#[inline]
pub fn clamp_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Forward transform RGB -> YUV.
#[inline]
pub fn rgb_to_yuv(r: u8, g: u8, b: u8) -> (u8, u8, u8) {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    (
        clamp_u8((54 * r + 183 * g + 18 * b) >> 8),
        clamp_u8(((-29 * r - 99 * g + 128 * b) >> 8) + 128),
        clamp_u8(((128 * r - 116 * g - 12 * b) >> 8) + 128),
    )
}

/// Reverse transform YUV -> RGB.
#[inline]
pub fn yuv_to_rgb(y: u8, u: u8, v: u8) -> (u8, u8, u8) {
    let luma = 256 * i32::from(y);
    let u = i32::from(u) - 128;
    let v = i32::from(v) - 128;
    (
        clamp_u8((luma + 403 * v) >> 8),
        clamp_u8((luma - 48 * u - 120 * v) >> 8),
        clamp_u8((luma + 475 * u) >> 8),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grey_axis_has_neutral_chroma() {
        for g in [0u8, 1, 64, 128, 200, 255] {
            let (_, u, v) = rgb_to_yuv(g, g, g);
            assert!((i32::from(u) - 128).abs() <= 1, "u={u} for grey {g}");
            assert!((i32::from(v) - 128).abs() <= 1, "v={v} for grey {g}");
        }
    }

    #[test]
    fn round_trip_error_is_small() {
        // Coarse sweep over the RGB cube; fixed-point rounding costs a few levels.
        let mut worst = 0;
        for r in (0..=255).step_by(15) {
            for g in (0..=255).step_by(15) {
                for b in (0..=255).step_by(15) {
                    let (y, u, v) = rgb_to_yuv(r as u8, g as u8, b as u8);
                    let (r2, g2, b2) = yuv_to_rgb(y, u, v);
                    let d = (r - i32::from(r2))
                        .abs()
                        .max((g - i32::from(g2)).abs())
                        .max((b - i32::from(b2)).abs());
                    worst = worst.max(d);
                }
            }
        }
        assert!(worst <= 6, "worst round-trip error {worst}");
    }

    #[test]
    fn primaries() {
        assert_eq!(yuv_to_rgb(0, 128, 128), (0, 0, 0));
        assert_eq!(yuv_to_rgb(255, 128, 128), (255, 255, 255));
        let (r, g, b) = yuv_to_rgb(
            rgb_to_yuv(255, 0, 0).0,
            rgb_to_yuv(255, 0, 0).1,
            rgb_to_yuv(255, 0, 0).2,
        );
        assert!(r > 245 && g < 10 && b < 10, "red -> ({r},{g},{b})");
    }
}
