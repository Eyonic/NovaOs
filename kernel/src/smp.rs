//! Symmetric multiprocessing: the other CPUs, the big kernel lock, IPIs and TLB shootdown.
//!
//! User code runs on every CPU at once; the kernel stays single-threaded. A CPU holds the
//! big kernel lock (BKL) whenever it runs kernel code, so every kernel structure keeps the
//! one-CPU assumptions it was written with ("tasks never run concurrently"):
//!
//! * taken on every entry from user mode (system calls, exceptions, the preemption tick, a
//!   kick) and by an idle CPU that goes looking for work; kernel tasks hold it while they run;
//! * released on the way back to user mode and when a CPU goes idle;
//! * not needed by user mode, the idle halt, and interrupt top halves: timer ticks, the PS/2
//!   and COM1 rings, IPIs. Device interrupts that run driver code (ACPI's SCI, Linux drivers'
//!   handlers) need it. When another CPU holds it, the handler is deferred: the line is
//!   masked if it is level triggered, and the holder is asked (IPI_IRQ) to run the handler,
//!   nested in its kernel code exactly as if the interrupt had arrived there. Whoever
//!   releases the lock runs what is still deferred first, so nothing waits for long.
//!
//! The lock belongs to a CPU, not a task: a task switch inside the kernel keeps it, a task
//! that blocked on one CPU may return to user mode from another.
//!
//! Each CPU has its own GDT and TSS (gdt.rs; the TSS selector identifies the CPU, [`id`]),
//! its own SYSCALL entry stub (process.rs) and LAPIC timer for preemption; device
//! interrupts all go to the boot CPU. See docs/smp.md.

use core::arch::{asm, naked_asm};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::{apic, memory, println};

/// Most CPUs Nova uses (CPU ids are 0..MAX_CPUS; the GDT has a TSS slot for each).
pub const MAX_CPUS: usize = 64;

/// IPI vectors: above the device range (48..=0xEF), below the spurious vector.
pub const IPI_KICK: u8 = 0xF0;
pub const IPI_TLB: u8 = 0xF1;
pub const IPI_IRQ: u8 = 0xF2;
pub const IPI_STOP: u8 = 0xF3;
/// The LAPIC timer of the application processors (the boot CPU ticks with the PIT).
pub const TIMER_VECTOR: u8 = 0xF4;

/// CPUs running Nova (bit n = CPU n). The boot CPU is CPU 0.
static ONLINE: AtomicU64 = AtomicU64::new(1);
static LAPIC_ID: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];
/// Local timer ticks per CPU (CPU 0 counts PIT ticks in pit.rs instead).
static TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// The CPU this code runs on: its TSS selector says (gdt.rs puts CPU n's at 0x80 + 16n).
/// 0 before the boot CPU has loaded its TSS.
#[inline]
pub fn id() -> usize {
    let sel: u16;
    // SAFETY: STR only reads the task register.
    unsafe { asm!("str {0:x}", out(reg) sel, options(nomem, nostack, preserves_flags)) };
    (sel.saturating_sub(crate::gdt::TSS_SELECTOR_BASE) >> 4) as usize
}

/// Number of CPUs running Nova.
pub fn count() -> usize {
    ONLINE.load(Ordering::Relaxed).count_ones() as usize
}

pub fn online_mask() -> u64 {
    ONLINE.load(Ordering::Relaxed)
}

/// The online CPUs in Linux's list format ("0-3"), for /sys/devices/system/cpu/online.
pub fn online_list() -> alloc::string::String {
    let mask = online_mask();
    let mut out = alloc::string::String::new();
    let mut cpu = 0;
    while cpu < MAX_CPUS {
        if mask & (1 << cpu) == 0 {
            cpu += 1;
            continue;
        }
        let start = cpu;
        while cpu + 1 < MAX_CPUS && mask & (1 << (cpu + 1)) != 0 {
            cpu += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        out += &if start == cpu { alloc::format!("{start}") } else { alloc::format!("{start}-{cpu}") };
        cpu += 1;
    }
    out
}

/// Local timer tick on an application processor; returns the CPU's tick count.
pub fn tick(cpu: usize) -> u64 {
    TICKS[cpu].fetch_add(1, Ordering::Relaxed) + 1
}

fn irq_save() -> u64 {
    let flags: u64;
    // SAFETY: save RFLAGS, disable interrupts.
    unsafe { asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem)) };
    flags
}

