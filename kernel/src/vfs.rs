//! Virtual file system for Linux programs: a mount table over file-system backends, with
//! symbolic links, hard links, permissions and directories that exist on their own.
//!
//! Paths inside Nova are canonical and relative to the root, without a leading slash
//! (`usr/lib/libc.so.6`; `""` is the root itself). [`walk`] turns a program's path into
//! one: it joins the working directory, applies `.` and `..`, and follows symbolic links in
//! every component (and in the last one unless asked not to). [`find_mount`] then picks
//! the mount that holds the result and the path inside its backend.
//!
//! Backends:
//! * [`NovaFsBackend`]: NovaFS plus the read-only system files (programs, libraries, the
//!   Chromium engine), what Nova's own apps see. NovaFS keeps flat path names, so the
//!   things it cannot store (symbolic links, permissions, empty directories) live in one
//!   metadata file, `.nova-vfs`.
//! * [`TmpFs`]: an in-memory tree of inodes (directories, files, symbolic links, hard
//!   links). File contents are shared-memory objects ([`ipc::SharedMemory`]), so every
//!   open file and every `MAP_SHARED` mapping sees the same pages. It has no 1024-file or
//!   88-byte-name limit, which large programs (Steam and its runtime) need.
//!
//! Default mounts: `/` is NovaFS; `/run` and `/home` are tmpfs.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::ipc::{self, SharedRef};
use crate::sync::SpinLock;

pub const EPERM: i64 = 1;
pub const EACCES: i64 = 13;
pub const ENOENT: i64 = 2;
pub const EEXIST: i64 = 17;
pub const EXDEV: i64 = 18;
pub const ENOTDIR: i64 = 20;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const ENOSPC: i64 = 28;
pub const EROFS: i64 = 30;
pub const ENAMETOOLONG: i64 = 36;
pub const ENOTEMPTY: i64 = 39;
pub const ELOOP: i64 = 40;

/// Linux follows at most 40 symbolic links while resolving one path.
const MAX_LINKS: u32 = 40;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Type {
    Reg,
    Dir,
    Lnk,
    /// Character device (DevFs): opening it gives the device named by Content::Device.
    Chr,
}

impl Type {
    /// The S_IFMT bits.
    pub fn mode(self) -> u32 {
        match self {
            Type::Reg => 0o100000,
            Type::Dir => 0o040000,
            Type::Lnk => 0o120000,
            Type::Chr => 0o020000,
        }
    }
    /// The d_type of a directory entry.
    pub fn dtype(self) -> u8 {
        match self {
            Type::Reg => 8,
            Type::Dir => 4,
            Type::Lnk => 10,
            Type::Chr => 2,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Meta {
    pub ty: Type,
    /// Permission bits (07777).
    pub perm: u32,
    pub size: u64,
    /// Modification time, Unix seconds.
    pub mtime: u64,
    pub ino: u64,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
}

/// The contents of a regular file being opened.
pub enum Content {
    /// A read-only system file, used in place.
    Static(&'static [u8]),
    /// A copy, written back through [`Backend::write_back`] (NovaFS).
    Copy(Vec<u8>),
    /// The file's own pages, shared by every opener (tmpfs).
    Shared(SharedRef),
    /// A device node: null, zero, full, random, urandom, tty.
    Device(&'static str),
}

/// A file system. Paths are relative to its root, canonical, and contain no symbolic
/// links except possibly in the last component (the VFS resolves the rest first).
pub trait Backend: Send + Sync {
    /// For /proc/mounts and statfs: "tmpfs", "novafs", ...
    fn kind(&self) -> &'static str;
    fn lstat(&self, p: &str) -> Result<Meta, i64>;
    /// The target if `p` is a symbolic link (the cheap check path walks make for every
    /// component).
    fn link_target(&self, p: &str) -> Option<String>;
    /// What readlink reports: the link target, or for magic links that path walks do not
    /// follow (/proc/self/fd/N), a description.
    fn readlink(&self, p: &str) -> Option<String> {
        self.link_target(p)
    }
    /// Entries of directory `p` (without . and ..): name, type, inode.
    fn list(&self, p: &str) -> Result<Vec<(String, Type, u64)>, i64>;
    /// An existing regular file's contents.
    fn open(&self, p: &str) -> Result<Content, i64>;
    /// Create an empty regular file (EEXIST if `p` exists).
    fn create(&self, p: &str, perm: u32) -> Result<(), i64>;
    /// Store the contents of a file opened as [`Content::Copy`].
    fn write_back(&self, _p: &str, _data: &[u8]) -> Result<(), i64> {
        Ok(())
    }
    fn mkdir(&self, p: &str, perm: u32) -> Result<(), i64>;
    fn rmdir(&self, p: &str) -> Result<(), i64>;
    /// Remove a non-directory.
    fn unlink(&self, p: &str) -> Result<(), i64>;
    /// Rename within this backend, replacing a non-directory (or empty directory) target.
    fn rename(&self, from: &str, to: &str) -> Result<(), i64>;
    fn symlink(&self, target: &str, p: &str) -> Result<(), i64>;
    /// A hard link: `to` names the same file as `from`.
    fn link(&self, from: &str, to: &str) -> Result<(), i64>;
    fn chmod(&self, p: &str, perm: u32) -> Result<(), i64>;
    fn chown(&self, _p: &str, _uid: u32, _gid: u32) -> Result<(), i64> {
        Ok(())
    }
    fn truncate(&self, p: &str, len: u64) -> Result<(), i64>;
    fn set_mtime(&self, _p: &str, _mtime: u64) -> Result<(), i64> {
        Ok(())
    }
    /// (used, total) bytes.
    fn usage(&self) -> (u64, u64);
}

fn now() -> u64 {
    crate::rtc::unix_time()
}

/// `a/b` joined with `c`, either side possibly empty.
pub fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.into(),
        (_, true) => a.into(),
        _ => format!("{a}/{b}"),
    }
}

/// (parent, last component) of a canonical path.
pub fn split(p: &str) -> (&str, &str) {
    p.rsplit_once('/').unwrap_or(("", p))
}

// ---------------------------------------------------------------- mounts and path walks

/// A mounted file system: a backend and the directory of it shown at the mount point.
pub struct Mount {
    pub fs: Arc<dyn Backend>,
    /// The backend directory shown there (bind mounts of a subdirectory).
    pub root: String,
    pub readonly: bool,
}

impl Mount {
    fn new(fs: Arc<dyn Backend>, root: &str, readonly: bool) -> Arc<Self> {
        Arc::new(Self { fs, root: root.into(), readonly })
    }
}

/// A place in a file system that does not depend on mount paths: an open directory, the
/// working directory, a file being written back. [`Namespace::vpath`] finds where it is
/// visible now (pivot_root moves everything).
#[derive(Clone)]
pub struct At {
    pub mount: Arc<Mount>,
    pub rel: String,
}

/// One mount in a namespace's tree: `parent` is the mount it sits on (the same id for
/// the root). Mounts stacked at one path are children of the one below them.
#[derive(Clone)]
struct Entry {
    id: u64,
    parent: u64,
    path: String,
    m: Arc<Mount>,
}

/// A mount namespace: a tree of mounts. Processes share one until unshare(CLONE_NEWNS)
/// or clone with CLONE_NEWNS copies it.
pub struct Namespace {
    inner: SpinLock<Tree>,
}

#[derive(Clone)]
struct Tree {
    entries: Vec<Entry>,
    root: u64,
}

static NEXT_MOUNT: AtomicU64 = AtomicU64::new(20);

fn under(p: &str, mount: &str) -> bool {
    mount.is_empty() || p == mount || (p.starts_with(mount) && p.as_bytes().get(mount.len()) == Some(&b'/'))
}

/// `p` without the leading `prefix/` (`prefix` must contain it, see [`under`]).
fn strip<'a>(p: &'a str, prefix: &str) -> &'a str {
    if prefix.is_empty() { p } else { p[prefix.len()..].trim_start_matches('/') }
}

static ROOT: SpinLock<Option<Arc<Namespace>>> = SpinLock::new(None);

/// The initial namespace, created on first use: NovaFS at `/`, tmpfs at `/run` and
/// `/home`, plus the Linux layer's /proc, /dev and /sys.
pub fn root() -> Arc<Namespace> {
    let mut r = ROOT.lock();
    r.get_or_insert_with(|| {
        let root_id = NEXT_MOUNT.fetch_add(1, Ordering::Relaxed);
        let mut entries = alloc::vec![Entry { id: root_id, parent: root_id, path: String::new(), m: Mount::new(Arc::new(NovaFsBackend::new()), "", false) }];
        let mut more: Vec<(&str, Arc<dyn Backend>)> = alloc::vec![("run", Arc::new(TmpFs::new())), ("home", Arc::new(TmpFs::new()))];
        more.extend(crate::linux::special_mounts());
        for (path, fs) in more {
            entries.push(Entry { id: NEXT_MOUNT.fetch_add(1, Ordering::Relaxed), parent: root_id, path: path.into(), m: Mount::new(fs, "", false) });
        }
        Arc::new(Namespace { inner: SpinLock::new(Tree { entries, root: root_id }) })
    })
    .clone()
}

impl Tree {
    fn get(&self, id: u64) -> &Entry {
        self.entries.iter().find(|e| e.id == id).expect("mount id")
    }

