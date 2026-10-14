//! Intel 8254x / 82574 ("e1000") Gigabit Ethernet driver.
//!
//! Legacy descriptors, polled (device interrupts masked): the network task calls
//! [`E1000::receive`] from its loop. Covers QEMU's `-device e1000` (82540EM) and the
//! 82574L (`e1000e`), which keeps the legacy register set and descriptor format.
//!
//! Reference: Intel PCI/PCI-X Family of Gigabit Ethernet Controllers Software
//! Developer's Manual (8254x), sections 13 (registers) and 3.2-3.4 (descriptors).

use alloc::vec::Vec;

use super::Nic;
use crate::memory::{self, DmaBuffer};
use crate::pci::{Bar, Device};

const CTRL: usize = 0x0000;
const STATUS: usize = 0x0008;
const IMC: usize = 0x00D8;
const RCTL: usize = 0x0100;
const TCTL: usize = 0x0400;
const TIPG: usize = 0x0410;
const RDBAL: usize = 0x2800;
const RDBAH: usize = 0x2804;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const TDBAL: usize = 0x3800;
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const MTA: usize = 0x5200;
const RAL0: usize = 0x5400;
const RAH0: usize = 0x5404;

const CTRL_ASDE: u32 = 1 << 5;
const CTRL_SLU: u32 = 1 << 6;
const CTRL_RST: u32 = 1 << 26;
const RCTL_EN: u32 = 1 << 1;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS: u8 = 1 << 3;
const DESC_DD: u8 = 1 << 0;
const RX_EOP: u8 = 1 << 1;

const RX_COUNT: usize = 64;
const TX_COUNT: usize = 64;
const BUF_SIZE: usize = 2048;

/// PCI device IDs handled by this driver (vendor 0x8086).
pub const DEVICE_IDS: &[(u16, &str)] = &[
    (0x100E, "82540EM"),
    (0x100F, "82545EM"),
    (0x1004, "82543GC"),
    (0x10D3, "82574L"),
    (0x153A, "I217-LM"),
    (0x10EA, "82577LM"),
];

pub struct E1000 {
    regs: u64,
    mac: [u8; 6],
    rx_ring: DmaBuffer,
    rx_bufs: Vec<DmaBuffer>,
    rx_next: usize,
    tx_ring: DmaBuffer,
    tx_bufs: Vec<DmaBuffer>,
    tx_next: usize,
}

impl E1000 {
    fn read(&self, reg: usize) -> u32 {
        // SAFETY: `regs` maps the device's register BAR; reg is a valid register offset.
        unsafe { ((self.regs as usize + reg) as *const u32).read_volatile() }
    }

    fn write(&self, reg: usize, value: u32) {
        // SAFETY: as for read.
        unsafe { ((self.regs as usize + reg) as *mut u32).write_volatile(value) }
    }

