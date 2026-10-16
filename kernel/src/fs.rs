//! NovaFS: a small extent-based filesystem for Nova's own data.
//!
//! Layout, in 4 KiB blocks from the start of the partition:
//!
//! | Block | Contents |
//! |---|---|
//! | 0 | superblock: magic `NOVAFS01`, block count, directory block count, label |
//! | 1..=32 | directory: 1024 entries of 128 bytes |
//! | 33.. | file data, one contiguous extent per file |
//!
//! A directory entry holds the full path (`apps/clock.rhai`; directories are just
//! path prefixes), size, first block, allocated blocks and modification time.
//! Writing a file that no longer fits its extent moves it to a new first-fit extent.
//! Every change writes the data first and then the one directory block it touched,
//! so a crash loses at most the latest write.
//!
//! One filesystem is mounted at a time (the installed disk's data partition, or a
//! RAM disk in a live session).

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::storage;
use crate::sync::SpinLock;

const MAGIC: &[u8; 8] = b"NOVAFS01";
const BLOCK: usize = 4096;
const SECTORS_PER_BLOCK: u64 = (BLOCK / 512) as u64;
const DIR_BLOCKS: u64 = 32;
const ENTRY_SIZE: usize = 128;
const ENTRIES: usize = DIR_BLOCKS as usize * BLOCK / ENTRY_SIZE;
const DATA_START: u64 = 1 + DIR_BLOCKS;
pub const NAME_MAX: usize = 88;

#[derive(Clone, Default)]
struct Entry {
    used: bool,
    size: u64,
    start: u64,
    blocks: u64,
    mtime: u64,
    name: String,
}

impl Entry {
    fn encode(&self) -> [u8; ENTRY_SIZE] {
        let mut b = [0u8; ENTRY_SIZE];
        b[0] = self.used as u8;
        b[8..16].copy_from_slice(&self.size.to_le_bytes());
        b[16..24].copy_from_slice(&self.start.to_le_bytes());
        b[24..32].copy_from_slice(&self.blocks.to_le_bytes());
        b[32..40].copy_from_slice(&self.mtime.to_le_bytes());
        let n = self.name.len().min(NAME_MAX);
        b[40..40 + n].copy_from_slice(&self.name.as_bytes()[..n]);
        b
    }

    fn decode(b: &[u8]) -> Self {
        let le = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let name_bytes = &b[40..40 + NAME_MAX];
        let len = name_bytes.iter().position(|&c| c == 0).unwrap_or(NAME_MAX);
        Self { used: b[0] == 1, size: le(8), start: le(16), blocks: le(24), mtime: le(32), name: String::from_utf8_lossy(&name_bytes[..len]).into() }
    }
}

pub struct NovaFs {
    disk: usize,
    first_lba: u64,
    blocks: u64,
    label: String,
    entries: Vec<Entry>,
}

static FS: SpinLock<Option<NovaFs>> = SpinLock::new(None);
/// Bumped on every change, so watchers (the desktop's app list) can poll cheaply.
static GENERATION: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn generation() -> u64 {
    GENERATION.load(core::sync::atomic::Ordering::Relaxed)
}

