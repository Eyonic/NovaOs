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
    fn nova_lx_netdev_get(index: u32) -> *mut c_void;
    fn nova_lx_netdev_open(dev: *mut c_void) -> c_int;
    fn nova_lx_netdev_mac(dev: *mut c_void, mac: *mut u8);
    fn nova_lx_netdev_carrier(dev: *mut c_void) -> bool;
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
        nova_lx_pci_add(dev.address.bus, devfn(dev), dev.vendor, dev.device, subsystem as u16, (subsystem >> 16) as u16, class_rev, dev.irq_line)
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

/// The "linux" kernel task: Linux's interrupt handlers, NAPI, timers and work items.
pub fn spawn_poller() {
    task::spawn("linux", || loop {
        // SAFETY: lx_emul's poll loop; only ever called from this task.
        unsafe { nova_lx_poll() };
        task::wait_event();
    });
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

/// Select a configuration register and return the data port for an access of the given
/// width; byte and word accesses use the matching port offset (no read-modify-write,
/// which would clear write-1-to-clear status bits in the same dword).
fn pci_select(bus: u8, devfn: u8, offset: u16) -> u16 {
    let address = 1u32 << 31 | (bus as u32) << 16 | (devfn as u32) << 8 | (offset as u32 & 0xFC);
    // SAFETY: PCI configuration address port.
    unsafe { core::arch::asm!("out dx, eax", in("dx") 0xCF8u16, in("eax") address, options(nomem, nostack, preserves_flags)) };
    0xCFC + (offset & 3)
}

#[unsafe(no_mangle)]
extern "C" fn nova_pci_read(bus: u8, devfn: u8, offset: u16, width: u8) -> u32 {
    let port = pci_select(bus, devfn, offset);
    // SAFETY: PCI configuration data port, width-matched access.
    unsafe {
        match width {
            1 => {
                let v: u8;
                core::arch::asm!("in al, dx", out("al") v, in("dx") port, options(nomem, nostack, preserves_flags));
                v as u32
            }
            2 => {
                let v: u16;
                core::arch::asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack, preserves_flags));
                v as u32
            }
            _ => {
                let v: u32;
                core::arch::asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack, preserves_flags));
                v
            }
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn nova_pci_write(bus: u8, devfn: u8, offset: u16, width: u8, value: u32) {
    let port = pci_select(bus, devfn, offset);
    // SAFETY: PCI configuration data port, width-matched access.
    unsafe {
        match width {
            1 => core::arch::asm!("out dx, al", in("dx") port, in("al") value as u8, options(nomem, nostack, preserves_flags)),
            2 => core::arch::asm!("out dx, ax", in("dx") port, in("ax") value as u16, options(nomem, nostack, preserves_flags)),
            _ => core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags)),
        }
    }
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
