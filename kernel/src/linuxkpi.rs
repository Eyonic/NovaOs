//! Nova's side of the Linux driver layer (FEATURES=linux-drivers).
//!
//! Real Linux driver code (compiled from the Linux source with Linux's own build system,
//! scripts/build-linux.sh) runs inside Nova on top of lx_emul, a C layer in
//! kernel/linuxkpi/lx_emul/ that implements the low-level Linux kernel API. lx_emul in turn
//! calls the `nova_*` functions here for memory, PCI configuration space, time, scheduling
//! and logging. Linux data structures stay on the C side; Rust holds opaque pointers.
//!
//! Devices: [`crate::drivers::probe_all`] hands every PCI function that Nova's own Rust
//! drivers did not claim to Linux ([`add_pci`]), then [`start`] runs the Linux initcalls,
//! which register the built-in drivers and probe. Network devices the drivers register
//! become [`LinuxNic`]s for Nova's TCP/IP stack. A kernel task ("linux") then polls
//! interrupt handlers, NAPI, timers and work items every tick.
//!
//! See docs/linux-drivers.md for the design and roadmap.

use alloc::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_void, CStr};

use crate::drivers::Nic;
use crate::pci::Device;
use crate::sync::SpinLock;
use crate::{memory, pit, task};

unsafe extern "C" {
    fn nova_lx_init();
    fn nova_lx_tick();
    fn nova_lx_pci_add(bus: u8, devfn: u8, vendor: u16, device: u16, subvendor: u16, subdevice: u16, class_rev: u32, irq: u8);
    fn nova_lx_start();
    fn nova_lx_pci_driver(bus: u8, devfn: u8) -> *const c_char;
    fn nova_lx_poll();
    fn nova_lx_worker(index: u32);
    fn nova_lx_irq(irq: u32) -> bool;
    fn nova_lx_switch(task_id: u64);
    fn nova_lx_netdev_get(index: u32) -> *mut c_void;
    fn nova_lx_netdev_open(dev: *mut c_void) -> c_int;
    fn nova_lx_netdev_mac(dev: *mut c_void, mac: *mut u8);
    fn nova_lx_netdev_carrier(dev: *mut c_void) -> bool;
    fn nova_lx_netdev_tx_ready(dev: *mut c_void) -> bool;
    fn nova_lx_netdev_xmit(dev: *mut c_void, frame: *const u8, len: usize) -> c_int;
}

/// Set up lx_emul's view of memory. Before any other call into Linux code.
pub fn init() {
    // SAFETY: lx_emul's entry point; runs once, before the drivers.
    unsafe { nova_lx_init() }
}

/// Scheduler hook: task `id` runs now, so Linux's `current` must be its task_struct.
pub fn switch_to(id: usize) {
    // SAFETY: lx_emul only updates its current_task pointer (no allocation, no locks).
    unsafe { nova_lx_switch(id as u64) }
}

/// Timer interrupt: advance `jiffies`.
pub fn tick() {
    // SAFETY: a single 64-bit increment; Linux code only reads jiffies.
    unsafe { nova_lx_tick() }
}

fn devfn(dev: &Device) -> u8 {
    dev.address.device << 3 | dev.address.function
}

/// Offer a PCI function to the Linux drivers (before [`start`]).
pub fn add_pci(dev: &Device) {
    let subsystem = dev.address.read32(0x2C);
    let class_rev = (dev.class as u32) << 24 | (dev.subclass as u32) << 16 | (dev.prog_if as u32) << 8 | dev.revision as u32;
    // SAFETY: plain values; lx_emul copies them into its own struct pci_dev.
    unsafe {
        nova_lx_pci_add(dev.address.bus, devfn(dev), dev.vendor, dev.device, subsystem as u16, (subsystem >> 16) as u16, class_rev, intx_irq(dev))
    }
}

/// Run the Linux initcalls: built-in drivers register and probe the offered devices.
pub fn start() {
    // SAFETY: called once, after init and the add_pci calls.
    unsafe { nova_lx_start() }
}

/// Name of the Linux driver that bound `dev`, if any.
pub fn driver_for(dev: &Device) -> Option<&'static str> {
    // SAFETY: driver names are static strings in the driver objects (pci_driver.name).
    let name = unsafe { nova_lx_pci_driver(dev.address.bus, devfn(dev)) };
    if name.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(name) }.to_str().ok()
}