fn irq_restore(flags: u64) {
    if flags & (1 << 9) != 0 {
        // SAFETY: interrupts were enabled before irq_save.
        unsafe { asm!("sti", options(nomem, nostack)) };
    }
}

// ---------------------------------------------------------------- big kernel lock

/// Owner CPU + 1, 0 when free. The boot CPU holds it from the start.
static BKL: AtomicU32 = AtomicU32::new(1);

/// Statistics (the `cpus` shell command), in TSC cycles: since when the lock is held, how
/// long it was held in all, and how long each CPU waited for it.
static HELD_SINCE: AtomicU64 = AtomicU64::new(0);
static HELD_CYCLES: AtomicU64 = AtomicU64::new(0);
static WAIT_CYCLES: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Cycles each CPU spent halted in its idle task (task.rs adds them).
pub static IDLE_CYCLES: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

pub fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: RDTSC only reads the timestamp counter.
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

/// What the lock holder was doing at timer ticks that interrupted it in the kernel: the
/// system call of its thread (0x1000 + n for i386 calls), 0x2000 = a user thread outside a
/// system call (page faults, preemption), 0x2001 = a kernel task.
static SAMPLES: [AtomicU32; 0x2002] = [const { AtomicU32::new(0) }; 0x2002];

/// A timer tick interrupted kernel code on this CPU: if it holds the lock, note what for.
pub fn sample() {
    if !held() {
        return;
    }
    let bucket = match crate::task::current_user() {
        Some(u) if u.in_syscall != u64::MAX => (u.in_syscall & 0x1FFF) as usize,
        Some(_) => 0x2000,
        None => 0x2001,
    };
    SAMPLES[bucket.min(0x2001)].fetch_add(1, Ordering::Relaxed);
}

/// Per-CPU idle and lock-wait time, how long the lock was held, and what for, since the
/// previous call (the `cpus` shell command).
pub fn stats() -> alloc::string::String {
    static LAST: crate::sync::SpinLock<Option<(u64, u64, [u64; MAX_CPUS], [u64; MAX_CPUS])>> = crate::sync::SpinLock::new(None);
    let now = rdtsc();
    let held = HELD_CYCLES.load(Ordering::Relaxed) + now.wrapping_sub(HELD_SINCE.load(Ordering::Relaxed));
    let idle: [u64; MAX_CPUS] = core::array::from_fn(|c| IDLE_CYCLES[c].load(Ordering::Relaxed));
    let wait: [u64; MAX_CPUS] = core::array::from_fn(|c| WAIT_CYCLES[c].load(Ordering::Relaxed));
    let mut last = LAST.lock();
    let (t0, h0, i0, w0) = last.unwrap_or((0, 0, [0; MAX_CPUS], [0; MAX_CPUS]));
    *last = Some((now, held, idle, wait));
    let total = now.wrapping_sub(t0).max(1);
    let pct = |v: u64| v * 100 / total;
    let mut out = alloc::format!("big kernel lock held {}% of the time\n", pct(held - h0));
    for c in (0..MAX_CPUS).filter(|c| online_mask() & (1 << c) != 0) {
        out += &alloc::format!("cpu {c}: idle {:>3}%  waiting for the lock {:>3}%\n", pct(idle[c] - i0[c]), pct(wait[c] - w0[c]));
    }
    let mut top: alloc::vec::Vec<(u32, usize)> = (0..SAMPLES.len()).map(|i| (SAMPLES[i].swap(0, Ordering::Relaxed), i)).filter(|s| s.0 != 0).collect();
    top.sort_unstable_by(|a, b| b.cmp(a));
    let n: u32 = top.iter().map(|s| s.0).sum();
    if n != 0 {
        out += "lock held for (timer samples):";
        for &(k, i) in top.iter().take(10) {
            let what = match i {
                0x2000 => "user fault/preempt".into(),
                0x2001 => "kernel tasks".into(),
                i if i >= 0x1000 => alloc::format!("i386 {}", crate::linux::compat32::label(i as u64 - 0x1000)),
                i => crate::linux::syscall_label(i as u64),
            };
            out += &alloc::format!(" {what} {}%", k * 100 / n);
        }
        out += "\n";
    }
    out
}

