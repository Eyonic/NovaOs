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
const LAPIC_LVT_TIMER: u32 = 0x320;
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
/// IRQ numbers of MSI vectors start here (above any GSI).
pub const MSI_IRQ_BASE: u32 = 0x1000;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static LAPIC: AtomicU64 = AtomicU64::new(0);
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
    LAPIC.store(memory::map_mmio(phys, 0x1000), Ordering::Relaxed);
    lapic_write(LAPIC_TPR, 0);
    lapic_write(LAPIC_LVT_TIMER, LVT_MASKED);
    lapic_write(LAPIC_LVT_LINT0, LVT_MASKED); // the PIC's ExtINT path: unused from now on
    lapic_write(LAPIC_LVT_LINT1, LVT_NMI);
    lapic_write(LAPIC_LVT_ERROR, LVT_MASKED);
    lapic_write(LAPIC_ESR, 0);
    lapic_write(LAPIC_SVR, 0x100 | SPURIOUS_VECTOR as u32);
    lapic_write(LAPIC_EOI, 0);
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

    pic::mask_all();
    ACTIVE.store(true, Ordering::Relaxed);
    for irq in [pic::IRQ_TIMER, pic::IRQ_KEYBOARD, pic::IRQ_COM1, pic::IRQ_MOUSE] {
        route_isa(irq, false);
    }
    let gsis: u32 = irqs_off(|| IOAPICS.try_lock().map_or(0, |a| a.iter().map(|a| a.pins).sum()));
    Some(alloc::format!(
        "local APIC {} at {phys:#x}, {} I/O APIC(s) with {gsis} inputs, {} override(s)",
        BSP_ID.load(Ordering::Relaxed),
        madt.io_apics.len(),
        madt.overrides.len()
    ))
}

/// Run `f` with interrupts disabled. I/O APIC programming happens both in tasks and in
/// interrupt handlers (Linux's disable_irq), so IOAPICS is only ever held with interrupts
/// off: a handler can never find it taken.
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
    let Some(ioapics) = IOAPICS.try_lock() else { return false };
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
        let Some(ioapics) = IOAPICS.try_lock() else { return };
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
    (FIRST_DYNAMIC_VECTOR..=LAST_DYNAMIC_VECTOR).find(|&v| {
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
    let vector = (FIRST_DYNAMIC_VECTOR..=LAST_DYNAMIC_VECTOR).find(|&v| {
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
