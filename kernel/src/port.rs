//! x86 port I/O.
//!
//! SAFETY (all functions): port I/O has hardware side effects. Callers must only
//! touch ports of devices the kernel owns: COM1, the 8259 PICs, the PIT, the 8042
//! PS/2 controller, the ACPI PM control port used for power-off, and QEMU's fw_cfg.

use core::arch::asm;

pub unsafe fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) }
}

pub unsafe fn outw(port: u16, value: u16) {
    unsafe { asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags)) }
}

pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags)) }
    value
}

/// A short delay for old devices (8259) that need time between commands:
/// a write to the unused POST diagnostic port 0x80.
pub unsafe fn io_wait() {
    unsafe { outb(0x80, 0) }
}