    /// The mount that holds `p`: start at the root and cross into child mounts along the
    /// way; a mount stacked on the current one (same path) hides it.
    fn find(&self, p: &str) -> &Entry {
        let mut cur = self.get(self.root);
        loop {
            let kids = || self.entries.iter().filter(move |e| e.parent == cur.id && e.id != cur.id);
            if let Some(top) = kids().filter(|e| e.path == cur.path).last() {
                cur = top;
                continue;
            }
            // The first mount the walk to `p` crosses (newest if several at one path).
            let next = kids().filter(|e| under(p, &e.path)).fold(None::<&Entry>, |best, e| match best {
                Some(b) if b.path.len() < e.path.len() => Some(b),
                _ => Some(e),
            });
            match next {
                Some(n) => cur = n,
                None => return cur,
            }
        }
    }

    /// Ids of `id` and every mount below it.
    fn subtree(&self, id: u64) -> Vec<u64> {
        let mut out = alloc::vec![id];
        let mut i = 0;
        while i < out.len() {
            let p = out[i];
            out.extend(self.entries.iter().filter(|e| e.parent == p && e.id != p).map(|e| e.id));
            i += 1;
        }
        out
    }
}

impl Namespace {
    /// A private copy (unshare/clone with CLONE_NEWNS, chroot).
    pub fn copy(&self) -> Arc<Namespace> {
        Arc::new(Namespace { inner: SpinLock::new(self.inner.lock().clone()) })
    }

    /// The mount holding canonical path `p` and the path inside its backend.
    pub fn find_mount(&self, p: &str) -> (Arc<Mount>, String) {
        let t = self.inner.lock();
        let e = t.find(p);
        (e.m.clone(), join(&e.m.root, strip(p, &e.path)))
    }

    pub fn at(&self, p: &str) -> At {
        let (mount, rel) = self.find_mount(p);
        At { mount, rel }
    }

    /// Where `at` is visible in this namespace now (None: not reachable).
    pub fn vpath(&self, at: &At) -> Option<String> {
        let t = self.inner.lock();
        let shown = |e: &Entry| under(&at.rel, &e.m.root) && t.find(&e.path).id == e.id;
        let e = t.entries.iter().rev().find(|e| Arc::ptr_eq(&e.m, &at.mount) && shown(e)).or_else(|| t.entries.iter().rev().find(|e| Arc::ptr_eq(&e.m.fs, &at.mount.fs) && shown(e)))?;
        Some(join(&e.path, strip(&at.rel, &e.m.root)))
    }

    /// Mount points directly inside directory `dir`, as names.
    pub fn mounts_in(&self, dir: &str) -> Vec<String> {
        let t = self.inner.lock();
        t.entries.iter().filter(|e| !e.path.is_empty() && split(&e.path).0 == dir && t.find(&e.path).id == e.id).map(|e| split(&e.path).1.to_string()).collect()
    }