/// Bring up every network device the Linux drivers registered, and start the polling task.
pub fn take_nics() -> Vec<LinuxNic> {
    let mut nics = Vec::new();
    for index in 0.. {
        // SAFETY: lx_emul returns registered net_device pointers or null past the end.
        let dev = unsafe { nova_lx_netdev_get(index) };
        if dev.is_null() {
            break;
        }
        // SAFETY: a registered net_device; open runs the driver's ndo_open.
        let err = unsafe { nova_lx_netdev_open(dev) };
        if err != 0 {
            crate::log::warn(&alloc::format!("linux: could not open network device {index}: error {err}"));
            continue;
        }
        RX.lock().insert(dev as usize, VecDeque::new());
        nics.push(LinuxNic { dev });
    }
    nics
}

/// Work-queue workers: Linux work items run on this pool, so one that sleeps (waiting
/// for a USB transfer, say) does not hold up the others.
const WORKERS: u32 = 4;

/// The "linux" kernel task: deferred interrupt work (NAPI, threaded handlers), timers and
/// RCU callbacks; every interrupt wakes it, so NAPI runs right after a device interrupt.
/// Plus the work-queue workers. Started before the initcalls: probing can already wait
/// for deferred work (USB host controllers do).
pub fn spawn_poller() {
    for index in 0..WORKERS {
        task::spawn(&alloc::format!("linux-worker-{index}"), move || {
            // SAFETY: lx_emul's worker loop for slot `index`; never returns.
            unsafe { nova_lx_worker(index) }
        });
    }
    task::spawn("linux", || loop {
        // SAFETY: lx_emul's poll loop; only ever called from this task.
        unsafe { nova_lx_poll() };
        if let Some(irq) = masked_storm() {
            crate::log::warn(&alloc::format!("linux: IRQ {irq} kept firing with no handler claiming it; masked, its devices are polled"));
        }
        task::wait_event();
    });
}

// ---------------------------------------------------------------- interrupts

use core::sync::atomic::{AtomicU32, Ordering};

/// IRQ numbers as Linux drivers see them: a PIC line (0-15) without an I/O APIC, a GSI
/// with one, or an MSI number (apic::MSI_IRQ_BASE + vector). The storm guard counts
/// unclaimed interrupts per number, folded into this table.
static UNCLAIMED: [AtomicU32; 512] = [const { AtomicU32::new(0) }; 512];
/// A line masked by the storm guard and not yet reported (IRQ number + 1).
static STORMED: AtomicU32 = AtomicU32::new(0);
/// Like Linux's "nobody cared": a level-triggered line nobody acknowledges would
/// otherwise lock the CPU in its interrupt handler.
const STORM_LIMIT: u32 = 100_000;

fn is_msi(irq: u32) -> bool {
    irq >= crate::apic::MSI_IRQ_BASE
}

/// IRQ handler (interrupts disabled) for an IRQ number Linux drivers requested.
pub fn irq(irq: u32) {
    // SAFETY: lx_emul runs the handlers registered for this IRQ, with interrupts off.
    let claimed = unsafe { nova_lx_irq(irq) };
    let counter = &UNCLAIMED[irq as usize % UNCLAIMED.len()];
    if claimed || is_msi(irq) {
        counter.store(0, Ordering::Relaxed);
    } else if counter.fetch_add(1, Ordering::Relaxed) + 1 == STORM_LIMIT {
        set_line(irq, false);
        STORMED.store(irq + 1, Ordering::Relaxed);
    }
}

fn masked_storm() -> Option<u32> {
    STORMED.swap(0, Ordering::Relaxed).checked_sub(1)
}

/// Unmask or mask an INTx line at whichever controller delivers it.
fn set_line(irq: u32, enable: bool) {
    if crate::apic::active() {
        if enable { crate::apic::unmask_gsi(irq) } else { crate::apic::mask_gsi(irq) }
    } else if irq < 16 && irq != 2 && !matches!(irq as u8, crate::pic::IRQ_TIMER | crate::pic::IRQ_KEYBOARD | crate::pic::IRQ_COM1 | crate::pic::IRQ_MOUSE) {
        if enable { crate::pic::unmask(irq as u8) } else { crate::pic::mask(irq as u8) }
    }
}

#[unsafe(no_mangle)]
extern "C" fn nova_irq_line(irq: u32, enable: bool) {
    // MSI vectors are always open at the CPU; lx_emul gates their handlers itself.
    if is_msi(irq) {
        return;
    }
    if enable {
        UNCLAIMED[irq as usize % UNCLAIMED.len()].store(0, Ordering::Relaxed);
    }
    set_line(irq, enable);
}

