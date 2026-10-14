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
    fn nova_lx_irq(irq: u32) -> bool;
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

/// The "linux" kernel task: deferred interrupt work (NAPI, threaded handlers), timers and
/// work items. Every interrupt wakes it, so NAPI runs right after a device interrupt.
pub fn spawn_poller() {
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
