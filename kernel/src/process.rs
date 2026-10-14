//! User processes: the compatibility environment for native C/C++ programs.
//!
//! Nova runs statically linked x86-64 Linux ELF executables (glibc or musl, C or
//! C++ with libstdc++) unmodified, in ring 3, each in its own address space. They
//! talk to Nova through the Linux system call ABI (`syscall` instruction, Linux
//! numbers and structures), implemented in linux.rs.
//!
//! * Address space: a fresh PML4 whose upper half is the kernel's (shared kernel
//!   mappings), lower half private. ELF segments are loaded eagerly; the stack,
//!   heap (`brk`) and anonymous `mmap` regions are reserved as areas and filled with
//!   zero pages on first touch (page faults).
//! * A process runs on the kernel task that started it: `run` enters user mode and
//!   returns when the program exits. Timer interrupts preempt user code, so other
//!   tasks (the desktop, other programs) keep running.
//! * CPU state that kernel code does not touch but programs do (SSE registers, the
//!   FS base used for thread-local storage) is saved and restored on task switches.
//!   The kernel is built soft-float, so it never uses SSE itself.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::{asm, naked_asm};
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

use crate::linux::FdTable;
use crate::memory::{self, PRESENT, USER, WRITABLE};
use crate::sync::SpinLock;
use crate::{gdt, task};

pub const PAGE: u64 = 4096;
/// Highest user stack address (exclusive).
pub const STACK_TOP: u64 = 0x0000_7FFF_FFFF_F000;
const STACK_SIZE: u64 = 8 << 20;
/// Anonymous mmap areas are placed growing down from here.
const MMAP_TOP: u64 = 0x0000_7F00_0000_0000;
/// Lowest address a program may use.
const USER_MIN: u64 = 0x1_0000;
pub const USER_MAX: u64 = 0x0000_8000_0000_0000;

const NX: u64 = 1 << 63;

/// SYSCALL entry stack for the running user task, and scratch for the user RSP.
static SYSCALL_KERNEL_RSP: AtomicU64 = AtomicU64::new(0);
static SYSCALL_USER_RSP: AtomicU64 = AtomicU64::new(0);

const IA32_EFER: u32 = 0xC000_0080;
const IA32_STAR: u32 = 0xC000_0081;
const IA32_LSTAR: u32 = 0xC000_0082;
const IA32_FMASK: u32 = 0xC000_0084;
const IA32_FS_BASE: u32 = 0xC000_0100;

pub fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: only architectural MSRs Nova knows exist are read.
    unsafe { asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

pub fn wrmsr(msr: u32, value: u64) {
    // SAFETY: only architectural MSRs with valid values are written.
    unsafe { asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nomem, nostack, preserves_flags)) };
}

/// Bytes of extended state saved per process (0 = FXSAVE only, 512 bytes).
static XSAVE_SIZE: AtomicU64 = AtomicU64::new(0);
/// Components enabled in XCR0 (x87, SSE, AVX, AVX-512 as available).
static XCR0: AtomicU64 = AtomicU64::new(0);

fn cpuid(leaf: u32, sub: u32) -> [u32; 4] {
    let (a, b, c, d): (u32, u32, u32, u32);
    // SAFETY: CPUID; RBX is reserved by LLVM, so it is saved by hand.
    unsafe {
        asm!("mov {tmp:r}, rbx", "cpuid", "xchg {tmp:r}, rbx", tmp = out(reg) b,
             inout("eax") leaf => a, inout("ecx") sub => c, out("edx") d, options(nostack, preserves_flags));
    }
    [a, b, c, d]
}

/// Which SIMD extensions user programs may use, for the boot log.
pub fn simd_summary() -> &'static str {
    let x = XCR0.load(Ordering::Relaxed);
    if x & 0xE0 == 0xE0 {
        "SSE, AVX, AVX-512"
    } else if x & 4 != 0 {
        "SSE, AVX"
    } else {
        "SSE"
    }
}

/// Enable SSE/AVX for user programs and the SYSCALL instruction. Once, at boot.
pub fn init() {
    // SAFETY: CR0/CR4/EFER bits that enable features; the kernel itself uses none of
    // the SSE state (soft-float target).
    unsafe {
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        cr0 &= !(1 << 2); // EM: no x87 emulation
        cr0 |= 1 << 1; // MP
        asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack, preserves_flags));
        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        cr4 |= (1 << 9) | (1 << 10); // OSFXSR, OSXMMEXCPT
        asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack, preserves_flags));
    }
    // XSAVE + AVX: many distributions build for x86-64-v3, so even static binaries
    // use AVX unconditionally. Enable every user-state component we can save.
    let ecx = cpuid(1, 0)[2];
    if ecx & (1 << 26) != 0 {
        // SAFETY: CR4.OSXSAVE enables XGETBV/XSETBV; XCR0 gets only supported bits.
        unsafe {
            let mut cr4: u64;
            asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
            asm!("mov cr4, {}", in(reg) cr4 | (1 << 18), options(nomem, nostack, preserves_flags));
            let supported = cpuid(0xD, 0)[0] as u64;
            let mut xcr0 = 0b11; // x87, SSE
            if supported & 4 != 0 && ecx & (1 << 28) != 0 {
                xcr0 |= 4; // AVX
                if supported & 0xE0 == 0xE0 {
                    xcr0 |= 0xE0; // AVX-512 opmask, ZMM_Hi256, Hi16_ZMM
                }
            }
            asm!("xsetbv", in("ecx") 0, in("eax") xcr0 as u32, in("edx") (xcr0 >> 32) as u32, options(nomem, nostack, preserves_flags));
            XCR0.store(xcr0, Ordering::Relaxed);
        }
        // Area size for the enabled components (EBX after XCR0 is set).
        XSAVE_SIZE.store(cpuid(0xD, 0)[1] as u64, Ordering::Relaxed);
    }
    wrmsr(IA32_EFER, rdmsr(IA32_EFER) | 1 | (1 << 11)); // SCE, NXE
    // SYSCALL: CS = 0x08, SS = 0x10. SYSRET: SS = 0x10 + 8, CS = 0x10 + 16 (both RPL 3).
    wrmsr(IA32_STAR, (0x10u64 << 48) | ((gdt::KERNEL_CODE as u64) << 32));
    wrmsr(IA32_LSTAR, syscall_entry as *const () as u64);
    wrmsr(IA32_FMASK, 0x0004_0700); // clear IF, DF, TF and AC on entry
}

