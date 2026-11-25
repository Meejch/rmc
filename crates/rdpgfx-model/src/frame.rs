//! Plane containers. All planes are padded to whole macroblocks, like H.264 pictures.

use rayon::prelude::*;

use crate::color::{rgb_to_yuv, yuv_to_rgb};

/// H.264 macroblock size; views and planes are padded to it.
pub const MACROBLOCK: u32 = 16;

pub const fn pad(v: u32) -> u32 {
    v.div_ceil(MACROBLOCK) * MACROBLOCK
}

/// Full-resolution YUV 4:4:4 picture (one byte per sample and plane).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Yuv444Frame {
    pub width: u32,
    pub height: u32,
    /// Padded width = bytes per row of every plane.
    pub stride: u32,
    /// Padded height = rows of every plane.
    pub rows: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl Yuv444Frame {
    /// Black picture (Y=0, U=V=128).
    pub fn new(width: u32, height: u32) -> Self {
        let (stride, rows) = (pad(width), pad(height));
        let n = (stride * rows) as usize;
        Self {
            width,
            height,
            stride,
            rows,
            y: vec![0; n],
            u: vec![128; n],
            v: vec![128; n],
        }
    }

    /// Converts tightly packed RGB (3 bytes per pixel). Padding repeats the last column/row,
    /// which keeps the encoder from wasting bits on an artificial edge.
    pub fn from_rgb(rgb: &[u8], width: u32, height: u32) -> Self {
        assert!(
            rgb.len() >= (width * height * 3) as usize,
            "rgb buffer too small"
        );
        let mut f = Self::new(width, height);
        let stride = f.stride as usize;
        let (w, h) = (width as usize, height as usize);
        f.y.par_chunks_mut(stride)
            .zip(f.u.par_chunks_mut(stride))
            .zip(f.v.par_chunks_mut(stride))
            .enumerate()
            .for_each(|(row, ((y, u), v))| {
                let src_row = row.min(h - 1);
                let line = &rgb[src_row * w * 3..(src_row + 1) * w * 3];
                for col in 0..stride {
                    let c = col.min(w - 1) * 3;
                    let (yy, uu, vv) = rgb_to_yuv(line[c], line[c + 1], line[c + 2]);
                    y[col] = yy;
                    u[col] = uu;
                    v[col] = vv;
                }
            });
        f
    }

    /// Converts to tightly packed RGB (`width` x `height`), without any chroma filter.
    pub fn to_rgb(&self) -> Vec<u8> {
        let (w, h, stride) = (
            self.width as usize,
            self.height as usize,
            self.stride as usize,
        );
        let mut out = vec![0u8; w * h * 3];
        out.par_chunks_mut(w * 3)
            .enumerate()
            .for_each(|(row, line)| {
                let base = row * stride;
                for col in 0..w {
                    let (r, g, b) =
                        yuv_to_rgb(self.y[base + col], self.u[base + col], self.v[base + col]);
                    line[col * 3..col * 3 + 3].copy_from_slice(&[r, g, b]);
                }
            });
        out
    }

    #[inline]
    pub fn idx(&self, row: u32, col: u32) -> usize {
        (row * self.stride + col) as usize
    }
}

/// YUV 4:2:0 picture as decoded from / fed to H.264 (planar I420 layout).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Yuv420Frame {
    /// Padded size (multiple of 16).
    pub width: u32,
    pub height: u32,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

impl Yuv420Frame {
    pub fn new(width: u32, height: u32) -> Self {
        assert!(
            width.is_multiple_of(MACROBLOCK) && height.is_multiple_of(MACROBLOCK),
            "views must be macroblock aligned"
        );
        let n = (width * height) as usize;
        Self {
            width,
            height,
            y: vec![0; n],
            u: vec![128; n / 4],
            v: vec![128; n / 4],
        }
    }

    pub fn chroma_width(&self) -> u32 {
        self.width / 2
    }

    /// Raw I420 bytes (Y, then U, then V), as ffmpeg `-pix_fmt yuv420p` expects.
    pub fn to_i420(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.y.len() * 3 / 2);
        out.extend_from_slice(&self.y);
        out.extend_from_slice(&self.u);
        out.extend_from_slice(&self.v);
        out
    }

    pub fn from_i420(data: &[u8], width: u32, height: u32) -> Self {
        let n = (width * height) as usize;
        assert_eq!(data.len(), n * 3 / 2, "I420 buffer size");
        Self {
            width,
            height,
            y: data[..n].to_vec(),
            u: data[n..n + n / 4].to_vec(),
            v: data[n + n / 4..].to_vec(),
        }
    }

    /// Byte size of one I420 picture.
    pub const fn i420_size(width: u32, height: u32) -> usize {
        (width * height * 3 / 2) as usize
    }
}
