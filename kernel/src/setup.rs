//! What happens after the kernel core is up: hardware setup, first-boot setup with
//! Claude, installation, and the desktop.
//!
//! 1. Hardware: scan PCI, bind Nova's Rust drivers, mount storage, start networking.
//! 2. First boot (no `system/setup-done`): the assistant (Nova Assistant on DeepSeek, or
//!    Claude Code) opens full screen; the user signs in. The assistant then ports drivers for devices Nova has none
//!    for (Rust sources saved under drivers/ for the next kernel build).
//! 3. Live session with a free disk: offer to install Nova onto it.
//! 4. The desktop starts, with the assistant open.
//!
//! Kernel command line `nova.mode=shell` skips 2-4 and runs the text shell (tests).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::claude::{self, Config};
use crate::drivers::{self, Status};
use crate::input::{self, Key};
use crate::install::{self, Mode};
use crate::{apps, fs, limine, net, pit, print, println, storage, task, wm};

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const GREEN: &str = "\x1b[32m";
const ORANGE: &str = "\x1b[33m";

pub fn cmdline_flag(name: &str) -> Option<&'static str> {
    limine::EXECUTABLE_CMDLINE.get()?.split_whitespace().find_map(|a| a.strip_prefix(name)?.strip_prefix('='))
}

/// First-boot setup has three steps; each announces itself so the user knows how far
/// along it is (one task per screen, progress visible).
fn step(n: u8, title: &str) {
    println!();
    println!("  {DIM}Step {n} of 3{RESET}");
    println!("{BOLD}{title}{RESET}");
}

/// Step 1: drivers, storage and network. Prints one line per device.
pub fn hardware() {
    step(1, "Setting up this computer");
    println!("  {DIM}Detecting hardware and installing Nova's Rust drivers...{RESET}");
    let probed = drivers::probe_all(|r| {
        let (mark, text) = match &r.status {
            Status::Bound { driver, detail } => (format!("{GREEN}\u{2713}{RESET}"), format!("{BOLD}{driver}{RESET}  {detail}")),
            Status::Failed { driver, error } => (format!("\x1b[31m\u{2717}{RESET}"), format!("{driver} failed: {error}")),
            Status::Missing => (format!("{ORANGE}?{RESET}"), "no driver yet".into()),
            Status::NotNeeded => (format!("{DIM}-{RESET}"), "built in, no driver needed".into()),
            Status::Ported { detail } => (format!("{ORANGE}~{RESET}"), detail.clone()),
        };
        println!("  {mark} {:<44} {text}", r.describe());
    });
    for disk in probed.disks {
        storage::add(disk);
    }
    println!("  {GREEN}\u{2713}{RESET} storage: {}", install::mount_system());
    apps::install_bundled();
    let installed = crate::process::install_bundled_programs();
    let (programs, files): (Vec<String>, Vec<String>) = installed.into_iter().partition(|p| !p.contains('/'));
    // System files under bin/ are Unix tools (BusyBox); shared libraries live in usr/lib.
    let (tools, files): (Vec<String>, Vec<String>) = files.into_iter().partition(|p| p.starts_with("bin/"));
    let libs = files.iter().filter(|p| p.contains(".so")).count();
    if !programs.is_empty() {
        let tools = if tools.is_empty() { String::new() } else { format!(", {} Unix tools (BusyBox: sh, ls, tar, ...)", tools.len()) };
        println!(
            "  {GREEN}\u{2713}{RESET} C/C++ compatibility: Linux x86-64 ABI ({}), {} programs in bin/ ({}){tools}, {} shared libraries",
            crate::process::simd_summary(),
            programs.len(),
            programs.join(", "),
            libs
        );
    }
    // Devices whose driver Claude already ported on an earlier boot.
    let drafts: Vec<String> = fs::list("drivers/").into_iter().map(|(n, _, _)| n).filter(|n| n.ends_with(".rs")).collect();
    if !drafts.is_empty() {
        for r in missing_devices() {
            let detail = format!("Rust port drafted by {} ({}), compiled into the next kernel build", claude::provider_name(), drafts.join(", "));
            println!("  {ORANGE}~{RESET} {:<44} {detail}", r.describe());
            drivers::update(r.device.address, Status::Ported { detail });
        }
    }
    match probed.nics.into_iter().next() {
        Some(nic) => {
            net::init(nic);
            print!("  {DIM}network: waiting for an address (DHCP)...{RESET}");
            if net::wait_configured(8 * pit::HZ) {
                let ip = net::info().and_then(|i| i.ip).map(|ip| format!("{ip}")).unwrap_or_default();
                println!("\r\x1b[K  {GREEN}\u{2713}{RESET} network: online, address {ip}");
            } else {
                println!("\r\x1b[K  {ORANGE}!{RESET} network: no DHCP answer yet (still trying in the background)");
            }
        }
        None => println!("  {ORANGE}!{RESET} network: no supported network card"),
    }
    crate::remote::start();
}