/// Saved SIMD/FPU state: an XSAVE area (64-byte aligned), or 512 bytes for FXSAVE.
pub struct FxArea {
    ptr: *mut u8,
    layout: alloc::alloc::Layout,
}

impl FxArea {
    fn initial() -> Box<Self> {
        let size = (XSAVE_SIZE.load(Ordering::Relaxed) as usize).max(512 + 64);
        let layout = alloc::alloc::Layout::from_size_align(size, 64).unwrap();
        // SAFETY: non-zero size. A zeroed XSAVE header (XSTATE_BV = 0) means "initial
        // state" for every component.
        let ptr = unsafe { alloc::alloc::alloc_zeroed(layout) };
        assert!(!ptr.is_null(), "out of memory for FPU state");
        // SAFETY: inside the 512-byte legacy region.
        unsafe {
            ptr.cast::<u16>().write(0x037F); // FCW: all exceptions masked
            ptr.add(24).cast::<u32>().write(0x1F80); // MXCSR default
        }
        Box::new(Self { ptr, layout })
    }

    fn save(&mut self) {
        // SAFETY: the area is large enough and aligned for the enabled components.
        unsafe {
            if XSAVE_SIZE.load(Ordering::Relaxed) != 0 {
                asm!("xsave64 [{}]", in(reg) self.ptr, in("eax") u32::MAX, in("edx") u32::MAX, options(nostack, preserves_flags));
            } else {
                asm!("fxsave64 [{}]", in(reg) self.ptr, options(nostack, preserves_flags));
            }
        }
    }

    fn restore(&self) {
        // SAFETY: the area holds a state saved by `save` or the initial image.
        unsafe {
            if XSAVE_SIZE.load(Ordering::Relaxed) != 0 {
                asm!("xrstor64 [{}]", in(reg) self.ptr, in("eax") u32::MAX, in("edx") u32::MAX, options(nostack, preserves_flags));
            } else {
                asm!("fxrstor64 [{}]", in(reg) self.ptr, options(nostack, preserves_flags));
            }
        }
    }
}

impl Drop for FxArea {
    fn drop(&mut self) {
        // SAFETY: allocated in `initial` with this layout.
        unsafe { alloc::alloc::dealloc(self.ptr, self.layout) };
    }
}

// SAFETY: owned buffer.
unsafe impl Send for FxArea {}

/// A region of the address space that is filled on demand.
#[derive(Clone, Copy, Debug)]
pub struct Area {
    pub start: u64,
    pub end: u64,
    pub writable: bool,
}

/// The memory of a process: page tables, mapped pages, areas, heap.
pub struct Memory {
    cr3: u64,
    /// Mapped user pages: virtual page -> kernel pointer to the frame.
    pages: BTreeMap<u64, *mut u8>,
    /// Page-table pages to free at exit.
    tables: Vec<u64>,
    /// Pages mapped read-only from system-file memory (not ours to free; copied on write).
    borrowed: alloc::collections::BTreeSet<u64>,
    /// Pages of shared memory objects (not ours to free; writable, never copied).
    foreign: alloc::collections::BTreeSet<u64>,
    /// Shared memory objects mapped here (kept alive while mapped).
    shared: Vec<crate::ipc::SharedRef>,
    pub areas: Vec<Area>,
    pub brk_start: u64,
    pub brk: u64,
    mmap_next: u64,
}

// SAFETY: the raw page pointers refer to heap pages owned by this value.
unsafe impl Send for Memory {}

/// A process: one address space and file table, shared by its threads.
pub struct Process {
    pub pid: u64,
    pub name: String,
    /// Path of the executable (for /proc/self/exe), without the leading '/'.
    pub exe: String,
    pub cr3: u64,
    pub mem: SpinLock<Memory>,
    pub fds: SpinLock<FdTable>,
    /// Set by exit_group or a fatal fault: every thread leaves at its next kernel entry.
    pub exiting: AtomicBool,
    pub exit_code: AtomicI32,
    /// Threads still running user code.
    pub threads: AtomicUsize,
    /// Log every system call to the serial console (`strace`).
    pub trace: AtomicBool,
}

/// Per-thread user state, owned by the kernel task that runs the thread.
pub struct UserState {
    pub process: Arc<Process>,
    pub tid: u64,
    /// Kernel stack for syscalls and interrupts while this thread runs.
    pub kernel_rsp: u64,
    pub fs_base: u64,
    fx: Box<FxArea>,
    /// Kernel stack pointer to return to when the thread leaves user mode.
    exit_ctx: u64,
    /// CLONE_CHILD_CLEARTID / set_tid_address: cleared and futex-woken at thread exit.
    pub clear_tid: u64,
}

static NEXT_PID: AtomicU64 = AtomicU64::new(1);

/// Heap bytes kept back from programs (see Memory::map_zero_page).
pub const USER_RESERVE: usize = 48 << 20;

/// Out of memory on behalf of the current program: stop it (like Linux's OOM killer)
/// and return a scratch page to back the request until its threads notice. The page is
/// shared and never freed; only dying programs write to it.
#[cold]
pub fn oom_scratch_page() -> *mut u8 {
    static SCRATCH: spin::Once<usize> = spin::Once::new();
    let scratch = *SCRATCH.call_once(|| memory::new_table() as usize); // a zeroed page
    if let Some(u) = task::current_user()
        && !u.process.exiting.load(Ordering::SeqCst)
    {
        crate::log::warn(&format!("{}: out of memory, program stopped", u.process.name));
        crate::println!("\n{}: out of memory (Nova has {} MiB free); the program was stopped", u.process.name, memory::heap_usage().1 >> 20);
        kill(&u.process);
    }
    memory::phys_to_virt(scratch as u64) as *mut u8
}

impl Memory {
    fn new(cr3: u64) -> Self {
        Self { cr3, pages: BTreeMap::new(), tables: Vec::new(), borrowed: alloc::collections::BTreeSet::new(), foreign: alloc::collections::BTreeSet::new(), shared: Vec::new(), areas: Vec::new(), brk_start: 0, brk: 0, mmap_next: MMAP_TOP }
    }