    pub fn is_mount_point(&self, p: &str) -> bool {
        let t = self.inner.lock();
        let e = t.find(p);
        e.path == p
    }

    /// Mount `fs` (its directory `root`) at `path`, on top of whatever is there.
    pub fn mount(&self, path: &str, fs: Arc<dyn Backend>, root: &str, readonly: bool) {
        let mut t = self.inner.lock();
        let parent = t.find(path).id;
        t.entries.push(Entry { id: NEXT_MOUNT.fetch_add(1, Ordering::Relaxed), parent, path: path.into(), m: Mount::new(fs, root, readonly) });
    }

    /// Bind mount: show what is at `src` also at `dst`; with `recursive`, the mounts
    /// below `src` too.
    pub fn bind(&self, src: &str, dst: &str, recursive: bool) -> Result<(), i64> {
        let mut t = self.inner.lock();
        let s = t.find(src).clone();
        let parent = t.find(dst).id;
        let top = NEXT_MOUNT.fetch_add(1, Ordering::Relaxed);
        let mut new = alloc::vec![Entry { id: top, parent, path: dst.into(), m: Mount::new(s.m.fs.clone(), &join(&s.m.root, strip(src, &s.path)), s.m.readonly) }];
        if recursive {
            // Copy the mounts below src (in the tree under the mount holding it), keeping
            // their shape.
            let mut map = BTreeMap::new();
            map.insert(s.id, top);
            for id in t.subtree(s.id).into_iter().skip(1) {
                let e = t.get(id).clone();
                if !under(&e.path, src) {
                    continue;
                }
                let Some(&np) = map.get(&e.parent) else { continue };
                let nid = NEXT_MOUNT.fetch_add(1, Ordering::Relaxed);
                map.insert(e.id, nid);
                new.push(Entry { id: nid, parent: np, path: join(dst, strip(&e.path, src)), m: e.m.clone() });
            }
        }
        t.entries.extend(new);
        Ok(())
    }

    /// Change a mount's flags (mount with MS_REMOUNT): the topmost mount at exactly `path`.
    pub fn remount(&self, path: &str, readonly: bool) -> Result<(), i64> {
        let mut t = self.inner.lock();
        let id = t.find(path).id;
        let e = t.entries.iter_mut().find(|e| e.id == id).unwrap();
        if e.path != path {
            return Err(EINVAL);
        }
        e.m = Mount::new(e.m.fs.clone(), &e.m.root.clone(), readonly);
        Ok(())
    }

    /// mount_setattr: make the mount at `path` (and with `recursive`, every mount below
    /// it) read-only or writable.
    pub fn set_readonly(&self, path: &str, readonly: bool, recursive: bool) -> Result<(), i64> {
        let mut t = self.inner.lock();
        let top = t.find(path).clone();
        let ids = if recursive { t.subtree(top.id) } else { alloc::vec![top.id] };
        for e in t.entries.iter_mut().filter(|e| ids.contains(&e.id)) {
            if e.m.readonly != readonly {
                e.m = Mount::new(e.m.fs.clone(), &e.m.root.clone(), readonly);
            }
        }
        Ok(())
    }

    /// Unmount the topmost mount at `path`, with everything mounted below it.
    pub fn umount(&self, path: &str) -> Result<(), i64> {
        let mut t = self.inner.lock();
        let e = t.find(path).clone();
        if e.path != path || e.id == t.root {
            return Err(EINVAL);
        }
        let gone = t.subtree(e.id);
        t.entries.retain(|x| !gone.contains(&x.id));
        Ok(())
    }

    /// Move the topmost mount at `src` (and its subtree) to `dst` (MS_MOVE).
    pub fn move_mount(&self, src: &str, dst: &str) -> Result<(), i64> {
        let mut t = self.inner.lock();
        let e = t.find(src).clone();
        if e.path != src || e.id == t.root {
            return Err(EINVAL);
        }
        let parent = t.find(dst).id;
        let ids = t.subtree(e.id);
        for x in t.entries.iter_mut().filter(|x| ids.contains(&x.id)) {
            x.path = join(dst, strip(&x.path.clone(), src));
            if x.id == e.id {
                x.parent = parent;
            }
        }
        Ok(())
    }

    /// pivot_root(new_root, put_old): the mount at `new_root` becomes the root, and the old
    /// root is mounted at `put_old` (a path at or below `new_root`).
    pub fn pivot_root(&self, new_root: &str, put_old: &str) -> Result<(), i64> {
        let mut t = self.inner.lock();
        let n = t.find(new_root).clone();
        if n.path != new_root || n.id == t.root || !under(put_old, new_root) {
            return Err(EINVAL);
        }
        let p = t.find(put_old).id;
        let old_rel = strip(put_old, new_root).to_string();
        let new_tree = t.subtree(n.id);
        let old_root = t.root;
        for e in t.entries.iter_mut() {
            if new_tree.contains(&e.id) {
                e.path = strip(&e.path.clone(), new_root).into();
            } else {
                e.path = join(&old_rel, &e.path.clone());
            }
            if e.id == n.id {
                e.parent = e.id;
            } else if e.id == old_root {
                e.parent = p;
            }
        }
        t.root = n.id;
        Ok(())
    }

    /// chroot(dir): a namespace whose root shows `dir`, with the mounts below it.
    pub fn chroot(&self, dir: &str) -> Arc<Namespace> {
        let t = self.inner.lock();
        let holder = t.find(dir).clone();
        let rid = NEXT_MOUNT.fetch_add(1, Ordering::Relaxed);
        let mut entries = alloc::vec![Entry { id: rid, parent: rid, path: String::new(), m: Mount::new(holder.m.fs.clone(), &join(&holder.m.root, strip(dir, &holder.path)), holder.m.readonly) }];
        let mut map = BTreeMap::new();
        map.insert(holder.id, rid);
        for id in t.subtree(holder.id).into_iter().skip(1) {
            let e = t.get(id);
            if !under(&e.path, dir) {
                continue;
            }
            let Some(&np) = map.get(&e.parent) else { continue };
            let nid = NEXT_MOUNT.fetch_add(1, Ordering::Relaxed);
            map.insert(e.id, nid);
            entries.push(Entry { id: nid, parent: np, path: strip(&e.path, dir).into(), m: e.m.clone() });
        }
        Arc::new(Namespace { inner: SpinLock::new(Tree { entries, root: rid }) })
    }

