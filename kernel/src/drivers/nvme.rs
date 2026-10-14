//! NVMe (NVM Express) SSD driver: polled, one admin and one I/O queue pair, PRP lists.
//!
//! One driver covers every NVMe drive (it is a single standard interface). Each active
//! namespace becomes a disk; drives formatted with 4 KiB blocks are served to Nova's
//! 512-byte-sector interface with read-modify-write for partial blocks.
//!
//! Reference: NVM Express Base Specification 2.0 (controller registers, admin commands
//! Identify / Create I/O Completion Queue / Create I/O Submission Queue, PRP lists) and
//! NVM Command Set Specification 1.0 (Read, Write, Flush; Identify Namespace).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::BlockDevice;
use crate::memory::{self, DmaBuffer};
use crate::pci::{Bar, Device};
use crate::sync::SpinLock;
use crate::{pit, task};

const CAP: u64 = 0x00;
const CC: u64 = 0x14;
const CSTS: u64 = 0x1C;
const AQA: u64 = 0x24;
const ASQ: u64 = 0x28;
const ACQ: u64 = 0x30;

const CC_EN: u32 = 1 << 0;
const CC_IOSQES: u32 = 6 << 16; // 64-byte submission entries
const CC_IOCQES: u32 = 4 << 20; // 16-byte completion entries
const CSTS_RDY: u32 = 1 << 0;
const CSTS_CFS: u32 = 1 << 1;

const ADMIN_CREATE_SQ: u8 = 0x01;
const ADMIN_CREATE_CQ: u8 = 0x05;
const ADMIN_IDENTIFY: u8 = 0x06;
const IO_FLUSH: u8 = 0x00;
const IO_WRITE: u8 = 0x01;
const IO_READ: u8 = 0x02;

const QUEUE_ENTRIES: usize = 64;
const PAGE: usize = 4096;
/// Bytes per command: 32 pages through one PRP list page.
const MAX_TRANSFER: usize = 128 * 1024;

struct Queue {
    sq: DmaBuffer,
    cq: DmaBuffer,
    sq_doorbell: u64,
    cq_doorbell: u64,
    tail: usize,
    head: usize,
    phase: bool,
    next_cid: u16,
}

impl Queue {
    fn new(regs: u64, qid: u64, stride: u64) -> Self {
        Self {
            sq: DmaBuffer::new(QUEUE_ENTRIES * 64, PAGE),
            cq: DmaBuffer::new(QUEUE_ENTRIES * 16, PAGE),
            sq_doorbell: regs + 0x1000 + (2 * qid) * stride,
            cq_doorbell: regs + 0x1000 + (2 * qid + 1) * stride,
            tail: 0,
            head: 0,
            phase: true,
            next_cid: 0,
        }
    }
}

struct Controller {
    regs: u64,
    admin: Queue,
    io: Queue,
    /// Transfer buffer and the PRP list describing its pages 2..n.
    bounce: DmaBuffer,
    prp_list: DmaBuffer,
    timeout_ticks: u64,
}

fn read32(addr: u64) -> u32 {
    // SAFETY: callers pass addresses inside the mapped controller registers.
    unsafe { (addr as *const u32).read_volatile() }
}

fn write32(addr: u64, value: u32) {
    // SAFETY: as for read32.
    unsafe { (addr as *mut u32).write_volatile(value) }
}

fn read64(addr: u64) -> u64 {
    read32(addr) as u64 | (read32(addr + 4) as u64) << 32
}

fn write64(addr: u64, value: u64) {
    write32(addr, value as u32);
    write32(addr + 4, (value >> 32) as u32);
}

