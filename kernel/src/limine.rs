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

#[repr(C)]
struct BootloaderInfoResponse {
    revision: u64,
    name: *const c_char,
    version: *const c_char,
}

#[repr(C, align(8))]
pub struct BootloaderInfoRequest {
    id: [u64; 4],
    revision: u64,
    response: UnsafeCell<*const BootloaderInfoResponse>,
}

// SAFETY: as for BaseRevision.
unsafe impl Sync for BootloaderInfoRequest {}

impl BootloaderInfoRequest {
    const fn new() -> Self {
        Self {
            id: [COMMON_MAGIC[0], COMMON_MAGIC[1], 0xf55038d8e2a1202f, 0x279426fcf5f59740],
            revision: 0,
            response: UnsafeCell::new(core::ptr::null()),
        }
    }

    /// Returns (name, version) if Limine answered the request.
    pub fn get(&self) -> Option<(&'static str, &'static str)> {
        // SAFETY: Limine either leaves the pointer null or points it at a valid response
        // with NUL-terminated strings in bootloader-reclaimable memory, which we never reclaim.
        unsafe {
            let resp = self.response.get().read_volatile();
            let resp = resp.as_ref()?;
            let name = CStr::from_ptr(resp.name).to_str().ok()?;
            let version = CStr::from_ptr(resp.version).to_str().ok()?;
            Some((name, version))
        }
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
pub static BOOTLOADER_INFO: BootloaderInfoRequest = BootloaderInfoRequest::new();

#[used]
#[unsafe(link_section = ".limine_requests_end")]
static REQUESTS_END: [u64; 2] = [0xadc0e0531bb10d03, 0x9572709f31764c62];