    /// /proc/self/mountinfo: "id parent 0:dev root mountpoint options - type source super".
    pub fn mountinfo(&self) -> String {
        let t = self.inner.lock();
        let mut out = String::new();
        for id in t.subtree(t.root) {
            let e = t.get(id);
            let kind = e.m.fs.kind();
            let ro = if e.m.readonly { "ro" } else { "rw" };
            out.push_str(&format!("{} {} 0:{} /{} /{} {ro},relatime - {kind} {kind} {ro}\n", e.id, if e.id == t.root { 1 } else { e.parent }, e.id, e.m.root, e.path));
        }
        out
    }

    /// /proc/mounts: "type mountpoint type options 0 0".
    pub fn mounts(&self) -> String {
        let t = self.inner.lock();
        t.subtree(t.root).iter().map(|&id| {
            let e = t.get(id);
            format!("{0} /{1} {0} {2},relatime 0 0\n", e.m.fs.kind(), e.path, if e.m.readonly { "ro" } else { "rw" })
        }).collect()
    }

    /// Canonical form of `path` (relative paths start at `cwd`), with symbolic links in
    /// every component followed; in the last one only if `follow`. The result need not
    /// exist (open with O_CREAT, mkdir, ...).
    pub fn walk(&self, cwd: &str, path: &str, follow: bool) -> Result<String, i64> {
        let mut todo: VecDeque<String> = VecDeque::new();
        if !path.starts_with('/') {
            todo.extend(cwd.split('/').map(String::from));
        }
        todo.extend(path.split('/').map(String::from));
        let mut done: Vec<String> = Vec::new();
        let mut links = 0;
        while let Some(c) = todo.pop_front() {
            match c.as_str() {
                "" | "." => continue,
                ".." => {
                    done.pop();
                    continue;
                }
                _ => {}
            }
            if c.len() > 255 {
                return Err(ENAMETOOLONG);
            }
            done.push(c);
            let last = todo.iter().all(|c| c.is_empty() || c == ".");
            if last && !follow {
                continue;
            }
            let p = done.join("/");
            let (m, rel) = self.find_mount(&p);
            if rel == m.root && self.is_mount_point(&p) {
                continue;
            }
            if let Some(target) = m.fs.link_target(&rel) {
                links += 1;
                if links > MAX_LINKS {
                    return Err(ELOOP);
                }
                done.pop();
                if target.starts_with('/') {
                    done.clear();
                }
                for c in target.split('/').rev() {
                    todo.push_front(c.into());
                }
            }
        }
        Ok(done.join("/"))
    }

    /// [`Namespace::walk`] with `root` as the root: absolute paths, absolute link targets
    /// and `..` stay inside it (openat2 with RESOLVE_IN_ROOT).
    pub fn walk_in(&self, root: &str, path: &str, follow: bool) -> Result<String, i64> {
        let base: Vec<String> = root.split('/').filter(|c| !c.is_empty()).map(String::from).collect();
        let mut todo: VecDeque<String> = path.split('/').map(String::from).collect();
        let mut done = base.clone();
        let mut links = 0;
        while let Some(c) = todo.pop_front() {
            match c.as_str() {
                "" | "." => continue,
                ".." => {
                    if done.len() > base.len() {
                        done.pop();
                    }
                    continue;
                }
                _ => {}
            }
            done.push(c);
            if todo.iter().all(|c| c.is_empty() || c == ".") && !follow {
                continue;
            }
            let p = done.join("/");
            let (m, rel) = self.find_mount(&p);
            if let Some(target) = m.fs.link_target(&rel) {
                links += 1;
                if links > MAX_LINKS {
                    return Err(ELOOP);
                }
                done.pop();
                if target.starts_with('/') {
                    done.truncate(base.len());
                }
                for c in target.split('/').rev() {
                    todo.push_front(c.into());
                }
            }
        }
        Ok(done.join("/"))
    }

    pub fn lstat(&self, p: &str) -> Result<Meta, i64> {
        let (m, rel) = self.find_mount(p);
        m.fs.lstat(&rel)
    }

    /// The backend for changing `p`: EROFS on read-only mounts.
    pub fn writable(&self, p: &str) -> Result<(Arc<Mount>, String), i64> {
        let (m, rel) = self.find_mount(p);
        if m.readonly {
            return Err(EROFS);
        }
        Ok((m, rel))
    }

    /// Both paths on one file system (rename, link), else EXDEV.
    pub fn same_mount(&self, a: &str, b: &str) -> Result<(Arc<Mount>, String, String), i64> {
        let (ma, ra) = self.writable(a)?;
        let (mb, rb) = self.writable(b)?;
        if !Arc::ptr_eq(&ma.fs, &mb.fs) {
            return Err(EXDEV);
        }
        Ok((ma, ra, rb))
    }

    /// A program file's bytes (exec, the dynamic loader): system files are used in place.
    pub fn read_program(&self, cwd: &str, path: &str) -> Result<alloc::borrow::Cow<'static, [u8]>, i64> {
        let p = self.walk(cwd, path, true)?;
        let (m, rel) = self.find_mount(&p);
        if m.fs.lstat(&rel)?.ty == Type::Dir {
            return Err(EISDIR);
        }
        Ok(match m.fs.open(&rel)? {
            Content::Static(s) => alloc::borrow::Cow::Borrowed(s),
            Content::Copy(v) => alloc::borrow::Cow::Owned(v),
            Content::Shared(s) => {
                let mut s = s.lock();
                let size = s.size as usize;
                alloc::borrow::Cow::Owned(s.read(0, size))
            }
            Content::Device(_) => return Err(EACCES),
        })
    }
}

