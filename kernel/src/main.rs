//! Project Nova kernel entry point.

#![no_std]
#![no_main]

mod bootcell;
mod gdt;
mod interrupts;
mod limine;
mod selftest;
mod serial;

use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

/// Startup banner printed on the serial console. scripts/test.sh looks for it.
const BANNER: &str = "PROJECT NOVA KERNEL 0.2";

/// Entry point; Limine jumps here in 64-bit long mode with paging enabled and IF=0.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    serial::init();

    println!();
    println!("{BANNER}");
    println!("[nova] arch: x86_64 (long mode, higher-half kernel)");

    if !limine::BASE_REVISION.is_supported() {
        panic!("Limine base revision {} not supported by bootloader", limine::REQUESTED_REVISION);
    }
    println!("[nova] boot protocol: Limine base revision {}", limine::REQUESTED_REVISION);
    match limine::BOOTLOADER_INFO.get() {
        Some((name, version)) => println!("[nova] bootloader: {name} {version}"),
        None => println!("[nova] bootloader: (no info response)"),
    }

    gdt::init();
    println!("[nova] gdt: loaded (code 0x08, data 0x10, tss 0x18, #DF on IST1)");
    interrupts::init();
    println!("[nova] idt: loaded, {} exception vectors installed", interrupts::installed_vectors());

    let cmdline = limine::EXECUTABLE_CMDLINE.get().unwrap_or("");
    println!("[nova] cmdline: \"{cmdline}\"");
    println!("[nova] status: kernel main path reached, init OK");

    selftest::run(cmdline);

    println!("[nova] entering idle loop");
    halt_forever()
}

/// Stop the CPU with interrupts disabled. `hlt` may wake on NMI/SMI, hence the loop.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: cli/hlt only affect this CPU's execution state.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

static PANICKING: AtomicBool = AtomicBool::new(false);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    if PANICKING.swap(true, Ordering::SeqCst) {
        // Panicked while reporting a panic: don't recurse into formatting again.
        print!("\n!!! NESTED PANIC, halting\n");
        halt_forever()
    }
    println!();
    println!("!!! KERNEL PANIC !!!");
    println!("{info}");
    println!("[nova] system halted: kernel panic");
    halt_forever()
}
