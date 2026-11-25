//! GPU core of RMC (docs/architektur.md).
//!
//! H.264 pictures are decoded by NVDEC (FFmpeg, CUDA hardware context), copied
//! device-to-device into Vulkan buffers and combined/converted by compute shaders that
//! reproduce the CPU reference in `rdpgfx-model` bit for bit. `core::GpuCore` holds
//! the surface model on the GPU: surfaces as images, fills, copies, cache, uploads.

pub mod arena;
pub mod core;
pub mod cuda;
pub mod decode;
pub mod profile;
pub mod vk;
