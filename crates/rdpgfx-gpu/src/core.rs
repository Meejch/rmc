//! The GPU core: surfaces as Vulkan images, AVC420/AVC444 via NVDEC and compute shaders,
//! solid fills, surface-to-surface copies, the bitmap cache and CPU pixel uploads.
//!
//! Semantics are those of the CPU reference `rdpgfx_model::surface::SurfaceModel`; tests run
//! the same command sequences against both and compare the pixels byte for byte.
//!
//! Commands are recorded into the command buffer of the current slot and submitted by
//! `end_frame` (or earlier, when a resource is about to be reused). A global barrier
//! separates consecutive commands, so a batch behaves exactly like immediate execution.
//! Submits do not wait: every submit signals the next value of one timeline semaphore,
//! `SLOTS` batches can be in flight, and a slot is only reused once its value is reached.
//! Frame acknowledgements are sent by a waiter thread when the present that showed the
//! frame has completed on the GPU.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc;
use std::thread::JoinHandle;

use anyhow::{Context, Result, bail, ensure};
use ash::vk;

use rdpgfx_model::frame::pad;
use rdpgfx_model::outputs::MappedSurface;
use rdpgfx_model::surface::{
    Color, INITIAL_PIXEL, PixelFormat, SurfaceError, SurfaceOp, check_slot, dest_rect, within,
};
use rdpgfx_model::{Avc444Layout, Rect};

use crate::arena::Arena;
use crate::cuda::{
    CU_MEMORYTYPE_DEVICE, CU_MEMORYTYPE_HOST, CuContext, CuStream, Cuda, ImportedBuffer, Memcpy2D,
};
pub use crate::decode::Backend;
use crate::decode::{Decoder, DevicePicture, HwDevice, Picture};
use crate::profile::{Profile, Stage};
use crate::vk::{Buffer, COLOR_LAYERS, COLOR_RANGE, Image, Vk};

mod present;
pub use present::{
    ButtonRow, OverlayImage, OverlayProgress, OverlayState, PresentStats, ViewDiag, ViewId,
};

const SPV_MAIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/avc444_main.spv"));
const SPV_AUX: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/avc444_aux.spv"));
const SPV_CONVERT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/yuv444_to_rgba.spv"));
const SPV_FILL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/fill.spv"));

/// Size of one cache chunk; larger entries get a chunk of their own.
const CACHE_CHUNK: u64 = 32 << 20;
/// Batches (and presents) that may be in flight at the same time.
pub const SLOTS: usize = 3;
/// Initial size of the per-batch staging (host) and scratch (device) buffers.
const TRANSIENT_INITIAL: u64 = 32 << 20;

/// Push constants shared by all shaders (`shaders/common.glsl`).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Params {
    stride: u32,
    rows: u32,
    width: u32,
    height: u32,
    area: [u32; 4],
    luma: [u32; 4],
    layout_v2: u32,
    reverse: u32,
    color: u32,
    pad0: u32,
}

impl Params {
    fn bytes(&self) -> &[u8] {
        // SAFETY: plain repr(C) struct of u32 without padding.
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }
}

const fn rect4(r: &Rect) -> [u32; 4] {
    [r.left, r.top, r.right, r.bottom]
}

/// One surface's AVC update within a frame (see `GpuCore::avc_frame`).
pub struct AvcUpdate<'a> {
    pub id: u16,
    pub layout: Avc444Layout,
    pub main: Option<AvcView<'a>>,
    pub aux: Option<AvcView<'a>>,
}

/// One AVC update view: access unit and region rectangles (surface coordinates).
pub struct AvcView<'a> {
    pub au: &'a [u8],
    pub rects: &'a [Rect],
}

pub struct CoreOptions {
    /// Decoder backend: automatic (NVDEC, else VA-API, else CPU) or a fixed one.
    pub backend: BackendChoice,
    /// CUDA device for NVDEC (the Vulkan device is chosen to match it).
    pub device: String,
    /// VA-API render node (e.g. /dev/dri/renderD128); None = first that decodes H.264.
    pub vaapi_device: Option<String>,
    /// Apply the reverse chroma filter to AVC444 (see docs/upstream/001).
    pub reverse_filter: bool,
    pub max_cache_slots: u16,
    /// Decode the surfaces of a frame in parallel (`avc_frame`).
    pub parallel_decode: bool,
    /// Window system for presentation (`None`: headless, no views).
    pub display: Option<raw_window_handle::RawDisplayHandle>,
}

impl Default for CoreOptions {
    fn default() -> Self {
        Self {
            backend: BackendChoice::Auto,
            device: "0".into(),
            vaapi_device: None,
            reverse_filter: true,
            max_cache_slots: rdpgfx_model::surface::DEFAULT_CACHE_SLOTS,
            parallel_decode: true,
            display: None,
        }
    }
}

/// Which decoder backend to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackendChoice {
    /// NVDEC if an NVIDIA GPU with CUDA is there, else VA-API, else the CPU.
    #[default]
    Auto,
    Cuda,
    Vaapi,
    Software,
}

impl BackendChoice {
    /// From the option value ("auto", "nvidia", "vaapi", "software").
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "nvidia" | "cuda" | "nvdec" => Some(Self::Cuda),
            "vaapi" | "va-api" | "amd" | "intel" => Some(Self::Vaapi),
            "software" | "cpu" => Some(Self::Software),
            _ => None,
        }
    }
}

/// What the core decodes and presents with (for logs and the launcher).
#[derive(Clone, Debug)]
pub struct BackendInfo {
    pub backend: Backend,
    /// Decoding device ("NVIDIA GeForce RTX 5080", "AMD (amdgpu, /dev/dri/renderD128)", "CPU").
    pub decoder: String,
    /// Vulkan device used for composition and presentation.
    pub vulkan: String,
}

impl BackendInfo {
    /// One line, e.g. "NVIDIA NVDEC auf NVIDIA GeForce RTX 5080, Darstellung Vulkan (...)".
    pub fn summary(&self) -> String {
        match self.backend {
            Backend::Software => format!(
                "Dekodierung auf der CPU (keine Hardware-Dekodierung), Darstellung Vulkan ({})",
                self.vulkan
            ),
            b => format!(
                "{} auf {}, Darstellung Vulkan ({})",
                b.label(),
                self.decoder,
                self.vulkan
            ),
        }
    }
}

/// Picks the decoder backend (and loads CUDA for NVDEC).
fn select_backend(opts: &CoreOptions) -> Result<(Option<Cuda>, HwDevice)> {
    let cuda = || -> Result<(Cuda, HwDevice)> {
        let c = Cuda::load()?;
        let hw = HwDevice::cuda(&opts.device)?;
        Ok((c, hw))
    };
    let vaapi = || HwDevice::vaapi(opts.vaapi_device.as_deref());
    Ok(match opts.backend {
        BackendChoice::Cuda => {
            let (c, hw) = cuda().context("NVIDIA-Dekodierung (CUDA/NVDEC) nicht verfügbar")?;
            (Some(c), hw)
        }
        BackendChoice::Vaapi => (None, vaapi().context("VA-API-Dekodierung nicht verfügbar")?),
        BackendChoice::Software => (None, HwDevice::software()),
        BackendChoice::Auto => match cuda() {
            Ok((c, hw)) => (Some(c), hw),
            Err(_) => match vaapi() {
                Ok(hw) => (None, hw),
                Err(_) => (None, HwDevice::software()),
            },
        },
    })
}

