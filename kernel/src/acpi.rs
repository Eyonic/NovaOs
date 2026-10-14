//! ACPI through uACPI (kernel/third_party/uacpi, MIT): firmware tables and the AML
//! interpreter.
//!
//! This module implements uACPI's kernel API (`uacpi_kernel_*`) on Nova's core, and
//! wraps the C glue in kernel/acpi/nova_acpi.c. Bring-up is in two stages:
//! [`init_tables`] (tables only, before interrupts are set up: the MADT and MCFG describe
//! the interrupt controllers and PCIe configuration space) and [`init_namespace`] (the
//! AML namespace, once tasks and interrupts run: it evaluates firmware methods such as
//! `_PIC` and `_PRT`).

use alloc::alloc::{alloc, alloc_zeroed, dealloc, Layout};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::ffi::{c_char, c_int, c_void, CStr};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::sync::SpinLock;
use crate::{limine, memory, pit, task};

type Status = c_int;
const OK: Status = 0;
const NOT_FOUND: Status = 6;
const TIMEOUT: Status = 18;
const UNIMPLEMENTED: Status = 8;
const INVALID_ARGUMENT: Status = 3;

unsafe extern "C" {
    fn nova_acpi_tables_init() -> c_int;
    fn nova_acpi_table(signature: *const c_char, out: *mut *const u8, length: *mut u32) -> c_int;
    fn nova_acpi_namespace_init(ioapic: c_int) -> c_int;
    fn nova_acpi_pci_irq(slot: u8, pin: u8, flags: *mut u32) -> c_int;
    fn nova_acpi_power_off() -> c_int;
    fn nova_acpi_reboot() -> c_int;
}

static TABLES: AtomicBool = AtomicBool::new(false);
static NAMESPACE: AtomicBool = AtomicBool::new(false);

/// Stage 1: find the RSDP and set up table access. False without ACPI.
pub fn init_tables() -> bool {
    if limine::RSDP.address().is_none() {
        return false;
    }
    // SAFETY: uACPI's table subsystem; the kernel API below is ready (heap, mapping).
    let st = unsafe { nova_acpi_tables_init() };
    if st != OK {
        crate::log::warn(&alloc::format!("acpi: table initialization failed (uACPI status {st})"));
        return false;
    }
    TABLES.store(true, Ordering::Relaxed);
    true
}

/// A firmware table by signature, e.g. `b"APIC"` (the MADT) or `b"MCFG"`.
pub fn table(signature: &[u8; 4]) -> Option<&'static [u8]> {
    if !TABLES.load(Ordering::Relaxed) {
        return None;
    }
    let sig = [signature[0], signature[1], signature[2], signature[3], 0];
    let (mut ptr, mut len) = (core::ptr::null(), 0u32);
    // SAFETY: uACPI maps the table and keeps it mapped (the reference is never dropped).
    let st = unsafe { nova_acpi_table(sig.as_ptr() as *const c_char, &mut ptr, &mut len) };
    (st == OK && !ptr.is_null()).then(|| unsafe { core::slice::from_raw_parts(ptr, len as usize) })
}

/// Stage 2: load the AML namespace and tell the firmware which interrupt model Nova uses.
pub fn init_namespace(ioapic: bool) -> bool {
    if !TABLES.load(Ordering::Relaxed) {
        return false;
    }
    task::spawn("acpi", work_task);
    // SAFETY: tables are up; tasks and interrupts run (AML may sleep or take locks).
    let st = unsafe { nova_acpi_namespace_init(ioapic as c_int) };
    if st == OK {
        NAMESPACE.store(true, Ordering::Relaxed);
    } else {
        crate::log::warn(&alloc::format!("acpi: namespace initialization failed (uACPI status {st})"));
    }
    st == OK
}

