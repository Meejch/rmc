//! Views and presentation: every window is a swapchain that shows a viewport of the
//! session desktop. Views come and go (fullscreen <-> windowed) while the surfaces stay on
//! the GPU. The present pass samples the surface images (compute), then the overlay layer
//! (connection bar placeholder) is drawn on top; nothing is ever written into a surface.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use ash::vk;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

use rdpgfx_model::Rect;
use rdpgfx_model::outputs::{ViewScale, ViewTransform, compose, uncovered_pixels};

use super::{GpuCore, SLOTS};
use crate::profile::Stage;
use crate::vk::{Buffer, COLOR_RANGE};

const SPV_PRESENT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/present.spv"));
const SPV_OVERLAY: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overlay.spv"));

/// Neutral colour of letterbox bars and uncovered corners (windowed mode).
const NEUTRAL: [f32; 4] = [0.08, 0.08, 0.08, 1.0];

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PresentParams {
    dst: [u32; 4],
    piece: [u32; 4],
    viewport: [i32; 2],
    origin: [i32; 2],
    offset: [f32; 2],
    scale: f32,
    pad0: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OverlayParams {
    bar: [u32; 4],
    pinned: u32,
    hover: i32,
    /// 0 = connection bar, 1 = scroll thumb, 2 = progress (fraction in `pinned`, 0..10000).
    kind: u32,
    grab: u32,
    /// Kind 2: 16 ASCII characters, 4 per u32 (little endian).
    text: [u32; 4],
    /// "Keep active" on (button 5 highlighted).
    keep: u32,
    /// Right button group, left to right: button ids packed 4 bits each (lowest first).
    buttons: u32,
    nbuttons: u32,
}

fn bytes<T: Copy>(v: &T) -> &[u8] {
    // SAFETY: plain repr(C) structs of 4-byte fields without padding.
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

pub type ViewId = u32;

/// Overlay state of one view: the bar rectangle (view pixels) when visible.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OverlayState {
    pub bar: Option<Rect>,
    pub pinned: bool,
    /// Button under the pointer (-1 none, 0 pin, 1..4 right buttons).
    pub hover: i32,
    /// Keyboard grab active (grab button highlighted).
    pub grab: bool,
    /// "Keep active" on (its button highlighted).
    pub keep_active: bool,
    /// Scroll thumbs (horizontal, vertical) of a windowed crop, in view pixels.
    pub thumbs: [Option<Rect>; 2],
    /// Transfer progress strip below the bar.
    pub progress: Option<OverlayProgress>,
    /// Right button group of the bar, left to right (ids as in `overlay.comp`).
    pub buttons: ButtonRow,
}

/// Button ids of the bar's right group, left to right (at most 8).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ButtonRow {
    pub packed: u32,
    pub count: u32,
}

impl ButtonRow {
    pub fn new(ids: &[u32]) -> Self {
        assert!(ids.len() <= 8 && ids.iter().all(|&b| b < 16));
        let packed = ids
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | (b << (4 * i)));
        Self {
            packed,
            count: ids.len() as u32,
        }
    }
}

/// Opaque RGBA picture drawn into a view after the bar (e.g. text rendered on the CPU).
/// Position and size in view pixels; pixels row by row, R in the lowest byte.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayImage {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: std::sync::Arc<[u32]>,
}

/// Progress strip: rectangle in view pixels, fill in 1/10000, short ASCII text (digits,
/// '%', ':', ' ', '.', '/') drawn centered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OverlayProgress {
    pub rect: Rect,
    pub permyriad: u32,
    pub text: [u8; 16],
}

/// Result of one `present` call (for logs and pacing).
#[derive(Clone, Copy, Debug, Default)]
pub struct PresentStats {
    pub presented: u32,
    pub skipped_minimized: u32,
    pub recreated: u32,
}

struct View {
    surface: vk::SurfaceKHR,
    swapchain: vk::SwapchainKHR,
    images: Vec<vk::Image>,
    image_views: Vec<vk::ImageView>,
    /// Per swapchain image: signalled by our submit, waited on by the present engine.
    render_done: Vec<vk::Semaphore>,
    /// One acquire semaphore per present slot.
    acquire: Vec<vk::Semaphore>,
    extent: vk::Extent2D,
    present_mode: vk::PresentModeKHR,
    window_size: (u32, u32),
    viewport: Rect,
    scale: ViewScale,
    overlay: OverlayState,
    overlay_images: Vec<OverlayImage>,
    minimized: bool,
    stale: bool,
    /// Diagnostics: presents, not-acquired images and non-success present results.
    diag: std::cell::Cell<(u32, u32, u32)>,
}