/// Wait (yielding to other tasks) until `done()` or `ticks` timer ticks pass.
fn wait_until(ticks: u64, mut done: impl FnMut() -> bool) -> bool {
    let end = pit::ticks() + ticks;
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

/// One NVMe command (submission queue entry, 16 dwords).
#[derive(Clone, Copy, Default)]
struct Command {
    opcode: u8,
    nsid: u32,
    prp1: u64,
    prp2: u64,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
}

impl Controller {
    fn new(dev: &Device) -> Result<Self, String> {
        let Some(Bar::Memory { base, size }) = dev.bar(0) else { return Err("BAR0 is not a memory BAR".into()) };
        dev.enable();
        let regs = memory::map_mmio(base, size.max(0x2000));
        let cap = read64(regs + CAP);
        let stride = 4u64 << ((cap >> 32) & 0xF);
        let mqes = (cap & 0xFFFF) as usize + 1;
        if mqes < 2 {
            return Err("controller reports no usable queue size".into());
        }
        if (cap >> 48) & 0xF != 0 {
            return Err("controller does not support 4 KiB memory pages".into());
        }
        // CAP.TO: worst-case time to become ready, in 500 ms units.
        let timeout_ticks = ((cap >> 24) & 0xFF).max(1) * pit::HZ / 2 + pit::HZ;

        // Reset, then set up the admin queues.
        write32(regs + CC, read32(regs + CC) & !CC_EN);
        if !wait_until(timeout_ticks, || read32(regs + CSTS) & CSTS_RDY == 0) {
            return Err("controller did not reset".into());
        }
        let mut ctrl = Self {
            regs,
            admin: Queue::new(regs, 0, stride),
            io: Queue::new(regs, 1, stride),
            bounce: DmaBuffer::new(MAX_TRANSFER, PAGE),
            prp_list: DmaBuffer::new(PAGE, PAGE),
            timeout_ticks,
        };
        let entries = QUEUE_ENTRIES.min(mqes) as u32;
        write32(regs + AQA, (entries - 1) << 16 | (entries - 1));
        write64(regs + ASQ, ctrl.admin.sq.phys());
        write64(regs + ACQ, ctrl.admin.cq.phys());
        write32(regs + CC, CC_IOSQES | CC_IOCQES | CC_EN);
        if !wait_until(timeout_ticks, || read32(regs + CSTS) & (CSTS_RDY | CSTS_CFS) != 0) || read32(regs + CSTS) & CSTS_CFS != 0 {
            return Err("controller did not become ready".into());
        }

        // I/O queue pair 1, polled (interrupts disabled for the completion queue).
        let io_entries = QUEUE_ENTRIES.min(mqes) as u32;
        let cq = ctrl.io.cq.phys();
        ctrl.admin_command(Command { opcode: ADMIN_CREATE_CQ, prp1: cq, cdw10: (io_entries - 1) << 16 | 1, cdw11: 1, ..Default::default() })?;
        let sq = ctrl.io.sq.phys();
        ctrl.admin_command(Command { opcode: ADMIN_CREATE_SQ, prp1: sq, cdw10: (io_entries - 1) << 16 | 1, cdw11: 1 << 16 | 1, ..Default::default() })?;
        Ok(ctrl)
    }

    fn submit(queue: &mut Queue, cmd: Command, timeout_ticks: u64) -> Result<u32, String> {
        let cid = queue.next_cid;
        queue.next_cid = queue.next_cid.wrapping_add(1);
        let mut entry = [0u32; 16];
        entry[0] = cmd.opcode as u32 | (cid as u32) << 16;
        entry[1] = cmd.nsid;
        entry[6] = cmd.prp1 as u32;
        entry[7] = (cmd.prp1 >> 32) as u32;
        entry[8] = cmd.prp2 as u32;
        entry[9] = (cmd.prp2 >> 32) as u32;
        entry[10] = cmd.cdw10;
        entry[11] = cmd.cdw11;
        entry[12] = cmd.cdw12;
        let slot = queue.tail;
        // SAFETY: slot < QUEUE_ENTRIES inside our submission queue buffer; the device
        // only reads entries the tail doorbell has published.
        unsafe {
            let dst = (queue.sq.as_ptr() as *mut u32).add(slot * 16);
            for (i, dword) in entry.iter().enumerate() {
                dst.add(i).write_volatile(*dword);
            }
        }
        queue.tail = (queue.tail + 1) % QUEUE_ENTRIES;
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        write32(queue.sq_doorbell, queue.tail as u32);

        // Wait for the completion entry whose phase bit flips to ours.
        let cq = queue.cq.as_ptr() as *const u32;
        let (head, phase) = (queue.head, queue.phase);
        // SAFETY: head < QUEUE_ENTRIES inside our completion queue buffer.
        let status_word = || unsafe { cq.add(head * 4 + 3).read_volatile() };
        if !wait_until(timeout_ticks, || ((status_word() >> 16) & 1 == 1) == phase) {
            return Err(format!("command {:#x} timed out", cmd.opcode));
        }
        let dw3 = status_word();
        // SAFETY: as above.
        let dw0 = unsafe { cq.add(head * 4).read_volatile() };
        queue.head = (queue.head + 1) % QUEUE_ENTRIES;
        if queue.head == 0 {
            queue.phase = !queue.phase;
        }
        write32(queue.cq_doorbell, queue.head as u32);
        let status = (dw3 >> 17) & 0x7FFF;
        if status != 0 {
            return Err(format!("command {:#x} failed: status {:#x} (type {}, code {:#x})", cmd.opcode, status, (status >> 8) & 7, status & 0xFF));
        }
        Ok(dw0)
    }

    fn admin_command(&mut self, cmd: Command) -> Result<u32, String> {
        Self::submit(&mut self.admin, cmd, self.timeout_ticks)
    }

    /// Point a command at the first `bytes` of the bounce buffer (PRP1, PRP2 or a list).
    fn prps(&mut self, bytes: usize) -> (u64, u64) {
        let base = self.bounce.phys();
        let pages = bytes.div_ceil(PAGE).max(1);
        match pages {
            1 => (base, 0),
            2 => (base, base + PAGE as u64),
            _ => {
                let list = self.prp_list.as_mut_slice();
                for i in 1..pages {
                    list[(i - 1) * 8..i * 8].copy_from_slice(&(base + (i * PAGE) as u64).to_le_bytes());
                }
                (base, self.prp_list.phys())
            }
        }
    }

    /// Identify data (4 KiB) for `cns` (1 = controller, 0 = namespace, 2 = active list).
    fn identify(&mut self, cns: u32, nsid: u32) -> Result<Vec<u8>, String> {
        let (prp1, prp2) = self.prps(PAGE);
        self.admin_command(Command { opcode: ADMIN_IDENTIFY, nsid, prp1, prp2, cdw10: cns, ..Default::default() })?;
        Ok(self.bounce.as_slice()[..PAGE].to_vec())
    }

    /// Read or write `blocks` logical blocks at `lba` through the bounce buffer.
    fn io(&mut self, write: bool, nsid: u32, lba: u64, blocks: u32, bytes: usize) -> Result<(), String> {
        let (prp1, prp2) = self.prps(bytes);
        let cmd = Command {
            opcode: if write { IO_WRITE } else { IO_READ },
            nsid,
            prp1,
            prp2,
            cdw10: lba as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: blocks - 1,
        };
        Self::submit(&mut self.io, cmd, self.timeout_ticks * 4)?;
        Ok(())
    }

    fn flush(&mut self, nsid: u32) -> Result<(), String> {
        Self::submit(&mut self.io, Command { opcode: IO_FLUSH, nsid, ..Default::default() }, self.timeout_ticks * 4).map(|_| ())
    }
}

pub struct NvmeDisk {
    ctrl: Arc<SpinLock<Controller>>,
    index: usize,
    nsid: u32,
    /// log2 of the logical block size (9 = 512 bytes, 12 = 4 KiB).
    block_shift: u32,
    blocks: u64,
    model: String,
}

fn ascii(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().into()
}

/// Bring up the controller and return one disk per active namespace.
pub fn probe(dev: &Device, first_index: usize) -> Result<Vec<NvmeDisk>, String> {
    let mut ctrl = Controller::new(dev)?;
    let id = ctrl.identify(1, 0)?;
    let model = ascii(&id[24..64]);
    let namespaces = u32::from_le_bytes(id[516..520].try_into().unwrap());
    // Active namespace IDs (CNS 2, NVMe 1.1+); fall back to 1..=NN.
    let mut nsids: Vec<u32> = match ctrl.identify(2, 0) {
        Ok(list) => list.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())).take_while(|&n| n != 0).collect(),
        Err(_) => (1..=namespaces.min(16)).collect(),
    };
    nsids.truncate(16);
    let mut found = Vec::new();
    for nsid in nsids {
        let ns = ctrl.identify(0, nsid)?;
        let blocks = u64::from_le_bytes(ns[0..8].try_into().unwrap());
        let format = (ns[26] & 0xF) as usize;
        let lbaf = u32::from_le_bytes(ns[128 + format * 4..132 + format * 4].try_into().unwrap());
        let block_shift = (lbaf >> 16) & 0xFF;
        if blocks == 0 || !(9..=12).contains(&block_shift) {
            continue; // inactive, or a block size Nova does not serve
        }
        found.push((nsid, block_shift, blocks));
    }
    let ctrl = Arc::new(SpinLock::new(ctrl));
    Ok(found
        .into_iter()
        .enumerate()
        .map(|(i, (nsid, block_shift, blocks))| NvmeDisk { ctrl: ctrl.clone(), index: first_index + i, nsid, block_shift, blocks, model: model.clone() })
        .collect())
}

