//! Physical memory, the kernel heap, and MMIO mappings.
//!
//! * Limine maps all usable RAM at `HHDM offset + physical address` (the higher-half
//!   direct map). Nova keeps Limine's page tables and adds to them.
//! * The heap is the largest usable RAM region, accessed through the direct map. It is
//!   physically contiguous, so `virt_to_phys` of any heap address is a plain
//!   subtraction and heap allocations can be handed to DMA-capable devices.
//! * Device registers (PCI BARs, ...) are not in the direct map under Limine base
//!   revision 3, so [`map_mmio`] maps them uncached into a separate window.

use alloc::alloc::{alloc_zeroed, Layout};
use core::arch::asm;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use talc::{ClaimOnOom, Span, Talc, Talck};

use crate::limine;
use crate::sync::SpinLock;

pub const PAGE_SIZE: u64 = 4096;

#[global_allocator]
static ALLOCATOR: Talck<spin::Mutex<()>, ClaimOnOom> = Talc::new(unsafe { ClaimOnOom::new(Span::empty()) }).lock();

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);
static HEAP_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Start of the MMIO window: PML4 slot 448, far above the direct map.
const MMIO_BASE: u64 = 0xFFFF_E000_0000_0000;
static MMIO_NEXT: SpinLock<u64> = SpinLock::new(MMIO_BASE);

pub fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Relaxed)
}

pub fn phys_to_virt(phys: u64) -> u64 {
    phys + hhdm_offset()
}

/// Physical address of a direct-map (e.g. heap) virtual address.
pub fn virt_to_phys(virt: u64) -> u64 {
    debug_assert!(virt >= hhdm_offset() && virt < MMIO_BASE, "not a direct-map address: {virt:#x}");
    virt - hhdm_offset()
}

pub fn heap_size() -> usize {
    HEAP_SIZE.load(Ordering::Relaxed)
}

/// (used, free) heap bytes.
pub fn heap_usage() -> (usize, usize) {
    let talc = ALLOCATOR.lock();
    let counters = talc.get_counters();
    (counters.allocated_bytes, counters.available_bytes)
}

/// Set up the direct map offset and the heap. Must run before anything allocates.
pub fn init() -> Result<(u64, usize), &'static str> {
    let offset = limine::HHDM.offset().ok_or("no HHDM response from bootloader")?;
    HHDM_OFFSET.store(offset, Ordering::Relaxed);
    KERNEL_CR3.store(read_cr3(), Ordering::Relaxed);

    let region = limine::MEMMAP
        .entries()
        .filter(|e| e.kind == limine::MEMMAP_USABLE)
        .max_by_key(|e| e.length)
        .ok_or("no usable memory")?;
    let start = (offset + region.base) as *mut u8;
    // SAFETY: the region is usable RAM per the memory map, mapped by the direct map,
    // and nothing else uses it; it is claimed exactly once.
    unsafe {
        ALLOCATOR.lock().claim(Span::from_base_size(start, region.length as usize)).map_err(|_| "heap claim failed")?;
    }
    HEAP_SIZE.store(region.length as usize, Ordering::Relaxed);
    // Create the MMIO window's top-level entry now, so address spaces that copy the
    // kernel's upper half (processes) see MMIO mappings made later.
    let entry = (phys_to_virt(read_cr3()) as *mut u64).wrapping_add(((MMIO_BASE >> 39) & 511) as usize);
    // SAFETY: Limine's PML4 lives in direct-mapped RAM; slot 448 is unused by Limine.
    unsafe {
        if entry.read_volatile() & PRESENT == 0 {
            entry.write_volatile(new_table() | PRESENT | WRITABLE);
        }
    }
    Ok((region.base, region.length as usize))
}

static KERNEL_CR3: AtomicU64 = AtomicU64::new(0);

/// The kernel's (Limine's) page-table root. Process address spaces share its upper half.
pub fn kernel_cr3() -> u64 {
    KERNEL_CR3.load(Ordering::Relaxed)
}

pub fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: reading CR3 has no side effects.
    unsafe { asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value & 0x000F_FFFF_FFFF_F000
}

