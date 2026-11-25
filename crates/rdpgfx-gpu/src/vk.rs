//! Vulkan context for the GPU core (compute, transfer and presentation).

use std::ffi::{CStr, c_char};

use anyhow::{Context, Result, anyhow, bail};
use ash::vk;

pub struct Vk {
    pub entry: ash::Entry,
    pub instance: ash::Instance,
    pub physical: vk::PhysicalDevice,
    pub device: ash::Device,
    pub queue: vk::Queue,
    pub queue_family: u32,
    pub mem_props: vk::PhysicalDeviceMemoryProperties,
    pub ext_mem_fd: ash::khr::external_memory_fd::Device,
    pub push_desc: ash::khr::push_descriptor::Device,
    /// Present support (surface + swapchain loaders), when created with window extensions.
    pub wsi: Option<Wsi>,
    pub device_name: String,
    pub pool: vk::CommandPool,
}

/// Window-system loaders.
pub struct Wsi {
    pub surface: ash::khr::surface::Instance,
    pub swapchain: ash::khr::swapchain::Device,
}

/// A 2D colour image with its own memory. The image format is RGBA8 (sampled later for
/// presentation); `storage_view` reinterprets every texel as one `u32` (R32_UINT) so shaders
/// write exact BGRA bytes.
pub struct Image {
    pub image: vk::Image,
    pub memory: vk::DeviceMemory,
    pub storage_view: vk::ImageView,
    pub width: u32,
    pub height: u32,
}

/// Subresource range / layers of a single-mip colour image.
pub const COLOR_RANGE: vk::ImageSubresourceRange = vk::ImageSubresourceRange {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    base_mip_level: 0,
    level_count: 1,
    base_array_layer: 0,
    layer_count: 1,
};
pub const COLOR_LAYERS: vk::ImageSubresourceLayers = vk::ImageSubresourceLayers {
    aspect_mask: vk::ImageAspectFlags::COLOR,
    mip_level: 0,
    base_array_layer: 0,
    layer_count: 1,
};

/// A buffer with its own (dedicated) memory allocation.
pub struct Buffer {
    pub buffer: vk::Buffer,
    pub memory: vk::DeviceMemory,
    pub size: u64,
    /// Host pointer for host-visible buffers.
    pub mapped: Option<*mut u8>,
}