    /// Map a zeroed page at `va` (page aligned) if not mapped yet; returns its kernel pointer.
    pub fn map_zero_page(&mut self, va: u64, writable: bool) -> *mut u8 {
        if let Some(&p) = self.pages.get(&va) {
            return p;
        }
        // Programs may not take the kernel's last USER_RESERVE bytes (page tables, network
        // buffers and the desktop need them to keep running).
        // Pool pages are zeroed, so programs never see stale kernel data.
        let page = if memory::heap_usage().1 < USER_RESERVE { core::ptr::null_mut() } else { memory::alloc_page().unwrap_or(core::ptr::null_mut()) };
        if page.is_null() {
            return self.out_of_memory(va);
        }
        let flags = PRESENT | USER | if writable { WRITABLE } else { 0 };
        let tables = &mut self.tables;
        // SAFETY: `va` is in the lower half, which belongs to this address space only.
        unsafe { memory::map_page_in(self.cr3, va, memory::virt_to_phys(page as u64), flags, PRESENT | WRITABLE | USER, &mut |t| tables.push(t)) };
        self.pages.insert(va, page);
        page
    }

    /// No memory left for a program page: kill the program (like Linux's OOM killer)
    /// instead of the kernel. Until its threads notice, `va` is backed by a shared scratch
    /// page that is never freed and not recorded in `pages`; only dying programs write it.
    #[cold]
    fn out_of_memory(&mut self, va: u64) -> *mut u8 {
        let scratch = memory::virt_to_phys(oom_scratch_page() as u64);
        let tables = &mut self.tables;
        // SAFETY: as in map_zero_page; the scratch page lives for the rest of the boot.
        unsafe { memory::map_page_in(self.cr3, va, scratch as u64, PRESENT | USER | WRITABLE, PRESENT | WRITABLE | USER, &mut |t| tables.push(t)) };
        memory::phys_to_virt(scratch as u64) as *mut u8
    }

    /// Map `len` bytes of a read-only system file at `va` (page aligned), starting at
    /// file offset `off` (page aligned): whole pages are shared with the file's memory
    /// and copied on the first write; the partial last page is copied now.
    pub fn map_file(&mut self, va: u64, file: &'static [u8], off: usize, len: u64) {
        let mut i = 0;
        while i < len {
            let page_va = va + i;
            let fo = off + i as usize;
            if fo + PAGE as usize <= file.len() {
                let frame = file[fo..].as_ptr();
                self.discard(page_va, PAGE);
                let tables = &mut self.tables;
                // SAFETY: lower-half address of this space; the frame is read-only
                // system-file memory that lives as long as the kernel.
                unsafe { memory::map_page_in(self.cr3, page_va, memory::virt_to_phys(frame as u64), PRESENT | USER, PRESENT | WRITABLE | USER, &mut |t| tables.push(t)) };
                self.pages.insert(page_va, frame as *mut u8);
                self.borrowed.insert(page_va);
            } else if fo < file.len() {
                self.poke(page_va, &file[fo..], true);
            }
            i += PAGE;
        }
    }

    /// Map pages of a shared memory object at `va` (MAP_SHARED of a memfd).
    pub fn map_shared(&mut self, va: u64, shm: crate::ipc::SharedRef, off: usize, len: u64) {
        let mut i = 0;
        while i < len {
            let page_va = va + i;
            let frame = shm.lock().page((off + i as usize) / PAGE as usize);
            self.discard(page_va, PAGE);
            let tables = &mut self.tables;
            // SAFETY: lower-half address of this space; the frame belongs to `shm`,
            // which this address space keeps alive.
            unsafe { memory::map_page_in(self.cr3, page_va, memory::virt_to_phys(frame as u64), PRESENT | USER | WRITABLE, PRESENT | WRITABLE | USER, &mut |t| tables.push(t)) };
            self.pages.insert(page_va, frame);
            self.foreign.insert(page_va);
            i += PAGE;
        }
        self.shared.push(shm);
    }

