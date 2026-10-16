//! Local APIC and I/O APIC: interrupt delivery on real PCs (and on QEMU's q35).
//!
//! [`init`] takes over from the 8259 PIC when ACPI's MADT describes an I/O APIC: the
//! PICs are masked, the local APIC is enabled, and the ISA interrupts Nova uses (timer,
//! keyboard, COM1, mouse) are routed through the I/O APIC to the same vectors as before
//! (32 + IRQ), honouring the MADT's interrupt source overrides (the PIT is usually GSI 2).
//! Vectors 48-239 are handed out on demand: PCI INTx lines (a GSI, routed level
//! triggered as ACPI's `_PRT` says) and MSI/MSI-X messages. Without an I/O APIC the PIC
//! stays in charge (pic.rs) and only ISA lines work.
//!
//! IRQ numbers given to Linux drivers: a GSI for INTx (as Linux on x86 numbers them),
//! and [`MSI_IRQ_BASE`] + vector for MSI.
//!
//! References: Intel SDM Vol. 3A, chapter "Advanced Programmable Interrupt Controller";
//! Intel 82093AA I/O APIC datasheet; ACPI 6.5 section 5.2.12 (MADT); PCI Local Bus 3.0
//! section 6.8 (MSI).

use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};

use alloc::vec::Vec;

use crate::sync::SpinLock;
use crate::{acpi, memory, pic};

const IA32_APIC_BASE: u32 = 0x1B;
const LAPIC_ID: u32 = 0x20;
const LAPIC_TPR: u32 = 0x80;
const LAPIC_EOI: u32 = 0xB0;
const LAPIC_SVR: u32 = 0xF0;
const LAPIC_ESR: u32 = 0x280;
const LAPIC_ICR_LOW: u32 = 0x300;
const LAPIC_ICR_HIGH: u32 = 0x310;
const LAPIC_LVT_TIMER: u32 = 0x320;
const LAPIC_TIMER_INITIAL: u32 = 0x380;
const LAPIC_TIMER_CURRENT: u32 = 0x390;
const LAPIC_TIMER_DIVIDE: u32 = 0x3E0;
const LAPIC_LVT_LINT0: u32 = 0x350;
const LAPIC_LVT_LINT1: u32 = 0x360;
const LAPIC_LVT_ERROR: u32 = 0x370;
const LVT_MASKED: u32 = 1 << 16;
const LVT_NMI: u32 = 0b100 << 8;

/// Spurious-interrupt vector (never acknowledged with an EOI).
pub const SPURIOUS_VECTOR: u8 = 0xFF;
/// First and last vector handed out for PCI interrupts.
pub const FIRST_DYNAMIC_VECTOR: u8 = 48;
pub const LAST_DYNAMIC_VECTOR: u8 = 0xEF;
/// The i386 system call vector (int 0x80): never handed out to devices.
pub const SYSCALL32_VECTOR: u8 = 0x80;
/// IRQ numbers of MSI vectors start here (above any GSI).
pub const MSI_IRQ_BASE: u32 = 0x1000;

static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The local APIC's register page (every CPU sees its own APIC at the same address).
static LAPIC: AtomicU64 = AtomicU64::new(0);
static LAPIC_PHYS: AtomicU64 = AtomicU64::new(0);
static BSP_ID: AtomicU32 = AtomicU32::new(0);

struct IoApic {
    base: u64,
    first_gsi: u32,
    pins: u32,
}

static IOAPICS: SpinLock<Vec<IoApic>> = SpinLock::new(Vec::new());
/// Interrupt source overrides: (ISA IRQ, GSI, MPS flags).
static OVERRIDES: SpinLock<Vec<(u8, u32, u16)>> = SpinLock::new(Vec::new());

/// What each dynamic vector is: 0 = free, else an IRQ number + 1.
static VECTOR_IRQ: [AtomicU32; 256] = [const { AtomicU32::new(0) }; 256];
/// GSI -> vector, for GSIs routed to dynamic vectors (0 = not routed).
static GSI_VECTOR: [AtomicU16; 256] = [const { AtomicU16::new(0) }; 256];

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