pub const PRESENT: u64 = 1 << 0;
pub const WRITABLE: u64 = 1 << 1;
pub const USER: u64 = 1 << 2;
const WRITE_THROUGH: u64 = 1 << 3;
const CACHE_DISABLE: u64 = 1 << 4;
const HUGE: u64 = 1 << 7;
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// One zeroed, page-aligned page from the heap, returned as a physical address.
pub fn new_table() -> u64 {
    // SAFETY: non-zero size, valid alignment.
    let page = unsafe { alloc_zeroed(Layout::from_size_align(4096, 4096).unwrap()) };
    assert!(!page.is_null(), "out of memory for page tables");
    virt_to_phys(page as u64)
}

/// Map one 4 KiB page `virt` -> `phys` with `flags` in the current address space.
///
/// SAFETY: `virt` must be unused and inside the MMIO window; `phys` must not be RAM
/// in use by something else.
unsafe fn map_page(virt: u64, phys: u64, flags: u64) {
    unsafe { map_page_in(read_cr3(), virt, phys, flags, PRESENT | WRITABLE, &mut |_| {}) }
}

/// Map `virt` -> `phys` in the page tables rooted at `root` (physical). Missing
/// intermediate tables are created with `table_flags`; `new_table_hook` is told
/// about each one (so process address spaces can free them later).
///
/// SAFETY: as for `map_page`, for the address space `root`.
pub unsafe fn map_page_in(root: u64, virt: u64, phys: u64, flags: u64, table_flags: u64, new_table_hook: &mut dyn FnMut(u64)) {
    let mut table = root;
    for level in [39, 30, 21] {
        let index = (virt >> level) & 511;
        let entry = (phys_to_virt(table) as *mut u64).wrapping_add(index as usize);
        // SAFETY: page tables are in RAM covered by the direct map.
        let value = unsafe { entry.read_volatile() };
        assert!(value & HUGE == 0, "mapping overlaps a huge page at {virt:#x}");
        table = if value & PRESENT == 0 {
            let new = new_table();
            new_table_hook(new);
            unsafe { entry.write_volatile(new | table_flags) };
            new
        } else {
            if table_flags & USER != 0 && value & USER == 0 {
                unsafe { entry.write_volatile(value | USER) };
            }
            value & ADDR_MASK
        };
    }
    let entry = (phys_to_virt(table) as *mut u64).wrapping_add(((virt >> 12) & 511) as usize);
    unsafe {
        entry.write_volatile(phys | flags);
        asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags));
    }
}

/// Map `len` bytes of device memory at physical `phys` uncached; returns the virtual
/// address of `phys`. Mappings are never removed.
pub fn map_mmio(phys: u64, len: u64) -> u64 {
    let first = phys & !(PAGE_SIZE - 1);
    let pages = (phys + len - first).div_ceil(PAGE_SIZE);
    let virt = {
        let mut next = MMIO_NEXT.try_lock().expect("map_mmio re-entered");
        let virt = *next;
        *next += (pages + 1) * PAGE_SIZE; // one unmapped guard page after each mapping
        virt
    };
    for i in 0..pages {
        // SAFETY: fresh addresses in the MMIO window, device memory.
        unsafe { map_page(virt + i * PAGE_SIZE, first + i * PAGE_SIZE, PRESENT | WRITABLE | WRITE_THROUGH | CACHE_DISABLE) };
    }
    virt + (phys - first)
}

/// A physically contiguous, zeroed buffer for device DMA, with its physical address.
pub struct DmaBuffer {
    ptr: *mut u8,
    layout: Layout,
}

// SAFETY: the buffer is plain memory owned by this value.
unsafe impl Send for DmaBuffer {}

impl DmaBuffer {
    pub fn new(size: usize, align: usize) -> Self {
        let layout = Layout::from_size_align(size, align).expect("bad DMA layout");
        // SAFETY: non-zero size (callers never ask for 0 bytes).
        let ptr = unsafe { alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory for DMA buffer");
        Self { ptr, layout }
    }

    pub fn phys(&self) -> u64 {
        virt_to_phys(self.ptr as u64)
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    pub fn len(&self) -> usize {
        self.layout.size()
    }

    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: owned allocation of `len` initialised bytes.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len()) }
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, exclusively borrowed.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len()) }
    }
}

impl Drop for DmaBuffer {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { alloc::alloc::dealloc(self.ptr, self.layout) };
    }
}
