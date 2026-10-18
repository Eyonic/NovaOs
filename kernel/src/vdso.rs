//! The vDSOs (vdso/vdso.c for 64-bit programs, vdso/vdso32.c for 32-bit ones, built by
//! build.rs): clock_gettime, gettimeofday and time in user mode, without a system call or
//! the kernel lock. Each process maps the same pages read-only: one data page
//! (`nova_vvar`) with the clock's parameters, then the shared object, announced to the
//! program by AT_SYSINFO_EHDR.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::process::Memory;
use crate::{pit, rtc};

static IMAGE64: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso.so"));
static IMAGE32: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/vdso32.so"));
const PAGE: usize = 4096;

/// For each image, the data page followed by the image (page aligned, direct-mapped heap
/// memory), set once.
static PAGES64: AtomicUsize = AtomicUsize::new(0);
static PAGES32: AtomicUsize = AtomicUsize::new(0);

/// The pages of `image`, created and filled the first time a program needs them (the
/// clock is calibrated by then).
fn pages(image: &'static [u8], cell: &AtomicUsize) -> Option<&'static [u8]> {
    let len = PAGE + image.len().next_multiple_of(PAGE);
    let mut p = cell.load(Ordering::Acquire);
    if p == 0 {
        let (tsc_base, base_ns, mult) = pit::clock_params()?;
        let layout = core::alloc::Layout::from_size_align(len, PAGE).ok()?;
        // SAFETY: a non-zero size; the memory is never freed (every process maps it).
        let mem = unsafe { alloc::alloc::alloc_zeroed(layout) };
        if mem.is_null() {
            return None;
        }
        // struct vvar in vdso.c: tsc_base, base_ns, mult, real_offset, ready.
        let vvar = [tsc_base, base_ns, mult, rtc::realtime_offset(), 1];
        // SAFETY: `mem` has `len` bytes: the data page, then room for the image.
        unsafe {
            core::ptr::copy_nonoverlapping(vvar.as_ptr() as *const u8, mem, vvar.len() * 8);
            core::ptr::copy_nonoverlapping(image.as_ptr(), mem.add(PAGE), image.len());
        }
        p = mem as usize;
        cell.store(p, Ordering::Release);
    }
    // SAFETY: set above to a leaked allocation of `len` bytes that is never written again.
    Some(unsafe { core::slice::from_raw_parts(p as *const u8, len) })
}

/// Where __kernel_vsyscall is in the 32-bit image (AT_SYSINFO = image address + this).
pub const VSYSCALL32: u64 = const_parse(env!("NOVA_VSYSCALL32"));

const fn const_parse(s: &str) -> u64 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut v = 0;
    while i < b.len() {
        v = v * 10 + (b[i] - b'0') as u64;
        i += 1;
    }
    v
}

/// Map the vDSO into a program's address space (the 32-bit one for a 32-bit program);
/// returns the image's address for AT_SYSINFO_EHDR (0: none, the C library then makes
/// system calls).
pub fn map(mem: &mut Memory, compat: bool) -> u64 {
    let found = if compat { pages(IMAGE32, &PAGES32) } else { pages(IMAGE64, &PAGES64) };
    let Some(pages) = found else { return 0 };
    let Some(start) = mem.reserve_with(pages.len() as u64, false) else { return 0 };
    mem.map_file(start, pages, 0, pages.len() as u64);
    start + PAGE as u64
}