/// Decoder and YUV 4:4:4 state of a surface that received AVC updates.
struct AvcState {
    decoder: Decoder,
    stride: u32,
    rows: u32,
    /// NV12 view buffers (main, aux): written by CUDA (exported, device local) or by the
    /// CPU (host visible, mapped) for VA-API and software decoding.
    views: [Buffer; 2],
    /// CUDA imports of the view buffers (empty without CUDA).
    imported: Vec<ImportedBuffer>,
    /// Y, U, V planes (stride x rows each).
    state: [Buffer; 3],
    /// Commands recorded in the current (unsubmitted) batch read the view buffers.
    read_recorded: bool,
    /// Timeline value of the last submitted batch that reads the view buffers.
    read_serial: u64,
}

struct Surface {
    width: u32,
    height: u32,
    format: PixelFormat,
    image: Image,
    origin: Option<(u32, u32)>,
    avc: Option<AvcState>,
}

struct CacheSlot {
    chunk: usize,
    offset: u64,
    size: u64,
    width: u32,
    height: u32,
}

/// Linear per-batch buffer (staging for uploads, scratch for copies).
struct Transient {
    buf: Buffer,
    used: u64,
    host: bool,
    usage: vk::BufferUsageFlags,
}

/// Command buffer and transient buffers of one in-flight batch.
struct Slot {
    cmd: vk::CommandBuffer,
    /// Timeline value signalled by the last submit of this slot.
    serial: u64,
    staging: Transient,
    scratch: Transient,
}

/// Frames waiting for their acknowledgement: (timeline value, frame ids).
type AckJob = (u64, Vec<u64>);

/// Debug aid for tests: host copies (I420) of the views decoded by the last `avc` call.
#[derive(Default)]
pub struct Captured {
    pub main: Option<Vec<u8>>,
    pub aux: Option<Vec<u8>>,
}

pub struct GpuCore {
    /// CUDA driver and the context/stream FFmpeg decodes on (NVDEC backend only).
    cuda: Option<Cuda>,
    hw: HwDevice,
    cu: Option<(CuContext, CuStream)>,
    vk: Vk,
    set_layout: vk::DescriptorSetLayout,
    pipe_layout: vk::PipelineLayout,
    pipe_main: vk::Pipeline,
    pipe_aux: vk::Pipeline,
    pipe_convert: vk::Pipeline,
    pipe_fill: vk::Pipeline,
    slots: Vec<Slot>,
    cur: usize,
    recording: bool,
    timeline: vk::Semaphore,
    /// Value the next submit signals (values start at 1).
    next_serial: Cell<u64>,
    /// Frames ended since the last present.
    ended_frames: Vec<u64>,
    ack_tx: Option<mpsc::Sender<AckJob>>,
    ack_thread: Option<JoinHandle<()>>,
    surfaces: BTreeMap<u16, Surface>,
    map_order: Vec<u16>,
    chunks: Vec<(Buffer, Arena)>,
    cache: HashMap<u16, CacheSlot>,
    max_cache_slots: u16,
    reverse: bool,
    capture: Option<HashMap<u16, Captured>>,
    /// Worker threads for parallel decoding (None: decode one surface after the other).
    decode_pool: Option<rayon::ThreadPool>,
    present: Option<present::PresentRes>,
    pub profile: Profile,
}

