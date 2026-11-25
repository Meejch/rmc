//! H.264 decoding with libavcodec on one of three backends:
//!
//! - **CUDA (NVDEC):** pictures stay in device memory (`AV_PIX_FMT_CUDA`, NV12); the core
//!   copies them GPU to GPU into the Vulkan view buffers.
//! - **VA-API (AMD, Intel, other Mesa drivers):** decoded in hardware, transferred to host
//!   memory (NV12) and written into host-visible view buffers.
//! - **Software:** libavcodec on the CPU (YUV 4:2:0), same upload as VA-API.
//!
//! One decoder per surface, low-delay: every access unit yields its picture immediately. A
//! hardware decoder that fails on its first picture switches itself to software decoding.

use std::ffi::{CString, c_void};
use std::ptr;

use anyhow::{Result, anyhow, bail};
use ffmpeg_sys_next as ff;

use crate::cuda::{CuContext, CuStream};

/// Decoder backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Cuda,
    Vaapi,
    Software,
}

impl Backend {
    /// Short German label for logs and the launcher.
    pub fn label(self) -> &'static str {
        match self {
            Backend::Cuda => "NVIDIA NVDEC",
            Backend::Vaapi => "VA-API",
            Backend::Software => "CPU (Software)",
        }
    }
}

/// Layout of `AVCUDADeviceContext` (libavutil/hwcontext_cuda.h, not part of the bindings
/// because it needs cuda.h).
#[repr(C)]
struct AvCudaDeviceContext {
    cuda_ctx: CuContext,
    stream: CuStream,
    internal: *mut c_void,
}

/// FFmpeg hardware device shared by all decoders (none for software decoding).
pub struct HwDevice {
    backend: Backend,
    device: *mut ff::AVBufferRef,
    /// Human readable description (render node and driver for VA-API).
    pub description: String,
}

// SAFETY: the AVBufferRef is reference counted and only read after creation; FFmpeg device
// contexts may be shared by decoders on different threads.
unsafe impl Send for HwDevice {}
unsafe impl Sync for HwDevice {}

impl HwDevice {
    /// Opens CUDA device `device` (e.g. "0").
    pub fn cuda(device: &str) -> Result<Self> {
        let dev = create_device(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA, Some(device))?;
        Ok(Self {
            backend: Backend::Cuda,
            device: dev,
            description: format!("CUDA-Gerät {device}"),
        })
    }

    /// Opens a VA-API device: the given render node, else the first one that opens and
    /// decodes H.264 (tried with a tiny test stream).
    pub fn vaapi(node: Option<&str>) -> Result<Self> {
        let nodes: Vec<String> = match node {
            Some(n) => vec![n.to_string()],
            None => render_nodes(),
        };
        let mut last = anyhow!("no DRM render node found");
        for n in nodes {
            match create_device(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI, Some(&n)) {
                Ok(dev) => {
                    let d = Self {
                        backend: Backend::Vaapi,
                        device: dev,
                        description: describe_node(&n),
                    };
                    match d.probe() {
                        Ok(()) => return Ok(d),
                        Err(e) => last = e.context(format!("{n}: H.264 test decode failed")),
                    }
                }
                Err(e) => last = e.context(n),
            }
        }
        Err(last)
    }

    pub fn software() -> Self {
        Self {
            backend: Backend::Software,
            device: ptr::null_mut(),
            description: "CPU".into(),
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// CUDA context and stream FFmpeg decodes on (copies must use the same stream).
    pub fn cuda_context(&self) -> Option<(CuContext, CuStream)> {
        if self.backend != Backend::Cuda {
            return None;
        }
        unsafe {
            let hw = (*self.device).data as *mut ff::AVHWDeviceContext;
            let cu = (*hw).hwctx as *mut AvCudaDeviceContext;
            Some(((*cu).cuda_ctx, (*cu).stream))
        }
    }

    /// Decodes a small embedded H.264 picture: the driver can really decode H.264.
    fn probe(&self) -> Result<()> {
        let mut dec = Decoder::new(self)?;
        dec.fallback_allowed = false;
        dec.decode(PROBE_H264)?;
        Ok(())
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        if !self.device.is_null() {
            unsafe { ff::av_buffer_unref(&mut self.device) };
        }
    }
}

fn create_device(kind: ff::AVHWDeviceType, name: Option<&str>) -> Result<*mut ff::AVBufferRef> {
    unsafe {
        let mut dev = ptr::null_mut();
        let name = name.map(CString::new).transpose()?;
        let r = ff::av_hwdevice_ctx_create(
            &mut dev,
            kind,
            name.as_ref().map_or(ptr::null(), |n| n.as_ptr()),
            ptr::null_mut(),
            0,
        );
        if r < 0 {
            return Err(averr(r, "av_hwdevice_ctx_create"));
        }
        Ok(dev)
    }
}

/// DRM render nodes, AMD and Intel first (NVIDIA has NVDEC through CUDA).
fn render_nodes() -> Vec<String> {
    let mut v: Vec<(u8, String)> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            name.starts_with("renderD").then(|| {
                let vendor =
                    std::fs::read_to_string(format!("/sys/class/drm/{name}/device/vendor"))
                        .unwrap_or_default();
                let rank = match vendor.trim() {
                    "0x1002" | "0x8086" => 0,
                    "0x10de" => 2,
                    _ => 1,
                };
                (rank, format!("/dev/dri/{name}"))
            })
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, n)| n).collect()
}

