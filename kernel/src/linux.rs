//! The Linux x86-64 system call interface for user programs (see process.rs).
//!
//! Numbers, argument registers (rdi, rsi, rdx, r10, r8, r9), structures and error
//! codes (negative errno in rax) follow the Linux ABI, so statically linked glibc
//! and musl programs, including C++ programs with libstdc++, run unmodified.
//!
//! What is behind them:
//! * files: NovaFS (flat paths; a "directory" is a path prefix). Paths are resolved
//!   against the process's working directory; `/a/b` is NovaFS file `a/b`.
//!   Files are read whole when opened and written back on close/exit.
//! * stdin/stdout/stderr: the process's terminal (cooked line input with echo).
//! * memory: brk and anonymous/private-file mmap, filled on demand.
//! * time: the RTC and the PIT; sleeping lets other tasks run.
//!
//! Not yet: threads (clone), fork/exec, signals, sockets, pipes. Unknown calls
//! return -ENOSYS and are logged once to the serial console.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use crate::input::Key;
use crate::process::{self, Process, SyscallFrame, UserState, PAGE};
use crate::signal;
use crate::gdt;
use crate::vfs;

pub mod compat32;
use crate::sync::SpinLock;
use crate::{ipc, pit, rand, rtc, task};

const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;
const ESPIPE: i64 = 29;
const ERANGE: i64 = 34;
const ENOSYS: i64 = 38;
const EAFNOSUPPORT: i64 = 97;
const ENOTSOCK: i64 = 88;
const ENOMEM_ERR: i64 = 12;
const EROFS: i64 = 30;
const EACCES: i64 = 13;
const ENODEV: i64 = 19;
const ELOOP: i64 = 40;
const O_NOFOLLOW: u64 = 0o400000;

const AT_FDCWD: i64 = -100;

/// readlink on a path that exists but is not a link gives EINVAL; otherwise ENOENT.
#[allow(non_snake_case)]
fn EINVAL_OR_NOENT(path: &str) -> i64 {
    if stat_path(path).is_ok() { EINVAL } else { ENOENT }
}

/// File contents: owned (NovaFS files) or borrowed read-only system files.
pub enum FileData {
    Static(&'static [u8]),
    Owned(Vec<u8>),
}

impl core::ops::Deref for FileData {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            FileData::Static(s) => s,
            FileData::Owned(v) => v,
        }
    }
}

impl FileData {
    fn to_mut(&mut self) -> &mut Vec<u8> {
        if let FileData::Static(s) = self {
            *self = FileData::Owned(s.to_vec());
        }
        match self {
            FileData::Owned(v) => v,
            FileData::Static(_) => unreachable!(),
        }
    }
}

pub enum Kind {
    Tty,
    Null,
    Random,
    /// Pipe end or Unix socket.
    Channel(ipc::End),
    EventFd(Arc<SpinLock<ipc::EventFd>>),
    /// signalfd: reads the pending signals in its mask as signalfd_siginfo records.
    SignalFd(u64),
    Epoll(ipc::Epoll),
    /// Shared-memory object: memfd, POSIX shm, or (with its path) a tmpfs file.
    Memfd(ipc::SharedRef, Option<vfs::At>),
    /// AF_INET TCP or UDP socket (smoltcp, net.rs).
    Inet(InetSocket),
    /// /dev/zero (and /dev/full): reads zeros, writes are discarded.
    Zero,
    /// A regular file read into memory (NovaFS) or used in place (system files); `at`
    /// is where it is written back.
    File { path: String, at: vfs::At, data: FileData, writable: bool, append: bool, dirty: bool },
    Dir { at: vfs::At },
}

pub struct OpenFile {
    kind: Kind,
    pos: usize,
    flags: u64,
    /// Where it was opened, for any file opened by path (devices too): fchdir, *at
    /// calls, /proc/self/fd links.
    at: Option<vfs::At>,
}

pub type FileRef = Arc<SpinLock<OpenFile>>;

pub struct InetSocket {
    handle: smoltcp::iface::SocketHandle,
    udp: bool,
    local_port: u16,
    /// Peer address (TCP: connected to; UDP: connect()ed default destination).
    peer: Option<(smoltcp::wire::Ipv4Address, u16)>,
    /// Last connect error to report through SO_ERROR (0 = none).
    error: i64,
}

impl Drop for InetSocket {
    fn drop(&mut self) {
        crate::net::sock_close(self.handle, self.udp);
    }
}

const EINPROGRESS: i64 = 115;
const ECONNREFUSED: i64 = 111;
const ENOTCONN: i64 = 107;
const EDESTADDRREQ: i64 = 89;
const ENETUNREACH: i64 = 101;

fn sockaddr_in(b: &[u8]) -> Result<(smoltcp::wire::Ipv4Address, u16), i64> {
    if b.len() < 8 || u16::from_le_bytes([b[0], b[1]]) != 2 {
        return Err(EAFNOSUPPORT);
    }
    Ok((smoltcp::wire::Ipv4Address::new(b[4], b[5], b[6], b[7]), u16::from_be_bytes([b[2], b[3]])))
}

fn encode_sockaddr_in(ip: smoltcp::wire::Ipv4Address, port: u16) -> Vec<u8> {
    let mut b = vec![0u8; 16];
    b[0..2].copy_from_slice(&2u16.to_le_bytes());
    b[2..4].copy_from_slice(&port.to_be_bytes());
    b[4..8].copy_from_slice(&ip.octets());
    b
}

/// Write a socket address to user (addr*, addrlen*) if both are given.
fn put_addr(addr: u64, lenp: u64, sa: &[u8]) -> Result<(), i64> {
    if addr != 0 && lenp != 0 {
        let cap = read_u32(lenp)? as usize;
        write_user(addr, &sa[..sa.len().min(cap)])?;
        write_user(lenp, &(sa.len() as u32).to_le_bytes())?;
    }
    Ok(())
}

fn sock_err(e: crate::net::SockErr) -> i64 {
    match e {
        crate::net::SockErr::WouldBlock => EAGAIN,
        crate::net::SockErr::Closed => EPIPE,
        crate::net::SockErr::Refused => ECONNREFUSED,
        crate::net::SockErr::NoNetwork => ENETUNREACH,
    }
}

/// Send on an inet socket (blocking unless non-blocking). `to` overrides the peer (UDP).
fn inet_send(file: &FileRef, data: &[u8], to: Option<(smoltcp::wire::Ipv4Address, u16)>, dontwait: bool) -> Result<usize, i64> {
    loop {
        {
            let mut f = file.lock();
            let nonblock = f.flags & O_NONBLOCK != 0 || dontwait;
            let Kind::Inet(s) = &mut f.kind else { return Err(EINVAL) };
            let r = if s.udp {
                let (ip, port) = to.or(s.peer).ok_or(EDESTADDRREQ)?;
                if s.local_port == 0 {
                    s.local_port = crate::net::udp_bind(s.handle, 0).map_err(sock_err)?;
                }
                crate::net::udp_send(s.handle, data, ip, port)
            } else {
                crate::net::tcp_send(s.handle, data)
            };
            match r {
                Ok(n) => return Ok(n),
                Err(crate::net::SockErr::WouldBlock) if !nonblock => {}
                Err(e) => return Err(sock_err(e)),
            }
        }
        ipc::wait(None);
        process::check_exiting();
    }
}

/// Receive from an inet socket. Returns data and the sender (UDP).
fn inet_recv(file: &FileRef, max: usize, dontwait: bool, peek: bool) -> Result<(Vec<u8>, Option<(smoltcp::wire::Ipv4Address, u16)>), i64> {
    let mut buf = vec![0u8; max.min(1 << 20)];
    loop {
        {
            let mut f = file.lock();
            let nonblock = f.flags & O_NONBLOCK != 0 || dontwait;
            let Kind::Inet(s) = &mut f.kind else { return Err(EINVAL) };
            let r = if s.udp {
                if s.local_port == 0 {
                    s.local_port = crate::net::udp_bind(s.handle, 0).map_err(sock_err)?;
                }
                crate::net::udp_recv(s.handle, &mut buf, peek).map(|(n, ip, port)| (n, Some((ip, port))))
            } else {
                crate::net::tcp_recv(s.handle, &mut buf, peek).map(|n| (n, None))
            };
            match r {
                Ok((n, from)) => {
                    buf.truncate(n);
                    return Ok((buf, from));
                }
                Err(crate::net::SockErr::WouldBlock) if !nonblock => {}
                Err(e) => return Err(sock_err(e)),
            }
        }
        ipc::wait(None);
        process::check_exiting();
    }
}

fn is_inet(f: &FileRef) -> bool {
    matches!(f.lock().kind, Kind::Inet(_))
}

/// Named POSIX shared-memory objects (/dev/shm).
static SHM: SpinLock<alloc::collections::BTreeMap<String, ipc::SharedRef>> = SpinLock::new(alloc::collections::BTreeMap::new());

const O_NONBLOCK: u64 = 0o4000;
/// O_CLOEXEC, which is also SOCK_CLOEXEC, EFD_CLOEXEC and EPOLL_CLOEXEC.
const O_CLOEXEC: u64 = 0o2000000;
const EPIPE: i64 = 32;

fn new_file(kind: Kind, flags: u64) -> FileRef {
    Arc::new(SpinLock::new(OpenFile { kind, pos: 0, flags, at: None }))
}

/// Readiness of a file for poll/epoll: (events, generation).
fn poll_file(f: &FileRef) -> (u32, u64) {
    let f = f.lock();
    match &f.kind {
        Kind::Channel(end) => end.poll(),
        Kind::EventFd(e) => e.lock().poll(),
        Kind::SignalFd(mask) => (if signal::pending_masked(thread(), *mask) { ipc::POLLIN } else { 0 }, 0),
        Kind::Inet(s) => {
            let (r, w, hup, err) = crate::net::sock_poll(s.handle, s.udp);
            let mut ev = 0;
            if r {
                ev |= ipc::POLLIN;
            }
            if w {
                ev |= ipc::POLLOUT;
            }
            if hup {
                ev |= ipc::POLLHUP | ipc::POLLRDHUP;
            }
            if err {
                ev |= ipc::POLLERR;
            }
            (ev, crate::pit::nanos() >> 20)
        }
        Kind::Tty => {
            let input = task::current_tty().is_some_and(|t| crate::tty::has_input(&t)) || !fds_stdin_empty();
            (ipc::POLLOUT | if input { ipc::POLLIN } else { 0 }, rtc::unix_time())
        }
        _ => (ipc::POLLIN | ipc::POLLOUT, 0),
    }
}

fn fds_stdin_empty() -> bool {
    proc().fds.try_lock().is_none_or(|f| f.stdin.is_empty())
}

pub struct FdTable {
    fds: Vec<Option<FileRef>>,
    /// Descriptors closed by execve (O_CLOEXEC / FD_CLOEXEC).
    cloexec: alloc::collections::BTreeSet<usize>,
    pub cwd: String,
    /// Typed-ahead terminal input not yet consumed by read().
    stdin: Vec<u8>,
}

impl FdTable {
    pub fn new() -> Self {
        let tty = || Some(Arc::new(SpinLock::new(OpenFile { kind: Kind::Tty, pos: 0, flags: 2, at: None })));
        Self { fds: vec![tty(), tty(), tty()], cloexec: Default::default(), cwd: String::new(), stdin: Vec::new() }
    }

    /// fork: the child shares every open file (offsets included), as on Linux.
    pub fn fork(&self) -> Self {
        Self { fds: self.fds.clone(), cloexec: self.cloexec.clone(), cwd: self.cwd.clone(), stdin: Vec::new() }
    }

    /// execve: close the descriptors marked close-on-exec.
    pub fn close_on_exec(&mut self) {
        for fd in core::mem::take(&mut self.cloexec) {
            if let Some(f) = self.fds.get_mut(fd).and_then(Option::take)
                && Arc::strong_count(&f) == 1
            {
                flush(&mut f.lock());
            }
        }
    }

    fn set_cloexec(&mut self, fd: i64, on: bool) {
        if fd < 0 {
            return;
        }
        if on {
            self.cloexec.insert(fd as usize);
        } else {
            self.cloexec.remove(&(fd as usize));
        }
    }

    fn get(&self, fd: i64) -> Result<FileRef, i64> {
        // File descriptors are C ints: the upper half of the register is not part of them.
        let fd = fd as i32 as i64;
        if fd < 0 {
            return Err(EBADF);
        }
        self.fds.get(fd as usize).and_then(|f| f.clone()).ok_or(EBADF)
    }

    /// Put `f` at descriptor `n` (program start-up: inherited pipes etc.).
    pub fn install(&mut self, n: usize, f: FileRef) {
        if n >= self.fds.len() {
            self.fds.resize(n + 1, None);
        }
        self.fds[n] = Some(f);
        self.cloexec.remove(&n);
    }

    fn add(&mut self, f: FileRef, min: usize) -> i64 {
        let slot = (min..).find(|&i| i >= self.fds.len() || self.fds[i].is_none()).unwrap();
        if slot >= self.fds.len() {
            self.fds.resize(slot + 1, None);
        }
        self.fds[slot] = Some(f);
        self.cloexec.remove(&slot);
        slot as i64
    }
}

fn flush(f: &mut OpenFile) {
    if let Kind::File { path, at, data, dirty, .. } = &mut f.kind
        && *dirty
    {
        if let Err(e) = at.mount.fs.write_back(&at.rel, &data[..]) {
            crate::log::warn(&format!("could not write back {path}: error {e}"));
        }
        *dirty = false;
    }
}

/// The last reference to an open file can go away in many ways (close, dup2 over it,
/// execve's close-on-exec, process exit): written data reaches the file system then.
impl Drop for OpenFile {
    fn drop(&mut self) {
        flush(self);
    }
}

impl Drop for FdTable {
    fn drop(&mut self) {
        for f in self.fds.drain(..).flatten() {
            if Arc::strong_count(&f) == 1 {
                flush(&mut f.lock());
            }
        }
    }
}

fn thread() -> &'static mut UserState {
    task::current_user().expect("syscall outside a process")
}

fn proc() -> &'static Process {
    &thread().process
}

fn fds() -> crate::sync::SpinLockGuard<'static, FdTable> {
    proc().fds.lock()
}

fn mem() -> crate::sync::SpinLockGuard<'static, process::Memory> {
    proc().mem.lock()
}

/// Copy user memory into the kernel (after making sure it is mapped).
fn read_user(addr: u64, len: usize) -> Result<Vec<u8>, i64> {
    if !mem().ensure(addr, len as u64, false) {
        return Err(EFAULT);
    }
    let mut v = vec![0u8; len];
    // SAFETY: the range is mapped in the active (this process's) address space.
    unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, v.as_mut_ptr(), len) };
    Ok(v)
}

fn write_user(addr: u64, data: &[u8]) -> Result<(), i64> {
    if !mem().ensure(addr, data.len() as u64, true) {
        return Err(EFAULT);
    }
    // SAFETY: as for read_user; the area is writable.
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
    Ok(())
}

fn write_u64(addr: u64, v: u64) -> Result<(), i64> {
    write_user(addr, &v.to_le_bytes())
}

fn read_u64(addr: u64) -> Result<u64, i64> {
    Ok(u64::from_le_bytes(read_user(addr, 8)?.try_into().unwrap()))
}

fn read_cstr(addr: u64) -> Result<String, i64> {
    let mut out = Vec::new();
    let mut a = addr;
    loop {
        let page_end = (a & !(PAGE - 1)) + PAGE;
        let chunk = read_user(a, (page_end - a) as usize)?;
        if let Some(i) = chunk.iter().position(|&b| b == 0) {
            out.extend_from_slice(&chunk[..i]);
            return String::from_utf8(out).map_err(|_| EINVAL);
        }
        out.extend_from_slice(&chunk);
        if out.len() > 4096 {
            return Err(ERANGE);
        }
        a = page_end;
    }
}

/// Resolve a path to a NovaFS name ("" = the root directory).
/// Canonical path of `path` for this process, symbolic links followed (vfs::walk).
fn resolve(path: &str) -> String {
    let cwd = fds().cwd.clone();
    ns().walk(&cwd, path, true).unwrap_or_else(|_| lexical(&cwd, path))
}

/// Like [`resolve`], but a symbolic link in the last component is not followed (lstat,
/// readlink, unlink, rename, O_NOFOLLOW).
fn resolve_nofollow(path: &str) -> String {
    let cwd = fds().cwd.clone();
    ns().walk(&cwd, path, false).unwrap_or_else(|_| lexical(&cwd, path))
}