impl GpuCore {
    pub fn new(opts: &CoreOptions) -> Result<Self> {
        let (cuda, hw) = select_backend(opts)?;
        let cu = hw.cuda_context();
        // NVDEC: the Vulkan device must be the CUDA device (shared memory); otherwise the
        // best available Vulkan device.
        let uuid = match (&cuda, cu) {
            (Some(c), Some((ctx, _))) => Some(c.with_context(ctx, || c.current_device_uuid())?),
            _ => None,
        };
        let vk = Vk::new(uuid, opts.display)?;
        let d = &vk.device;

        let bindings: Vec<_> = (0..5)
            .map(|i| {
                vk::DescriptorSetLayoutBinding::default()
                    .binding(i)
                    .descriptor_type(if i == 4 {
                        vk::DescriptorType::STORAGE_IMAGE
                    } else {
                        vk::DescriptorType::STORAGE_BUFFER
                    })
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE)
            })
            .collect();
        let set_layout = unsafe {
            d.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default()
                    .flags(vk::DescriptorSetLayoutCreateFlags::PUSH_DESCRIPTOR_KHR)
                    .bindings(&bindings),
                None,
            )
        }?;
        let pc = [vk::PushConstantRange::default()
            .stage_flags(vk::ShaderStageFlags::COMPUTE)
            .size(std::mem::size_of::<Params>() as u32)];
        let layouts = [set_layout];
        let pipe_layout = unsafe {
            d.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&layouts)
                    .push_constant_ranges(&pc),
                None,
            )
        }?;
        let pipe_main = vk.compute_pipeline(SPV_MAIN, pipe_layout)?;
        let pipe_aux = vk.compute_pipeline(SPV_AUX, pipe_layout)?;
        let pipe_convert = vk.compute_pipeline(SPV_CONVERT, pipe_layout)?;
        let pipe_fill = vk.compute_pipeline(SPV_FILL, pipe_layout)?;
        let cmds = unsafe {
            d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(vk.pool)
                    .command_buffer_count(SLOTS as u32),
            )
        }?;
        let usage = vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST;
        let mut slots = Vec::new();
        for cmd in cmds {
            slots.push(Slot {
                cmd,
                serial: 0,
                staging: Transient {
                    buf: vk.buffer(TRANSIENT_INITIAL, usage, false, true)?,
                    used: 0,
                    host: true,
                    usage,
                },
                scratch: Transient {
                    buf: vk.buffer(TRANSIENT_INITIAL, usage, false, false)?,
                    used: 0,
                    host: false,
                    usage,
                },
            });
        }
        let mut ty = vk::SemaphoreTypeCreateInfo::default()
            .semaphore_type(vk::SemaphoreType::TIMELINE)
            .initial_value(0);
        let timeline = unsafe {
            d.create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut ty), None)
        }?;
        Ok(Self {
            cuda,
            hw,
            cu,
            vk,
            set_layout,
            pipe_layout,
            pipe_main,
            pipe_aux,
            pipe_convert,
            pipe_fill,
            slots,
            cur: 0,
            recording: false,
            timeline,
            next_serial: Cell::new(1),
            ended_frames: Vec::new(),
            ack_tx: None,
            ack_thread: None,
            surfaces: BTreeMap::new(),
            map_order: Vec::new(),
            chunks: Vec::new(),
            cache: HashMap::new(),
            max_cache_slots: opts.max_cache_slots,
            reverse: opts.reverse_filter,
            capture: None,
            decode_pool: if opts.parallel_decode {
                Some(
                    rayon::ThreadPoolBuilder::new()
                        .num_threads(2)
                        .thread_name(|i| format!("rdpgfx-decode-{i}"))
                        .build()?,
                )
            } else {
                None
            },
            present: None,
            profile: Profile::default(),
        })
    }

    pub fn device_name(&self) -> &str {
        &self.vk.device_name
    }

    /// Decoder backend and devices. A hardware decoder that fell back to the CPU on some
    /// surface is reported as software.
    pub fn backend_info(&self) -> BackendInfo {
        let fell_back = self.surfaces.values().any(|s| {
            s.avc
                .as_ref()
                .is_some_and(|a| a.decoder.backend() != self.hw.backend())
        });
        let backend = if fell_back {
            Backend::Software
        } else {
            self.hw.backend()
        };
        let decoder = match backend {
            Backend::Cuda => self.vk.device_name.clone(),
            Backend::Vaapi => self.hw.description.clone(),
            Backend::Software => "CPU".into(),
        };
        BackendInfo {
            backend,
            decoder,
            vulkan: self.vk.device_name.clone(),
        }
    }

    /// Releases CUDA imports of view buffers.
    fn release_imports(&self, imported: Vec<ImportedBuffer>) {
        if let (Some(c), Some((ctx, _))) = (&self.cuda, self.cu) {
            let _ = c.with_context(ctx, || {
                for b in imported {
                    c.release(b);
                }
                Ok(())
            });
        }
    }

    pub fn set_reverse_filter(&mut self, on: bool) {
        self.reverse = on;
    }

    /// Test aid: keep host copies of the decoded views (costs a download per view).
    pub fn set_capture(&mut self, on: bool) {
        self.capture = on.then(HashMap::new);
    }

    /// Host copies of the views decoded for surface `id` by the last AVC update.
    pub fn take_captured(&mut self, id: u16) -> Captured {
        self.capture
            .as_mut()
            .and_then(|c| c.remove(&id))
            .unwrap_or_default()
    }

    // ---- batch handling ------------------------------------------------------------------

    fn cmd(&mut self) -> Result<vk::CommandBuffer> {
        if !self.recording {
            // The slot's previous batch must be done before its buffers are reused.
            self.wait_serial(self.slots[self.cur].serial)?;
            let slot = &mut self.slots[self.cur];
            slot.staging.used = 0;
            slot.scratch.used = 0;
            let cmd = slot.cmd;
            unsafe {
                let d = &self.vk.device;
                d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
                d.begin_command_buffer(
                    cmd,
                    &vk::CommandBufferBeginInfo::default()
                        .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
                )?;
            }
            // Earlier submits (presents reading surfaces) before anything in this batch.
            self.barrier(cmd);
            self.recording = true;
        }
        Ok(self.slots[self.cur].cmd)
    }

    /// Timeline value reached by the GPU.
    pub fn completed(&self) -> u64 {
        unsafe { self.vk.device.get_semaphore_counter_value(self.timeline) }.unwrap_or(0)
    }

    /// Value of the most recent submit (0 before the first).
    pub fn last_serial(&self) -> u64 {
        self.next_serial.get() - 1
    }

    fn take_serial(&self) -> u64 {
        let v = self.next_serial.get();
        self.next_serial.set(v + 1);
        v
    }

    /// Blocks until the GPU reached timeline value `v` (returns at once if it has).
    pub fn wait_serial(&self, v: u64) -> Result<()> {
        if v == 0 || self.completed() >= v {
            return Ok(());
        }
        let sp = self.profile.start(Stage::Wait);
        let sems = [self.timeline];
        let vals = [v];
        unsafe {
            self.vk.device.wait_semaphores(
                &vk::SemaphoreWaitInfo::default()
                    .semaphores(&sems)
                    .values(&vals),
                u64::MAX,
            )
        }?;
        self.profile.end(sp);
        Ok(())
    }

    /// Makes everything recorded so far visible to everything recorded afterwards.
    fn barrier(&self, cmd: vk::CommandBuffer) {
        let all = vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER;
        let mb = [vk::MemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(
                vk::AccessFlags::SHADER_READ
                    | vk::AccessFlags::SHADER_WRITE
                    | vk::AccessFlags::TRANSFER_READ
                    | vk::AccessFlags::TRANSFER_WRITE,
            )];
        unsafe {
            self.vk.device.cmd_pipeline_barrier(
                cmd,
                all,
                all,
                vk::DependencyFlags::empty(),
                &mb,
                &[],
                &[],
            )
        };
    }

    /// Submits the recorded batch without waiting; returns its timeline value (or the
    /// last one when nothing was recorded).
    pub fn submit(&mut self) -> Result<u64> {
        if !self.recording {
            return Ok(self.last_serial());
        }
        let serial = self.take_serial();
        let cmd = self.slots[self.cur].cmd;
        let cmds = [cmd];
        let sems = [self.timeline];
        let vals = [serial];
        let mut tl = vk::TimelineSemaphoreSubmitInfo::default().signal_semaphore_values(&vals);
        unsafe {
            let d = &self.vk.device;
            d.end_command_buffer(cmd)?;
            let sp = self.profile.start(Stage::Submit);
            d.queue_submit(
                self.vk.queue,
                &[vk::SubmitInfo::default()
                    .command_buffers(&cmds)
                    .signal_semaphores(&sems)
                    .push_next(&mut tl)],
                vk::Fence::null(),
            )?;
            self.profile.end(sp);
        }
        self.slots[self.cur].serial = serial;
        self.cur = (self.cur + 1) % SLOTS;
        self.recording = false;
        for s in self.surfaces.values_mut() {
            if let Some(a) = &mut s.avc
                && a.read_recorded
            {
                a.read_serial = serial;
                a.read_recorded = false;
            }
        }
        Ok(serial)
    }

    /// Submits the recorded commands and waits until the GPU finished everything.
    pub fn flush(&mut self) -> Result<()> {
        let s = self.submit()?;
        self.wait_serial(s)
    }

    /// End of an RDPGFX frame: submits the batch (no waiting).
    pub fn end_frame(&mut self) -> Result<()> {
        self.submit().map(|_| ())
    }

    /// Like `end_frame`, and frame `id` is acknowledged (via `on_presented`) once the
    /// present that shows it has completed on the GPU.
    pub fn end_frame_ack(&mut self, id: u64) -> Result<()> {
        self.end_frame()?;
        self.ended_frames.push(id);
        Ok(())
    }

    /// Starts the acknowledgement thread: `f(frame_id)` runs on that thread.
    pub fn on_presented(&mut self, f: impl Fn(u64) + Send + 'static) {
        let (tx, rx) = mpsc::channel::<AckJob>();
        let device = self.vk.device.clone();
        let sem = self.timeline;
        self.ack_tx = Some(tx);
        self.ack_thread = Some(std::thread::spawn(move || {
            while let Ok((serial, ids)) = rx.recv() {
                let sems = [sem];
                let vals = [serial];
                let info = vk::SemaphoreWaitInfo::default()
                    .semaphores(&sems)
                    .values(&vals);
                loop {
                    match unsafe { device.wait_semaphores(&info, 100_000_000) } {
                        Ok(()) => break,
                        Err(vk::Result::TIMEOUT) => continue,
                        Err(_) => return,
                    }
                }
                for id in ids {
                    f(id);
                }
            }
        }));
    }

    /// Hands the frames ended so far to the acknowledgement thread, due at `serial`.
    fn release_frames(&mut self, serial: u64) {
        if self.ended_frames.is_empty() {
            return;
        }
        let ids = std::mem::take(&mut self.ended_frames);
        if let Some(tx) = &self.ack_tx {
            let _ = tx.send((serial, ids));
        }
    }

    fn transient_mut(&mut self, staging: bool) -> &mut Transient {
        let slot = &mut self.slots[self.cur];
        if staging {
            &mut slot.staging
        } else {
            &mut slot.scratch
        }
    }

    /// Reserves `size` bytes in a transient buffer of the current slot, moving on to the
    /// next slot (or growing the buffer) when it is full. Starts recording.
    fn transient(&mut self, staging: bool, size: u64) -> Result<u64> {
        let size = size.div_ceil(16) * 16;
        self.cmd()?;
        let t = self.transient_mut(staging);
        if t.used + size > t.buf.size {
            self.submit()?;
            self.cmd()?; // next slot, idle after the wait in `cmd`
            let (host, usage, cur) = {
                let t = self.transient_mut(staging);
                (t.host, t.usage, t.buf.size)
            };
            if size > cur {
                let new = self.vk.buffer(size.max(cur * 2), usage, false, host)?;
                let old = std::mem::replace(&mut self.transient_mut(staging).buf, new);
                self.vk.destroy_buffer(old);
            }
        }
        let t = self.transient_mut(staging);
        let off = t.used;
        t.used += size;
        Ok(off)
    }

    // ---- surface model -------------------------------------------------------------------

    fn surface(&self, id: u16) -> Result<&Surface> {
        Ok(self
            .surfaces
            .get(&id)
            .ok_or(SurfaceError::UnknownSurface(id))?)
    }

    /// Applies one non-codec command (same semantics and errors as `SurfaceModel::apply`).
    pub fn apply(&mut self, op: &SurfaceOp) -> Result<()> {
        match op {
            SurfaceOp::Create {
                id,
                width,
                height,
                format,
            } => self.create_surface(*id, *width, *height, *format),
            SurfaceOp::Delete { id } => self.delete_surface(*id),
            SurfaceOp::MapToOutput { id, x, y } => {
                let s = self
                    .surfaces
                    .get_mut(id)
                    .ok_or(SurfaceError::UnknownSurface(*id))?;
                s.origin = Some((*x, *y));
                self.map_order.retain(|s| s != id);
                self.map_order.push(*id);
                Ok(())
            }
            SurfaceOp::SolidFill { id, color, rects } => self.solid_fill(*id, *color, rects),
            SurfaceOp::SurfaceToSurface {
                src,
                rect,
                dst,
                points,
            } => self.surface_to_surface(*src, *rect, *dst, points),
            SurfaceOp::SurfaceToCache { id, rect, slot } => {
                self.surface_to_cache(*id, *rect, *slot)
            }
            SurfaceOp::CacheToSurface { slot, dst, points } => {
                self.cache_to_surface(*slot, *dst, points)
            }
            SurfaceOp::EvictCache { slot } => self.evict(*slot),
            SurfaceOp::Upload { id, rect, bgra } => self.upload(*id, *rect, bgra),
        }
    }

    pub fn create_surface(
        &mut self,
        id: u16,
        width: u32,
        height: u32,
        format: PixelFormat,
    ) -> Result<()> {
        if self.surfaces.contains_key(&id) {
            bail!(SurfaceError::SurfaceExists(id));
        }
        if width == 0 || height == 0 {
            bail!(SurfaceError::EmptySurface(id));
        }
        let image = self.vk.image(width, height)?;
        let cmd = self.cmd()?;
        let to_general = [vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::UNDEFINED)
            .new_layout(vk::ImageLayout::GENERAL)
            .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(image.image)
            .subresource_range(COLOR_RANGE)];
        let p = INITIAL_PIXEL;
        // Image channels hold BGRA bytes: R slot = B, G = G, B slot = R, A = A.
        let clear = vk::ClearColorValue {
            float32: [p[0], p[1], p[2], p[3]].map(|v| f32::from(v) / 255.0),
        };
        unsafe {
            let d = &self.vk.device;
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &to_general,
            );
            d.cmd_clear_color_image(
                cmd,
                image.image,
                vk::ImageLayout::GENERAL,
                &clear,
                &[COLOR_RANGE],
            );
        }
        self.barrier(cmd);
        self.surfaces.insert(
            id,
            Surface {
                width,
                height,
                format,
                image,
                origin: None,
                avc: None,
            },
        );
        Ok(())
    }

    pub fn delete_surface(&mut self, id: u16) -> Result<()> {
        if !self.surfaces.contains_key(&id) {
            bail!(SurfaceError::UnknownSurface(id));
        }
        // Recorded commands may still reference the image.
        self.flush()?;
        let s = self.surfaces.remove(&id).expect("checked above");
        self.map_order.retain(|m| *m != id);
        self.destroy_surface(s);
        Ok(())
    }

    fn destroy_surface(&mut self, s: Surface) {
        if let Some(a) = s.avc {
            self.destroy_avc(a);
        }
        self.vk.destroy_image(s.image);
    }

    fn destroy_avc(&self, a: AvcState) {
        let AvcState {
            decoder,
            views,
            imported,
            state,
            ..
        } = a;
        drop(decoder);
        self.release_imports(imported);
        for b in views.into_iter().chain(state) {
            self.vk.destroy_buffer(b);
        }
    }

    /// Graphics channel closed (reconnect): every surface and cache entry is dropped.
    pub fn clear_all(&mut self) -> Result<()> {
        self.flush()?;
        let surfaces = std::mem::take(&mut self.surfaces);
        self.map_order.clear();
        for (_, s) in surfaces {
            self.destroy_surface(s);
        }
        let slots: Vec<u16> = self.cache.keys().copied().collect();
        for slot in slots {
            self.evict(slot)?;
        }
        Ok(())
    }

    /// RDPGFX_RESET_GRAPHICS_PDU: drops all codec states; surfaces and cache stay.
    pub fn reset_graphics(&mut self) -> Result<()> {
        self.flush()?;
        let states: Vec<AvcState> = self
            .surfaces
            .values_mut()
            .filter_map(|s| s.avc.take())
            .collect();
        for a in states {
            self.destroy_avc(a);
        }
        Ok(())
    }

    /// Mapped surfaces in drawing order (input for `rdpgfx_model::outputs::compose`).
    pub fn mapped_surfaces(&self) -> Vec<MappedSurface> {
        self.map_order
            .iter()
            .filter_map(|id| {
                let s = self.surfaces.get(id)?;
                Some(MappedSurface {
                    id: *id,
                    origin: s.origin?,
                    width: s.width,
                    height: s.height,
                })
            })
            .collect()
    }

    pub fn surface_ids(&self) -> Vec<u16> {
        self.surfaces.keys().copied().collect()
    }

    pub fn surface_size(&self, id: u16) -> Result<(u32, u32)> {
        let s = self.surface(id)?;
        Ok((s.width, s.height))
    }

    pub fn surface_format(&self, id: u16) -> Result<PixelFormat> {
        Ok(self.surface(id)?.format)
    }

    fn push_image(
        &self,
        cmd: vk::CommandBuffer,
        view: vk::ImageView,
        avc: Option<(&AvcState, usize)>,
    ) {
        let img = [vk::DescriptorImageInfo::default()
            .image_view(view)
            .image_layout(vk::ImageLayout::GENERAL)];
        let bufs: Vec<[vk::DescriptorBufferInfo; 1]> = avc
            .map(|(a, v)| {
                [
                    a.views[v].buffer,
                    a.state[0].buffer,
                    a.state[1].buffer,
                    a.state[2].buffer,
                ]
                .iter()
                .map(|&b| {
                    [vk::DescriptorBufferInfo::default()
                        .buffer(b)
                        .range(vk::WHOLE_SIZE)]
                })
                .collect()
            })
            .unwrap_or_default();
        let mut writes: Vec<vk::WriteDescriptorSet> = bufs
            .iter()
            .enumerate()
            .map(|(i, info)| {
                vk::WriteDescriptorSet::default()
                    .dst_binding(i as u32)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(info)
            })
            .collect();
        writes.push(
            vk::WriteDescriptorSet::default()
                .dst_binding(4)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&img),
        );
        unsafe {
            self.vk.push_desc.cmd_push_descriptor_set(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                self.pipe_layout,
                0,
                &writes,
            )
        };
    }

    fn dispatch(&self, cmd: vk::CommandBuffer, p: &Params, area: &Rect) {
        if area.is_empty() {
            return;
        }
        unsafe {
            let d = &self.vk.device;
            d.cmd_push_constants(
                cmd,
                self.pipe_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                p.bytes(),
            );
            d.cmd_dispatch(
                cmd,
                area.width().div_ceil(16),
                area.height().div_ceil(16),
                1,
            );
        }
    }

    pub fn solid_fill(&mut self, id: u16, color: Color, rects: &[Rect]) -> Result<()> {
        let s = self.surface(id)?;
        let (bounds, view) = (Rect::new(0, 0, s.width, s.height), s.image.storage_view);
        let clipped: Vec<Rect> = rects.iter().filter_map(|r| r.intersect(&bounds)).collect();
        if clipped.is_empty() {
            return Ok(());
        }
        let cmd = self.cmd()?;
        unsafe {
            self.vk
                .device
                .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipe_fill)
        };
        self.push_image(cmd, view, None);
        for r in &clipped {
            let p = Params {
                luma: rect4(r),
                color: color.packed(),
                ..Default::default()
            };
            self.dispatch(cmd, &p, r);
        }
        self.barrier(cmd);
        Ok(())
    }

    fn check_within(&self, id: u16, r: &Rect) -> Result<()> {
        let s = self.surface(id)?;
        if !within(r, s.width, s.height) {
            bail!(SurfaceError::OutOfBounds {
                surface: id,
                rect: *r
            });
        }
        Ok(())
    }

    fn image_to_buffer(
        &self,
        cmd: vk::CommandBuffer,
        image: vk::Image,
        r: &Rect,
        buf: vk::Buffer,
        off: u64,
    ) {
        let region = [vk::BufferImageCopy::default()
            .buffer_offset(off)
            .image_subresource(COLOR_LAYERS)
            .image_offset(vk::Offset3D {
                x: r.left as i32,
                y: r.top as i32,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: r.width(),
                height: r.height(),
                depth: 1,
            })];
        unsafe {
            self.vk.device.cmd_copy_image_to_buffer(
                cmd,
                image,
                vk::ImageLayout::GENERAL,
                buf,
                &region,
            )
        };
    }

    fn buffer_to_image(
        &self,
        cmd: vk::CommandBuffer,
        buf: vk::Buffer,
        off: u64,
        image: vk::Image,
        r: &Rect,
    ) {
        let region = [vk::BufferImageCopy::default()
            .buffer_offset(off)
            .image_subresource(COLOR_LAYERS)
            .image_offset(vk::Offset3D {
                x: r.left as i32,
                y: r.top as i32,
                z: 0,
            })
            .image_extent(vk::Extent3D {
                width: r.width(),
                height: r.height(),
                depth: 1,
            })];
        unsafe {
            self.vk.device.cmd_copy_buffer_to_image(
                cmd,
                buf,
                image,
                vk::ImageLayout::GENERAL,
                &region,
            )
        };
    }

    pub fn surface_to_surface(
        &mut self,
        src: u16,
        rect: Rect,
        dst: u16,
        points: &[(u32, u32)],
    ) -> Result<()> {
        self.check_within(src, &rect)?;
        self.surface(dst)?;
        let (w, h) = (rect.width(), rect.height());
        for p in points {
            self.check_within(dst, &dest_rect(*p, w, h))?;
        }
        if rect.is_empty() || points.is_empty() {
            return Ok(());
        }
        // Snapshot of the source in scratch memory, then one copy per destination.
        let off = self.transient(false, u64::from(w * h * 4))?;
        let cmd = self.cmd()?;
        let (src_img, dst_img) = (
            self.surface(src)?.image.image,
            self.surface(dst)?.image.image,
        );
        let scratch = self.slots[self.cur].scratch.buf.buffer;
        self.image_to_buffer(cmd, src_img, &rect, scratch, off);
        self.barrier(cmd);
        for p in points {
            self.buffer_to_image(cmd, scratch, off, dst_img, &dest_rect(*p, w, h));
        }
        self.barrier(cmd);
        Ok(())
    }

    pub fn surface_to_cache(&mut self, id: u16, rect: Rect, slot: u16) -> Result<()> {
        check_slot(slot, self.max_cache_slots)?;
        self.check_within(id, &rect)?;
        if let Some(old) = self.cache.remove(&slot) {
            self.chunks[old.chunk].1.release(old.offset, old.size);
        }
        let size = u64::from(rect.width() * rect.height() * 4);
        let (chunk, offset) = self.cache_alloc(size)?;
        self.cache.insert(
            slot,
            CacheSlot {
                chunk,
                offset,
                size,
                width: rect.width(),
                height: rect.height(),
            },
        );
        if !rect.is_empty() {
            let cmd = self.cmd()?;
            let img = self.surface(id)?.image.image;
            self.image_to_buffer(cmd, img, &rect, self.chunks[chunk].0.buffer, offset);
            self.barrier(cmd);
        }
        Ok(())
    }

    fn cache_alloc(&mut self, size: u64) -> Result<(usize, u64)> {
        for (i, (_, arena)) in self.chunks.iter_mut().enumerate() {
            if let Some(off) = arena.alloc(size) {
                return Ok((i, off));
            }
        }
        let cap = Arena::reserved(size).max(CACHE_CHUNK);
        let usage = vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST;
        let buf = self.vk.buffer(cap, usage, false, false)?;
        let mut arena = Arena::new(cap);
        let off = arena.alloc(size).context("fresh cache chunk too small")?;
        self.chunks.push((buf, arena));
        Ok((self.chunks.len() - 1, off))
    }

    pub fn cache_to_surface(&mut self, slot: u16, dst: u16, points: &[(u32, u32)]) -> Result<()> {
        check_slot(slot, self.max_cache_slots)?;
        let e = self
            .cache
            .get(&slot)
            .ok_or(SurfaceError::UnknownCacheSlot(slot))?;
        let (chunk, offset, w, h) = (e.chunk, e.offset, e.width, e.height);
        self.surface(dst)?;
        for p in points {
            self.check_within(dst, &dest_rect(*p, w, h))?;
        }
        if w == 0 || h == 0 || points.is_empty() {
            return Ok(());
        }
        let cmd = self.cmd()?;
        let img = self.surface(dst)?.image.image;
        for p in points {
            self.buffer_to_image(
                cmd,
                self.chunks[chunk].0.buffer,
                offset,
                img,
                &dest_rect(*p, w, h),
            );
        }
        self.barrier(cmd);
        Ok(())
    }

    pub fn evict(&mut self, slot: u16) -> Result<()> {
        check_slot(slot, self.max_cache_slots)?;
        let e = self
            .cache
            .remove(&slot)
            .ok_or(SurfaceError::UnknownCacheSlot(slot))?;
        self.chunks[e.chunk].1.release(e.offset, e.size);
        Ok(())
    }

    /// Cache usage: (entries, bytes in use, bytes allocated on the GPU).
    pub fn cache_usage(&self) -> (usize, u64, u64) {
        let cap: u64 = self.chunks.iter().map(|c| c.1.capacity()).sum();
        let free: u64 = self.chunks.iter().map(|c| c.1.free_bytes()).sum();
        (self.cache.len(), cap - free, cap)
    }

    /// CPU-decoded pixels (tight BGRA rows) into `rect`.
    pub fn upload(&mut self, id: u16, rect: Rect, bgra: &[u8]) -> Result<()> {
        self.check_within(id, &rect)?;
        let expected = (rect.width() * rect.height() * 4) as usize;
        if bgra.len() != expected {
            bail!(SurfaceError::PixelDataSize {
                expected,
                got: bgra.len()
            });
        }
        if rect.is_empty() {
            return Ok(());
        }
        let off = self.transient(true, expected as u64)?;
        let staging = &self.slots[self.cur].staging.buf;
        let (ptr, staging_buf) = (
            staging.mapped.context("staging not mapped")?,
            staging.buffer,
        );
        // SAFETY: `transient` reserved `expected` bytes at `off` inside the mapped buffer, and
        // no submitted work reads this range (it was reset by the last flush).
        unsafe { std::ptr::copy_nonoverlapping(bgra.as_ptr(), ptr.add(off as usize), expected) };
        let cmd = self.cmd()?;
        let img = self.surface(id)?.image.image;
        self.buffer_to_image(cmd, staging_buf, off, img, &rect);
        self.barrier(cmd);
        Ok(())
    }

    /// Reads a surface back as tight BGRA (test path; synchronous).
    pub fn read_surface(&mut self, id: u16) -> Result<Vec<u8>> {
        let (w, h, img) = {
            let s = self.surface(id)?;
            (s.width, s.height, s.image.image)
        };
        let size = u64::from(w * h * 4);
        let buf = self
            .vk
            .buffer(size, vk::BufferUsageFlags::TRANSFER_DST, false, true)?;
        let cmd = self.cmd()?;
        self.image_to_buffer(cmd, img, &Rect::new(0, 0, w, h), buf.buffer, 0);
        let res = self.flush().and_then(|()| {
            let ptr = buf.mapped.context("readback not mapped")?;
            Ok(unsafe { std::slice::from_raw_parts(ptr, size as usize) }.to_vec())
        });
        self.vk.destroy_buffer(buf);
        res
    }

    // ---- AVC -----------------------------------------------------------------------------

    /// View and state buffers of a codec state with `stride` x `rows` pictures; the state
    /// starts black (Y=0, U=V=128, like the CPU reference).
    #[allow(clippy::type_complexity)]
    fn alloc_avc_buffers(
        &mut self,
        stride: u32,
        rows: u32,
    ) -> Result<([Buffer; 2], Vec<ImportedBuffer>, [Buffer; 3])> {
        let plane = u64::from(stride * rows);
        let storage = vk::BufferUsageFlags::STORAGE_BUFFER;
        // CUDA writes exported device-local buffers; otherwise the CPU writes mapped ones.
        let cuda = self.cuda.is_some();
        let views = [
            self.vk.buffer(plane * 3 / 2, storage, cuda, !cuda)?,
            self.vk.buffer(plane * 3 / 2, storage, cuda, !cuda)?,
        ];
        let mut imported = Vec::new();
        if let (Some(c), Some((ctx, _))) = (&self.cuda, self.cu) {
            c.with_context(ctx, || {
                for v in &views {
                    let fd = self.vk.export_fd(v)?;
                    imported.push(c.import_buffer(fd, v.size)?);
                }
                Ok(())
            })?;
        }
        let usage = storage | vk::BufferUsageFlags::TRANSFER_DST;
        let state = [
            self.vk.buffer(plane, usage, false, false)?,
            self.vk.buffer(plane, usage, false, false)?,
            self.vk.buffer(plane, usage, false, false)?,
        ];
        let cmd = self.cmd()?;
        unsafe {
            let d = &self.vk.device;
            d.cmd_fill_buffer(cmd, state[0].buffer, 0, vk::WHOLE_SIZE, 0);
            d.cmd_fill_buffer(cmd, state[1].buffer, 0, vk::WHOLE_SIZE, 0x8080_8080);
            d.cmd_fill_buffer(cmd, state[2].buffer, 0, vk::WHOLE_SIZE, 0x8080_8080);
        }
        self.barrier(cmd);
        Ok((views, imported, state))
    }

    fn new_avc_state(&mut self, width: u32, height: u32) -> Result<AvcState> {
        let (stride, rows) = (pad(width), pad(height));
        let (views, imported, state) = self.alloc_avc_buffers(stride, rows)?;
        Ok(AvcState {
            decoder: Decoder::new(&self.hw)?,
            stride,
            rows,
            views,
            imported,
            state,
            read_recorded: false,
            read_serial: 0,
        })
    }

    /// The server may encode with a coded size other than the macroblock-padded surface
    /// (Windows aligns the width to 64: 3440 -> 3456). Views and state follow the coded
    /// picture, since the AVC444v2 auxiliary layout splits at half / quarter of that width.
    fn fit_avc_to_stream(&mut self, id: u16, au: &[u8]) -> Result<()> {
        let (w, h) = {
            let s = self.surface(id)?;
            (s.width, s.height)
        };
        let a = self
            .surfaces
            .get_mut(&id)
            .and_then(|s| s.avc.as_mut())
            .context("no codec state")?;
        let Some((cw, ch)) = a.decoder.coded_size(au) else {
            return Ok(());
        };
        if (cw, ch) == (a.stride, a.rows) {
            return Ok(());
        }
        ensure!(
            cw >= w && ch >= h && cw % 16 == 0 && ch % 16 == 0,
            "coded picture {cw}x{ch} does not fit surface {w}x{h}"
        );
        // The old buffers may still be in use by submitted work.
        self.flush()?;
        let (views, imported, state) = self.alloc_avc_buffers(cw, ch)?;
        let a = self
            .surfaces
            .get_mut(&id)
            .and_then(|s| s.avc.as_mut())
            .expect("checked above");
        let old_views = std::mem::replace(&mut a.views, views);
        let old_imported = std::mem::replace(&mut a.imported, imported);
        let old_state = std::mem::replace(&mut a.state, state);
        a.stride = cw;
        a.rows = ch;
        a.read_recorded = false;
        a.read_serial = 0;
        self.release_imports(old_imported);
        for b in old_views.into_iter().chain(old_state) {
            self.vk.destroy_buffer(b);
        }
        Ok(())
    }

    /// AVC420 (`aux = None`, layout ignored) or AVC444 update. A view left out by the LC
    /// field is `None`. Semantics: `SurfaceModel::avc`.
    pub fn avc(
        &mut self,
        id: u16,
        layout: Avc444Layout,
        main: Option<AvcView<'_>>,
        aux: Option<AvcView<'_>>,
    ) -> Result<()> {
        self.avc_frame(&[AvcUpdate {
            id,
            layout,
            main,
            aux,
        }])
    }

    /// Decode several surfaces' updates of one frame, in parallel (one thread per surface,
    /// shared CUDA context) when enabled; then record their shader passes in order.
    /// Updates for the same surface are handled one after the other.
    pub fn avc_frame(&mut self, updates: &[AvcUpdate<'_>]) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        if !updates.iter().all(|u| seen.insert(u.id)) {
            for u in updates {
                self.avc_frame(std::slice::from_ref(u))?;
            }
            return Ok(());
        }
        // Prepare: codec state exists, earlier GPU reads of the view buffers are done.
        for u in updates {
            let (w, h) = {
                let s = self.surface(u.id)?;
                (s.width, s.height)
            };
            if self.surfaces[&u.id].avc.is_none() {
                let st = self.new_avc_state(w, h)?;
                self.surfaces.get_mut(&u.id).expect("exists").avc = Some(st);
            }
        }
        // Coded size from the stream (SPS); resizes views and state when it differs.
        for u in updates {
            if let Some(v) = u.main.as_ref().or(u.aux.as_ref()) {
                self.fit_avc_to_stream(u.id, v.au)?;
            }
        }
        if updates.iter().any(|u| {
            self.surfaces[&u.id]
                .avc
                .as_ref()
                .is_some_and(|a| a.read_recorded)
        }) {
            self.submit()?;
        }
        let newest = updates
            .iter()
            .filter_map(|u| self.surfaces[&u.id].avc.as_ref().map(|a| a.read_serial))
            .max()
            .unwrap_or(0);
        self.wait_serial(newest)?;

        // Decode and copy the views (one decoder: main and aux alternate in one stream).
        let capture = self.capture.is_some();
        let cu_ref = match (&self.cuda, self.cu) {
            (Some(cuda), Some((ctx, stream))) => Some(CudaRef { cuda, ctx, stream }),
            _ => None,
        };
        let cu = cu_ref.as_ref();
        let sp = self.profile.start(Stage::Decode);
        let mut states: Vec<(&mut AvcState, &AvcUpdate<'_>)> = Vec::new();
        // `surfaces` iterates in id order; results come back in the same order.
        for (id, s) in self.surfaces.iter_mut() {
            if let Some(u) = updates.iter().find(|u| u.id == *id) {
                states.push((s.avc.as_mut().expect("created above"), u));
            }
        }
        let results: Vec<Result<Captured>> = if let Some(pool) =
            self.decode_pool.as_ref().filter(|_| states.len() > 1)
        {
            let mut out: Vec<Option<Result<Captured>>> = (0..states.len()).map(|_| None).collect();
            let mut jobs: Vec<SendJob<'_, '_>> =
                states.into_iter().map(|(a, u)| SendJob(a, u, cu)).collect();
            // The calling thread decodes the first surface itself, the pool the others.
            let first = jobs.remove(0);
            let (head, tail) = out.split_at_mut(1);
            pool.in_place_scope(|scope| {
                for (job, slot) in jobs.into_iter().zip(tail.iter_mut()) {
                    let slot = SendSlot(slot);
                    scope.spawn(move |_| {
                        let slot = slot;
                        *slot.0 = Some(job.run(capture));
                    });
                }
                head[0] = Some(first.run(capture));
            });
            out.into_iter()
                .map(|r| r.unwrap_or_else(|| Err(anyhow::anyhow!("decode job did not run"))))
                .collect()
        } else {
            states
                .into_iter()
                .map(|(a, u)| decode_upload(cu, a, u, capture))
                .collect()
        };
        self.profile.end(sp);
        let mut ids: Vec<u16> = updates.iter().map(|u| u.id).collect();
        ids.sort_unstable();
        for (id, r) in ids.into_iter().zip(results) {
            let c = r?;
            if let Some(map) = &mut self.capture {
                map.insert(id, c);
            }
        }
        for u in updates {
            self.record_avc(
                u.id,
                u.layout,
                u.main.as_ref().map(|v| v.rects),
                u.aux.as_ref().map(|v| v.rects),
            )?;
        }
        Ok(())
    }

    /// Records the main/aux combination and the conversion passes of one AVC update.
    fn record_avc(
        &mut self,
        id: u16,
        layout: Avc444Layout,
        main: Option<&[Rect]>,
        aux: Option<&[Rect]>,
    ) -> Result<()> {
        let (w, h) = {
            let s = self.surface(id)?;
            (s.width, s.height)
        };
        let rec = self.profile.start(Stage::Record);
        let cmd = self.cmd()?;
        let s = &self.surfaces[&id];
        let a = s.avc.as_ref().expect("created above");
        let full = Rect::new(0, 0, a.stride, a.rows);
        let base = Params {
            stride: a.stride,
            rows: a.rows,
            width: w,
            height: h,
            layout_v2: u32::from(layout == Avc444Layout::V2),
            // Neutral where only a main view was applied (all four samples equal: 4a-3a = a),
            // so it can stay on for AVC420 and luma-only updates.
            reverse: u32::from(self.reverse),
            ..Default::default()
        };
        // The view buffers were written by CUDA (external queue family): acquire them.
        let acquire: Vec<_> = a
            .views
            .iter()
            .map(|v| {
                vk::BufferMemoryBarrier::default()
                    .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                    .dst_queue_family_index(self.vk.queue_family)
                    .dst_access_mask(vk::AccessFlags::SHADER_READ)
                    .buffer(v.buffer)
                    .size(vk::WHOLE_SIZE)
            })
            .collect();
        unsafe {
            self.vk.device.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &acquire,
                &[],
            )
        };
        let passes = [(self.pipe_main, 0usize, main), (self.pipe_aux, 1, aux)];
        for (pipe, k, rects) in passes {
            let Some(rects) = rects else { continue };
            unsafe {
                self.vk
                    .device
                    .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipe)
            };
            self.push_image(cmd, s.image.storage_view, Some((a, k)));
            for r in rects {
                let Some(area) = r.align_to_chroma_blocks().intersect(&full) else {
                    continue;
                };
                let luma = r.intersect(&full).unwrap_or(Rect::new(0, 0, 0, 0));
                let p = Params {
                    area: rect4(&area),
                    luma: rect4(&luma),
                    ..base
                };
                self.dispatch(cmd, &p, &area);
            }
            self.barrier(cmd);
        }
        // Convert the region rectangles of both views to surface pixels.
        unsafe {
            self.vk
                .device
                .cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, self.pipe_convert)
        };
        self.push_image(cmd, s.image.storage_view, Some((a, 0)));
        let visible = Rect::new(0, 0, w, h);
        for r in [main, aux].into_iter().flatten().flatten() {
            let Some(luma) = r.intersect(&visible) else {
                continue;
            };
            let p = Params {
                luma: rect4(&luma),
                ..base
            };
            self.dispatch(cmd, &p, &luma);
        }
        self.barrier(cmd);
        self.surfaces
            .get_mut(&id)
            .expect("exists")
            .avc
            .as_mut()
            .expect("created above")
            .read_recorded = true;
        self.profile.end(rec);
        Ok(())
    }
}

