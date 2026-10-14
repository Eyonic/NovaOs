//! Disks, GPT partition tables, and the installer's disk operations.
//!
//! All disks found by drivers (plus a RAM disk in live mode) live in one registry
//! and are addressed by index. GPT is parsed for every disk; the boot disk is
//! recognised by the disk GUID Limine reports for the kernel file.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::drivers::BlockDevice;
use crate::sync::SpinLock;
use crate::{limine, rand};

pub const ESP_TYPE: [u8; 16] = guid(0xC12A7328, 0xF81F, 0x11D2, [0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B]);
/// Partition type of a NovaFS data partition (Nova's own GUID).
pub const NOVA_DATA_TYPE: [u8; 16] = guid(0x4E6F7661, 0x4653, 0x4E44, [0x80, 0x01, 0x4E, 0x4F, 0x56, 0x41, 0x46, 0x53]);

/// A GUID in GPT's on-disk byte layout (first three fields little-endian).
pub const fn guid(a: u32, b: u16, c: u16, d: [u8; 8]) -> [u8; 16] {
    let a = a.to_le_bytes();
    let b = b.to_le_bytes();
    let c = c.to_le_bytes();
    [a[0], a[1], a[2], a[3], b[0], b[1], c[0], c[1], d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]]
}

#[derive(Clone, Debug)]
pub struct Partition {
    pub index: usize,
    pub type_guid: [u8; 16],
    pub unique_guid: [u8; 16],
    pub first_lba: u64,
    pub last_lba: u64,
    pub name: String,
}

impl Partition {
    pub fn sectors(&self) -> u64 {
        self.last_lba - self.first_lba + 1
    }
}

pub struct Disk {
    pub dev: Box<dyn BlockDevice>,
    pub disk_guid: Option<[u8; 16]>,
    pub partitions: Vec<Partition>,
}

static DISKS: SpinLock<Vec<Disk>> = SpinLock::new(Vec::new());

pub fn add(dev: Box<dyn BlockDevice>) -> usize {
    let mut disk = Disk { dev, disk_guid: None, partitions: Vec::new() };
    if let Ok((guid, parts)) = read_gpt(&mut *disk.dev) {
        disk.disk_guid = Some(guid);
        disk.partitions = parts;
    }
    let mut disks = DISKS.lock();
    disks.push(disk);
    disks.len() - 1
}

/// Run `f` with exclusive access to disk `index`.
pub fn with_disk<R>(index: usize, f: impl FnOnce(&mut Disk) -> R) -> Option<R> {
    let mut disks = DISKS.lock();
    disks.get_mut(index).map(f)
}

pub struct DiskInfo {
    pub index: usize,
    pub name: String,
    pub sectors: u64,
    pub partitions: Vec<Partition>,
    pub is_boot: bool,
    /// The device's logical block size (Nova addresses 512-byte sectors everywhere).
    pub block_size: usize,
}

pub fn list() -> Vec<DiskInfo> {
    let boot = limine::EXECUTABLE_FILE.file().map(|f| f.gpt_disk_uuid).filter(|g| *g != [0; 16]);
    DISKS
        .lock()
        .iter()
        .enumerate()
        .map(|(index, d)| DiskInfo {
            index,
            name: d.dev.name(),
            sectors: d.dev.sector_count(),
            partitions: d.partitions.clone(),
            is_boot: boot.is_some() && d.disk_guid == boot,
            block_size: d.dev.logical_block_size(),
        })
        .collect()
}

/// Bitwise CRC-32 (IEEE 802.3), as used by GPT.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn read_gpt(dev: &mut dyn BlockDevice) -> Result<([u8; 16], Vec<Partition>), String> {
    let mut header = vec![0u8; 512];
    dev.read(1, &mut header)?;
    if &header[0..8] != b"EFI PART" {
        return Err("no GPT".into());
    }
    let le64 = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let le32 = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let disk_guid: [u8; 16] = header[56..72].try_into().unwrap();
    let entries_lba = le64(&header, 72);
    let count = le32(&header, 80) as usize;
    let size = le32(&header, 84) as usize;
    if size < 128 || count > 1024 {
        return Err("unsupported GPT entry layout".into());
    }
    let mut entries = vec![0u8; (count * size).div_ceil(512) * 512];
    dev.read(entries_lba, &mut entries)?;
    let mut parts = Vec::new();
    for i in 0..count {
        let e = &entries[i * size..(i + 1) * size];
        let type_guid: [u8; 16] = e[0..16].try_into().unwrap();
        if type_guid == [0; 16] {
            continue;
        }
        let name: String = char::decode_utf16(e[56..128].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).take_while(|&c| c != 0))
            .map(|c| c.unwrap_or('?'))
            .collect();
        parts.push(Partition { index: i + 1, type_guid, unique_guid: e[16..32].try_into().unwrap(), first_lba: le64(e, 32), last_lba: le64(e, 40), name });
    }
    Ok((disk_guid, parts))
}