/// Whether this CPU holds the big kernel lock.
#[inline]
pub fn held() -> bool {
    BKL.load(Ordering::Relaxed) == id() as u32 + 1
}

fn try_take(me: u32) -> bool {
    BKL.load(Ordering::Relaxed) == 0 && BKL.compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed).is_ok()
}

/// Take the big kernel lock (a no-op if this CPU holds it). While waiting with interrupts
/// disabled, TLB shootdowns are still served (the holder may be waiting for this CPU).
pub fn lock() {
    let me = id() as u32 + 1;
    if BKL.load(Ordering::Relaxed) == me {
        return;
    }
    if !try_take(me) {
        let t0 = rdtsc();
        WAITING.fetch_add(1, Ordering::AcqRel);
        while !try_take(me) {
            service_tlb();
            core::hint::spin_loop();
        }
        WAITING.fetch_sub(1, Ordering::AcqRel);
        WAIT_CYCLES[me as usize - 1].fetch_add(rdtsc().wrapping_sub(t0), Ordering::Relaxed);
    }
    acquired();
}

/// CPUs spinning in [`lock`].
static WAITING: AtomicUsize = AtomicUsize::new(0);

/// A point where the lock holder may let others in (the scheduler, between tasks): if
/// another CPU waits for the lock, hand it over and queue up behind it. Without this, a
/// CPU running parked tasks one after another (each checks its condition at every
/// interrupt) could keep the lock for good while threads on other CPUs wait to enter the
/// kernel.
pub fn relax() {
    if WAITING.load(Ordering::Acquire) == 0 || !held() {
        return;
    }
    unlock();
    // Let a waiter take it before competing again.
    let t0 = rdtsc();
    while BKL.load(Ordering::Acquire) == 0 && WAITING.load(Ordering::Acquire) != 0 && rdtsc().wrapping_sub(t0) < 100_000 {
        service_tlb();
        core::hint::spin_loop();
    }
    lock();
}

/// [`lock`] from an interrupt or exception that arrived in user mode: other interrupts are
/// let in while waiting (nothing in the kernel was interrupted), then disabled again.
pub fn lock_from_user() {
    // SAFETY: the interrupted code is user code; handlers may nest while this CPU waits.
    unsafe { asm!("sti", options(nomem, nostack)) };
    lock();
    // SAFETY: back to the handler's interrupts-off state.
    unsafe { asm!("cli", options(nomem, nostack)) };
}

/// The lock was just taken: kernel state that names "the running task" globally follows
/// this CPU's current task (Linux's `current` in the driver layer).
fn acquired() {
    HELD_SINCE.store(rdtsc(), Ordering::Relaxed);
    #[cfg(feature = "linux-drivers")]
    if let Some(lx) = crate::task::current_lx_checked() {
        crate::linuxkpi::switch_to(lx);
    }
}

/// Release the big kernel lock, after running the device interrupt handlers deferred to
/// it. If one is deferred in the instant of the release, the lock is taken back for it.
pub fn unlock() {
    let me = id() as u32 + 1;
    debug_assert_eq!(BKL.load(Ordering::Relaxed), me, "unlock without the BKL");
    loop {
        run_deferred();
        HELD_CYCLES.fetch_add(rdtsc().wrapping_sub(HELD_SINCE.load(Ordering::Relaxed)), Ordering::Relaxed);
        BKL.store(0, Ordering::Release);
        if !deferred_pending() || !try_take(me) {
            return;
        }
        acquired();
    }
}

// ---------------------------------------------------------------- deferred device interrupts

/// Vectors whose handler waits for the BKL, and those of them masked at the I/O APIC
/// meanwhile (level-triggered lines would fire again at once).
static DEFERRED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static DEFER_MASKED: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