fn ask_yes_no(question: &str, default_yes: bool) -> bool {
    print!("{question} {}", if default_yes { "[Y/n] " } else { "[y/N] " });
    loop {
        match input::read_key() {
            Key::Char(b'y' | b'Y') => {
                println!("yes");
                return true;
            }
            Key::Char(b'n' | b'N') => {
                println!("no");
                return false;
            }
            Key::Char(b'\n') => {
                println!("{}", if default_yes { "yes" } else { "no" });
                return default_yes;
            }
            _ => {}
        }
    }
}

fn missing_devices() -> Vec<drivers::Record> {
    drivers::records().into_iter().filter(|r| matches!(r.status, Status::Missing)).collect()
}

const DRIVER_TRAITS: &str = r#"
Nova driver interfaces (kernel/src/drivers/mod.rs):

pub trait Nic: Send {            // network cards; the TCP/IP stack drives it by polling
    fn mac(&self) -> [u8; 6];
    fn link_up(&self) -> bool;
    fn receive(&mut self) -> Option<Vec<u8>>;   // one Ethernet frame
    fn transmit(&mut self, frame: &[u8]) -> bool;
}
pub trait BlockDevice: Send {    // disks, 512-byte sectors
    fn name(&self) -> String;
    fn sector_count(&self) -> u64;
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), String>;
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), String>;
    fn flush(&mut self) -> Result<(), String>;
}
Helpers: crate::pci::Device { address, vendor, device, class, subclass, prog_if, .. } with
dev.bar(i) -> Option<Bar::Memory{base,size} | Bar::Io{port,size}>, dev.enable() (memory + bus master);
crate::memory::map_mmio(phys, len) -> virtual address (uncached); crate::memory::DmaBuffer::new(size, align)
with .phys(), .as_ptr(), .as_slice(); crate::port::{inb, outb, outw}; crate::task::yield_now() while polling.
Drivers are polled (no device interrupts). Existing examples: drivers/e1000.rs, drivers/ahci.rs. no_std + alloc Rust 2024.
Other device kinds (audio, USB host controllers, ...) have no trait yet: write a self-contained driver with an
`init(dev: &Device) -> Result<Self, String>` constructor and the obvious methods, and explain in the .md which
kernel interface it needs.
"#;

/// Step 2b: Claude ports drivers for devices without one.
fn port_drivers(cfg: &Config) {
    let missing = missing_devices();
    if missing.is_empty() {
        println!("  {GREEN}\u{2713}{RESET} every device has a driver; nothing to port.");
        return;
    }
    step(2, &format!("Porting drivers with {}", cfg.provider.name()));
    for r in &missing {
        println!("  {ORANGE}?{RESET} {}", r.describe());
    }
    let list: String = missing.iter().map(|r| format!("- {} (prog_if {:02x}, rev {:02x})\n", r.describe(), r.device.prog_if, r.device.revision)).collect();
    let task_text = format!(
        "Nova is setting up this computer. These PCI devices have no Nova driver yet:\n{list}\n\
         For each device: use pci_config to inspect it, identify it (vendor, model, programming interface), \
         and write a driver port to drivers/<short-name>.rs plus drivers/<short-name>.md explaining what it is, \
         what the driver supports, and what is left to do. Keep each driver focused (initialisation, \
         identification, and the core data path where practical). Finish with a two-line summary per device.\n{DRIVER_TRAITS}"
    );
    let mut agent = claude::Agent::new(cfg.clone(), "You are running Nova's first-boot driver setup. Work autonomously; the user is watching.");
    agent.cfg.auto_approve = true; // reading hardware and writing drafts only
    agent.turn(serde_json::Value::String(task_text));
    let drafts = fs::list("drivers/");
    for r in &missing {
        let detail = format!("Rust port drafted by {} ({} files in drivers/); compiled into the next kernel build", cfg.provider.name(), drafts.len());
        drivers::update(r.device.address, Status::Ported { detail });
    }
}