/// Present diagnostics of a view since the last [`GpuCore::take_view_diag`].
#[derive(Clone, Copy, Debug)]
pub struct ViewDiag {
    pub presents: u32,
    /// Images not acquired because the swapchain was out of date.
    pub not_acquired: u32,
    /// Present results other than success (suboptimal, out of date).
    pub present_errors: u32,
    pub viewport: Rect,
    pub minimized: bool,
    pub stale: bool,
}

pub(super) struct PresentRes {
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipe_present: vk::Pipeline,
    pipe_overlay: vk::Pipeline,
    cmds: Vec<vk::CommandBuffer>,
    /// Timeline value of the last present submitted from each slot.
    serials: Vec<u64>,
    /// Per slot: host buffer with the overlay images of the present recorded there.
    image_bufs: Vec<Option<Buffer>>,
    /// Pixel offset of every view's images in the current slot's buffer.
    image_offsets: BTreeMap<ViewId, Vec<u32>>,
    cur: usize,
    views: BTreeMap<ViewId, View>,
    next_id: ViewId,
}

impl GpuCore {
    fn present_res(&mut self) -> Result<&mut PresentRes> {
        if self.present.is_none() {
            let d = &self.vk.device;
            let bindings = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
            ];
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
                .size(
                    std::mem::size_of::<PresentParams>().max(std::mem::size_of::<OverlayParams>())
                        as u32,
                )];
            let sl = [set_layout];
            let layout = unsafe {
                d.create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&sl)
                        .push_constant_ranges(&pc),
                    None,
                )
            }?;
            let pipe_present = self.vk.compute_pipeline(SPV_PRESENT, layout)?;
            let pipe_overlay = self.vk.compute_pipeline(SPV_OVERLAY, layout)?;
            let cmds = unsafe {
                d.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(self.vk.pool)
                        .command_buffer_count(SLOTS as u32),
                )
            }?;
            self.present = Some(PresentRes {
                set_layout,
                layout,
                pipe_present,
                pipe_overlay,
                cmds,
                serials: vec![0; SLOTS],
                image_bufs: (0..SLOTS).map(|_| None).collect(),
                image_offsets: BTreeMap::new(),
                cur: 0,
                views: BTreeMap::new(),
                next_id: 1,
            });
        }
        Ok(self.present.as_mut().expect("created above"))
    }

    /// Creates a view for a window of `size` pixels showing `viewport` (session coordinates).
    pub fn create_view(
        &mut self,
        display: RawDisplayHandle,
        window: RawWindowHandle,
        size: (u32, u32),
        viewport: Rect,
    ) -> Result<ViewId> {
        self.present_res()?;
        let wsi = self
            .vk
            .wsi
            .as_ref()
            .context("core created without a display")?;
        let surface = unsafe {
            ash_window::create_surface(&self.vk.entry, &self.vk.instance, display, window, None)
        }
        .context("create Vulkan surface")?;
        let ok = unsafe {
            wsi.surface.get_physical_device_surface_support(
                self.vk.physical,
                self.vk.queue_family,
                surface,
            )
        }?;
        if !ok {
            unsafe { wsi.surface.destroy_surface(surface, None) };
            bail!("queue family cannot present to this window");
        }
        let mut acquire = Vec::new();
        for _ in 0..SLOTS {
            acquire.push(unsafe {
                self.vk
                    .device
                    .create_semaphore(&vk::SemaphoreCreateInfo::default(), None)
            }?);
        }
        let mut view = View {
            surface,
            swapchain: vk::SwapchainKHR::null(),
            images: Vec::new(),
            image_views: Vec::new(),
            render_done: Vec::new(),
            acquire,
            extent: vk::Extent2D::default(),
            present_mode: vk::PresentModeKHR::FIFO,
            window_size: size,
            viewport,
            scale: ViewScale::Native,
            overlay: OverlayState::default(),
            overlay_images: Vec::new(),
            minimized: false,
            stale: true,
            diag: std::cell::Cell::new((0, 0, 0)),
        };
        self.build_swapchain(&mut view)?;
        let res = self.present.as_mut().expect("created above");
        let id = res.next_id;
        res.next_id += 1;
        res.views.insert(id, view);
        Ok(id)
    }

    fn build_swapchain(&self, view: &mut View) -> Result<()> {
        let wsi = self.vk.wsi.as_ref().context("no WSI")?;
        let caps = unsafe {
            wsi.surface
                .get_physical_device_surface_capabilities(self.vk.physical, view.surface)
        }?;
        let formats = unsafe {
            wsi.surface
                .get_physical_device_surface_formats(self.vk.physical, view.surface)
        }?;
        let format = formats
            .iter()
            .find(|f| f.format == vk::Format::B8G8R8A8_UNORM)
            .or_else(|| {
                formats
                    .iter()
                    .find(|f| f.format == vk::Format::R8G8B8A8_UNORM)
            })
            .copied()
            .context("no 8-bit UNORM swapchain format")?;
        if !caps
            .supported_usage_flags
            .contains(vk::ImageUsageFlags::STORAGE)
        {
            bail!("swapchain images do not support storage usage");
        }
        let modes = unsafe {
            wsi.surface
                .get_physical_device_surface_present_modes(self.vk.physical, view.surface)
        }?;
        // MAILBOX: newest frame wins, never blocks (240 Hz and 60 Hz outputs side by side).
        let present_mode = if modes.contains(&vk::PresentModeKHR::MAILBOX) {
            vk::PresentModeKHR::MAILBOX
        } else {
            vk::PresentModeKHR::FIFO
        };
        let extent = if caps.current_extent.width != u32::MAX {
            caps.current_extent
        } else {
            vk::Extent2D {
                width: view
                    .window_size
                    .0
                    .clamp(caps.min_image_extent.width, caps.max_image_extent.width),
                height: view
                    .window_size
                    .1
                    .clamp(caps.min_image_extent.height, caps.max_image_extent.height),
            }
        };
        let mut count = caps.min_image_count + 1;
        if caps.max_image_count > 0 {
            count = count.min(caps.max_image_count);
        }
        let old = view.swapchain;
        let swapchain = unsafe {
            wsi.swapchain.create_swapchain(
                &vk::SwapchainCreateInfoKHR::default()
                    .surface(view.surface)
                    .min_image_count(count)
                    .image_format(format.format)
                    .image_color_space(format.color_space)
                    .image_extent(extent)
                    .image_array_layers(1)
                    .image_usage(vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_DST)
                    .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .pre_transform(vk::SurfaceTransformFlagsKHR::IDENTITY)
                    .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
                    .present_mode(present_mode)
                    .clipped(true)
                    .old_swapchain(old),
                None,
            )
        }
        .context("vkCreateSwapchainKHR")?;
        self.drop_swapchain_images(view);
        if old != vk::SwapchainKHR::null() {
            unsafe { wsi.swapchain.destroy_swapchain(old, None) };
        }
        view.swapchain = swapchain;
        view.images = unsafe { wsi.swapchain.get_swapchain_images(swapchain) }?;
        let d = &self.vk.device;
        for &img in &view.images {
            view.image_views.push(unsafe {
                d.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(img)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(format.format)
                        .subresource_range(COLOR_RANGE),
                    None,
                )
            }?);
            view.render_done
                .push(unsafe { d.create_semaphore(&vk::SemaphoreCreateInfo::default(), None) }?);
        }
        view.extent = extent;
        view.present_mode = present_mode;
        view.stale = false;
        Ok(())
    }

    fn drop_swapchain_images(&self, view: &mut View) {
        let d = &self.vk.device;
        for v in view.image_views.drain(..) {
            unsafe { d.destroy_image_view(v, None) };
        }
        for s in view.render_done.drain(..) {
            unsafe { d.destroy_semaphore(s, None) };
        }
        view.images.clear();
    }

    fn destroy_view_inner(&self, mut view: View) {
        let d = &self.vk.device;
        unsafe {
            let _ = d.queue_wait_idle(self.vk.queue);
        }
        self.drop_swapchain_images(&mut view);
        if let Some(wsi) = &self.vk.wsi {
            unsafe {
                wsi.swapchain.destroy_swapchain(view.swapchain, None);
                wsi.surface.destroy_surface(view.surface, None);
            }
        }
        for s in view.acquire.drain(..) {
            unsafe { d.destroy_semaphore(s, None) };
        }
    }

    /// Destroys a view (call before the window goes away).
    pub fn destroy_view(&mut self, id: ViewId) -> Result<()> {
        let view = self
            .present
            .as_mut()
            .and_then(|p| p.views.remove(&id))
            .context("unknown view")?;
        self.destroy_view_inner(view);
        Ok(())
    }

    fn view_mut(&mut self, id: ViewId) -> Result<&mut View> {
        self.present
            .as_mut()
            .and_then(|p| p.views.get_mut(&id))
            .context("unknown view")
    }

    /// The window was resized (swapchain is rebuilt before the next present).
    pub fn set_view_size(&mut self, id: ViewId, w: u32, h: u32) -> Result<()> {
        let v = self.view_mut(id)?;
        if v.window_size != (w, h) {
            v.window_size = (w, h);
            v.stale = true;
        }
        Ok(())
    }

    pub fn set_view_viewport(&mut self, id: ViewId, viewport: Rect) -> Result<()> {
        self.view_mut(id)?.viewport = viewport;
        Ok(())
    }

    pub fn set_view_scale(&mut self, id: ViewId, scale: ViewScale) -> Result<()> {
        self.view_mut(id)?.scale = scale;
        Ok(())
    }

    pub fn set_view_overlay(&mut self, id: ViewId, overlay: OverlayState) -> Result<()> {
        self.view_mut(id)?.overlay = overlay;
        Ok(())
    }

    /// Pictures drawn on top of the view's overlay (replaces the previous ones).
    pub fn set_view_images(&mut self, id: ViewId, images: Vec<OverlayImage>) -> Result<()> {
        let v = self.view_mut(id)?;
        if v.overlay_images != images {
            v.overlay_images = images;
        }
        Ok(())
    }

    /// Minimised views draw nothing (decoding continues).
    pub fn set_view_minimized(&mut self, id: ViewId, minimized: bool) -> Result<()> {
        self.view_mut(id)?.minimized = minimized;
        Ok(())
    }

    /// Transformation view <-> session of a view (same as used for presenting).
    pub fn view_transform(&self, id: ViewId) -> Result<ViewTransform> {
        let v = self
            .present
            .as_ref()
            .and_then(|p| p.views.get(&id))
            .context("unknown view")?;
        let (w, h) = if v.extent.width > 0 {
            (v.extent.width, v.extent.height)
        } else {
            v.window_size
        };
        Ok(ViewTransform::new(v.scale, v.viewport, w, h))
    }

    /// Swapchain size, present mode of a view (diagnostics).
    pub fn view_info(&self, id: ViewId) -> Option<((u32, u32), vk::PresentModeKHR)> {
        let v = self.present.as_ref()?.views.get(&id)?;
        Some(((v.extent.width, v.extent.height), v.present_mode))
    }

    /// Diagnostics per view since the last call (see [`ViewDiag`]).
    pub fn take_view_diag(&self, id: ViewId) -> Option<ViewDiag> {
        let v = self.present.as_ref()?.views.get(&id)?;
        let (presents, not_acquired, present_errors) = v.diag.replace((0, 0, 0));
        Some(ViewDiag {
            presents,
            not_acquired,
            present_errors,
            viewport: v.viewport,
            minimized: v.minimized,
            stale: v.stale,
        })
    }

    /// Submits pending surface work, then draws and presents every visible view.
    pub fn present(&mut self) -> Result<PresentStats> {
        let mut stats = self.present_pass()?;
        // A view whose swapchain was rebuilt (resize, fullscreen <-> windowed) shows nothing
        // until it is drawn again; without new session frames (static desktop) that would
        // only happen on the next overlay change. Draw it once more right away.
        if stats.recreated > 0 {
            let again = self.present_pass()?;
            stats.presented += again.presented;
            stats.recreated += again.recreated;
        }
        Ok(stats)
    }

    fn present_pass(&mut self) -> Result<PresentStats> {
        let batch = self.submit()?;
        let mut stats = PresentStats::default();
        let Some(mut res) = self.present.take() else {
            // Headless: frames count as shown once their batch is done.
            self.release_frames(batch);
            return Ok(stats);
        };
        let r = self
            .prepare_images(&mut res)
            .and_then(|()| self.present_views(&res, &mut stats));
        let (stale, serial) = match &r {
            Ok((st, se)) => (st.clone(), *se),
            Err(_) => (Vec::new(), None),
        };
        if let Some(serial) = serial {
            res.serials[res.cur] = serial;
            res.cur = (res.cur + 1) % SLOTS;
        }
        // Nothing presented (all minimised): acknowledge with the decode batch.
        self.release_frames(serial.unwrap_or(batch));
        // Out-of-date or resized views get a new swapchain right away.
        let mut rebuilt = Ok(());
        for v in res.views.values_mut() {
            if stale.contains(&v.swapchain) {
                v.stale = true;
            }
            if v.stale && !v.minimized {
                unsafe {
                    let _ = self.vk.device.queue_wait_idle(self.vk.queue);
                }
                if let Err(e) = self.build_swapchain(v) {
                    rebuilt = Err(e);
                }
                stats.recreated += 1;
            }
        }
        self.present = Some(res);
        r.map(|_| ()).and(rebuilt).map(|()| stats)
    }

    /// Draws and presents from the current slot; returns the swapchains that turned out of
    /// date or suboptimal and the timeline value of the present (if anything was shown).
    fn present_views(
        &self,
        res: &PresentRes,
        stats: &mut PresentStats,
    ) -> Result<(Vec<vk::SwapchainKHR>, Option<u64>)> {
        // The slot's previous present must be done (command buffer, acquire semaphores).
        self.wait_serial(res.serials[res.cur])?;
        let wsi = self.vk.wsi.as_ref().context("no WSI")?;
        let d = &self.vk.device;
        let mapped = self.mapped_surfaces();
        // Acquire an image of every visible, up-to-date view.
        let sp = self.profile.start(Stage::Acquire);
        let mut targets: Vec<(&View, u32)> = Vec::new();
        // Swapchains the present engine reported out of date at acquire: rebuilt by the
        // caller like the ones reported by vkQueuePresentKHR (else the view freezes).
        let mut out_of_date: Vec<vk::SwapchainKHR> = Vec::new();
        for v in res.views.values() {
            if v.minimized {
                stats.skipped_minimized += 1;
                continue;
            }
            if v.stale {
                continue;
            }
            match unsafe {
                wsi.swapchain.acquire_next_image(
                    v.swapchain,
                    u64::MAX,
                    v.acquire[res.cur],
                    vk::Fence::null(),
                )
            } {
                Ok((idx, _suboptimal)) => targets.push((v, idx)),
                Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => {
                    let (p, a, r) = v.diag.get();
                    v.diag.set((p, a + 1, r));
                    out_of_date.push(v.swapchain);
                }
                Err(e) => bail!("vkAcquireNextImageKHR: {e}"),
            }
        }
        self.profile.end(sp);
        if targets.is_empty() {
            return Ok((out_of_date, None));
        }
        let rec = self.profile.start(Stage::PresentRecord);
        let cmd = res.cmds[res.cur];
        unsafe {
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            // Surface writes of earlier submits -> present shader reads.
            let mb = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)];
            let to_general: Vec<_> = targets
                .iter()
                .map(|(v, i)| {
                    vk::ImageMemoryBarrier::default()
                        .old_layout(vk::ImageLayout::UNDEFINED)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .dst_access_mask(
                            vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE,
                        )
                        .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                        .image(v.images[*i as usize])
                        .subresource_range(COLOR_RANGE)
                })
                .collect();
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER
                    | vk::PipelineStageFlags::TRANSFER
                    | vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &mb,
                &[],
                &to_general,
            );
        }
        for (v, idx) in &targets {
            let view_id = res
                .views
                .iter()
                .find(|(_, x)| std::ptr::eq(*x, *v))
                .map_or(0, |(id, _)| *id);
            let img = v.images[*idx as usize];
            let target_view = v.image_views[*idx as usize];
            let (w, h) = (v.extent.width, v.extent.height);
            let t = ViewTransform::new(v.scale, v.viewport, w, h);
            let pieces = compose(&mapped, v.viewport);
            let exact = t.scale == 1.0 && t.offset == (0.0, 0.0);
            if !exact || uncovered_pixels(&pieces, v.viewport) > 0 {
                unsafe {
                    d.cmd_clear_color_image(
                        cmd,
                        img,
                        vk::ImageLayout::GENERAL,
                        &vk::ClearColorValue { float32: NEUTRAL },
                        &[COLOR_RANGE],
                    );
                    let mb = [vk::MemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::SHADER_WRITE)];
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::COMPUTE_SHADER,
                        vk::DependencyFlags::empty(),
                        &mb,
                        &[],
                        &[],
                    );
                }
            }
            unsafe { d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, res.pipe_present) };
            for p in &pieces {
                let Some(s) = self.surfaces.get(&p.surface) else {
                    continue;
                };
                let origin = s.origin.unwrap_or((0, 0));
                // The piece in session coordinates and its rectangle in view pixels.
                let session = Rect::new(
                    p.dst.left + v.viewport.left,
                    p.dst.top + v.viewport.top,
                    p.dst.right + v.viewport.left,
                    p.dst.bottom + v.viewport.top,
                );
                let (x0, y0) = t.session_to_view(session.left, session.top);
                let x1 = t.offset.0 + f64::from(session.right - v.viewport.left) * t.scale;
                let y1 = t.offset.1 + f64::from(session.bottom - v.viewport.top) * t.scale;
                let dst = Rect::new(
                    x0.floor().max(0.0) as u32,
                    y0.floor().max(0.0) as u32,
                    (x1.ceil() as u32).min(w),
                    (y1.ceil() as u32).min(h),
                );
                if dst.is_empty() {
                    continue;
                }
                let params = PresentParams {
                    dst: [dst.left, dst.top, dst.right, dst.bottom],
                    piece: [session.left, session.top, session.right, session.bottom],
                    viewport: [v.viewport.left as i32, v.viewport.top as i32],
                    origin: [origin.0 as i32, origin.1 as i32],
                    offset: [t.offset.0 as f32, t.offset.1 as f32],
                    scale: t.scale as f32,
                    pad0: 0,
                };
                self.push_present(res, cmd, target_view, Some(s.image.storage_view));
                unsafe {
                    d.cmd_push_constants(
                        cmd,
                        res.layout,
                        vk::ShaderStageFlags::COMPUTE,
                        0,
                        bytes(&params),
                    );
                    d.cmd_dispatch(cmd, dst.width().div_ceil(16), dst.height().div_ceil(16), 1);
                }
            }
            let screen = Rect::new(0, 0, w, h);
            let mut items: Vec<(Rect, u32)> = Vec::new();
            for t in v.overlay.thumbs.iter().flatten() {
                if let Some(r) = t.intersect(&screen) {
                    items.push((r, 1));
                }
            }
            if let Some(r) = v.overlay.bar.and_then(|b| b.intersect(&screen)) {
                items.push((r, 0));
            }
            let progress = v.overlay.progress;
            if let Some(r) = v.overlay.progress.and_then(|p| p.rect.intersect(&screen)) {
                items.push((r, 2));
            }
            // Images last, on top of the bar; kind 3 + index into the view's images.
            let offsets = res.image_offsets.get(&view_id);
            for (k, img) in v.overlay_images.iter().enumerate() {
                let rect = Rect::from_xywh(img.x, img.y, img.width, img.height);
                if offsets.is_some()
                    && let Some(r) = rect.intersect(&screen)
                {
                    items.push((r, 3 | ((k as u32) << 8)));
                }
            }
            if !items.is_empty() {
                unsafe {
                    let mb = [vk::MemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::SHADER_WRITE)
                        .dst_access_mask(vk::AccessFlags::SHADER_WRITE)];
                    d.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::COMPUTE_SHADER,
                        vk::PipelineStageFlags::COMPUTE_SHADER,
                        vk::DependencyFlags::empty(),
                        &mb,
                        &[],
                        &[],
                    );
                    d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, res.pipe_overlay);
                }
                self.push_present(res, cmd, target_view, None);
            }
            for (r, item) in items {
                let kind = item & 0xFF;
                let image = (kind == 3).then(|| {
                    let k = (item >> 8) as usize;
                    let img = &v.overlay_images[k];
                    let off = offsets.map_or(0, |o| o[k]);
                    [off, img.width, img.x, img.y]
                });
                let params = OverlayParams {
                    bar: [r.left, r.top, r.right, r.bottom],
                    pinned: if kind == 2 {
                        progress.map_or(0, |p| p.permyriad.min(10000))
                    } else {
                        u32::from(v.overlay.pinned)
                    },
                    hover: v.overlay.hover,
                    kind,
                    grab: u32::from(v.overlay.grab),
                    keep: u32::from(v.overlay.keep_active),
                    text: match progress {
                        Some(p) if kind == 2 => std::array::from_fn(|i| {
                            u32::from_le_bytes(p.text[i * 4..i * 4 + 4].try_into().expect("4"))
                        }),
                        _ => image.unwrap_or([0; 4]),
                    },
                    buttons: v.overlay.buttons.packed,
                    nbuttons: v.overlay.buttons.count,
                };
                unsafe {
                    d.cmd_push_constants(
                        cmd,
                        res.layout,
                        vk::ShaderStageFlags::COMPUTE,
                        0,
                        bytes(&params),
                    );
                    d.cmd_dispatch(cmd, r.width().div_ceil(16), r.height().div_ceil(16), 1);
                }
            }
        }
        let to_present: Vec<_> = targets
            .iter()
            .map(|(v, i)| {
                vk::ImageMemoryBarrier::default()
                    .old_layout(vk::ImageLayout::GENERAL)
                    .new_layout(vk::ImageLayout::PRESENT_SRC_KHR)
                    .src_access_mask(
                        vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::TRANSFER_WRITE,
                    )
                    .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                    .image(v.images[*i as usize])
                    .subresource_range(COLOR_RANGE)
            })
            .collect();
        let waits: Vec<_> = targets.iter().map(|(v, _)| v.acquire[res.cur]).collect();
        let stages = vec![
            vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER;
            waits.len()
        ];
        let render_done: Vec<_> = targets
            .iter()
            .map(|(v, i)| v.render_done[*i as usize])
            .collect();
        let serial = self.take_serial();
        let mut signals = render_done.clone();
        signals.push(self.timeline);
        let mut signal_values = vec![0u64; render_done.len()];
        signal_values.push(serial);
        let wait_values = vec![0u64; waits.len()];
        let mut tl = vk::TimelineSemaphoreSubmitInfo::default()
            .wait_semaphore_values(&wait_values)
            .signal_semaphore_values(&signal_values);
        let cmds = [cmd];
        unsafe {
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER | vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &to_present,
            );
            d.end_command_buffer(cmd)?;
            self.profile.end(rec);
            let sp = self.profile.start(Stage::PresentSubmit);
            d.queue_submit(
                self.vk.queue,
                &[vk::SubmitInfo::default()
                    .wait_semaphores(&waits)
                    .wait_dst_stage_mask(&stages)
                    .command_buffers(&cmds)
                    .signal_semaphores(&signals)
                    .push_next(&mut tl)],
                vk::Fence::null(),
            )?;
            self.profile.end(sp);
        }
        let swapchains: Vec<_> = targets.iter().map(|(v, _)| v.swapchain).collect();
        let indices: Vec<_> = targets.iter().map(|(_, i)| *i).collect();
        let mut results = vec![vk::Result::SUCCESS; targets.len()];
        let sp = self.profile.start(Stage::QueuePresent);
        let pr = unsafe {
            wsi.swapchain.queue_present(
                self.vk.queue,
                &vk::PresentInfoKHR::default()
                    .wait_semaphores(&render_done)
                    .swapchains(&swapchains)
                    .image_indices(&indices)
                    .results(&mut results),
            )
        };
        self.profile.end(sp);
        let mut stale: Vec<vk::SwapchainKHR> = targets
            .iter()
            .zip(&results)
            .filter(|(_, r)| {
                matches!(
                    **r,
                    vk::Result::ERROR_OUT_OF_DATE_KHR | vk::Result::SUBOPTIMAL_KHR
                )
            })
            .map(|((v, _), _)| v.swapchain)
            .collect();
        stale.extend(out_of_date);
        stats.presented += targets.len() as u32;
        for ((v, _), r) in targets.iter().zip(&results) {
            let (p, a, n) = v.diag.get();
            v.diag
                .set((p + 1, a, n + u32::from(*r != vk::Result::SUCCESS)));
        }
        match pr {
            Ok(_) | Err(vk::Result::ERROR_OUT_OF_DATE_KHR) => Ok((stale, Some(serial))),
            Err(e) => bail!("vkQueuePresentKHR: {e}"),
        }
    }

    /// Copies the overlay images of every view into the current slot's host buffer (after
    /// the slot's previous present finished) and records their offsets.
    fn prepare_images(&self, res: &mut PresentRes) -> Result<()> {
        self.wait_serial(res.serials[res.cur])?;
        let total: usize = res
            .views
            .values()
            .flat_map(|v| &v.overlay_images)
            .map(|i| i.pixels.len())
            .sum();
        // Always a buffer: the overlay shader declares binding 2.
        let need = (total.max(1) * 4) as u64;
        if res.image_bufs[res.cur]
            .as_ref()
            .is_none_or(|b| b.size < need)
        {
            if let Some(old) = res.image_bufs[res.cur].take() {
                self.vk.destroy_buffer(old);
            }
            let size = need.max(256 * 1024).next_power_of_two();
            res.image_bufs[res.cur] =
                Some(
                    self.vk
                        .buffer(size, vk::BufferUsageFlags::STORAGE_BUFFER, false, true)?,
                );
        }
        let buf = res.image_bufs[res.cur].as_ref().expect("created above");
        let base = buf.mapped.context("image buffer not mapped")? as *mut u32;
        res.image_offsets.clear();
        let mut at = 0usize;
        for (id, v) in &res.views {
            let mut offs = Vec::with_capacity(v.overlay_images.len());
            for img in &v.overlay_images {
                let n = img.pixels.len().min((img.width * img.height) as usize);
                // SAFETY: the buffer holds `total` pixels; the GPU no longer reads this slot.
                unsafe { std::ptr::copy_nonoverlapping(img.pixels.as_ptr(), base.add(at), n) };
                offs.push(at as u32);
                at += img.pixels.len();
            }
            res.image_offsets.insert(*id, offs);
        }
        Ok(())
    }

    fn push_present(
        &self,
        res: &PresentRes,
        cmd: vk::CommandBuffer,
        target: vk::ImageView,
        source: Option<vk::ImageView>,
    ) {
        let t = [vk::DescriptorImageInfo::default()
            .image_view(target)
            .image_layout(vk::ImageLayout::GENERAL)];
        let s = [vk::DescriptorImageInfo::default()
            .image_view(source.unwrap_or(target))
            .image_layout(vk::ImageLayout::GENERAL)];
        let mut writes = vec![
            vk::WriteDescriptorSet::default()
                .dst_binding(0)
                .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                .image_info(&t),
        ];
        if source.is_some() {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                    .image_info(&s),
            );
        }
        // Overlay pass: the slot's image buffer (created by `prepare_images`).
        let b = res.image_bufs[res.cur].as_ref().map(|b| {
            [vk::DescriptorBufferInfo::default()
                .buffer(b.buffer)
                .offset(0)
                .range(vk::WHOLE_SIZE)]
        });
        if source.is_none()
            && let Some(b) = &b
        {
            writes.push(
                vk::WriteDescriptorSet::default()
                    .dst_binding(2)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(b),
            );
        }
        unsafe {
            self.vk.push_desc.cmd_push_descriptor_set(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                res.layout,
                0,
                &writes,
            )
        };
    }

    pub(super) fn destroy_present(&mut self) {
        let Some(mut res) = self.present.take() else {
            return;
        };
        for (_, v) in std::mem::take(&mut res.views) {
            self.destroy_view_inner(v);
        }
        for b in res.image_bufs.drain(..).flatten() {
            self.vk.destroy_buffer(b);
        }
        let d = &self.vk.device;
        unsafe {
            d.free_command_buffers(self.vk.pool, &res.cmds);
            d.destroy_pipeline(res.pipe_present, None);
            d.destroy_pipeline(res.pipe_overlay, None);
            d.destroy_pipeline_layout(res.layout, None);
            d.destroy_descriptor_set_layout(res.set_layout, None);
        }
    }
}