fn deferred_pending() -> bool {
    DEFERRED.iter().any(|w| w.load(Ordering::Acquire) != 0)
}

/// A device interrupt whose handler runs driver code: run it now if this CPU holds or can
/// take the BKL, else hand it to the holder. Interrupt context, interrupts disabled.
pub fn device_irq(vector: u8) {
    let me = id() as u32 + 1;
    if BKL.load(Ordering::Relaxed) == me {
        crate::interrupts::device_handler(vector);
        return;
    }
    if try_take(me) {
        acquired();
        crate::interrupts::device_handler(vector);
        unlock();
        return;
    }
    let (word, bit) = (vector as usize / 64, 1u64 << (vector % 64));
    if apic::mask_vector(vector, true) {
        DEFER_MASKED[word].fetch_or(bit, Ordering::AcqRel);
    }
    DEFERRED[word].fetch_or(bit, Ordering::AcqRel);
    forward();
}

/// Ask the BKL holder to run the deferred handlers (if the lock is free by now, its last
/// holder ran them or this CPU's IPI handler will).
fn forward() {
    let owner = BKL.load(Ordering::Acquire);
    let target = if owner == 0 { id() } else { owner as usize - 1 };
    send(target, IPI_IRQ);
}

/// IPI_IRQ: run deferred handlers if this CPU holds or can take the BKL, else pass it on.
pub fn run_forwarded() {
    let me = id() as u32 + 1;
    if BKL.load(Ordering::Relaxed) == me {
        run_deferred();
    } else if try_take(me) {
        acquired();
        unlock(); // runs them
    } else if deferred_pending() && BKL.load(Ordering::Relaxed) != me {
        forward();
    }
}

/// Run every deferred handler (BKL held), with interrupts disabled as handlers expect.
fn run_deferred() {
    if !deferred_pending() {
        return;
    }
    let flags = irq_save();
    for word in 0..4 {
        let mut bits = DEFERRED[word].swap(0, Ordering::AcqRel);
        while bits != 0 {
            let bit = bits.trailing_zeros();
            bits &= !(1 << bit);
            let vector = (word * 64) as u8 + bit as u8;
            // Unmask first: a driver that disables its line in the handler keeps it so.
            if DEFER_MASKED[word].fetch_and(!(1 << bit), Ordering::AcqRel) & (1 << bit) != 0 {
                apic::mask_vector(vector, false);
            }
            crate::interrupts::device_handler(vector);
        }
    }
    irq_restore(flags);
}

// ---------------------------------------------------------------- IPIs, idle CPUs, kicks

pub fn send(cpu: usize, vector: u8) {
    apic::send_ipi(LAPIC_ID[cpu].load(Ordering::Relaxed), vector);
}

/// CPUs halted in their idle task (bit n = CPU n).
static IDLE: AtomicU64 = AtomicU64::new(0);
static KICKED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

pub fn set_idle(cpu: usize, idle: bool) {
    if idle {
        IDLE.fetch_or(1 << cpu, Ordering::AcqRel);
    } else {
        IDLE.fetch_and(!(1 << cpu), Ordering::AcqRel);
    }
}

pub fn is_idle(cpu: usize) -> bool {
    IDLE.load(Ordering::Relaxed) & (1 << cpu) != 0
}

/// Wake CPU `cpu` (from its idle halt, or into the kernel from user mode).
pub fn kick(cpu: usize) {
    KICKED[cpu].store(true, Ordering::Release);
    send(cpu, IPI_KICK);
}

/// Was this CPU kicked since it last looked?
pub fn take_kick(cpu: usize) -> bool {
    KICKED[cpu].swap(false, Ordering::AcqRel)
}

/// Wake one idle CPU (not this one) to look for work. False if none is idle.
pub fn kick_idle() -> bool {
    let me = id();
    loop {
        let idle = IDLE.load(Ordering::Acquire) & !(1 << me);
        if idle == 0 {
            return false;
        }
        let cpu = idle.trailing_zeros() as usize;
        // Claim it, so the next kick picks another CPU.
        if IDLE.fetch_and(!(1 << cpu), Ordering::AcqRel) & (1 << cpu) != 0 {
            kick(cpu);
            return true;
        }
    }
}