    /// Give a borrowed page its own writable copy.
    fn unshare(&mut self, va: u64) {
        if !self.borrowed.remove(&va) {
            return;
        }
        let shared = self.pages.remove(&va).expect("borrowed page not mapped");
        let copy = self.map_zero_page(va, true);
        // SAFETY: both are 4 KiB pages; map_zero_page replaced the mapping (PTE rewritten).
        unsafe {
            core::ptr::copy_nonoverlapping(shared, copy, PAGE as usize);
            asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags));
        }
    }

    /// /proc/self/maps-style listing of the areas.
    pub fn maps(&self) -> String {
        let mut areas = self.areas.clone();
        areas.sort_by_key(|a| a.start);
        areas.iter().map(|a| format!("{:08x}-{:08x} {}p 00000000 00:00 0\n", a.start, a.end, if a.writable { "rwx" } else { "r-x" })).collect()
    }

    /// Is every byte of [start, end) inside some area? (Interval walk, not per page:
    /// programs mprotect multi-gigabyte reservations.)
    fn covered(&self, start: u64, end: u64) -> bool {
        let mut spans: Vec<(u64, u64)> = self.areas.iter().filter(|a| a.end > start && a.start < end).map(|a| (a.start, a.end)).collect();
        spans.sort_unstable();
        let mut pos = start;
        for (s, e) in spans {
            if s > pos {
                return false;
            }
            pos = pos.max(e);
            if pos >= end {
                return true;
            }
        }
        pos >= end
    }

    pub fn area_at(&self, addr: u64) -> Option<Area> {
        self.areas.iter().find(|a| addr >= a.start && addr < a.end).copied()
    }

    /// Make sure [addr, addr+len) is mapped (filling demand areas); false if any part
    /// is outside every area.
    pub fn ensure(&mut self, addr: u64, len: u64, write: bool) -> bool {
        if len == 0 {
            return true;
        }
        let Some(end) = addr.checked_add(len) else { return false };
        if addr < USER_MIN || end > USER_MAX {
            return false;
        }
        let mut page = addr & !(PAGE - 1);
        while page < end {
            if write && !self.area_at(page).is_some_and(|a| a.writable) {
                return false; // read-only memory: the caller reports EFAULT
            }
            if !self.pages.contains_key(&page) {
                match self.area_at(page) {
                    Some(a) if !write || a.writable => {
                        self.map_zero_page(page, a.writable);
                    }
                    _ => return false,
                }
            } else if write && self.borrowed.contains(&page) {
                self.unshare(page);
            }
            page += PAGE;
        }
        true
    }

    /// Reserve a fresh area of `len` bytes for mmap; returns its start.
    pub fn reserve(&mut self, len: u64) -> u64 {
        self.reserve_with(len, true)
    }

    /// Is [start, start+len) free (no area overlaps it, inside the user range)?
    pub fn is_free(&self, start: u64, len: u64) -> bool {
        let Some(end) = start.checked_add(len) else { return false };
        start >= USER_MIN && end <= STACK_TOP - STACK_SIZE && !self.areas.iter().any(|a| a.start < end && start < a.end)
    }

    /// Reserve `len` bytes at `hint` if that range is free (mmap address hints, as on
    /// Linux), else anywhere.
    pub fn reserve_at(&mut self, hint: u64, len: u64, writable: bool) -> u64 {
        let len = len.next_multiple_of(PAGE);
        if hint != 0 && hint % PAGE == 0 && self.is_free(hint, len) {
            self.areas.push(Area { start: hint, end: hint + len, writable });
            return hint;
        }
        self.reserve_with(len, writable)
    }

    pub fn reserve_with(&mut self, len: u64, writable: bool) -> u64 {
        let len = len.next_multiple_of(PAGE);
        self.mmap_next -= len + PAGE; // one guard page between areas
        let start = self.mmap_next;
        self.areas.push(Area { start, end: start + len, writable });
        start
    }

    /// Free the pages in [start, start+len) but keep the areas (they read as zero again).
    pub fn discard(&mut self, start: u64, len: u64) {
        let end = start.saturating_add(len.next_multiple_of(PAGE));
        let doomed: Vec<u64> = self.pages.range(start..end).map(|(&va, _)| va).collect();
        for va in doomed {
            if let Some(p) = self.pages.remove(&va) {
                self.unmap_page(va);
                let not_ours = self.borrowed.remove(&va) | self.foreign.remove(&va);
                if !not_ours {
                    // SAFETY: from the page pool in map_zero_page; just unmapped.
                    unsafe { memory::free_page(p) };
                }
            }
        }
    }

    /// mprotect: set write permission on [start, start+len) (areas and mapped pages).
    /// Returns false if part of the range is not mapped.
    pub fn protect(&mut self, start: u64, len: u64, writable: bool) -> bool {
        let end = start.saturating_add(len.next_multiple_of(PAGE));
        if !self.covered(start, end) {
            return false;
        }
        let mut out = Vec::new();
        for a in self.areas.drain(..) {
            if a.end <= start || a.start >= end {
                out.push(a);
                continue;
            }
            if a.start < start {
                out.push(Area { start: a.start, end: start, ..a });
            }
            out.push(Area { start: a.start.max(start), end: a.end.min(end), writable });
            if a.end > end {
                out.push(Area { start: end, end: a.end, ..a });
            }
        }
        self.areas = out;
        let pages: Vec<u64> = self.pages.range(start..end).map(|(&va, _)| va).collect();
        for va in pages {
            // Shared system-file pages stay read-only (copy on write).
            let w = writable && !self.borrowed.contains(&va);
            self.set_writable(va, w);
        }
        true
    }

    fn set_writable(&mut self, va: u64, writable: bool) {
        let mut table = self.cr3;
        for level in [39, 30, 21] {
            // SAFETY: walking this process's own page tables in direct-mapped RAM.
            let e = unsafe { (memory::phys_to_virt(table) as *const u64).add(((va >> level) & 511) as usize).read() };
            if e & PRESENT == 0 {
                return;
            }
            table = e & 0x000F_FFFF_FFFF_F000;
        }
        // SAFETY: update the leaf entry of a mapped page, then flush its TLB entry.
        unsafe {
            let leaf = (memory::phys_to_virt(table) as *mut u64).add(((va >> 12) & 511) as usize);
            let v = leaf.read();
            leaf.write(if writable { v | WRITABLE } else { v & !WRITABLE });
            asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags));
        }
    }

    /// Remove [start, start+len) from the areas and free its pages.
    pub fn unmap(&mut self, start: u64, len: u64) {
        let end = start.saturating_add(len.next_multiple_of(PAGE));
        let mut kept = Vec::new();
        for a in self.areas.drain(..) {
            if a.end <= start || a.start >= end {
                kept.push(a);
                continue;
            }
            if a.start < start {
                kept.push(Area { start: a.start, end: start, ..a });
            }
            if a.end > end {
                kept.push(Area { start: end, end: a.end, ..a });
            }
        }
        self.areas = kept;
        self.discard(start, end - start);
    }

    fn unmap_page(&mut self, va: u64) {
        let mut table = self.cr3;
        for level in [39, 30, 21] {
            // SAFETY: walking this process's own page tables in direct-mapped RAM.
            let e = unsafe { (memory::phys_to_virt(table) as *const u64).add(((va >> level) & 511) as usize).read() };
            if e & PRESENT == 0 {
                return;
            }
            table = e & 0x000F_FFFF_FFFF_F000;
        }
        // SAFETY: clear the leaf entry, then flush the TLB entry (it may be the live space).
        unsafe {
            (memory::phys_to_virt(table) as *mut u64).add(((va >> 12) & 511) as usize).write(0);
            asm!("invlpg [{}]", in(reg) va, options(nostack, preserves_flags));
        }
    }

    /// Copy `data` into user memory at `va` (mapping pages as needed, ignoring areas).
    fn poke(&mut self, va: u64, data: &[u8], writable: bool) {
        let mut done = 0;
        while done < data.len() {
            let addr = va + done as u64;
            let page = addr & !(PAGE - 1);
            let off = (addr - page) as usize;
            let n = (PAGE as usize - off).min(data.len() - done);
            let p = self.map_zero_page(page, writable);
            // SAFETY: p is a 4 KiB page; off + n <= 4096.
            unsafe { core::ptr::copy_nonoverlapping(data[done..].as_ptr(), p.add(off), n) };
            done += n;
        }
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        for (va, p) in core::mem::take(&mut self.pages) {
            if !self.borrowed.contains(&va) && !self.foreign.contains(&va) {
                // SAFETY: from the page pool in map_zero_page; the address space is gone.
                unsafe { memory::free_page(p) };
            }
        }
        for t in self.tables.drain(..).chain([self.cr3]) {
            // SAFETY: page-table pages come from memory::new_table (the page pool).
            // The last thread switched to the kernel address space before dropping.
            unsafe { memory::free_page(memory::phys_to_virt(t) as *mut u8) };
        }
    }
}

