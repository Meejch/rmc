//! Minimal CUDA driver API, loaded at run time from libcuda.so (no toolkit needed).
//!
//! Only what the Vulkan interop needs: import of exported Vulkan memory and
//! device-to-device 2D copies out of FFmpeg's CUDA frames.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};

use anyhow::{Context, Result, bail};
use libloading::Library;

pub type CuResult = c_int;
pub type CuContext = *mut c_void;
pub type CuStream = *mut c_void;
pub type CuDevicePtr = u64;
pub type CuExternalMemory = *mut c_void;

const CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD: c_uint = 1;
const CUDA_EXTERNAL_MEMORY_DEDICATED: c_uint = 1;
pub const CU_MEMORYTYPE_HOST: c_uint = 1;
pub const CU_MEMORYTYPE_DEVICE: c_uint = 2;

#[repr(C)]
struct ExternalMemoryHandleDesc {
    kind: c_uint,
    // union { int fd; struct { void* handle; const void* name; } win32; const void* nvSciBufObject; }
    handle: [u64; 2],
    size: u64,
    flags: c_uint,
    reserved: [c_uint; 16],
}

#[repr(C)]
struct ExternalMemoryBufferDesc {
    offset: u64,
    size: u64,
    flags: c_uint,
    reserved: [c_uint; 16],
}

#[repr(C)]
#[derive(Default)]
pub struct Memcpy2D {
    pub src_x_in_bytes: usize,
    pub src_y: usize,
    pub src_memory_type: c_uint,
    pub src_host: usize,
    pub src_device: CuDevicePtr,
    pub src_array: usize,
    pub src_pitch: usize,
    pub dst_x_in_bytes: usize,
    pub dst_y: usize,
    pub dst_memory_type: c_uint,
    pub dst_host: usize,
    pub dst_device: CuDevicePtr,
    pub dst_array: usize,
    pub dst_pitch: usize,
    pub width_in_bytes: usize,
    pub height: usize,
}

type FnCtx = unsafe extern "C" fn(CuContext) -> CuResult;
type FnCtxPop = unsafe extern "C" fn(*mut CuContext) -> CuResult;
type FnImport =
    unsafe extern "C" fn(*mut CuExternalMemory, *const ExternalMemoryHandleDesc) -> CuResult;
type FnMapped = unsafe extern "C" fn(
    *mut CuDevicePtr,
    CuExternalMemory,
    *const ExternalMemoryBufferDesc,
) -> CuResult;
type FnDestroy = unsafe extern "C" fn(CuExternalMemory) -> CuResult;
type FnFree = unsafe extern "C" fn(CuDevicePtr) -> CuResult;
type FnCopy = unsafe extern "C" fn(*const Memcpy2D, CuStream) -> CuResult;
type FnSync = unsafe extern "C" fn(CuStream) -> CuResult;
type FnGetDevice = unsafe extern "C" fn(*mut c_int) -> CuResult;
type FnUuid = unsafe extern "C" fn(*mut [u8; 16], c_int) -> CuResult;
type FnErr = unsafe extern "C" fn(CuResult, *mut *const c_char) -> CuResult;

pub struct Cuda {
    _lib: Library,
    ctx_push: FnCtx,
    ctx_pop: FnCtxPop,
    import: FnImport,
    mapped: FnMapped,
    destroy: FnDestroy,
    free: FnFree,
    copy2d: FnCopy,
    stream_sync: FnSync,
    ctx_get_device: FnGetDevice,
    device_uuid: FnUuid,
    error_string: FnErr,
}

macro_rules! sym {
    ($lib:expr, $name:literal) => {
        *unsafe { $lib.get(concat!($name, "\0").as_bytes()) }
            .with_context(|| format!("libcuda: {}", $name))?
    };
}

impl Cuda {
    pub fn load() -> Result<Self> {
        let lib =
            unsafe { Library::new("libcuda.so.1") }.context("load libcuda.so.1 (NVIDIA driver)")?;
        Ok(Self {
            ctx_push: sym!(lib, "cuCtxPushCurrent_v2"),
            ctx_pop: sym!(lib, "cuCtxPopCurrent_v2"),
            import: sym!(lib, "cuImportExternalMemory"),
            mapped: sym!(lib, "cuExternalMemoryGetMappedBuffer"),
            destroy: sym!(lib, "cuDestroyExternalMemory"),
            free: sym!(lib, "cuMemFree_v2"),
            copy2d: sym!(lib, "cuMemcpy2DAsync_v2"),
            stream_sync: sym!(lib, "cuStreamSynchronize"),
            ctx_get_device: sym!(lib, "cuCtxGetDevice"),
            device_uuid: sym!(lib, "cuDeviceGetUuid_v2"),
            error_string: sym!(lib, "cuGetErrorString"),
            _lib: lib,
        })
    }