/// Step 3: offer installation when running live with a free disk.
fn offer_install() -> bool {
    if install::mode() != Mode::Live {
        return false;
    }
    let targets = install::targets();
    let Some(target) = targets.first() else { return false };
    step(3, "Install Nova");
    println!("  You are running Nova from the installation image.");
    println!("  Disk {}: {} ({} MiB) can hold an installed system. {ORANGE}Installing erases it.{RESET}", target.index, target.name, target.sectors / 2048);
    if cmdline_flag("nova.install") == Some("no") || !ask_yes_no(&format!("  Install Nova on disk {}?", target.index), true) {
        println!("  {DIM}Continuing live. You can install later from Settings (Storage & install).{RESET}");
        return false;
    }
    let _ = fs::write("system/setup-done", b"1\n");
    let mut last = 999;
    let result = install::install(target.index, |pct, msg| {
        if pct / 5 != last / 5 {
            last = pct;
            let bar: String = (0..30).map(|i| if i * 100 / 30 < pct { '\u{2588}' } else { '\u{2591}' }).collect();
            print!("\r\x1b[K  {bar} {pct:>3}%  {msg}");
        }
    });
    println!();
    match result {
        Ok(()) => {
            println!("  {GREEN}\u{2713} Nova is installed.{RESET} Your settings and files were copied.");
            println!("  Restarting into the installed system...");
            task::sleep_ticks(2 * pit::HZ);
            crate::power::reboot();
        }
        Err(e) => {
            println!("  \x1b[31m\u{2717} Installation failed: {e}{RESET}");
            false
        }
    }
}

/// Runs after the kernel core is up; never returns.
pub fn run() -> ! {
    hardware();
    if cmdline_flag("nova.mode") == Some("shell") || crate::console::screen().is_none() {
        println!("[nova] starting shell");
        crate::wayland::start(); // headless: no windows, the `wayland` command shows clients
        crate::pulse::start();
        crate::log::boot_done();
        loop {
            crate::shell::run();
        }
    }

    let first_boot = !fs::exists("system/setup-done");
    let mut cfg = Config::load();
    if first_boot {
        claude::welcome(&cfg);
        println!("  Welcome to Nova. {} will help set up this computer and build apps for you.", cfg.provider.app_name());
        println!("  {DIM}Three steps: sign in, port drivers, install. Each one can be skipped and done later.{RESET}");
        if claude::sign_in(&mut cfg) {
            port_drivers(&cfg);
        } else if !missing_devices().is_empty() {
            println!("  {DIM}Not signed in: devices without a driver stay unsupported for now.{RESET}");
        }
        offer_install();
        let _ = fs::write("system/setup-done", b"1\n");
        println!();
        println!("  {GREEN}\u{2713}{RESET} Setup is done. Press the Windows key for Home, F1 for the assistant.");
        println!("  {DIM}Starting the desktop...{RESET}");
        task::sleep_ticks(pit::HZ);
    }

    crate::log::boot_done();
    if !wm::start() {
        loop {
            crate::shell::run();
        }
    }
    // Its terminal also mirrors the serial line.
    // The assistant waits behind its bubble (bottom-right) until clicked.
    wm::spawn_assistant(true, false, None);
    // nova.launch=<app path>[,<file>]: open an app (and a file) as soon as the desktop is
    // up, for screenshots and tests.
    if let Some(what) = cmdline_flag("nova.launch") {
        task::sleep_ticks(pit::HZ / 2);
        for p in what.split(',') {
            let _ = crate::apps::launch(p);
        }
    }
    loop {
        task::wait_event();
    }
}