fn lapic_read(reg: u32) -> u32 {
    // SAFETY: the local APIC register page, mapped uncached by init.
    unsafe { ((LAPIC.load(Ordering::Relaxed) + reg as u64) as *const u32).read_volatile() }
}

fn lapic_write(reg: u32, value: u32) {
    // SAFETY: as above.
    unsafe { ((LAPIC.load(Ordering::Relaxed) + reg as u64) as *mut u32).write_volatile(value) }
}

fn ioapic_read(base: u64, reg: u32) -> u32 {
    // SAFETY: IOREGSEL at +0, IOWIN at +0x10, mapped uncached by init. Callers hold
    // IOAPICS (or run before any interrupt), so the select/access pair is not split.
    unsafe {
        (base as *mut u32).write_volatile(reg);
        ((base + 0x10) as *const u32).read_volatile()
    }
}

fn ioapic_write(base: u64, reg: u32, value: u32) {
    // SAFETY: as above.
    unsafe {
        (base as *mut u32).write_volatile(reg);
        ((base + 0x10) as *mut u32).write_volatile(value);
    }
}

fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: IA32_APIC_BASE exists on every CPU with an APIC.
    unsafe { core::arch::asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

fn wrmsr(msr: u32, value: u64) {
    // SAFETY: as above.
    unsafe { core::arch::asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32, options(nomem, nostack, preserves_flags)) };
}

/// Switch from the PIC to the APICs if the MADT describes them. Interrupts must be
/// disabled. Returns a one-line description, or None (the PIC stays in use).
pub fn init() -> Option<alloc::string::String> {
    let madt = acpi::madt()?;
    if madt.io_apics.is_empty() {
        return None;
    }

    // Local APIC: xAPIC mode at the MADT's address. Leaving x2APIC requires passing
    // through "disabled" first (SDM "x2APIC State Transitions").
    let base = rdmsr(IA32_APIC_BASE);
    if base & (1 << 10) != 0 {
        wrmsr(IA32_APIC_BASE, base & !((1 << 11) | (1 << 10)));
    }
    let phys = madt.local_apic & !0xFFF;
    wrmsr(IA32_APIC_BASE, phys | (1 << 11) | (base & (1 << 8)));
    LAPIC_PHYS.store(phys, Ordering::Relaxed);
    LAPIC.store(memory::map_mmio(phys, 0x1000), Ordering::Relaxed);
    lapic_setup(LVT_NMI);
    BSP_ID.store(lapic_read(LAPIC_ID) >> 24, Ordering::Relaxed);

    // I/O APICs: every pin masked until something is routed to it.
    {
        let mut ioapics = IOAPICS.lock();
        for &(_, phys, first_gsi) in &madt.io_apics {
            let base = memory::map_mmio(phys, 0x20);
            let pins = ((ioapic_read(base, 1) >> 16) & 0xFF) + 1;
            for pin in 0..pins {
                ioapic_write(base, 0x10 + 2 * pin, LVT_MASKED);
                ioapic_write(base, 0x11 + 2 * pin, 0);
            }
            ioapics.push(IoApic { base, first_gsi, pins });
        }
    }
    *OVERRIDES.lock() = madt.overrides.clone();
    for irq in 0..16u8 {
        let (gsi, level, _) = isa_gsi(irq);
        ISA_GSI[irq as usize].store(gsi | (level as u32) << 31, Ordering::Relaxed);
    }

    pic::mask_all();
    ACTIVE.store(true, Ordering::Relaxed);
    for irq in [pic::IRQ_TIMER, pic::IRQ_KEYBOARD, pic::IRQ_COM1, pic::IRQ_MOUSE] {
        route_isa(irq, false);
    }
    let gsis: u32 = irqs_off(|| ioapics().iter().map(|a| a.pins).sum());
    Some(alloc::format!(
        "local APIC {} at {phys:#x}, {} I/O APIC(s) with {gsis} inputs, {} override(s)",
        BSP_ID.load(Ordering::Relaxed),
        madt.io_apics.len(),
        madt.overrides.len()
    ))
}

