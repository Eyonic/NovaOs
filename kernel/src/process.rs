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
/// 32-bit programs (Linux's i386 layout on x86-64): everything below 4 GiB.
const STACK_TOP32: u64 = 0xFFFF_E000;
const MMAP_TOP32: u64 = 0xF700_0000;
const PIE_BASE32: u64 = 0x5655_5000;
const INTERP_BASE32: u64 = 0xF7F0_0000;
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
    // SYSCALL: CS = 0x08, SS = 0x10. SYSRET: SS = 0x13 + 8, CS = 0x13 + 16. The RPL bits
    // go into STAR itself, as Linux does: Intel forces RPL 3 on both selectors, but AMD
    // loads SS verbatim, and an SS of 0x18 (RPL 0) makes the next iretq to user mode #GP.
    wrmsr(IA32_STAR, (((gdt::USER_DATA - 8) as u64) << 48) | ((gdt::KERNEL_CODE as u64) << 32));
    wrmsr(IA32_LSTAR, syscall_entry as *const () as u64);
    wrmsr(IA32_FMASK, 0x0004_0700); // clear IF, DF, TF and AC on entry
}

/// Size of the XSAVE area for the enabled components (0 without XSAVE: FXSAVE, 512 bytes).
pub fn xsave_size() -> u64 {
    XSAVE_SIZE.load(Ordering::Relaxed)
}

/// The enabled XSAVE components (XCR0).
pub fn xcr0() -> u64 {
    XCR0.load(Ordering::Relaxed)
}

/// Save the live SIMD/FPU state at `dst` (64-byte aligned, xsave_size() or 512 bytes).
///
/// # Safety
/// `dst` must be writable for that many bytes in the current address space.
pub unsafe fn save_fpu(dst: *mut u8) {
    // SAFETY: per the caller; the kernel itself never touches SIMD registers.
    unsafe {
        if xsave_size() != 0 {
            asm!("xsave64 [{}]", in(reg) dst, in("eax") u32::MAX, in("edx") u32::MAX, options(nostack, preserves_flags));
        } else {
            asm!("fxsave64 [{}]", in(reg) dst, options(nostack, preserves_flags));
        }
    }
}

/// Load SIMD/FPU state a program handed back (rt_sigreturn). The state is copied and
/// sanitized first, as Linux does: a malformed image would fault inside the kernel.
///
/// # Safety
/// `src` must be readable for 512 bytes (xsave_size() bytes if `xsave`).
pub unsafe fn restore_fpu(src: *const u8, xsave: bool) {
    let area = FxArea::initial();
    let len = if xsave && xsave_size() != 0 { xsave_size() as usize } else { 512 };
    // SAFETY: per the caller; the area holds at least max(xsave_size, 576) bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(src, area.ptr, len);
        // MXCSR: only defined bits (a reserved bit set makes fxrstor/xrstor #GP).
        let mxcsr = area.ptr.add(24).cast::<u32>();
        mxcsr.write(mxcsr.read() & 0xFFFF);
        if xsave_size() != 0 {
            let header = area.ptr.add(512);
            if len == 512 {
                core::ptr::write_bytes(header, 0, 64);
                // Legacy region only: x87 and SSE state.
                header.cast::<u64>().write(3 & xcr0());
            } else {
                let bv = header.cast::<u64>();
                bv.write(bv.read() & xcr0());
                header.add(8).cast::<u64>().write(0); // XCOMP_BV: standard format
                core::ptr::write_bytes(header.add(16), 0, 48);
            }
        }
    }
    area.restore();
}

/// A 32-bit signal handler is about to run: keep the interrupted code's SIMD state.
pub fn push_signal_fpu(u: &mut UserState) {
    let area = FxArea::initial();
    // SAFETY: the area is sized and aligned for the enabled state components.
    unsafe { save_fpu(area.ptr) };
    if u.sigfpu.len() == 32 {
        u.sigfpu.remove(0); // handlers left with siglongjmp: forget the oldest
    }
    u.sigfpu.push(area);
}

/// The handler returned (sigreturn): bring the interrupted code's SIMD state back.
pub fn pop_signal_fpu(u: &mut UserState) {
    if let Some(area) = u.sigfpu.pop() {
        area.restore();
    }
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
    /// A 32-bit program's address space: everything stays below 4 GiB.
    pub compat: bool,
}

// SAFETY: the raw page pointers refer to heap pages owned by this value.
unsafe impl Send for Memory {}