impl Vk {
    /// Creates instance and device on the GPU whose UUID matches `uuid` (the CUDA device),
    /// or the first discrete GPU when `uuid` is `None`. `display` enables presentation
    /// (surface extensions for that window system, swapchain on the device).
    pub fn new(
        uuid: Option<[u8; 16]>,
        display: Option<raw_window_handle::RawDisplayHandle>,
    ) -> Result<Self> {
        let entry = unsafe { ash::Entry::load() }.context("load Vulkan loader")?;
        let app = vk::ApplicationInfo::default()
            .application_name(c"RMC gpu core")
            .api_version(vk::API_VERSION_1_2);
        let validation = c"VK_LAYER_KHRONOS_validation";
        let layers: Vec<*const c_char> = if std::env::var_os("RDPGFX_VALIDATION").is_some() {
            vec![validation.as_ptr()]
        } else {
            vec![]
        };
        let inst_exts: &[*const c_char] = match display {
            Some(d) => ash_window::enumerate_required_extensions(d)
                .context("window system not supported by Vulkan")?,
            None => &[],
        };
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app)
                    .enabled_layer_names(&layers)
                    .enabled_extension_names(inst_exts),
                None,
            )
        }
        .context("vkCreateInstance")?;

        // With a CUDA device: exactly that GPU (shared memory). Otherwise the best device:
        // discrete, then integrated, then virtual, then a CPU implementation (llvmpipe).
        // RMC_VULKAN_DEVICE=<part of the name> overrides the choice.
        let want_name = std::env::var("RMC_VULKAN_DEVICE")
            .ok()
            .map(|v| v.to_lowercase());
        let mut chosen: Option<(i32, vk::PhysicalDevice, String)> = None;
        for pd in unsafe { instance.enumerate_physical_devices()? } {
            let mut id = vk::PhysicalDeviceIDProperties::default();
            let mut props2 = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
            unsafe { instance.get_physical_device_properties2(pd, &mut props2) };
            let props = props2.properties;
            let name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
                .to_string_lossy()
                .into_owned();
            let has_compute = unsafe { instance.get_physical_device_queue_family_properties(pd) }
                .iter()
                .any(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE));
            if !has_compute {
                continue;
            }
            let score = match (uuid, &want_name) {
                (Some(u), _) => {
                    if id.device_uuid == u {
                        100
                    } else {
                        continue;
                    }
                }
                (None, Some(w)) if name.to_lowercase().contains(w.as_str()) => 50,
                (None, _) => match props.device_type {
                    vk::PhysicalDeviceType::DISCRETE_GPU => 4,
                    vk::PhysicalDeviceType::INTEGRATED_GPU => 3,
                    vk::PhysicalDeviceType::VIRTUAL_GPU => 2,
                    vk::PhysicalDeviceType::CPU => 1,
                    _ => 0,
                },
            };
            if chosen.as_ref().is_none_or(|c| score > c.0) {
                chosen = Some((score, pd, name));
            }
        }
        let (_, physical, device_name) = chosen.ok_or_else(|| match uuid {
            Some(_) => anyhow!("no Vulkan device matches the CUDA device"),
            None => anyhow!("no Vulkan device with a compute queue"),
        })?;

        let queue_family = unsafe { instance.get_physical_device_queue_family_properties(physical) }
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::COMPUTE))
            .ok_or_else(|| anyhow!("no compute queue"))? as u32;

        let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
        let mut f2 = vk::PhysicalDeviceFeatures2::default().push_next(&mut f12);
        unsafe { instance.get_physical_device_features2(physical, &mut f2) };
        let base = f2.features;
        if f12.timeline_semaphore == vk::FALSE {
            bail!("device lacks timelineSemaphore");
        }
        if f12.storage_buffer8_bit_access == vk::FALSE {
            bail!("device lacks storageBuffer8BitAccess");
        }
        let mut want12 = vk::PhysicalDeviceVulkan12Features::default()
            .storage_buffer8_bit_access(true)
            .timeline_semaphore(true)
            .shader_int8(f12.shader_int8 == vk::TRUE);
        let mut features = vk::PhysicalDeviceFeatures::default();
        if display.is_some() {
            // The present shader writes swapchain images (BGRA) without a format qualifier.
            if base.shader_storage_image_write_without_format == vk::FALSE {
                bail!("device lacks shaderStorageImageWriteWithoutFormat");
            }
            features.shader_storage_image_write_without_format = vk::TRUE;
        }
        let prio = [1.0f32];
        let qinfo = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(queue_family)
            .queue_priorities(&prio)];
        let mut exts = vec![
            ash::khr::external_memory_fd::NAME.as_ptr(),
            ash::khr::push_descriptor::NAME.as_ptr(),
        ];
        if display.is_some() {
            exts.push(ash::khr::swapchain::NAME.as_ptr());
        }
        let device = unsafe {
            instance.create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&qinfo)
                    .enabled_extension_names(&exts)
                    .enabled_features(&features)
                    .push_next(&mut want12),
                None,
            )
        }
        .context("vkCreateDevice")?;
        let queue = unsafe { device.get_device_queue(queue_family, 0) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(physical) };
        let ext_mem_fd = ash::khr::external_memory_fd::Device::new(&instance, &device);
        let push_desc = ash::khr::push_descriptor::Device::new(&instance, &device);
        let wsi = display.map(|_| Wsi {
            surface: ash::khr::surface::Instance::new(&entry, &instance),
            swapchain: ash::khr::swapchain::Device::new(&instance, &device),
        });
        let pool = unsafe {
            device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(queue_family)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }?;
        Ok(Self {
            entry,
            instance,
            physical,
            device,
            queue,
            queue_family,
            mem_props,
            ext_mem_fd,
            push_desc,
            wsi,
            device_name,
            pool,
        })
    }

    fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Result<u32> {
        (0..self.mem_props.memory_type_count)
            .find(|&i| {
                bits & (1 << i) != 0
                    && self.mem_props.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .ok_or_else(|| anyhow!("no memory type with {flags:?}"))
    }

    /// Creates a buffer. `exportable` adds OPAQUE_FD export (for CUDA import),
    /// `host` makes it host-visible and maps it.
    pub fn buffer(
        &self,
        size: u64,
        usage: vk::BufferUsageFlags,
        exportable: bool,
        host: bool,
    ) -> Result<Buffer> {
        let mut ext_info = vk::ExternalMemoryBufferCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let mut info = vk::BufferCreateInfo::default()
            .size(size)
            .usage(usage)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);
        if exportable {
            info = info.push_next(&mut ext_info);
        }
        let buffer = unsafe { self.device.create_buffer(&info, None) }?;
        let req = unsafe { self.device.get_buffer_memory_requirements(buffer) };
        let flags = if host {
            vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
        } else {
            vk::MemoryPropertyFlags::DEVICE_LOCAL
        };
        let mut export = vk::ExportMemoryAllocateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
        let mut alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(self.memory_type(req.memory_type_bits, flags)?)
            .push_next(&mut dedicated);
        if exportable {
            alloc = alloc.push_next(&mut export);
        }
        let memory =
            unsafe { self.device.allocate_memory(&alloc, None) }.context("vkAllocateMemory")?;
        unsafe { self.device.bind_buffer_memory(buffer, memory, 0) }?;
        let mapped = if host {
            Some(unsafe {
                self.device
                    .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())
            }? as *mut u8)
        } else {
            None
        };
        Ok(Buffer {
            buffer,
            memory,
            size: req.size,
            mapped,
        })
    }

    /// Creates a device-local RGBA8 image (usage: storage, transfer, sampled) with an
    /// R32_UINT storage view. The caller transitions it to GENERAL before use.
    pub fn image(&self, width: u32, height: u32) -> Result<Image> {
        let info = vk::ImageCreateInfo::default()
            .flags(vk::ImageCreateFlags::MUTABLE_FORMAT)
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::R8G8B8A8_UNORM)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(
                vk::ImageUsageFlags::STORAGE
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST
                    | vk::ImageUsageFlags::SAMPLED,
            )
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);
        let image = unsafe { self.device.create_image(&info, None) }.context("vkCreateImage")?;
        let req = unsafe { self.device.get_image_memory_requirements(image) };
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(req.size)
            .memory_type_index(
                self.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)?,
            )
            .push_next(&mut dedicated);
        let memory =
            unsafe { self.device.allocate_memory(&alloc, None) }.context("vkAllocateMemory")?;
        unsafe { self.device.bind_image_memory(image, memory, 0) }?;
        let storage_view = unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::R32_UINT)
                    .subresource_range(COLOR_RANGE),
                None,
            )
        }?;
        Ok(Image {
            image,
            memory,
            storage_view,
            width,
            height,
        })
    }

    pub fn destroy_image(&self, i: Image) {
        unsafe {
            self.device.destroy_image_view(i.storage_view, None);
            self.device.destroy_image(i.image, None);
            self.device.free_memory(i.memory, None);
        }
    }

    /// Exports the buffer's memory as an OPAQUE_FD (ownership of the fd goes to the caller).
    pub fn export_fd(&self, b: &Buffer) -> Result<i32> {
        let info = vk::MemoryGetFdInfoKHR::default()
            .memory(b.memory)
            .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        unsafe { self.ext_mem_fd.get_memory_fd(&info) }.context("vkGetMemoryFdKHR")
    }

    pub fn destroy_buffer(&self, b: Buffer) {
        unsafe {
            if b.mapped.is_some() {
                self.device.unmap_memory(b.memory);
            }
            self.device.destroy_buffer(b.buffer, None);
            self.device.free_memory(b.memory, None);
        }
    }

    /// Compute pipeline from SPIR-V bytes.
    pub fn compute_pipeline(&self, spv: &[u8], layout: vk::PipelineLayout) -> Result<vk::Pipeline> {
        let code = ash::util::read_spv(&mut std::io::Cursor::new(spv))?;
        let module = unsafe {
            self.device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)
        }?;
        let stage = vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::COMPUTE)
            .module(module)
            .name(c"main");
        let info = [vk::ComputePipelineCreateInfo::default()
            .stage(stage)
            .layout(layout)];
        let p = unsafe {
            self.device
                .create_compute_pipelines(vk::PipelineCache::null(), &info, None)
        }
        .map_err(|(_, e)| anyhow!("vkCreateComputePipelines: {e}"))?[0];
        unsafe { self.device.destroy_shader_module(module, None) };
        Ok(p)
    }
}

impl Drop for Vk {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}