impl Process {
    fn new(name: &str, exe: &str) -> Arc<Self> {
        let cr3 = memory::new_table();
        // Share the kernel's upper half.
        // SAFETY: both tables are 4 KiB pages in direct-mapped RAM.
        unsafe {
            let src = memory::phys_to_virt(memory::kernel_cr3()) as *const u64;
            let dst = memory::phys_to_virt(cr3) as *mut u64;
            for i in 256..512 {
                dst.add(i).write(src.add(i).read());
            }
        }
        Arc::new(Self {
            pid: NEXT_PID.fetch_add(1, Ordering::Relaxed),
            name: name.into(),
            exe: exe.into(),
            cr3,
            mem: SpinLock::new(Memory::new(cr3)),
            fds: SpinLock::new(FdTable::new()),
            exiting: AtomicBool::new(false),
            exit_code: AtomicI32::new(0),
            threads: AtomicUsize::new(1),
            trace: AtomicBool::new(false),
        })
    }
}

impl UserState {
    fn new_thread(process: Arc<Process>, tid: u64) -> Box<Self> {
        Box::new(Self { process, tid, kernel_rsp: 0, fs_base: 0, fx: FxArea::initial(), exit_ctx: 0, clear_tid: 0 })
    }
}

/// Kernel stack for a thread: the free space below the caller's frame.
fn kernel_stack_here() -> u64 {
    let here: u64;
    // SAFETY: reading RSP.
    unsafe { asm!("mov {}, rsp", out(reg) here, options(nomem, nostack, preserves_flags)) };
    (here - 1024) & !15
}

/// Called by the scheduler for the task being switched out.
pub fn switch_out(user: Option<&mut UserState>) {
    if let Some(u) = user {
        u.fx.save();
        u.fs_base = rdmsr(IA32_FS_BASE);
    }
}

/// Called by the scheduler for the task being switched in.
pub fn switch_in(user: Option<&UserState>) {
    let cr3 = user.map_or(memory::kernel_cr3(), |u| u.process.cr3);
    if memory::read_cr3() != cr3 {
        // SAFETY: both kinds of address space map the whole kernel identically.
        unsafe { asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags)) };
    }
    if let Some(u) = user {
        gdt::set_kernel_stack(u.kernel_rsp);
        SYSCALL_KERNEL_RSP.store(u.kernel_rsp, Ordering::Relaxed);
        wrmsr(IA32_FS_BASE, u.fs_base);
        u.fx.restore();
    }
}

/// Registers saved by `syscall_entry`, lowest address first.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct SyscallFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r9: u64,
    pub r8: u64,
    pub r10: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rax: u64,
    pub rip: u64,
    pub rflags: u64,
    pub rsp: u64,
}

#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    naked_asm!(
        // IF is clear (FMASK). Switch to this task's kernel stack.
        "mov [rip + {user_rsp}], rsp",
        "mov rsp, [rip + {kernel_rsp}]",
        "push qword ptr [rip + {user_rsp}]",
        "push r11", "push rcx",
        "push rax", "push rdi", "push rsi", "push rdx", "push r10", "push r8", "push r9",
        "push rbx", "push rbp", "push r12", "push r13", "push r14", "push r15",
        "mov rdi, rsp",
        "call {dispatch}",
        "cli",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop rbp", "pop rbx",
        "pop r9", "pop r8", "pop r10", "pop rdx", "pop rsi", "pop rdi", "pop rax",
        "pop rcx", "pop r11",
        "pop rsp",
        "sysretq",
        user_rsp = sym SYSCALL_USER_RSP,
        kernel_rsp = sym SYSCALL_KERNEL_RSP,
        dispatch = sym syscall_dispatch,
    )
}

extern "C" fn syscall_dispatch(frame: &mut SyscallFrame) {
    // SAFETY: re-enable interrupts: syscalls may block and let other tasks run.
    unsafe { asm!("sti", options(nomem, nostack)) };
    check_exiting();
    frame.rax = crate::linux::syscall(frame) as u64;
    check_exiting();
    // SYSRET to a non-canonical address would fault in ring 0: refuse.
    if frame.rip >= USER_MAX {
        exit(-11);
    }
}

/// Enter user mode with the registers in `regs` (rip, rsp, rflags and all GPRs).
/// Returns the exit code when the thread leaves user mode through [`exit_thread`].
#[unsafe(naked)]
unsafe extern "C" fn enter_user(regs: *const SyscallFrame, ctx: *mut u64) -> i64 {
    naked_asm!(
        "pushfq",
        "push rbx", "push rbp", "push r12", "push r13", "push r14", "push r15",
        "mov [rsi], rsp",
        // iretq frame: SS, RSP, RFLAGS, CS, RIP.
        "push {ss}", "push qword ptr [rdi + 15*8]", "push qword ptr [rdi + 14*8]", "push {cs}", "push qword ptr [rdi + 13*8]",
        "mov r15, [rdi + 0*8]", "mov r14, [rdi + 1*8]", "mov r13, [rdi + 2*8]", "mov r12, [rdi + 3*8]",
        "mov rbp, [rdi + 4*8]", "mov rbx, [rdi + 5*8]", "mov r9, [rdi + 6*8]", "mov r8, [rdi + 7*8]",
        "mov r10, [rdi + 8*8]", "mov rdx, [rdi + 9*8]", "mov rsi, [rdi + 10*8]", "mov rax, [rdi + 12*8]",
        "xor ecx, ecx", "xor r11d, r11d",
        "mov rdi, [rdi + 11*8]",
        "iretq",
        ss = const gdt::USER_DATA as u64,
        cs = const gdt::USER_CODE as u64,
    )
}

