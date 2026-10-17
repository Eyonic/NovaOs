//! inotify: file-change notifications (Chromium's file watchers, Steam, file pickers).
//!
//! Watches are kept on canonical paths (NovaFS names, as `resolve` returns them). The
//! system calls that change files call [`notify`]; it costs one atomic load while no
//! inotify instance exists. An event goes to watches on the file itself and, with the
//! file's name, to watches on its directory.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use alloc::collections::VecDeque;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::ipc::{self, POLLIN};
use crate::sync::SpinLock;

pub const IN_MODIFY: u32 = 0x2;
pub const IN_ATTRIB: u32 = 0x4;
pub const IN_CLOSE_WRITE: u32 = 0x8;
pub const IN_MOVED_FROM: u32 = 0x40;
pub const IN_MOVED_TO: u32 = 0x80;
pub const IN_CREATE: u32 = 0x100;
pub const IN_DELETE: u32 = 0x200;
pub const IN_DELETE_SELF: u32 = 0x400;
pub const IN_MOVE_SELF: u32 = 0x800;
const IN_Q_OVERFLOW: u32 = 0x4000;
const IN_IGNORED: u32 = 0x8000;
pub const IN_ONLYDIR: u32 = 0x0100_0000;
pub const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
const IN_MASK_CREATE: u32 = 0x1000_0000;
const IN_MASK_ADD: u32 = 0x2000_0000;
pub const IN_ISDIR: u32 = 0x4000_0000;
const IN_ONESHOT: u32 = 0x8000_0000;
/// Every event a watch can ask for.
const IN_ALL_EVENTS: u32 = 0xFFF;
/// Queued events per instance (Linux's default max_queued_events).
const QUEUE_LIMIT: usize = 16384;

struct Watch {
    wd: i32,
    path: String,
    mask: u32,
}

pub struct Inotify {
    watches: Vec<Watch>,
    next_wd: i32,
    queue: VecDeque<Vec<u8>>,
    overflowed: bool,
    generation: u64,
}

/// Every live instance; `ACTIVE` counts them so [`notify`] is free without any.
static INSTANCES: SpinLock<Vec<Weak<SpinLock<Inotify>>>> = SpinLock::new(Vec::new());
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static COOKIE: AtomicUsize = AtomicUsize::new(1);

pub type InotifyRef = Arc<SpinLock<Inotify>>;

pub fn new() -> InotifyRef {
    let i = Arc::new(SpinLock::new(Inotify { watches: Vec::new(), next_wd: 1, queue: VecDeque::new(), overflowed: false, generation: ipc::next_gen() }));
    let mut all = INSTANCES.lock();
    all.retain(|w| w.strong_count() > 0);
    all.push(Arc::downgrade(&i));
    ACTIVE.fetch_add(1, Ordering::Relaxed);
    i
}

impl Drop for Inotify {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Inotify {
    /// inotify_add_watch on canonical `path` (checked to exist by the caller).
    pub fn add_watch(&mut self, path: String, mask: u32) -> Result<i32, i64> {
        if mask & IN_ALL_EVENTS == 0 || (mask & IN_MASK_ADD != 0 && mask & IN_MASK_CREATE != 0) {
            return Err(super::EINVAL);
        }
        let flags = mask & (IN_ONESHOT | IN_EXCL_UNLINK);
        if let Some(w) = self.watches.iter_mut().find(|w| w.path == path) {
            if mask & IN_MASK_CREATE != 0 {
                return Err(super::EEXIST);
            }
            w.mask = if mask & IN_MASK_ADD != 0 { w.mask | mask & IN_ALL_EVENTS } else { mask & IN_ALL_EVENTS } | flags;
            return Ok(w.wd);
        }
        let wd = self.next_wd;
        self.next_wd += 1;
        self.watches.push(Watch { wd, path, mask: mask & IN_ALL_EVENTS | flags });
        Ok(wd)
    }

    pub fn rm_watch(&mut self, wd: i32) -> Result<(), i64> {
        let i = self.watches.iter().position(|w| w.wd == wd).ok_or(super::EINVAL)?;
        self.watches.remove(i);
        self.push(wd, IN_IGNORED, 0, "");
        Ok(())
    }

    fn push(&mut self, wd: i32, mask: u32, cookie: u32, name: &str) {
        if self.queue.len() >= QUEUE_LIMIT {
            if !self.overflowed {
                self.overflowed = true;
                self.queue.push_back(event(-1, IN_Q_OVERFLOW, 0, ""));
            }
            return;
        }
        // Linux merges an event identical to the last unread one.
        let e = event(wd, mask, cookie, name);
        if self.queue.back() != Some(&e) {
            self.queue.push_back(e);
        }
        self.generation = ipc::next_gen();
    }

