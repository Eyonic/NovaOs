//! PCI bus enumeration through the legacy configuration mechanism (ports 0xCF8/0xCFC).
//!
//! Mechanism #1 reaches the first 256 bytes of every function's configuration space.
//! When ACPI's MCFG describes PCIe ECAM, all accesses go through that memory-mapped
//! window instead, which reaches the whole 4 KiB (extended capabilities).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl core::fmt::Display for Address {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

fn outl(port: u16, value: u32) {
    // SAFETY: PCI configuration ports only.
    unsafe { core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags)) }
}

fn inl(port: u16) -> u32 {
    let value: u32;
    // SAFETY: PCI configuration ports only.
    unsafe { core::arch::asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack, preserves_flags)) };
    value
}

impl Address {
    fn devfn(&self) -> u8 {
        self.device << 3 | self.function
    }

    pub fn read32(&self, offset: u8) -> u32 {
        config_read(self.bus, self.devfn(), offset as u16 & !3, 4)
    }

    pub fn write32(&self, offset: u8, value: u32) {
        config_write(self.bus, self.devfn(), offset as u16 & !3, 4, value)
    }

    pub fn read16(&self, offset: u8) -> u16 {
        (self.read32(offset) >> ((offset & 2) * 8)) as u16
    }

    pub fn read8(&self, offset: u8) -> u8 {
        (self.read32(offset) >> ((offset & 3) * 8)) as u8
    }

    pub fn write16(&self, offset: u8, value: u16) {
        let shift = (offset & 2) * 8;
        let old = self.read32(offset) & !(0xFFFF << shift);
        self.write32(offset, old | (value as u32) << shift);
    }
}

/// Select a configuration register and return the data port for an access of `offset`'s
/// width: byte and word accesses use the matching port offset, with no read-modify-write
/// (which would clear write-1-to-clear status bits in the same dword).
fn select(bus: u8, devfn: u8, offset: u16) -> u16 {
    outl(CONFIG_ADDRESS, 1 << 31 | (bus as u32) << 16 | (devfn as u32) << 8 | (offset as u32 & 0xFC));
    CONFIG_DATA + (offset & 3)
}

/// PCIe enhanced configuration access (ECAM, from ACPI's MCFG): the virtual address of
/// bus `ECAM_FIRST_BUS`'s configuration space, 0 when there is none (legacy ports only).
static ECAM: AtomicU64 = AtomicU64::new(0);
static ECAM_BUSES: AtomicU32 = AtomicU32::new(0); // first bus << 8 | last bus

/// Use ECAM for segment 0 if the MCFG describes it. Returns the bus range mapped.
pub fn init_ecam(regions: &[(u64, u16, u8, u8)]) -> Option<(u8, u8)> {
    let &(base, _, first, last) = regions.iter().find(|r| r.1 == 0)?;
    let virt = crate::memory::map_mmio(base + ((first as u64) << 20), ((last - first) as u64 + 1) << 20);
    ECAM_BUSES.store((first as u32) << 8 | last as u32, Ordering::Relaxed);
    ECAM.store(virt, Ordering::Release);
    Some((first, last))
}

/// The ECAM address of a register, if ECAM covers `bus`.
fn ecam(bus: u8, devfn: u8, offset: u16) -> Option<u64> {
    let base = ECAM.load(Ordering::Acquire);
    let range = ECAM_BUSES.load(Ordering::Relaxed);
    let (first, last) = ((range >> 8) as u8, range as u8);
    (base != 0 && bus >= first && bus <= last && offset < 4096)
        .then(|| base + (((bus - first) as u64) << 20 | (devfn as u64) << 12 | offset as u64))
}

/// Size of a function's configuration space: 4096 bytes through ECAM, else 256.
pub fn config_size(bus: u8) -> u16 {
    if ecam(bus, 0, 0).is_some() { 4096 } else { 256 }
}

/// Read 1, 2 or 4 bytes of configuration space (offset < 256, or < 4096 with ECAM) of
/// function `devfn` on `bus`.
pub fn config_read(bus: u8, devfn: u8, offset: u16, width: u8) -> u32 {
    if let Some(addr) = ecam(bus, devfn, offset) {
        // SAFETY: inside the ECAM window mapped uncached by init_ecam; width-matched access.
        return unsafe {
            match width {
                1 => (addr as *const u8).read_volatile() as u32,
                2 => (addr as *const u16).read_volatile() as u32,
                _ => (addr as *const u32).read_volatile(),
            }
        };
    }
    if offset >= 256 {
        return !0;
    }
    let port = select(bus, devfn, offset);
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
            _ => inl(port),
        }
    }
}

