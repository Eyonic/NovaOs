//! Reboot and power-off.

use core::arch::asm;

use crate::{port, println};

pub fn reboot() -> ! {
    println!("[nova] rebooting");
    // The firmware's reset register (FADT) first: the modern way, and on PCs without
    // an 8042. Then the 8042, then a triple fault.
    crate::acpi::reboot();
    // SAFETY: 8042 command 0xFE pulses the CPU reset line.
    unsafe { port::outb(0x64, 0xFE) };
    // If that did not work, force a triple fault: load an empty IDT and raise an exception.
    let empty = [0u16; 5];
    // SAFETY: deliberately resets the machine.
    unsafe { asm!("lidt [{}]", "int3", in(reg) &empty, options(noreturn)) }
}

/// Power off through ACPI (S5); returns if that did not work.
pub fn shutdown() {
    println!("[nova] powering off");
    for d in crate::storage::list() {
        let _ = crate::storage::with_disk(d.index, |disk| disk.dev.flush());
    }
    // ACPI S5 with the firmware's own sleep values (\_S5, _PTS), through uACPI.
    crate::acpi::power_off();
    // Without ACPI: QEMU's q35 (ICH9) PM1a control register is at 0x604 with OVMF's PM
    // base 0x600, where SLP_TYP=0 with SLP_EN (bit 13) enters S5.
    // SAFETY: ACPI PM1a control write; at worst it does nothing.
    unsafe { port::outw(0x604, 0x2000) };
    println!("shutdown: not supported on this machine");
}
