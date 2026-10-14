//! Reboot and power-off.

use core::arch::asm;

use crate::{port, println};

pub fn reboot() -> ! {
    println!("[nova] rebooting");
    // SAFETY: 8042 command 0xFE pulses the CPU reset line.
    unsafe { port::outb(0x64, 0xFE) };
    // If that did not work, force a triple fault: load an empty IDT and raise an exception.
    let empty = [0u16; 5];
    // SAFETY: deliberately resets the machine.
    unsafe { asm!("lidt [{}]", "int3", in(reg) &empty, options(noreturn)) }
}

/// Power off. Only works on QEMU's q35 machine (see below); returns if it did not.
pub fn shutdown() {
    println!("[nova] powering off");
    for d in crate::storage::list() {
        let _ = crate::storage::with_disk(d.index, |disk| disk.dev.flush());
    }
    // QEMU's q35 (ICH9) PM1a control register is at 0x604 with OVMF's PM base 0x600:
    // SLP_TYP=0 with SLP_EN (bit 13) enters S5. Real hardware needs the values from
    // the ACPI tables.
    // SAFETY: ACPI PM1a control write; at worst it does nothing.
    unsafe { port::outw(0x604, 0x2000) };
    println!("shutdown: not supported on this machine");
}