/// `.` and `..` applied, no links followed (a path whose walk failed, e.g. ELOOP).
fn lexical(cwd: &str, path: &str) -> String {
    let joined = if path.starts_with('/') { String::from(path) } else { format!("/{cwd}/{path}") };
    let mut parts: Vec<&str> = Vec::new();
    for p in joined.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// The calling process's mount namespace.
fn ns() -> Arc<vfs::Namespace> {
    proc().ns.lock().clone()
}

/// Where an open directory is now, in this process's namespace ("" if unreachable).
fn dir_path(at: &vfs::At) -> String {
    ns().vpath(at).unwrap_or_default()
}

fn is_dir(name: &str) -> bool {
    ns().lstat(name).is_ok_and(|m| m.ty == vfs::Type::Dir)
}

/// struct stat for VFS metadata.
fn stat_meta(m: &vfs::Meta) -> Vec<u8> {
    let mut b = stat_buf(m.ty.mode() | m.perm, m.size, m.mtime, m.ino);
    b[16..24].copy_from_slice(&(m.nlink as u64).to_le_bytes());
    b[28..32].copy_from_slice(&m.uid.to_le_bytes());
    b[32..36].copy_from_slice(&m.gid.to_le_bytes());
    b
}

fn tty_write(data: &[u8]) {
    crate::tty::write_bytes(data);
}

/// Cooked terminal input: a line with echo and Backspace; Ctrl-D = end of input,
/// Ctrl-C ends the program.
fn tty_read(buf_len: usize) -> Vec<u8> {
    let empty = fds().stdin.is_empty();
    if empty {
        let mut line: Vec<u8> = Vec::new();
        loop {
            match crate::input::read_key() {
                Key::Char(b'\n') => {
                    tty_write(b"\n");
                    line.push(b'\n');
                    break;
                }
                Key::Char(0x08) => {
                    if line.pop().is_some() {
                        tty_write(b"\x08 \x08");
                    }
                }
                Key::Char(0x04) => break,
                Key::Char(0x03) => {
                    tty_write(b"^C\n");
                    process::exit(130);
                }
                Key::Char(c) if c == b' ' || c.is_ascii_graphic() => {
                    line.push(c);
                    tty_write(&[c]);
                }
                _ => {}
            }
        }
        fds().stdin = line;
    }
    let mut fds = fds();
    let n = buf_len.min(fds.stdin.len());
    fds.stdin.drain(..n).collect()
}

fn stat_buf(mode: u32, size: u64, mtime: u64, ino: u64) -> Vec<u8> {
    let mut b = vec![0u8; 144];
    b[0..8].copy_from_slice(&1u64.to_le_bytes()); // st_dev
    b[8..16].copy_from_slice(&ino.to_le_bytes());
    b[16..24].copy_from_slice(&1u64.to_le_bytes()); // st_nlink
    b[24..28].copy_from_slice(&mode.to_le_bytes());
    b[48..56].copy_from_slice(&size.to_le_bytes());
    b[56..64].copy_from_slice(&4096u64.to_le_bytes()); // st_blksize
    b[64..72].copy_from_slice(&size.div_ceil(512).to_le_bytes());
    for o in [72, 88, 104] {
        b[o..o + 8].copy_from_slice(&mtime.to_le_bytes());
    }
    b
}

fn ino_of(name: &str) -> u64 {
    crate::storage::crc32(name.as_bytes()) as u64 + 2
}

/// Files Nova provides under /etc when NovaFS has none of its own.
pub const ETC_FILES: &[&str] = &["hostname", "hosts", "resolv.conf", "nsswitch.conf", "passwd", "group", "machine-id"];

pub fn etc_file(name: &str) -> Option<String> {
    Some(match name {
        "etc/hostname" => "nova\n".into(),
        "etc/hosts" => "127.0.0.1 localhost\n::1 localhost\n".into(),
        "etc/resolv.conf" => {
            let dns = crate::net::info().and_then(|i| i.dns.first().copied()).map(|d| format!("{d}")).unwrap_or_else(|| "10.0.2.3".into());
            format!("nameserver {dns}\n")
        }
        "etc/nsswitch.conf" => "hosts: files dns\npasswd: files\ngroup: files\n".into(),
        "etc/passwd" => "root:x:0:0:root:/:/bin/sh\nnova:x:1000:1000:Nova:/:/bin/sh\n".into(),
        "etc/group" => "root:x:0:\nnova:x:1000:\n".into(),
        "etc/machine-id" => "4e6f7661000000000000000000000001\n".into(),
        _ => return None,
    })
}

/// Synthetic /proc and /sys files.
fn proc_file(name: &str) -> Option<String> {
    let name = name.replace("proc/thread-self/", "proc/self/").replace(&format!("proc/{}/", proc().pid), "proc/self/");
    let (used, free) = crate::memory::heap_usage();
    let total_kb = (used + free) / 1024;
    Some(match name.as_str() {
        "proc/self/maps" => mem().maps(),
        "proc/self/status" => format!(
            "Name:\t{}\nState:\tR (running)\nTgid:\t{p}\nPid:\t{p}\nPPid:\t1\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nThreads:\t{}\nVmRSS:\t{} kB\nCpus_allowed:\t1\nCpus_allowed_list:\t0\n",
            proc().name,
            proc().threads.load(core::sync::atomic::Ordering::Relaxed),
            used / 1024,
            p = proc().pid
        ),
        "proc/self/stat" => format!("{} ({}) R 1 {} {} 0 -1 0 0 0 0 0 0 0 0 0 20 0 {} 0 0 0 0\n", proc().pid, proc().name, proc().pid, proc().pid, proc().threads.load(core::sync::atomic::Ordering::Relaxed)),
        "proc/self/statm" => "1000 500 100 10 0 400 0\n".into(),
        "proc/self/cmdline" => format!("{}\0", *proc().exe.lock()),
        "proc/self/comm" => format!("{}\n", proc().name),
        "proc/self/oom_score_adj" => "0\n".into(),
        "proc/self/limits" => "Limit                     Soft Limit           Hard Limit           Units\nMax open files            1024                 4096                 files\n".into(),
        "proc/mounts" | "proc/self/mounts" => ns().mounts(),
        "proc/self/mountinfo" => ns().mountinfo(),
        "proc/self/uid_map" | "proc/self/gid_map" => "         0          0 4294967295\n".into(),
        "proc/self/setgroups" => "allow\n".into(),
        "proc/self/cgroup" => "0::/\n".into(),
        "proc/filesystems" => "nodev\tsysfs\nnodev\ttmpfs\nnodev\tproc\nnodev\tdevpts\nnodev\tmqueue\n\tnovafs\n".into(),
        "proc/sys/kernel/unprivileged_userns_clone" => "1\n".into(),
        "proc/sys/user/max_user_namespaces" => "15000\n".into(),
        "proc/sys/kernel/overflowuid" | "proc/sys/kernel/overflowgid" => "65534\n".into(),
        "proc/sys/fs/nr_open" => "1048576\n".into(),
        "proc/meminfo" => format!("MemTotal:       {total_kb} kB\nMemFree:        {} kB\nMemAvailable:   {} kB\nBuffers:               0 kB\nCached:                0 kB\nSwapTotal:             0 kB\nSwapFree:              0 kB\n", free / 1024, free / 1024),
        "proc/cpuinfo" => format!("processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: {}\ncpu MHz\t\t: 3000.000\ncpu cores\t: 1\nflags\t\t: fpu sse sse2 ssse3 sse4_1 sse4_2 avx avx2\n\n", crate::cpu::brand()),
        "proc/stat" => format!("cpu  {0} 0 0 {0} 0 0 0 0 0 0\ncpu0 {0} 0 0 {0} 0 0 0 0 0 0\nbtime {1}\nprocesses 1\n", pit::ticks(), rtc::unix_time() - pit::ticks() / pit::HZ),
        "proc/uptime" => format!("{}.{:02} 0.00\n", pit::ticks() / pit::HZ, pit::ticks() % pit::HZ),
        "proc/loadavg" => "0.00 0.00 0.00 1/1 1\n".into(),
        "proc/sys/kernel/osrelease" => "6.8.0-nova\n".into(),
        "proc/sys/kernel/ostype" => "Linux\n".into(),
        "proc/sys/kernel/yama/ptrace_scope" => "0\n".into(),
        "proc/sys/kernel/pid_max" => "4194304\n".into(),
        "proc/sys/vm/overcommit_memory" => "1\n".into(),
        "proc/sys/fs/inotify/max_user_watches" => "8192\n".into(),
        "sys/devices/system/cpu/online" | "sys/devices/system/cpu/possible" | "sys/devices/system/cpu/present" => "0\n".into(),
        "sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq" => "3000000\n".into(),
        _ => return None,
    })
}


// ---------------------------------------------------------------- /proc, /dev, /sys

/// The file systems the Linux layer provides: mounted at /proc, /dev and /sys in the
/// initial namespace (and wherever a program mounts proc, sysfs or devtmpfs).
pub fn special_mounts() -> Vec<(&'static str, Arc<dyn vfs::Backend>)> {
    alloc::vec![("proc", Arc::new(ProcFs) as Arc<dyn vfs::Backend>), ("dev", Arc::new(DevFs)), ("sys", Arc::new(SysFs))]
}

fn meta(ty: vfs::Type, perm: u32, size: u64, name: &str) -> vfs::Meta {
    vfs::Meta { ty, perm, size, mtime: 0, ino: ino_of(name), nlink: if ty == vfs::Type::Dir { 2 } else { 1 }, uid: 0, gid: 0 }
}

/// /proc paths of any process ("<pid>/...", "thread-self/...") as "self/...": every
/// process sees its own data (enough for programs that look up their own pid).
fn proc_norm(rel: &str) -> String {
    let (first, rest) = rel.split_once('/').unwrap_or((rel, ""));
    if first == "thread-self" || (!first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) && process::exists(first.parse().unwrap_or(0))) {
        return vfs::join("self", rest);
    }
    rel.into()
}

/// What /proc/self/fd/N points at.
fn fd_link(n: i64) -> Option<String> {
    let file = fds().get(n).ok()?;
    let f = file.lock();
    if let Some(at) = &f.at {
        return Some(format!("/{}", ns().vpath(at).unwrap_or_default()));
    }
    Some(match &f.kind {
        Kind::File { at, .. } | Kind::Dir { at } | Kind::Memfd(_, Some(at)) => format!("/{}", ns().vpath(at).unwrap_or_default()),
        Kind::Tty => "/dev/tty".into(),
        Kind::Null => "/dev/null".into(),
        Kind::Zero => "/dev/zero".into(),
        Kind::Random => "/dev/urandom".into(),
        Kind::Channel(_) => "socket:[1]".into(),
        Kind::EventFd(_) => "anon_inode:[eventfd]".into(),
        Kind::SignalFd(_) => "anon_inode:[signalfd]".into(),
        Kind::Epoll(_) => "anon_inode:[eventpoll]".into(),
        Kind::Memfd(_, None) => "/memfd: (deleted)".into(),
        Kind::Inet(_) => "socket:[2]".into(),
    })
}

const PROC_DIRS: &[&str] = &["", "self", "self/fd", "self/ns", "self/task", "sys", "sys/kernel", "sys/kernel/yama", "sys/vm", "sys/fs", "sys/fs/inotify", "sys/user"];
const PROC_ROOT: &[&str] = &["self", "thread-self", "cpuinfo", "meminfo", "mounts", "stat", "uptime", "loadavg", "filesystems", "sys"];
const PROC_SELF: &[&str] = &["exe", "fd", "ns", "maps", "status", "stat", "statm", "cmdline", "comm", "mountinfo", "mounts", "limits", "oom_score_adj", "uid_map", "gid_map", "setgroups", "cgroup"];
const NAMESPACES: &[&str] = &["cgroup", "ipc", "mnt", "net", "pid", "user", "uts"];

struct ProcFs;