/// This CPU's local APIC registers in a known state: no priority filter, timer and
/// LINT0 masked, `lint1` on LINT1 (NMI on the boot CPU), enabled with the spurious vector.
fn lapic_setup(lint1: u32) {
    lapic_write(LAPIC_TPR, 0);
    lapic_write(LAPIC_LVT_TIMER, LVT_MASKED);
    lapic_write(LAPIC_LVT_LINT0, LVT_MASKED); // the PIC's ExtINT path: unused from now on
    lapic_write(LAPIC_LVT_LINT1, lint1);
    lapic_write(LAPIC_LVT_ERROR, LVT_MASKED);
    lapic_write(LAPIC_ESR, 0);
    lapic_write(LAPIC_SVR, 0x100 | SPURIOUS_VECTOR as u32);
    lapic_write(LAPIC_EOI, 0);
}

/// An application processor's local APIC: xAPIC mode at the boot CPU's address (the
/// firmware may have left it in x2APIC mode), registers as on the boot CPU, LINT1 masked.
pub fn init_ap() {
    let base = rdmsr(IA32_APIC_BASE);
    if base & (1 << 10) != 0 {
        wrmsr(IA32_APIC_BASE, base & !((1 << 11) | (1 << 10)));
    }
    wrmsr(IA32_APIC_BASE, LAPIC_PHYS.load(Ordering::Relaxed) | (1 << 11));
    lapic_setup(LVT_MASKED);
}

/// Send interrupt `vector` to the CPU with local APIC id `dest` (fixed delivery, physical
/// destination).
pub fn send_ipi(dest: u32, vector: u8) {
    irqs_off(|| {
        // xAPIC: the previous IPI must have left before the ICR is written again.
        while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
        lapic_write(LAPIC_ICR_HIGH, dest << 24);
        lapic_write(LAPIC_ICR_LOW, vector as u32 | 1 << 14);
    })
}

/// Send `vector` to every CPU but this one (the "all excluding self" shorthand).
pub fn send_ipi_others(vector: u8) {
    irqs_off(|| {
        while lapic_read(LAPIC_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
        lapic_write(LAPIC_ICR_HIGH, 0);
        lapic_write(LAPIC_ICR_LOW, vector as u32 | 1 << 14 | 0b11 << 18);
    })
}

/// LAPIC timer counts (divide by 16) per PIT tick, measured once on the boot CPU; every
/// CPU's timer runs at the same rate.
static TIMER_PER_TICK: AtomicU32 = AtomicU32::new(0);

/// Measure the LAPIC timer against the TSC (calibrated already) over 10 ms.
pub fn calibrate_timer() {
    lapic_write(LAPIC_TIMER_DIVIDE, 0b0011); // divide by 16
    lapic_write(LAPIC_LVT_TIMER, LVT_MASKED);
    let t0 = crate::pit::nanos();
    lapic_write(LAPIC_TIMER_INITIAL, u32::MAX);
    while crate::pit::nanos() - t0 < 10_000_000 {
        core::hint::spin_loop();
    }
    let elapsed = u32::MAX - lapic_read(LAPIC_TIMER_CURRENT);
    let ns = crate::pit::nanos() - t0;
    lapic_write(LAPIC_TIMER_INITIAL, 0);
    let per_tick = (elapsed as u64 * (1_000_000_000 / crate::pit::HZ) / ns.max(1)) as u32;
    TIMER_PER_TICK.store(per_tick.max(1), Ordering::Relaxed);
}

/// Start this CPU's LAPIC timer: periodic, `vector`, at the PIT's rate.
pub fn start_timer(vector: u8) {
    lapic_write(LAPIC_TIMER_DIVIDE, 0b0011);
    lapic_write(LAPIC_LVT_TIMER, vector as u32 | 1 << 17); // periodic
    lapic_write(LAPIC_TIMER_INITIAL, TIMER_PER_TICK.load(Ordering::Relaxed));
}

/// ISA IRQ -> GSI (bit 31: level triggered), from the overrides, for interrupt context.
static ISA_GSI: [AtomicU32; 16] = [const { AtomicU32::new(0) }; 16];

/// Mask or unmask the I/O APIC line that delivers `vector` (a level-triggered GSI: a PCI
/// INTx line or ACPI's SCI) while its handler is deferred. False for vectors that are not
/// such a line (MSI, edge-triggered ISA lines). Interrupt context.
pub fn mask_vector(vector: u8, masked: bool) -> bool {
    if !active() {
        return false;
    }
    let gsi = if (pic::IRQ_BASE..pic::IRQ_BASE + 16).contains(&vector) {
        let g = ISA_GSI[(vector - pic::IRQ_BASE) as usize].load(Ordering::Relaxed);
        if g & 1 << 31 == 0 {
            return false;
        }
        g & !(1 << 31)
    } else {
        match vector_irq(vector) {
            Some(irq) if irq < MSI_IRQ_BASE => irq,
            _ => return false,
        }
    };
    set_masked(gsi, masked);
    true
}

/// The I/O APIC registers, with interrupts off. I/O APIC programming happens both in
/// tasks and in interrupt handlers (Linux's disable_irq), on any CPU, so IOAPICS is only
/// ever held with interrupts off and for a few register accesses: waiting for it is short.
fn ioapics() -> crate::sync::SpinLockGuard<'static, Vec<IoApic>> {
    loop {
        if let Some(g) = IOAPICS.try_lock() {
            return g;
        }
        core::hint::spin_loop();
    }
}