    pub fn new(dev: &Device) -> Result<Self, &'static str> {
        let Some(Bar::Memory { base, size }) = dev.bar(0) else { return Err("BAR0 is not a memory BAR") };
        dev.enable();
        let regs = memory::map_mmio(base, size.max(0x20000));
        let mut nic = Self {
            regs,
            mac: [0; 6],
            rx_ring: DmaBuffer::new(RX_COUNT * 16, 128),
            rx_bufs: (0..RX_COUNT).map(|_| DmaBuffer::new(BUF_SIZE, 16)).collect(),
            rx_next: 0,
            tx_ring: DmaBuffer::new(TX_COUNT * 16, 128),
            tx_bufs: (0..TX_COUNT).map(|_| DmaBuffer::new(BUF_SIZE, 16)).collect(),
            tx_next: 0,
        };
        nic.reset();
        nic.read_mac();
        nic.init_rx();
        nic.init_tx();
        Ok(nic)
    }

    fn reset(&mut self) {
        self.write(IMC, 0xFFFF_FFFF);
        self.write(CTRL, self.read(CTRL) | CTRL_RST);
        for _ in 0..100_000 {
            if self.read(CTRL) & CTRL_RST == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.write(IMC, 0xFFFF_FFFF); // reset re-enables nothing, but be explicit
        self.write(CTRL, self.read(CTRL) | CTRL_SLU | CTRL_ASDE);
        for i in 0..128 {
            self.write(MTA + i * 4, 0);
        }
    }

    fn read_mac(&mut self) {
        // The firmware/EEPROM loader fills receive address 0 with the permanent MAC.
        let low = self.read(RAL0);
        let high = self.read(RAH0);
        self.mac = [low as u8, (low >> 8) as u8, (low >> 16) as u8, (low >> 24) as u8, high as u8, (high >> 8) as u8];
        self.write(RAH0, high | 1 << 31); // address valid
    }

    fn init_rx(&mut self) {
        for i in 0..RX_COUNT {
            let desc = self.rx_desc(i);
            // SAFETY: descriptor i lies inside rx_ring; the device is not yet receiving.
            unsafe {
                (desc as *mut u64).write_volatile(self.rx_bufs[i].phys());
                (desc.add(8) as *mut u64).write_volatile(0);
            }
        }
        let ring = self.rx_ring.phys();
        self.write(RDBAL, ring as u32);
        self.write(RDBAH, (ring >> 32) as u32);
        self.write(RDLEN, (RX_COUNT * 16) as u32);
        self.write(RDH, 0);
        self.write(RDT, (RX_COUNT - 1) as u32);
        // Buffer size 2048 is BSIZE=00 with BSEX=0.
        self.write(RCTL, RCTL_EN | RCTL_BAM | RCTL_SECRC);
    }

    fn init_tx(&mut self) {
        let ring = self.tx_ring.phys();
        // Mark every descriptor done so the first lap finds them free.
        for i in 0..TX_COUNT {
            // SAFETY: status byte of descriptor i, inside tx_ring.
            unsafe { self.tx_desc(i).add(12).write_volatile(DESC_DD) };
        }
        self.write(TDBAL, ring as u32);
        self.write(TDBAH, (ring >> 32) as u32);
        self.write(TDLEN, (TX_COUNT * 16) as u32);
        self.write(TDH, 0);
        self.write(TDT, 0);
        self.write(TCTL, TCTL_EN | TCTL_PSP | (0x0F << 4) | (0x40 << 12));
        self.write(TIPG, 0x0060_200A);
    }

    fn rx_desc(&self, i: usize) -> *mut u8 {
        self.rx_ring.as_ptr().wrapping_add(i * 16)
    }

    fn tx_desc(&self, i: usize) -> *mut u8 {
        self.tx_ring.as_ptr().wrapping_add(i * 16)
    }
}

impl Nic for E1000 {
    fn mac(&self) -> [u8; 6] {
        self.mac
    }

    fn link_up(&self) -> bool {
        self.read(STATUS) & 2 != 0
    }

    fn receive(&mut self) -> Option<Vec<u8>> {
        let i = self.rx_next;
        let desc = self.rx_desc(i);
        // SAFETY: descriptor fields inside rx_ring; volatile because the device writes them.
        let (len, status) = unsafe { ((desc.add(8) as *const u16).read_volatile(), desc.add(12).read_volatile()) };
        if status & DESC_DD == 0 {
            return None;
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        // Frames larger than one buffer are dropped (MTU 1500 always fits in 2048).
        let packet = (status & RX_EOP != 0).then(|| self.rx_bufs[i].as_slice()[..len as usize].to_vec());
        // SAFETY: hand the descriptor back to the device.
        unsafe { desc.add(12).write_volatile(0) };
        self.write(RDT, i as u32);
        self.rx_next = (i + 1) % RX_COUNT;
        Some(packet.unwrap_or_default())
    }

    fn transmit(&mut self, frame: &[u8]) -> bool {
        let i = self.tx_next;
        let desc = self.tx_desc(i);
        // SAFETY: status byte of descriptor i.
        if unsafe { desc.add(12).read_volatile() } & DESC_DD == 0 || frame.len() > BUF_SIZE {
            return false; // ring full (device still owns the next descriptor) or oversized
        }
        self.tx_bufs[i].as_mut_slice()[..frame.len()].copy_from_slice(frame);
        // SAFETY: descriptor i is owned by the driver (DD set).
        unsafe {
            (desc as *mut u64).write_volatile(self.tx_bufs[i].phys());
            (desc.add(8) as *mut u16).write_volatile(frame.len() as u16);
            desc.add(10).write_volatile(0); // CSO
            desc.add(11).write_volatile(TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS);
            desc.add(12).write_volatile(0); // status
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::Release);
        self.tx_next = (i + 1) % TX_COUNT;
        self.write(TDT, self.tx_next as u32);
        let _ = self.read(TDH); // flush posted write
        true
    }
}