// ---------------------------------------------------------------- NovaFS

/// NovaFS and the system files, with symbolic links, permissions and empty directories
/// recorded in the metadata file `.nova-vfs` (lines `L<TAB>path<TAB>target`,
/// `M<TAB>path<TAB>octal`, `D<TAB>path`).
pub struct NovaFsBackend {
    meta: SpinLock<Option<NovaMeta>>,
}

#[derive(Default)]
struct NovaMeta {
    links: BTreeMap<String, String>,
    modes: BTreeMap<String, u32>,
    dirs: BTreeMap<String, ()>,
}

const META_FILE: &str = ".nova-vfs";

impl NovaFsBackend {
    pub fn new() -> Self {
        Self { meta: SpinLock::new(None) }
    }

    fn with_meta<R>(&self, f: impl FnOnce(&mut NovaMeta) -> R) -> R {
        let mut g = self.meta.lock();
        let m = g.get_or_insert_with(|| {
            let mut m = NovaMeta::default();
            for line in crate::fs::read_string(META_FILE).unwrap_or_default().lines() {
                let mut parts = line.splitn(3, '\t');
                match (parts.next(), parts.next(), parts.next()) {
                    (Some("L"), Some(p), Some(t)) => {
                        m.links.insert(p.into(), t.into());
                    }
                    (Some("M"), Some(p), Some(o)) => {
                        m.modes.insert(p.into(), u32::from_str_radix(o, 8).unwrap_or(0o644));
                    }
                    (Some("D"), Some(p), None) => {
                        m.dirs.insert(p.into(), ());
                    }
                    _ => {}
                }
            }
            m
        });
        f(m)
    }

    /// Change the metadata and save it.
    fn update<R>(&self, f: impl FnOnce(&mut NovaMeta) -> R) -> Result<R, i64> {
        let (r, text) = self.with_meta(|m| {
            let r = f(m);
            let mut text = String::new();
            for (p, t) in &m.links {
                text.push_str(&format!("L\t{p}\t{t}\n"));
            }
            for (p, o) in &m.modes {
                text.push_str(&format!("M\t{p}\t{o:o}\n"));
            }
            for p in m.dirs.keys() {
                text.push_str(&format!("D\t{p}\n"));
            }
            (r, text)
        });
        crate::fs::write(META_FILE, text.as_bytes()).map_err(|_| ENOSPC)?;
        Ok(r)
    }

    fn is_file(&self, p: &str) -> Option<(u64, u64)> {
        if let Some(s) = crate::fs::system_file(p) {
            return Some((s.len() as u64, 0));
        }
        crate::fs::list(p).into_iter().find(|(n, _, _)| n == p).map(|(_, size, mtime)| (size, mtime))
    }

    fn is_dir(&self, p: &str) -> bool {
        p.is_empty() || matches!(p, "etc" | "tmp") || self.with_meta(|m| m.dirs.contains_key(p)) || !crate::fs::list(&format!("{p}/")).is_empty() || self.with_meta(|m| m.links.keys().any(|l| l.starts_with(p) && l.as_bytes().get(p.len()) == Some(&b'/')))
    }

    fn exists(&self, p: &str) -> bool {
        self.link_target(p).is_some() || self.is_file(p).is_some() || self.is_dir(p)
    }

    fn default_perm(p: &str) -> u32 {
        if p.starts_with("bin/") || p.contains("/bin/") || p.ends_with(".so") || p.contains(".so.") { 0o755 } else { 0o644 }
    }
}

impl Default for NovaFsBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn ino_of(p: &str) -> u64 {
    crate::storage::crc32(p.as_bytes()) as u64 + 2
}