/// Run `f` with interrupts disabled. I/O APIC programming happens both in tasks and in
/// interrupt handlers (Linux's disable_irq), so IOAPICS is only ever held with interrupts
/// off: a handler on the same CPU can never find it taken.
fn irqs_off<R>(f: impl FnOnce() -> R) -> R {
    let flags: u64;
    // SAFETY: save RFLAGS, disable interrupts; restored below.
    unsafe { core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem)) };
    let r = f();
    if flags & (1 << 9) != 0 {
        // SAFETY: interrupts were enabled on entry.
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    }
    r
}

/// Program the redirection entry for `gsi`. Returns false if no I/O APIC has it.
fn redirect(gsi: u32, vector: u8, level: bool, active_low: bool, masked: bool) -> bool {
    irqs_off(|| redirect_locked(gsi, vector, level, active_low, masked))
}

fn redirect_locked(gsi: u32, vector: u8, level: bool, active_low: bool, masked: bool) -> bool {
    let ioapics = ioapics();
    let Some(a) = ioapics.iter().find(|a| gsi >= a.first_gsi && gsi < a.first_gsi + a.pins) else {
        return false;
    };
    let pin = gsi - a.first_gsi;
    let low = vector as u32 | (active_low as u32) << 13 | (level as u32) << 15 | if masked { LVT_MASKED } else { 0 };
    ioapic_write(a.base, 0x11 + 2 * pin, BSP_ID.load(Ordering::Relaxed) << 24);
    ioapic_write(a.base, 0x10 + 2 * pin, low);
    true
}

fn set_masked(gsi: u32, masked: bool) {
    irqs_off(|| {
        let ioapics = ioapics();
        if let Some(a) = ioapics.iter().find(|a| gsi >= a.first_gsi && gsi < a.first_gsi + a.pins) {
            let reg = 0x10 + 2 * (gsi - a.first_gsi);
            let low = ioapic_read(a.base, reg);
            ioapic_write(a.base, reg, if masked { low | LVT_MASKED } else { low & !LVT_MASKED });
        }
    })
}

