//! AHCI (SATA) host controller driver: polled, one command slot per port, LBA48 DMA.
//!
//! Reference: Serial ATA AHCI 1.3.1 specification (HBA and port registers, command
//! list / FIS / PRDT layouts); ATA8-ACS (IDENTIFY DEVICE, READ/WRITE DMA EXT).

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::BlockDevice;
use crate::memory::{self, DmaBuffer};
use crate::pci::{Bar, Device};
use crate::{pit, task};

const GHC: usize = 0x04;
const PI: usize = 0x0C;
const GHC_AE: u32 = 1 << 31;

const P_CLB: usize = 0x00;
const P_CLBU: usize = 0x04;
const P_FB: usize = 0x08;
const P_FBU: usize = 0x0C;
const P_IS: usize = 0x10;
const P_IE: usize = 0x14;
const P_CMD: usize = 0x18;
const P_TFD: usize = 0x20;
const P_SIG: usize = 0x24;
const P_SSTS: usize = 0x28;
const P_SERR: usize = 0x30;
const P_CI: usize = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const IS_TFES: u32 = 1 << 30;
const TFD_ERR: u32 = 1 << 0;
const TFD_BSY: u32 = 1 << 7;
const TFD_DRQ: u32 = 1 << 3;
const SIG_ATA: u32 = 0x0000_0101;

const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_FLUSH_CACHE_EXT: u8 = 0xEA;
const ATA_IDENTIFY: u8 = 0xEC;

/// Sectors per command: one 64 KiB bounce buffer.
const MAX_SECTORS: usize = 128;

pub struct AhciDisk {
    port_regs: u64,
    port: u32,
    sectors: u64,
    model: String,
    _cmd_list: DmaBuffer,
    _fis: DmaBuffer,
    table: DmaBuffer,
    bounce: DmaBuffer,
}

fn read(addr: u64) -> u32 {
    // SAFETY: callers pass addresses inside the mapped ABAR.
    unsafe { (addr as *const u32).read_volatile() }
}

fn write(addr: u64, value: u32) {
    // SAFETY: as for read.
    unsafe { (addr as *mut u32).write_volatile(value) }
}

/// Wait (yielding to other tasks) until `done()` or a 5 s timeout.
fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    let end = pit::ticks() + 5 * pit::HZ;
    let mut spins = 0u32;
    loop {
        if done() {
            return true;
        }
        if pit::ticks() >= end {
            return false;
        }
        spins += 1;
        if spins > 1000 {
            task::yield_now();
        }
        core::hint::spin_loop();
    }
}

/// Probe the controller and return every SATA disk attached to it.
pub fn probe(dev: &Device) -> Result<Vec<AhciDisk>, String> {
    let Some(Bar::Memory { base, size }) = dev.bar(5) else { return Err("BAR5 (ABAR) is not a memory BAR".into()) };
    dev.enable();
    let abar = memory::map_mmio(base, size.max(0x1100));
    write(abar + GHC as u64, read(abar + GHC as u64) | GHC_AE);
    let implemented = read(abar + PI as u64);
    let mut disks = Vec::new();
    for port in 0..32 {
        if implemented & (1 << port) == 0 {
            continue;
        }
        let regs = abar + 0x100 + port as u64 * 0x80;
        let det = read(regs + P_SSTS as u64) & 0xF;
        if det != 3 || read(regs + P_SIG as u64) != SIG_ATA {
            continue; // no device, or not a plain ATA disk (e.g. ATAPI CD-ROM)
        }
        match AhciDisk::new(regs, port) {
            Ok(disk) => disks.push(disk),
            Err(e) => crate::println!("[nova]   ahci port {port}: {e}"),
        }
    }
    Ok(disks)
}

impl AhciDisk {
    fn reg(&self, r: usize) -> u64 {
        self.port_regs + r as u64
    }