impl Backend for NovaFsBackend {
    fn kind(&self) -> &'static str {
        "novafs"
    }

    fn lstat(&self, p: &str) -> Result<Meta, i64> {
        let ino = ino_of(p);
        if let Some(t) = self.link_target(p) {
            return Ok(Meta { ty: Type::Lnk, perm: 0o777, size: t.len() as u64, mtime: 0, ino, nlink: 1, uid: 0, gid: 0 });
        }
        let perm = self.with_meta(|m| m.modes.get(p).copied());
        if let Some((size, mtime)) = self.is_file(p) {
            return Ok(Meta { ty: Type::Reg, perm: perm.unwrap_or_else(|| Self::default_perm(p)), size, mtime, ino, nlink: 1, uid: 0, gid: 0 });
        }
        if self.is_dir(p) {
            return Ok(Meta { ty: Type::Dir, perm: perm.unwrap_or(if p == "tmp" { 0o1777 } else { 0o755 }), size: 4096, mtime: 0, ino, nlink: 2, uid: 0, gid: 0 });
        }
        if let Some(text) = crate::linux::etc_file(p) {
            return Ok(Meta { ty: Type::Reg, perm: 0o644, size: text.len() as u64, mtime: 0, ino, nlink: 1, uid: 0, gid: 0 });
        }
        Err(ENOENT)
    }

    fn link_target(&self, p: &str) -> Option<String> {
        self.with_meta(|m| m.links.get(p).cloned())
    }

    fn list(&self, p: &str) -> Result<Vec<(String, Type, u64)>, i64> {
        if !self.is_dir(p) {
            return Err(if self.exists(p) { ENOTDIR } else { ENOENT });
        }
        let prefix = if p.is_empty() { String::new() } else { format!("{p}/") };
        let mut names: BTreeMap<String, Type> = BTreeMap::new();
        for (name, _, _) in crate::fs::list(&prefix) {
            let rest = &name[prefix.len()..];
            if rest == ".dir" || (p.is_empty() && rest == META_FILE) {
                continue; // directory marker (mkdir), the metadata file
            }
            match rest.split_once('/') {
                Some((d, _)) => {
                    names.insert(d.into(), Type::Dir);
                }
                None => {
                    names.entry(rest.into()).or_insert(Type::Reg);
                }
            }
        }
        if p == "etc" {
            for name in crate::linux::ETC_FILES {
                names.entry(name.to_string()).or_insert(Type::Reg);
            }
        } else if p.is_empty() {
            names.entry("etc".into()).or_insert(Type::Dir);
            names.entry("tmp".into()).or_insert(Type::Dir);
        }
        self.with_meta(|m| {
            for (l, _) in m.links.range(prefix.clone()..).take_while(|(l, _)| l.starts_with(&prefix)) {
                match l[prefix.len()..].split_once('/') {
                    Some((d, _)) => names.insert(d.into(), Type::Dir),
                    None => names.insert(l[prefix.len()..].into(), Type::Lnk),
                };
            }
            for (d, _) in m.dirs.range(prefix.clone()..).take_while(|(d, _)| d.starts_with(&prefix)) {
                if let Some(first) = d[prefix.len()..].split('/').next() {
                    names.entry(first.into()).or_insert(Type::Dir);
                }
            }
        });
        Ok(names.into_iter().map(|(n, t)| (n.clone(), t, ino_of(&join(p, &n)))).collect())
    }

    fn open(&self, p: &str) -> Result<Content, i64> {
        if let Some(s) = crate::fs::system_file(p) {
            return Ok(Content::Static(s));
        }
        if self.is_file(p).is_none() {
            if let Some(text) = crate::linux::etc_file(p) {
                return Ok(Content::Copy(text.into_bytes()));
            }
            return Err(if self.is_dir(p) { EISDIR } else { ENOENT });
        }
        crate::fs::read(p).map(Content::Copy).map_err(|_| ENOENT)
    }

    fn create(&self, p: &str, perm: u32) -> Result<(), i64> {
        if self.exists(p) {
            return Err(EEXIST);
        }
        if p.is_empty() || p.len() > crate::fs::NAME_MAX {
            return Err(ENAMETOOLONG);
        }
        crate::fs::write(p, b"").map_err(|_| ENOSPC)?;
        if perm & 0o7777 != Self::default_perm(p) {
            self.update(|m| m.modes.insert(p.into(), perm & 0o7777))?;
        }
        Ok(())
    }

    fn write_back(&self, p: &str, data: &[u8]) -> Result<(), i64> {
        if crate::fs::system_file(p).is_some() {
            return Err(EROFS);
        }
        crate::fs::write(p, data).map_err(|_| ENOSPC)
    }

    fn mkdir(&self, p: &str, perm: u32) -> Result<(), i64> {
        if self.exists(p) {
            return Err(EEXIST);
        }
        self.update(|m| {
            m.dirs.insert(p.into(), ());
            if perm & 0o7777 != 0o755 {
                m.modes.insert(p.into(), perm & 0o7777);
            }
        })
    }

    fn rmdir(&self, p: &str) -> Result<(), i64> {
        if !self.is_dir(p) {
            return Err(if self.exists(p) { ENOTDIR } else { ENOENT });
        }
        if !self.list(p)?.is_empty() {
            return Err(ENOTEMPTY);
        }
        let _ = crate::fs::delete(&format!("{p}/.dir"));
        self.update(|m| {
            m.dirs.remove(p);
            m.modes.remove(p);
        })
    }

    fn unlink(&self, p: &str) -> Result<(), i64> {
        if self.link_target(p).is_some() {
            return self.update(|m| {
                m.links.remove(p);
            });
        }
        if self.is_file(p).is_none() {
            return Err(if self.is_dir(p) { EISDIR } else { ENOENT });
        }
        crate::fs::delete(p).map_err(|_| EROFS)?;
        if self.with_meta(|m| m.modes.contains_key(p)) {
            self.update(|m| m.modes.remove(p))?;
        }
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), i64> {
        if from == to {
            return Ok(());
        }
        if let Some(t) = self.link_target(from) {
            if self.is_file(to).is_some() {
                crate::fs::delete(to).map_err(|_| EROFS)?;
            }
            return self.update(|m| {
                m.links.remove(from);
                m.links.insert(to.into(), t);
            });
        }
        if self.is_file(from).is_some() {
            if self.is_dir(to) {
                return Err(EISDIR);
            }
            let data = crate::fs::read(from).map_err(|_| ENOENT)?;
            crate::fs::write(to, &data).map_err(|_| ENOSPC)?;
            crate::fs::delete(from).map_err(|_| EROFS)?;
            let mode = self.with_meta(|m| m.modes.get(from).copied());
            if mode.is_some() || self.link_target(to).is_some() {
                self.update(|m| {
                    m.links.remove(to);
                    if let Some(mode) = m.modes.remove(from) {
                        m.modes.insert(to.into(), mode);
                    }
                })?;
            }
            return Ok(());
        }
        if !self.is_dir(from) {
            return Err(ENOENT);
        }
        // A directory: move everything below it.
        if self.exists(to) && !(self.is_dir(to) && self.list(to)?.is_empty()) {
            return Err(ENOTEMPTY);
        }
        let (fp, tp) = (format!("{from}/"), format!("{to}/"));
        for (name, _, _) in crate::fs::list(&fp) {
            let data = crate::fs::read(&name).map_err(|_| ENOENT)?;
            crate::fs::write(&format!("{tp}{}", &name[fp.len()..]), &data).map_err(|_| ENOSPC)?;
            crate::fs::delete(&name).map_err(|_| EROFS)?;
        }
        let moved = |k: &String| k == from || k.starts_with(&fp);
        let renamed = |k: &String| if k == from { to.to_string() } else { format!("{tp}{}", &k[fp.len()..]) };
        self.update(|m| {
            m.links = core::mem::take(&mut m.links).into_iter().map(|(k, v)| if moved(&k) { (renamed(&k), v) } else { (k, v) }).collect();
            m.modes = core::mem::take(&mut m.modes).into_iter().map(|(k, v)| if moved(&k) { (renamed(&k), v) } else { (k, v) }).collect();
            m.dirs = core::mem::take(&mut m.dirs).into_iter().map(|(k, v)| if moved(&k) { (renamed(&k), v) } else { (k, v) }).collect();
            m.dirs.insert(to.into(), ());
        })
    }

    fn symlink(&self, target: &str, p: &str) -> Result<(), i64> {
        if self.exists(p) {
            return Err(EEXIST);
        }
        self.update(|m| {
            m.links.insert(p.into(), target.into());
        })
    }

    fn link(&self, from: &str, to: &str) -> Result<(), i64> {
        // NovaFS has no inodes: a hard link is a copy.
        if self.exists(to) {
            return Err(EEXIST);
        }
        if let Some(t) = self.link_target(from) {
            return self.symlink(&t, to);
        }
        if self.is_dir(from) {
            return Err(EPERM);
        }
        let data = match self.open(from)? {
            Content::Static(s) => s.to_vec(),
            Content::Copy(v) => v,
            Content::Shared(_) | Content::Device(_) => return Err(EINVAL),
        };
        crate::fs::write(to, &data).map_err(|_| ENOSPC)
    }

    fn chmod(&self, p: &str, perm: u32) -> Result<(), i64> {
        if !self.exists(p) {
            return Err(ENOENT);
        }
        self.update(|m| m.modes.insert(p.into(), perm & 0o7777)).map(|_| ())
    }

    fn truncate(&self, p: &str, len: u64) -> Result<(), i64> {
        let mut data = match self.open(p)? {
            Content::Static(_) => return Err(EROFS),
            Content::Copy(v) => v,
            Content::Shared(_) | Content::Device(_) => return Err(EINVAL),
        };
        data.resize(len as usize, 0);
        self.write_back(p, &data)
    }

    fn usage(&self) -> (u64, u64) {
        crate::fs::usage().unwrap_or((0, 1 << 30))
    }
}