    pub fn poll(&self) -> (u32, u64) {
        (if self.queue.is_empty() { 0 } else { POLLIN }, self.generation)
    }

    /// read(): whole events that fit in `len` bytes; EINVAL if not even the first fits.
    pub fn read(&mut self, len: usize) -> Result<Option<Vec<u8>>, i64> {
        let Some(first) = self.queue.front() else { return Ok(None) };
        if first.len() > len {
            return Err(super::EINVAL);
        }
        let mut out = Vec::new();
        while let Some(e) = self.queue.front() {
            if out.len() + e.len() > len {
                break;
            }
            out.extend(self.queue.pop_front().unwrap());
        }
        if self.queue.is_empty() {
            self.overflowed = false;
        }
        Ok(Some(out))
    }

    /// FIONREAD: bytes queued.
    pub fn pending(&self) -> usize {
        self.queue.iter().map(|e| e.len()).sum()
    }
}

/// struct inotify_event: wd, mask, cookie, len, then the name NUL-padded to 16 bytes.
fn event(wd: i32, mask: u32, cookie: u32, name: &str) -> Vec<u8> {
    let len = if name.is_empty() { 0 } else { (name.len() + 1).next_multiple_of(16) };
    let mut e = Vec::with_capacity(16 + len);
    e.extend_from_slice(&wd.to_le_bytes());
    e.extend_from_slice(&mask.to_le_bytes());
    e.extend_from_slice(&cookie.to_le_bytes());
    e.extend_from_slice(&(len as u32).to_le_bytes());
    e.extend_from_slice(name.as_bytes());
    e.resize(16 + len, 0);
    e
}

fn split(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// `path` changed (`mask` one of the IN_* events; IN_ISDIR added for directories).
pub fn notify(path: &str, mask: u32, is_dir: bool) {
    notify_cookie(path, mask, is_dir, 0);
}

fn notify_cookie(path: &str, mask: u32, is_dir: bool, cookie: u32) {
    if ACTIVE.load(Ordering::Relaxed) == 0 {
        return;
    }
    let (dir, name) = split(path);
    let dirflag = if is_dir { IN_ISDIR } else { 0 };
    let instances: Vec<InotifyRef> = INSTANCES.lock().iter().filter_map(|w| w.upgrade()).collect();
    let mut woke = false;
    for inst in instances {
        let mut i = inst.lock();
        let mut fired = Vec::new();
        for w in &i.watches {
            // The file's own watch: no name. Its directory's watch: with the name.
            let (hit, n) = if w.path == path && mask & (IN_MOVED_FROM | IN_MOVED_TO | IN_CREATE | IN_DELETE) == 0 {
                (true, "")
            } else if w.path == dir && !name.is_empty() && mask & (IN_DELETE_SELF | IN_MOVE_SELF) == 0 {
                (true, name)
            } else {
                (false, "")
            };
            if hit && w.mask & mask != 0 {
                fired.push((w.wd, n, w.mask & IN_ONESHOT != 0));
            }
        }
        for (wd, n, oneshot) in fired {
            i.push(wd, mask | dirflag, cookie, n);
            if oneshot {
                let _ = i.rm_watch(wd);
            }
            woke = true;
        }
        // A deleted or moved-away file's own watch ends.
        if mask & IN_DELETE_SELF != 0 {
            let gone: Vec<i32> = i.watches.iter().filter(|w| w.path == path).map(|w| w.wd).collect();
            for wd in gone {
                let _ = i.rm_watch(wd);
                woke = true;
            }
        }
    }
    if woke {
        ipc::notify();
    }
}

/// A file or directory was removed.
pub fn removed(path: &str, is_dir: bool) {
    notify(path, IN_DELETE, is_dir);
    notify(path, IN_DELETE_SELF, is_dir);
}

/// `from` was renamed to `to`: MOVED_FROM and MOVED_TO share a cookie, and watches on
/// the moved file follow it.
pub fn moved(from: &str, to: &str, is_dir: bool) {
    if ACTIVE.load(Ordering::Relaxed) == 0 {
        return;
    }
    let cookie = COOKIE.fetch_add(1, Ordering::Relaxed) as u32;
    notify_cookie(from, IN_MOVED_FROM, is_dir, cookie);
    notify_cookie(to, IN_MOVED_TO, is_dir, cookie);
    notify(from, IN_MOVE_SELF, is_dir);
    for inst in INSTANCES.lock().iter().filter_map(|w| w.upgrade()) {
        for w in inst.lock().watches.iter_mut().filter(|w| w.path == from) {
            w.path = String::from(to);
        }
    }
}