/// Write 1, 2 or 4 bytes of configuration space.
pub fn config_write(bus: u8, devfn: u8, offset: u16, width: u8, value: u32) {
    if let Some(addr) = ecam(bus, devfn, offset) {
        // SAFETY: as in config_read.
        unsafe {
            match width {
                1 => (addr as *mut u8).write_volatile(value as u8),
                2 => (addr as *mut u16).write_volatile(value as u16),
                _ => (addr as *mut u32).write_volatile(value),
            }
        }
        return;
    }
    if offset >= 256 {
        return;
    }
    let port = select(bus, devfn, offset);
    // SAFETY: PCI configuration data port, width-matched access.
    unsafe {
        match width {
            1 => core::arch::asm!("out dx, al", in("dx") port, in("al") value as u8, options(nomem, nostack, preserves_flags)),
            2 => core::arch::asm!("out dx, ax", in("dx") port, in("ax") value as u16, options(nomem, nostack, preserves_flags)),
            _ => outl(port, value),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Bar {
    Memory { base: u64, size: u64 },
    Io { port: u16, size: u32 },
}

#[derive(Clone, Debug)]
pub struct Device {
    pub address: Address,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    pub irq_line: u8,
}

impl Device {
    /// Decode BAR `index` (0-5). Sizing briefly writes all-ones to the BAR, so it must
    /// not run while a driver uses the device; drivers call it once during init.
    pub fn bar(&self, index: u8) -> Option<Bar> {
        let offset = 0x10 + index * 4;
        let a = self.address;
        let original = a.read32(offset);
        if original & 1 == 1 {
            a.write32(offset, 0xFFFF_FFFF);
            let size = !(a.read32(offset) & 0xFFFF_FFFC) + 1;
            a.write32(offset, original);
            return Some(Bar::Io { port: (original & 0xFFFC) as u16, size: size & 0xFFFF });
        }
        let is_64 = (original >> 1) & 3 == 2;
        let high = if is_64 { a.read32(offset + 4) } else { 0 };
        a.write32(offset, 0xFFFF_FFFF);
        let mut mask = (a.read32(offset) & 0xFFFF_FFF0) as u64;
        a.write32(offset, original);
        if is_64 {
            a.write32(offset + 4, 0xFFFF_FFFF);
            mask |= (a.read32(offset + 4) as u64) << 32;
            a.write32(offset + 4, high);
        } else {
            mask |= 0xFFFF_FFFF_0000_0000;
        }
        let base = (original & 0xFFFF_FFF0) as u64 | (high as u64) << 32;
        if mask == 0xFFFF_FFFF_0000_0000 || base == 0 {
            return None; // unimplemented or unassigned BAR
        }
        Some(Bar::Memory { base, size: !mask + 1 })
    }

    /// Enable memory/I-O decoding and bus mastering (DMA).
    pub fn enable(&self) {
        let command = self.address.read16(0x04);
        self.address.write16(0x04, command | 0b111);
    }

    pub fn class_name(&self) -> &'static str {
        class_name(self.class, self.subclass, self.prog_if)
    }
}

/// Every function on every bus, depth-first through bridges is unnecessary:
/// scanning all 256 buses directly finds everything.
pub fn scan() -> Vec<Device> {
    let mut found = Vec::new();
    for bus in 0..=255u8 {
        for device in 0..32u8 {
            let functions = {
                let a = Address { bus, device, function: 0 };
                if a.read16(0x00) == 0xFFFF {
                    continue;
                }
                if a.read8(0x0E) & 0x80 != 0 { 8 } else { 1 }
            };
            for function in 0..functions {
                let a = Address { bus, device, function };
                let id = a.read32(0x00);
                if id & 0xFFFF == 0xFFFF {
                    continue;
                }
                let class = a.read32(0x08);
                found.push(Device {
                    address: a,
                    vendor: id as u16,
                    device: (id >> 16) as u16,
                    class: (class >> 24) as u8,
                    subclass: (class >> 16) as u8,
                    prog_if: (class >> 8) as u8,
                    revision: class as u8,
                    irq_line: a.read8(0x3C),
                });
            }
        }
    }
    found
}

pub fn vendor_name(vendor: u16) -> &'static str {
    match vendor {
        0x8086 => "Intel",
        0x1022 => "AMD",
        0x10DE => "NVIDIA",
        0x10EC => "Realtek",
        0x1AF4 => "Red Hat (virtio)",
        0x1B36 => "Red Hat (QEMU)",
        0x1234 => "QEMU/Bochs",
        0x15AD => "VMware",
        0x80EE => "VirtualBox",
        0x14E4 => "Broadcom",
        0x144D => "Samsung",
        0x1B21 => "ASMedia",
        _ => "unknown vendor",
    }
}

pub fn class_name(class: u8, subclass: u8, prog_if: u8) -> &'static str {
    match (class, subclass, prog_if) {
        (0x01, 0x01, _) => "IDE controller",
        (0x01, 0x06, 0x01) => "SATA controller (AHCI)",
        (0x01, 0x06, _) => "SATA controller",
        (0x01, 0x08, 0x02) => "NVMe controller",
        (0x01, 0x00, _) => "SCSI controller",
        (0x01, _, _) => "storage controller",
        (0x02, 0x00, _) => "Ethernet controller",
        (0x02, 0x80, _) => "network controller",
        (0x02, _, _) => "network controller",
        (0x03, 0x00, _) => "VGA display controller",
        (0x03, _, _) => "display controller",
        (0x04, 0x01, _) => "audio device",
        (0x04, 0x03, _) => "HD audio controller",
        (0x04, _, _) => "multimedia device",
        (0x06, 0x00, _) => "host bridge",
        (0x06, 0x01, _) => "ISA/LPC bridge",
        (0x06, 0x04, _) => "PCI bridge",
        (0x06, _, _) => "bridge",
        (0x0C, 0x03, 0x00) => "USB UHCI controller",
        (0x0C, 0x03, 0x10) => "USB OHCI controller",
        (0x0C, 0x03, 0x20) => "USB EHCI controller",
        (0x0C, 0x03, 0x30) => "USB xHCI controller",
        (0x0C, 0x05, _) => "SMBus controller",
        (0x0C, _, _) => "serial bus controller",
        (0x08, _, _) => "system peripheral",
        (0x07, _, _) => "communication controller",
        (0x00, _, _) => "unclassified device",
        _ => "other device",
    }
}