fn bump() {
    GENERATION.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

fn io_read(disk: usize, lba: u64, buf: &mut [u8]) -> Result<(), String> {
    storage::with_disk(disk, |d| d.dev.read(lba, buf)).ok_or("disk vanished")?
}

fn io_write(disk: usize, lba: u64, buf: &[u8]) -> Result<(), String> {
    storage::with_disk(disk, |d| d.dev.write(lba, buf)).ok_or("disk vanished")?
}

impl NovaFs {
    fn lba(&self, block: u64) -> u64 {
        self.first_lba + block * SECTORS_PER_BLOCK
    }

    /// Create an empty filesystem on `sectors` sectors starting at `first_lba`.
    pub fn format(disk: usize, first_lba: u64, sectors: u64, label: &str) -> Result<(), String> {
        let blocks = sectors / SECTORS_PER_BLOCK;
        if blocks < DATA_START + 16 {
            return Err("partition too small for NovaFS".into());
        }
        let mut sb = vec![0u8; BLOCK];
        sb[0..8].copy_from_slice(MAGIC);
        sb[8..16].copy_from_slice(&blocks.to_le_bytes());
        sb[16..24].copy_from_slice(&DIR_BLOCKS.to_le_bytes());
        let l = label.len().min(64);
        sb[64..64 + l].copy_from_slice(&label.as_bytes()[..l]);
        io_write(disk, first_lba, &sb)?;
        io_write(disk, first_lba + SECTORS_PER_BLOCK, &vec![0u8; DIR_BLOCKS as usize * BLOCK])?;
        storage::with_disk(disk, |d| d.dev.flush()).ok_or("disk vanished")?
    }

    pub fn open(disk: usize, first_lba: u64) -> Result<Self, String> {
        let mut sb = vec![0u8; BLOCK];
        io_read(disk, first_lba, &mut sb)?;
        if &sb[0..8] != MAGIC {
            return Err("not a NovaFS partition".into());
        }
        let blocks = u64::from_le_bytes(sb[8..16].try_into().unwrap());
        let label_bytes = &sb[64..128];
        let label = String::from_utf8_lossy(&label_bytes[..label_bytes.iter().position(|&c| c == 0).unwrap_or(64)]).into();
        let mut dir = vec![0u8; DIR_BLOCKS as usize * BLOCK];
        io_read(disk, first_lba + SECTORS_PER_BLOCK, &mut dir)?;
        let entries = dir.chunks(ENTRY_SIZE).map(Entry::decode).collect();
        Ok(Self { disk, first_lba, blocks, label, entries })
    }

    fn save_entry(&self, i: usize) -> Result<(), String> {
        let per_block = BLOCK / ENTRY_SIZE;
        let block = i / per_block;
        let mut buf = vec![0u8; BLOCK];
        for (j, e) in self.entries[block * per_block..(block + 1) * per_block].iter().enumerate() {
            buf[j * ENTRY_SIZE..(j + 1) * ENTRY_SIZE].copy_from_slice(&e.encode());
        }
        io_write(self.disk, self.lba(1 + block as u64), &buf)?;
        storage::with_disk(self.disk, |d| d.dev.flush()).ok_or("disk vanished")?
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.entries.iter().position(|e| e.used && e.name == name)
    }

    /// First run of `n` free blocks, or None.
    fn allocate(&self, n: u64, ignore: Option<usize>) -> Option<u64> {
        let mut used: Vec<(u64, u64)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(i, e)| e.used && e.blocks > 0 && Some(*i) != ignore)
            .map(|(_, e)| (e.start, e.start + e.blocks))
            .collect();
        used.sort_unstable();
        let mut candidate = DATA_START;
        for (start, end) in used {
            if start >= candidate + n {
                break;
            }
            candidate = candidate.max(end);
        }
        (candidate + n <= self.blocks).then_some(candidate)
    }

    pub fn read(&self, name: &str) -> Result<Vec<u8>, String> {
        let e = &self.entries[self.find(name).ok_or_else(|| format!("{name}: no such file"))?];
        let mut buf = vec![0u8; (e.blocks as usize * BLOCK).max(BLOCK)];
        if e.blocks > 0 {
            io_read(self.disk, self.lba(e.start), &mut buf[..e.blocks as usize * BLOCK])?;
        }
        buf.truncate(e.size as usize);
        Ok(buf)
    }

    pub fn write(&mut self, name: &str, data: &[u8]) -> Result<(), String> {
        if name.is_empty() || name.len() > NAME_MAX || name.contains('\0') {
            return Err(format!("invalid file name {name:?}"));
        }
        let needed = (data.len() as u64).div_ceil(BLOCK as u64);
        let index = match self.find(name) {
            Some(i) => i,
            None => self.entries.iter().position(|e| !e.used).ok_or("directory full")?,
        };
        let existing = self.entries[index].used.then_some(index);
        let start = if existing.is_some() && self.entries[index].blocks >= needed {
            self.entries[index].start
        } else {
            self.allocate(needed, existing).ok_or("disk full")?
        };
        if needed > 0 {
            let mut buf = vec![0u8; needed as usize * BLOCK];
            buf[..data.len()].copy_from_slice(data);
            io_write(self.disk, self.lba(start), &buf)?;
        }
        let blocks = if existing.is_some() && start == self.entries[index].start { self.entries[index].blocks.max(needed) } else { needed };
        self.entries[index] = Entry { used: true, size: data.len() as u64, start, blocks, mtime: crate::rtc::unix_time(), name: name.into() };
        self.save_entry(index)
    }

    pub fn delete(&mut self, name: &str) -> Result<(), String> {
        let i = self.find(name).ok_or_else(|| format!("{name}: no such file"))?;
        self.entries[i] = Entry::default();
        self.save_entry(i)
    }

    /// (name, size, mtime) of every file whose path starts with `prefix`, sorted.
    pub fn list(&self, prefix: &str) -> Vec<(String, u64, u64)> {
        let mut v: Vec<_> = self.entries.iter().filter(|e| e.used && e.name.starts_with(prefix)).map(|e| (e.name.clone(), e.size, e.mtime)).collect();
        v.sort();
        v
    }

    /// (used bytes, total bytes).
    pub fn usage(&self) -> (u64, u64) {
        let used: u64 = self.entries.iter().filter(|e| e.used).map(|e| e.blocks).sum::<u64>() + DATA_START;
        (used * BLOCK as u64, self.blocks * BLOCK as u64)
    }
}