/// Global system interrupt for INTx `pin` (0 = INTA) of root-bus device `slot`, and
/// whether it is level triggered and active low.
pub fn pci_irq(slot: u8, pin: u8) -> Option<(u32, bool, bool)> {
    if !NAMESPACE.load(Ordering::Relaxed) {
        return None;
    }
    let mut flags = 0;
    // SAFETY: evaluates firmware methods through uACPI; task context.
    let gsi = unsafe { nova_acpi_pci_irq(slot, pin, &mut flags) };
    (gsi >= 0).then_some((gsi as u32, flags & 1 != 0, flags & 2 != 0))
}

/// Switch the machine off through ACPI (S5). Returns only if that failed.
pub fn power_off() {
    if NAMESPACE.load(Ordering::Relaxed) {
        // SAFETY: uACPI's sleep-state sequence; does not return on success.
        unsafe { nova_acpi_power_off() };
    }
}

/// Reset through the FADT reset register. Returns only if that failed.
pub fn reboot() {
    if NAMESPACE.load(Ordering::Relaxed) {
        // SAFETY: writes the firmware-described reset register.
        unsafe { nova_acpi_reboot() };
    }
}

// ---------------------------------------------------------------- MADT and MCFG

/// What the MADT says about interrupt controllers.
pub struct Madt {
    pub local_apic: u64,
    /// (id, physical address, first GSI)
    pub io_apics: Vec<(u8, u64, u32)>,
    /// (ISA IRQ, GSI, flags) from interrupt source overrides
    pub overrides: Vec<(u8, u32, u16)>,
    /// The system also has 8259 PICs (which must be masked when the APIC is used).
    pub pcat_compat: bool,
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

pub fn madt() -> Option<Madt> {
    let t = table(b"APIC")?;
    if t.len() < 44 {
        return None;
    }
    let mut madt = Madt { local_apic: le32(t, 36) as u64, io_apics: Vec::new(), overrides: Vec::new(), pcat_compat: le32(t, 40) & 1 != 0 };
    let mut at = 44;
    while at + 2 <= t.len() {
        let (kind, len) = (t[at], t[at + 1] as usize);
        if len < 2 || at + len > t.len() {
            break;
        }
        let e = &t[at..at + len];
        match kind {
            1 if len >= 12 => madt.io_apics.push((e[2], le32(e, 4) as u64, le32(e, 8))),
            2 if len >= 10 => madt.overrides.push((e[3], le32(e, 4), u16::from_le_bytes([e[8], e[9]]))),
            5 if len >= 12 => madt.local_apic = le64(e, 4),
            _ => {}
        }
        at += len;
    }
    Some(madt)
}

/// PCIe enhanced configuration (ECAM) regions: (base, segment, first bus, last bus).
pub fn mcfg() -> Vec<(u64, u16, u8, u8)> {
    let Some(t) = table(b"MCFG") else { return Vec::new() };
    t.get(44..).unwrap_or(&[]).chunks_exact(16).map(|e| (le64(e, 0), u16::from_le_bytes([e[8], e[9]]), e[10], e[11])).collect()
}

// ---------------------------------------------------------------- uACPI kernel API: memory

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_get_rsdp(out: *mut u64) -> Status {
    match limine::RSDP.address() {
        Some(addr) => {
            // SAFETY: uACPI passes a valid out pointer.
            unsafe { *out = addr };
            OK
        }
        None => NOT_FOUND,
    }
}

/// RAM-backed memory (cached) versus device registers (uncached), from the memory map.
fn is_ram(phys: u64, len: u64) -> bool {
    limine::MEMMAP.entries().any(|e| matches!(e.kind, 0 | 2 | 3 | 5 | 6) && phys >= e.base && phys + len <= e.base + e.length)
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_map(addr: u64, len: usize) -> *mut c_void {
    let len = len as u64;
    (if is_ram(addr, len) { memory::map_ram(addr, len) } else { memory::map_mmio(addr, len) }) as *mut c_void
}

/// Mappings are permanent in Nova (the window is large and tables stay referenced).
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_unmap(_addr: *mut c_void, _len: usize) {}

fn layout(size: usize) -> Layout {
    Layout::from_size_align(size.max(1), 16).unwrap()
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_alloc(size: usize) -> *mut c_void {
    // SAFETY: non-zero size.
    unsafe { alloc(layout(size)) as *mut c_void }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_alloc_zeroed(size: usize) -> *mut c_void {
    // SAFETY: non-zero size.
    unsafe { alloc_zeroed(layout(size)) as *mut c_void }
}

/// uACPI is built with UACPI_SIZED_FREES, so the size comes back with the pointer.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free(mem: *mut c_void, size: usize) {
    if !mem.is_null() {
        // SAFETY: allocated by uacpi_kernel_alloc{,_zeroed} with this size.
        unsafe { dealloc(mem as *mut u8, layout(size)) }
    }
}

// ---------------------------------------------------------------- logging

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_log(level: c_int, msg: *const c_char) {
    // SAFETY: uACPI passes a NUL-terminated, pre-formatted message.
    let text = unsafe { CStr::from_ptr(msg) }.to_str().unwrap_or("(invalid UTF-8)").trim_end();
    match level {
        1 | 2 => crate::log::warn(&alloc::format!("acpi: {text}")),
        3 => crate::log::info(&alloc::format!("acpi: {text}")),
        _ => {} // debug and trace
    }
}

// ---------------------------------------------------------------- PCI configuration space

#[repr(C)]
struct PciAddress {
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_device_open(address: PciAddress, out: *mut *mut c_void) -> Status {
    if address.segment != 0 {
        return UNIMPLEMENTED;
    }
    let handle = (address.bus as usize) << 8 | (address.device as usize) << 3 | address.function as usize;
    // SAFETY: valid out pointer. The handle is the bus/devfn itself, plus 1 (never null).
    unsafe { *out = (handle + 1) as *mut c_void };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_pci_device_close(_handle: *mut c_void) {}

fn pci_handle(h: *mut c_void) -> (u8, u8) {
    let v = h as usize - 1;
    ((v >> 8) as u8, v as u8)
}

macro_rules! pci_access {
    ($read:ident, $write:ident, $ty:ty, $width:literal) => {
        #[unsafe(no_mangle)]
        extern "C" fn $read(device: *mut c_void, offset: usize, value: *mut $ty) -> Status {
            if offset + $width > 256 {
                return INVALID_ARGUMENT;
            }
            let (bus, devfn) = pci_handle(device);
            // SAFETY: valid out pointer.
            unsafe { *value = crate::pci::config_read(bus, devfn, offset as u16, $width) as $ty };
            OK
        }

        #[unsafe(no_mangle)]
        extern "C" fn $write(device: *mut c_void, offset: usize, value: $ty) -> Status {
            if offset + $width > 256 {
                return INVALID_ARGUMENT;
            }
            let (bus, devfn) = pci_handle(device);
            crate::pci::config_write(bus, devfn, offset as u16, $width, value as u32);
            OK
        }
    };
}

pci_access!(uacpi_kernel_pci_read8, uacpi_kernel_pci_write8, u8, 1);
pci_access!(uacpi_kernel_pci_read16, uacpi_kernel_pci_write16, u16, 2);
pci_access!(uacpi_kernel_pci_read32, uacpi_kernel_pci_write32, u32, 4);

// ---------------------------------------------------------------- port I/O

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_map(base: u64, _len: usize, out: *mut *mut c_void) -> Status {
    // SAFETY: valid out pointer; the handle is the base port + 1 (never null).
    unsafe { *out = (base as usize + 1) as *mut c_void };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_unmap(_handle: *mut c_void) {}

fn io_port(handle: *mut c_void, offset: usize) -> u16 {
    (handle as usize - 1 + offset) as u16
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_read8(h: *mut c_void, offset: usize, out: *mut u8) -> Status {
    // SAFETY: port I/O on firmware-described registers; valid out pointer.
    unsafe { *out = crate::port::inb(io_port(h, offset)) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_read16(h: *mut c_void, offset: usize, out: *mut u16) -> Status {
    let port = io_port(h, offset);
    let v: u16;
    // SAFETY: as above.
    unsafe {
        core::arch::asm!("in ax, dx", out("ax") v, in("dx") port, options(nomem, nostack, preserves_flags));
        *out = v;
    }
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_read32(h: *mut c_void, offset: usize, out: *mut u32) -> Status {
    let port = io_port(h, offset);
    let v: u32;
    // SAFETY: as above.
    unsafe {
        core::arch::asm!("in eax, dx", out("eax") v, in("dx") port, options(nomem, nostack, preserves_flags));
        *out = v;
    }
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_write8(h: *mut c_void, offset: usize, value: u8) -> Status {
    // SAFETY: port I/O on firmware-described registers.
    unsafe { crate::port::outb(io_port(h, offset), value) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_write16(h: *mut c_void, offset: usize, value: u16) -> Status {
    let port = io_port(h, offset);
    // SAFETY: as above.
    unsafe { core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nomem, nostack, preserves_flags)) };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_io_write32(h: *mut c_void, offset: usize, value: u32) -> Status {
    let port = io_port(h, offset);
    // SAFETY: as above.
    unsafe { core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags)) };
    OK
}

// ---------------------------------------------------------------- time

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_get_nanoseconds_since_boot() -> u64 {
    pit::nanos()
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_stall(usec: u8) {
    let end = pit::nanos() + usec as u64 * 1000;
    while pit::nanos() < end {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_sleep(msec: u64) {
    task::sleep_ticks(msec.div_ceil(1000 / pit::HZ) + 1);
}

// ---------------------------------------------------------------- locks and events

/// Wait until `ready()` holds, letting other tasks run; false after `timeout_ms`
/// (0xFFFF = forever, as uACPI defines it).
fn wait_until(timeout_ms: u16, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = (timeout_ms != 0xFFFF).then(|| pit::nanos() + timeout_ms as u64 * 1_000_000);
    loop {
        if ready() {
            return true;
        }
        if deadline.is_some_and(|d| pit::nanos() >= d) {
            return false;
        }
        task::yield_now();
    }
}

fn thread_id() -> u64 {
    task::current_id() as u64 + 1
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_create_mutex() -> *mut c_void {
    Box::into_raw(Box::new(AtomicU64::new(0))) as *mut c_void
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free_mutex(m: *mut c_void) {
    // SAFETY: created by uacpi_kernel_create_mutex.
    drop(unsafe { Box::from_raw(m as *mut AtomicU64) });
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_acquire_mutex(m: *mut c_void, timeout: u16) -> Status {
    // SAFETY: a live mutex handle.
    let owner = unsafe { &*(m as *const AtomicU64) };
    let me = thread_id();
    if wait_until(timeout, || owner.compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed).is_ok()) { OK } else { TIMEOUT }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_release_mutex(m: *mut c_void) {
    // SAFETY: a live mutex handle.
    unsafe { &*(m as *const AtomicU64) }.store(0, Ordering::Release);
}

/// Events are counting semaphores.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_create_event() -> *mut c_void {
    Box::into_raw(Box::new(AtomicU64::new(0))) as *mut c_void
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free_event(e: *mut c_void) {
    // SAFETY: created by uacpi_kernel_create_event.
    drop(unsafe { Box::from_raw(e as *mut AtomicU64) });
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_wait_for_event(e: *mut c_void, timeout: u16) -> bool {
    // SAFETY: a live event handle.
    let count = unsafe { &*(e as *const AtomicU64) };
    wait_until(timeout, || count.try_update(Ordering::AcqRel, Ordering::Relaxed, |c| c.checked_sub(1)).is_ok())
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_signal_event(e: *mut c_void) {
    // SAFETY: a live event handle.
    unsafe { &*(e as *const AtomicU64) }.fetch_add(1, Ordering::Release);
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_reset_event(e: *mut c_void) {
    // SAFETY: a live event handle.
    unsafe { &*(e as *const AtomicU64) }.store(0, Ordering::Release);
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_get_thread_id() -> *mut c_void {
    thread_id() as *mut c_void
}

fn save_and_cli() -> u64 {
    let flags: u64;
    // SAFETY: reads RFLAGS, then disables interrupts.
    unsafe { core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem)) };
    flags
}

fn restore(flags: u64) {
    if flags & (1 << 9) != 0 {
        // SAFETY: interrupts were enabled before save_and_cli.
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    }
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_disable_interrupts() -> u64 {
    save_and_cli()
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_restore_interrupts(state: u64) {
    restore(state)
}

/// Spinlocks only need to keep interrupt handlers out on one CPU.
#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_create_spinlock() -> *mut c_void {
    Box::into_raw(Box::new(0u8)) as *mut c_void
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_free_spinlock(s: *mut c_void) {
    // SAFETY: created by uacpi_kernel_create_spinlock.
    drop(unsafe { Box::from_raw(s as *mut u8) });
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_lock_spinlock(_s: *mut c_void) -> u64 {
    save_and_cli()
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_unlock_spinlock(_s: *mut c_void, flags: u64) {
    restore(flags)
}

// ---------------------------------------------------------------- firmware requests

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_handle_firmware_request(_request: *mut c_void) -> Status {
    // AML Breakpoint and Fatal opcodes: nothing to do but carry on.
    OK
}

// ---------------------------------------------------------------- interrupts and work

type IrqHandler = extern "C" fn(*mut c_void) -> u32;

/// The SCI (ACPI's own interrupt): one handler, on an ISA line or GSI.
static SCI: SpinLock<Option<(u32, IrqHandler, usize)>> = SpinLock::new(None);

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_install_interrupt_handler(irq: u32, handler: IrqHandler, ctx: *mut c_void, out: *mut *mut c_void) -> Status {
    // With interrupts off, so the SCI handler never finds the lock taken.
    let flags = save_and_cli();
    *SCI.lock() = Some((irq, handler, ctx as usize));
    restore(flags);
    crate::apic::route_sci(irq);
    // SAFETY: valid out pointer.
    unsafe { *out = (irq as usize + 1) as *mut c_void };
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_uninstall_interrupt_handler(_handler: IrqHandler, _irq_handle: *mut c_void) -> Status {
    let flags = save_and_cli();
    *SCI.lock() = None;
    restore(flags);
    OK
}

/// From the IRQ dispatcher (interrupts off): run the SCI handler if `irq` is its line.
pub fn interrupt(irq: u32) -> bool {
    let Some(sci) = SCI.try_lock() else { return false };
    match *sci {
        Some((line, handler, ctx)) if line == irq => {
            handler(ctx as *mut c_void);
            true
        }
        _ => false,
    }
}

type WorkHandler = extern "C" fn(*mut c_void);

/// Deferred ACPI work (GPE methods, notifications), queued from the SCI handler.
static WORK: SpinLock<VecDeque<(WorkHandler, usize)>> = SpinLock::new(VecDeque::new());
static WORK_RUNNING: AtomicBool = AtomicBool::new(false);

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_schedule_work(_kind: c_int, handler: WorkHandler, ctx: *mut c_void) -> Status {
    // The SCI handler runs with interrupts off and never while this lock is held by a
    // task (tasks only hold it with interrupts off too).
    let flags = save_and_cli();
    if let Some(mut work) = WORK.try_lock() {
        work.push_back((handler, ctx as usize));
    }
    restore(flags);
    OK
}

#[unsafe(no_mangle)]
extern "C" fn uacpi_kernel_wait_for_work_completion() -> Status {
    wait_until(0xFFFF, || {
        let flags = save_and_cli();
        let idle = WORK.try_lock().is_some_and(|w| w.is_empty()) && !WORK_RUNNING.load(Ordering::Acquire);
        restore(flags);
        idle
    });
    OK
}

fn work_task() {
    loop {
        let flags = save_and_cli();
        let next = WORK.try_lock().and_then(|mut w| w.pop_front());
        if next.is_some() {
            WORK_RUNNING.store(true, Ordering::Release);
        }
        restore(flags);
        match next {
            Some((handler, ctx)) => {
                handler(ctx as *mut c_void);
                WORK_RUNNING.store(false, Ordering::Release);
            }
            None => task::wait_event(),
        }
    }
}