impl vfs::Backend for ProcFs {
    fn kind(&self) -> &'static str {
        "proc"
    }
    fn lstat(&self, p: &str) -> Result<vfs::Meta, i64> {
        let n = proc_norm(p);
        if PROC_DIRS.contains(&n.as_str()) || (p.split('/').next().is_some_and(|f| f.bytes().all(|b| b.is_ascii_digit())) && n == "self") {
            return Ok(meta(vfs::Type::Dir, 0o555, 0, p));
        }
        if let Some(t) = self.readlink(p) {
            return Ok(meta(vfs::Type::Lnk, 0o777, t.len() as u64, p));
        }
        let text = proc_file(&format!("proc/{n}")).ok_or(ENOENT)?;
        let perm = if n.ends_with("_map") || n == "self/setgroups" || n == "self/oom_score_adj" { 0o644 } else { 0o444 };
        Ok(meta(vfs::Type::Reg, perm, text.len() as u64, p))
    }
    fn link_target(&self, p: &str) -> Option<String> {
        // Path walks follow exe, and fd/N when it is a file or directory (mount and
        // openat on /proc/self/fd/N reach it, as on Linux). Other fd/N (pipes, sockets,
        // memfd) and ns/* are magic links that only readlink describes; opening fd/N
        // reopens the object.
        let n = proc_norm(p);
        if n == "self/exe" {
            return Some(format!("/{}", *proc().exe.lock()));
        }
        let fd: i64 = n.strip_prefix("self/fd/")?.parse().ok()?;
        let file = fds().get(fd).ok()?;
        let f = file.lock();
        let at = match (&f.at, &f.kind) {
            (_, Kind::Channel(_) | Kind::Inet(_) | Kind::EventFd(_) | Kind::Epoll(_) | Kind::SignalFd(_) | Kind::Memfd(_, None)) => return None,
            (Some(at), _) => at.clone(),
            (None, Kind::File { at, .. } | Kind::Dir { at } | Kind::Memfd(_, Some(at))) => at.clone(),
            _ => return None,
        };
        drop(f);
        ns().vpath(&at).map(|p| format!("/{p}"))
    }
    fn readlink(&self, p: &str) -> Option<String> {
        let n = proc_norm(p);
        if let Some(fd) = n.strip_prefix("self/fd/") {
            return fd_link(fd.parse().ok()?);
        }
        if let Some(name) = n.strip_prefix("self/ns/") {
            return NAMESPACES.contains(&name).then(|| format!("{name}:[4026531840]"));
        }
        self.link_target(p)
    }
    fn list(&self, p: &str) -> Result<Vec<(String, vfs::Type, u64)>, i64> {
        let n = proc_norm(p);
        let ty = |name: &str| self.lstat(&vfs::join(p, name)).map(|m| m.ty).unwrap_or(vfs::Type::Reg);
        let names: Vec<String> = match n.as_str() {
            "" => PROC_ROOT.iter().map(|s| s.to_string()).chain([proc().pid.to_string()]).collect(),
            "self" => PROC_SELF.iter().map(|s| s.to_string()).collect(),
            "self/fd" => fds().fds.iter().enumerate().filter(|(_, f)| f.is_some()).map(|(i, _)| i.to_string()).collect(),
            "self/ns" => NAMESPACES.iter().map(|s| s.to_string()).collect(),
            "self/task" => alloc::vec![thread().tid.to_string()],
            _ if PROC_DIRS.contains(&n.as_str()) => Vec::new(),
            _ => return Err(ENOTDIR),
        };
        Ok(names.into_iter().map(|name| (name.clone(), ty(&name), ino_of(&vfs::join(p, &name)))).collect())
    }
    fn open(&self, p: &str) -> Result<vfs::Content, i64> {
        proc_file(&format!("proc/{}", proc_norm(p))).map(|t| vfs::Content::Copy(t.into_bytes())).ok_or(ENOENT)
    }
    fn create(&self, _: &str, _: u32) -> Result<(), i64> {
        Err(EACCES)
    }
    fn mkdir(&self, _: &str, _: u32) -> Result<(), i64> {
        Err(EACCES)
    }
    fn rmdir(&self, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn unlink(&self, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn rename(&self, _: &str, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn symlink(&self, _: &str, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn link(&self, _: &str, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn chmod(&self, _: &str, _: u32) -> Result<(), i64> {
        Err(EACCES)
    }
    fn truncate(&self, _: &str, _: u64) -> Result<(), i64> {
        Ok(()) // writing uid_map and friends opens them with O_TRUNC
    }
    fn usage(&self) -> (u64, u64) {
        (0, 0)
    }
}

const DEV_NODES: &[&str] = &["null", "zero", "full", "random", "urandom", "tty", "console", "ptmx"];
const DEV_LINKS: &[(&str, &str)] = &[("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")];

struct DevFs;

impl vfs::Backend for DevFs {
    fn kind(&self) -> &'static str {
        "devtmpfs"
    }
    fn lstat(&self, p: &str) -> Result<vfs::Meta, i64> {
        if matches!(p, "" | "shm" | "pts") {
            return Ok(meta(vfs::Type::Dir, if p == "shm" { 0o1777 } else { 0o755 }, 0, p));
        }
        if DEV_NODES.contains(&p) {
            return Ok(meta(vfs::Type::Chr, 0o666, 0, p));
        }
        if let Some(t) = self.link_target(p) {
            return Ok(meta(vfs::Type::Lnk, 0o777, t.len() as u64, p));
        }
        Err(ENOENT)
    }
    fn link_target(&self, p: &str) -> Option<String> {
        DEV_LINKS.iter().find(|(n, _)| *n == p).map(|(_, t)| t.to_string())
    }
    fn list(&self, p: &str) -> Result<Vec<(String, vfs::Type, u64)>, i64> {
        Ok(match p {
            "" => DEV_NODES
                .iter()
                .map(|n| (n.to_string(), vfs::Type::Chr, ino_of(n)))
                .chain(DEV_LINKS.iter().map(|(n, _)| (n.to_string(), vfs::Type::Lnk, ino_of(n))))
                .chain(["shm", "pts"].iter().map(|n| (n.to_string(), vfs::Type::Dir, ino_of(n))))
                .collect(),
            "shm" => SHM.lock().keys().map(|n| (n.clone(), vfs::Type::Reg, ino_of(n))).collect(),
            "pts" => Vec::new(),
            _ => return Err(ENOTDIR),
        })
    }
    fn open(&self, p: &str) -> Result<vfs::Content, i64> {
        DEV_NODES.iter().find(|n| **n == p).map(|n| vfs::Content::Device(n)).ok_or(ENOENT)
    }
    fn create(&self, _: &str, _: u32) -> Result<(), i64> {
        Err(EACCES)
    }
    fn mkdir(&self, _: &str, _: u32) -> Result<(), i64> {
        Err(EACCES)
    }
    fn rmdir(&self, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn unlink(&self, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn rename(&self, _: &str, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn symlink(&self, _: &str, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn link(&self, _: &str, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn chmod(&self, _: &str, _: u32) -> Result<(), i64> {
        Ok(())
    }
    fn truncate(&self, _: &str, _: u64) -> Result<(), i64> {
        Ok(())
    }
    fn usage(&self) -> (u64, u64) {
        (0, 0)
    }
}

const SYS_FILES: &[&str] = &["devices/system/cpu/online", "devices/system/cpu/possible", "devices/system/cpu/present", "devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq"];

struct SysFs;

impl vfs::Backend for SysFs {
    fn kind(&self) -> &'static str {
        "sysfs"
    }
    fn lstat(&self, p: &str) -> Result<vfs::Meta, i64> {
        if let Some(t) = proc_file(&format!("sys/{p}")) {
            return Ok(meta(vfs::Type::Reg, 0o444, t.len() as u64, p));
        }
        if p.is_empty() || SYS_FILES.iter().any(|f| f.starts_with(p) && f.as_bytes().get(p.len()) == Some(&b'/')) {
            return Ok(meta(vfs::Type::Dir, 0o555, 0, p));
        }
        Err(ENOENT)
    }
    fn link_target(&self, _: &str) -> Option<String> {
        None
    }
    fn list(&self, p: &str) -> Result<Vec<(String, vfs::Type, u64)>, i64> {
        let prefix = if p.is_empty() { String::new() } else { format!("{p}/") };
        let mut names = BTreeMap::new();
        for f in SYS_FILES.iter().filter_map(|f| f.strip_prefix(prefix.as_str())) {
            match f.split_once('/') {
                Some((d, _)) => names.insert(d.to_string(), vfs::Type::Dir),
                None => names.insert(f.to_string(), vfs::Type::Reg),
            };
        }
        Ok(names.into_iter().map(|(n, t)| (n.clone(), t, ino_of(&n))).collect())
    }
    fn open(&self, p: &str) -> Result<vfs::Content, i64> {
        proc_file(&format!("sys/{p}")).map(|t| vfs::Content::Copy(t.into_bytes())).ok_or(ENOENT)
    }
    fn create(&self, _: &str, _: u32) -> Result<(), i64> {
        Err(EACCES)
    }
    fn mkdir(&self, _: &str, _: u32) -> Result<(), i64> {
        Err(EACCES)
    }
    fn rmdir(&self, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn unlink(&self, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn rename(&self, _: &str, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn symlink(&self, _: &str, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn link(&self, _: &str, _: &str) -> Result<(), i64> {
        Err(EACCES)
    }
    fn chmod(&self, _: &str, _: u32) -> Result<(), i64> {
        Err(EACCES)
    }
    fn truncate(&self, _: &str, _: u64) -> Result<(), i64> {
        Err(EACCES)
    }
    fn usage(&self) -> (u64, u64) {
        (0, 0)
    }
}

/// A path relative to directory descriptor `dirfd` (AT_FDCWD, or a descriptor that is not
/// a directory Nova tracks by path: the working directory, as openat does).
fn at_path(dirfd: i64, p: &str) -> Result<String, i64> {
    if dirfd != AT_FDCWD && !p.starts_with('/') {
        if let Kind::Dir { at } = &fds().get(dirfd)?.lock().kind {
            return Ok(resolve(&format!("/{}/{p}", dir_path(at))));
        }
    }
    Ok(resolve(p))
}

/// [`at_path`], following a symbolic link in the last component only if `follow`.
fn at_path_nf(dirfd: i64, p: &str, follow: bool) -> Result<String, i64> {
    let p = if dirfd != AT_FDCWD && !p.starts_with('/') {
        match &fds().get(dirfd)?.lock().kind {
            Kind::Dir { at } => format!("/{}/{p}", dir_path(at)),
            _ => String::from(p),
        }
    } else {
        String::from(p)
    };
    Ok(if follow { resolve(&p) } else { resolve_nofollow(&p) })
}

/// The file-system path an open descriptor refers to (None: not a file).
fn fd_path(fd: i64) -> Result<Option<String>, i64> {
    let file = fds().get(fd)?;
    let f = file.lock();
    if let Some(at) = &f.at {
        return Ok(ns().vpath(at));
    }
    Ok(match &f.kind {
        Kind::File { at, .. } | Kind::Dir { at } | Kind::Memfd(_, Some(at)) => ns().vpath(at),
        _ => None,
    })
}

/// struct statx (256 bytes) from an x86-64 struct stat.
fn statx(s: &[u8]) -> Vec<u8> {
    let q = |o: usize| u64::from_le_bytes(s[o..o + 8].try_into().unwrap());
    let d = |o: usize| u32::from_le_bytes(s[o..o + 4].try_into().unwrap());
    let mut b = vec![0u8; 256];
    let mut put = |o: usize, v: &[u8]| b[o..o + v.len()].copy_from_slice(v);
    put(0, &0x7FFu32.to_le_bytes()); // STATX_BASIC_STATS
    put(4, &(q(56) as u32).to_le_bytes()); // blksize
    put(16, &(q(16) as u32).to_le_bytes()); // nlink
    put(20, &d(28).to_le_bytes()); // uid
    put(24, &d(32).to_le_bytes()); // gid
    put(28, &(d(24) as u16).to_le_bytes()); // mode
    put(32, &q(8).to_le_bytes()); // ino
    put(40, &q(48).to_le_bytes()); // size
    put(48, &q(64).to_le_bytes()); // blocks
    for (dst, src) in [(64, 72), (96, 104), (112, 88)] {
        // atime, ctime, mtime: tv_sec (s64), tv_nsec (u32)
        put(dst, &q(src).to_le_bytes());
        put(dst + 8, &(q(src + 8) as u32).to_le_bytes());
    }
    put(80, &q(88).to_le_bytes()); // btime = mtime
    put(88, &(q(96) as u32).to_le_bytes());
    b
}

fn stat_path(name: &str) -> Result<Vec<u8>, i64> {
    if let Some(n) = name.strip_prefix("dev/shm/") {
        let size = SHM.lock().get(n).ok_or(ENOENT)?.lock().size;
        return Ok(stat_buf(0o100600, size, 0, ino_of(name)));
    }
    ns().lstat(name).map(|m| stat_meta(&m))
}

fn stat_file(f: &OpenFile) -> Vec<u8> {
    match &f.kind {
        Kind::Tty => stat_buf(0o20620, 0, 0, 1),
        Kind::Null | Kind::Random | Kind::Zero => stat_buf(0o20666, 0, 0, 1),
        Kind::File { path, data, at, .. } => match at.mount.fs.lstat(&at.rel) {
            Ok(meta) => stat_meta(&vfs::Meta { size: data.len() as u64, ..meta }),
            Err(_) => stat_buf(0o100644, data.len() as u64, rtc::unix_time(), ino_of(path)),
        },
        Kind::Dir { at } => at.mount.fs.lstat(&at.rel).map(|m| stat_meta(&m)).unwrap_or_else(|_| stat_buf(0o40755, 4096, 0, 2)),
        Kind::Channel(e) => stat_buf(if e.rx.is_some() && e.tx.is_some() { 0o140777 } else { 0o10600 }, 0, 0, 3),
        Kind::EventFd(_) | Kind::Epoll(_) | Kind::SignalFd(_) => stat_buf(0o600, 0, 0, 4),
        Kind::Memfd(m, Some(at)) => match at.mount.fs.lstat(&at.rel) {
            Ok(meta) => stat_meta(&vfs::Meta { size: m.lock().size, ..meta }),
            Err(_) => stat_buf(0o100644, m.lock().size, 0, 7),
        },
        Kind::Memfd(m, None) => stat_buf(0o100600, m.lock().size, 0, 5),
        Kind::Inet(_) => stat_buf(0o140777, 0, 0, 6),
    }
}

fn open(path: &str, flags: u64, mode: u32) -> Result<FileRef, i64> {
    let acc = flags & 3;
    let (creat, excl, trunc, append, directory) = (flags & 0o100 != 0, flags & 0o200 != 0, flags & 0o1000 != 0, flags & 0o2000 != 0, flags & 0o200000 != 0);
    let name = if flags & O_NOFOLLOW != 0 || (creat && excl) { resolve_nofollow(path) } else { resolve(path) };
    // POSIX shared memory (/dev/shm/NAME): named shared-memory objects.
    if let Some(shm_name) = name.strip_prefix("dev/shm/") {
        let mut reg = SHM.lock();
        let obj = match reg.get(shm_name) {
            Some(_) if creat && excl => return Err(EEXIST),
            Some(o) => o.clone(),
            None if creat => {
                let o = ipc::memfd();
                reg.insert(shm_name.into(), o.clone());
                o
            }
            None => return Err(ENOENT),
        };
        if trunc {
            obj.lock().truncate(0);
        }
        return Ok(new_file(Kind::Memfd(obj, None), flags));
    }
    let ns = ns();
    let (m, rel) = ns.find_mount(&name);
    // /proc/self/fd/N: a new open file description of the same object, with the
    // access mode asked for now (Chromium makes read-only handles this way).
    if m.fs.kind() == "proc"
        && let Some(n) = proc_norm(&rel).strip_prefix("self/fd/").and_then(|n| n.parse::<i64>().ok())
    {
        let file = fds().get(n)?;
        let f = file.lock();
        return Ok(match &f.kind {
            Kind::Memfd(m, p) => new_file(Kind::Memfd(m.clone(), p.clone()), flags),
            Kind::File { path, data: FileData::Static(d), at, .. } => new_file(Kind::File { path: path.clone(), at: at.clone(), data: FileData::Static(d), writable: false, append: false, dirty: false }, flags),
            _ => {
                drop(f);
                file
            }
        });
    }
    let kind = {
        {
            let meta = match m.fs.lstat(&rel) {
                Ok(_) if creat && excl => return Err(EEXIST),
                Ok(meta) => meta,
                Err(ENOENT) if creat => {
                    if m.readonly {
                        return Err(EROFS);
                    }
                    m.fs.create(&rel, mode & !0o022)?;
                    m.fs.lstat(&rel)?
                }
                Err(e) => return Err(e),
            };
            match meta.ty {
                vfs::Type::Dir => {
                    if acc != 0 {
                        return Err(EISDIR);
                    }
                    Kind::Dir { at: vfs::At { mount: m.clone(), rel: rel.clone() } }
                }
                vfs::Type::Lnk => return Err(ELOOP), // O_NOFOLLOW on a link
                vfs::Type::Chr => match m.fs.open(&rel)? {
                    vfs::Content::Device("null") => Kind::Null,
                    vfs::Content::Device("zero" | "full") => Kind::Zero,
                    vfs::Content::Device("random" | "urandom") => Kind::Random,
                    _ => Kind::Tty,
                },
                vfs::Type::Reg if directory => return Err(ENOTDIR),
                vfs::Type::Reg => {
                    if acc != 0 && m.readonly {
                        return Err(EROFS);
                    }
                    let at = vfs::At { mount: m.clone(), rel: rel.clone() };
                    match m.fs.open(&rel)? {
                        vfs::Content::Static(s) => {
                            if acc != 0 || trunc {
                                return Err(EROFS);
                            }
                            Kind::File { path: name, at, data: FileData::Static(s), writable: false, append: false, dirty: false }
                        }
                        vfs::Content::Device(_) => return Err(EINVAL),
                        vfs::Content::Copy(data) => Kind::File {
                            path: name,
                            at,
                            data: FileData::Owned(if trunc && acc != 0 { Vec::new() } else { data }),
                            writable: acc != 0,
                            append,
                            dirty: trunc && acc != 0,
                        },
                        vfs::Content::Shared(obj) => {
                            if trunc && acc != 0 {
                                obj.lock().truncate(0);
                            }
                            Kind::Memfd(obj, Some(at))
                        }
                    }
                }
            }
        }
    };
    let mut f = OpenFile { kind, pos: 0, flags, at: Some(vfs::At { mount: m.clone(), rel: rel.clone() }) };
    if let Kind::File { dirty: true, .. } = f.kind {
        flush(&mut f); // create the file now, so it exists for stat/other opens
    }
    Ok(Arc::new(SpinLock::new(f)))
}

/// Read from a pipe/socket/eventfd, blocking unless O_NONBLOCK. Returns data and
/// any file descriptors that came with it.
fn channel_read(file: &FileRef, len: usize, peek_message: bool, dontwait: bool) -> Result<(Vec<u8>, Vec<FileRef>, bool), i64> {
    loop {
        {
            let f = file.lock();
            let nonblock = f.flags & O_NONBLOCK != 0 || dontwait;
            match &f.kind {
                Kind::Channel(end) => {
                    let rx = end.rx.as_ref().ok_or(EBADF)?;
                    let mut c = rx.lock();
                    if c.readable() {
                        let r = if end.kind == ipc::SockType::Stream && !peek_message {
                            let (d, fds) = c.read_stream(len);
                            (d, fds, false)
                        } else {
                            c.read_message(len).unwrap()
                        };
                        drop(c);
                        ipc::notify();
                        return Ok(r);
                    }
                    if c.writer_closed {
                        return Ok((Vec::new(), Vec::new(), false));
                    }
                }
                Kind::EventFd(e) => {
                    if len < 8 {
                        return Err(EINVAL);
                    }
                    if let Some(v) = e.lock().take() {
                        ipc::notify();
                        return Ok((v.to_le_bytes().to_vec(), Vec::new(), false));
                    }
                }
                _ => return Err(EINVAL),
            }
            if nonblock {
                return Err(EAGAIN);
            }
        }
        ipc::wait(None);
        process::check_exiting();
    }
}

fn channel_write(file: &FileRef, data: &[u8], fds: Vec<FileRef>, dontwait: bool) -> Result<usize, i64> {
    let mut fds = Some(fds);
    loop {
        {
            let f = file.lock();
            let nonblock = f.flags & O_NONBLOCK != 0 || dontwait;
            match &f.kind {
                Kind::Channel(end) => {
                    let tx = end.tx.as_ref().ok_or(EBADF)?;
                    let mut c = tx.lock();
                    if c.reader_closed {
                        return Err(EPIPE);
                    }
                    if c.writable() {
                        c.push(ipc::Message { data: data.to_vec(), fds: fds.take().unwrap_or_default() });
                        drop(c);
                        ipc::notify();
                        return Ok(data.len());
                    }
                }
                Kind::EventFd(e) => {
                    if data.len() < 8 {
                        return Err(EINVAL);
                    }
                    let v = u64::from_le_bytes(data[..8].try_into().unwrap());
                    e.lock().add(v);
                    ipc::notify();
                    return Ok(8);
                }
                _ => return Err(EINVAL),
            }
            if nonblock {
                return Err(EAGAIN);
            }
        }
        ipc::wait(None);
        process::check_exiting();
    }
}

/// read() on a signalfd: one 128-byte signalfd_siginfo per pending signal in `mask`.
fn signalfd_read(f: &FileRef, mask: u64, len: usize) -> Result<Vec<u8>, i64> {
    if len < 128 {
        return Err(EINVAL);
    }
    loop {
        let mut out = Vec::new();
        while out.len() + 128 <= len {
            let Some((sig, sender)) = signal::take_masked(thread(), mask) else { break };
            let mut info = [0u8; 128];
            info[0..4].copy_from_slice(&sig.to_le_bytes()); // ssi_signo
            info[8..12].copy_from_slice(&(if sig == signal::SIGCHLD { 1i32 } else { 0 }).to_le_bytes()); // ssi_code: CLD_EXITED / SI_USER
            info[12..16].copy_from_slice(&sender.to_le_bytes()); // ssi_pid
            out.extend_from_slice(&info);
        }
        if !out.is_empty() {
            return Ok(out);
        }
        if f.lock().flags & O_NONBLOCK != 0 {
            return Err(EAGAIN);
        }
        ipc::wait(None);
        process::check_exiting();
    }
}

fn is_channel(f: &FileRef) -> bool {
    matches!(f.lock().kind, Kind::Channel(_) | Kind::EventFd(_))
}

fn do_read(f: &FileRef, len: usize, at: Option<usize>) -> Result<Vec<u8>, i64> {
    let is_tty = matches!(f.lock().kind, Kind::Tty);
    if is_tty {
        return Ok(tty_read(len));
    }
    let signal_mask = match f.lock().kind {
        Kind::SignalFd(m) => Some(m),
        _ => None,
    };
    if let Some(mask) = signal_mask {
        return signalfd_read(f, mask, len);
    }
    if is_channel(f) {
        let (data, fds, _) = channel_read(f, len, false, false)?;
        drop(fds); // plain read() discards passed descriptors
        return Ok(data);
    }
    if is_inet(f) {
        return inet_recv(f, len, false, false).map(|(d, _)| d);
    }
    let mut f = f.lock();
    let pos = at.unwrap_or(f.pos);
    let out = match &f.kind {
        Kind::Null => Vec::new(),
        Kind::Zero => vec![0; len],
        Kind::Random => {
            let mut v = vec![0; len];
            rand::fill(&mut v);
            v
        }
        Kind::File { data, .. } => data.get(pos..).map(|d| d[..len.min(d.len())].to_vec()).unwrap_or_default(),
        Kind::Dir { .. } => return Err(EISDIR),
        Kind::Memfd(m, _) => m.lock().read(pos, len),
        Kind::Epoll(_) | Kind::Inet(_) => return Err(EINVAL),
        Kind::Tty | Kind::Channel(_) | Kind::EventFd(_) | Kind::SignalFd(_) => unreachable!(),
    };
    if at.is_none() {
        f.pos = pos + out.len();
    }
    Ok(out)
}

fn do_write(f: &FileRef, data: &[u8], at: Option<usize>) -> Result<usize, i64> {
    if is_channel(f) {
        return channel_write(f, data, Vec::new(), false);
    }
    if is_inet(f) {
        return inet_send(f, data, None, false);
    }
    let mut f = f.lock();
    let pos = at.unwrap_or(f.pos);
    let flags = f.flags;
    let written = match &mut f.kind {
        Kind::Tty => {
            drop(f);
            tty_write(data);
            return Ok(data.len());
        }
        Kind::Null | Kind::Random | Kind::Zero => data.len(),
        Kind::Dir { .. } => return Err(EISDIR),
        Kind::Memfd(m, path) => {
            if path.is_some() && flags & 3 == 0 {
                return Err(EBADF);
            }
            let mut m = m.lock();
            let start = if flags & 0o2000 != 0 { m.size as usize } else { pos };
            m.write(start, data);
            drop(m);
            if at.is_none() {
                f.pos = start + data.len();
            }
            return Ok(data.len());
        }
        Kind::Epoll(_) | Kind::Channel(_) | Kind::EventFd(_) | Kind::Inet(_) | Kind::SignalFd(_) => return Err(EINVAL),
        Kind::File { data: contents, writable, append, dirty, .. } => {
            if !*writable {
                return Err(EBADF);
            }
            let start = if *append { contents.len() } else { pos };
            let contents = contents.to_mut();
            if contents.len() < start + data.len() {
                contents.resize(start + data.len(), 0);
            }
            contents[start..start + data.len()].copy_from_slice(data);
            *dirty = true;
            if at.is_none() {
                f.pos = start + data.len();
            }
            return Ok(data.len());
        }
    };
    if at.is_none() {
        f.pos = pos + written;
    }
    Ok(written)
}

/// Directory entries (linux_dirent64) of `dir`: its immediate children, including mount
/// points.
fn dirents(at: &vfs::At) -> Vec<Vec<u8>> {
    let ns = ns();
    let dir = ns.vpath(at).unwrap_or_default();
    let dir = dir.as_str();
    let mut names: BTreeMap<String, (u8, u64)> = at.mount.fs.list(&at.rel).unwrap_or_default().into_iter().map(|(n, t, ino)| (n, (t.dtype(), ino))).collect();
    for n in ns.mounts_in(dir) {
        names.insert(n.clone(), (4, ino_of(&vfs::join(dir, &n))));
    }
    let mut out = Vec::new();
    let dots = [(".".to_string(), (4, ino_of(dir))), ("..".to_string(), (4, ino_of(vfs::split(dir).0)))];
    for (i, (name, (dtype, ino))) in dots.into_iter().chain(names).enumerate() {
        let reclen = (19 + name.len() + 1).next_multiple_of(8);
        let mut e = vec![0u8; reclen];
        e[0..8].copy_from_slice(&ino.to_le_bytes());
        e[8..16].copy_from_slice(&((i + 1) as u64).to_le_bytes());
        e[16..18].copy_from_slice(&(reclen as u16).to_le_bytes());
        e[18] = dtype;
        e[19..19 + name.len()].copy_from_slice(name.as_bytes());
        out.push(e);
    }
    out
}

fn timespec(ns: u64) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.extend_from_slice(&(ns / 1_000_000_000).to_le_bytes());
    b.extend_from_slice(&(ns % 1_000_000_000).to_le_bytes());
    b
}

fn now_ns(clock: u64) -> u64 {
    let mono = pit::nanos();
    match clock {
        0 | 5 | 8 | 11 => rtc::unix_nanos(), // realtime
        _ => mono,                                                                  // monotonic, boottime, cputime
    }
}

/// Sleep at least `ns` (POSIX: never less than requested), in timer ticks, checking the
/// TSC clock at each one: counting whole ticks alone would end up to one tick early.
fn sleep_ns(ns: u64) {
    let end = pit::nanos().saturating_add(ns);
    while pit::nanos() < end {
        let left = end - pit::nanos();
        task::sleep_ticks(left.div_ceil(1_000_000_000 / pit::HZ).max(1));
    }
}

fn uname() -> Vec<u8> {
    let mut b = vec![0u8; 65 * 6];
    for (i, s) in ["Linux", "nova", "6.8.0-nova", "#1 SMP Nova 0.4", "x86_64", "(none)"].iter().enumerate() {
        b[i * 65..i * 65 + s.len()].copy_from_slice(s.as_bytes());
    }
    b
}

static REPORTED: SpinLock<BTreeSet<u64>> = SpinLock::new(BTreeSet::new());

fn unimplemented(nr: u64) -> i64 {
    if let Some(mut r) = REPORTED.try_lock()
        && r.insert(nr)
    {
        crate::log::warn(&format!("{}: unimplemented Linux syscall {nr}", proc().name));
    }
    -ENOSYS
}

fn res(r: Result<i64, i64>) -> i64 {
    r.unwrap_or_else(|e| -e)
}

/// Handle one system call. Returns the value for rax.
pub fn syscall(f: &mut SyscallFrame) -> i64 {
    let (nr, a0, a1, a2, a3, a4, a5) = (f.rax, f.rdi, f.rsi, f.rdx, f.r10, f.r8, f.r9);
    let trace = proc().trace.load(core::sync::atomic::Ordering::Relaxed);
    let tid = thread().tid;
    let r = dispatch(f, nr, a0, a1, a2, a3, a4, a5);
    if trace {
        let path = if matches!(nr, 2 | 4 | 6 | 21 | 59 | 80 | 83 | 87 | 89) {
            read_cstr(a0).map(|p| format!(" \"{p}\"")).unwrap_or_default()
        } else if matches!(nr, 257 | 262 | 263 | 267 | 269 | 439) || (nr == 157 && a0 == 15) {
            read_cstr(a1).map(|p| format!(" \"{p}\"")).unwrap_or_default()
        } else if matches!(nr, 0 | 1 | 46 | 47 | 233) {
            // Which kind of file the descriptor is.
            let fd = if nr == 233 { a2 } else { a0 };
            fds().get(fd as i64).map(|f| format!(" <{}>", kind_name(&f.lock().kind))).unwrap_or_default()
        } else {
            String::new()
        };
        crate::log::info(&format!("[{tid}] {}({a0:#x}, {a1:#x}, {a2:#x}, {a3:#x}, {a4:#x}, {a5:#x}){path} = {r}", syscall_name(nr)));
    }
    r
}

fn kind_name(k: &Kind) -> String {
    match k {
        Kind::Tty => "tty".into(),
        Kind::Null => "null".into(),
        Kind::Random => "random".into(),
        Kind::File { path, .. } => format!("file {path}"),
        Kind::Dir { at } => format!("dir {}", at.rel),
        Kind::Zero => "zero".into(),
        Kind::Channel(e) => format!("{}{:?}", if e.rx.is_some() && e.tx.is_some() { "socket " } else { "pipe " }, e.kind),
        Kind::EventFd(_) => "eventfd".into(),
        Kind::SignalFd(_) => "signalfd".into(),
        Kind::Epoll(_) => "epoll".into(),
        Kind::Memfd(_, Some(at)) => format!("file {}", at.rel),
        Kind::Memfd(_, None) => "memfd".into(),
        Kind::Inet(s) => format!("inet {}", if s.udp { "udp" } else { "tcp" }),
    }
}

fn syscall_name(nr: u64) -> String {
    let n = match nr {
        0 => "read", 1 => "write", 2 => "open", 3 => "close", 4 => "stat", 5 => "fstat", 6 => "lstat", 7 => "poll",
        8 => "lseek", 9 => "mmap", 10 => "mprotect", 11 => "munmap", 12 => "brk", 13 => "rt_sigaction",
        14 => "rt_sigprocmask", 16 => "ioctl", 17 => "pread64", 19 => "readv", 20 => "writev", 21 => "access",
        22 => "pipe", 23 => "select", 24 => "sched_yield", 25 => "mremap", 28 => "madvise", 32 => "dup", 33 => "dup2",
 39 => "getpid", 40 => "sendfile", 57 => "fork", 58 => "vfork", 59 => "execve", 61 => "wait4", 62 => "kill", 77 => "ftruncate", 87 => "unlink", 84 => "rmdir", 88 => "symlink", 266 => "symlinkat", 86 => "link", 265 => "linkat", 280 => "utimensat", 76 => "truncate", 90 => "chmod", 285 => "fallocate", 41 => "socket", 42 => "connect", 44 => "sendto", 45 => "recvfrom", 46 => "sendmsg", 47 => "recvmsg",
        42 => "connect", 49 => "bind", 53 => "socketpair", 307 => "sendmmsg", 56 => "clone", 60 => "exit", 63 => "uname", 72 => "fcntl", 79 => "getcwd",
        89 => "readlink", 96 => "gettimeofday", 97 => "getrlimit", 99 => "sysinfo", 137 => "statfs", 138 => "fstatfs",
        157 => "prctl", 158 => "arch_prctl", 186 => "gettid", 202 => "futex", 204 => "sched_getaffinity",
        213 => "epoll_create", 217 => "getdents64", 218 => "set_tid_address", 228 => "clock_gettime",
        231 => "exit_group", 232 => "epoll_wait", 233 => "epoll_ctl", 257 => "openat", 262 => "newfstatat",
        273 => "set_robust_list", 332 => "statx", 281 => "epoll_pwait", 284 => "eventfd", 290 => "eventfd2", 291 => "epoll_create1",
        293 => "pipe2", 302 => "prlimit64", 318 => "getrandom", 319 => "memfd_create", 334 => "rseq", 435 => "clone3",
        _ => return format!("syscall_{nr}"),
    };
    n.into()
}

#[allow(clippy::too_many_arguments)]
fn dispatch(f: &mut SyscallFrame, nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    res((|| -> Result<i64, i64> {
        Ok(match nr {
            0 => {
                // read
                let file = fds().get(a0 as i64)?;
                let data = do_read(&file, a2 as usize, None)?;
                write_user(a1, &data)?;
                data.len() as i64
            }
            1 => {
                // write
                let file = fds().get(a0 as i64)?;
                let data = read_user(a1, a2 as usize)?;
                do_write(&file, &data, None)? as i64
            }
            2 | 257 => {
                // open, openat
                let (dirfd, path, flags) = if nr == 2 { (AT_FDCWD, a0, a1) } else { (a0 as i32 as i64, a1, a2) };
                let mut p = read_cstr(path)?;
                if dirfd != AT_FDCWD && !p.starts_with('/') {
                    if let Kind::Dir { at } = &fds().get(dirfd)?.lock().kind {
                        p = format!("/{}/{p}", dir_path(at));
                    }
                }
                let file = open(&p, flags, (if nr == 2 { a2 } else { a3 }) as u32)?;
                let mut t = fds();
                let fd = t.add(file, 0);
                t.set_cloexec(fd, flags & O_CLOEXEC != 0);
                fd
            }
            3 => {
                // close
                let file = {
                    let mut fds = fds();
                    fds.cloexec.remove(&(a0 as usize));
                    let slot = fds.fds.get_mut(a0 as usize).ok_or(EBADF)?;
                    slot.take().ok_or(EBADF)?
                };
                if Arc::strong_count(&file) == 1 {
                    flush(&mut file.lock());
                }
                0
            }
            4 | 6 => {
                // stat, lstat
                let p = read_cstr(a0)?;
                let b = stat_path(&if nr == 6 { resolve_nofollow(&p) } else { resolve(&p) })?;
                write_user(a1, &b)?;
                0
            }
            5 => {
                // fstat
                let b = stat_file(&fds().get(a0 as i64)?.lock());
                write_user(a1, &b)?;
                0
            }
            262 => {
                // newfstatat
                let p = read_cstr(a1)?;
                let b = if p.is_empty() && a3 & 0x1000 != 0 { stat_file(&fds().get(a0 as i64)?.lock()) } else { stat_path(&at_path_nf(a0 as i32 as i64, &p, a3 & 0x100 == 0)?)? };
                write_user(a2, &b)?;
                0
            }
            332 => {
                // statx(dirfd, path, flags, mask, statxbuf): filled from the same data as stat
                let p = read_cstr(a1)?;
                let s = if p.is_empty() && a2 & 0x1000 != 0 { stat_file(&fds().get(a0 as i64)?.lock()) } else { stat_path(&at_path_nf(a0 as i32 as i64, &p, a2 & 0x100 == 0)?)? };
                write_user(a4, &statx(&s))?;
                0
            }
            7 | 271 => {
                // poll(fds, nfds, timeout_ms) / ppoll(fds, nfds, timespec*, sigmask)
                let deadline = if nr == 7 {
                    if (a2 as i64) < 0 { None } else { Some(pit::ticks() + (a2 as u64).div_ceil(1000 / pit::HZ)) }
                } else {
                    deadline(a2, false, 1)?
                };
                poll_fds(a0, a1 as usize, deadline)?
            }
            22 | 293 => {
                // pipe, pipe2
                let flags = if nr == 293 { a1 } else { 0 };
                let (r, w) = ipc::pipe();
                let mut t = fds();
                let rfd = t.add(new_file(Kind::Channel(r), flags & O_NONBLOCK), 0);
                let wfd = t.add(new_file(Kind::Channel(w), (flags & O_NONBLOCK) | 1), 0);
                t.set_cloexec(rfd, flags & O_CLOEXEC != 0);
                t.set_cloexec(wfd, flags & O_CLOEXEC != 0);
                drop(t);
                let mut b = Vec::new();
                b.extend_from_slice(&(rfd as i32).to_le_bytes());
                b.extend_from_slice(&(wfd as i32).to_le_bytes());
                write_user(a0, &b)?;
                0
            }
            53 => {
                // socketpair(domain, type, protocol, sv)
                if a0 != 1 {
                    return Err(EAFNOSUPPORT);
                }
                let kind = sock_type(a1)?;
                let (x, y) = ipc::socketpair(kind);
                let flags = if a1 & 0o4000 != 0 { O_NONBLOCK } else { 0 } | 2;
                let mut t = fds();
                let fa = t.add(new_file(Kind::Channel(x), flags), 0);
                let fb = t.add(new_file(Kind::Channel(y), flags), 0);
                t.set_cloexec(fa, a1 & O_CLOEXEC != 0);
                t.set_cloexec(fb, a1 & O_CLOEXEC != 0);
                drop(t);
                let mut b = Vec::new();
                b.extend_from_slice(&(fa as i32).to_le_bytes());
                b.extend_from_slice(&(fb as i32).to_le_bytes());
                write_user(a3, &b)?;
                0
            }
            41 => {
                // socket(domain, type, protocol): AF_INET TCP and UDP
                if a0 != 2 {
                    return Err(EAFNOSUPPORT);
                }
                let udp = match a1 & 0xF {
                    1 => false,
                    2 => true,
                    _ => return Err(EINVAL),
                };
                let handle = if udp { crate::net::udp_open() } else { crate::net::tcp_open() }.map_err(sock_err)?;
                let flags = (a1 & O_NONBLOCK) | 2;
                let mut t = fds();
                let fd = t.add(new_file(Kind::Inet(InetSocket { handle, udp, local_port: 0, peer: None, error: 0 }), flags), 0);
                t.set_cloexec(fd, a1 & O_CLOEXEC != 0);
                fd
            }
            42 => {
                // connect(fd, addr, len)
                let file = fds().get(a0 as i64)?;
                let (ip, port) = sockaddr_in(&read_user(a1, (a2 as usize).min(128))?)?;
                let nonblock = {
                    let mut f = file.lock();
                    let nonblock = f.flags & O_NONBLOCK != 0;
                    let Kind::Inet(s) = &mut f.kind else { return Err(ENOTSOCK) };
                    s.peer = Some((ip, port));
                    if s.udp {
                        if s.local_port == 0 {
                            s.local_port = crate::net::udp_bind(s.handle, 0).map_err(sock_err)?;
                        }
                        return Ok(0);
                    }
                    s.local_port = crate::net::tcp_connect(s.handle, ip, port).map_err(sock_err)?;
                    nonblock
                };
                if nonblock {
                    return Err(EINPROGRESS);
                }
                loop {
                    let h = match &file.lock().kind {
                        Kind::Inet(s) => s.handle,
                        _ => return Err(EINVAL),
                    };
                    match crate::net::tcp_connected(h) {
                        Some(true) => return Ok(0),
                        Some(false) => return Err(ECONNREFUSED),
                        None => {}
                    }
                    ipc::wait(None);
                    process::check_exiting();
                }
            }
            44 => {
                // sendto(fd, buf, len, flags, addr, addrlen)
                let file = fds().get(a0 as i64)?;
                let data = read_user(a1, a2 as usize)?;
                if is_inet(&file) {
                    let to = if a4 != 0 { Some(sockaddr_in(&read_user(a4, (a5 as usize).min(128))?)?) } else { None };
                    inet_send(&file, &data, to, a3 & 0x40 != 0)? as i64
                } else {
                    channel_write(&file, &data, Vec::new(), a3 & 0x40 != 0)? as i64
                }
            }
            45 => {
                // recvfrom(fd, buf, len, flags, addr, addrlen)
                let file = fds().get(a0 as i64)?;
                if is_inet(&file) {
                    // flags: MSG_PEEK 0x2, MSG_DONTWAIT 0x40
                    let (data, from) = inet_recv(&file, a2 as usize, a3 & 0x40 != 0, a3 & 0x2 != 0)?;
                    write_user(a1, &data)?;
                    if let Some((ip, port)) = from {
                        put_addr(a4, a5, &encode_sockaddr_in(ip, port))?;
                    }
                    data.len() as i64
                } else {
                    let (data, _, _) = channel_read(&file, a2 as usize, false, a3 & 0x40 != 0)?;
                    write_user(a1, &data)?;
                    if a4 != 0 && a5 != 0 {
                        write_user(a5, &0u32.to_le_bytes())?;
                    }
                    data.len() as i64
                }
            }
            307 => {
                // sendmmsg(fd, mmsghdr*, vlen, flags): one sendmsg per entry
                let mut sent = 0;
                for i in 0..a2.min(64) {
                    let m = a1 + i * 64;
                    match sendmsg(a0 as i64, m, a3) {
                        Ok(n) => {
                            write_user(m + 56, &(n as u32).to_le_bytes())?;
                            sent += 1;
                        }
                        Err(e) if sent == 0 => return Err(e),
                        Err(_) => break,
                    }
                }
                sent
            }
            46 => sendmsg(a0 as i64, a1, a2)?,
            47 => recvmsg(a0 as i64, a1, a2)?,
            48 => {
                // shutdown(fd, how): 0 read, 1 write, 2 both
                let file = fds().get(a0 as i64)?;
                if let Kind::Channel(end) = &file.lock().kind {
                    if a1 != 0 && let Some(tx) = &end.tx {
                        tx.lock().writer_closed = true;
                    }
                    if a1 != 1 && let Some(rx) = &end.rx {
                        rx.lock().reader_closed = true;
                    }
                }
                ipc::notify();
                0
            }
            49 => {
                // bind(fd, addr, len): UDP ports; TCP client sockets ignore it
                let file = fds().get(a0 as i64)?;
                let mut f = file.lock();
                if let Kind::Inet(s) = &mut f.kind
                    && s.udp
                {
                    let (_, port) = sockaddr_in(&read_user(a1, (a2 as usize).min(128))?)?;
                    s.local_port = crate::net::udp_bind(s.handle, port).map_err(sock_err)?;
                }
                0
            }
            50 => 0, // listen
            51 | 52 => {
                // getsockname, getpeername
                let file = fds().get(a0 as i64)?;
                let sa = match &file.lock().kind {
                    Kind::Inet(s) if nr == 51 => encode_sockaddr_in(crate::net::local_ip(), s.local_port),
                    Kind::Inet(s) => {
                        let (ip, port) = s.peer.ok_or(ENOTCONN)?;
                        encode_sockaddr_in(ip, port)
                    }
                    _ => 1u16.to_le_bytes().to_vec(), // unnamed AF_UNIX
                };
                put_addr(a1, a2, &sa)?;
                0
            }
            54 => 0, // setsockopt
            55 => {
                // getsockopt(fd, level, name, value, len*)
                let so_error = {
                    let file = fds().get(a0 as i64)?;
                    let mut f = file.lock();
                    match &mut f.kind {
                        Kind::Inet(s) if !s.udp => {
                            let e = core::mem::take(&mut s.error);
                            if e != 0 { e } else if crate::net::tcp_connected(s.handle) == Some(false) && s.peer.is_some() { ECONNREFUSED } else { 0 }
                        }
                        _ => 0,
                    }
                };
                let v: Vec<u8> = match (a1, a2) {
                    (1, 4) => (so_error as i32).to_le_bytes().to_vec(), // SO_ERROR
                    (1, 3) => {
                        let t = match &fds().get(a0 as i64)?.lock().kind {
                            Kind::Channel(e) => match e.kind { ipc::SockType::Stream => 1, ipc::SockType::Datagram => 2, ipc::SockType::SeqPacket => 5 },
                            _ => 1,
                        };
                        (t as i32).to_le_bytes().to_vec()
                    }
                    (1, 7) | (1, 8) => 212992i32.to_le_bytes().to_vec(), // SO_SNDBUF, SO_RCVBUF
                    (1, 17) => {
                        // SO_PEERCRED: struct ucred { pid, uid, gid }
                        let mut c = Vec::new();
                        c.extend_from_slice(&(proc().pid as i32).to_le_bytes());
                        c.extend_from_slice(&0u32.to_le_bytes());
                        c.extend_from_slice(&0u32.to_le_bytes());
                        c
                    }
                    _ => 0i32.to_le_bytes().to_vec(),
                };
                write_user(a3, &v)?;
                write_user(a4, &(v.len() as u32).to_le_bytes())?;
                0
            }
            284 | 290 => {
                // eventfd(initval), eventfd2(initval, flags)
                let flags = if nr == 290 { a1 } else { 0 };
                // Valid flags: EFD_SEMAPHORE, EFD_NONBLOCK, EFD_CLOEXEC (programs probe with ~0).
                if flags & !(1 | 0o4000 | 0o2000000) != 0 {
                    return Err(EINVAL);
                }
                let e = ipc::eventfd(a0 as u32 as u64, flags & 1 != 0);
                fds().add(new_file(Kind::EventFd(e), (flags & O_NONBLOCK) | 2), 0)
            }
            213 | 291 => fds().add(new_file(Kind::Epoll(ipc::Epoll::default()), 2), 0), // epoll_create(1)
            233 => {
                // epoll_ctl(epfd, op, fd, event*)
                let ep = fds().get(a0 as i64)?;
                let target = fds().get(a2 as i64)?;
                let (events, data) = if a3 != 0 {
                    let e = read_user(a3, 12)?;
                    (u32::from_le_bytes(e[0..4].try_into().unwrap()), u64::from_le_bytes(e[4..12].try_into().unwrap()))
                } else {
                    (0, 0)
                };
                let mut epf = ep.lock();
                let Kind::Epoll(ep) = &mut epf.kind else { return Err(EINVAL) };
                let existing = ep.interests.iter().position(|i| i.fd == a2 as i64 && i.file.upgrade().is_some_and(|f| Arc::ptr_eq(&f, &target)));
                match (a1, existing) {
                    (1, Some(_)) => return Err(EEXIST),
                    (1, None) => ep.interests.push(ipc::Interest { fd: a2 as i64, file: Arc::downgrade(&target), events, data, reported: u64::MAX }),
                    (2, Some(i)) => {
                        ep.interests.remove(i);
                    }
                    (3, Some(i)) => {
                        ep.interests[i].events = events;
                        ep.interests[i].data = data;
                        ep.interests[i].reported = u64::MAX;
                    }
                    _ => return Err(ENOENT),
                }
                0
            }
            232 | 281 | 441 => {
                // epoll_wait(epfd, events, max, timeout_ms), epoll_pwait(..., sigmask), epoll_pwait2(..., timespec*)
                let deadline = if nr == 441 {
                    deadline(a3, false, 1)?
                } else if (a3 as i32) < 0 {
                    None
                } else {
                    Some(pit::ticks() + (a3 as u32 as u64).div_ceil(1000 / pit::HZ))
                };
                epoll_wait(a0 as i64, a1, a2 as usize, deadline)?
            }
            319 => {
                // memfd_create(name, flags): CLOEXEC, ALLOW_SEALING, NOEXEC_SEAL, EXEC.
                // Unknown flags (and huge pages) are EINVAL, which programs probe for.
                if a1 & !0x1B != 0 {
                    return Err(EINVAL);
                }
                let flags = if a1 & 1 != 0 { 0o2000000 } else { 0 } | 2;
                fds().add(new_file(Kind::Memfd(ipc::memfd(), None), flags), 0)
            }
            77 | 285 => {
                // ftruncate(fd, len), fallocate(fd, mode, off, len)
                let file = fds().get(a0 as i64)?;
                let size = if nr == 77 { a1 } else { a2 + a3 };
                let mut fl = file.lock();
                match &mut fl.kind {
                    Kind::Memfd(m, _) => {
                        let mut m = m.lock();
                        if nr == 77 || size > m.size {
                            m.truncate(size);
                        }
                    }
                    Kind::File { data, dirty, .. } => {
                        data.to_mut().resize(size as usize, 0);
                        *dirty = true;
                    }
                    _ => return Err(EINVAL),
                }
                0
            }
            8 => {
                // lseek
                let file = fds().get(a0 as i64)?;
                let mut fl = file.lock();
                let len = match &fl.kind {
                    Kind::File { data, .. } => data.len() as i64,
                    Kind::Tty => return Err(ESPIPE),
                    _ => 0,
                };
                let base = match a2 {
                    0 => 0,
                    1 => fl.pos as i64,
                    2 => len,
                    _ => return Err(EINVAL),
                };
                let pos = base + a1 as i64;
                if pos < 0 {
                    return Err(EINVAL);
                }
                fl.pos = pos as usize;
                pos
            }
            9 => {
                // mmap
                let len = a1.next_multiple_of(PAGE);
                if len == 0 {
                    return Err(EINVAL);
                }
                let (flags, fd) = (a3, a4 as i32 as i64);
                let writable = a2 & 2 != 0;
                let addr = {
                    let mut m = mem();
                    if flags & 0x10 != 0 {
                        // MAP_FIXED
                        if a0 % PAGE != 0 || a0 < 0x1_0000 || a0 + len > process::USER_MAX {
                            return Err(EINVAL);
                        }
                        m.unmap(a0, len);
                        // Writable while we fill it; the requested protection is applied below.
                        m.areas.push(process::Area { start: a0, end: a0 + len, writable: true });
                        a0
                    } else {
                        m.reserve_at(a0, len, true)
                    }
                };
                if flags & 0x20 == 0 && fd >= 0 {
                    // File mapping. Read-only system files: map their pages directly
                    // (copy on write). Other files: copy the contents in.
                    let file = fds().get(fd)?;
                    let fl = file.lock();
                    match &fl.kind {
                        Kind::Memfd(m, _) => {
                            // Shared memory: map its frames (every mapping sees the same pages).
                            mem().map_shared(addr, m.clone(), a5 as usize, len);
                        }
                        Kind::File { data: FileData::Static(s), .. } if a5 % PAGE == 0 => {
                            mem().map_file(addr, s, a5 as usize, len);
                        }
                        Kind::File { data, .. } => {
                            let bytes = data.get(a5 as usize..).map(|d| d[..d.len().min(len as usize)].to_vec()).unwrap_or_default();
                            drop(fl);
                            write_user(addr, &bytes)?;
                        }
                        _ => return Err(EINVAL),
                    }
                }
                if !writable {
                    mem().protect(addr, len, false);
                }
                addr as i64
            }
            28 => {
                // madvise: MADV_DONTNEED and MADV_FREE drop the pages (they read as zero again)
                if a2 == 4 || a2 == 8 {
                    mem().discard(a0, a1);
                }
                0
            }
            10 => {
                // mprotect: write permission is enforced (exec and read are not)
                if a0 % PAGE != 0 {
                    return Err(EINVAL);
                }
                if !mem().protect(a0, a1, a2 & 2 != 0) {
                    return Err(ENOMEM_ERR);
                }
                0
            }
            149 | 150 => 0, // mlock, munlock
            11 => {
                // munmap
                mem().unmap(a0, a1);
                0
            }
            12 => {
                // brk
                let mut u = mem();
                if a0 >= u.brk_start && a0 < u.brk_start + (1 << 36) {
                    let new = a0.next_multiple_of(PAGE);
                    if new < u.brk {
                        let old = u.brk;
                        u.unmap(new, old - new);
                    } else if new > u.brk {
                        let old = u.brk;
                        u.areas.push(process::Area { start: old, end: new, writable: true });
                    }
                    u.brk = new;
                    a0 as i64
                } else {
                    u.brk as i64
                }
            }
            13 => {
                // rt_sigaction(sig, act, oldact, sigsetsize)
                let sig = a0 as u32;
                if sig == 0 || sig > signal::NSIG {
                    return Err(EINVAL);
                }
                let old = proc().signals.lock().actions[sig as usize];
                if a2 != 0 {
                    let mut b = Vec::with_capacity(32);
                    for v in [old.handler, old.flags, old.restorer, old.mask] {
                        b.extend_from_slice(&v.to_le_bytes());
                    }
                    write_user(a2, &b)?;
                }
                if a1 != 0 {
                    if sig == signal::SIGKILL || sig == signal::SIGSTOP {
                        return Err(EINVAL);
                    }
                    let b = read_user(a1, 32)?;
                    let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
                    let a = signal::Action { handler: w(0), flags: w(1), restorer: w(2), mask: w(3) & !(signal::bit(signal::SIGKILL) | signal::bit(signal::SIGSTOP)) };
                    let mut st = proc().signals.lock();
                    st.actions[sig as usize] = a;
                    // Setting a signal to "ignore" discards it if pending (POSIX).
                    if a.handler == 1 {
                        st.pending &= !signal::bit(sig);
                    }
                }
                0
            }
            14 => {
                // rt_sigprocmask(how, set, oldset, sigsetsize)
                let t = thread();
                if a2 != 0 {
                    write_user(a2, &t.sigmask.to_le_bytes())?;
                }
                if a1 != 0 {
                    let set = read_u64(a1)? & !(signal::bit(signal::SIGKILL) | signal::bit(signal::SIGSTOP));
                    t.sigmask = match a0 {
                        0 => t.sigmask | set,  // SIG_BLOCK
                        1 => t.sigmask & !set, // SIG_UNBLOCK
                        2 => set,              // SIG_SETMASK
                        _ => return Err(EINVAL),
                    };
                }
                0
            }
            127 => {
                // rt_sigpending(set, sigsetsize)
                let t = thread();
                let pending = (t.pending | proc().signals.lock().pending) & t.sigmask;
                write_user(a0, &pending.to_le_bytes())?;
                0
            }
            131 => {
                // sigaltstack(ss, old_ss): stack_t { ss_sp, ss_flags (int), ss_size }
                let t = thread();
                if a1 != 0 {
                    let mut b = Vec::with_capacity(24);
                    b.extend_from_slice(&t.altstack.0.to_le_bytes());
                    b.extend_from_slice(&(t.altstack.2 as u64).to_le_bytes());
                    b.extend_from_slice(&t.altstack.1.to_le_bytes());
                    write_user(a1, &b)?;
                }
                if a0 != 0 {
                    let b = read_user(a0, 24)?;
                    let sp = u64::from_le_bytes(b[0..8].try_into().unwrap());
                    let flags = u32::from_le_bytes(b[8..12].try_into().unwrap());
                    let size = u64::from_le_bytes(b[16..24].try_into().unwrap());
                    t.altstack = if flags & 2 != 0 { (0, 0, 2) } else { (sp, size, flags & !1) };
                }
                0
            }
            15 => signal::sigreturn(f), // rt_sigreturn
            34 | 130 => {
                // pause(), rt_sigsuspend(mask, sigsetsize): wait for a signal handler to run
                let t = thread();
                if nr == 130 {
                    let temporary = read_u64(a0)? & !(signal::bit(signal::SIGKILL) | signal::bit(signal::SIGSTOP));
                    // The handler runs with the temporary mask; its frame restores the old one.
                    t.saved_mask = Some(t.sigmask);
                    t.sigmask = temporary;
                }
                while !signal::has_deliverable(thread()) {
                    process::check_exiting();
                    task::wait_event();
                }
                return Err(EINTR);
            }
            16 => {
                // ioctl
                let file = fds().get(a0 as i64)?;
                let tty = matches!(file.lock().kind, Kind::Tty);
                match a1 {
                    0x5401 if tty => {
                        // TCGETS: a canonical, echoing terminal.
                        let mut t = vec![0u8; 60];
                        t[0..4].copy_from_slice(&0x0500u32.to_le_bytes()); // ICRNL | IXON
                        t[4..8].copy_from_slice(&0x0005u32.to_le_bytes()); // OPOST | ONLCR
                        t[8..12].copy_from_slice(&0x00BFu32.to_le_bytes()); // CS8 | CREAD | B38400
                        t[12..16].copy_from_slice(&0x8A3Bu32.to_le_bytes()); // ISIG | ICANON | ECHO...
                        write_user(a2, &t)?;
                        0
                    }
                    0x5402..=0x5404 if tty => 0,
                    0x5413 if tty => {
                        // TIOCGWINSZ
                        let (cols, rows) = task::current_tty().map(|t| { let t = t.lock(); (t.grid.cols, t.grid.rows) }).unwrap_or((80, 25));
                        let mut w = [0u8; 8];
                        w[0..2].copy_from_slice(&(rows as u16).to_le_bytes());
                        w[2..4].copy_from_slice(&(cols as u16).to_le_bytes());
                        write_user(a2, &w)?;
                        0
                    }
                    0x540F if tty => {
                        write_user(a2, &(proc().pid as u32).to_le_bytes())?;
                        0
                    }
                    0x541B => {
                        // FIONREAD
                        let n = match &file.lock().kind {
                            Kind::Channel(e) => e.rx.as_ref().map_or(0, |c| if c.lock().readable() { 1 } else { 0 }),
                            _ => 0,
                        };
                        write_user(a2, &(n as u32).to_le_bytes())?;
                        0
                    }
                    0x5421 => {
                        // FIONBIO
                        let on = read_u32(a2)? != 0;
                        let mut f = file.lock();
                        f.flags = if on { f.flags | O_NONBLOCK } else { f.flags & !O_NONBLOCK };
                        0
                    }
                    _ => return Err(ENOTTY),
                }
            }
            17 | 18 => {
                // pread64, pwrite64
                let file = fds().get(a0 as i64)?;
                if nr == 17 {
                    let data = do_read(&file, a2 as usize, Some(a3 as usize))?;
                    write_user(a1, &data)?;
                    data.len() as i64
                } else {
                    let data = read_user(a1, a2 as usize)?;
                    do_write(&file, &data, Some(a3 as usize))? as i64
                }
            }
            19 | 20 => {
                // readv, writev
                let file = fds().get(a0 as i64)?;
                let mut total = 0i64;
                for i in 0..a2.min(1024) {
                    let base = read_u64(a1 + i * 16)?;
                    let len = read_u64(a1 + i * 16 + 8)? as usize;
                    if len == 0 {
                        continue;
                    }
                    // Like Linux: stop at the first short transfer, and report what was
                    // already transferred instead of a later error (EAGAIN on a socket whose
                    // buffer filled up). Carrying on past a short write would drop bytes
                    // from the middle of a stream; failing after one would make the program
                    // send them twice (both break TLS connections).
                    let done = if nr == 19 {
                        do_read(&file, len, None).and_then(|data| write_user(base, &data).map(|_| data.len()))
                    } else {
                        read_user(base, len).and_then(|data| do_write(&file, &data, None))
                    };
                    match done {
                        Ok(n) => {
                            total += n as i64;
                            if n < len {
                                break;
                            }
                        }
                        Err(_) if total > 0 => break,
                        Err(e) => return Err(e),
                    }
                }
                total
            }
            21 | 269 | 439 => {
                // access, faccessat, faccessat2
                let p = if nr == 21 { resolve(&read_cstr(a0)?) } else { at_path(a0 as i32 as i64, &read_cstr(a1)?)? };
                stat_path(&p)?;
                0
            }
            24 => {
                task::yield_now();
                0
            }
            32 | 33 | 292 => {
                // dup, dup2, dup3
                let file = fds().get(a0 as i64)?;
                if nr == 32 {
                    fds().add(file, 0)
                } else {
                    let mut fds = fds();
                    let new = a1 as usize;
                    if new > 4096 {
                        return Err(EBADF);
                    }
                    if new >= fds.fds.len() {
                        fds.fds.resize(new + 1, None);
                    }
                    fds.fds[new] = Some(file);
                    fds.set_cloexec(new as i64, nr == 292 && a2 & O_CLOEXEC != 0);
                    new as i64
                }
            }
            35 => {
                // nanosleep
                let t = read_user(a0, 16)?;
                let (s, ns) = (u64::from_le_bytes(t[0..8].try_into().unwrap()), u64::from_le_bytes(t[8..16].try_into().unwrap()));
                sleep_ns(s.saturating_mul(1_000_000_000).saturating_add(ns));
                0
            }
            230 => {
                // clock_nanosleep
                let t = read_user(a2, 16)?;
                let target = u64::from_le_bytes(t[0..8].try_into().unwrap()).saturating_mul(1_000_000_000) + u64::from_le_bytes(t[8..16].try_into().unwrap());
                let ns = if a1 & 1 != 0 { target.saturating_sub(now_ns(a0)) } else { target };
                sleep_ns(ns);
                0
            }
            39 => proc().pid as i64,
            186 => thread().tid as i64,
            110 => proc().parent.load(core::sync::atomic::Ordering::Relaxed) as i64, // getppid
            60 => process::exit_thread(a0 as i32 & 0xFF),
            231 => process::exit(a0 as i32 & 0xFF),
            63 => {
                write_user(a0, &uname())?;
                0
            }
            72 => match a1 {
                // fcntl
                0 | 1030 => {
                    let file = fds().get(a0 as i64)?;
                    let mut t = fds();
                    let fd = t.add(file, a2 as usize);
                    t.set_cloexec(fd, a1 == 1030);
                    fd
                }
                1 => {
                    // F_GETFD
                    let t = fds();
                    t.get(a0 as i64)?;
                    t.cloexec.contains(&(a0 as usize)) as i64
                }
                2 => {
                    // F_SETFD
                    let mut t = fds();
                    t.get(a0 as i64)?;
                    t.set_cloexec(a0 as i64, a2 & 1 != 0);
                    0
                }
                1033 => 0, // F_ADD_SEALS (memfd sealing: accepted, not enforced)
                1034 => 0, // F_GET_SEALS
                3 => fds().get(a0 as i64)?.lock().flags as i64,
                4 => {
                    let file = fds().get(a0 as i64)?;
                    let mut f = file.lock();
                    f.flags = (f.flags & !O_NONBLOCK) | (a2 & O_NONBLOCK);
                    0
                }
                _ => return Err(EINVAL),
            },
            79 => {
                // getcwd
                let cwd = format!("/{}\0", fds().cwd);
                if cwd.len() > a1 as usize {
                    return Err(ERANGE);
                }
                write_user(a0, cwd.as_bytes())?;
                cwd.len() as i64
            }
            81 => {
                // fchdir
                let file = fds().get(a0 as i64)?;
                let path = match &file.lock().kind {
                    Kind::Dir { at } => dir_path(at),
                    _ => return Err(ENOTDIR),
                };
                fds().cwd = path;
                0
            }
            80 => {
                // chdir
                let name = resolve(&read_cstr(a0)?);
                if !is_dir(&name) {
                    return Err(ENOENT);
                }
                fds().cwd = name;
                0
            }
            85 => {
                // creat(path, mode) = open(path, O_CREAT | O_WRONLY | O_TRUNC)
                let p = read_cstr(a0)?;
                let file = open(&p, 0o100 | 1 | 0o1000, a1 as u32)?;
                fds().add(file, 0)
            }
            86 | 265 => {
                // link(old, new), linkat(olddirfd, old, newdirfd, new, flags)
                let (from, to) = if nr == 86 { (resolve_nofollow(&read_cstr(a0)?), resolve_nofollow(&read_cstr(a1)?)) } else { (at_path_nf(a0 as i32 as i64, &read_cstr(a1)?, a4 & 0x400 == 0)?, at_path_nf(a2 as i32 as i64, &read_cstr(a3)?, false)?) };
                let (m, rf, rt) = ns().same_mount(&from, &to)?;
                m.fs.link(&rf, &rt)?;
                0
            }
            88 | 266 => {
                // symlink(target, path), symlinkat(target, newdirfd, path)
                let target = read_cstr(a0)?;
                let p = if nr == 88 { resolve_nofollow(&read_cstr(a1)?) } else { at_path_nf(a1 as i32 as i64, &read_cstr(a2)?, false)? };
                if target.is_empty() {
                    return Err(ENOENT);
                }
                let (m, rel) = ns().writable(&p)?;
                m.fs.symlink(&target, &rel)?;
                0
            }
            84 => {
                // rmdir
                let p = resolve_nofollow(&read_cstr(a0)?);
                let (m, rel) = ns().writable(&p)?;
                m.fs.rmdir(&rel)?;
                0
            }
            90 | 268 => {
                // chmod(path, mode), fchmodat(dirfd, path, mode)
                let p = if nr == 90 { resolve(&read_cstr(a0)?) } else { at_path(a0 as i32 as i64, &read_cstr(a1)?)? };
                let mode = if nr == 90 { a1 } else { a2 };
                let (m, rel) = ns().writable(&p)?;
                m.fs.chmod(&rel, mode as u32)?;
                0
            }
            91 | 93 => {
                // fchmod(fd, mode), fchown(fd, uid, gid)
                let path = fd_path(a0 as i64)?;
                if let Some(p) = path {
                    let (m, rel) = ns().find_mount(&p);
                    if nr == 91 { m.fs.chmod(&rel, a1 as u32) } else { m.fs.chown(&rel, a1 as u32, a2 as u32) }.or_else(|e| if e == ENOENT { Ok(()) } else { Err(e) })?;
                }
                0
            }
            92 | 94 | 260 => {
                // chown(path, uid, gid), lchown, fchownat(dirfd, path, uid, gid, flags)
                let p = match nr {
                    92 => resolve(&read_cstr(a0)?),
                    94 => resolve_nofollow(&read_cstr(a0)?),
                    _ => at_path_nf(a0 as i32 as i64, &read_cstr(a1)?, a4 & 0x100 == 0)?,
                };
                let (uid, gid) = if nr == 260 { (a2, a3) } else { (a1, a2) };
                let (m, rel) = ns().writable(&p)?;
                m.fs.chown(&rel, uid as u32, gid as u32)?;
                0
            }
            83 | 258 => {
                // mkdir(path, mode), mkdirat(dirfd, path, mode)
                let (p, mode) = if nr == 83 { (resolve_nofollow(&read_cstr(a0)?), a1) } else { (at_path_nf(a0 as i32 as i64, &read_cstr(a1)?, false)?, a2) };
                if stat_path(&p).is_ok() {
                    return Err(EEXIST);
                }
                let (m, rel) = ns().writable(&p)?;
                m.fs.mkdir(&rel, mode as u32 & !0o022)?;
                0
            }
            82 | 264 | 316 => {
                // rename, renameat, renameat2(olddirfd, old, newdirfd, new, flags)
                let (from, to) = if nr == 82 { (resolve_nofollow(&read_cstr(a0)?), resolve_nofollow(&read_cstr(a1)?)) } else { (at_path_nf(a0 as i32 as i64, &read_cstr(a1)?, false)?, at_path_nf(a2 as i32 as i64, &read_cstr(a3)?, false)?) };
                if nr == 316 && a4 & 1 != 0 && stat_path(&to).is_ok() {
                    return Err(EEXIST); // RENAME_NOREPLACE
                }
                let (m, rf, rt) = ns().same_mount(&from, &to)?;
                m.fs.rename(&rf, &rt)?;
                0
            }
            87 | 263 => {
                // unlink(path), unlinkat(dirfd, path, flags: AT_REMOVEDIR)
                let p = if nr == 87 { resolve_nofollow(&read_cstr(a0)?) } else { at_path_nf(a0 as i32 as i64, &read_cstr(a1)?, false)? };
                if let Some(n) = p.strip_prefix("dev/shm/") {
                    return SHM.lock().remove(n).map(|_| 0).ok_or(ENOENT);
                }
                let (m, rel) = ns().writable(&p)?;
                if nr == 263 && a2 & 0x200 != 0 { m.fs.rmdir(&rel)? } else { m.fs.unlink(&rel)? }
                0
            }
            280 | 235 => {
                // utimensat(dirfd, path, times[2], flags), utimes(path, timeval times[2]):
                // the modification time (index 1); NULL times = now.
                let (p, times) = if nr == 280 {
                    let p = if a1 == 0 { fd_path(a0 as i64)?.ok_or(EBADF)? } else { at_path_nf(a0 as i32 as i64, &read_cstr(a1)?, a3 & 0x100 == 0)? };
                    (p, a2)
                } else {
                    (resolve(&read_cstr(a0)?), a1)
                };
                let mtime = if times == 0 {
                    Some(rtc::unix_time())
                } else {
                    let t = read_user(times + 16, 16)?;
                    let (sec, frac) = (u64::from_le_bytes(t[0..8].try_into().unwrap()), u64::from_le_bytes(t[8..16].try_into().unwrap()));
                    match frac {
                        _ if nr == 235 => Some(sec),
                        0x3FFF_FFFF => Some(rtc::unix_time()), // UTIME_NOW
                        0x3FFF_FFFE => None,                   // UTIME_OMIT
                        _ => Some(sec),
                    }
                };
                stat_path(&p)?;
                if let Some(t) = mtime {
                    let (m, rel) = ns().find_mount(&p);
                    m.fs.set_mtime(&rel, t)?;
                }
                0
            }
            289 | 282 => {
                // signalfd4(fd, mask, sizemask, flags), signalfd(fd, mask, sizemask)
                let mask = read_u64(a1)? & !(signal::bit(signal::SIGKILL) | signal::bit(signal::SIGSTOP));
                let flags = if nr == 289 { a3 } else { 0 };
                proc().signals.lock().watched |= mask;
                if a0 as i32 == -1 {
                    let fd = fds().add(new_file(Kind::SignalFd(mask), (flags & O_NONBLOCK) | 2), 0);
                    fds().set_cloexec(fd, flags & O_CLOEXEC != 0);
                    fd
                } else {
                    let file = fds().get(a0 as i64)?;
                    let mut fl = file.lock();
                    let Kind::SignalFd(m) = &mut fl.kind else { return Err(EINVAL) };
                    *m = mask;
                    a0 as i64
                }
            }
            437 => {
                // openat2(dirfd, path, struct open_how { flags, mode, resolve }, size):
                // RESOLVE_IN_ROOT (0x10) resolves the path as if dirfd were the root.
                let how = read_user(a2, 24)?;
                let q = |i: usize| u64::from_le_bytes(how[i * 8..i * 8 + 8].try_into().unwrap());
                let (flags, mode, res) = (q(0), q(1), q(2));
                let p = read_cstr(a1)?;
                let dirfd = a0 as i32 as i64;
                let base = if dirfd == AT_FDCWD { fds().cwd.clone() } else { fd_path(dirfd)?.ok_or(ENOTDIR)? };
                let name = if res & 0x10 != 0 {
                    ns().walk_in(&base, &p, flags & O_NOFOLLOW == 0)?
                } else {
                    ns().walk(&base, &p, flags & O_NOFOLLOW == 0)?
                };
                let file = open(&format!("/{name}"), flags, mode as u32)?;
                let mut t = fds();
                let fd = t.add(file, 0);
                t.set_cloexec(fd, flags & O_CLOEXEC != 0);
                fd
            }
            125 => {
                // capget(header, data): every capability, as root has (Nova does not
                // check them). Version 3 headers carry two 32-bit sets.
                let version = read_u32(a0)?;
                if version != 0x2008_0522 && version != 0x2007_1026 {
                    write_user(a0, &0x2008_0522u32.to_le_bytes())?;
                    return if a1 == 0 { Ok(0) } else { Err(EINVAL) };
                }
                if a1 != 0 {
                    let all = [u32::MAX, u32::MAX, 0, (1u32 << (CAP_LAST - 31)) - 1, (1u32 << (CAP_LAST - 31)) - 1, 0];
                    write_user(a1, &all.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>())?;
                }
                0
            }
            126 => 0, // capset: accepted
            165 => {
                // mount(source, target, fstype, flags, data)
                const MS_RDONLY: u64 = 1;
                const MS_REMOUNT: u64 = 0x20;
                const MS_BIND: u64 = 0x1000;
                const MS_MOVE: u64 = 0x2000;
                const MS_REC: u64 = 0x4000;
                const PROPAGATION: u64 = (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20);
                let target = resolve(&read_cstr(a1)?);
                let ns = ns();
                if a3 & MS_REMOUNT != 0 {
                    ns.remount(&target, a3 & MS_RDONLY != 0)?;
                } else if a3 & MS_BIND != 0 {
                    let src = resolve(&read_cstr(a0)?);
                    stat_path(&src)?;
                    stat_path(&target)?;
                    ns.bind(&src, &target, a3 & MS_REC != 0)?;
                } else if a3 & MS_MOVE != 0 {
                    ns.move_mount(&resolve(&read_cstr(a0)?), &target)?;
                } else if a3 & PROPAGATION != 0 {
                    // Shared/private/slave: nothing propagates between namespaces here.
                } else {
                    if !is_dir(&target) {
                        return Err(if stat_path(&target).is_ok() { ENOTDIR } else { ENOENT });
                    }
                    let fs: Arc<dyn vfs::Backend> = match read_cstr(a2)?.as_str() {
                        "tmpfs" | "mqueue" | "devpts" => Arc::new(vfs::TmpFs::new()),
                        "proc" => Arc::new(ProcFs),
                        "sysfs" => Arc::new(SysFs),
                        "devtmpfs" => Arc::new(DevFs),
                        _ => return Err(ENODEV),
                    };
                    ns.mount(&target, fs, "", a3 & MS_RDONLY != 0);
                }
                0
            }
            442 => {
                // mount_setattr(dirfd, path, flags, struct mount_attr { set, clr, propagation,
                // userns_fd }, size): MOUNT_ATTR_RDONLY; the other attributes (nosuid,
                // nodev, noexec, atime) have no effect in Nova.
                let attr = read_user(a3, 32)?;
                let (set, clr) = (u64::from_le_bytes(attr[0..8].try_into().unwrap()), u64::from_le_bytes(attr[8..16].try_into().unwrap()));
                let p = read_cstr(a1)?;
                let target = if p.is_empty() && a2 & 0x1000 != 0 { fd_path(a0 as i32 as i64)?.ok_or(EBADF)? } else { at_path(a0 as i32 as i64, &p)? };
                if set & 1 != 0 || clr & 1 != 0 {
                    ns().set_readonly(&target, set & 1 != 0, a2 & 0x8000 != 0)?;
                }
                0
            }
            166 => {
                // umount2(target, flags)
                let p = read_cstr(a0)?;
                let target = if a1 & 8 != 0 { resolve_nofollow(&p) } else { resolve(&p) };
                ns().umount(&target)?;
                0
            }
            155 => {
                // pivot_root(new_root, put_old): the working directory keeps pointing at the
                // same directory, wherever it now appears.
                let (new_root, put_old) = (resolve(&read_cstr(a0)?), resolve(&read_cstr(a1)?));
                let ns = ns();
                let cwd = ns.at(&fds().cwd.clone());
                ns.pivot_root(&new_root, &put_old)?;
                fds().cwd = ns.vpath(&cwd).unwrap_or_default();
                0
            }
            161 => {
                // chroot(path): this process (and its children) see `path` as /.
                let p = resolve(&read_cstr(a0)?);
                if !is_dir(&p) {
                    return Err(ENOTDIR);
                }
                let new = ns().chroot(&p);
                *proc().ns.lock() = new;
                let cwd = fds().cwd.clone();
                fds().cwd = if cwd == p { String::new() } else { cwd.strip_prefix(&format!("{p}/")).unwrap_or("").into() };
                0
            }
            272 => {
                // unshare(flags): CLONE_NEWNS gives the process its own mount table; the
                // other namespaces (user, pid, ipc, uts, net, cgroup) are accepted as is.
                if a0 & CLONE_NEWNS != 0 {
                    let copy = ns().copy();
                    *proc().ns.lock() = copy;
                }
                0
            }
            308 => return Err(EINVAL), // setns
            76 => {
                // truncate(path, length)
                let p = resolve(&read_cstr(a0)?);
                let (m, rel) = ns().writable(&p)?;
                m.fs.truncate(&rel, a1)?;
                0
            }
            89 | 267 => {
                // readlink(at): only /proc/self/exe (and /proc/self/fd/N) are links
                let (path, buf, size) = if nr == 89 { (a0, a1, a2) } else { (a1, a2, a3) };
                let p = if nr == 89 { resolve_nofollow(&read_cstr(path)?) } else { at_path_nf(a0 as i32 as i64, &read_cstr(path)?, false)? };
                let target = {
                    let (m, rel) = ns().find_mount(&p);
                    m.fs.readlink(&rel).ok_or_else(|| EINVAL_OR_NOENT(&p))?
                };
                let bytes = &target.as_bytes()[..target.len().min(size as usize)];
                write_user(buf, bytes)?;
                bytes.len() as i64
            }
            157 => match a0 {
                // prctl
                3 => 1,                                     // PR_GET_DUMPABLE
                23 => if a1 <= CAP_LAST { 1 } else { return Err(EINVAL) }, // PR_CAPBSET_READ
                21 | 22 | 24 | 47 => 0,                     // PR_GET/SET_SECCOMP, PR_CAPBSET_DROP, PR_CAP_AMBIENT
                1 | 4 | 8 | 15 | 36 | 38 | 0x53564d41 => 0, // PDEATHSIG, SET_DUMPABLE, KEEPCAPS, SET_NAME, CHILD_SUBREAPER, NO_NEW_PRIVS, SET_VMA
                16 => {
                    // PR_GET_NAME
                    let mut n = [0u8; 16];
                    let name = proc().name.as_bytes();
                    n[..name.len().min(15)].copy_from_slice(&name[..name.len().min(15)]);
                    write_user(a1, &n)?;
                    0
                }
                _ => return Err(EINVAL),
            },
            95 => 0o022,                   // umask
            96 => {
                // gettimeofday
                let ns = now_ns(0);
                if a0 != 0 {
                    let mut tv = Vec::new();
                    tv.extend_from_slice(&(ns / 1_000_000_000).to_le_bytes());
                    tv.extend_from_slice(&((ns % 1_000_000_000) / 1000).to_le_bytes());
                    write_user(a0, &tv)?;
                }
                0
            }
            97 | 302 => {
                // getrlimit, prlimit64
                let (resource, old) = if nr == 97 { (a0, a1) } else { (a1, a3) };
                if old != 0 {
                    let (cur, max) = match resource {
                        3 => (8 << 20, u64::MAX), // stack
                        7 => (1024, 4096),        // open files
                        _ => (u64::MAX, u64::MAX),
                    };
                    let mut b = Vec::new();
                    b.extend_from_slice(&cur.to_le_bytes());
                    b.extend_from_slice(&max.to_le_bytes());
                    write_user(old, &b)?;
                }
                0
            }
            99 => {
                // sysinfo
                let (used, free) = crate::memory::heap_usage();
                let mut b = vec![0u8; 112];
                b[0..8].copy_from_slice(&(pit::ticks() / pit::HZ).to_le_bytes());
                b[32..40].copy_from_slice(&((used + free) as u64).to_le_bytes());
                b[40..48].copy_from_slice(&(free as u64).to_le_bytes());
                b[80..82].copy_from_slice(&(task::list().len() as u16).to_le_bytes());
                b[104..108].copy_from_slice(&1u32.to_le_bytes()); // mem_unit
                write_user(a0, &b)?;
                0
            }
            102 | 104 | 107 | 108 | 111 | 112 | 121 => 0, // uid/gid/pgrp/setsid queries: root
            158 => match a0 {
                // arch_prctl
                0x1002 => {
                    thread().fs_base = a1;
                    process::wrmsr(0xC000_0100, a1);
                    0
                }
                0x1003 => {
                    write_u64(a1, thread().fs_base)?;
                    0
                }
                _ => return Err(EINVAL),
            },
            201 => {
                // time
                let t = rtc::unix_time();
                if a0 != 0 {
                    write_u64(a0, t)?;
                }
                t as i64
            }
            202 => futex(a0, a1, a2 as u32, a3, a4, a5 as u32)?,
            56 => {
                // clone(flags, stack, parent_tid, child_tid, tls)
                clone(f, a0, a1, a2, a3, a4)?
            }
            435 => {
                // clone3(struct clone_args *, size)
                let args = read_user(a0, (a1 as usize).min(88))?;
                let w = |i: usize| args.get(i * 8..i * 8 + 8).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
                let (flags, child_tid, parent_tid, stack, stack_size, tls) = (w(0), w(2), w(3), w(5), w(6), w(7));
                clone(f, flags, if stack != 0 { stack + stack_size } else { 0 }, parent_tid, child_tid, tls)?
            }
            204 => {
                // sched_getaffinity: one CPU
                let mut mask = vec![0u8; (a1 as usize).min(128)];
                if let Some(b) = mask.first_mut() {
                    *b = 1;
                }
                write_user(a2, &mask)?;
                mask.len() as i64
            }
            217 => {
                // getdents64
                let file = fds().get(a0 as i64)?;
                let mut fl = file.lock();
                let Kind::Dir { at } = &fl.kind else { return Err(ENOTDIR) };
                let at = at.clone();
                // The file position counts entries already returned (lseek(fd, 0) rewinds).
                let ents = dirents(&at);
                let mut out = Vec::new();
                let mut next = fl.pos;
                for e in ents.iter().skip(fl.pos) {
                    if out.len() + e.len() > a2 as usize {
                        break;
                    }
                    out.extend_from_slice(e);
                    next += 1;
                }
                if out.is_empty() && next < ents.len() {
                    return Err(EINVAL); // the buffer cannot hold one entry
                }
                fl.pos = next;
                drop(fl);
                write_user(a1, &out)?;
                out.len() as i64
            }
            218 => {
                // set_tid_address
                thread().clear_tid = a0;
                thread().tid as i64
            }
            228 => {
                // clock_gettime
                write_user(a1, &timespec(now_ns(a0)))?;
                0
            }
            229 => {
                // clock_getres
                if a1 != 0 {
                    write_user(a1, &timespec(1))?;
                }
                0
            }
            273 => 0,         // set_robust_list
            140 => 20,        // getpriority: nice 0
            141 | 203 => 0,   // setpriority, sched_setaffinity
            // Scheduling policy: every thread is SCHED_OTHER with priority 0; requests to
            // change it are accepted and ignored (Nova schedules round-robin).
            142 | 144 => 0,   // sched_setparam, sched_setscheduler
            143 => {
                // sched_getparam: struct sched_param { int sched_priority; }
                write_user(a1, &0i32.to_le_bytes())?;
                0
            }
            145 => 0,         // sched_getscheduler: SCHED_OTHER
            146 | 147 => 0,   // sched_get_priority_max/min for SCHED_OTHER
            118 | 120 => {
                // getresuid, getresgid: root
                for a in [a0, a1, a2] {
                    write_user(a, &0u32.to_le_bytes())?;
                }
                0
            }
            137 | 138 => {
                // statfs, fstatfs: a tmpfs-like filesystem
                let mut b = vec![0u8; 120];
                b[0..8].copy_from_slice(&0x0102_1994u64.to_le_bytes()); // TMPFS_MAGIC
                b[8..16].copy_from_slice(&4096u64.to_le_bytes()); // f_bsize
                let p = if nr == 137 { Some(resolve(&read_cstr(a0)?)) } else { fd_path(a0 as i64)? };
                let (used, total) = ns().find_mount(&p.unwrap_or_default()).0.fs.usage();
                let total = total.max(used + 4096);
                b[16..24].copy_from_slice(&(total / 4096).to_le_bytes());
                b[24..32].copy_from_slice(&((total - used) / 4096).to_le_bytes());
                b[32..40].copy_from_slice(&((total - used) / 4096).to_le_bytes());
                b[64..72].copy_from_slice(&255u64.to_le_bytes()); // f_namelen
                b[72..80].copy_from_slice(&4096u64.to_le_bytes()); // f_frsize
                write_user(a1, &b)?;
                0
            }
            318 => {
                // getrandom
                let mut v = vec![0u8; (a1 as usize).min(1 << 20)];
                rand::fill(&mut v);
                write_user(a0, &v)?;
                v.len() as i64
            }
            334 => return Err(ENOSYS), // rseq: glibc copes without it
            40 | 326 => {
                // sendfile(out, in, offset*, count), copy_file_range(in, off_in*, out, off_out*, len, flags)
                let (out_fd, in_fd, in_off, out_off, count) = if nr == 40 { (a0, a1, a2, 0, a3) } else { (a2, a0, a1, a3, a4) };
                let (src, dst) = (fds().get(in_fd as i64)?, fds().get(out_fd as i64)?);
                let count = (count as usize).min(1 << 20);
                let at_in = if in_off != 0 { Some(read_u64(in_off)? as usize) } else { None };
                let at_out = if out_off != 0 { Some(read_u64(out_off)? as usize) } else { None };
                let data = do_read(&src, count, at_in)?;
                let n = if data.is_empty() { 0 } else { do_write(&dst, &data, at_out)? };
                if let Some(p) = at_in {
                    write_u64(in_off, (p + data.len()) as u64)?;
                }
                if let Some(p) = at_out {
                    write_u64(out_off, (p + n) as u64)?;
                }
                n as i64
            }
            57 | 58 => process::fork(f, nr == 58, 0, None)? as i64, // fork, vfork
            59 | 322 => {
                // execve(path, argv, envp), execveat(dirfd, path, argv, envp, flags)
                let (path, argv, envp) = if nr == 59 { (a0, a1, a2) } else { (a1, a2, a3) };
                let path = resolve(&read_cstr(path)?);
                let list = |mut p: u64| -> Result<Vec<String>, i64> {
                    let mut v = Vec::new();
                    while p != 0 {
                        let s = read_u64(p)?;
                        if s == 0 || v.len() > 4096 {
                            break;
                        }
                        v.push(read_cstr(s)?);
                        p += 8;
                    }
                    Ok(v)
                };
                let (args, env) = (list(argv)?, list(envp)?);
                process::exec(f, path, args, env)?;
                0
            }
            62 => {
                // kill(pid, sig): a process, or (pid <= 0) every process Nova runs for the caller
                let (pid, sig) = (a0 as i32 as i64, a1 as u32);
                if sig > signal::NSIG {
                    return Err(EINVAL);
                }
                if pid == proc().pid as i64 || pid == 0 {
                    signal::send_process(proc(), sig, proc().pid);
                } else if pid > 0 {
                    let target = process::find(pid as u64).ok_or(ESRCH)?;
                    signal::send_process(&target, sig, proc().pid);
                } else {
                    for p in process::processes() {
                        if p.pid != proc().pid && p.parent.load(core::sync::atomic::Ordering::Relaxed) == proc().pid {
                            signal::send_process(&p, sig, proc().pid);
                        }
                    }
                }
                0
            }
            200 | 234 => {
                // tkill(tid, sig), tgkill(tgid, tid, sig): to a thread
                let (tid, sig) = if nr == 200 { (a0, a1 as u32) } else { (a1, a2 as u32) };
                if sig > signal::NSIG {
                    return Err(EINVAL);
                }
                if tid == thread().tid {
                    signal::send_self(thread(), sig);
                } else if let Some(p) = process::find(if nr == 234 { a0 } else { tid }) {
                    signal::send_process(&p, sig, proc().pid); // to the process: delivered to some thread
                } else {
                    signal::send_process(proc(), sig, proc().pid); // another thread of ours
                }
                0
            }
            61 => {
                // wait4(pid, wstatus, options, rusage)
                let nohang = a2 & 1 != 0;
                match process::wait_child(a0 as i32 as i64, nohang, true)? {
                    Some((pid, status)) => {
                        if a1 != 0 {
                            write_user(a1, &status.to_le_bytes())?;
                        }
                        if a3 != 0 {
                            write_user(a3, &[0u8; 144])?;
                        }
                        pid as i64
                    }
                    None => 0,
                }
            }
            247 => {
                // waitid(idtype, id, infop, options): P_ALL 0, P_PID 1
                let pid = if a0 == 1 { a1 as i32 as i64 } else { -1 };
                let (nohang, reap) = (a3 & 1 != 0, a3 & 0x0100_0000 == 0); // WNOHANG, WNOWAIT
                match process::wait_child(pid, nohang, reap)? {
                    Some((child, status)) => {
                        if a2 != 0 {
                            // siginfo: si_signo = SIGCHLD, si_code = CLD_EXITED (1) / CLD_KILLED (2)
                            let mut info = [0u8; 128];
                            info[0..4].copy_from_slice(&(signal::SIGCHLD as i32).to_le_bytes());
                            let (code, value) = if status & 0x7F != 0 { (2, status & 0x7F) } else { (1, (status >> 8) & 0xFF) };
                            info[8..12].copy_from_slice(&(code as i32).to_le_bytes());
                            info[16..20].copy_from_slice(&(child as i32).to_le_bytes());
                            info[24..28].copy_from_slice(&value.to_le_bytes());
                            write_user(a2, &info)?;
                        }
                        0
                    }
                    None => {
                        if a2 != 0 {
                            write_user(a2, &[0u8; 128])?;
                        }
                        0
                    }
                }
            }
            _ => unimplemented(nr),
        })
    })())
}

const CLONE_VM: u64 = 0x100;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x100000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;
const CLONE_CHILD_SETTID: u64 = 0x1000000;

const CLONE_VFORK: u64 = 0x4000;
pub const CLONE_NEWNS: u64 = 0x20000;
/// CAP_CHECKPOINT_RESTORE: the highest capability number Linux 7.x has.
const CAP_LAST: u64 = 40;

/// clone: a thread with CLONE_VM | CLONE_THREAD; anything else creates a process. A
/// process always gets its own copy of the memory (CLONE_VM without CLONE_THREAD, as
/// glibc's posix_spawn uses with CLONE_VFORK, is served as a vfork-style copy).
fn clone(f: &SyscallFrame, flags: u64, stack: u64, parent_tid: u64, child_tid: u64, tls: u64) -> Result<i64, i64> {
    if flags & CLONE_VM == 0 || flags & CLONE_THREAD == 0 {
        let pid = process::fork(f, flags & CLONE_VFORK != 0, stack, (flags & CLONE_SETTLS != 0).then_some(tls))?;
        if flags & CLONE_NEWNS != 0 {
            process::unshare_ns(pid);
        }
        if flags & CLONE_PARENT_SETTID != 0 {
            write_user(parent_tid, &(pid as u32).to_le_bytes())?;
        }
        return Ok(pid as i64);
    }
    let tid = process::spawn_thread(f, stack, (flags & CLONE_SETTLS != 0).then_some(tls), if flags & CLONE_CHILD_CLEARTID != 0 { child_tid } else { 0 });
    if flags & CLONE_PARENT_SETTID != 0 {
        write_user(parent_tid, &(tid as u32).to_le_bytes())?;
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        write_user(child_tid, &(tid as u32).to_le_bytes())?;
    }
    Ok(tid as i64)
}

const ETIMEDOUT: i64 = 110;
const EINTR: i64 = 4;
const ESRCH: i64 = 3;

struct Waiter {
    process: usize,
    addr: u64,
    bitset: u32,
    task: usize,
    woken: bool,
}

static WAITERS: SpinLock<Vec<Waiter>> = SpinLock::new(Vec::new());

fn key(p: &Process) -> usize {
    p as *const Process as usize
}

/// Wake up to `n` threads of `p` waiting on `addr` (matching `bitset`). Returns how many.
fn wake_matching(p: &Process, addr: u64, n: usize, bitset: u32) -> usize {
    let mut woken = 0;
    for w in WAITERS.lock().iter_mut() {
        if woken >= n {
            break;
        }
        if !w.woken && w.process == key(p) && w.addr == addr && w.bitset & bitset != 0 {
            w.woken = true;
            task::wake(w.task);
            woken += 1;
        }
    }
    woken
}

/// A pipe end usable by the kernel itself (the browser's DevTools pipe): returns the
/// file for the program and the kernel's end.
pub fn kernel_pipe(program_reads: bool) -> (FileRef, ipc::End) {
    let (r, w) = ipc::pipe();
    if program_reads { (new_file(Kind::Channel(r), 0), w) } else { (new_file(Kind::Channel(w), 1), r) }
}

pub fn futex_wake(p: &Process, addr: u64, n: usize) -> usize {
    wake_matching(p, addr, n, u32::MAX)
}

/// Wake every waiter of `p` (the program is exiting).
pub fn futex_wake_all(p: &Process) {
    for w in WAITERS.lock().iter_mut() {
        if w.process == key(p) {
            task::wake(w.task);
        }
    }
}

fn read_u32(addr: u64) -> Result<u32, i64> {
    Ok(u32::from_le_bytes(read_user(addr, 4)?.try_into().unwrap()))
}

/// Deadline in timer ticks from a timespec (relative, or absolute on `clock`).
fn deadline(ts: u64, absolute: bool, clock: u64) -> Result<Option<u64>, i64> {
    if ts == 0 {
        return Ok(None);
    }
    let t = read_user(ts, 16)?;
    let ns = u64::from_le_bytes(t[0..8].try_into().unwrap()).saturating_mul(1_000_000_000).saturating_add(u64::from_le_bytes(t[8..16].try_into().unwrap()));
    let rel = if absolute { ns.saturating_sub(now_ns(clock)) } else { ns };
    Ok(Some(pit::ticks() + rel.div_ceil(1_000_000_000 / pit::HZ)))
}

fn futex_wait(addr: u64, val: u32, until: Option<u64>, bitset: u32) -> Result<i64, i64> {
    if read_u32(addr)? != val {
        return Err(EAGAIN);
    }
    let p = proc();
    let me = task::current_id();
    WAITERS.lock().push(Waiter { process: key(p), addr, bitset, task: me, woken: false });
    let remove = || {
        let mut ws = WAITERS.lock();
        let i = ws.iter().position(|w| w.task == me && w.process == key(p) && w.addr == addr);
        i.map(|i| ws.remove(i).woken).unwrap_or(false)
    };
    loop {
        let woken = WAITERS.lock().iter().any(|w| w.task == me && w.woken);
        if woken {
            remove();
            return Ok(0);
        }
        if p.exiting.load(core::sync::atomic::Ordering::SeqCst) {
            remove();
            return Err(EINTR);
        }
        if until.is_some_and(|u| pit::ticks() >= u) {
            return if remove() { Ok(0) } else { Err(ETIMEDOUT) };
        }
        task::wait_event();
    }
}

fn futex(addr: u64, op: u64, val: u32, a3: u64, addr2: u64, val3: u32) -> Result<i64, i64> {
    let realtime = op & 256 != 0;
    match op & 0x7F {
        0 => futex_wait(addr, val, deadline(a3, false, 1)?, u32::MAX),
        9 => futex_wait(addr, val, deadline(a3, true, if realtime { 0 } else { 1 })?, val3),
        1 => Ok(futex_wake(proc(), addr, val as usize) as i64),
        10 => Ok(wake_matching(proc(), addr, val as usize, val3) as i64),
        3 | 4 => {
            // REQUEUE / CMP_REQUEUE: wake `val`, move up to `a3` waiters to addr2.
            if op & 0x7F == 4 && read_u32(addr)? != val3 {
                return Err(EAGAIN);
            }
            let woken = futex_wake(proc(), addr, val as usize);
            let mut moved = 0;
            for w in WAITERS.lock().iter_mut() {
                if moved >= a3 as usize {
                    break;
                }
                if !w.woken && w.process == key(proc()) && w.addr == addr {
                    w.addr = addr2;
                    moved += 1;
                }
            }
            Ok((woken + moved) as i64)
        }
        5 => {
            // WAKE_OP: *addr2 = op(*addr2, oparg); wake addr; maybe wake addr2.
            let (opcode, cmp) = ((val3 >> 28) & 7, (val3 >> 24) & 15);
            let mut oparg = (val3 >> 12) & 0xFFF;
            let cmparg = val3 & 0xFFF;
            if opcode & 8 != 0 {
                oparg = 1 << oparg.min(31);
            }
            let old = read_u32(addr2)?;
            let new = match opcode & 7 {
                0 => oparg,
                1 => old.wrapping_add(oparg),
                2 => old | oparg,
                3 => old & !oparg,
                4 => old ^ oparg,
                _ => old,
            };
            write_user(addr2, &new.to_le_bytes())?;
            let mut n = futex_wake(proc(), addr, val as usize);
            let hit = match cmp {
                0 => old == cmparg,
                1 => old != cmparg,
                2 => old < cmparg,
                3 => old <= cmparg,
                4 => old > cmparg,
                5 => old >= cmparg,
                _ => false,
            };
            if hit {
                n += futex_wake(proc(), addr2, a3 as usize);
            }
            Ok(n as i64)
        }
        _ => Err(ENOSYS),
    }
}

fn sock_type(t: u64) -> Result<ipc::SockType, i64> {
    Ok(match t & 0xF {
        1 => ipc::SockType::Stream,
        2 => ipc::SockType::Datagram,
        5 => ipc::SockType::SeqPacket,
        _ => return Err(EINVAL),
    })
}

/// sendmsg(fd, msghdr*, flags): gathers the iovecs, passes SCM_RIGHTS descriptors.
fn sendmsg(fd: i64, msg: u64, flags: u64) -> Result<i64, i64> {
    let file = fds().get(fd)?;
    let h = read_user(msg, 56)?;
    let w = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap());
    let (iov, iovlen, control, controllen) = (w(16), w(24), w(32), w(40));
    let mut data = Vec::new();
    for i in 0..iovlen.min(1024) {
        let base = read_u64(iov + i * 16)?;
        let len = read_u64(iov + i * 16 + 8)? as usize;
        data.extend_from_slice(&read_user(base, len)?);
    }
    let mut passed = Vec::new();
    if control != 0 && controllen >= 16 {
        let c = read_user(control, controllen as usize)?;
        let mut off = 0;
        while off + 16 <= c.len() {
            let len = u64::from_le_bytes(c[off..off + 8].try_into().unwrap()) as usize;
            let (level, kind) = (i32::from_le_bytes(c[off + 8..off + 12].try_into().unwrap()), i32::from_le_bytes(c[off + 12..off + 16].try_into().unwrap()));
            if len < 16 || off + len > c.len() {
                break;
            }
            if level == 1 && kind == 1 {
                for chunk in c[off + 16..off + len].chunks_exact(4) {
                    let n = i32::from_le_bytes(chunk.try_into().unwrap());
                    passed.push(fds().get(n as i64)?);
                }
            }
            off += len.next_multiple_of(8);
        }
    }
    if is_inet(&file) {
        let name = w(0);
        let to = if name != 0 { Some(sockaddr_in(&read_user(name, 16)?)?) } else { None };
        return inet_send(&file, &data, to, flags & 0x40 != 0).map(|n| n as i64);
    }
    channel_write(&file, &data, passed, flags & 0x40 != 0).map(|n| n as i64)
}

/// recvmsg(fd, msghdr*, flags): scatters into the iovecs, installs passed descriptors.
fn recvmsg(fd: i64, msg: u64, flags: u64) -> Result<i64, i64> {
    let file = fds().get(fd)?;
    let h = read_user(msg, 56)?;
    let w = |o: usize| u64::from_le_bytes(h[o..o + 8].try_into().unwrap());
    let (iov, iovlen, control, controllen) = (w(16), w(24), w(32), w(40));
    let mut vecs = Vec::new();
    let mut total = 0usize;
    for i in 0..iovlen.min(1024) {
        let base = read_u64(iov + i * 16)?;
        let len = read_u64(iov + i * 16 + 8)? as usize;
        vecs.push((base, len));
        total += len;
    }
    let (data, passed, truncated) = if is_inet(&file) {
        let (d, from) = inet_recv(&file, total, flags & 0x40 != 0, flags & 0x2 != 0)?;
        if let (Some((ip, port)), name) = (from, w(0))
            && name != 0
        {
            write_user(name, &encode_sockaddr_in(ip, port))?;
            write_user(msg + 8, &16u32.to_le_bytes())?;
        }
        (d, Vec::new(), false)
    } else {
        channel_read(&file, total, false, flags & 0x40 != 0)?
    };
    let mut off = 0;
    for (base, len) in vecs {
        if off >= data.len() {
            break;
        }
        let n = len.min(data.len() - off);
        write_user(base, &data[off..off + n])?;
        off += n;
    }
    let mut msg_flags: u32 = if truncated { 0x20 } else { 0 }; // MSG_TRUNC
    let mut used = 0u64;
    if !passed.is_empty() {
        let need = 16 + passed.len() * 4;
        if control == 0 || (controllen as usize) < need {
            msg_flags |= 0x8; // MSG_CTRUNC: descriptors dropped
        } else {
            let mut c = Vec::with_capacity(need.next_multiple_of(8));
            c.extend_from_slice(&(need as u64).to_le_bytes());
            c.extend_from_slice(&1i32.to_le_bytes());
            c.extend_from_slice(&1i32.to_le_bytes());
            for f in passed {
                let n = fds().add(f, 0);
                c.extend_from_slice(&(n as i32).to_le_bytes());
            }
            c.resize(need.next_multiple_of(8), 0);
            write_user(control, &c)?;
            used = need.next_multiple_of(8) as u64;
        }
    }
    write_user(msg + 40, &used.to_le_bytes())?;
    write_user(msg + 48, &msg_flags.to_le_bytes())?;
    if h[8..12] != [0, 0, 0, 0] && !is_inet(&file) {
        write_user(msg + 8, &0u32.to_le_bytes())?; // msg_namelen: no address
    }
    Ok(data.len() as i64)
}

fn poll_fds(ptr: u64, n: usize, deadline: Option<u64>) -> Result<i64, i64> {
    loop {
        let raw = read_user(ptr, n * 8)?;
        let mut out = raw.clone();
        let mut ready = 0;
        for i in 0..n {
            let fd = i32::from_le_bytes(raw[i * 8..i * 8 + 4].try_into().unwrap());
            let want = u16::from_le_bytes(raw[i * 8 + 4..i * 8 + 6].try_into().unwrap()) as u32;
            let rev = if fd < 0 {
                0
            } else {
                match fds().get(fd as i64) {
                    Ok(f) => poll_file(&f).0 & (want | ipc::POLLERR | ipc::POLLHUP),
                    Err(_) => 0x20, // POLLNVAL
                }
            };
            out[i * 8 + 6..i * 8 + 8].copy_from_slice(&(rev as u16).to_le_bytes());
            if rev != 0 {
                ready += 1;
            }
        }
        if ready > 0 || deadline.is_some_and(|d| pit::ticks() >= d) {
            write_user(ptr, &out)?;
            return Ok(ready);
        }
        ipc::wait(deadline);
        process::check_exiting();
    }
}

fn epoll_wait(epfd: i64, events: u64, max: usize, deadline: Option<u64>) -> Result<i64, i64> {
    let ep = fds().get(epfd)?;
    loop {
        let mut out: Vec<(u32, u64)> = Vec::new();
        {
            let mut epf = ep.lock();
            let Kind::Epoll(e) = &mut epf.kind else { return Err(EINVAL) };
            e.interests.retain(|i| i.file.strong_count() > 0);
            for i in e.interests.iter_mut() {
                if out.len() >= max {
                    break;
                }
                let Some(file) = i.file.upgrade() else { continue };
                if Arc::ptr_eq(&file, &ep) {
                    continue;
                }
                let (ev, mark) = poll_file(&file);
                let hit = ev & (i.events | ipc::POLLERR | ipc::POLLHUP) & !(ipc::EPOLLET | ipc::EPOLLONESHOT);
                if hit == 0 || i.events & !(ipc::EPOLLET | ipc::EPOLLONESHOT) == 0 && i.events & ipc::EPOLLONESHOT != 0 {
                    continue;
                }
                if i.events & ipc::EPOLLET != 0 {
                    if i.reported == mark {
                        continue;
                    }
                    i.reported = mark;
                }
                if i.events & ipc::EPOLLONESHOT != 0 {
                    i.events = ipc::EPOLLONESHOT; // disarmed until EPOLL_CTL_MOD
                }
                out.push((hit, i.data));
            }
        }
        if !out.is_empty() {
            let mut b = Vec::with_capacity(out.len() * 12);
            for (ev, data) in &out {
                b.extend_from_slice(&ev.to_le_bytes());
                b.extend_from_slice(&data.to_le_bytes());
            }
            write_user(events, &b)?;
            return Ok(out.len() as i64);
        }
        if deadline.is_some_and(|d| pit::ticks() >= d) {
            return Ok(0);
        }
        ipc::wait(deadline);
        process::check_exiting();
    }
}