/// ISA IRQ `irq` -> (GSI, level, active low), per the overrides (ISA default: same
/// number, edge triggered, active high).
fn isa_gsi(irq: u8) -> (u32, bool, bool) {
    let overrides = OVERRIDES.lock();
    match overrides.iter().find(|o| o.0 == irq) {
        Some(&(_, gsi, flags)) => (gsi, flags >> 2 & 3 == 3, flags & 3 == 3),
        None => (irq as u32, false, false),
    }
}

/// Route ISA IRQ `irq` to vector 32 + irq. `masked`: program it but keep it off.
fn route_isa(irq: u8, masked: bool) {
    let (gsi, level, low) = isa_gsi(irq);
    redirect(gsi, pic::IRQ_BASE + irq, level, low, masked);
}

/// Route ACPI's SCI. In APIC mode the FADT's SCI_INT is a GSI (usually ISA IRQ 9's).
pub fn route_sci(irq: u32) {
    if !active() {
        if irq < 16 {
            pic::unmask(irq as u8);
        }
        return;
    }
    // An ISA line keeps its legacy vector; anything else is a level, active-low GSI.
    match (0..16u8).find(|&i| isa_gsi(i).0 == irq) {
        Some(isa) => route_isa(isa, false),
        None => {
            if let Some(vector) = alloc_vector(irq) {
                redirect(irq, vector, true, true, false);
            }
        }
    }
}

/// Take a free dynamic vector for IRQ number `irq`.
fn alloc_vector(irq: u32) -> Option<u8> {
    (FIRST_DYNAMIC_VECTOR..=LAST_DYNAMIC_VECTOR).filter(|&v| v != SYSCALL32_VECTOR).find(|&v| {
        VECTOR_IRQ[v as usize].compare_exchange(0, irq + 1, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    })
}

/// The IRQ number a dynamic vector was handed out for.
pub fn vector_irq(vector: u8) -> Option<u32> {
    VECTOR_IRQ[vector as usize].load(Ordering::Relaxed).checked_sub(1)
}

/// Route PCI INTx `gsi` (masked until [`unmask_gsi`]); idempotent for shared lines.
pub fn route_gsi(gsi: u32, level: bool, active_low: bool) -> bool {
    if !active() || gsi as usize >= GSI_VECTOR.len() {
        return false;
    }
    if GSI_VECTOR[gsi as usize].load(Ordering::Relaxed) != 0 {
        return true;
    }
    let Some(vector) = alloc_vector(gsi) else { return false };
    GSI_VECTOR[gsi as usize].store(vector as u16, Ordering::Relaxed);
    redirect(gsi, vector, level, active_low, true)
}

pub fn unmask_gsi(gsi: u32) {
    set_masked(gsi, false);
}

pub fn mask_gsi(gsi: u32) {
    set_masked(gsi, true);
}

/// A vector for one MSI/MSI-X message: (IRQ number, message address, message data).
pub fn alloc_msi() -> Option<(u32, u64, u32)> {
    if !active() {
        return None;
    }
    let vector = (FIRST_DYNAMIC_VECTOR..=LAST_DYNAMIC_VECTOR).filter(|&v| v != SYSCALL32_VECTOR).find(|&v| {
        let irq = MSI_IRQ_BASE + v as u32;
        VECTOR_IRQ[v as usize].compare_exchange(0, irq + 1, Ordering::AcqRel, Ordering::Relaxed).is_ok()
    })?;
    // Fixed delivery to the boot CPU, physical destination, edge triggered.
    let address = 0xFEE0_0000 | (BSP_ID.load(Ordering::Relaxed) as u64) << 12;
    Some((MSI_IRQ_BASE + vector as u32, address, vector as u32))
}

/// Give back an MSI vector.
pub fn free_msi(irq: u32) {
    if let Some(v) = irq.checked_sub(MSI_IRQ_BASE).filter(|&v| v < 256) {
        VECTOR_IRQ[v as usize].store(0, Ordering::Release);
    }
}

/// Acknowledge the interrupt being handled.
pub fn eoi() {
    lapic_write(LAPIC_EOI, 0);
}
