//! Project Nova kernel entry point.

#![no_std]
#![no_main]

extern crate alloc;

mod acpi;
mod apic;
mod apps;
mod bootcell;
mod browser;
mod claude;
mod console;
mod desktop;
mod cpu;
mod drivers;
mod editor;
mod files;
mod font;
mod fs;
mod fwcfg;
mod gdt;
mod gfx;
mod home;
mod icons;
mod http;
mod imagegen;
mod input;
mod install;
mod ipc;
mod interrupts;
mod keyboard;
mod limine;
mod linux;
#[cfg(feature = "linux-drivers")]
mod linuxkpi;
mod lineedit;
mod log;
mod memory;
mod monitor;
mod mouse;
mod net;
mod pci;
mod pic;
mod pit;
mod pulse;
mod port;
mod power;
mod process;
mod rand;
mod remote;
mod rtc;
mod selftest;
mod serial;
mod settings;
mod setup;
mod shell;
mod shields;
mod signal;
mod storage;
mod sync;
mod task;
mod textgrid;
mod tls;
mod tty;
mod ui;
mod wayland;
mod vfs;
mod wm;

use core::arch::asm;
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

/// Startup banner printed on the serial console. scripts/test.sh looks for it.
const BANNER: &str = "PROJECT NOVA KERNEL 0.4";

/// Entry point; Limine jumps here in 64-bit long mode with paging enabled and IF=0.
#[unsafe(no_mangle)]
extern "C" fn kmain() -> ! {
    serial::init();
    // The heap first (the console needs it), then the console before anything is
    // printed, so the whole boot log is on screen too.
    let memory = memory::init();
    let console = console::init();

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

    match console {
        Ok((cols, rows)) => println!("[nova] console: framebuffer, {cols}x{rows} characters"),
        Err(why) => println!("[nova] console: serial only ({why})"),
    }
    match memory {
        Ok((base, size)) => println!(
            "[nova] memory: heap {} MiB in {} regions (largest {} MiB at physical {base:#x}), direct map at {:#x}",
            memory::heap_size() >> 20,
            memory::heap_regions(),
            size >> 20,
            memory::hhdm_offset()
        ),
        Err(why) => panic!("memory init failed: {why}"),
    }
    rand::init();
    task::init();
    if !limine::STACK_SIZE.granted() {
        println!("[nova] warning: bootloader ignored the {} KiB stack request", limine::KERNEL_STACK_SIZE / 1024);
    }

    gdt::init();
    println!("[nova] gdt: loaded (kernel 0x08/0x10, user 0x2b/0x23, user32 0x1b, tss 0x30, #DF on IST1)");
    interrupts::init();
    process::init();
    let (exceptions, irqs) = interrupts::installed_vectors();
    println!("[nova] idt: loaded, {exceptions} exception vectors + {irqs} IRQ vectors installed");

    let cmdline = limine::EXECUTABLE_CMDLINE.get().unwrap_or("");
    println!("[nova] cmdline: \"{cmdline}\"");
    println!("[nova] status: kernel main path reached, init OK");

    selftest::run(cmdline);

    // ACPI tables first: the MADT says whether an I/O APIC replaces the 8259 PICs.
    if acpi::init_tables() {
        println!("[nova] acpi: tables found (uACPI {})", env!("NOVA_UACPI_VERSION"));
        if let Some((first, last)) = pci::init_ecam(&acpi::mcfg()) {
            println!("[nova] pci: PCIe ECAM for buses {first}-{last} (4 KiB configuration space)");
        }
    } else {
        println!("[nova] acpi: no tables, legacy PC devices only");
    }
    pic::init();
    match apic::init() {
        Some(apic) => println!("[nova] apic: {apic}; 8259 PICs masked"),
        None => println!("[nova] pic: remapped to vectors 32-47 (local APIC disabled)"),
    }
    pit::init();
    rtc::init();
    println!("[nova] pit: {} Hz", pit::HZ);
    if keyboard::init() {
        println!("[nova] keyboard: PS/2 port 1 enabled");
    } else {
        println!("[nova] keyboard: no response from PS/2 controller, serial input only");
    }
    if mouse::init() {
        println!("[nova] mouse: PS/2 port 2 enabled{}", if mouse::absolute() { ", absolute pointer (vmmouse)" } else { "" });
    }
    serial::enable_rx_interrupt();
    interrupts::enable();
    println!("[nova] interrupts: enabled (timer, keyboard, mouse, COM1)");
    println!("[nova] clock: TSC at {} MHz", pit::calibrate() / 1_000_000);
    if acpi::init_namespace(apic::active()) {
        println!("[nova] acpi: AML namespace ready ({} interrupt model)", if apic::active() { "I/O APIC" } else { "PIC" });
    }
    if !rand::hardware() {
        println!("[nova] warning: no RDRAND; random numbers come from timer jitter");
    }
    setup::run()
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
    tty::emergency();
    console::resume();
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