/// "AMD (amdgpu, /dev/dri/renderD128)" from sysfs.
fn describe_node(node: &str) -> String {
    let name = node.rsplit('/').next().unwrap_or(node);
    let base = format!("/sys/class/drm/{name}/device");
    let vendor = match std::fs::read_to_string(format!("{base}/vendor"))
        .unwrap_or_default()
        .trim()
    {
        "0x1002" => "AMD",
        "0x8086" => "Intel",
        "0x10de" => "NVIDIA",
        _ => "GPU",
    };
    let driver = std::fs::read_link(format!("{base}/driver"))
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "?".into());
    format!("{vendor} ({driver}, {node})")
}

/// One decoded picture in device memory (NV12, pitched planes).
pub struct DevicePicture {
    pub width: u32,
    pub height: u32,
    pub y: u64,
    pub y_pitch: usize,
    pub uv: u64,
    pub uv_pitch: usize,
}

/// One decoded picture in host memory, valid until the next `decode`.
pub struct HostPicture {
    pub width: u32,
    pub height: u32,
    pub y: *const u8,
    pub y_pitch: usize,
    pub chroma: HostChroma,
}

pub enum HostChroma {
    /// Interleaved UV (NV12).
    Nv12 { uv: *const u8, pitch: usize },
    /// Separate U and V planes (I420).
    I420 {
        u: *const u8,
        v: *const u8,
        pitch: usize,
    },
}

pub enum Picture {
    Device(DevicePicture),
    Host(HostPicture),
}

impl HostPicture {
    /// Writes the picture as NV12 with `stride` x `rows` (Y plane, then interleaved UV) to
    /// `dst`, which must hold `stride * rows * 3 / 2` bytes.
    ///
    /// # Safety
    /// `dst` must be valid for that many bytes; the picture's planes must still be valid.
    pub unsafe fn write_nv12(&self, dst: *mut u8, stride: usize, rows: usize) {
        let w = stride.min(self.width as usize);
        let h = rows.min(self.height as usize);
        unsafe {
            for r in 0..h {
                ptr::copy_nonoverlapping(self.y.add(r * self.y_pitch), dst.add(r * stride), w);
            }
            let uv_dst = dst.add(stride * rows);
            match self.chroma {
                HostChroma::Nv12 { uv, pitch } => {
                    for r in 0..h / 2 {
                        ptr::copy_nonoverlapping(uv.add(r * pitch), uv_dst.add(r * stride), w);
                    }
                }
                HostChroma::I420 { u, v, pitch } => {
                    for r in 0..h / 2 {
                        let (us, vs) = (u.add(r * pitch), v.add(r * pitch));
                        let row = uv_dst.add(r * stride);
                        for c in 0..w / 2 {
                            *row.add(2 * c) = *us.add(c);
                            *row.add(2 * c + 1) = *vs.add(c);
                        }
                    }
                }
            }
        }
    }

    /// Tight I420 copy (CPU reference path).
    pub fn to_i420(&self) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let mut out = vec![0u8; w * h * 3 / 2];
        unsafe {
            for r in 0..h {
                let src = std::slice::from_raw_parts(self.y.add(r * self.y_pitch), w);
                out[r * w..(r + 1) * w].copy_from_slice(src);
            }
            let (u_off, v_off) = (w * h, w * h + w * h / 4);
            for r in 0..h / 2 {
                for c in 0..w / 2 {
                    let (uu, vv) = match self.chroma {
                        HostChroma::Nv12 { uv, pitch } => {
                            (*uv.add(r * pitch + 2 * c), *uv.add(r * pitch + 2 * c + 1))
                        }
                        HostChroma::I420 { u, v, pitch } => {
                            (*u.add(r * pitch + c), *v.add(r * pitch + c))
                        }
                    };
                    out[u_off + r * w / 2 + c] = uu;
                    out[v_off + r * w / 2 + c] = vv;
                }
            }
        }
        out
    }
}

unsafe extern "C" fn pick_cuda(
    _ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    unsafe { pick(fmts, ff::AVPixelFormat::AV_PIX_FMT_CUDA) }
}