/// One MSI/MSI-X vector for a Linux driver: the IRQ number, and the message the device
/// must write (address, data). False when no vector is free or there is no local APIC.
#[unsafe(no_mangle)]
extern "C" fn nova_msi_alloc(irq: *mut u32, address: *mut u64, data: *mut u32) -> bool {
    let Some((n, a, d)) = crate::apic::alloc_msi() else { return false };
    // SAFETY: lx_emul passes valid out pointers.
    unsafe {
        *irq = n;
        *address = a;
        *data = d;
    }
    true
}

#[unsafe(no_mangle)]
extern "C" fn nova_msi_free(irq: u32) {
    crate::apic::free_msi(irq)
}

/// The IRQ number Linux should see for `dev`'s INTx pin: with an I/O APIC, the GSI from
/// ACPI's _PRT (routed, masked until request_irq); without, the PIC line from config space.
fn intx_irq(dev: &Device) -> u8 {
    let pin = crate::pci::config_read(dev.address.bus, devfn(dev), 0x3D, 1) as u8;
    if !crate::apic::active() || pin == 0 {
        return dev.irq_line;
    }
    match crate::acpi::pci_irq(dev.address.device, pin - 1) {
        Some((gsi, level, low)) if gsi < 256 && crate::apic::route_gsi(gsi, level, low) => gsi as u8,
        _ => 0xFF, // no route: the driver must use MSI (or is polled)
    }
}

// ---------------------------------------------------------------- network devices

/// Frames received per device, waiting for Nova's network task.
static RX: SpinLock<BTreeMap<usize, VecDeque<Vec<u8>>>> = SpinLock::new(BTreeMap::new());
/// Frames kept per device before new ones are dropped (the stack polls every tick).
const RX_QUEUE_LIMIT: usize = 512;

/// A network device driven by a Linux driver.
pub struct LinuxNic {
    dev: *mut c_void,
}

// SAFETY: Nova runs on one CPU; the net_device is only used through lx_emul's bridge calls.
unsafe impl Send for LinuxNic {}

impl Nic for LinuxNic {
    fn mac(&self) -> [u8; 6] {
        let mut mac = [0; 6];
        // SAFETY: writes six bytes into `mac`.
        unsafe { nova_lx_netdev_mac(self.dev, mac.as_mut_ptr()) };
        mac
    }

    fn link_up(&self) -> bool {
        // SAFETY: reads the net_device's carrier bit.
        unsafe { nova_lx_netdev_carrier(self.dev) }
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        RX.lock().get_mut(&(self.dev as usize))?.pop_front()
    }

    fn can_transmit(&self) -> bool {
        // SAFETY: reads the net_device's carrier and queue state.
        unsafe { nova_lx_netdev_tx_ready(self.dev) }
    }

    fn transmit(&mut self, frame: &[u8]) -> bool {
        // SAFETY: the driver copies the frame into an sk_buff before returning.
        unsafe { nova_lx_netdev_xmit(self.dev, frame.as_ptr(), frame.len()) == 0 }
    }
}

#[unsafe(no_mangle)]
extern "C" fn nova_netdev_rx(dev: *mut c_void, frame: *const u8, len: usize) {
    // SAFETY: lx_emul passes the received frame's bytes, valid for this call.
    let data = unsafe { core::slice::from_raw_parts(frame, len) }.to_vec();
    let mut rx = RX.lock();
    if let Some(queue) = rx.get_mut(&(dev as usize))
        && queue.len() < RX_QUEUE_LIMIT
    {
        queue.push_back(data);
    }
}

// ---------------------------------------------------------------- firmware

/// request_firmware(): /lib/firmware/<name> (or /usr/lib/firmware/<name>) copied into a
/// nova_alloc() buffer that lx_emul frees with nova_free; null if absent.
#[unsafe(no_mangle)]
extern "C" fn nova_firmware_read(name: *const u8, name_len: usize, len: *mut usize) -> *mut u8 {
    // SAFETY: lx_emul passes the firmware name's bytes, valid for this call.
    let name = unsafe { core::slice::from_raw_parts(name, name_len) };
    let Ok(name) = core::str::from_utf8(name) else { return core::ptr::null_mut() };
    if name.split('/').any(|part| part == "..") {
        return core::ptr::null_mut();
    }
    let ns = crate::vfs::root();
    let Some(bytes) = ["lib/firmware", "usr/lib/firmware"].iter().find_map(|dir| ns.read_program("/", &alloc::format!("/{dir}/{name}")).ok()) else {
        return core::ptr::null_mut();
    };
    let buf = nova_alloc(bytes.len(), 16, false, false);
    if !buf.is_null() {
        // SAFETY: buf holds bytes.len() bytes; len is lx_emul's out-parameter.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
            *len = bytes.len();
        }
    }
    buf
}

// ---------------------------------------------------------------- memory

