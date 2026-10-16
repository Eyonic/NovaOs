//! Files of Linux drivers for Linux programs (FEATURES=linux-drivers): DRM device nodes
//! (`/dev/dri/card0`, `/dev/dri/renderD128`), and the dma-buf and sync-file descriptors
//! DRM hands out, as ordinary descriptors of Nova's Linux programs.
//!
//! The Linux side is lx_emul's user.c: a descriptor here ([`LxFile`]) holds a `struct
//! lx_ufile`, which owns a reference to the driver's `struct file`. System calls become
//! its file operations (ioctl, compat_ioctl for 32-bit programs, read, write, llseek,
//! poll); the descriptor closes with the last [`OpenFile`] reference, so dup, fork and
//! SCM_RIGHTS share one open file as on Linux. Descriptors Linux code creates
//! (`fd_install`) land in the calling program's table, and `fget` finds the file behind
//! one a program passes in.
//!
//! mmap of such a file makes a Linux `vm_area_struct` for the Nova mapping
//! ([`process::Memory`] keeps it in `lx_vmas`). Its pages are mapped by the driver's
//! fault handler through [`nova_vma_insert`] when the program (or Linux code on its
//! behalf) touches them, and zapped again by `unmap_mapping_range` ([`nova_vma_zap`]),
//! after which they fault anew. munmap and exit close the vma once no fault is in flight.
//!
//! The device nodes appear in `/dev/dri`, and in `/sys` as libdrm and Mesa look for them
//! ([`sys_node`]): `/sys/dev/char/226:N`, `/sys/class/drm/*`, and the parent device
//! (a PCI function with its vendor and device ids, or a faux device for vgem and vkms).

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::ffi::{c_int, c_long, c_void};
use core::sync::atomic::{AtomicUsize, Ordering};

use super::{fds, new_file, FileRef, Kind};
use crate::process::Memory;
use crate::sync::SpinLock;
use crate::{task, vfs};

const EBADF: i64 = 9;
const EMFILE: i64 = 24;

#[repr(C)]
struct DevnodeC {
    path: [u8; 64],
    name: [u8; 32],
    parent: [u8; 64],
    bus: [u8; 16],
    driver: [u8; 32],
    major: u32,
    minor: u32,
    vendor: u16,
    device: u16,
    subvendor: u16,
    subdevice: u16,
    class_code: u32,
    revision: u8,
}

#[repr(C)]
struct UfileInfo {
    mode: u32,
    rdev_major: u32,
    rdev_minor: u32,
    ino: u64,
    name: [u8; 32],
    anon: bool,
}

