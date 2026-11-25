//! Minimal H.264 Annex B helpers: split a stream into access units at AUD NAL units.

/// Byte range of one access unit (one coded picture) inside the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessUnit {
    pub offset: usize,
    pub len: usize,
}

/// Start offsets (of the start code) and NAL types of all NAL units.
pub fn nal_units(data: &[u8]) -> Vec<(usize, u8)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            let start = if i > 0 && data[i - 1] == 0 { i - 1 } else { i };
            if let Some(&h) = data.get(i + 3) {
                out.push((start, h & 0x1F));
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// Splits at access unit delimiters (NAL type 9). The encoder must emit AUDs (`aud=1`).
pub fn split_access_units(data: &[u8]) -> Vec<AccessUnit> {
    let starts: Vec<usize> = nal_units(data)
        .into_iter()
        .filter(|&(_, t)| t == 9)
        .map(|(o, _)| o)
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(k, &o)| {
            let end = starts.get(k + 1).copied().unwrap_or(data.len());
            AccessUnit {
                offset: o,
                len: end - o,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_at_aud() {
        // AUD, SPS, AUD, slice  (mixed 3- and 4-byte start codes)
        let s = [
            0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x09, 0xF0, 0, 0, 1, 0x65, 9,
            9,
        ];
        let aus = split_access_units(&s);
        assert_eq!(
            aus,
            vec![
                AccessUnit { offset: 0, len: 12 },
                AccessUnit {
                    offset: 12,
                    len: 12
                }
            ]
        );
        assert_eq!(
            nal_units(&s).iter().map(|n| n.1).collect::<Vec<_>>(),
            vec![9, 7, 9, 5]
        );
    }
}
