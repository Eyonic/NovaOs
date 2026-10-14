//! Physical memory, the kernel heap, and MMIO mappings.
//!
//! * Limine maps all usable RAM at `HHDM offset + physical address` (the higher-half
//!   direct map). Nova keeps Limine's page tables and adds to them.
//! * The heap is every usable RAM region (on QEMU with more than ~2 GiB that is RAM below
//!   the PCI hole plus RAM above 4 GiB), accessed through the direct map. Each region is
//!   physically contiguous and an allocation never spans two, so `virt_to_phys` of any
//!   heap address is a plain subtraction and allocations can be handed to DMA-capable
//!   devices (Nova's drivers all take 64-bit addresses).
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
static HEAP_REGIONS: AtomicUsize = AtomicUsize::new(0);

/// Number of RAM regions the heap spans.
pub fn heap_regions() -> usize {
    HEAP_REGIONS.load(Ordering::Relaxed)
}

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

/// (used, free) heap bytes; free pages in the page pool count as free.
pub fn heap_usage() -> (usize, usize) {
    let (allocated, available) = {
        let talc = ALLOCATOR.lock();
        let counters = talc.get_counters();
        (counters.allocated_bytes, counters.available_bytes)
    };
    let pooled = POOL_FREE_BYTES.load(Ordering::Relaxed);
    (allocated.saturating_sub(pooled), available + pooled)
}