unsafe extern "C" {
    fn nova_lx_devnode(index: u32, out: *mut DevnodeC) -> bool;
    fn nova_lx_ufile_open(devt: u32, flags: u32, err: *mut c_int) -> *mut c_void;
    fn nova_lx_ufile_close(uf: *mut c_void);
    fn nova_lx_ufile_ioctl(uf: *mut c_void, cmd: u32, arg: u64, compat: bool) -> c_long;
    fn nova_lx_ufile_read(uf: *mut c_void, buf: u64, len: u64) -> c_long;
    fn nova_lx_ufile_write(uf: *mut c_void, buf: u64, len: u64) -> c_long;
    fn nova_lx_ufile_llseek(uf: *mut c_void, offset: i64, whence: c_int) -> i64;
    fn nova_lx_ufile_poll(uf: *mut c_void, events: *mut u64) -> u32;
    fn nova_lx_ufile_setfl(uf: *mut c_void, flags: u32);
    fn nova_lx_ufile_info(uf: *mut c_void, info: *mut UfileInfo);
    fn nova_lx_ufile_get(uf: *mut c_void) -> *mut c_void;
    fn nova_lx_vma_new(uf: *mut c_void, space: u64, addr: u64, len: u64, prot: u32, flags: u32, pgoff: u64, err: *mut c_int) -> *mut c_void;
    fn nova_lx_vma_mmap(vma: *mut c_void) -> c_int;
    fn nova_lx_vma_free(vma: *mut c_void);
    fn nova_lx_vma_writable(vma: *mut c_void) -> bool;
    fn nova_lx_vma_fault(vma: *mut c_void, addr: u64, write: bool) -> u32;
    fn nova_lx_vma_trim(vma: *mut c_void, start: u64, end: u64);
    fn nova_lx_vma_detach(vma: *mut c_void);
    fn nova_lx_vma_close(vma: *mut c_void);
    fn nova_lx_vma_dup(vma: *mut c_void, space: u64) -> *mut c_void;
    fn nova_lx_put_page(page: *mut c_void);
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

// ---------------------------------------------------------------- device nodes

/// A device node a Linux driver registered (DRM minors), and its parent device.
pub struct Devnode {
    /// Under /dev: "dri/card0".
    pub path: String,
    /// "card0", "renderD128".
    pub name: String,
    /// The parent device: "0000:03:00.0", "vkms".
    pub parent: String,
    /// The parent's bus: "pci", "faux", "platform".
    pub bus: String,
    pub driver: String,
    pub major: u32,
    pub minor: u32,
    pub vendor: u16,
    pub device: u16,
    pub subvendor: u16,
    pub subdevice: u16,
    pub class_code: u32,
    pub revision: u8,
}

/// The DRM device nodes (/dev/dri/*), in the order the drivers registered them.
pub fn devnodes() -> Vec<Devnode> {
    let mut out = Vec::new();
    for index in 0.. {
        // SAFETY: lx_emul fills the plain-data record; false past the end.
        let mut d: DevnodeC = unsafe { core::mem::zeroed() };
        if !unsafe { nova_lx_devnode(index, &mut d) } {
            break;
        }
        let path = cstr(&d.path);
        if !path.starts_with("dri/") {
            continue; // only DRM's nodes are offered to programs so far
        }
        out.push(Devnode {
            path,
            name: cstr(&d.name),
            parent: cstr(&d.parent),
            bus: cstr(&d.bus),
            driver: cstr(&d.driver),
            major: d.major,
            minor: d.minor,
            vendor: d.vendor,
            device: d.device,
            subvendor: d.subvendor,
            subdevice: d.subdevice,
            class_code: d.class_code,
            revision: d.revision,
        });
    }
    out
}

/// st_rdev as Linux's stat() reports it (new_encode_dev), which glibc's major()/minor()
/// decode.
pub fn encode_dev(major: u32, minor: u32) -> u64 {
    (minor & 0xFF) as u64 | (major as u64) << 8 | ((minor & !0xFF) as u64) << 12
}

// ---------------------------------------------------------------- open files

/// A program's descriptor of a Linux driver's file: owns a `struct lx_ufile`.
pub struct LxFile {
    uf: *mut c_void,
}

// SAFETY: the lx_ufile is only used through lx_emul's calls; Nova runs on one CPU.
unsafe impl Send for LxFile {}

impl Drop for LxFile {
    fn drop(&mut self) {
        // SAFETY: the last reference to this descriptor's lx_ufile; may run the driver's
        // release (DRM drops the client's buffers there).
        unsafe { nova_lx_ufile_close(self.uf) }
    }
}

/// Open device node major:minor (/dev/dri/*) with open(2) `flags`.
pub fn open(major: u32, minor: u32, flags: u64) -> Result<Kind, i64> {
    let devt = major << 20 | minor;
    let mut err = 0;
    // SAFETY: runs the driver's open (drm_open) for a new struct file.
    let uf = unsafe { nova_lx_ufile_open(devt, flags as u32, &mut err) };
    if uf.is_null() {
        return Err(-err as i64);
    }
    Ok(Kind::Linux(LxFile { uf }))
}

/// The lx_ufile behind `file`, if it is a Linux driver's file. The caller keeps `file`
/// (an Arc) alive while it uses the pointer, so the descriptor cannot close meanwhile;
/// the OpenFile lock is not held, since driver calls can block.
fn ufile(file: &FileRef) -> Option<*mut c_void> {
    match &file.lock().kind {
        Kind::Linux(l) => Some(l.uf),
        _ => None,
    }
}

pub fn is_linux(file: &FileRef) -> bool {
    ufile(file).is_some()
}

fn ret(r: i64) -> Result<i64, i64> {
    if r < 0 { Err(-r) } else { Ok(r) }
}

/// ioctl on a Linux file (compat_ioctl for 32-bit programs); None if it is not one.
pub fn ioctl(file: &FileRef, cmd: u64, arg: u64, compat: bool) -> Option<Result<i64, i64>> {
    let uf = ufile(file)?;
    // SAFETY: `file` keeps the lx_ufile alive; the driver validates `arg` itself
    // (copy_from_user against the program's mappings).
    Some(ret(unsafe { nova_lx_ufile_ioctl(uf, cmd as u32, arg, compat) }))
}

/// read(2) into the program's buffer at `buf` (DRM events); None if not a Linux file.
pub fn read(file: &FileRef, buf: u64, len: u64) -> Option<Result<i64, i64>> {
    let uf = ufile(file)?;
    // SAFETY: as for ioctl; the driver copies to user memory with copy_to_user.
    Some(ret(unsafe { nova_lx_ufile_read(uf, buf, len) }))
}

pub fn write(file: &FileRef, buf: u64, len: u64) -> Option<Result<i64, i64>> {
    let uf = ufile(file)?;
    // SAFETY: as for read.
    Some(ret(unsafe { nova_lx_ufile_write(uf, buf, len) }))
}

/// lseek (a dma-buf reports its size at SEEK_END, which Mesa relies on).
pub fn llseek(file: &FileRef, offset: i64, whence: u64) -> Option<Result<i64, i64>> {
    let uf = ufile(file)?;
    // SAFETY: as for ioctl.
    Some(ret(unsafe { nova_lx_ufile_llseek(uf, offset, whence as c_int) }))
}

/// Ready events and a counter of the file's wakeups (epoll's edge detection).
pub fn poll(file: &FileRef) -> Option<(u32, u64)> {
    let uf = ufile(file)?;
    let mut events = 0;
    // SAFETY: as for ioctl. The first poll puts the file on the driver's wait queues.
    let mask = unsafe { nova_lx_ufile_poll(uf, &mut events) };
    Some((mask, events))
}

/// fcntl(F_SETFL) and FIONBIO: the driver sees O_NONBLOCK in f_flags.
pub fn set_flags(file: &FileRef, flags: u64) {
    if let Some(uf) = ufile(file) {
        // SAFETY: as for ioctl; updates f_flags under the file's lock.
        unsafe { nova_lx_ufile_setfl(uf, flags as u32) }
    }
}

fn info(l: &LxFile) -> UfileInfo {
    // SAFETY: plain-data out record.
    let mut i: UfileInfo = unsafe { core::mem::zeroed() };
    unsafe { nova_lx_ufile_info(l.uf, &mut i) };
    i
}

/// fstat: a character device for device nodes, an anonymous-inode file otherwise.
pub fn stat(l: &LxFile) -> Vec<u8> {
    let i = info(l);
    let mut b = super::stat_buf(if i.anon { 0o600 } else { i.mode }, 0, 0, i.ino);
    b[40..48].copy_from_slice(&encode_dev(i.rdev_major, i.rdev_minor).to_le_bytes());
    b
}

/// What /proc/self/fd/N reads as: "/dev/dri/card0", "anon_inode:dmabuf".
pub fn name(l: &LxFile) -> String {
    let i = info(l);
    let name = cstr(&i.name);
    if !i.anon {
        return devnodes().into_iter().find(|d| d.major == i.rdev_major && d.minor == i.rdev_minor).map_or(format!("/dev/{name}"), |d| format!("/dev/{}", d.path));
    }
    format!("anon_inode:{name}")
}

pub fn kind_name(l: &LxFile) -> String {
    format!("linux {}", name(l))
}

// ---------------------------------------------------------------- descriptors made by Linux code

/// get_unused_fd_flags(): a slot of the calling program's descriptor table.
#[unsafe(no_mangle)]
extern "C" fn nova_fd_reserve(cloexec: bool) -> c_int {
    if task::current_user().is_none() {
        return -EBADF as c_int;
    }
    match fds().reserve(cloexec) {
        Some(fd) => fd as c_int,
        None => -EMFILE as c_int,
    }
}

#[unsafe(no_mangle)]
extern "C" fn nova_fd_unreserve(fd: c_int) {
    if task::current_user().is_some() {
        fds().unreserve(fd as usize);
    }
}

/// fd_install(): the reserved slot gets the file (the lx_ufile's reference moves here).
#[unsafe(no_mangle)]
extern "C" fn nova_fd_install(fd: c_int, uf: *mut c_void) {
    let file = LxFile { uf };
    if task::current_user().is_none() {
        return; // dropping `file` closes it again
    }
    // Access mode and O_NONBLOCK as the driver set them (dma-buf: O_RDWR | O_CLOEXEC).
    let f = new_file(Kind::Linux(file), 2);
    fds().fill_reserved(fd as usize, f);
}

/// fget(): the struct file behind a program's descriptor, with a reference; NULL if it
/// is not a Linux driver's file.
#[unsafe(no_mangle)]
extern "C" fn nova_fd_get(fd: c_int) -> *mut c_void {
    if task::current_user().is_none() {
        return core::ptr::null_mut();
    }
    let Ok(file) = fds().get(fd as i64) else { return core::ptr::null_mut() };
    match &file.lock().kind {
        // SAFETY: the descriptor's lx_ufile is alive (we hold its OpenFile); get_file.
        Kind::Linux(l) => unsafe { nova_lx_ufile_get(l.uf) },
        _ => core::ptr::null_mut(),
    }
}

/// A wakeup on a wait queue a Linux file is polled on (any context, interrupts too):
/// waiters in poll/select/epoll re-check, as after any interrupt.
#[unsafe(no_mangle)]
extern "C" fn nova_poll_wake() {
    task::IRQ_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// copy_from_user()/copy_to_user()/get_user(): is the range mapped in the calling
/// program (writable if asked)? Pages not present yet are filled when Linux code touches
/// them (process::kernel_user_fault).
#[unsafe(no_mangle)]
extern "C" fn nova_user_range(addr: u64, len: u64, write: bool) -> bool {
    let Some(u) = task::current_user() else { return false };
    u.process.mem.lock().accessible(addr, len, write)
}

// ---------------------------------------------------------------- mappings

/// Faults being served by Linux code: vmas are only closed while there are none (a
/// fault handler may sleep; the vma must outlive it).
static FAULTS: AtomicUsize = AtomicUsize::new(0);
/// Vmas waiting to be closed (unmapped while the address-space lock was held).
static DEAD: SpinLock<Vec<usize>> = SpinLock::new(Vec::new());

fn space_of(mem: &SpinLock<Memory>) -> u64 {
    mem as *const SpinLock<Memory> as u64
}

/// mmap of a Linux file at `addr` (already reserved as an area of `len` bytes).
pub fn mmap(file: &FileRef, addr: u64, len: u64, prot: u64, flags: u64, off: u64) -> Option<Result<(), i64>> {
    let uf = ufile(file)?;
    let p = &task::current_user()?.process;
    let mut err = 0;
    // SAFETY: a new vma for this file (access checked against the file's mode).
    let vma = unsafe { nova_lx_vma_new(uf, space_of(&p.mem), addr, len, prot as u32, flags as u32, off >> 12, &mut err) };
    if vma.is_null() {
        p.mem.lock().unmap(addr, len);
        return Some(Err(-err as i64));
    }
    // Registered before the driver's mmap runs: remap_pfn_range() inserts right away.
    p.mem.lock().lx_vmas.insert(addr, (addr + len, vma as usize));
    // SAFETY: runs f_op->mmap; the driver's inserts reach this address space through
    // `space` and are checked against the vma registered above.
    let err = unsafe { nova_lx_vma_mmap(vma) };
    let mut m = p.mem.lock();
    if err != 0 {
        m.lx_vmas.remove(&addr);
        m.unmap(addr, len); // also removes what the driver had mapped already
        drop(m);
        // SAFETY: the driver's mmap failed, so there is no vm_ops->close to run.
        unsafe { nova_lx_vma_free(vma) };
        return Some(Err(-err as i64));
    }
    // SAFETY: the vma just made.
    if !unsafe { nova_lx_vma_writable(vma) } {
        m.protect(addr, len, false);
    }
    Some(Ok(()))
}

/// A page fault (from the program, or from Linux code touching its memory) in Linux
/// mapping `vma`. Called without the address-space lock. False: SIGBUS/SIGSEGV.
pub fn fault(vma: usize, addr: u64, write: bool) -> bool {
    FAULTS.fetch_add(1, Ordering::SeqCst);
    // SAFETY: vmas are not closed while FAULTS is non-zero.
    let r = unsafe { nova_lx_vma_fault(vma as *mut c_void, addr, write) };
    FAULTS.fetch_sub(1, Ordering::SeqCst);
    if r != 0 {
        crate::log::warn(&format!("linux: fault at {addr:#x} in a driver mapping failed ({r:#x})"));
    }
    r == 0
}

/// Detach vma `v` from Nova's address space (it maps nothing any more) and queue it to be
/// closed once no fault is in flight. Called with the address-space lock held: calls no
/// driver code.
pub fn retire(v: usize) {
    // SAFETY: a plain store in the vma; no driver code runs.
    unsafe { nova_lx_vma_detach(v as *mut c_void) };
    DEAD.lock().push(v);
}

/// munmap of the front or back of a mapping: the vma covers [start, end) now.
pub fn trim(v: usize, start: u64, end: u64) {
    // SAFETY: plain stores in the vma; no driver code runs.
    unsafe { nova_lx_vma_trim(v as *mut c_void, start, end) }
}

/// Close retired vmas (vm_ops->close): after system calls, without any lock held.
pub fn reap() {
    if FAULTS.load(Ordering::SeqCst) != 0 {
        return;
    }
    let dead = core::mem::take(&mut *DEAD.lock());
    for v in dead {
        // SAFETY: retired (detached) and no fault is using it.
        unsafe { nova_lx_vma_close(v as *mut c_void) }
    }
}

/// Whether there are vmas to close (cheap check for the system-call path).
pub fn reap_pending() -> bool {
    DEAD.try_lock().is_some_and(|d| !d.is_empty())
}

/// fork: give the child's address space its own vmas for the parent's Linux mappings
/// (pages fault in again there). Runs the drivers' vm_ops->open, so no lock is held.
pub fn fork(parent: &SpinLock<Memory>, child: &SpinLock<Memory>) {
    let vmas: Vec<(u64, u64, usize)> = parent.lock().lx_vmas.iter().map(|(&s, &(e, v))| (s, e, v)).collect();
    for (start, end, v) in vmas {
        // SAFETY: the parent's vma is alive (we are its process's thread, in fork).
        let n = unsafe { nova_lx_vma_dup(v as *mut c_void, space_of(child)) };
        let mut c = child.lock();
        if n.is_null() {
            c.unmap(start, end - start);
        } else {
            c.lx_vmas.insert(start, (end, n as usize));
        }
    }
}

/// The struct page reference a mapping kept, when the page leaves it.
pub fn put_page(page: usize) {
    if page != 0 {
        // SAFETY: a reference taken by lx_emul for this mapping.
        unsafe { nova_lx_put_page(page as *mut c_void) }
    }
}

/// Map a frame for Linux code (vmf_insert_pfn and friends) in address space `space`,
/// inside vma `vma`. False if the vma is no longer mapped there.
#[unsafe(no_mangle)]
extern "C" fn nova_vma_insert(space: u64, vma: usize, va: u64, pfn: u64, flags: u64, page: usize) -> bool {
    // SAFETY: `space` is a live address space: vmas are detached (space 0) before their
    // Memory goes, and lx_emul passes only attached ones.
    let mem = unsafe { &*(space as *const SpinLock<Memory>) };
    mem.lock().lx_insert(vma, va, pfn << 12, flags, page)
}

/// unmap_mapping_range(): remove the mappings of vma `vma` in [start, end).
#[unsafe(no_mangle)]
extern "C" fn nova_vma_zap(space: u64, vma: usize, start: u64, end: u64) {
    // SAFETY: as for nova_vma_insert.
    let mem = unsafe { &*(space as *const SpinLock<Memory>) };
    mem.lock().lx_zap(vma, start, end);
}

// ---------------------------------------------------------------- /dev and /sys

/// /dev entries: "dri" and its nodes (name, major, minor).
pub fn dev_entries(dir: &str) -> Vec<(String, u32, u32)> {
    devnodes()
        .into_iter()
        .filter_map(|d| {
            let (parent, name) = d.path.rsplit_once('/')?;
            (parent == dir).then(|| (name.to_string(), d.major, d.minor))
        })
        .collect()
}

/// A /dev path that is a Linux device node: (major, minor).
pub fn dev_node(path: &str) -> Option<(u32, u32)> {
    devnodes().into_iter().find(|d| d.path == path).map(|d| (d.major, d.minor))
}

/// A node of the DRM part of /sys.
pub enum SysNode {
    Dir,
    File(Vec<u8>),
    Link(String),
}

/// /sys path of a device node's parent device.
fn parent_dir(d: &Devnode) -> String {
    match d.bus.as_str() {
        "pci" => format!("devices/pci0000:00/{}", d.parent),
        "" => format!("devices/virtual/{}", d.parent),
        bus => format!("devices/{bus}/{}", d.parent),
    }
}

/// PCI configuration space of function "0000:BB:DD.F" (the first 256 bytes).
fn pci_config(slot: &str) -> Vec<u8> {
    let parse = || -> Option<(u8, u8)> {
        let rest = slot.strip_prefix("0000:")?;
        let (bus, rest) = rest.split_once(':')?;
        let (dev, func) = rest.split_once('.')?;
        Some((u8::from_str_radix(bus, 16).ok()?, u8::from_str_radix(dev, 16).ok()? << 3 | u8::from_str_radix(func, 16).ok()?))
    };
    let Some((bus, devfn)) = parse() else { return Vec::new() };
    (0..64).flat_map(|i| crate::pci::config_read(bus, devfn, i * 4, 4).to_le_bytes()).collect()
}

/// Every node of the DRM part of /sys, by path relative to /sys.
fn sys_tree() -> BTreeMap<String, SysNode> {
    let mut t = BTreeMap::new();
    let file = |s: String| SysNode::File(s.into_bytes());
    for d in devnodes() {
        let pdir = parent_dir(&d);
        if !t.contains_key(&pdir) {
            t.insert(pdir.clone(), SysNode::Dir);
            let depth = pdir.split('/').count();
            let up = "../".repeat(depth);
            t.insert(format!("{pdir}/subsystem"), SysNode::Link(format!("{up}bus/{}", d.bus)));
            t.insert(format!("{pdir}/drm"), SysNode::Dir);
            if !d.driver.is_empty() {
                t.insert(format!("{pdir}/driver"), SysNode::Link(format!("{up}bus/{}/drivers/{}", d.bus, d.driver)));
                t.insert(format!("bus/{}/drivers/{}", d.bus, d.driver), SysNode::Dir);
            }
            t.insert(format!("bus/{}/devices/{}", d.bus, d.parent), SysNode::Link(format!("../../../{pdir}")));
            if d.bus == "pci" {
                let cc = d.class_code;
                let uevent = format!(
                    "DRIVER={}\nPCI_CLASS={cc:X}\nPCI_ID={:04X}:{:04X}\nPCI_SUBSYS_ID={:04X}:{:04X}\nPCI_SLOT_NAME={}\nMODALIAS=pci:v{:08X}d{:08X}sv{:08X}sd{:08X}bc{:02X}sc{:02X}i{:02X}\n",
                    d.driver, d.vendor, d.device, d.subvendor, d.subdevice, d.parent, d.vendor, d.device, d.subvendor, d.subdevice, cc >> 16, (cc >> 8) & 0xFF, cc & 0xFF
                );
                t.insert(format!("{pdir}/uevent"), file(uevent));
                t.insert(format!("{pdir}/vendor"), file(format!("0x{:04x}\n", d.vendor)));
                t.insert(format!("{pdir}/device"), file(format!("0x{:04x}\n", d.device)));
                t.insert(format!("{pdir}/subsystem_vendor"), file(format!("0x{:04x}\n", d.subvendor)));
                t.insert(format!("{pdir}/subsystem_device"), file(format!("0x{:04x}\n", d.subdevice)));
                t.insert(format!("{pdir}/revision"), file(format!("0x{:02x}\n", d.revision)));
                t.insert(format!("{pdir}/class"), file(format!("0x{cc:06x}\n")));
                // Mesa and Vulkan's device-select layer prefer the boot display.
                t.insert(format!("{pdir}/boot_vga"), file(format!("{}\n", (cc >> 8 == 0x0300) as u8)));
                t.insert(format!("{pdir}/config"), SysNode::File(pci_config(&d.parent)));
            } else {
                let driver = if d.bus == "faux" { "faux_driver" } else { d.driver.as_str() };
                t.insert(format!("{pdir}/uevent"), file(format!("DRIVER={driver}\nMODALIAS={}:{}\n", d.bus, d.parent)));
            }
        }
        let node = format!("{pdir}/drm/{}", d.name);
        let (maj, min) = (d.major, d.minor);
        let devtype = "drm_minor";
        t.insert(node.clone(), SysNode::Dir);
        t.insert(format!("{node}/uevent"), file(format!("MAJOR={maj}\nMINOR={min}\nDEVNAME={}\nDEVTYPE={devtype}\n", d.path)));
        t.insert(format!("{node}/dev"), file(format!("{maj}:{min}\n")));
        t.insert(format!("{node}/device"), SysNode::Link("../..".into()));
        let up = "../".repeat(node.split('/').count());
        t.insert(format!("{node}/subsystem"), SysNode::Link(format!("{up}class/drm")));
        t.insert(format!("class/drm/{}", d.name), SysNode::Link(format!("../../{node}")));
        t.insert(format!("dev/char/{maj}:{min}"), SysNode::Link(format!("../../{node}")));
    }
    // Directories implied by the paths.
    let dirs: Vec<String> = t
        .keys()
        .flat_map(|k| {
            let parts: Vec<&str> = k.split('/').collect();
            (1..parts.len()).map(move |n| parts[..n].join("/")).collect::<Vec<_>>()
        })
        .collect();
    for d in dirs {
        t.entry(d).or_insert(SysNode::Dir);
    }
    t
}

/// The node at `p` (relative to /sys) if it belongs to the DRM devices.
pub fn sys_node(p: &str) -> Option<SysNode> {
    sys_tree().remove(p)
}

/// Entries of /sys directory `p` contributed by the DRM devices.
pub fn sys_list(p: &str) -> Vec<(String, vfs::Type)> {
    let prefix = if p.is_empty() { String::new() } else { format!("{p}/") };
    sys_tree()
        .into_iter()
        .filter_map(|(k, n)| {
            let name = k.strip_prefix(prefix.as_str())?;
            (!name.contains('/') && !name.is_empty()).then(|| {
                let ty = match n {
                    SysNode::Dir => vfs::Type::Dir,
                    SysNode::File(_) => vfs::Type::Reg,
                    SysNode::Link(_) => vfs::Type::Lnk,
                };
                (name.to_string(), ty)
            })
        })
        .collect()
}