/// Stop every other CPU (panics and fatal exceptions: nothing may run on behind a report).
pub fn stop_others() {
    if count() > 1 {
        apic::send_ipi_others(IPI_STOP);
    }
}

// ---------------------------------------------------------------- TLB shootdown

/// The address space each CPU has loaded (its CR3), for shootdowns.
static LOADED_CR3: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Load page-table root `cr3` on this CPU (BKL held, or this CPU's own early init).
pub fn load_cr3(cr3: u64) {
    LOADED_CR3[id()].store(cr3, Ordering::Release);
    if memory::read_cr3() != cr3 {
        // SAFETY: every address space maps the kernel's upper half identically.
        unsafe { asm!("mov cr3, {}", in(reg) cr3, options(nostack, preserves_flags)) };
    }
}

/// Up to this many pages are invalidated one by one; more flush the whole TLB.
const FLUSH_PAGES: usize = 32;
const FLUSH_ALL: usize = usize::MAX;
const EVICT: usize = usize::MAX - 1;

/// The request being served (one at a time: only the BKL holder sends shootdowns).
static REQ_CR3: AtomicU64 = AtomicU64::new(0);
static REQ_COUNT: AtomicUsize = AtomicUsize::new(0);
static REQ_PAGES: [AtomicU64; FLUSH_PAGES] = [const { AtomicU64::new(0) }; FLUSH_PAGES];
static PENDING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Page-table entries of address space `cr3` changed (unmapped, write-protected or
/// remapped) for `pages`: make every other CPU that has it loaded forget them, and wait.
/// The caller has updated the entries and invalidated its own TLB already; frames that
/// were unmapped may be reused once this returns. BKL held.
pub fn flush_user(cr3: u64, pages: &[u64]) {
    if count() > 1 && !pages.is_empty() {
        shoot(cr3, if pages.len() > FLUSH_PAGES { FLUSH_ALL } else { pages.len() }, pages);
    }
}

/// Kernel mappings (upper half, shared by every address space) changed: every other CPU.
#[cfg_attr(not(feature = "linux-drivers"), allow(dead_code))]
pub fn flush_kernel(pages: &[u64]) {
    if count() > 1 && !pages.is_empty() {
        shoot(0, if pages.len() > FLUSH_PAGES { FLUSH_ALL } else { pages.len() }, pages);
    }
}

/// Address space `cr3` is about to be freed: any CPU that still has it loaded switches to
/// the kernel's (it could otherwise walk freed page tables speculatively).
pub fn evict(cr3: u64) {
    if count() > 1 {
        shoot(cr3, EVICT, &[]);
    }
}

fn shoot(cr3: u64, n: usize, pages: &[u64]) {
    debug_assert!(held(), "TLB shootdown without the BKL");
    let me = id();
    let online = online_mask();
    let targets: u64 = (0..MAX_CPUS)
        .filter(|&c| c != me && online & (1 << c) != 0 && (cr3 == 0 || LOADED_CR3[c].load(Ordering::Acquire) == cr3))
        .fold(0, |m, c| m | 1 << c);
    if targets == 0 {
        return;
    }
    REQ_CR3.store(cr3, Ordering::Relaxed);
    REQ_COUNT.store(n, Ordering::Relaxed);
    if n <= FLUSH_PAGES {
        for (slot, &va) in REQ_PAGES.iter().zip(pages) {
            slot.store(va, Ordering::Relaxed);
        }
    }
    for c in (0..MAX_CPUS).filter(|c| targets & (1 << c) != 0) {
        PENDING[c].store(true, Ordering::Release);
        send(c, IPI_TLB);
    }
    let start = crate::pit::nanos();
    let mut warned = false;
    for c in (0..MAX_CPUS).filter(|c| targets & (1 << c) != 0) {
        while PENDING[c].load(Ordering::Acquire) {
            core::hint::spin_loop();
            if !warned && crate::pit::nanos() - start > 1_000_000_000 {
                warned = true;
                println!("[nova] smp: CPU {c} does not answer a TLB shootdown");
            }
        }
    }
}

/// Serve a shootdown request for this CPU, if one is pending (IPI_TLB, or polled while
/// waiting for the BKL).
pub fn service_tlb() {
    let me = id();
    if !PENDING[me].load(Ordering::Acquire) {
        return;
    }
    let (cr3, n) = (REQ_CR3.load(Ordering::Relaxed), REQ_COUNT.load(Ordering::Relaxed));
    match n {
        EVICT => {
            if LOADED_CR3[me].load(Ordering::Relaxed) == cr3 {
                let kernel = memory::kernel_cr3();
                LOADED_CR3[me].store(kernel, Ordering::Release);
                // SAFETY: the kernel's address space maps everything the kernel uses.
                unsafe { asm!("mov cr3, {}", in(reg) kernel, options(nostack, preserves_flags)) };
            }
        }
        FLUSH_ALL if cr3 != 0 => {
            // SAFETY: reloading CR3 drops every non-global translation.
            unsafe { asm!("mov {0}, cr3", "mov cr3, {0}", out(reg) _, options(nostack, preserves_flags)) };
        }
        FLUSH_ALL => {
            // Kernel mappings may be global: toggling CR4.PGE drops those too.
            // SAFETY: PGE is set again at once; nothing else changes.
            unsafe { asm!("mov {0}, cr4", "xor {0}, 0x80", "mov cr4, {0}", "xor {0}, 0x80", "mov cr4, {0}", out(reg) _, options(nostack, preserves_flags)) };
        }
        _ => {
            for slot in &REQ_PAGES[..n.min(FLUSH_PAGES)] {
                // SAFETY: INVLPG only drops a translation.
                unsafe { asm!("invlpg [{}]", in(reg) slot.load(Ordering::Relaxed), options(nostack, preserves_flags)) };
            }
        }
    }
    PENDING[me].store(false, Ordering::Release);
}

// ---------------------------------------------------------------- bringing up the APs

/// Control registers and MSRs of the boot CPU that the APs copy (paging, NX, SSE/AVX
/// enables, memory types).
static BSP_CR0: AtomicU64 = AtomicU64::new(0);
static BSP_CR4: AtomicU64 = AtomicU64::new(0);
static BSP_EFER: AtomicU64 = AtomicU64::new(0);
static BSP_PAT: AtomicU64 = AtomicU64::new(0);
const IA32_EFER: u32 = 0xC000_0080;
const IA32_PAT: u32 = 0x277;

/// Each AP's initial stack (its idle task's), indexed by CPU id; read by `ap_entry`.
static AP_STACK: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Set by an AP that finished its init.
static AP_READY: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// How many CPUs to use: `nova.cpus=N` on the kernel command line (1 = only the boot CPU,
/// as before SMP), else all of them.
fn wanted() -> usize {
    crate::setup::cmdline_flag("nova.cpus").and_then(|v| v.parse::<usize>().ok()).unwrap_or(MAX_CPUS).clamp(1, MAX_CPUS)
}

/// Start the application processors Limine found. Needs the local APIC (apic::init) and a
/// calibrated TSC. Returns a line for the boot log.
pub fn start_aps() -> alloc::string::String {
    let Some(mp) = crate::limine::MP.get() else { return "one CPU (no MP response from the bootloader)".into() };
    LAPIC_ID[0].store(mp.bsp_lapic_id, Ordering::Relaxed);
    let total = mp.cpus().count();
    let limit = wanted();
    let mut started = 0;
    if apic::active() && limit > 1 {
        apic::calibrate_timer();
        // SAFETY: reading control registers and architectural MSRs.
        unsafe {
            let (cr0, cr4): (u64, u64);
            asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
            asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
            BSP_CR0.store(cr0, Ordering::Relaxed);
            BSP_CR4.store(cr4, Ordering::Relaxed);
        }
        BSP_EFER.store(crate::process::rdmsr(IA32_EFER), Ordering::Relaxed);
        BSP_PAT.store(crate::process::rdmsr(IA32_PAT), Ordering::Relaxed);
    }
    let mut next = 1;
    for info in mp.cpus().filter(|c| c.lapic_id != mp.bsp_lapic_id) {
        if !apic::active() || next >= limit {
            // Unused: halt it for good instead of leaving it spinning in the bootloader.
            info.goto_address.store(ap_park as *const () as u64, Ordering::SeqCst);
            continue;
        }
        let cpu = next;
        next += 1;
        LAPIC_ID[cpu].store(info.lapic_id, Ordering::Relaxed);
        AP_STACK[cpu].store(crate::task::add_idle(cpu), Ordering::Relaxed);
        info.extra_argument.store(cpu as u64, Ordering::SeqCst);
        info.goto_address.store(ap_entry as *const () as u64, Ordering::SeqCst);
    }
    // Wait for them (each takes well under a millisecond on real hardware and under KVM).
    let deadline = crate::pit::nanos() + 2_000_000_000;
    for cpu in 1..next {
        while !AP_READY[cpu].load(Ordering::Acquire) && crate::pit::nanos() < deadline {
            core::hint::spin_loop();
        }
        if AP_READY[cpu].load(Ordering::Acquire) {
            ONLINE.fetch_or(1 << cpu, Ordering::AcqRel);
            started += 1;
        } else {
            println!("[nova] smp: CPU {cpu} (LAPIC {}) did not start", LAPIC_ID[cpu].load(Ordering::Relaxed));
        }
    }
    let why = if !apic::active() {
        " (no local APIC: one CPU)"
    } else if limit < total {
        " (nova.cpus)"
    } else {
        ""
    };
    alloc::format!("{} of {total} CPUs online{why}", started + 1)
}

/// Where Limine sends an AP: switch to the stack Nova prepared for it and continue in Rust.
/// RDI = its `limine::MpInfo`, whose extra argument is the CPU id.
#[unsafe(naked)]
unsafe extern "C" fn ap_entry(info: *const crate::limine::MpInfo) -> ! {
    naked_asm!(
        "cli",
        "mov rsi, [rdi + 24]",
        "lea rax, [rip + {stacks}]",
        "mov rsp, [rax + rsi * 8]",
        "xor ebp, ebp",
        "call {main}",
        "2:",
        "cli",
        "hlt",
        "jmp 2b",
        stacks = sym AP_STACK,
        main = sym ap_main,
    )
}

/// An AP Nova does not use.
#[unsafe(naked)]
unsafe extern "C" fn ap_park(info: *const crate::limine::MpInfo) -> ! {
    naked_asm!("2:", "cli", "hlt", "jmp 2b")
}

extern "C" fn ap_main(_info: &'static crate::limine::MpInfo, cpu: usize) -> ! {
    // The boot CPU's control state first: paging with NX and global pages, write
    // protection, the SSE/XSAVE enables and the memory types.
    // SAFETY: the values the boot CPU runs with; the page tables are the kernel's own.
    unsafe {
        asm!("mov cr0, {}", in(reg) BSP_CR0.load(Ordering::Relaxed), options(nostack, preserves_flags));
        asm!("mov cr4, {}", in(reg) BSP_CR4.load(Ordering::Relaxed), options(nostack, preserves_flags));
    }
    crate::process::wrmsr(IA32_EFER, BSP_EFER.load(Ordering::Relaxed));
    crate::process::wrmsr(IA32_PAT, BSP_PAT.load(Ordering::Relaxed));
    let kernel = memory::kernel_cr3();
    LOADED_CR3[cpu].store(kernel, Ordering::Relaxed);
    // SAFETY: the kernel's page tables, which Limine's (the ones the AP starts on) are.
    unsafe { asm!("mov cr3, {}", in(reg) kernel, options(nostack, preserves_flags)) };
    crate::gdt::init_cpu(cpu);
    crate::interrupts::load_idt();
    crate::process::init_cpu(cpu);
    apic::init_ap();
    apic::start_timer(TIMER_VECTOR);
    AP_READY[cpu].store(true, Ordering::Release);
    crate::task::run_idle(cpu)
}
