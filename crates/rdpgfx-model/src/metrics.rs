//! Image quality metrics for fixture and GPU verification.

/// Peak signal-to-noise ratio in dB between two equally sized byte buffers (inf if equal).
pub fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "psnr: size mismatch");
    if a.is_empty() {
        return f64::INFINITY;
    }
    let se: u64 = a
        .iter()
        .zip(b)
        .map(|(&x, &y)| (i64::from(x) - i64::from(y)).unsigned_abs().pow(2))
        .sum();
    if se == 0 {
        return f64::INFINITY;
    }
    let mse = se as f64 / a.len() as f64;
    10.0 * (255.0 * 255.0 / mse).log10()
}

/// Largest absolute per-byte difference.
pub fn max_abs_diff(a: &[u8], b: &[u8]) -> u8 {
    assert_eq!(a.len(), b.len(), "max_abs_diff: size mismatch");
    a.iter()
        .zip(b)
        .map(|(&x, &y)| x.abs_diff(y))
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psnr_basics() {
        assert!(psnr(&[1, 2, 3], &[1, 2, 3]).is_infinite());
        let p = psnr(&[0; 100], &[1; 100]);
        assert!((p - 48.13).abs() < 0.01, "{p}");
        assert_eq!(max_abs_diff(&[0, 10], &[3, 4]), 6);
    }
}