    fn new(port_regs: u64, port: u32) -> Result<Self, String> {
        let mut disk = Self {
            port_regs,
            port,
            sectors: 0,
            model: String::new(),
            _cmd_list: DmaBuffer::new(1024, 1024),
            _fis: DmaBuffer::new(256, 256),
            table: DmaBuffer::new(256, 128),
            bounce: DmaBuffer::new(MAX_SECTORS * 512, 4096),
        };
        // Stop the command engine before relocating its structures.
        write(disk.reg(P_CMD), read(disk.reg(P_CMD)) & !(CMD_ST | CMD_FRE));
        if !wait_until(|| read(port_regs + P_CMD as u64) & (CMD_CR | CMD_FR) == 0) {
            return Err("port did not stop".into());
        }
        let clb = disk._cmd_list.phys();
        let fb = disk._fis.phys();
        write(disk.reg(P_CLB), clb as u32);
        write(disk.reg(P_CLBU), (clb >> 32) as u32);
        write(disk.reg(P_FB), fb as u32);
        write(disk.reg(P_FBU), (fb >> 32) as u32);
        write(disk.reg(P_SERR), 0xFFFF_FFFF);
        write(disk.reg(P_IS), 0xFFFF_FFFF);
        write(disk.reg(P_IE), 0); // polled
        // Command header 0 points at our single command table.
        let table = disk.table.phys();
        // SAFETY: header 0 is the first 32 bytes of the command list buffer.
        unsafe {
            let header = disk._cmd_list.as_ptr();
            (header.add(8) as *mut u32).write_volatile(table as u32);
            (header.add(12) as *mut u32).write_volatile((table >> 32) as u32);
        }
        write(disk.reg(P_CMD), read(disk.reg(P_CMD)) | CMD_FRE);
        write(disk.reg(P_CMD), read(disk.reg(P_CMD)) | CMD_ST);

        let identify = disk.command(ATA_IDENTIFY, 0, 1, false)?;
        let words: Vec<u16> = identify.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        disk.sectors = (words[100] as u64) | (words[101] as u64) << 16 | (words[102] as u64) << 32 | (words[103] as u64) << 48;
        if disk.sectors == 0 {
            disk.sectors = (words[60] as u64) | (words[61] as u64) << 16; // LBA28 count
        }
        let model: Vec<u8> = words[27..47].iter().flat_map(|w| w.to_be_bytes()).collect();
        disk.model = String::from_utf8_lossy(&model).trim().into();
        Ok(disk)
    }

    /// Run one ATA command through slot 0. For reads (and IDENTIFY) returns the data.
    fn command(&mut self, ata: u8, lba: u64, count: usize, write_dir: bool) -> Result<Vec<u8>, String> {
        let bytes = count * 512;
        // SAFETY: the command list, table and bounce buffer are ours and the port is
        // idle (slot 0 is the only slot used and every command is waited for).
        unsafe {
            let header = self._cmd_list.as_ptr();
            let prdtl: u32 = if bytes > 0 { 1 } else { 0 };
            let flags: u32 = 5 | if write_dir { 1 << 6 } else { 0 } | prdtl << 16; // CFL = 5 dwords
            (header as *mut u32).write_volatile(flags);
            (header.add(4) as *mut u32).write_volatile(0); // PRDBC

            let t = self.table.as_ptr();
            core::ptr::write_bytes(t, 0, 256);
            let fis = [
                0x27, 0x80, ata, 0, // H2D register FIS, command bit
                lba as u8, (lba >> 8) as u8, (lba >> 16) as u8, 0x40, // LBA mode
                (lba >> 24) as u8, (lba >> 32) as u8, (lba >> 40) as u8, 0,
                count as u8, (count >> 8) as u8, 0, 0,
            ];
            core::ptr::copy_nonoverlapping(fis.as_ptr(), t, fis.len());
            if bytes > 0 {
                let prd = t.add(0x80);
                (prd as *mut u64).write_volatile(self.bounce.phys());
                (prd.add(12) as *mut u32).write_volatile((bytes as u32 - 1) | 1 << 31);
            }
        }
        if !wait_until(|| read(self.reg(P_TFD)) & (TFD_BSY | TFD_DRQ) == 0) {
            return Err(format!("port {} busy", self.port));
        }
        write(self.reg(P_IS), 0xFFFF_FFFF);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        write(self.reg(P_CI), 1);
        let finished = wait_until(|| read(self.reg(P_CI)) & 1 == 0 || read(self.reg(P_IS)) & IS_TFES != 0);
        let tfd = read(self.reg(P_TFD));
        if !finished || read(self.reg(P_IS)) & IS_TFES != 0 || tfd & TFD_ERR != 0 {
            return Err(format!("ATA command {ata:#x} failed (lba {lba}, tfd {tfd:#x})"));
        }
        Ok(if write_dir { Vec::new() } else { self.bounce.as_slice()[..bytes].to_vec() })
    }
}

impl BlockDevice for AhciDisk {
    fn name(&self) -> String {
        format!("sata{} ({})", self.port, self.model)
    }

    fn sector_count(&self) -> u64 {
        self.sectors
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), String> {
        for (i, chunk) in buf.chunks_mut(MAX_SECTORS * 512).enumerate() {
            let data = self.command(ATA_READ_DMA_EXT, lba + (i * MAX_SECTORS) as u64, chunk.len() / 512, false)?;
            chunk.copy_from_slice(&data);
        }
        Ok(())
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), String> {
        for (i, chunk) in buf.chunks(MAX_SECTORS * 512).enumerate() {
            self.bounce.as_mut_slice()[..chunk.len()].copy_from_slice(chunk);
            self.command(ATA_WRITE_DMA_EXT, lba + (i * MAX_SECTORS) as u64, chunk.len() / 512, true)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        self.command(ATA_FLUSH_CACHE_EXT, 0, 0, false).map(|_| ())
    }
}