// ---------------------------------------------------------------- tmpfs

struct Node {
    ty: Type,
    perm: u32,
    uid: u32,
    gid: u32,
    mtime: u64,
    nlink: u32,
    /// Regular files.
    data: Option<SharedRef>,
    /// Directories.
    children: BTreeMap<String, u64>,
    /// Symbolic links.
    target: String,
}

impl Node {
    fn new(ty: Type, perm: u32) -> Self {
        Self { ty, perm: perm & 0o7777, uid: 0, gid: 0, mtime: now(), nlink: 1, data: (ty == Type::Reg).then(ipc::memfd), children: BTreeMap::new(), target: String::new() }
    }
}

pub struct TmpFs {
    inner: SpinLock<BTreeMap<u64, Node>>,
    next: AtomicU64,
    /// Inode numbers are unique across tmpfs instances.
    base: u64,
}

static TMPFS_COUNT: AtomicU64 = AtomicU64::new(1);
const ROOT_INO: u64 = 1;

impl TmpFs {
    pub fn new() -> Self {
        let mut nodes = BTreeMap::new();
        let mut root = Node::new(Type::Dir, 0o755);
        root.nlink = 2;
        nodes.insert(ROOT_INO, root);
        Self { inner: SpinLock::new(nodes), next: AtomicU64::new(2), base: TMPFS_COUNT.fetch_add(1, Ordering::Relaxed) << 40 }
    }

    fn find(nodes: &BTreeMap<u64, Node>, p: &str) -> Result<u64, i64> {
        let mut ino = ROOT_INO;
        for c in p.split('/').filter(|c| !c.is_empty()) {
            let n = &nodes[&ino];
            if n.ty != Type::Dir {
                return Err(ENOTDIR);
            }
            ino = *n.children.get(c).ok_or(ENOENT)?;
        }
        Ok(ino)
    }

    /// (parent directory inode, name) for creating or removing `p`.
    fn parent<'a>(nodes: &BTreeMap<u64, Node>, p: &'a str) -> Result<(u64, &'a str), i64> {
        let (dir, name) = split(p);
        if name.is_empty() {
            return Err(EEXIST); // the root
        }
        let d = Self::find(nodes, dir)?;
        if nodes[&d].ty != Type::Dir {
            return Err(ENOTDIR);
        }
        Ok((d, name))
    }

    fn add(&self, p: &str, node: Node) -> Result<(), i64> {
        let mut nodes = self.inner.lock();
        let (d, name) = Self::parent(&nodes, p)?;
        if nodes[&d].children.contains_key(name) {
            return Err(EEXIST);
        }
        let is_dir = node.ty == Type::Dir;
        let ino = self.next.fetch_add(1, Ordering::Relaxed);
        nodes.insert(ino, node);
        let parent = nodes.get_mut(&d).unwrap();
        parent.children.insert(name.into(), ino);
        parent.mtime = now();
        if is_dir {
            parent.nlink += 1;
        }
        Ok(())
    }

    /// Drop one name of inode `ino`; the inode goes with its last name (open files keep
    /// their pages through their own reference).
    fn drop_link(nodes: &mut BTreeMap<u64, Node>, ino: u64) {
        let n = nodes.get_mut(&ino).unwrap();
        n.nlink = n.nlink.saturating_sub(if n.ty == Type::Dir { 2 } else { 1 });
        if n.nlink == 0 {
            nodes.remove(&ino);
        }
    }
}

impl Default for TmpFs {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for TmpFs {
    fn kind(&self) -> &'static str {
        "tmpfs"
    }

    fn lstat(&self, p: &str) -> Result<Meta, i64> {
        let nodes = self.inner.lock();
        let ino = Self::find(&nodes, p)?;
        let n = &nodes[&ino];
        let size = match n.ty {
            Type::Reg => n.data.as_ref().unwrap().lock().size,
            Type::Dir => 4096,
            Type::Lnk => n.target.len() as u64,
            Type::Chr => 0,
        };
        Ok(Meta { ty: n.ty, perm: n.perm, size, mtime: n.mtime, ino: self.base | ino, nlink: n.nlink, uid: n.uid, gid: n.gid })
    }

    fn link_target(&self, p: &str) -> Option<String> {
        let nodes = self.inner.lock();
        let n = &nodes[&Self::find(&nodes, p).ok()?];
        (n.ty == Type::Lnk).then(|| n.target.clone())
    }