/// Live allocations: lx_emul's kfree() has no size, the Rust allocator needs the layout.
static ALLOCATIONS: SpinLock<BTreeMap<usize, (Layout, bool)>> = SpinLock::new(BTreeMap::new());

#[unsafe(no_mangle)]
extern "C" fn nova_alloc(size: usize, align: usize, zero: bool, dma32: bool) -> *mut u8 {
    let Ok(layout) = Layout::from_size_align(size.max(1), align.max(16)) else {
        return core::ptr::null_mut();
    };
    let ptr = if dma32 {
        memory::dma32_alloc(layout, zero)
    } else {
        // SAFETY: non-zero size.
        unsafe { if zero { alloc_zeroed(layout) } else { alloc(layout) } }
    };
    if !ptr.is_null() {
        ALLOCATIONS.lock().insert(ptr as usize, (layout, dma32));
    }
    ptr
}

#[unsafe(no_mangle)]
extern "C" fn nova_free(ptr: *const u8) {
    let Some((layout, dma32)) = ALLOCATIONS.lock().remove(&(ptr as usize)) else {
        crate::log::warn(&alloc::format!("linux: kfree of unknown pointer {ptr:p}"));
        return;
    };
    // SAFETY: allocated by nova_alloc with this layout and pool.
    unsafe {
        if dma32 {
            memory::dma32_free(ptr as *mut u8, layout)
        } else {
            dealloc(ptr as *mut u8, layout)
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn nova_alloc_size(ptr: *const u8) -> usize {
    ALLOCATIONS.lock().get(&(ptr as usize)).map_or(0, |(layout, _)| layout.size())
}

/// Back the `struct page` entries (`page_size` bytes each) of `count` page frames from
/// `pfn` with memory, so Linux code can use them (memory.rs, VMEMMAP_BASE).
#[unsafe(no_mangle)]
extern "C" fn nova_vmemmap_populate(pfn: u64, count: u64, page_size: u64) -> bool {
    memory::populate(memory::VMEMMAP_BASE + pfn * page_size, count * page_size)
}

#[unsafe(no_mangle)]
extern "C" fn nova_hhdm_offset() -> u64 {
    memory::hhdm_offset()
}

#[unsafe(no_mangle)]
extern "C" fn nova_kernel_phys_base() -> u64 {
    crate::limine::EXECUTABLE_ADDRESS.physical_base().unwrap_or(0)
}

#[unsafe(no_mangle)]
extern "C" fn nova_map_mmio(phys: u64, len: u64, _write_combine: bool) -> *mut c_void {
    memory::map_mmio(phys, len.max(1)) as *mut c_void
}

// ---------------------------------------------------------------- PCI configuration space

#[unsafe(no_mangle)]
extern "C" fn nova_pci_read(bus: u8, devfn: u8, offset: u16, width: u8) -> u32 {
    crate::pci::config_read(bus, devfn, offset, width)
}

#[unsafe(no_mangle)]
extern "C" fn nova_pci_config_size(bus: u8) -> c_int {
    crate::pci::config_size(bus) as c_int
}

#[unsafe(no_mangle)]
extern "C" fn nova_pci_write(bus: u8, devfn: u8, offset: u16, width: u8, value: u32) {
    crate::pci::config_write(bus, devfn, offset, width, value)
}

// ---------------------------------------------------------------- time, tasks, log

#[unsafe(no_mangle)]
extern "C" fn nova_nanos() -> u64 {
    pit::nanos()
}

#[unsafe(no_mangle)]
extern "C" fn nova_ndelay(ns: u64) {
    let end = pit::nanos() + ns;
    while pit::nanos() < end {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
extern "C" fn nova_sleep_ticks(ticks: u64) {
    task::sleep_ticks(ticks)
}

/// Sleep until woken (`nova_wake`) or the next interrupt; Linux code re-checks its
/// condition after every return.
#[unsafe(no_mangle)]
extern "C" fn nova_park() {
    task::wait_event()
}

#[unsafe(no_mangle)]
extern "C" fn nova_wake(task_id: u64) {
    task::wake(task_id as usize)
}

#[unsafe(no_mangle)]
extern "C" fn nova_yield() {
    task::yield_now()
}

#[unsafe(no_mangle)]
extern "C" fn nova_task_id() -> u64 {
    task::current_id() as u64
}

#[unsafe(no_mangle)]
extern "C" fn nova_random_u64() -> u64 {
    crate::rand::u64()
}

#[unsafe(no_mangle)]
extern "C" fn nova_log(level: c_int, msg: *const u8, len: usize) {
    // SAFETY: lx_emul passes a formatted buffer of `len` bytes.
    let bytes = unsafe { core::slice::from_raw_parts(msg, len) };
    let text = core::str::from_utf8(bytes).unwrap_or("(invalid UTF-8)");
    if level <= 3 {
        crate::log::warn(&alloc::format!("linux: {text}"));
    } else {
        crate::log::info(&alloc::format!("linux: {text}"));
    }
}

// ---------------------------------------------------------------- input devices

/// Pointer state accumulated between Linux SYN_REPORT events.
struct Pointer {
    dx: i32,
    dy: i32,
    wheel: i32,
    abs: Option<(u32, u32)>,
    buttons: [bool; 3],
    changed: bool,
}

static POINTER: SpinLock<Pointer> = SpinLock::new(Pointer { dx: 0, dy: 0, wheel: 0, abs: None, buttons: [false; 3], changed: false });

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_WHEEL: u16 = 0x08;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_TOUCH: u16 = 0x14a;

/// A Linux key code as a PS/2 scan code set 1 make code (0xE0-prefixed when `.1`).
/// Codes 1-88 are the set 1 codes themselves; the rest of a PC keyboard is mapped here.
fn set1(code: u16) -> Option<(u8, bool)> {
    Some(match code {
        1..=88 => (code as u8, false),
        96 => (0x1C, true),  // keypad Enter
        97 => (0x1D, true),  // right Ctrl
        98 => (0x35, true),  // keypad /
        99 => (0x37, true),  // SysRq / Print Screen
        100 => (0x38, true), // right Alt
        102 => (0x47, true), // Home
        103 => (0x48, true), // Up
        104 => (0x49, true), // Page Up
        105 => (0x4B, true), // Left
        106 => (0x4D, true), // Right
        107 => (0x4F, true), // End
        108 => (0x50, true), // Down
        109 => (0x51, true), // Page Down
        110 => (0x52, true), // Insert
        111 => (0x53, true), // Delete
        119 => (0x45, false), // Pause (as Num Lock's code without the E1 sequence)
        125 => (0x5B, true), // left Windows
        126 => (0x5C, true), // right Windows
        127 => (0x5D, true), // Menu
        _ => return None,
    })
}

/// One event from a Linux input device (lx_emul/input.c, task context). Absolute axes
/// arrive scaled to 0..=0xFFFF.
#[unsafe(no_mangle)]
extern "C" fn nova_input_event(kind: u16, code: u16, value: i32) {
    match kind {
        EV_KEY if code < 0x100 => {
            if let Some((make, extended)) = set1(code) {
                if value == 2 {
                    return; // autorepeat: Nova repeats keys itself
                }
                if extended {
                    crate::input::SCANCODES.push(0xE0);
                }
                crate::input::SCANCODES.push(if value == 0 { make | 0x80 } else { make });
            }
        }
        EV_KEY => {
            let mut p = POINTER.lock();
            let index = match code {
                BTN_LEFT | BTN_TOUCH => 0,
                BTN_RIGHT => 1,
                BTN_MIDDLE => 2,
                _ => return,
            };
            p.buttons[index] = value != 0;
            p.changed = true;
        }
        EV_REL => {
            let mut p = POINTER.lock();
            match code {
                REL_X => p.dx += value,
                REL_Y => p.dy += value,
                REL_WHEEL => p.wheel -= value, // Linux: positive = away from the user
                _ => return,
            }
            p.changed = true;
        }
        EV_ABS => {
            let mut p = POINTER.lock();
            let (x, y) = p.abs.unwrap_or((0x8000, 0x8000));
            p.abs = Some(match code {
                ABS_X => (value.clamp(0, 0xFFFF) as u32, y),
                ABS_Y => (x, value.clamp(0, 0xFFFF) as u32),
                _ => return,
            });
            p.changed = true;
        }
        EV_SYN => {
            let mut p = POINTER.lock();
            if !p.changed {
                return;
            }
            let motion = match p.abs {
                Some((x, y)) => crate::mouse::Motion::Absolute(x, y),
                None => crate::mouse::Motion::Relative(p.dx, p.dy),
            };
            let report = crate::mouse::Report { motion, left: p.buttons[0], right: p.buttons[1], middle: p.buttons[2], wheel: p.wheel };
            p.dx = 0;
            p.dy = 0;
            p.wheel = 0;
            p.changed = false;
            drop(p);
            crate::mouse::inject(report);
        }
        _ => {}
    }
    // Wake whoever waits for input (the desktop and shells poll after interrupts).
    crate::task::IRQ_COUNT.fetch_add(1, Ordering::SeqCst);
}
