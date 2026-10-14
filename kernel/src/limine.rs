//! The minimal subset of the Limine boot protocol that Nova uses.
//!
//! Spec: https://github.com/limine-bootloader/limine-protocol (PROTOCOL.md).
//! Limine scans the `.limine_requests` section for these magic-tagged structs
//! and patches them before jumping to `kmain`.

use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char};

const COMMON_MAGIC: [u64; 2] = [0xc7b1dd30df4c8b88, 0x0a82e883a194f07b];

/// Base revision we were written against.
pub const REQUESTED_REVISION: u64 = 3;

/// Base-revision tag. Limine sets word 2 to 0 if it supports the revision.
#[repr(C, align(8))]
pub struct BaseRevision(UnsafeCell<[u64; 3]>);

// SAFETY: written only by the bootloader before the kernel starts; read-only afterwards.
unsafe impl Sync for BaseRevision {}

impl BaseRevision {
    const fn new(rev: u64) -> Self {
        Self(UnsafeCell::new([0xf9562b2d5c95a6c8, 0x6a7b384944536bdc, rev]))
    }

    pub fn is_supported(&self) -> bool {
        // SAFETY: aligned pointer into our own static; volatile because the bootloader wrote it.
        unsafe { (self.0.get() as *const u64).add(2).read_volatile() == 0 }
    }
}

/// A Limine request: magic id, revision, a response pointer the bootloader fills
/// in, and request-specific trailing fields `E` (most requests have none).
#[repr(C, align(8))]
pub struct Request<R, E = ()> {
    id: [u64; 4],
    revision: u64,
    response: UnsafeCell<*const R>,
    extra: E,
}

// SAFETY: as for BaseRevision.
unsafe impl<R, E: Sync> Sync for Request<R, E> {}

impl<R> Request<R> {
    const fn new(id_high: [u64; 2]) -> Self {
        Request::with_extra(id_high, ())
    }
}

impl<R, E> Request<R, E> {
    const fn with_extra(id_high: [u64; 2], extra: E) -> Self {
        Self {
            id: [COMMON_MAGIC[0], COMMON_MAGIC[1], id_high[0], id_high[1]],
            revision: 0,
            response: UnsafeCell::new(core::ptr::null()),
            extra,
        }
    }

    /// The bootloader's response, or None if it did not answer.
    fn response(&self) -> Option<&'static R> {
        // SAFETY: Limine either leaves the pointer null or points it at a valid response
        // in bootloader-reclaimable memory, which Nova never reclaims.
        unsafe { self.response.get().read_volatile().as_ref() }
    }
}

/// SAFETY: `ptr` must be a NUL-terminated string that lives forever (Limine-provided).
unsafe fn static_str(ptr: *const c_char) -> Option<&'static str> {
    unsafe { CStr::from_ptr(ptr) }.to_str().ok()
}

#[repr(C)]
pub struct BootloaderInfoResponse {
    revision: u64,
    name: *const c_char,
    version: *const c_char,
}

impl Request<BootloaderInfoResponse> {
    /// Returns (name, version) if Limine answered the request.
    pub fn get(&self) -> Option<(&'static str, &'static str)> {
        let resp = self.response()?;
        // SAFETY: Limine's strings are NUL-terminated and never reclaimed.
        unsafe { Some((static_str(resp.name)?, static_str(resp.version)?)) }
    }
}

#[repr(C)]
pub struct ExecutableCmdlineResponse {
    revision: u64,
    cmdline: *const c_char,
}

impl Request<ExecutableCmdlineResponse> {
    /// The `cmdline:` value from limine.conf ("" if none), or None if Limine did not answer.
    pub fn get(&self) -> Option<&'static str> {
        // SAFETY: as for BootloaderInfo.
        unsafe { static_str(self.response()?.cmdline) }
    }
}

#[repr(C)]
pub struct FramebufferResponse {
    revision: u64,
    count: u64,
    framebuffers: *const *const Framebuffer,
}

/// One framebuffer, as described by Limine. `address` is a virtual (HHDM) address.
#[repr(C)]
pub struct Framebuffer {
    pub address: *mut u8,
    pub width: u64,
    pub height: u64,
    pub pitch: u64,
    pub bpp: u16,
    pub memory_model: u8,
    pub red_mask_size: u8,
    pub red_mask_shift: u8,
    pub green_mask_size: u8,
    pub green_mask_shift: u8,
    pub blue_mask_size: u8,
    pub blue_mask_shift: u8,
}

/// `memory_model` value for direct RGB pixels.
pub const FRAMEBUFFER_RGB: u8 = 1;

impl Request<FramebufferResponse> {
    /// The first framebuffer, if there is one.
    pub fn first(&self) -> Option<&'static Framebuffer> {
        let resp = self.response()?;
        if resp.count == 0 {
            return None;
        }
        // SAFETY: Limine provides `count` valid pointers in reclaimable memory.
        unsafe { (*resp.framebuffers).as_ref() }
    }
}

#[repr(C)]
pub struct MemmapResponse {
    revision: u64,
    count: u64,
    entries: *const *const MemmapEntry,
}

#[repr(C)]
pub struct MemmapEntry {
    pub base: u64,
    pub length: u64,
    pub kind: u64,
}