impl NvmeDisk {
    fn sectors_per_block(&self) -> u64 {
        1 << (self.block_shift - 9)
    }

    /// Transfer whole logical blocks starting at `block`, through the bounce buffer.
    fn blocks(&mut self, write: bool, mut block: u64, buf: &mut [u8]) -> Result<(), String> {
        let block_bytes = 1usize << self.block_shift;
        let mut ctrl = self.ctrl.lock();
        for chunk in buf.chunks_mut(MAX_TRANSFER) {
            let count = (chunk.len() / block_bytes) as u32;
            if write {
                ctrl.bounce.as_mut_slice()[..chunk.len()].copy_from_slice(chunk);
            }
            ctrl.io(write, self.nsid, block, count, chunk.len())?;
            if !write {
                chunk.copy_from_slice(&ctrl.bounce.as_slice()[..chunk.len()]);
            }
            block += count as u64;
        }
        Ok(())
    }

    /// Read or write 512-byte sectors; partial logical blocks are read, modified, written.
    fn sectors(&mut self, write: bool, lba: u64, data: &mut [u8]) -> Result<(), String> {
        let spb = self.sectors_per_block();
        if spb == 1 {
            return self.blocks(write, lba, data);
        }
        let block_bytes = 1usize << self.block_shift;
        let (first, last) = (lba / spb, (lba + data.len() as u64 / 512 - 1) / spb);
        let mut span = alloc::vec![0u8; ((last - first + 1) as usize) * block_bytes];
        let offset = ((lba % spb) * 512) as usize;
        let aligned = offset == 0 && data.len() == span.len();
        if !write || !aligned {
            self.blocks(false, first, &mut span)?;
        }
        if write {
            span[offset..offset + data.len()].copy_from_slice(data);
            self.blocks(true, first, &mut span)
        } else {
            data.copy_from_slice(&span[offset..offset + data.len()]);
            Ok(())
        }
    }
}

impl BlockDevice for NvmeDisk {
    fn name(&self) -> String {
        format!("nvme{} ({}, namespace {})", self.index, self.model, self.nsid)
    }

    fn sector_count(&self) -> u64 {
        self.blocks * self.sectors_per_block()
    }

    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), String> {
        self.sectors(false, lba, buf)
    }

    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), String> {
        let mut data = buf.to_vec();
        self.sectors(true, lba, &mut data)
    }

    fn logical_block_size(&self) -> usize {
        1 << self.block_shift
    }

    fn flush(&mut self) -> Result<(), String> {
        let nsid = self.nsid;
        self.ctrl.lock().flush(nsid)
    }
}