    pub fn check(&self, r: CuResult, what: &str) -> Result<()> {
        if r == 0 {
            return Ok(());
        }
        let mut s: *const c_char = std::ptr::null();
        unsafe { (self.error_string)(r, &mut s) };
        let msg = if s.is_null() {
            "?".into()
        } else {
            unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
        };
        bail!("{what}: CUDA error {r} ({msg})")
    }

    /// Runs `f` with `ctx` current on this thread.
    // CUDA handles are opaque and only passed back to the driver, never dereferenced here.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn with_context<T>(&self, ctx: CuContext, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.check(unsafe { (self.ctx_push)(ctx) }, "cuCtxPushCurrent")?;
        let r = f();
        let mut old = std::ptr::null_mut();
        let pop = self.check(unsafe { (self.ctx_pop)(&mut old) }, "cuCtxPopCurrent");
        let v = r?;
        pop?;
        Ok(v)
    }

    /// UUID of the current context's device (to pick the matching Vulkan device).
    pub fn current_device_uuid(&self) -> Result<[u8; 16]> {
        let mut dev = 0;
        self.check(unsafe { (self.ctx_get_device)(&mut dev) }, "cuCtxGetDevice")?;
        let mut uuid = [0u8; 16];
        self.check(
            unsafe { (self.device_uuid)(&mut uuid, dev) },
            "cuDeviceGetUuid",
        )?;
        Ok(uuid)
    }

    /// Imports an exported Vulkan allocation (OPAQUE_FD; CUDA takes ownership of `fd`)
    /// and maps it as one linear device buffer. Needs a current context.
    pub fn import_buffer(&self, fd: i32, size: u64) -> Result<ImportedBuffer> {
        let desc = ExternalMemoryHandleDesc {
            kind: CU_EXTERNAL_MEMORY_HANDLE_TYPE_OPAQUE_FD,
            handle: [fd as u32 as u64, 0],
            size,
            flags: CUDA_EXTERNAL_MEMORY_DEDICATED,
            reserved: [0; 16],
        };
        let mut ext = std::ptr::null_mut();
        self.check(
            unsafe { (self.import)(&mut ext, &desc) },
            "cuImportExternalMemory",
        )?;
        let bdesc = ExternalMemoryBufferDesc {
            offset: 0,
            size,
            flags: 0,
            reserved: [0; 16],
        };
        let mut ptr = 0;
        if let Err(e) = self.check(
            unsafe { (self.mapped)(&mut ptr, ext, &bdesc) },
            "cuExternalMemoryGetMappedBuffer",
        ) {
            unsafe { (self.destroy)(ext) };
            return Err(e);
        }
        Ok(ImportedBuffer { ext, ptr, size })
    }

    /// Releases an imported buffer. Needs a current context.
    pub fn release(&self, b: ImportedBuffer) {
        unsafe {
            (self.free)(b.ptr);
            (self.destroy)(b.ext);
        }
    }

    // CUDA handles are opaque and only passed back to the driver, never dereferenced here.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn copy_2d(&self, c: &Memcpy2D, stream: CuStream) -> Result<()> {
        self.check(unsafe { (self.copy2d)(c, stream) }, "cuMemcpy2DAsync")
    }

    // CUDA handles are opaque and only passed back to the driver, never dereferenced here.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn sync(&self, stream: CuStream) -> Result<()> {
        self.check(unsafe { (self.stream_sync)(stream) }, "cuStreamSynchronize")
    }
}

/// A Vulkan allocation visible to CUDA as a device pointer.
pub struct ImportedBuffer {
    ext: CuExternalMemory,
    pub ptr: CuDevicePtr,
    pub size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_layouts_match_cuda_h() {
        assert_eq!(std::mem::size_of::<ExternalMemoryHandleDesc>(), 104);
        assert_eq!(std::mem::size_of::<ExternalMemoryBufferDesc>(), 88);
        assert_eq!(std::mem::size_of::<Memcpy2D>(), 128);
    }
}