/// CUDA handles shared with the decode threads.
struct CudaRef<'a> {
    cuda: &'a Cuda,
    ctx: CuContext,
    stream: CuStream,
}

// SAFETY: the CUDA driver API is thread-safe; every call pushes `ctx` on the calling thread
// (`Cuda::with_context`), and the stream may be used from several threads.
unsafe impl Sync for CudaRef<'_> {}

/// One decode job handed to a scoped thread.
struct SendJob<'a, 'b>(&'a mut AvcState, &'a AvcUpdate<'b>, Option<&'a CudaRef<'a>>);

// SAFETY: each job owns exclusive access to one surface's codec state for the duration of
// the scoped thread (FFmpeg codec contexts and the Vulkan buffer handles are not bound to a
// thread, they must just not be used concurrently, which `&mut` guarantees). The access
// units are plain byte slices.
unsafe impl Send for SendJob<'_, '_> {}

/// Result slot written by exactly one pool job.
struct SendSlot<'a>(&'a mut Option<Result<Captured>>);

// SAFETY: `Captured` holds plain byte vectors; anyhow errors are Send.
unsafe impl Send for SendSlot<'_> {}

impl SendJob<'_, '_> {
    // Taking `self` makes the closure capture the whole (Send) job, not its fields.
    fn run(self, capture: bool) -> Result<Captured> {
        decode_upload(self.2, self.0, self.1, capture)
    }
}