#[unsafe(naked)]
unsafe extern "C" fn leave_user(ctx: u64, code: i64) -> ! {
    naked_asm!(
        "mov rsp, rdi",
        "mov rax, rsi",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop rbp", "pop rbx",
        "popfq",
        "ret",
    )
}

/// End the current thread (from a syscall or fault handler on its kernel stack).
pub fn exit_thread(code: i32) -> ! {
    let u = task::current_user().expect("exit outside a process");
    if u.clear_tid != 0 {
        // CLONE_CHILD_CLEARTID: zero the tid word and wake a joiner.
        let tid_addr = u.clear_tid;
        if u.process.mem.lock().ensure(tid_addr, 4, true) {
            // SAFETY: mapped and writable in the live address space.
            unsafe { (tid_addr as *mut u32).write_volatile(0) };
        }
        crate::linux::futex_wake(&u.process, tid_addr, 1);
    }
    let ctx = u.exit_ctx;
    // SAFETY: ctx was saved by enter_user on this same kernel stack, further up;
    // everything below it (this handler's frames) is abandoned.
    unsafe { leave_user(ctx, code as i64) }
}

/// End the whole program: every thread leaves at its next kernel entry.
pub fn exit(code: i32) -> ! {
    let u = task::current_user().expect("exit outside a process");
    if !u.process.exiting.swap(true, Ordering::SeqCst) {
        u.process.exit_code.store(code, Ordering::SeqCst);
    }
    crate::linux::futex_wake_all(&u.process);
    exit_thread(code)
}

/// If the program is being torn down, leave now.
pub fn check_exiting() {
    if let Some(u) = task::current_user()
        && u.process.exiting.load(Ordering::SeqCst)
    {
        let code = u.process.exit_code.load(Ordering::SeqCst);
        exit_thread(code);
    }
}

/// A user-mode page fault: fill demand areas. Returns false if the access is invalid.
pub fn page_fault(addr: u64, error: u64) -> bool {
    let Some(u) = task::current_user() else { return false };
    let page = addr & !(PAGE - 1);
    let mut mem = u.process.mem.lock();
    if error & 1 != 0 {
        // Present page: only a write to a shared system-file page is legitimate.
        if error & 2 != 0 && mem.borrowed.contains(&page) && mem.area_at(page).is_some_and(|a| a.writable) {
            mem.unshare(page);
            return true;
        }
        return false;
    }
    match mem.area_at(page) {
        Some(a) if error & 2 == 0 || a.writable => {
            mem.map_zero_page(page, a.writable);
            true
        }
        _ => false,
    }
}

/// Kill the current program after a fault it caused.
pub fn fault(what: &str, rip: u64, addr: Option<u64>) -> ! {
    let name = task::current_user().map(|u| u.process.name.clone()).unwrap_or_default();
    match addr {
        Some(a) => crate::println!("\n{name}: {what} at address {a:#x} (instruction {rip:#x})"),
        None => crate::println!("\n{name}: {what} (instruction {rip:#x})"),
    }
    exit(if what.starts_with("segmentation") { 139 } else { 132 })
}

/// Register the files Limine loaded as modules (programs, libraries, the browser
/// engine) as read-only system files. Returns their paths.
pub fn install_bundled_programs() -> Vec<String> {
    let mut names = Vec::new();
    for m in crate::limine::MODULES.modules() {
        // "@pak": an archive of system files (see scripts/mkpak.py).
        if m.string() == "@pak" {
            names.extend(register_pak(m.data()));
            continue;
        }
        // "name" is bin/name; a path ("usr/lib/libc.so.6") is used as is.
        let name = m.string();
        let allowed = ["lib/", "lib64/", "usr/", "etc/", "opt/"];
        let path = if !name.contains('/') && !name.is_empty() {
            format!("bin/{name}")
        } else if allowed.iter().any(|p| name.starts_with(p)) && !name.contains("..") {
            String::from(name)
        } else {
            continue;
        };
        crate::fs::add_system_file(&path, m.data());
        names.push(String::from(name));
    }
    names
}

/// Register every file of a NOVAPAK1 archive as a read-only system file (no copy).
fn register_pak(pak: &'static [u8]) -> Vec<String> {
    let mut names = Vec::new();
    if pak.len() < 16 || &pak[0..8] != b"NOVAPAK1" {
        crate::log::warn("system pak: bad header");
        return names;
    }
    let count = le64(pak, 8) as usize;
    let mut p = 16;
    for _ in 0..count {
        if p + 18 > pak.len() {
            break;
        }
        let (off, size, len) = (le64(pak, p) as usize, le64(pak, p + 8) as usize, le16(pak, p + 16) as usize);
        let Some(path) = pak.get(p + 18..p + 18 + len).and_then(|b| core::str::from_utf8(b).ok()) else { break };
        p += 18 + len;
        if let Some(data) = pak.get(off..off + size)
            && !path.contains("..")
        {
            crate::fs::add_system_file(path, data);
            names.push(String::from(path));
        }
    }
    names
}

// ---------------------------------------------------------------------------
// ELF loading

fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn le64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

struct Loaded {
    entry: u64,
    phdr: u64,
    phnum: u64,
    /// PT_INTERP path (dynamically linked programs).
    interp: Option<String>,
}

/// Load address of position-independent executables (as on Linux).
const PIE_BASE: u64 = 0x5555_5555_4000;
/// Load address of the dynamic loader (between the mmap areas and the stack).
const INTERP_BASE: u64 = 0x7F80_0000_0000;

/// `elf` as a 'static slice: only valid when it is a system file (is_static).
fn elf_static(elf: &[u8]) -> &'static [u8] {
    // SAFETY: callers pass slices obtained from fs::system_file, which are 'static.
    unsafe { core::slice::from_raw_parts(elf.as_ptr(), elf.len()) }
}