/// Names of the memory map entry types, indexed by `MemmapEntry::kind`.
pub const MEMMAP_KINDS: [&str; 9] = [
    "usable",
    "reserved",
    "ACPI reclaimable",
    "ACPI NVS",
    "bad memory",
    "bootloader reclaimable",
    "kernel and modules",
    "framebuffer",
    "reserved (mapped)",
];
pub const MEMMAP_USABLE: u64 = 0;

impl Request<MemmapResponse> {
    pub fn entries(&self) -> impl Iterator<Item = &'static MemmapEntry> {
        let (ptr, count) = self.response().map_or((core::ptr::null(), 0), |r| (r.entries, r.count as usize));
        // SAFETY: Limine provides `count` valid entry pointers in reclaimable memory.
        (0..count).filter_map(move |i| unsafe { (*ptr.add(i)).as_ref() })
    }
}

#[used]
#[unsafe(link_section = ".limine_requests_start")]
static REQUESTS_START: [u64; 4] = [0xf6b8f4b39de7d1ae, 0xfab91a6940fcb9cf, 0x785c6ed015d3e316, 0x181e920a7852b9d9];

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static BASE_REVISION: BaseRevision = BaseRevision::new(REQUESTED_REVISION);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static BOOTLOADER_INFO: Request<BootloaderInfoResponse> = Request::new([0xf55038d8e2a1202f, 0x279426fcf5f59740]);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static EXECUTABLE_CMDLINE: Request<ExecutableCmdlineResponse> = Request::new([0x4b161536e598651e, 0xb390ad4a2f1f303a]);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static FRAMEBUFFER: Request<FramebufferResponse> = Request::new([0x9d5827dcd881dd75, 0xa3148604f6fab11b]);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static MEMMAP: Request<MemmapResponse> = Request::new([0x67cf3d9d378a806f, 0xe304acdfc50c3c62]);

#[repr(C)]
pub struct HhdmResponse {
    revision: u64,
    offset: u64,
}

impl Request<HhdmResponse> {
    /// Virtual address of physical address 0 in the higher-half direct map.
    pub fn offset(&self) -> Option<u64> {
        self.response().map(|r| r.offset)
    }
}

#[repr(C)]
pub struct StackSizeResponse {
    revision: u64,
}

impl Request<StackSizeResponse, u64> {
    pub fn granted(&self) -> bool {
        self.response().is_some()
    }
}

#[repr(C)]
pub struct File {
    revision: u64,
    pub address: *const u8,
    pub size: u64,
    path: *const c_char,
    string: *const c_char,
    pub media_type: u32,
    unused: u32,
    tftp_ip: u32,
    tftp_port: u32,
    pub partition_index: u32,
    pub mbr_disk_id: u32,
    /// GUIDs in the same byte layout as on a GPT disk.
    pub gpt_disk_uuid: [u8; 16],
    pub gpt_part_uuid: [u8; 16],
    pub part_uuid: [u8; 16],
}

#[repr(C)]
pub struct ExecutableFileResponse {
    revision: u64,
    file: *const File,
}

impl Request<ExecutableFileResponse> {
    /// The kernel file as loaded by Limine, including where it came from.
    pub fn file(&self) -> Option<&'static File> {
        // SAFETY: Limine points `file` at a valid struct in reclaimable memory.
        unsafe { self.response()?.file.as_ref() }
    }
}

impl File {
    /// The `module_string` from limine.conf ("" if none).
    pub fn string(&self) -> &'static str {
        // SAFETY: Limine strings are NUL-terminated and never reclaimed.
        unsafe { static_str(self.string) }.unwrap_or("")
    }

    pub fn data(&self) -> &'static [u8] {
        // SAFETY: Limine loaded `size` bytes at `address` (executable-and-modules memory).
        unsafe { core::slice::from_raw_parts(self.address, self.size as usize) }
    }
}

#[repr(C)]
pub struct ModuleResponse {
    revision: u64,
    count: u64,
    modules: *const *const File,
}

impl Request<ModuleResponse> {
    pub fn modules(&self) -> impl Iterator<Item = &'static File> {
        let (ptr, count) = self.response().map_or((core::ptr::null(), 0), |r| (r.modules, r.count as usize));
        // SAFETY: Limine provides `count` valid file pointers.
        (0..count).filter_map(move |i| unsafe { (*ptr.add(i)).as_ref() })
    }
}

/// Files loaded with `module_path:` in limine.conf (Nova's bundled programs).
#[used]
#[unsafe(link_section = ".limine_requests")]
pub static MODULES: Request<ModuleResponse> = Request::new([0x3e7e279702be32af, 0xca1c4f3bd1280cee]);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static EXECUTABLE_FILE: Request<ExecutableFileResponse> = Request::new([0xad97e90e83f1ed67, 0x31eb5d1c5ff23b69]);

/// Stack for kmain. Limine's default (64 KiB) is too small for TLS and the script engine.
pub const KERNEL_STACK_SIZE: u64 = 1024 * 1024;

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static HHDM: Request<HhdmResponse> = Request::new([0x48dcf1cb8ad2b852, 0x63984e959a98244b]);

#[used]
#[unsafe(link_section = ".limine_requests")]
pub static STACK_SIZE: Request<StackSizeResponse, u64> =
    Request::with_extra([0x224ef0460a8e8926, 0xe1cb0fc25f46ea3d], KERNEL_STACK_SIZE);

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static REQUESTS_END: [u64; 2] = [0xadc0e0531bb10d03, 0x9572709f31764c62];