/// Decodes the views of one update and copies them into the surface's view buffers.
fn decode_upload(
    cu: Option<&CudaRef<'_>>,
    a: &mut AvcState,
    u: &AvcUpdate<'_>,
    capture: bool,
) -> Result<Captured> {
    let mut captured = Captured::default();
    for (k, v) in [&u.main, &u.aux].into_iter().enumerate() {
        let Some(v) = v else { continue };
        let pic = a.decoder.decode(v.au)?;
        if capture {
            let i420 = a.decoder.download_i420()?;
            if k == 0 {
                captured.main = Some(i420);
            } else {
                captured.aux = Some(i420);
            }
        }
        upload_view(cu, a, k, &pic)?;
    }
    Ok(captured)
}

/// Copies a decoded picture into a view buffer: GPU to GPU for NVDEC pictures, from host
/// memory (VA-API, software) into the mapped buffer or, with CUDA buffers, via CUDA.
fn upload_view(cu: Option<&CudaRef<'_>>, a: &AvcState, view: usize, pic: &Picture) -> Result<()> {
    let (w, h) = match pic {
        Picture::Device(p) => (p.width, p.height),
        Picture::Host(p) => (p.width, p.height),
    };
    ensure!(
        w >= a.stride && h >= a.rows,
        "decoded picture {w}x{h} smaller than the padded surface {}x{}",
        a.stride,
        a.rows
    );
    let (s, r) = (a.stride as usize, a.rows as usize);
    match (pic, cu, a.imported.get(view)) {
        (Picture::Device(p), Some(cu), Some(dst)) => upload_device(cu, dst.ptr, s, r, p),
        (Picture::Host(p), Some(cu), Some(dst)) => {
            // CUDA buffers, but the decoder fell back to the CPU
            let mut nv12 = vec![0u8; s * r * 3 / 2];
            // SAFETY: nv12 holds stride*rows*3/2 bytes; the picture is the current one.
            unsafe { p.write_nv12(nv12.as_mut_ptr(), s, r) };
            upload_host_to_device(cu, dst.ptr, s, r, &nv12)
        }
        (Picture::Host(p), _, None) => {
            let dst = a.views[view].mapped.context("view buffer not mapped")?;
            // SAFETY: the mapped buffer holds stride*rows*3/2 bytes and no GPU work reads it
            // (avc_frame waited for the last batch that did).
            unsafe { p.write_nv12(dst, s, r) };
            Ok(())
        }
        _ => bail!("decoded picture and view buffers do not match the decoder backend"),
    }
}