fn random_guid() -> [u8; 16] {
    let mut g = [0u8; 16];
    rand::fill(&mut g);
    g[7] = (g[7] & 0x0F) | 0x40; // version 4 (in the little-endian third field)
    g[8] = (g[8] & 0x3F) | 0x80; // RFC 4122 variant
    g
}

/// Write a fresh GPT (protective MBR, primary and backup headers and entries) with
/// the given (type, first_lba, last_lba, name) partitions. Returns the new disk GUID.
pub fn write_gpt(dev: &mut dyn BlockDevice, parts: &[([u8; 16], u64, u64, &str)]) -> Result<[u8; 16], String> {
    let total = dev.sector_count();
    let mut mbr = vec![0u8; 512];
    mbr[446 + 4] = 0xEE;
    mbr[446 + 8..446 + 12].copy_from_slice(&1u32.to_le_bytes());
    mbr[446 + 12..446 + 16].copy_from_slice(&((total - 1).min(0xFFFF_FFFF) as u32).to_le_bytes());
    mbr[446 + 1..446 + 4].copy_from_slice(&[0x00, 0x02, 0x00]);
    mbr[446 + 5..446 + 8].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
    mbr[510] = 0x55;
    mbr[511] = 0xAA;

    let mut entries = vec![0u8; 128 * 128];
    for (i, (type_guid, first, last, name)) in parts.iter().enumerate() {
        let e = &mut entries[i * 128..(i + 1) * 128];
        e[0..16].copy_from_slice(type_guid);
        e[16..32].copy_from_slice(&random_guid());
        e[32..40].copy_from_slice(&first.to_le_bytes());
        e[40..48].copy_from_slice(&last.to_le_bytes());
        for (j, c) in name.encode_utf16().take(36).enumerate() {
            e[56 + j * 2..58 + j * 2].copy_from_slice(&c.to_le_bytes());
        }
    }
    let entries_crc = crc32(&entries);
    let disk_guid = random_guid();
    let header = |my_lba: u64, alt_lba: u64, entries_lba: u64| {
        let mut h = vec![0u8; 512];
        h[0..8].copy_from_slice(b"EFI PART");
        h[8..12].copy_from_slice(&0x0001_0000u32.to_le_bytes());
        h[12..16].copy_from_slice(&92u32.to_le_bytes());
        h[24..32].copy_from_slice(&my_lba.to_le_bytes());
        h[32..40].copy_from_slice(&alt_lba.to_le_bytes());
        h[40..48].copy_from_slice(&34u64.to_le_bytes());
        h[48..56].copy_from_slice(&(total - 34).to_le_bytes());
        h[56..72].copy_from_slice(&disk_guid);
        h[72..80].copy_from_slice(&entries_lba.to_le_bytes());
        h[80..84].copy_from_slice(&128u32.to_le_bytes());
        h[84..88].copy_from_slice(&128u32.to_le_bytes());
        h[88..92].copy_from_slice(&entries_crc.to_le_bytes());
        let crc = crc32(&h[..92]);
        h[16..20].copy_from_slice(&crc.to_le_bytes());
        h
    };
    dev.write(0, &mbr)?;
    dev.write(1, &header(1, total - 1, 2))?;
    dev.write(2, &entries)?;
    dev.write(total - 33, &entries)?;
    dev.write(total - 1, &header(total - 1, 1, total - 33))?;
    dev.flush()?;
    Ok(disk_guid)
}

/// Re-read a disk's partition table after it was rewritten.
pub fn rescan(index: usize) {
    with_disk(index, |d| {
        if let Ok((guid, parts)) = read_gpt(&mut *d.dev) {
            d.disk_guid = Some(guid);
            d.partitions = parts;
        }
    });
}

/// A disk in RAM, used for the live session's filesystem.
pub struct RamDisk {
    data: Vec<u8>,
}

impl RamDisk {
    pub fn new(bytes: usize) -> Self {
        Self { data: vec![0; bytes] }
    }
}

impl BlockDevice for RamDisk {
    fn name(&self) -> String {
        format!("ram ({} MiB)", self.data.len() >> 20)
    }
    fn sector_count(&self) -> u64 {
        (self.data.len() / 512) as u64
    }
    fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), String> {
        let start = lba as usize * 512;
        let src = self.data.get(start..start + buf.len()).ok_or("read beyond end of RAM disk")?;
        buf.copy_from_slice(src);
        Ok(())
    }
    fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), String> {
        let start = lba as usize * 512;
        self.data.get_mut(start..start + buf.len()).ok_or("write beyond end of RAM disk")?.copy_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), String> {
        Ok(())
    }
}