/// Set up the direct map offset and the heap. Must run before anything allocates.
pub fn init() -> Result<(u64, usize), &'static str> {
    let offset = limine::HHDM.offset().ok_or("no HHDM response from bootloader")?;
    HHDM_OFFSET.store(offset, Ordering::Relaxed);
    KERNEL_CR3.store(read_cr3(), Ordering::Relaxed);

    let largest = limine::MEMMAP
        .entries()
        .filter(|e| e.kind == limine::MEMMAP_USABLE)
        .max_by_key(|e| e.length)
        .ok_or("no usable memory")?;
    let mut total = 0usize;
    // Regions under 1 MiB are not worth the allocator's bookkeeping.
    for region in limine::MEMMAP.entries().filter(|e| e.kind == limine::MEMMAP_USABLE && e.length >= 1 << 20) {
        #[cfg_attr(not(feature = "linux-drivers"), allow(unused_mut))]
        let (mut base, mut length) = (region.base, region.length);
        #[cfg(feature = "linux-drivers")]
        if DMA32_SIZE.load(Ordering::Relaxed) == 0 && base + length <= 1 << 32 && length >= 4 * DMA32_POOL {
            // SAFETY: as for the heap below; this slice of the region goes to the pool only.
            if unsafe { DMA32.lock().claim(Span::from_base_size((offset + base) as *mut u8, DMA32_POOL as usize)) }.is_ok() {
                DMA32_SIZE.store(DMA32_POOL as usize, Ordering::Relaxed);
                base += DMA32_POOL;
                length -= DMA32_POOL;
            }
        }
        let start = (offset + base) as *mut u8;
        // SAFETY: the region is usable RAM per the memory map, mapped by the direct map,
        // and nothing else uses it; each region is claimed exactly once.
        let claimed = unsafe { ALLOCATOR.lock().claim(Span::from_base_size(start, length as usize)) };
        if claimed.is_ok() {
            total += length as usize;
            HEAP_REGIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
    if total == 0 {
        return Err("heap claim failed");
    }
    HEAP_SIZE.store(total, Ordering::Relaxed);
    let region = largest;
    // Create the MMIO window's top-level entry now, so address spaces that copy the
    // kernel's upper half (processes) see MMIO mappings made later.
    let entry = (phys_to_virt(read_cr3()) as *mut u64).wrapping_add(((MMIO_BASE >> 39) & 511) as usize);
    // SAFETY: Limine's PML4 lives in direct-mapped RAM; slot 448 is unused by Limine.
    unsafe {
        if entry.read_volatile() & PRESENT == 0 {
            entry.write_volatile(new_table() | PRESENT | WRITABLE);
        }
    }
    // The same for Linux's struct page array (filled on demand by `populate`).
    #[cfg(feature = "linux-drivers")]
    {
        let entry = (phys_to_virt(read_cr3()) as *mut u64).wrapping_add(((VMEMMAP_BASE >> 39) & 511) as usize);
        // SAFETY: as above; slot 468 is unused by Limine.
        unsafe {
            if entry.read_volatile() & PRESENT == 0 {
                entry.write_volatile(new_table() | PRESENT | WRITABLE);
            }
        }
    }
    Ok((region.base, region.length as usize))
}

/// Memory below 4 GiB for devices that can only address 32 bits (Linux's default DMA mask),
/// carved out of the first suitable RAM region. Only the Linux driver layer uses it.
#[cfg(feature = "linux-drivers")]
static DMA32: Talck<spin::Mutex<()>, talc::ErrOnOom> = Talc::new(talc::ErrOnOom).lock();
#[cfg(feature = "linux-drivers")]
const DMA32_POOL: u64 = 16 << 20;
#[cfg(feature = "linux-drivers")]
static DMA32_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Allocate from the below-4-GiB pool (null when it is exhausted or absent).
#[cfg(feature = "linux-drivers")]
pub fn dma32_alloc(layout: Layout, zero: bool) -> *mut u8 {
    use core::alloc::GlobalAlloc;
    // SAFETY: non-zero size (callers never ask for 0 bytes).
    unsafe { if zero { DMA32.alloc_zeroed(layout) } else { DMA32.alloc(layout) } }
}

/// Free memory from [`dma32_alloc`].
///
/// # Safety
/// `ptr` must come from `dma32_alloc` with the same `layout`.
#[cfg(feature = "linux-drivers")]
pub unsafe fn dma32_free(ptr: *mut u8, layout: Layout) {
    use core::alloc::GlobalAlloc;
    // SAFETY: per the caller's contract.
    unsafe { DMA32.dealloc(ptr, layout) }
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

// ---------------------------------------------------------------------------
// Page pool: 4 KiB pages for program memory, shared memory and page tables.
//
// The heap allocator keeps a small header next to each allocation, so page-aligned
// single pages from it waste nearly a page each (programs could use only about half of
// RAM). The pool takes 2 MiB blocks from the heap and hands out their pages; freed pages
// go on an intrusive free list (the first 8 bytes of a free page point to the next).

const POOL_BLOCK: usize = 2 << 20;
/// Head of the free list (a kernel virtual address, 0 = empty).
static POOL_FREE: SpinLock<usize> = SpinLock::new(0);
/// Bytes on the free list (counted as free in `heap_usage`).
static POOL_FREE_BYTES: AtomicUsize = AtomicUsize::new(0);

/// One zeroed 4 KiB page (kernel virtual address), or None when memory is exhausted.
pub fn alloc_page() -> Option<*mut u8> {
    let page = {
        let mut head = POOL_FREE.lock();
        if *head == 0 {
            refill(&mut head);
        }
        let p = *head;
        if p != 0 {
            // SAFETY: pages on the free list are ours; the first word links to the next.
            *head = unsafe { (p as *const usize).read() };
            POOL_FREE_BYTES.fetch_sub(PAGE_SIZE as usize, Ordering::Relaxed);
        }
        p
    };
    if page == 0 {
        return None;
    }
    // SAFETY: the page is exclusively ours now.
    unsafe { core::ptr::write_bytes(page as *mut u8, 0, PAGE_SIZE as usize) };
    Some(page as *mut u8)
}

/// Give back a page from [`alloc_page`].
///
/// # Safety
/// `page` came from `alloc_page`, is not freed twice, and nothing maps or uses it any more.
pub unsafe fn free_page(page: *mut u8) {
    let mut head = POOL_FREE.lock();
    // SAFETY: per the contract the page is ours to reuse as a list node.
    unsafe { (page as *mut usize).write(*head) };
    *head = page as usize;
    POOL_FREE_BYTES.fetch_add(PAGE_SIZE as usize, Ordering::Relaxed);
}

/// Move a block of pages from the heap to the (empty) free list: 2 MiB, or smaller
/// blocks when the heap has no 2 MiB run left.
fn refill(head: &mut usize) {
    let mut size = POOL_BLOCK;
    while size >= PAGE_SIZE as usize {
        let layout = Layout::from_size_align(size, PAGE_SIZE as usize).unwrap();
        // SAFETY: non-zero size, valid alignment. Pool blocks are never returned to the heap.
        let block = unsafe { alloc::alloc::alloc(layout) };
        if !block.is_null() {
            for i in (0..size).step_by(PAGE_SIZE as usize).rev() {
                let page = block as usize + i;
                // SAFETY: inside the block we just allocated.
                unsafe { (page as *mut usize).write(*head) };
                *head = page;
            }
            POOL_FREE_BYTES.fetch_add(size, Ordering::Relaxed);
            return;
        }
        size /= 8;
    }
}

/// One zeroed page from the page pool, returned as a physical address.
pub fn new_table() -> u64 {
    let page = alloc_page().unwrap_or(core::ptr::null_mut());
    if page.is_null() {
        let (used, free) = heap_usage();
        panic!("out of memory for page tables ({} MiB used, {} MiB free)", used >> 20, free >> 20);
    }
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

/// Where the Linux driver layer keeps its `struct page` array (Linux's default
/// VMEMMAP_START, PML4 slot 468); lx_emul sets `vmemmap_base` to it.
#[cfg(feature = "linux-drivers")]
pub const VMEMMAP_BASE: u64 = 0xFFFF_EA00_0000_0000;

/// Physical address `virt` maps to in the kernel's page tables, if any.
pub fn translate(virt: u64) -> Option<u64> {
    let mut table = read_cr3();
    for level in [39u64, 30, 21, 12] {
        let entry = (phys_to_virt(table) as *const u64).wrapping_add(((virt >> level) & 511) as usize);
        // SAFETY: page tables are in RAM covered by the direct map.
        let value = unsafe { entry.read_volatile() };
        if value & PRESENT == 0 {
            return None;
        }
        if level == 12 || (level != 39 && value & HUGE != 0) {
            let page = 1u64 << level;
            return Some((value & ADDR_MASK & !(page - 1)) + (virt & (page - 1)));
        }
        table = value & ADDR_MASK;
    }
    None
}

/// Back every unmapped page of [virt, virt + len) in the kernel's address space with a
/// zeroed RAM page. False when memory ran out.
pub fn populate(virt: u64, len: u64) -> bool {
    let first = virt & !(PAGE_SIZE - 1);
    let mut page = first;
    while page < virt + len {
        if translate(page).is_none() {
            let Some(frame) = alloc_page() else { return false };
            // SAFETY: an unmapped kernel address, backed by a fresh zeroed page.
            unsafe { map_page(page, virt_to_phys(frame as u64), PRESENT | WRITABLE) };
        }
        page += PAGE_SIZE;
    }
    true
}

/// Map `len` bytes of device memory at physical `phys` uncached; returns the virtual
/// address of `phys`. Mappings are never removed.
pub fn map_mmio(phys: u64, len: u64) -> u64 {
    map_window(phys, len, PRESENT | WRITABLE | WRITE_THROUGH | CACHE_DISABLE)
}

/// Map `len` bytes of physical RAM outside the direct map (ACPI tables, firmware data)
/// with normal caching; returns the virtual address of `phys`. Never removed.
pub fn map_ram(phys: u64, len: u64) -> u64 {
    map_window(phys, len, PRESENT | WRITABLE)
}

fn map_window(phys: u64, len: u64, flags: u64) -> u64 {
    let first = phys & !(PAGE_SIZE - 1);
    let pages = (phys + len.max(1) - first).div_ceil(PAGE_SIZE);
    let virt = {
        let mut next = MMIO_NEXT.try_lock().expect("map_mmio re-entered");
        let virt = *next;
        *next += (pages + 1) * PAGE_SIZE; // one unmapped guard page after each mapping
        virt
    };
    for i in 0..pages {
        // SAFETY: fresh addresses in the MMIO window, device memory or firmware RAM.
        unsafe { map_page(virt + i * PAGE_SIZE, first + i * PAGE_SIZE, flags) };
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