fn upload_device(
    cu: &CudaRef<'_>,
    dst: u64,
    s: usize,
    r: usize,
    pic: &DevicePicture,
) -> Result<()> {
    cu.cuda.with_context(cu.ctx, || {
        let y = Memcpy2D {
            src_memory_type: CU_MEMORYTYPE_DEVICE,
            src_device: pic.y,
            src_pitch: pic.y_pitch,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_device: dst,
            dst_pitch: s,
            width_in_bytes: s,
            height: r,
            ..Default::default()
        };
        cu.cuda.copy_2d(&y, cu.stream)?;
        let uv = Memcpy2D {
            src_memory_type: CU_MEMORYTYPE_DEVICE,
            src_device: pic.uv,
            src_pitch: pic.uv_pitch,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_device: dst + (s * r) as u64,
            dst_pitch: s,
            width_in_bytes: s,
            height: r / 2,
            ..Default::default()
        };
        cu.cuda.copy_2d(&uv, cu.stream)?;
        // Vulkan reads only after the copy finished (external semaphores).
        cu.cuda.sync(cu.stream)
    })
}

fn upload_host_to_device(
    cu: &CudaRef<'_>,
    dst: u64,
    s: usize,
    r: usize,
    nv12: &[u8],
) -> Result<()> {
    cu.cuda.with_context(cu.ctx, || {
        let all = Memcpy2D {
            src_memory_type: CU_MEMORYTYPE_HOST,
            src_host: nv12.as_ptr() as usize,
            src_pitch: s,
            dst_memory_type: CU_MEMORYTYPE_DEVICE,
            dst_device: dst,
            dst_pitch: s,
            width_in_bytes: s,
            height: r * 3 / 2,
            ..Default::default()
        };
        cu.cuda.copy_2d(&all, cu.stream)?;
        cu.cuda.sync(cu.stream)
    })
}

