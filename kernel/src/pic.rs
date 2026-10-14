//! Legacy 8259 PIC pair, remapped to vectors 32-47.
//!
//! Limine leaves both PICs fully masked and the I/O APIC entries masked. The local
//! APIC state is whatever the firmware left (Limine base revision 3), and it may
//! block the PIC's output (LINT0 masked). Nova therefore hardware-disables the
//! local APIC (IA32_APIC_BASE bit 11), which routes the PIC's INTR line straight
//! to the CPU as on a pre-APIC PC. Single CPU only, which is all Nova supports.
//!
//! References: Intel 8259A datasheet (ICW1-4, OCW2/3); Intel SDM Vol. 3A,
//! "Advanced Programmable Interrupt Controller (APIC)", "Enabling or Disabling the
//! Local APIC".

use core::arch::asm;

use crate::port::{inb, io_wait, outb};

const MASTER_CMD: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_CMD: u16 = 0xA0;
const SLAVE_DATA: u16 = 0xA1;

/// IDT vector of IRQ 0. IRQs 0-15 use vectors 32-47, just above the CPU exceptions.
pub const IRQ_BASE: u8 = 32;
pub const IRQ_TIMER: u8 = 0;
pub const IRQ_KEYBOARD: u8 = 1;
pub const IRQ_COM1: u8 = 4;
pub const IRQ_MOUSE: u8 = 12;

const IA32_APIC_BASE: u32 = 0x1B;

pub fn init() {
    disable_local_apic();
    // SAFETY: the standard 8259 initialisation sequence on the PIC ports.
    unsafe {
        outb(MASTER_CMD, 0x11); // ICW1: init, ICW4 follows (edge triggered, cascade)
        io_wait();
        outb(SLAVE_CMD, 0x11);
        io_wait();
        outb(MASTER_DATA, IRQ_BASE); // ICW2: vector offsets
        io_wait();
        outb(SLAVE_DATA, IRQ_BASE + 8);
        io_wait();
        outb(MASTER_DATA, 0x04); // ICW3: slave on IRQ 2
        io_wait();
        outb(SLAVE_DATA, 0x02); //        slave cascade identity 2
        io_wait();
        outb(MASTER_DATA, 0x01); // ICW4: 8086 mode
        io_wait();
        outb(SLAVE_DATA, 0x01);
        io_wait();
        // OCW1: unmask only the lines Nova handles. Everything else stays masked.
        // IRQ 2 is the cascade from the slave, which carries IRQ 12 (mouse).
        let enabled: u16 = (1 << IRQ_TIMER) | (1 << IRQ_KEYBOARD) | (1 << 2) | (1 << IRQ_COM1) | (1 << IRQ_MOUSE);
        outb(MASTER_DATA, !(enabled as u8));
        outb(SLAVE_DATA, !((enabled >> 8) as u8));
    }
}

fn disable_local_apic() {
    // CPUID.01H:EDX bit 9 = the CPU has a local APIC (and therefore the MSR).
    let edx: u32;
    // SAFETY: CPUID leaf 1 is always available on x86-64. RBX is reserved by LLVM, so it is saved by hand.
    unsafe {
        asm!("mov {tmp}, rbx", "cpuid", "mov rbx, {tmp}", tmp = out(reg) _,
             inout("eax") 1 => _, out("ecx") _, out("edx") edx, options(nostack, preserves_flags));
    }
    if edx & (1 << 9) == 0 {
        return;
    }
    // SAFETY: clearing EN (bit 11) and EXTD (bit 10, x2APIC) together is a valid
    // transition from either xAPIC or x2APIC mode to disabled.
    unsafe {
        let (low, high): (u32, u32);
        asm!("rdmsr", in("ecx") IA32_APIC_BASE, out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags));
        let low = low & !((1 << 11) | (1 << 10));
        asm!("wrmsr", in("ecx") IA32_APIC_BASE, in("eax") low, in("edx") high, options(nomem, nostack, preserves_flags));
    }
}

/// Returns false for a spurious IRQ 7/15 (the PIC raised the line, then withdrew
/// the request), which must be ignored and, for IRQ 7, not acknowledged.
pub fn is_genuine(irq: u8) -> bool {
    if irq != 7 && irq != 15 {
        return true;
    }
    // IRQ 7 is line 7 of the master, IRQ 15 line 7 of the slave.
    let cmd = if irq == 7 { MASTER_CMD } else { SLAVE_CMD };
    // SAFETY: OCW3 "read ISR", then read it back.
    let isr = unsafe {
        outb(cmd, 0x0B);
        inb(cmd)
    };
    if isr & 0x80 != 0 {
        return true;
    }
    if irq == 15 {
        // The master did see a (cascade) interrupt on IRQ 2 and needs its EOI.
        // SAFETY: non-specific EOI to the master.
        unsafe { outb(MASTER_CMD, 0x20) };
    }
    false
}

/// Let `irq` (0-15) through to the CPU (for a slave line, the cascade is already open).
pub fn unmask(irq: u8) {
    let port = if irq < 8 { MASTER_DATA } else { SLAVE_DATA };
    // SAFETY: OCW1 read-modify-write of the interrupt mask.
    unsafe { outb(port, inb(port) & !(1 << (irq & 7))) };
}

/// Block `irq` (0-15) at the PIC.
pub fn mask(irq: u8) {
    let port = if irq < 8 { MASTER_DATA } else { SLAVE_DATA };
    // SAFETY: OCW1 read-modify-write of the interrupt mask.
    unsafe { outb(port, inb(port) | 1 << (irq & 7)) };
}

/// Mask every line (the I/O APIC takes over; the PICs stay remapped away from the
/// exception vectors, so a stray PIC interrupt cannot look like a CPU exception).
pub fn mask_all() {
    // SAFETY: OCW1 on both PICs.
    unsafe {
        outb(MASTER_DATA, 0xFF);
        outb(SLAVE_DATA, 0xFF);
    }
}

/// Acknowledge `irq` so the PIC can deliver the next one.
pub fn end_of_interrupt(irq: u8) {
    // SAFETY: non-specific EOI (OCW2) to the PIC(s) that delivered the IRQ.
    unsafe {
        if irq >= 8 {
            outb(SLAVE_CMD, 0x20);
        }
        outb(MASTER_CMD, 0x20);
    }
}
