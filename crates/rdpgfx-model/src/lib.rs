//! Protocol-neutral RDPGFX building blocks shared by the fixture generator, the GPU core
//! and its tests.
//!
//! The CPU code here is the *reference*: it defines what the GPU shaders must produce.
//! It follows MS-RDPEGFX 3.3.8.3.1 (colour conversion), 3.3.8.3.2 (AVC444 v1 stream
//! combination) and 3.3.8.3.3 (AVC444v2), cross-checked against FreeRDP 3.33.1
//! (`libfreerdp/primitives/prim_YUV.c`) and IronRDP (`ironrdp-egfx/src/avc444.rs`).

pub mod avc444;
pub mod color;
pub mod frame;
pub mod metrics;
pub mod outputs;
pub mod rect;
pub mod surface;

pub use avc444::{Avc444Layout, ChromaMode};
pub use frame::{MACROBLOCK, Yuv420Frame, Yuv444Frame};
pub use rect::Rect;
pub use surface::{Color, LumaChroma, PixelFormat, SurfaceModel, SurfaceOp};