impl Drop for GpuCore {
    fn drop(&mut self) {
        let _ = self.flush();
        unsafe {
            let _ = self.vk.device.device_wait_idle();
        }
        self.destroy_present();
        let surfaces = std::mem::take(&mut self.surfaces);
        for (_, s) in surfaces {
            self.destroy_surface(s);
        }
        for (b, _) in std::mem::take(&mut self.chunks) {
            self.vk.destroy_buffer(b);
        }
        // Stop the acknowledgement thread (all submitted work has completed above).
        self.ack_tx = None;
        if let Some(t) = self.ack_thread.take() {
            let _ = t.join();
        }
        for slot in std::mem::take(&mut self.slots) {
            self.vk.destroy_buffer(slot.staging.buf);
            self.vk.destroy_buffer(slot.scratch.buf);
            unsafe {
                self.vk
                    .device
                    .free_command_buffers(self.vk.pool, &[slot.cmd])
            };
        }
        let d = &self.vk.device;
        unsafe {
            d.destroy_semaphore(self.timeline, None);
            for p in [
                self.pipe_main,
                self.pipe_aux,
                self.pipe_convert,
                self.pipe_fill,
            ] {
                d.destroy_pipeline(p, None);
            }
            d.destroy_pipeline_layout(self.pipe_layout, None);
            d.destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}