fn load_elf(u: &mut Memory, elf: &[u8], dyn_base: u64, is_static: bool) -> Result<Loaded, String> {
    if elf.len() < 64 || &elf[0..4] != b"\x7fELF" {
        return Err("not an ELF executable".into());
    }
    if elf[4] != 2 || elf[5] != 1 || le16(elf, 18) != 0x3E {
        return Err("not a 64-bit little-endian x86-64 ELF file".into());
    }
    let kind = le16(elf, 16);
    let base = match kind {
        2 => 0,        // ET_EXEC: fixed addresses
        3 => dyn_base, // ET_DYN: PIE program or shared object (the dynamic loader)
        _ => return Err(format!("not an executable (ELF type {kind})")),
    };
    let mut interp = None;
    let phoff = le64(elf, 32) as usize;
    let phentsize = le16(elf, 54) as usize;
    let phnum = le16(elf, 56) as usize;
    let mut phdr_addr = 0;
    let mut top = 0;
    for i in 0..phnum {
        let p = &elf.get(phoff + i * phentsize..phoff + (i + 1) * phentsize).ok_or("truncated program headers")?;
        let ptype = le32(p, 0);
        let flags = le32(p, 4);
        let offset = le64(p, 8) as usize;
        let vaddr = le64(p, 16) + base;
        let filesz = le64(p, 32) as usize;
        let memsz = le64(p, 40);
        match ptype {
            1 => {
                // PT_LOAD
                if vaddr < USER_MIN || vaddr + memsz > STACK_TOP - STACK_SIZE {
                    return Err(format!("segment at {vaddr:#x} outside the user range"));
                }
                let data = elf.get(offset..offset + filesz).ok_or("truncated segment")?;
                let writable = flags & 2 != 0;
                let page_va = vaddr & !(PAGE - 1);
                let skew = (vaddr - page_va) as usize;
                if is_static && offset >= skew && (offset - skew) % PAGE as usize == 0 {
                    // Share whole pages with the read-only file (copy on write); copy the
                    // partial last page so .bss bytes on it start as zero.
                    let page_off = offset - skew;
                    let full_end = (offset + filesz) & !(PAGE as usize - 1);
                    if full_end > page_off {
                        u.map_file(page_va, elf_static(elf), page_off, (full_end - page_off) as u64);
                    }
                    let tail_va = page_va + full_end.saturating_sub(page_off) as u64;
                    let tail_start = full_end.max(offset);
                    if tail_start < offset + filesz {
                        u.poke(tail_va.max(vaddr), &elf[tail_start..offset + filesz], writable);
                    }
                } else {
                    u.poke(vaddr, data, writable);
                }
                // Zero-fill the rest of the segment (.bss) on whole pages.
                let mut page = (vaddr + filesz as u64) & !(PAGE - 1);
                while page < vaddr + memsz {
                    u.map_zero_page(page, writable);
                    page += PAGE;
                }
                u.areas.push(Area { start: vaddr & !(PAGE - 1), end: (vaddr + memsz).next_multiple_of(PAGE), writable });
                if offset == 0 {
                    phdr_addr = vaddr + phoff as u64;
                }
                top = top.max(vaddr + memsz);
            }
            3 => {
                // PT_INTERP: the dynamic loader this program needs.
                let raw = elf.get(offset..offset + filesz).ok_or("truncated PT_INTERP")?;
                interp = Some(String::from_utf8_lossy(raw).trim_end_matches('\0').into());
            }
            6 => phdr_addr = vaddr, // PT_PHDR
            _ => {}
        }
    }
    if top == 0 {
        return Err("no loadable segments".into());
    }
    if u.brk_start == 0 {
        // The program's heap follows the program (not the dynamic loader).
        u.brk_start = top.next_multiple_of(PAGE);
        u.brk = u.brk_start;
    }
    Ok(Loaded { entry: le64(elf, 24) + base, phdr: phdr_addr, phnum: phnum as u64, interp })
}

/// Build the initial stack: argc, argv, envp, auxv (System V ABI), return RSP.
fn build_stack(u: &mut Memory, args: &[String], env: &[String], l: &Loaded, interp_base: u64, execfn: &str) -> u64 {
    u.areas.push(Area { start: STACK_TOP - STACK_SIZE, end: STACK_TOP, writable: true });
    let mut sp = STACK_TOP;
    let push_bytes = |u: &mut Memory, sp: &mut u64, data: &[u8]| -> u64 {
        *sp -= data.len() as u64;
        u.poke(*sp, data, true);
        *sp
    };
    let mut cstr = |s: &str| {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    };
    let execfn_ptr = push_bytes(u, &mut sp, &cstr(execfn));
    let platform = push_bytes(u, &mut sp, &cstr("x86_64"));
    let mut random = [0u8; 16];
    crate::rand::fill(&mut random);
    let random_ptr = push_bytes(u, &mut sp, &random);
    let argv: Vec<u64> = args.iter().map(|a| push_bytes(u, &mut sp, &cstr(a))).collect();
    let envp: Vec<u64> = env.iter().map(|e| push_bytes(u, &mut sp, &cstr(e))).collect();
    let auxv: [(u64, u64); 17] = [
        (3, l.phdr),
        (4, 56),
        (5, l.phnum),
        (6, PAGE),
        (7, interp_base),
        (8, 0),
        (9, l.entry),
        (11, 0),
        (12, 0),
        (13, 0),
        (14, 0),
        (15, platform),
        (16, 0),
        (17, 100),
        (23, 0),
        (25, random_ptr),
        (31, execfn_ptr),
    ];
    let words = 1 + argv.len() + 1 + envp.len() + 1 + (auxv.len() + 1) * 2;
    sp &= !15;
    if words % 2 == 1 {
        sp -= 8; // keep RSP 16-byte aligned at entry
    }
    let mut table: Vec<u64> = Vec::with_capacity(words);
    table.push(args.len() as u64);
    table.extend(&argv);
    table.push(0);
    table.extend(&envp);
    table.push(0);
    for (k, v) in auxv {
        table.push(k);
        table.push(v);
    }
    table.push(0);
    table.push(0);
    let bytes: Vec<u8> = table.iter().flat_map(|w| w.to_le_bytes()).collect();
    sp -= bytes.len() as u64;
    u.poke(sp, &bytes, true);
    sp
}