unsafe extern "C" fn pick_vaapi(
    _ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    unsafe { pick(fmts, ff::AVPixelFormat::AV_PIX_FMT_VAAPI) }
}

unsafe fn pick(fmts: *const ff::AVPixelFormat, want: ff::AVPixelFormat) -> ff::AVPixelFormat {
    let mut p = fmts;
    unsafe {
        while *p != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == want {
                return *p;
            }
            p = p.add(1);
        }
    }
    ff::AVPixelFormat::AV_PIX_FMT_NONE
}

fn averr(code: i32, what: &str) -> anyhow::Error {
    let mut buf = [0 as std::ffi::c_char; 128];
    unsafe { ff::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    let msg = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    anyhow!("{what}: {msg} ({code})")
}

/// Opens an H.264 decoder context for `backend` on `device` (null for software).
unsafe fn open_codec(
    backend: Backend,
    device: *mut ff::AVBufferRef,
) -> Result<*mut ff::AVCodecContext> {
    unsafe {
        let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_H264);
        if codec.is_null() {
            bail!("no H.264 decoder in libavcodec");
        }
        let ctx = ff::avcodec_alloc_context3(codec);
        if ctx.is_null() {
            bail!("avcodec_alloc_context3 failed");
        }
        (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
        match backend {
            Backend::Cuda | Backend::Vaapi => {
                (*ctx).hw_device_ctx = ff::av_buffer_ref(device);
                (*ctx).get_format = Some(if backend == Backend::Cuda {
                    pick_cuda
                } else {
                    pick_vaapi
                });
                (*ctx).thread_count = 1;
            }
            Backend::Software => {
                // Slice threads keep the low delay (frame threads would add latency).
                (*ctx).thread_type = ff::FF_THREAD_SLICE;
                (*ctx).thread_count = 4;
            }
        }
        let mut ctx = ctx;
        let r = ff::avcodec_open2(ctx, codec, ptr::null_mut());
        if r < 0 {
            ff::avcodec_free_context(&mut ctx);
            return Err(averr(r, "avcodec_open2"));
        }
        Ok(ctx)
    }
}

pub struct Decoder {
    backend: Backend,
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    sw_frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
    buf: Vec<u8>,
    /// H.264 parser, only for reading the coded picture size from the SPS.
    parser: *mut ff::AVCodecParserContext,
    /// No picture decoded yet (a hardware failure here switches to software).
    first: bool,
    fallback_allowed: bool,
}

impl Decoder {
    /// Creates an H.264 decoder on the shared device.
    pub fn new(hw: &HwDevice) -> Result<Self> {
        unsafe {
            let ctx = open_codec(hw.backend, hw.device)?;
            Ok(Self {
                backend: hw.backend,
                ctx,
                frame: ff::av_frame_alloc(),
                sw_frame: ff::av_frame_alloc(),
                pkt: ff::av_packet_alloc(),
                buf: Vec::new(),
                parser: ptr::null_mut(),
                first: true,
                fallback_allowed: true,
            })
        }
    }

    /// Backend this decoder currently uses (software after a hardware fallback).
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Coded picture size (macroblock aligned, as decoded) known after an access unit with an
    /// SPS has been seen. Servers may encode wider than the surface (Windows: 3440 -> 3456);
    /// the AVC444v2 auxiliary layout depends on this width.
    pub fn coded_size(&mut self, au: &[u8]) -> Option<(u32, u32)> {
        unsafe {
            if self.parser.is_null() {
                self.parser = ff::av_parser_init(ff::AVCodecID::AV_CODEC_ID_H264 as i32);
                if self.parser.is_null() {
                    return None;
                }
                (*self.parser).flags |= ff::PARSER_FLAG_COMPLETE_FRAMES;
            }
            self.buf.clear();
            self.buf.extend_from_slice(au);
            self.buf
                .resize(au.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize, 0);
            let (mut out, mut out_size) = (ptr::null_mut(), 0);
            ff::av_parser_parse2(
                self.parser,
                self.ctx,
                &mut out,
                &mut out_size,
                self.buf.as_ptr(),
                au.len() as i32,
                ff::AV_NOPTS_VALUE,
                ff::AV_NOPTS_VALUE,
                0,
            );
            let (w, h) = ((*self.parser).coded_width, (*self.parser).coded_height);
            (w > 0 && h > 0).then_some((w as u32, h as u32))
        }
    }

    /// Decodes one access unit and returns its picture.
    pub fn decode(&mut self, au: &[u8]) -> Result<Picture> {
        match self.decode_once(au) {
            Ok(p) => {
                self.first = false;
                Ok(p)
            }
            Err(e) if self.first && self.fallback_allowed && self.backend != Backend::Software => {
                eprintln!(
                    "Grafik: Hardware-Dekodierung ({}) fehlgeschlagen ({e:#}), weiter mit CPU",
                    self.backend.label()
                );
                unsafe {
                    ff::avcodec_free_context(&mut self.ctx);
                    self.ctx = open_codec(Backend::Software, ptr::null_mut())?;
                }
                self.backend = Backend::Software;
                let p = self.decode_once(au)?;
                self.first = false;
                Ok(p)
            }
            Err(e) => Err(e),
        }
    }

    fn decode_once(&mut self, au: &[u8]) -> Result<Picture> {
        unsafe {
            self.buf.clear();
            self.buf.extend_from_slice(au);
            self.buf
                .resize(au.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize, 0);
            (*self.pkt).data = self.buf.as_mut_ptr();
            (*self.pkt).size = au.len() as i32;
            let r = ff::avcodec_send_packet(self.ctx, self.pkt);
            if r < 0 {
                return Err(averr(r, "avcodec_send_packet"));
            }
            ff::av_frame_unref(self.frame);
            let r = ff::avcodec_receive_frame(self.ctx, self.frame);
            if r < 0 {
                return Err(averr(
                    r,
                    "avcodec_receive_frame (low delay expected one picture per AU)",
                ));
            }
            let f = &*self.frame;
            match self.backend {
                Backend::Cuda => {
                    if f.format != ff::AVPixelFormat::AV_PIX_FMT_CUDA as i32 {
                        bail!("decoder did not use NVDEC (format {})", f.format);
                    }
                    Ok(Picture::Device(DevicePicture {
                        width: f.width as u32,
                        height: f.height as u32,
                        y: f.data[0] as u64,
                        y_pitch: f.linesize[0] as usize,
                        uv: f.data[1] as u64,
                        uv_pitch: f.linesize[1] as usize,
                    }))
                }
                Backend::Vaapi => {
                    if f.format != ff::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 {
                        bail!("decoder did not use VA-API (format {})", f.format);
                    }
                    ff::av_frame_unref(self.sw_frame);
                    (*self.sw_frame).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
                    let r = ff::av_hwframe_transfer_data(self.sw_frame, self.frame, 0);
                    if r < 0 {
                        return Err(averr(r, "av_hwframe_transfer_data"));
                    }
                    host_picture(&*self.sw_frame)
                }
                Backend::Software => host_picture(f),
            }
        }
    }

    /// Downloads the last picture to host memory as tight I420 (CPU reference path).
    pub fn download_i420(&mut self) -> Result<Vec<u8>> {
        unsafe {
            match self.backend {
                Backend::Cuda => {
                    ff::av_frame_unref(self.sw_frame);
                    let r = ff::av_hwframe_transfer_data(self.sw_frame, self.frame, 0);
                    if r < 0 {
                        return Err(averr(r, "av_hwframe_transfer_data"));
                    }
                    Ok(host_picture_of(&*self.sw_frame)?.to_i420())
                }
                Backend::Vaapi => Ok(host_picture_of(&*self.sw_frame)?.to_i420()),
                Backend::Software => Ok(host_picture_of(&*self.frame)?.to_i420()),
            }
        }
    }
}

fn host_picture(f: &ff::AVFrame) -> Result<Picture> {
    host_picture_of(f).map(Picture::Host)
}

fn host_picture_of(f: &ff::AVFrame) -> Result<HostPicture> {
    let nv12 = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
    let i420 = [
        ff::AVPixelFormat::AV_PIX_FMT_YUV420P as i32,
        ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32,
    ];
    let chroma = if f.format == nv12 {
        HostChroma::Nv12 {
            uv: f.data[1],
            pitch: f.linesize[1] as usize,
        }
    } else if i420.contains(&f.format) {
        if f.linesize[1] != f.linesize[2] {
            bail!("I420 with different U/V pitches");
        }
        HostChroma::I420 {
            u: f.data[1],
            v: f.data[2],
            pitch: f.linesize[1] as usize,
        }
    } else {
        bail!(
            "unexpected picture format {} (want NV12 or YUV420P)",
            f.format
        );
    };
    Ok(HostPicture {
        width: f.width as u32,
        height: f.height as u32,
        y: f.data[0],
        y_pitch: f.linesize[0] as usize,
        chroma,
    })
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_packet_free(&mut self.pkt);
            ff::av_frame_free(&mut self.frame);
            ff::av_frame_free(&mut self.sw_frame);
            ff::avcodec_free_context(&mut self.ctx);
            if !self.parser.is_null() {
                ff::av_parser_close(self.parser);
            }
        }
    }
}

/// A 128x128 H.264 IDR picture (main profile, grey, Annex B) for probing a hardware decoder.
const PROBE_H264: &[u8] = include_bytes!("probe-128.bin");