/// A process: one address space and file table, shared by its threads.
pub struct Process {
    pub pid: u64,
    pub name: String,
    /// Path of the executable (for /proc/self/exe), without the leading '/'.
    pub exe: SpinLock<String>,
    /// Page-table root; execve gives the process a new address space.
    pub cr3: AtomicU64,
    pub mem: SpinLock<Memory>,
    pub fds: SpinLock<FdTable>,
    /// Set by exit_group or a fatal fault: every thread leaves at its next kernel entry.
    pub exiting: AtomicBool,
    pub exit_code: AtomicI32,
    /// Threads still running user code.
    pub threads: AtomicUsize,
    /// Log every system call to the serial console (`strace`).
    pub trace: AtomicBool,
    /// Parent process id (0: started by Nova itself, e.g. from the shell).
    pub parent: AtomicU64,
    /// Signal dispositions and process-directed pending signals.
    pub signals: SpinLock<crate::signal::Signals>,
    /// The signal that terminated the process (0 if it exited).
    pub term_signal: AtomicI32,
    /// vfork: set when the child has called execve or exited (the parent then resumes).
    pub vfork_done: AtomicBool,
    /// Mount namespace (shared with the parent unless unshared).
    pub ns: SpinLock<Arc<crate::vfs::Namespace>>,
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
    /// Blocked signals (bit n-1 = signal n).
    pub sigmask: u64,
    /// Signals sent to this thread (tgkill, raise).
    pub pending: u64,
    /// sigaltstack: (base, size, flags).
    pub altstack: (u64, u64, u32),
    /// rt_sigsuspend: the mask to restore once the handler that ends the wait returns.
    pub saved_mask: Option<u64>,
    /// A 32-bit (compatibility mode) thread.
    pub compat: bool,
    /// 32-bit TLS descriptors (set_thread_area), GDT entries 12-14 while this thread runs.
    pub tls: [u64; gdt::TLS_ENTRIES],
    /// The user's %fs and %gs selectors, kept across task switches (32-bit TLS uses %gs).
    pub user_fs: u16,
    pub user_gs: u16,
    /// SIMD state of the interrupted code while 32-bit signal handlers run (innermost last).
    sigfpu: Vec<Box<FxArea>>,
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
        Self { cr3, pages: BTreeMap::new(), tables: Vec::new(), borrowed: alloc::collections::BTreeSet::new(), foreign: alloc::collections::BTreeSet::new(), shared: Vec::new(), areas: Vec::new(), brk_start: 0, brk: 0, mmap_next: MMAP_TOP, compat: false }
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

