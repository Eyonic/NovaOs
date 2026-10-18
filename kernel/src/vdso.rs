//! The vDSO of 64-bit programs (vdso/vdso.c, built by build.rs): clock_gettime,
//! gettimeofday and time in user mode, without a system call or the kernel lock. Each
//! process maps the same pages read-only: one data page (`nova_vvar`) with the clock's
//! parameters, then the shared object, announced to the program by AT_SYSINFO_EHDR.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::process::Memory;
use crate::{pit, rtc};

static IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso.so"));
const PAGE: usize = 4096;

/// The data page followed by the image (page aligned, direct-mapped heap memory), set once.
static PAGES: AtomicUsize = AtomicUsize::new(0);

fn len() -> usize {
    PAGE + IMAGE.len().next_multiple_of(PAGE)
}

/// The pages, created and filled the first time a program needs them (the clock is
/// calibrated by then).
fn pages() -> Option<&'static [u8]> {
    let mut p = PAGES.load(Ordering::Acquire);
    if p == 0 {
        let (tsc_base, base_ns, mult) = pit::clock_params()?;
        let layout = core::alloc::Layout::from_size_align(len(), PAGE).ok()?;
        // SAFETY: a non-zero size; the memory is never freed (every process maps it).
        let mem = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if mem.is_null() {
            return None;
        }
        // struct vvar in vdso.c: tsc_base, base_ns, mult, real_offset, ready.
        let vvar = [tsc_base, base_ns, mult, rtc::realtime_offset(), 1];
        // SAFETY: `mem` has len() bytes: the data page, then room for the image.
        unsafe {
            core::ptr::copy_nonoverlapping(vvar.as_ptr() as *const u8, mem, vvar.len() * 8);
            core::ptr::copy_nonoverlapping(IMAGE.as_ptr(), mem.add(PAGE), IMAGE.len());
        }
        p = mem as usize;
        PAGES.store(p, Ordering::Release);
    }
    // SAFETY: set above to a leaked allocation of len() bytes that is never written again.
    Some(unsafe { core::slice::from_raw_parts(p as *const u8, len()) })
}

/// Map the vDSO into a 64-bit program's address space; returns the image's address for
/// AT_SYSINFO_EHDR (0: none, the C library then makes system calls).
pub fn map(mem: &mut Memory) -> u64 {
    let Some(pages) = pages() else { return 0 };
    let Some(start) = mem.reserve_with(pages.len() as u64, false) else { return 0 };
    mem.map_file(start, pages, 0, pages.len() as u64);
    start + PAGE as u64
}