    fn list(&self, p: &str) -> Result<Vec<(String, Type, u64)>, i64> {
        let nodes = self.inner.lock();
        let n = &nodes[&Self::find(&nodes, p)?];
        if n.ty != Type::Dir {
            return Err(ENOTDIR);
        }
        Ok(n.children.iter().map(|(name, &ino)| (name.clone(), nodes[&ino].ty, self.base | ino)).collect())
    }

    fn open(&self, p: &str) -> Result<Content, i64> {
        let nodes = self.inner.lock();
        let n = &nodes[&Self::find(&nodes, p)?];
        match n.ty {
            Type::Reg => Ok(Content::Shared(n.data.clone().unwrap())),
            Type::Dir => Err(EISDIR),
            Type::Lnk => Err(ELOOP),
            Type::Chr => Err(EINVAL),
        }
    }

    fn create(&self, p: &str, perm: u32) -> Result<(), i64> {
        self.add(p, Node::new(Type::Reg, perm))
    }

    fn mkdir(&self, p: &str, perm: u32) -> Result<(), i64> {
        let mut n = Node::new(Type::Dir, perm);
        n.nlink = 2;
        self.add(p, n)
    }

    fn rmdir(&self, p: &str) -> Result<(), i64> {
        let mut nodes = self.inner.lock();
        let (d, name) = Self::parent(&nodes, p)?;
        let ino = *nodes[&d].children.get(name).ok_or(ENOENT)?;
        let n = &nodes[&ino];
        if n.ty != Type::Dir {
            return Err(ENOTDIR);
        }
        if !n.children.is_empty() {
            return Err(ENOTEMPTY);
        }
        let parent = nodes.get_mut(&d).unwrap();
        parent.children.remove(name);
        parent.nlink -= 1;
        Self::drop_link(&mut nodes, ino);
        Ok(())
    }

    fn unlink(&self, p: &str) -> Result<(), i64> {
        let mut nodes = self.inner.lock();
        let (d, name) = Self::parent(&nodes, p)?;
        let ino = *nodes[&d].children.get(name).ok_or(ENOENT)?;
        if nodes[&ino].ty == Type::Dir {
            return Err(EISDIR);
        }
        nodes.get_mut(&d).unwrap().children.remove(name);
        Self::drop_link(&mut nodes, ino);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), i64> {
        if from == to {
            return Ok(());
        }
        if to.starts_with(from) && to.as_bytes().get(from.len()) == Some(&b'/') {
            return Err(EINVAL); // into itself
        }
        let mut nodes = self.inner.lock();
        let (fd, fname) = Self::parent(&nodes, from)?;
        let ino = *nodes[&fd].children.get(fname).ok_or(ENOENT)?;
        let (td, tname) = Self::parent(&nodes, to)?;
        let is_dir = nodes[&ino].ty == Type::Dir;
        if let Some(&old) = nodes[&td].children.get(tname) {
            let o = &nodes[&old];
            match (is_dir, o.ty == Type::Dir) {
                (true, false) => return Err(ENOTDIR),
                (false, true) => return Err(EISDIR),
                (true, true) if !o.children.is_empty() => return Err(ENOTEMPTY),
                _ => {}
            }
            if o.ty == Type::Dir {
                nodes.get_mut(&td).unwrap().nlink -= 1;
            }
            Self::drop_link(&mut nodes, old);
        }
        nodes.get_mut(&fd).unwrap().children.remove(fname);
        let t = nodes.get_mut(&td).unwrap();
        t.children.insert(tname.into(), ino);
        t.mtime = now();
        if is_dir && fd != td {
            nodes.get_mut(&td).unwrap().nlink += 1;
            nodes.get_mut(&fd).unwrap().nlink -= 1;
        }
        Ok(())
    }

    fn symlink(&self, target: &str, p: &str) -> Result<(), i64> {
        let mut n = Node::new(Type::Lnk, 0o777);
        n.target = target.into();
        self.add(p, n)
    }

    fn link(&self, from: &str, to: &str) -> Result<(), i64> {
        let mut nodes = self.inner.lock();
        let ino = Self::find(&nodes, from)?;
        if nodes[&ino].ty == Type::Dir {
            return Err(EPERM);
        }
        let (d, name) = Self::parent(&nodes, to)?;
        if nodes[&d].children.contains_key(name) {
            return Err(EEXIST);
        }
        nodes.get_mut(&d).unwrap().children.insert(name.into(), ino);
        nodes.get_mut(&ino).unwrap().nlink += 1;
        Ok(())
    }

    fn chmod(&self, p: &str, perm: u32) -> Result<(), i64> {
        let mut nodes = self.inner.lock();
        let ino = Self::find(&nodes, p)?;
        nodes.get_mut(&ino).unwrap().perm = perm & 0o7777;
        Ok(())
    }

    fn chown(&self, p: &str, uid: u32, gid: u32) -> Result<(), i64> {
        let mut nodes = self.inner.lock();
        let ino = Self::find(&nodes, p)?;
        let n = nodes.get_mut(&ino).unwrap();
        if uid != u32::MAX {
            n.uid = uid;
        }
        if gid != u32::MAX {
            n.gid = gid;
        }
        Ok(())
    }

    fn truncate(&self, p: &str, len: u64) -> Result<(), i64> {
        match self.open(p)? {
            Content::Shared(s) => s.lock().truncate(len),
            _ => unreachable!(),
        }
        Ok(())
    }

    fn set_mtime(&self, p: &str, mtime: u64) -> Result<(), i64> {
        let mut nodes = self.inner.lock();
        let ino = Self::find(&nodes, p)?;
        nodes.get_mut(&ino).unwrap().mtime = mtime;
        Ok(())
    }

    fn usage(&self) -> (u64, u64) {
        let nodes = self.inner.lock();
        let used: u64 = nodes.values().filter_map(|n| n.data.as_ref()).map(|d| d.lock().size).sum();
        let (heap_used, heap_free) = crate::memory::heap_usage();
        (used, used + (heap_free as u64).min((heap_used + heap_free) as u64 / 2))
    }
}