    /// fork: give `child` (an empty address space) the same mappings. Private pages are
    /// copied; read-only system-file pages stay shared (copied on write, as here); shared
    /// memory pages stay shared. False when memory ran out.
    pub fn fork_into(&self, child: &mut Memory) -> bool {
        for (&va, &frame) in &self.pages {
            let (target, flags) = if self.borrowed.contains(&va) {
                child.borrowed.insert(va);
                (frame, PRESENT | USER)
            } else if self.foreign.contains(&va) {
                child.foreign.insert(va);
                (frame, PRESENT | USER | WRITABLE)
            } else {
                let Some(copy) = memory::alloc_page() else { return false };
                // SAFETY: both are 4 KiB pages; `frame` is mapped in this address space.
                unsafe { core::ptr::copy_nonoverlapping(frame, copy, PAGE as usize) };
                let writable = self.area_at(va).is_some_and(|a| a.writable);
                (copy, PRESENT | USER | if writable { WRITABLE } else { 0 })
            };
            let tables = &mut child.tables;
            // SAFETY: a lower-half address of the child's own (unused) address space.
            unsafe { memory::map_page_in(child.cr3, va, memory::virt_to_phys(target as u64), flags, PRESENT | WRITABLE | USER, &mut |t| tables.push(t)) };
            child.pages.insert(va, target);
        }
        child.shared = self.shared.clone();
        child.areas = self.areas.clone();
        child.brk_start = self.brk_start;
        child.brk = self.brk;
        child.mmap_next = self.mmap_next;
        child.compat = self.compat;
        true
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
    /// Top of the stack area (a 32-bit program's is below 4 GiB).
    pub fn stack_top(&self) -> u64 {
        if self.compat { STACK_TOP32 } else { STACK_TOP }
    }

    /// Make this (empty) address space a 32-bit program's.
    fn make_compat(&mut self) {
        self.compat = true;
        self.mmap_next = MMAP_TOP32;
    }

    pub fn reserve(&mut self, len: u64) -> u64 {
        self.reserve_with(len, true)
    }

    /// Is [start, start+len) free (no area overlaps it, inside the user range)?
    pub fn is_free(&self, start: u64, len: u64) -> bool {
        let Some(end) = start.checked_add(len) else { return false };
        start >= USER_MIN && end <= self.stack_top() - STACK_SIZE && !self.areas.iter().any(|a| a.start < end && start < a.end)
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

/// A new page-table root sharing the kernel's upper half.
fn new_address_space() -> u64 {
    let cr3 = memory::new_table();
    // SAFETY: both tables are 4 KiB pages in direct-mapped RAM.
    unsafe {
        let src = memory::phys_to_virt(memory::kernel_cr3()) as *const u64;
        let dst = memory::phys_to_virt(cr3) as *mut u64;
        for i in 256..512 {
            dst.add(i).write(src.add(i).read());
        }
    }
    cr3
}

impl Process {
    fn new(name: &str, exe: &str, parent: u64) -> Arc<Self> {
        let cr3 = new_address_space();
        let p = Arc::new(Self {
            pid: NEXT_PID.fetch_add(1, Ordering::Relaxed),
            name: name.into(),
            exe: SpinLock::new(exe.into()),
            cr3: AtomicU64::new(cr3),
            mem: SpinLock::new(Memory::new(cr3)),
            fds: SpinLock::new(FdTable::new()),
            exiting: AtomicBool::new(false),
            exit_code: AtomicI32::new(0),
            threads: AtomicUsize::new(1),
            trace: AtomicBool::new(false),
            parent: AtomicU64::new(parent),
            signals: SpinLock::new(crate::signal::Signals::new()),
            term_signal: AtomicI32::new(0),
            vfork_done: AtomicBool::new(false),
            ns: SpinLock::new(crate::vfs::root()),
        });
        TABLE.lock().insert(p.pid, Entry { parent, process: Arc::downgrade(&p), status: None });
        p
    }
}

// ---------------------------------------------------------------- process table

/// Give process `pid` its own copy of its mount namespace (clone with CLONE_NEWNS).
pub fn unshare_ns(pid: u64) {
    let p = TABLE.lock().get(&pid).and_then(|e| e.process.upgrade());
    if let Some(p) = p {
        let copy = p.ns.lock().copy();
        *p.ns.lock() = copy;
    }
}

/// Whether process `pid` exists (or is a zombie).
pub fn exists(pid: u64) -> bool {
    TABLE.lock().contains_key(&pid)
}

/// Every process that exists or has exited and not been waited for (a zombie).
struct Entry {
    parent: u64,
    process: alloc::sync::Weak<Process>,
    /// wait() status once the process has ended.
    status: Option<i32>,
}

static TABLE: SpinLock<BTreeMap<u64, Entry>> = SpinLock::new(BTreeMap::new());

/// The live process with id `pid`.
pub fn find(pid: u64) -> Option<Arc<Process>> {
    TABLE.lock().get(&pid).and_then(|e| e.process.upgrade())
}

/// Every live process whose parent is `pid` (or every process, for 0).
pub fn processes() -> Vec<Arc<Process>> {
    TABLE.lock().values().filter_map(|e| e.process.upgrade()).collect()
}

/// End of a process: record its wait status for the parent, hand its children to no
/// one, and tell the parent (SIGCHLD).
fn finish(process: &Process, status: i32) {
    let parent = process.parent.load(Ordering::SeqCst);
    {
        let mut table = TABLE.lock();
        // Orphans: nobody will wait for them; their zombies go away now.
        let orphans: Vec<u64> = table.iter().filter(|(_, e)| e.parent == process.pid).map(|(&pid, _)| pid).collect();
        for pid in orphans {
            if table.get(&pid).is_some_and(|e| e.status.is_some()) {
                table.remove(&pid);
            } else if let Some(e) = table.get_mut(&pid) {
                e.parent = 0;
                if let Some(p) = e.process.upgrade() {
                    p.parent.store(0, Ordering::SeqCst);
                }
            }
        }
        if parent == 0 || !table.contains_key(&parent) {
            table.remove(&process.pid);
        } else if let Some(e) = table.get_mut(&process.pid) {
            e.status = Some(status);
        }
    }
    process.vfork_done.store(true, Ordering::SeqCst);
    if parent != 0
        && let Some(p) = find(parent)
    {
        crate::signal::send_process(&p, crate::signal::SIGCHLD, process.pid);
    }
    crate::ipc::notify();
}

/// The wait() status word for a process that ended with `code` or by `signal`.
fn wait_status(code: i32, signal: i32) -> i32 {
    if signal != 0 { signal & 0x7F } else { (code & 0xFF) << 8 }
}

/// wait4/waitid: a child of the calling process that has ended. `pid` > 0 selects one
/// child, -1 (or 0 / < -1: process groups, which Nova does not have) any child.
/// Returns (pid, status); Ok(None) with `nohang` when children are still running.
pub fn wait_child(pid: i64, nohang: bool, reap: bool) -> Result<Option<(u64, i32)>, i64> {
    let me = task::current_user().expect("wait outside a process").process.pid;
    loop {
        {
            let mut table = TABLE.lock();
            let mut children = table.iter().filter(|(p, e)| e.parent == me && (pid <= 0 || **p == pid as u64)).peekable();
            if children.peek().is_none() {
                return Err(10); // ECHILD
            }
            if let Some((&child, e)) = children.find(|(_, e)| e.status.is_some()) {
                let status = e.status.unwrap();
                if reap {
                    table.remove(&child);
                }
                return Ok(Some((child, status)));
            }
        }
        if nohang {
            return Ok(None);
        }
        if let Some(u) = task::current_user()
            && crate::signal::has_deliverable(u)
        {
            return Err(4); // EINTR: a signal handler runs first
        }
        check_exiting();
        task::wait_event();
    }
}

/// Terminate `p` by signal `sig` (default action, or SIGKILL).
pub fn terminate(p: &Process, sig: u32) {
    if !p.exiting.swap(true, Ordering::SeqCst) {
        p.exit_code.store(128 + sig as i32, Ordering::SeqCst);
        p.term_signal.store(sig as i32, Ordering::SeqCst);
    }
    crate::linux::futex_wake_all(p);
    crate::ipc::notify();
}

impl UserState {
    fn new_thread(process: Arc<Process>, tid: u64) -> Box<Self> {
        Box::new(Self { process, tid, kernel_rsp: 0, fs_base: 0, fx: FxArea::initial(), exit_ctx: 0, clear_tid: 0, sigmask: 0, pending: 0, altstack: (0, 0, 2), saved_mask: None, compat: false, tls: [0; gdt::TLS_ENTRIES], user_fs: 0, user_gs: 0, sigfpu: Vec::new() })
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
        if u.compat {
            let (fs, gs): (u16, u16);
            // SAFETY: reading segment selectors has no side effects.
            unsafe { asm!("mov {0:x}, fs", "mov {1:x}, gs", out(reg) fs, out(reg) gs, options(nomem, nostack, preserves_flags)) };
            u.user_fs = fs;
            u.user_gs = gs;
        } else {
            u.fs_base = rdmsr(IA32_FS_BASE);
        }
    }
}

/// DS and ES for compatibility mode (64-bit code ignores them; 32-bit code needs a usable
/// data segment there).
fn load_ds_es(sel: u16) {
    // SAFETY: the user data segment is a flat, present DPL 3 data descriptor.
    unsafe { asm!("mov ds, {0:x}", "mov es, {0:x}", in(reg) sel, options(nostack, preserves_flags)) };
}

/// sigreturn of a 32-bit thread restored these selectors: load them now.
pub fn reload_fs_gs(fs: u16, gs: u16) {
    load_fs_gs(fs, gs);
}

/// Load %fs and %gs selectors (from GDT entries, e.g. 32-bit TLS).
fn load_fs_gs(fs: u16, gs: u16) {
    // SAFETY: the selectors are null or user TLS/data descriptors (set_thread_area
    // validates what it installs); the kernel itself does not use %fs or %gs.
    unsafe { asm!("mov fs, {0:x}", "mov gs, {1:x}", in(reg) fs, in(reg) gs, options(nostack, preserves_flags)) };
}

/// Called by the scheduler for the task being switched in.
pub fn switch_in(user: Option<&UserState>) {
    let cr3 = user.map_or(memory::kernel_cr3(), |u| u.process.cr3.load(Ordering::Relaxed));
    if memory::read_cr3() != cr3 {
        // SAFETY: both kinds of address space map the whole kernel identically.
        unsafe { asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags)) };
    }
    if let Some(u) = user {
        gdt::set_kernel_stack(u.kernel_rsp);
        SYSCALL_KERNEL_RSP.store(u.kernel_rsp, Ordering::Relaxed);
        if u.compat {
            // The descriptors first: loading a selector reads its descriptor.
            gdt::set_tls(&u.tls);
            load_fs_gs(u.user_fs, u.user_gs);
            load_ds_es(gdt::USER_DATA);
        } else {
            // A 64-bit thread uses the FS base MSR; null selectors first, since loading a
            // selector would overwrite the base.
            load_fs_gs(0, 0);
            wrmsr(IA32_FS_BASE, u.fs_base);
        }
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
    crate::signal::deliver(frame);
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

/// `enter_user` for a 32-bit thread: the same, with the compatibility-mode code segment.
#[unsafe(naked)]
unsafe extern "C" fn enter_user32(regs: *const SyscallFrame, ctx: *mut u64) -> i64 {
    naked_asm!(
        "pushfq",
        "push rbx", "push rbp", "push r12", "push r13", "push r14", "push r15",
        "mov [rsi], rsp",
        // Compatibility mode uses DS and ES (a null DS faults there): the user data segment.
        "mov eax, {ss}", "mov ds, ax", "mov es, ax",
        "push {ss}", "push qword ptr [rdi + 15*8]", "push qword ptr [rdi + 14*8]", "push {cs}", "push qword ptr [rdi + 13*8]",
        "mov r15, [rdi + 0*8]", "mov r14, [rdi + 1*8]", "mov r13, [rdi + 2*8]", "mov r12, [rdi + 3*8]",
        "mov rbp, [rdi + 4*8]", "mov rbx, [rdi + 5*8]", "mov r9, [rdi + 6*8]", "mov r8, [rdi + 7*8]",
        "mov r10, [rdi + 8*8]", "mov rdx, [rdi + 9*8]", "mov rsi, [rdi + 10*8]", "mov rax, [rdi + 12*8]",
        // i386 code keeps ecx across int 0x80: it travels in the r10 slot.
        "mov rcx, [rdi + 8*8]", "xor r11d, r11d",
        "mov rdi, [rdi + 11*8]",
        "iretq",
        ss = const gdt::USER_DATA as u64,
        cs = const gdt::USER_CODE32 as u64,
    )
}

/// Leave the kernel for user mode with `regs` in 64-bit or compatibility mode (execve,
/// which may switch between them). Kernel frames below are abandoned: the next entry
/// starts again at the thread's kernel stack top.
#[unsafe(naked)]
unsafe extern "C" fn resume_user(regs: *const SyscallFrame, cs: u64) -> ! {
    naked_asm!(
        "cli",
        // DS/ES: the user data segment (needed by 32-bit programs, ignored in 64-bit mode).
        "mov eax, {ss}", "mov ds, ax", "mov es, ax",
        "push {ss}", "push qword ptr [rdi + 15*8]", "push qword ptr [rdi + 14*8]", "push rsi", "push qword ptr [rdi + 13*8]",
        "mov r15, [rdi + 0*8]", "mov r14, [rdi + 1*8]", "mov r13, [rdi + 2*8]", "mov r12, [rdi + 3*8]",
        "mov rbp, [rdi + 4*8]", "mov rbx, [rdi + 5*8]", "mov r9, [rdi + 6*8]", "mov r8, [rdi + 7*8]",
        "mov r10, [rdi + 8*8]", "mov rdx, [rdi + 9*8]", "mov rsi, [rdi + 10*8]", "mov rax, [rdi + 12*8]",
        "xor ecx, ecx", "xor r11d, r11d",
        "mov rdi, [rdi + 11*8]",
        "iretq",
        ss = const gdt::USER_DATA as u64,
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
    /// ELFCLASS32 (an i386 program).
    class32: bool,
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
    // ELFCLASS64 + EM_X86_64, or ELFCLASS32 + EM_386 (32-bit programs).
    let class32 = match (elf[4], elf[5], le16(elf, 18)) {
        (2, 1, 0x3E) => false,
        (1, 1, 3) if elf.len() >= 52 => true,
        _ => return Err("not an x86-64 or i386 little-endian ELF file".into()),
    };
    if class32 != u.compat {
        return Err("program and dynamic loader are not the same kind (32- and 64-bit)".into());
    }
    // Header and program-header fields by ELF class.
    let word = |b: &[u8], o32: usize, o64: usize| if class32 { le32(b, o32) as u64 } else { le64(b, o64) };
    let kind = le16(elf, 16);
    let base = match kind {
        2 => 0,        // ET_EXEC: fixed addresses
        3 => dyn_base, // ET_DYN: PIE program or shared object (the dynamic loader)
        _ => return Err(format!("not an executable (ELF type {kind})")),
    };
    let mut interp = None;
    let phoff = word(elf, 28, 32) as usize;
    let phentsize = le16(elf, if class32 { 42 } else { 54 }) as usize;
    let phnum = le16(elf, if class32 { 44 } else { 56 }) as usize;
    let user_top = u.stack_top() - STACK_SIZE;
    let mut phdr_addr = 0;
    let mut top = 0;
    for i in 0..phnum {
        let p = &elf.get(phoff + i * phentsize..phoff + (i + 1) * phentsize).ok_or("truncated program headers")?;
        let ptype = le32(p, 0);
        let flags = if class32 { le32(p, 24) } else { le32(p, 4) };
        let offset = word(p, 4, 8) as usize;
        let vaddr = word(p, 8, 16) + base;
        let filesz = word(p, 16, 32) as usize;
        let memsz = word(p, 20, 40);
        match ptype {
            1 => {
                // PT_LOAD
                if vaddr < USER_MIN || vaddr + memsz > user_top {
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
    Ok(Loaded { entry: word(elf, 24, 24) + base, phdr: phdr_addr, phnum: phnum as u64, class32, interp })
}

/// Build the initial stack: argc, argv, envp, auxv (System V ABI), return RSP.
fn build_stack(u: &mut Memory, args: &[String], env: &[String], l: &Loaded, interp_base: u64, execfn: &str) -> u64 {
    let top = u.stack_top();
    u.areas.push(Area { start: top - STACK_SIZE, end: top, writable: true });
    let mut sp = top;
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
    let platform = push_bytes(u, &mut sp, &cstr(if l.class32 { "i686" } else { "x86_64" }));
    let mut random = [0u8; 16];
    crate::rand::fill(&mut random);
    let random_ptr = push_bytes(u, &mut sp, &random);
    let argv: Vec<u64> = args.iter().map(|a| push_bytes(u, &mut sp, &cstr(a))).collect();
    let envp: Vec<u64> = env.iter().map(|e| push_bytes(u, &mut sp, &cstr(e))).collect();
    let auxv: [(u64, u64); 17] = [
        (3, l.phdr),
        (4, if l.class32 { 32 } else { 56 }),
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
    let wsize = if l.class32 { 4 } else { 8 };
    sp &= !15;
    if (words * wsize) % 16 != 0 {
        sp -= (16 - (words * wsize) % 16) as u64; // keep the stack pointer 16-byte aligned at entry
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
    let bytes: Vec<u8> = if l.class32 {
        table.iter().flat_map(|w| (*w as u32).to_le_bytes()).collect()
    } else {
        table.iter().flat_map(|w| w.to_le_bytes()).collect()
    };
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
    terminate(p, crate::signal::SIGKILL);
}

/// The bytes of program `path` (a system file is used in place), and whether it is one.
fn program_bytes(path: &str) -> Result<(alloc::borrow::Cow<'static, [u8]>, bool), String> {
    // `path` is canonical (relative to the root, as resolved by the caller).
    let ns = task::current_user().map_or_else(crate::vfs::root, |u| u.process.ns.lock().clone());
    let bytes = ns.read_program("", path).map_err(|e| format!("{path}: error {e}"))?;
    let in_place = matches!(bytes, alloc::borrow::Cow::Borrowed(_));
    Ok((bytes, in_place))
}

/// Load program `path` (and its dynamic loader) into `mem` and build its initial stack.
/// Returns (entry point, stack pointer).
fn load_program(mem: &mut Memory, path: &str, elf: &[u8], is_static: bool, args: &[String], env: &[String]) -> Result<(u64, u64), String> {
    if elf.len() > 4 && elf[4] == 1 {
        mem.make_compat(); // ELFCLASS32: a 32-bit program
    }
    let (pie_base, interp_load) = if mem.compat { (PIE_BASE32, INTERP_BASE32) } else { (PIE_BASE, INTERP_BASE) };
    let loaded = load_elf(mem, elf, pie_base, is_static)?;
    // Dynamically linked: load the interpreter too and start there; it maps the
    // shared libraries itself (openat + mmap) and then jumps to AT_ENTRY.
    let (start, interp_base) = match &loaded.interp {
        Some(ip) => {
            let ip = ip.trim_start_matches('/');
            let (ld, in_place) = program_bytes(ip).map_err(|_| format!("{path} needs the dynamic loader /{ip}, which is not installed"))?;
            let l = load_elf(mem, &ld, interp_load, in_place)?;
            (l.entry, interp_load)
        }
        None => (loaded.entry, 0),
    };
    // The heap starts after the program, not after the loader.
    let rsp = build_stack(mem, args, env, &loaded, interp_base, path);
    Ok((start, rsp))
}

const DEFAULT_ENV: [&str; 7] = ["HOME=/", "PATH=/bin:/usr/bin", "TERM=xterm", "LANG=C.UTF-8", "USER=nova", "XDG_RUNTIME_DIR=/run/user/1000", "WAYLAND_DISPLAY=wayland-0"];

fn run_with(path: &str, args: &[String], trace: bool, extra: Vec<(usize, crate::linux::FileRef)>, on_start: impl FnOnce(Arc<Process>)) -> Result<i32, String> {
    if task::current_user().is_some() {
        return Err("already running a program".into());
    }
    let (elf, is_static) = program_bytes(path)?;
    let name: String = path.rsplit('/').next().unwrap_or(path).into();
    let process = Process::new(&name, path, 0);
    process.trace.store(trace, Ordering::Relaxed);
    for (fd, file) in extra {
        process.fds.lock().install(fd, file);
    }
    let env = DEFAULT_ENV.map(String::from);
    let loaded = load_program(&mut process.mem.lock(), path, &elf, is_static, args, &env);
    let (entry, rsp) = match loaded {
        Ok(v) => v,
        Err(e) => {
            finish(&process, wait_status(127, 0));
            return Err(e);
        }
    };
    drop(elf);
    let regs = SyscallFrame { r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r9: 0, r8: 0, r10: 0, rdx: 0, rsi: 0, rdi: 0, rax: 0, rip: entry, rflags: 0x202, rsp };
    Ok(run_main(process, regs, |_| {}, on_start))
}

/// Run the main thread of `process` from `regs` in the current task until the whole
/// process has ended; record its end (finish) and return its exit status (shell style:
/// 128 + signal for a signal). `setup` adjusts the thread before it enters user mode.
fn run_main(process: Arc<Process>, regs: SyscallFrame, setup: impl FnOnce(&mut UserState), on_start: impl FnOnce(Arc<Process>)) -> i32 {
    let mut thread = UserState::new_thread(process.clone(), process.pid);
    thread.kernel_rsp = kernel_stack_here();
    thread.compat = process.mem.lock().compat;
    setup(&mut thread);
    let compat = thread.compat;
    task::set_user(Some(thread));
    let ctx = &raw mut task::current_user().unwrap().exit_ctx;
    on_start(process.clone());
    // SAFETY: the address space is loaded; registers were set up by the caller.
    let code = unsafe { if compat { enter_user32(&regs, ctx) } else { enter_user(&regs, ctx) } } as i32;
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
    finish(&process, wait_status(status, process.term_signal.load(Ordering::SeqCst)));
    status
}

// ---------------------------------------------------------------- fork and exec

/// The calling thread's live %fs/%gs selectors (32-bit threads; 0 for 64-bit ones).
pub fn current_fs_gs(u: &UserState) -> (u16, u16) {
    if !u.compat {
        return (0, 0);
    }
    let (fs, gs): (u16, u16);
    // SAFETY: reading segment selectors has no side effects.
    unsafe { asm!("mov {0:x}, fs", "mov {1:x}, gs", out(reg) fs, out(reg) gs, options(nomem, nostack, preserves_flags)) };
    (fs, gs)
}

/// fork (and vfork, and clone without CLONE_VM): a new process with a copy of the
/// caller's memory and open files, whose one thread resumes after the system call with
/// rax = 0. With `vfork`, the caller waits until the child execs or exits.
pub fn fork(parent_regs: &SyscallFrame, vfork: bool, child_stack: u64, tls: Option<u64>) -> Result<u64, i64> {
    let me = task::current_user().expect("fork outside a process");
    let parent = me.process.clone();
    let child = Process::new(&parent.name, &parent.exe.lock(), parent.pid);
    child.trace.store(parent.trace.load(Ordering::Relaxed), Ordering::Relaxed);
    *child.ns.lock() = parent.ns.lock().clone();
    {
        let src = parent.mem.lock();
        let mut dst = child.mem.lock();
        if !src.fork_into(&mut dst) {
            drop(dst);
            TABLE.lock().remove(&child.pid);
            return Err(12); // ENOMEM
        }
    }
    *child.fds.lock() = parent.fds.lock().fork();
    {
        let from = parent.signals.lock();
        child.signals.lock().actions = from.actions;
    }
    let mut regs = *parent_regs;
    regs.rax = 0;
    if child_stack != 0 {
        regs.rsp = child_stack;
    }
    // The child starts with the parent's SIMD state, FS base and signal mask.
    let fx = FxArea::initial();
    // SAFETY: the area is sized for xsave_size() (or 512) bytes and 64-byte aligned.
    unsafe { save_fpu(fx.ptr) };
    let (fs, sigmask, altstack) = (tls.unwrap_or(me.fs_base), me.sigmask, me.altstack);
    let (compat, tls32, sel) = (me.compat, me.tls, current_fs_gs(me));
    let pid = child.pid;
    let name = format!("{}[{pid}]", child.name);
    let c = child.clone();
    task::spawn_with_tty(&name, task::current_tty(), move || {
        run_main(c, regs, move |t| {
            t.fx = fx;
            t.fs_base = fs;
            t.sigmask = sigmask;
            t.altstack = altstack;
            t.compat = compat;
            t.tls = tls32;
            (t.user_fs, t.user_gs) = sel;
        }, |_| {});
    });
    if vfork {
        while !child.vfork_done.load(Ordering::SeqCst) {
            check_exiting();
            task::wait_event();
        }
    }
    Ok(pid)
}

/// execve: replace the calling process's program. Only the calling thread survives
/// (other threads are asked to exit). On success the frame `f` starts the new program.
pub fn exec(f: &mut SyscallFrame, path: String, args: Vec<String>, env: Vec<String>) -> Result<(), i64> {
    let me = task::current_user().expect("exec outside a process");
    let process = me.process.clone();
    // Scripts: "#!interpreter [arg]" runs the interpreter on the script.
    let (mut path, mut args) = (String::from(path.trim_start_matches('/')), args);
    for _ in 0..4 {
        let (bytes, _) = program_bytes(&path).map_err(|_| 2i64)?; // ENOENT
        if !bytes.starts_with(b"#!") {
            break;
        }
        let line = bytes[2..].split(|&b| b == b'\n').next().unwrap_or(&[]);
        let line = core::str::from_utf8(line).map_err(|_| 8i64)?.trim(); // ENOEXEC
        let mut parts = line.splitn(2, |c: char| c == ' ' || c == '\t');
        let interp = parts.next().filter(|i| !i.is_empty()).ok_or(8i64)?;
        let interp = String::from(interp);
        let mut new_args = alloc::vec![interp.clone()];
        if let Some(arg) = parts.next().map(str::trim).filter(|a| !a.is_empty()) {
            new_args.push(arg.into());
        }
        new_args.push(format!("/{path}"));
        new_args.extend(args.into_iter().skip(1));
        args = new_args;
        path = interp.trim_start_matches('/').into();
    }
    let (elf, is_static) = program_bytes(&path).map_err(|_| 2i64)?;
    if !elf.starts_with(b"\x7fELF") {
        return Err(8); // ENOEXEC
    }
    if process.threads.load(Ordering::SeqCst) > 1 {
        return Err(11); // EAGAIN: multi-threaded execve is not supported yet
    }
    let cr3 = new_address_space();
    let mut mem = Memory::new(cr3);
    let env = if env.is_empty() { DEFAULT_ENV.map(String::from).to_vec() } else { env };
    let (entry, rsp) = load_program(&mut mem, &path, &elf, is_static, &args, &env).map_err(|_| 8i64)?;
    drop(elf);
    // Switch to the new address space, then free the old one.
    let old = core::mem::replace(&mut *process.mem.lock(), mem);
    process.cr3.store(cr3, Ordering::SeqCst);
    // SAFETY: the new tables map the kernel's upper half like every address space.
    unsafe { asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags)) };
    drop(old);
    *process.exe.lock() = path;
    process.fds.lock().close_on_exec();
    process.signals.lock().reset_handlers();
    let u = task::current_user().unwrap();
    u.fs_base = 0;
    wrmsr(IA32_FS_BASE, 0);
    u.altstack = (0, 0, 2);
    u.clear_tid = 0;
    // SAFETY: an initial-state image.
    unsafe { restore_fpu(FxArea::initial().ptr, xsave_size() != 0) };
    *f = SyscallFrame { r15: 0, r14: 0, r13: 0, r12: 0, rbp: 0, rbx: 0, r9: 0, r8: 0, r10: 0, rdx: 0, rsi: 0, rdi: 0, rax: 0, rip: entry, rflags: 0x202, rsp };
    process.vfork_done.store(true, Ordering::SeqCst);
    crate::ipc::notify();
    // The new program may be 32- or 64-bit, whichever the caller was: leave through iretq
    // with the matching code segment instead of the caller's return path.
    let compat = process.mem.lock().compat;
    // resume_user does not return: release what this function owns first (the extra
    // reference to the process above all, which would keep it alive forever).
    drop(args);
    drop(env);
    drop(process);
    let u = task::current_user().unwrap();
    u.compat = compat;
    u.tls = [0; gdt::TLS_ENTRIES];
    (u.user_fs, u.user_gs) = (0, 0);
    load_fs_gs(0, 0);
    let regs = *f;
    // SAFETY: the new address space is loaded and the registers describe its entry.
    unsafe { resume_user(&regs, if compat { gdt::USER_CODE32 } else { gdt::USER_CODE } as u64) }
}

/// Start a new thread of the current process (clone with CLONE_VM | CLONE_THREAD):
/// it resumes after the syscall with rax = 0, on `stack`, with FS base `tls`.
/// clone for a 32-bit thread: like [`spawn_thread`], with the child's TLS descriptors and
/// the parent's %gs selector (`parent` is a 32-bit frame: ecx in the r10 slot).
pub fn spawn_thread32(parent: &SyscallFrame, stack: u64, tls: [u64; gdt::TLS_ENTRIES], clear_tid: u64) -> u64 {
    let me = task::current_user().expect("clone outside a process");
    let process = me.process.clone();
    let tid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let mut regs = SyscallFrame { ..*parent };
    regs.rax = 0;
    if stack != 0 {
        regs.rsp = stack;
    }
    let (fs, gs) = current_fs_gs(me);
    process.threads.fetch_add(1, Ordering::SeqCst);
    let name = format!("{}:{tid}", process.name);
    task::spawn_with_tty(&name, task::current_tty(), move || {
        let mut thread = UserState::new_thread(process.clone(), tid);
        thread.compat = true;
        thread.tls = tls;
        thread.user_fs = fs;
        thread.user_gs = gs;
        thread.clear_tid = clear_tid;
        thread.kernel_rsp = kernel_stack_here();
        task::set_user(Some(thread));
        let ctx = &raw mut task::current_user().unwrap().exit_ctx;
        // SAFETY: as for run(): this task now runs in the process's address space.
        unsafe { enter_user32(&regs, ctx) };
        task::set_user(None);
        process.threads.fetch_sub(1, Ordering::SeqCst);
    });
    tid
}

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