/// Run the program at `path` (NovaFS) with `args` (args[0] = program name) in the
/// current task, attached to its terminal. Returns the exit status once every
/// thread of the program has finished.
pub fn run(path: &str, args: &[String]) -> Result<i32, String> {
    run_traced(path, args, false)
}

/// `run`, optionally logging every system call to the serial console.
pub fn run_traced(path: &str, args: &[String], trace: bool) -> Result<i32, String> {
    run_with(path, args, trace, Vec::new(), |_| {})
}

/// Start a program in a new task, with extra open files at given descriptor
/// numbers (e.g. pipes on 3 and 4). Returns the process once it is set up; it runs
/// until it exits or [`kill`] is called.
pub fn spawn(path: &str, args: Vec<String>, extra: Vec<(usize, crate::linux::FileRef)>, tty: Option<crate::tty::TtyRef>) -> Result<Arc<Process>, String> {
    let slot: Arc<SpinLock<Option<Result<Arc<Process>, String>>>> = Arc::new(SpinLock::new(None));
    let slot2 = slot.clone();
    let path = String::from(path);
    task::spawn_with_tty(&format!("run {path}"), tty, move || {
        let s3 = slot2.clone();
        let r = run_with(&path, &args, false, extra, move |p| *s3.lock() = Some(Ok(p)));
        if let Err(e) = r {
            *slot2.lock() = Some(Err(e));
        }
    });
    loop {
        if let Some(r) = slot.lock().take() {
            return r;
        }
        task::yield_now();
    }
}

/// Ask every thread of `p` to exit (like SIGKILL).
pub fn kill(p: &Process) {
    if !p.exiting.swap(true, Ordering::SeqCst) {
        p.exit_code.store(137, Ordering::SeqCst);
    }
    crate::linux::futex_wake_all(p);
    crate::ipc::notify();
}

fn run_with(path: &str, args: &[String], trace: bool, extra: Vec<(usize, crate::linux::FileRef)>, on_start: impl FnOnce(Arc<Process>)) -> Result<i32, String> {
    if task::current_user().is_some() {
        return Err("already running a program".into());
    }
    let owned;
    let (elf, is_static): (&[u8], bool) = match crate::fs::system_file(path) {
        Some(s) => (s, true),
        None => {
            owned = crate::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            (&owned, false)
        }
    };
    let name: String = path.rsplit('/').next().unwrap_or(path).into();
    let process = Process::new(&name, path);
    process.trace.store(trace, Ordering::Relaxed);
    for (fd, file) in extra {
        process.fds.lock().install(fd, file);
    }
    let env = ["HOME=/", "PATH=/bin", "TERM=xterm", "LANG=C.UTF-8", "USER=nova"].map(String::from);
    let (entry, rsp) = {
        let mut mem = process.mem.lock();
        let loaded = load_elf(&mut mem, elf, PIE_BASE, is_static)?;
        // Dynamically linked: load the interpreter too and start there; it maps the
        // shared libraries itself (openat + mmap) and then jumps to AT_ENTRY.
        let (start, interp_base) = match &loaded.interp {
            Some(ip) => {
                let ip = ip.trim_start_matches('/');
                let l = match crate::fs::system_file(ip) {
                    Some(ld) => load_elf(&mut mem, ld, INTERP_BASE, true)?,
                    None => {
                        let ld = crate::fs::read(ip).map_err(|_| format!("{path} needs the dynamic loader /{ip}, which is not installed"))?;
                        load_elf(&mut mem, &ld, INTERP_BASE, false)?
                    }
                };
                (l.entry, INTERP_BASE)
            }
            None => (loaded.entry, 0),
        };
        // The heap starts after the program, not after the loader.
        let rsp = build_stack(&mut mem, args, &env, &loaded, interp_base, path);
        (start, rsp)
    };

    let mut thread = UserState::new_thread(process.clone(), process.pid);
    thread.kernel_rsp = kernel_stack_here();
    task::set_user(Some(thread));
    let regs = SyscallFrame { r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r9: 0, r8: 0, r10: 0, rdx: 0, rsi: 0, rdi: 0, rax: 0, rip: entry, rflags: 0x202, rsp };
    let ctx = &raw mut task::current_user().unwrap().exit_ctx;
    on_start(process.clone());
    // SAFETY: the address space is loaded; registers were set up above.
    let code = unsafe { enter_user(&regs, ctx) } as i32;
    process.threads.fetch_sub(1, Ordering::SeqCst);
    task::set_user(None);
    // Wait for the other threads to leave (they notice `exiting`).
    if process.threads.load(Ordering::SeqCst) > 0 {
        process.exiting.store(true, Ordering::SeqCst);
        crate::linux::futex_wake_all(&process);
        while process.threads.load(Ordering::SeqCst) > 0 {
            task::wait_event();
        }
    }
    let status = if process.exiting.load(Ordering::SeqCst) { process.exit_code.load(Ordering::SeqCst) } else { code };
    Ok(status)
}

/// Start a new thread of the current process (clone with CLONE_VM | CLONE_THREAD):
/// it resumes after the syscall with rax = 0, on `stack`, with FS base `tls`.
pub fn spawn_thread(parent: &SyscallFrame, stack: u64, tls: Option<u64>, clear_tid: u64) -> u64 {
    let me = task::current_user().expect("clone outside a process");
    let process = me.process.clone();
    let tid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let mut regs = SyscallFrame { ..*parent };
    regs.rax = 0;
    if stack != 0 {
        regs.rsp = stack;
    }
    let fs = tls.unwrap_or(me.fs_base);
    process.threads.fetch_add(1, Ordering::SeqCst);
    let name = format!("{}:{tid}", process.name);
    task::spawn_with_tty(&name, task::current_tty(), move || {
        let mut thread = UserState::new_thread(process.clone(), tid);
        thread.fs_base = fs;
        thread.clear_tid = clear_tid;
        thread.kernel_rsp = kernel_stack_here();
        task::set_user(Some(thread));
        let ctx = &raw mut task::current_user().unwrap().exit_ctx;
        // SAFETY: as for run(): this task now runs in the process's address space.
        unsafe { enter_user(&regs, ctx) };
        task::set_user(None);
        process.threads.fetch_sub(1, Ordering::SeqCst);
    });
    tid
}
