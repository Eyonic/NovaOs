//! Project Nova kernel entry point.

#![no_std]
#![no_main]

mod limine;
mod serial;

use core::arch::asm;
use core::panic::PanicInfo;

/// Startup banner printed on the serial console. scripts/test.sh looks for it.
const BANNER: &str = "PROJECT NOVA KERNEL 0.1 - IDE TEST PASSED";

/// Entry point; Limine jumps here in 64-bit long mode with paging enabled.
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

    println!("[nova] status: kernel main path reached, init OK");

    #[cfg(feature = "panic-test")]
    panic!("intentional panic requested by the panic-test feature");

    #[allow(unreachable_code)]
    {
        println!("[nova] entering idle loop");
        halt_forever()
    }
}

/// Stop the CPU with interrupts disabled. `hlt` may wake on NMI/SMI, hence the loop.
fn halt_forever() -> ! {
    loop {
        // SAFETY: cli/hlt only affect this CPU's execution state.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!();
    println!("!!! KERNEL PANIC !!!");
    println!("{info}");
    halt_forever()
}