/// Mount the filesystem at (`disk`, `first_lba`) as the system filesystem.
pub fn mount(disk: usize, first_lba: u64) -> Result<(), String> {
    let fs = NovaFs::open(disk, first_lba)?;
    *FS.lock() = Some(fs);
    bump();
    Ok(())
}

pub fn mounted() -> bool {
    FS.lock().is_some()
}

/// Label and disk of the mounted filesystem.
pub fn mount_info() -> Option<(String, String)> {
    let fs = FS.lock();
    let fs = fs.as_ref()?;
    let disk = storage::list().into_iter().find(|d| d.index == fs.disk).map(|d| d.name).unwrap_or_default();
    Some((fs.label.clone(), disk))
}

fn with_fs<R>(f: impl FnOnce(&mut NovaFs) -> Result<R, String>) -> Result<R, String> {
    let mut guard = FS.lock();
    f(guard.as_mut().ok_or("no filesystem mounted")?)
}

/// Read-only system files (programs, libraries, the Chromium engine) served straight
/// from the memory Limine loaded them into: no copy into NovaFS, which in a live
/// session is a small RAM disk. They overlay NovaFS paths.
static SYSTEM_FILES: SpinLock<alloc::collections::BTreeMap<String, &'static [u8]>> = SpinLock::new(alloc::collections::BTreeMap::new());

pub fn add_system_file(path: &str, data: &'static [u8]) {
    SYSTEM_FILES.lock().insert(path.into(), data);
    bump();
}

/// A read-only system file's contents, without copying.
pub fn system_file(path: &str) -> Option<&'static [u8]> {
    SYSTEM_FILES.lock().get(path).copied()
}

/// The system files whose path starts with `prefix` (e.g. the sample media).
pub fn system_list(prefix: &str) -> Vec<String> {
    SYSTEM_FILES.lock().range(String::from(prefix)..).take_while(|(p, _)| p.starts_with(prefix)).map(|(p, _)| p.clone()).collect()
}

pub fn read(name: &str) -> Result<Vec<u8>, String> {
    if let Some(data) = system_file(name) {
        return Ok(data.to_vec());
    }
    with_fs(|fs| fs.read(name))
}

pub fn read_string(name: &str) -> Result<String, String> {
    read(name).map(|b| String::from_utf8_lossy(&b).into_owned())
}

pub fn write(name: &str, data: &[u8]) -> Result<(), String> {
    if system_file(name).is_some() {
        return Err(format!("{name} is a read-only system file"));
    }
    let r = with_fs(|fs| fs.write(name, data));
    bump();
    r
}

pub fn delete(name: &str) -> Result<(), String> {
    if system_file(name).is_some() {
        return Err(format!("{name} is a read-only system file"));
    }
    let r = with_fs(|fs| fs.delete(name));
    bump();
    r
}

pub fn exists(name: &str) -> bool {
    system_file(name).is_some() || with_fs(|fs| Ok(fs.find(name).is_some())).unwrap_or(false)
}

/// Files under `prefix`: NovaFS files plus read-only system files, sorted.
pub fn list(prefix: &str) -> Vec<(String, u64, u64)> {
    let mut v = with_fs(|fs| Ok(fs.list(prefix))).unwrap_or_default();
    for (name, data) in SYSTEM_FILES.lock().range(String::from(prefix)..) {
        if !name.starts_with(prefix) {
            break;
        }
        if !v.iter().any(|(n, _, _)| n == name) {
            v.push((name.clone(), data.len() as u64, 0));
        }
    }
    v.sort();
    v
}

pub fn usage() -> Option<(u64, u64)> {
    with_fs(|fs| Ok(fs.usage())).ok()
}

/// Every file as (name, contents): used to carry a live session's files over to an
/// installed system.
pub fn snapshot() -> Vec<(String, Vec<u8>)> {
    let names = with_fs(|fs| Ok(fs.list(""))).unwrap_or_default();
    names.into_iter().filter_map(|(name, _, _)| with_fs(|fs| fs.read(&name)).ok().map(|d| (name, d))).collect()
}
