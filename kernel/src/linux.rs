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
pub mod inotify;
pub mod seccomp;
#[cfg(feature = "linux-drivers")]
pub mod lxfile;
pub mod sysv;
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
/// Size of each 32-bit thread's kernel scratch mapping (see compat32::scratch).
pub const COMPAT_SCRATCH_SIZE: u64 = 256 * 1024;
const EROFS: i64 = 30;
const EOPNOTSUPP: i64 = 95;
const EPERM: i64 = 1;
const EADDRINUSE: i64 = 98;
const EISCONN: i64 = 106;
const EACCES: i64 = 13;
const ENODEV: i64 = 19;
const ELOOP: i64 = 40;
const O_NOFOLLOW: u64 = 0o400000;
const O_PATH: u64 = 0o10000000;

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
    TimerFd(Arc<SpinLock<ipc::TimerFd>>),
    Inotify(inotify::InotifyRef),
    /// signalfd: reads the pending signals in its mask as signalfd_siginfo records.
    SignalFd(u64),
    Epoll(ipc::Epoll),
    /// Shared-memory object: memfd, POSIX shm, or (with its path) a tmpfs file.
    Memfd(ipc::SharedRef, Option<vfs::At>),
    /// AF_INET TCP or UDP socket (smoltcp, net.rs).
    Inet(InetSocket),
    /// AF_UNIX socket before connect (then it becomes a Channel), or bound / listening.
    Unix(UnixSocket),
    /// /dev/zero (and /dev/full): reads zeros, writes are discarded.
    Zero,
    /// O_PATH descriptor of a symbolic link (O_PATH | O_NOFOLLOW): only names the link
    /// (fstat, *at calls, /proc/self/fd); its location is in OpenFile::at.
    Path,
    /// A regular file read into memory (NovaFS) or used in place (system files); `at`
    /// is where it is written back.
    File { path: String, at: vfs::At, data: FileData, writable: bool, append: bool, dirty: bool },
    Dir { at: vfs::At },
    /// A Linux driver's file: a DRM device node, a dma-buf, a sync file (lxfile.rs).
    #[cfg(feature = "linux-drivers")]
    Linux(lxfile::LxFile),
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

pub struct UnixSocket {
    kind: ipc::SockType,
    /// Bound (and after listen(), listening) socket.
    listener: Option<ipc::ListenerRef>,
    /// The sockaddr_un it was bound to, for getsockname.
    name: Vec<u8>,
}

/// The registry key and sockaddr for a sockaddr_un: abstract names ("\0name") as
/// "@name", paths by the file system that holds them (so bind mounts reach the same
/// socket).
fn unix_key(sa: &[u8]) -> Result<(String, bool), i64> {
    if sa.len() < 3 || u16::from_le_bytes([sa[0], sa[1]]) != 1 {
        return Err(EINVAL);
    }
    let path = &sa[2..];
    if path[0] == 0 {
        return Ok((format!("@{}", String::from_utf8_lossy(&path[1..])), true));
    }
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    let p = resolve(&String::from_utf8_lossy(&path[..end]));
    Ok((path_key(&p), false))
}

/// The registry key for socket path `p` in the initial namespace (kernel servers such
/// as the Wayland compositor bind there).
pub fn root_socket_key(p: &str) -> String {
    let (m, rel) = vfs::root().find_mount(p);
    format!("{:p}:{rel}", Arc::as_ptr(&m.fs) as *const u8)
}

/// What descriptor `fd` of process `p` is (for `ps`), if its table is free to look at.
pub fn describe_fd(p: &Process, fd: u64) -> String {
    let Some(t) = p.fds.try_lock() else { return String::from("?") };
    match t.fds.get(fd as usize).and_then(|f| f.clone()) {
        Some(f) => f.try_lock().map_or(String::from("?"), |f| kind_name(&f.kind)),
        None => String::from("-"),
    }
}

/// Where the other end of socket/pipe `fd` of process `pid` is held: "pid:fd" pairs
/// (debugging: the `peer` shell command).
pub fn find_peer(pid: u64, fd: u64) -> String {
    let Some(p) = process::find(pid) else { return String::from("no such process") };
    let Some(file) = p.fds.lock().fds.get(fd as usize).and_then(|f| f.clone()) else { return String::from("no such descriptor") };
    let (rx, tx) = match &file.lock().kind {
        Kind::Channel(e) => (e.rx.as_ref().map(|c| Arc::as_ptr(c) as usize), e.tx.as_ref().map(|c| Arc::as_ptr(c) as usize)),
        _ => return String::from("not a connected socket or pipe"),
    };
    let mut out = Vec::new();
    for q in process::processes() {
        let Some(t) = q.fds.try_lock() else { continue };
        for (n, f) in t.fds.iter().enumerate() {
            let Some(f) = f else { continue };
            if Arc::ptr_eq(f, &file) {
                continue;
            }
            if let Some(f) = f.try_lock()
                && let Kind::Channel(e) = &f.kind
            {
                let (r2, t2) = (e.rx.as_ref().map(|c| Arc::as_ptr(c) as usize), e.tx.as_ref().map(|c| Arc::as_ptr(c) as usize));
                if (r2.is_some() && r2 == tx) || (t2.is_some() && t2 == rx) {
                    out.push(format!("{}:{n}", q.pid));
                }
            }
        }
    }
    if out.is_empty() { String::from("no other end held by any process") } else { out.join(" ") }
}

/// (effective uid, gid) of the calling process, for the owner of files it creates; root
/// for the kernel's own tasks.
pub fn creator_ids() -> (u32, u32) {
    match task::current_user() {
        Some(u) => {
            let c = u.process.cred.lock();
            (c.euid, c.egid)
        }
        None => (0, 0),
    }
}

/// Named pipes: the channel of each FIFO node, by file system and path.
static FIFOS: SpinLock<BTreeMap<String, ipc::ChannelRef>> = SpinLock::new(BTreeMap::new());

/// Open a FIFO: the reading end, the writing end, or both (O_RDWR), of its one channel.
fn open_fifo(m: &vfs::Mount, rel: &str, acc: u64) -> Kind {
    let key = format!("{:p}:{rel}", Arc::as_ptr(&m.fs) as *const u8);
    let chan = FIFOS.lock().entry(key).or_insert_with(ipc::channel).clone();
    {
        // A new reader or writer reopens the pipe after the last one closed it.
        let mut c = chan.lock();
        if acc != 1 {
            c.reader_closed = false;
        }
        if acc != 0 {
            c.writer_closed = false;
        }
    }
    let rx = (acc != 1).then(|| chan.clone());
    let tx = (acc != 0).then_some(chan);
    Kind::Channel(ipc::End { rx, tx, kind: ipc::SockType::Stream, inet: None })
}

/// The shared-memory object behind a descriptor (memfd, POSIX shm, tmpfs file).
pub fn shared_memory(f: &FileRef) -> Option<ipc::SharedRef> {
    match &f.lock().kind {
        Kind::Memfd(m, _) => Some(m.clone()),
        _ => None,
    }
}

/// A new read-only memfd holding `data` (to hand to a program, e.g. a keymap).
pub fn memfd_with(data: &[u8]) -> FileRef {
    let m = ipc::memfd();
    m.lock().write(0, data);
    new_file(Kind::Memfd(m, None), 0)
}

fn path_key(p: &str) -> String {
    let (m, rel) = ns().find_mount(p);
    format!("{:p}:{rel}", Arc::as_ptr(&m.fs) as *const u8)
}

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

/// OpenFile flag (above the O_ flags): SO_PASSCRED is set on this socket.
const PASSCRED: u64 = 1 << 40;

/// ENOTSOCK unless `file` is a socket (programs fall back from send() to write() on it).
fn need_socket(file: &FileRef) -> Result<(), i64> {
    match &file.lock().kind {
        Kind::Inet(_) | Kind::Unix(_) => Ok(()),
        Kind::Channel(e) if e.rx.is_some() && e.tx.is_some() => Ok(()),
        _ => Err(ENOTSOCK),
    }
}

/// Nova's loopback TCP sockets and their owners, one per line:
/// "pid ppid uid fd local remote state" (read by /bin/lsof; Linux programs such as Steam
/// ask lsof which process is at the other end of a local connection).
fn loopback_table() -> String {
    let mut out = String::new();
    for p in process::processes() {
        let Some(t) = p.fds.try_lock() else { continue };
        let ppid = p.parent.load(core::sync::atomic::Ordering::Relaxed);
        let uid = { let c = p.cred.lock(); c.show(c.euid, false) };
        for (n, f) in t.fds.iter().enumerate() {
            let Some(f) = f else { continue };
            let Some(f) = f.try_lock() else { continue };
            let line = match &f.kind {
                Kind::Channel(ipc::End { inet: Some((local, peer)), .. }) => format!("127.0.0.1:{local} 127.0.0.1:{peer} ESTABLISHED"),
                Kind::Unix(UnixSocket { listener: Some(_), name, .. }) if name.len() >= 8 && name[0] == 2 => {
                    let port = u16::from_be_bytes([name[2], name[3]]);
                    format!("{}.{}.{}.{}:{port} *:* LISTEN", name[4], name[5], name[6], name[7])
                }
                _ => continue,
            };
            out.push_str(&format!("{} {ppid} {uid} {n} {line}\n", p.pid));
        }
    }
    out
}

/// A free loopback TCP port (ephemeral range).
fn loopback_port() -> u16 {
    static NEXT: core::sync::atomic::AtomicU16 = core::sync::atomic::AtomicU16::new(32768);
    loop {
        let p = NEXT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        if p < 32768 {
            NEXT.store(32768, core::sync::atomic::Ordering::Relaxed);
            continue;
        }
        if !ipc::name_bound(&format!("tcp:{p}")) {
            return p;
        }
    }
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
    // A Linux driver's file: its f_op->poll (without the OpenFile lock; it may sleep).
    #[cfg(feature = "linux-drivers")]
    if let Some(r) = lxfile::poll(f) {
        return r;
    }
    let f = f.lock();
    match &f.kind {
        Kind::Channel(end) => end.poll(),
        Kind::EventFd(e) => e.lock().poll(),
        Kind::TimerFd(t) => t.lock().poll(),
        Kind::Inotify(i) => i.lock().poll(),
        Kind::SignalFd(mask) => (if signal::pending_masked(thread(), *mask) { ipc::POLLIN } else { 0 }, 0),
        Kind::Unix(UnixSocket { listener: Some(l), .. }) => l.lock().poll(),
        Kind::Unix(_) => (0, 0),
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
    /// Slots Linux code reserved (get_unused_fd_flags) and fills next (fd_install),
    /// with their close-on-exec flag.
    reserved: BTreeMap<usize, bool>,
    /// Descriptors closed by execve (O_CLOEXEC / FD_CLOEXEC).
    cloexec: alloc::collections::BTreeSet<usize>,
    pub cwd: String,
    /// Typed-ahead terminal input not yet consumed by read().
    stdin: Vec<u8>,
}

impl FdTable {
    pub fn new() -> Self {
        let tty = || Some(Arc::new(SpinLock::new(OpenFile { kind: Kind::Tty, pos: 0, flags: 2, at: None })));
        Self { fds: vec![tty(), tty(), tty()], reserved: BTreeMap::new(), cloexec: Default::default(), cwd: String::new(), stdin: Vec::new() }
    }

    /// fork: the child shares every open file (offsets included), as on Linux.
    pub fn fork(&self) -> Self {
        Self { fds: self.fds.clone(), reserved: BTreeMap::new(), cloexec: self.cloexec.clone(), cwd: self.cwd.clone(), stdin: Vec::new() }
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

    pub fn get(&self, fd: i64) -> Result<FileRef, i64> {
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

    /// get_unused_fd_flags() for Linux code: the lowest free slot, held until
    /// [`FdTable::fill_reserved`] or [`FdTable::unreserve`].
    #[cfg(feature = "linux-drivers")]
    fn reserve(&mut self, cloexec: bool) -> Option<usize> {
        let slot = (0..).find(|&i| (i >= self.fds.len() || self.fds[i].is_none()) && !self.reserved.contains_key(&i))?;
        self.reserved.insert(slot, cloexec);
        Some(slot)
    }

    #[cfg(feature = "linux-drivers")]
    fn unreserve(&mut self, fd: usize) {
        self.reserved.remove(&fd);
    }

    /// fd_install() for Linux code.
    #[cfg(feature = "linux-drivers")]
    fn fill_reserved(&mut self, fd: usize, f: FileRef) {
        let cloexec = self.reserved.remove(&fd).unwrap_or(false);
        self.install(fd, f);
        self.set_cloexec(fd as i64, cloexec);
    }

    fn add(&mut self, f: FileRef, min: usize) -> i64 {
        let slot = (min..).find(|&i| (i >= self.fds.len() || self.fds[i].is_none()) && !self.reserved.contains_key(&i)).unwrap();
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
        inotify::notify(path, inotify::IN_MODIFY, false);
        inotify::notify(path, inotify::IN_CLOSE_WRITE, false);
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
    let mut v = Vec::new();
    v.try_reserve_exact(len).map_err(|_| ENOMEM_ERR)?;
    v.resize(len, 0);
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
    if let Some(n) = &thread().ns_override {
        return n.clone();
    }
    proc().ns.lock().clone()
}

/// The path of directory `at` for a lookup of `p` relative to it. A directory outside
/// the caller's root (a descriptor opened before chroot) still works, as on Linux: the
/// rest of this system call then resolves paths inside that directory alone.
fn dir_base(at: &vfs::At) -> String {
    match ns().vpath(at) {
        Some(p) => p,
        None => {
            thread().ns_override = Some(vfs::Namespace::rooted_at(at));
            String::new()
        }
    }
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
    b[40..48].copy_from_slice(&m.rdev.to_le_bytes());
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
    if let Some((pid, file)) = name.strip_prefix("proc/").and_then(|r| r.split_once('/'))
        && let Ok(pid) = pid.parse::<u64>()
    {
        let p = process::find(pid)?;
        return proc_pid_file(&p, file);
    }
    if let Some(file) = name.strip_prefix("proc/self/")
        && matches!(file, "stat" | "cmdline")
    {
        return proc_pid_file(proc(), file);
    }
    let (used, free) = crate::memory::heap_usage();
    let total_kb = (used + free) / 1024;
    Some(match name.as_str() {
        "proc/self/maps" => mem().maps(),
        "proc/self/status" => format!(
            "Name:\t{}\nState:\tR (running)\nTgid:\t{p}\nPid:\t{p}\nPPid:\t{pp}\nUid:\t{u}\t{u}\t{u}\t{u}\nGid:\t{g}\t{g}\t{g}\t{g}\nThreads:\t{}\nVmRSS:\t{} kB\nCpus_allowed:\t1\nCpus_allowed_list:\t0\n{}",
            proc().name,
            proc().threads.load(core::sync::atomic::Ordering::Relaxed),
            used / 1024,
            seccomp_status(proc().pid),
            p = proc().pid,
            pp = proc().parent.load(core::sync::atomic::Ordering::Relaxed),
            u = { let c = proc().cred.lock(); c.show(c.uid, false) },
            g = { let c = proc().cred.lock(); c.show(c.gid, true) }
        ),
        "proc/self/stat" => format!("{} ({}) R 1 {} {} 0 -1 0 0 0 0 0 0 0 0 0 20 0 {} 0 0 0 0\n", proc().pid, proc().name, proc().pid, proc().pid, proc().threads.load(core::sync::atomic::Ordering::Relaxed)),
        "proc/self/statm" => "1000 500 100 10 0 400 0\n".into(),
        "proc/self/cmdline" => {
            let argv = proc().argv.lock();
            if argv.is_empty() { format!("/{}\0", *proc().exe.lock()) } else { argv.iter().map(|a| format!("{a}\0")).collect() }
        }
        "proc/self/comm" => format!("{}\n", proc().name),
        "proc/self/oom_score_adj" => "0\n".into(),
        "proc/self/limits" => "Limit                     Soft Limit           Hard Limit           Units\nMax open files            1024                 4096                 files\n".into(),
        "proc/mounts" | "proc/self/mounts" => ns().mounts(),
        "proc/self/mountinfo" => ns().mountinfo(),
        "proc/self/uid_map" | "proc/self/gid_map" => match &proc().cred.lock().userns {
            None => "         0          0 4294967295\n".into(),
            Some((u, g)) => (if name.ends_with("gid_map") { g } else { u }).iter().map(|(i, o, n)| format!("{i:>10} {o:>10} {n:>10}\n")).collect(),
        },
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
        "proc/nova/tcp" => loopback_table(),
        "proc/nova/net" => {
            let ((rx, tx), (polls, frames)) = (crate::net::counters(), crate::net::poll_counters());
            format!("rx_bytes {rx}\ntx_bytes {tx}\nrx_frames {frames}\npolls {polls}\nuptime_ms {}\n", pit::ticks() * 1000 / pit::HZ)
        }
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
    vfs::Meta { ty, perm, size, mtime: 0, ino: ino_of(name), nlink: if ty == vfs::Type::Dir { 2 } else { 1 }, uid: 0, gid: 0, rdev: 0 }
}

/// /proc paths of any process ("<pid>/...", "thread-self/...") as "self/...": every
/// process sees its own data (enough for programs that look up their own pid).
/// /proc/<pid> files that describe that process (not the caller).
const PER_PROCESS: &[&str] = &["stat", "status", "cmdline", "comm"];

/// /proc/<pid>/{stat,status,cmdline,comm} of `p`.
fn proc_pid_file(p: &Process, file: &str) -> Option<String> {
    let comm: String = p.exe.lock().rsplit('/').next().unwrap_or("").chars().take(15).collect();
    let ppid = p.parent.load(core::sync::atomic::Ordering::Relaxed);
    let threads = p.threads.load(core::sync::atomic::Ordering::Relaxed);
    let (uid, gid) = { let c = p.cred.lock(); (c.show(c.uid, false), c.show(c.gid, true)) };
    Some(match file {
        "stat" => format!("{} ({comm}) R {ppid} {} {} 0 -1 0 0 0 0 0 0 0 0 0 20 0 {threads} 0 0 0 0\n", p.pid, p.pid, p.pid),
        "status" => format!(
            "Name:\t{comm}\nState:\tR (running)\nTgid:\t{pid}\nPid:\t{pid}\nPPid:\t{ppid}\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t{gid}\t{gid}\t{gid}\t{gid}\nThreads:\t{threads}\nVmRSS:\t0 kB\nCpus_allowed:\t1\nCpus_allowed_list:\t0\n{}",
            seccomp_status(p.pid),
            pid = p.pid
        ),
        "cmdline" => {
            let argv = p.argv.lock();
            if argv.is_empty() { format!("/{}\0", *p.exe.lock()) } else { argv.iter().map(|a| format!("{a}\0")).collect() }
        }
        "comm" => format!("{comm}\n"),
        _ => return None,
    })
}

/// status's NoNewPrivs/Seccomp lines: the calling thread's for its own process, the
/// main thread's for another.
fn seccomp_status(pid: u64) -> String {
    if pid == proc().pid {
        return seccomp::status_lines(thread().no_new_privs, &thread().seccomp);
    }
    let mut out = None;
    task::each_thread(pid, |t| {
        if out.is_none() || t.tid == pid {
            out = Some(seccomp::status_lines(t.no_new_privs, &t.seccomp));
        }
    });
    out.unwrap_or_default()
}

/// clone with CLONE_FS: the other processes that share the caller's root and working
/// directory.
fn fs_sharers() -> Vec<Arc<Process>> {
    let Some(token) = proc().fs_share.lock().clone() else { return Vec::new() };
    process::processes().into_iter().filter(|q| q.pid != proc().pid && q.fs_share.lock().as_ref().is_some_and(|t| Arc::ptr_eq(t, &token))).collect()
}

fn proc_norm(rel: &str) -> String {
    let (first, rest) = rel.split_once('/').unwrap_or((rel, ""));
    let is_pid = !first.is_empty() && first.bytes().all(|b| b.is_ascii_digit()) && process::exists(first.parse().unwrap_or(0));
    // Another process's identity files describe that process (ps, lsof, and Steam
    // walking parent pids need them); its other files still stand in with our own.
    if is_pid && first.parse::<u64>().ok() != Some(proc().pid) && PER_PROCESS.contains(&rest) {
        return rel.into();
    }
    if first == "thread-self" || is_pid {
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
        Kind::Path => "/".into(), // fd_link returns OpenFile::at's path first
        Kind::Tty => "/dev/tty".into(),
        Kind::Null => "/dev/null".into(),
        Kind::Zero => "/dev/zero".into(),
        Kind::Random => "/dev/urandom".into(),
        Kind::Channel(_) => "socket:[1]".into(),
        Kind::EventFd(_) => "anon_inode:[eventfd]".into(),
        Kind::TimerFd(_) => "anon_inode:[timerfd]".into(),
        Kind::Inotify(_) => "anon_inode:inotify".into(),
        Kind::SignalFd(_) => "anon_inode:[signalfd]".into(),
        Kind::Epoll(_) => "anon_inode:[eventpoll]".into(),
        Kind::Memfd(_, None) => "/memfd: (deleted)".into(),
        Kind::Inet(_) | Kind::Unix(_) => "socket:[2]".into(),
        #[cfg(feature = "linux-drivers")]
        Kind::Linux(l) => lxfile::name(l),
    })
}

const PROC_DIRS: &[&str] = &["", "self", "self/fd", "self/fdinfo", "self/ns", "self/task", "sys", "sys/kernel", "sys/kernel/yama", "sys/vm", "sys/fs", "sys/fs/inotify", "sys/user"];
const PROC_ROOT: &[&str] = &["self", "thread-self", "cpuinfo", "meminfo", "mounts", "stat", "uptime", "loadavg", "filesystems", "sys"];
const PROC_SELF: &[&str] = &["exe", "root", "cwd", "fd", "fdinfo", "ns", "maps", "status", "stat", "statm", "cmdline", "comm", "mountinfo", "mounts", "limits", "oom_score_adj", "uid_map", "gid_map", "setgroups", "cgroup"];
const NAMESPACES: &[&str] = &["cgroup", "ipc", "mnt", "net", "pid", "user", "uts"];

struct ProcFs;

impl vfs::Backend for ProcFs {
    fn kind(&self) -> &'static str {
        "proc"
    }
    fn lstat(&self, p: &str) -> Result<vfs::Meta, i64> {
        let n = proc_norm(p);
        if PROC_DIRS.contains(&n.as_str()) || (p.split('/').next().is_some_and(|f| f.bytes().all(|b| b.is_ascii_digit())) && n == "self") {
            let mut m = meta(vfs::Type::Dir, 0o555, 0, p);
            if n == "self/task" {
                // ".", ".." and one entry per thread: programs count threads this way
                // (Chromium's sandbox wants exactly 3 before it forks).
                m.nlink = 2 + proc().threads.load(core::sync::atomic::Ordering::Relaxed) as u32;
            }
            return Ok(m);
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
        if n == "self/root" {
            return Some("/".into());
        }
        if n == "self/cwd" {
            return Some(format!("/{}", fds().cwd));
        }
        let fd: i64 = n.strip_prefix("self/fd/")?.parse().ok()?;
        let file = fds().get(fd).ok()?;
        let f = file.lock();
        let at = match (&f.at, &f.kind) {
            (_, Kind::Channel(_) | Kind::Inet(_) | Kind::Unix(_) | Kind::EventFd(_) | Kind::TimerFd(_) | Kind::Inotify(_) | Kind::Epoll(_) | Kind::SignalFd(_) | Kind::Memfd(_, None)) => return None,
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
            // Every running process, as on Linux (ps, lsof and process-tree walkers list it).
            "" => PROC_ROOT.iter().map(|s| s.to_string()).chain(process::processes().iter().map(|q| q.pid.to_string())).collect(),
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
    fn write_back(&self, p: &str, data: &[u8]) -> Result<(), i64> {
        // /proc/<pid>/uid_map and gid_map: "inside outside count" lines for a process in a
        // user namespace.
        let (first, file) = p.split_once('/').ok_or(EACCES)?;
        let pid = if first == "self" || first == "thread-self" { proc().pid } else { first.parse().map_err(|_| EACCES)? };
        let gid = match file {
            "uid_map" => false,
            "gid_map" => true,
            "setgroups" | "oom_score_adj" => return Ok(()),
            _ => return Err(EACCES),
        };
        let map: Vec<(u32, u32, u32)> = String::from_utf8_lossy(data)
            .lines()
            .filter_map(|l| {
                let v: Vec<u32> = l.split_whitespace().filter_map(|w| w.parse().ok()).collect();
                (v.len() == 3).then(|| (v[0], v[1], v[2]))
            })
            .collect();
        let p = process::find(pid).ok_or(ESRCH)?;
        let mut c = p.cred.lock();
        // A nested namespace's "outside" ids are the enclosing namespace's: translate them.
        let map: Vec<(u32, u32, u32)> = match &c.parent_maps {
            Some((pu, pg)) => {
                let parent = if gid { pg } else { pu };
                map.into_iter()
                    .map(|(i, o, n)| (i, parent.iter().find(|&&(pi, _, pn)| o >= pi && o - pi < pn).map_or(o, |&(pi, po, _)| po + (o - pi)), n))
                    .collect()
            }
            None => map,
        };
        let ns = c.userns.get_or_insert_with(|| (Vec::new(), Vec::new()));
        if gid { ns.1 = map } else { ns.0 = map }
        Ok(())
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
        // Linux drivers' device nodes: /dev/dri and its card and render nodes.
        #[cfg(feature = "linux-drivers")]
        {
            if p == "dri" && !lxfile::dev_entries("dri").is_empty() {
                return Ok(meta(vfs::Type::Dir, 0o755, 0, p));
            }
            if let Some((major, minor)) = lxfile::dev_node(p) {
                return Ok(vfs::Meta { rdev: lxfile::encode_dev(major, minor), ..meta(vfs::Type::Chr, 0o666, 0, p) });
            }
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
        #[cfg(feature = "linux-drivers")]
        if p == "dri" {
            return Ok(lxfile::dev_entries("dri").into_iter().map(|(n, _, _)| (n.clone(), vfs::Type::Chr, ino_of(&format!("dri/{n}")))).collect());
        }
        Ok(match p {
            "" => {
                let mut v: Vec<(String, vfs::Type, u64)> = DEV_NODES
                    .iter()
                    .map(|n| (n.to_string(), vfs::Type::Chr, ino_of(n)))
                    .chain(DEV_LINKS.iter().map(|(n, _)| (n.to_string(), vfs::Type::Lnk, ino_of(n))))
                    .chain(["shm", "pts"].iter().map(|n| (n.to_string(), vfs::Type::Dir, ino_of(n))))
                    .collect();
                #[cfg(feature = "linux-drivers")]
                if !lxfile::dev_entries("dri").is_empty() {
                    v.push(("dri".into(), vfs::Type::Dir, ino_of("dri")));
                }
                v
            }
            "shm" => SHM.lock().keys().map(|n| (n.clone(), vfs::Type::Reg, ino_of(n))).collect(),
            "pts" => Vec::new(),
            _ => return Err(ENOTDIR),
        })
    }
    fn open(&self, p: &str) -> Result<vfs::Content, i64> {
        #[cfg(feature = "linux-drivers")]
        if let Some((major, minor)) = lxfile::dev_node(p) {
            return Ok(vfs::Content::Linux(major, minor));
        }
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
        // The DRM devices of Linux drivers (/sys/class/drm, /sys/dev/char, their parents).
        #[cfg(feature = "linux-drivers")]
        if let Some(n) = lxfile::sys_node(p) {
            return Ok(match n {
                lxfile::SysNode::Dir => meta(vfs::Type::Dir, 0o755, 0, p),
                lxfile::SysNode::File(b) => meta(vfs::Type::Reg, 0o444, b.len() as u64, p),
                lxfile::SysNode::Link(t) => meta(vfs::Type::Lnk, 0o777, t.len() as u64, p),
            });
        }
        if p.is_empty() || SYS_FILES.iter().any(|f| f.starts_with(p) && f.as_bytes().get(p.len()) == Some(&b'/')) {
            return Ok(meta(vfs::Type::Dir, 0o555, 0, p));
        }
        Err(ENOENT)
    }
    fn link_target(&self, _p: &str) -> Option<String> {
        #[cfg(feature = "linux-drivers")]
        if let Some(lxfile::SysNode::Link(t)) = lxfile::sys_node(_p) {
            return Some(t);
        }
        None
    }
    fn list(&self, p: &str) -> Result<Vec<(String, vfs::Type, u64)>, i64> {
        let prefix = if p.is_empty() { String::new() } else { format!("{p}/") };
        let mut names = BTreeMap::new();
        #[cfg(feature = "linux-drivers")]
        for (n, t) in lxfile::sys_list(p) {
            names.insert(n, t);
        }
        for f in SYS_FILES.iter().filter_map(|f| f.strip_prefix(prefix.as_str())) {
            match f.split_once('/') {
                Some((d, _)) => names.insert(d.to_string(), vfs::Type::Dir),
                None => names.insert(f.to_string(), vfs::Type::Reg),
            };
        }
        Ok(names.into_iter().map(|(n, t)| (n.clone(), t, ino_of(&n))).collect())
    }
    fn open(&self, p: &str) -> Result<vfs::Content, i64> {
        #[cfg(feature = "linux-drivers")]
        if let Some(lxfile::SysNode::File(b)) = lxfile::sys_node(p) {
            return Ok(vfs::Content::Copy(b));
        }
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
        let file = fds().get(dirfd)?;
        let f = file.lock();
        if let Some(at) = f.at.as_ref().or(match &f.kind {
            Kind::Dir { at } => Some(at),
            _ => None,
        }) {
            let at = at.clone();
            drop(f);
            let base = dir_base(&at);
            return Ok(resolve(&format!("/{base}/{p}")));
        }
    }
    Ok(resolve(p))
}

/// [`at_path`], following a symbolic link in the last component only if `follow`.
fn at_path_nf(dirfd: i64, p: &str, follow: bool) -> Result<String, i64> {
    let p = if dirfd != AT_FDCWD && !p.starts_with('/') {
        let file = fds().get(dirfd)?;
        let f = file.lock();
        match f.at.as_ref().or(match &f.kind {
            Kind::Dir { at } => Some(at),
            _ => None,
        }) {
            Some(at) => {
                let at = at.clone();
                drop(f);
                format!("/{}/{p}", dir_base(&at))
            }
            None => String::from(p),
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
    // stx_rdev_major, stx_rdev_minor from st_rdev (Linux's new_encode_dev)
    let rdev = q(40);
    put(128, &(((rdev >> 8) & 0xFFF) as u32).to_le_bytes());
    put(132, &((rdev & 0xFF | (rdev >> 12) & !0xFF) as u32).to_le_bytes());
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
        Kind::Path => match &f.at {
            Some(at) => at.mount.fs.lstat(&at.rel).map(|m| stat_meta(&m)).unwrap_or_else(|_| stat_buf(0o120777, 0, 0, 8)),
            None => stat_buf(0o120777, 0, 0, 8),
        },
        Kind::Tty => stat_buf(0o20620, 0, 0, 1),
        Kind::Null | Kind::Random | Kind::Zero => stat_buf(0o20666, 0, 0, 1),
        Kind::File { path, data, at, .. } => match at.mount.fs.lstat(&at.rel) {
            Ok(meta) => stat_meta(&vfs::Meta { size: data.len() as u64, ..meta }),
            Err(_) => stat_buf(0o100644, data.len() as u64, rtc::unix_time(), ino_of(path)),
        },
        Kind::Dir { at } => at.mount.fs.lstat(&at.rel).map(|m| stat_meta(&m)).unwrap_or_else(|_| stat_buf(0o40755, 4096, 0, 2)),
        Kind::Channel(e) => stat_buf(if e.rx.is_some() && e.tx.is_some() { 0o140777 } else { 0o10600 }, 0, 0, 3),
        Kind::EventFd(_) | Kind::TimerFd(_) | Kind::Inotify(_) | Kind::Epoll(_) | Kind::SignalFd(_) => stat_buf(0o600, 0, 0, 4),
        Kind::Memfd(m, Some(at)) => match at.mount.fs.lstat(&at.rel) {
            Ok(meta) => stat_meta(&vfs::Meta { size: m.lock().size, ..meta }),
            Err(_) => stat_buf(0o100644, m.lock().size, 0, 7),
        },
        Kind::Memfd(m, None) => stat_buf(0o100600, m.lock().size, 0, 5),
        Kind::Inet(_) | Kind::Unix(_) => stat_buf(0o140777, 0, 0, 6),
        #[cfg(feature = "linux-drivers")]
        Kind::Linux(l) => lxfile::stat(l),
    }
}

fn open(path: &str, flags: u64, mode: u32) -> Result<FileRef, i64> {
    // O_PATH only names the file: the access mode, O_CREAT and O_TRUNC are ignored.
    let flags = if flags & O_PATH != 0 { flags & (O_PATH | O_CLOEXEC | O_NOFOLLOW | 0o200000) } else { flags };
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
                    inotify::notify(&name, inotify::IN_CREATE, false);
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
                vfs::Type::Lnk if flags & O_PATH != 0 => Kind::Path,
                vfs::Type::Lnk => return Err(ELOOP), // O_NOFOLLOW on a link
                vfs::Type::Fifo => open_fifo(&m, &rel, acc),
                vfs::Type::Chr => match m.fs.open(&rel)? {
                    #[cfg(feature = "linux-drivers")]
                    vfs::Content::Linux(major, minor) => lxfile::open(major, minor, flags)?,
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
                        #[cfg(feature = "linux-drivers")]
                        vfs::Content::Linux(..) => return Err(EINVAL),
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
/// recv with MSG_WAITALL on a stream socket: keep reading until `len` bytes (or the end of
/// the stream); other sockets and non-blocking reads return what is there.
fn channel_read_flags(file: &FileRef, len: usize, flags: u64) -> Result<(Vec<u8>, Vec<FileRef>, bool), i64> {
    let dontwait = flags & 0x40 != 0;
    let stream = matches!(&file.lock().kind, Kind::Channel(e) if e.kind == ipc::SockType::Stream);
    if flags & 0x100 == 0 || dontwait || !stream {
        return channel_read(file, len, false, dontwait);
    }
    let (mut data, mut fds) = (Vec::new(), Vec::new());
    while data.len() < len {
        match channel_read(file, len - data.len(), false, false) {
            Ok((d, f, _)) => {
                if d.is_empty() {
                    break; // end of stream
                }
                data.extend_from_slice(&d);
                fds.extend(f);
            }
            Err(e) if data.is_empty() => return Err(e),
            Err(_) => break, // e.g. EINTR after part of the data: return that part
        }
    }
    Ok((data, fds, false))
}

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
                        c.push(ipc::Message { data: data.to_vec(), fds: fds.take().unwrap_or_default(), pid: proc().pid as u32 });
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

/// read() on a timerfd: the number of expirations since the last read, as a u64.
fn timerfd_read(f: &FileRef, t: &Arc<SpinLock<ipc::TimerFd>>, len: usize) -> Result<Vec<u8>, i64> {
    if len < 8 {
        return Err(EINVAL);
    }
    loop {
        {
            let mut t = t.lock();
            t.update();
            if t.pending > 0 {
                let n = core::mem::take(&mut t.pending);
                return Ok(n.to_le_bytes().to_vec());
            }
        }
        if f.lock().flags & O_NONBLOCK != 0 {
            return Err(EAGAIN);
        }
        ipc::wait(None);
        process::check_exiting();
    }
}

/// An itimerspec (interval, value) as nanoseconds.
fn read_itimerspec(addr: u64) -> Result<(u64, u64), i64> {
    let b = read_user(addr, 32)?;
    let ts = |o: usize| -> Result<u64, i64> {
        let (sec, ns) = (i64::from_le_bytes(b[o..o + 8].try_into().unwrap()), i64::from_le_bytes(b[o + 8..o + 16].try_into().unwrap()));
        if sec < 0 || !(0..1_000_000_000).contains(&ns) {
            return Err(EINVAL);
        }
        Ok((sec as u64).saturating_mul(1_000_000_000).saturating_add(ns as u64))
    };
    Ok((ts(0)?, ts(16)?))
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
    let timer = match &f.lock().kind {
        Kind::TimerFd(t) => Some(t.clone()),
        _ => None,
    };
    if let Some(t) = timer {
        return timerfd_read(f, &t, len);
    }
    let watcher = match &f.lock().kind {
        Kind::Inotify(i) => Some(i.clone()),
        _ => None,
    };
    if let Some(i) = watcher {
        loop {
            if let Some(out) = i.lock().read(len)? {
                return Ok(out);
            }
            if f.lock().flags & O_NONBLOCK != 0 {
                return Err(EAGAIN);
            }
            ipc::wait(None);
            process::check_exiting();
        }
    }
    if is_channel(f) {
        let (data, fds, _) = channel_read(f, len, false, false)?;
        drop(fds); // plain read() discards passed descriptors
        return Ok(data);
    }
    if is_inet(f) {
        return inet_recv(f, len, false, false).map(|(d, _)| d);
    }
    // A Linux driver's file (DRM events): its read, into a kernel buffer (copy_to_user
    // takes kernel pointers too).
    #[cfg(feature = "linux-drivers")]
    if lxfile::is_linux(f) {
        let mut buf = vec![0u8; len.min(1 << 20)];
        let n = lxfile::read(f, buf.as_mut_ptr() as u64, buf.len() as u64).unwrap_or(Err(EINVAL))?;
        buf.truncate(n as usize);
        return Ok(buf);
    }
    let mut f = f.lock();
    let pos = at.unwrap_or(f.pos);
    let out = match &f.kind {
        Kind::Null => Vec::new(),
        Kind::Path => return Err(EBADF),
        // Short reads are fine for these, and keep a huge count from sizing a kernel buffer.
        Kind::Zero => vec![0; len.min(1 << 20)],
        Kind::Random => {
            let mut v = vec![0; len.min(1 << 20)];
            rand::fill(&mut v);
            v
        }
        Kind::File { data, .. } => data.get(pos..).map(|d| d[..len.min(d.len())].to_vec()).unwrap_or_default(),
        Kind::Dir { .. } => return Err(EISDIR),
        Kind::Memfd(m, _) => m.lock().read(pos, len),
        Kind::Epoll(_) | Kind::Inet(_) => return Err(EINVAL),
        Kind::Unix(_) => return Err(ENOTCONN),
        Kind::Tty | Kind::Channel(_) | Kind::EventFd(_) | Kind::TimerFd(_) | Kind::Inotify(_) | Kind::SignalFd(_) => unreachable!(),
        #[cfg(feature = "linux-drivers")]
        Kind::Linux(_) => unreachable!(),
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
    #[cfg(feature = "linux-drivers")]
    if let Some(r) = lxfile::write(f, data.as_ptr() as u64, data.len() as u64) {
        return r.map(|n| n as usize);
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
        Kind::Path => return Err(EBADF),
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
        Kind::Epoll(_) | Kind::Channel(_) | Kind::EventFd(_) | Kind::TimerFd(_) | Kind::Inotify(_) | Kind::Inet(_) | Kind::SignalFd(_) => return Err(EINVAL),
        #[cfg(feature = "linux-drivers")]
        Kind::Linux(_) => return Err(EINVAL),
        Kind::Unix(_) => return Err(ENOTCONN),
        Kind::File { data: contents, writable, append, dirty, .. } => {
            if !*writable {
                return Err(EBADF);
            }
            let start = if *append { contents.len() } else { pos };
            let contents = contents.to_mut();
            if contents.len() < start + data.len() {
                vfs::resize_file(contents, (start + data.len()) as u64)?;
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
    thread().wake_at = end;
    thread().phase = "sleep";
    loop {
        // One reading of the clock: checking it and then subtracting a second reading
        // could underflow (the deadline passing in between) into an endless sleep.
        let now = pit::nanos();
        if now >= end {
            break;
        }
        process::check_exiting(); // exit_group or execve by another thread
        task::sleep_ticks((end - now).div_ceil(1_000_000_000 / pit::HZ).max(1));
    }
    thread().phase = "slept";
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
    if thread().seccomp.mode != seccomp::MODE_DISABLED
        && let Some(r) = seccomp::check(f.rip, nr, [a0, a1, a2, a3, a4, a5], false)
    {
        thread().recent.push(nr, [a0, a1, a2], r);
        return r;
    }
    thread().in_syscall = nr;
    thread().syscall_arg = a0;
    thread().syscall_arg2 = a1;
    let r = dispatch(f, nr, a0, a1, a2, a3, a4, a5);
    thread().ns_override = None;
    // Linux mappings unmapped by this call: their drivers' vm_ops->close, lock-free now.
    #[cfg(feature = "linux-drivers")]
    if lxfile::reap_pending() {
        lxfile::reap();
    }
    thread().recent.push(nr, [a0, a1, a2], r);
    thread().in_syscall = u64::MAX;
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
        Kind::TimerFd(_) => "timerfd".into(),
        Kind::Inotify(_) => "inotify".into(),
        Kind::SignalFd(_) => "signalfd".into(),
        Kind::Path => "path".into(),
        Kind::Unix(_) => "unix socket".into(),
        Kind::Epoll(_) => "epoll".into(),
        Kind::Memfd(_, Some(at)) => format!("file {}", at.rel),
        Kind::Memfd(_, None) => "memfd".into(),
        Kind::Inet(s) => format!("inet {}", if s.udp { "udp" } else { "tcp" }),
        #[cfg(feature = "linux-drivers")]
        Kind::Linux(l) => lxfile::kind_name(l),
    }
}

/// A syscall's name for debugging lists: native numbers, or i386 ones (0x1000 + nr).
pub fn syscall_label(nr: u64) -> String {
    if nr & 0x1000 != 0 { format!("i386 {}", compat32::label(nr & 0xFFF)) } else { syscall_name(nr) }
}

fn syscall_name(nr: u64) -> String {
    let n = match nr {
        0 => "read", 1 => "write", 2 => "open", 3 => "close", 4 => "stat", 5 => "fstat", 6 => "lstat", 7 => "poll",
        8 => "lseek", 9 => "mmap", 10 => "mprotect", 11 => "munmap", 12 => "brk", 13 => "rt_sigaction",
        14 => "rt_sigprocmask", 16 => "ioctl", 17 => "pread64", 19 => "readv", 20 => "writev", 21 => "access",
        22 => "pipe", 23 => "select", 270 => "pselect6", 436 => "close_range", 73 => "flock", 74 => "fsync", 24 => "sched_yield", 25 => "mremap", 28 => "madvise", 32 => "dup", 33 => "dup2",
 39 => "getpid", 40 => "sendfile", 57 => "fork", 58 => "vfork", 59 => "execve", 61 => "wait4", 62 => "kill", 77 => "ftruncate", 87 => "unlink", 84 => "rmdir", 88 => "symlink", 266 => "symlinkat", 86 => "link", 265 => "linkat", 280 => "utimensat", 76 => "truncate", 90 => "chmod", 285 => "fallocate", 41 => "socket", 42 => "connect", 44 => "sendto", 45 => "recvfrom", 46 => "sendmsg", 47 => "recvmsg",
        42 => "connect", 49 => "bind", 53 => "socketpair", 307 => "sendmmsg", 56 => "clone", 60 => "exit", 63 => "uname", 72 => "fcntl", 79 => "getcwd",
        89 => "readlink", 96 => "gettimeofday", 97 => "getrlimit", 99 => "sysinfo", 137 => "statfs", 138 => "fstatfs",
        157 => "prctl", 158 => "arch_prctl", 317 => "seccomp", 186 => "gettid", 202 => "futex", 204 => "sched_getaffinity",
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
                    let at = match &fds().get(dirfd)?.lock().kind {
                        Kind::Dir { at } => Some(at.clone()),
                        _ => None,
                    };
                    if let Some(at) = at {
                        p = format!("/{}/{p}", dir_base(&at));
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
            23 | 270 => {
                // select(n, readfds, writefds, exceptfds, timeval*),
                // pselect6(n, readfds, writefds, exceptfds, timespec*, sigmask)
                let d = if a4 == 0 {
                    None
                } else if nr == 23 {
                    let t = read_user(a4, 16)?;
                    let us = u64::from_le_bytes(t[0..8].try_into().unwrap()) * 1_000_000 + u64::from_le_bytes(t[8..16].try_into().unwrap());
                    Some(pit::ticks() + us.div_ceil(1_000_000 / pit::HZ))
                } else {
                    deadline(a4, false, 1)?
                };
                select_fds(a0 as usize, a1, a2, a3, d)?
            }
            436 => {
                // close_range(first, last, flags): CLOSE_RANGE_CLOEXEC (4) marks instead
                let mut t = fds();
                let last = (a1 as usize).min(t.fds.len().saturating_sub(1));
                let mut closed = Vec::new();
                for fd in (a0 as usize)..=last {
                    if a2 & 4 != 0 {
                        if t.fds.get(fd).is_some_and(|f| f.is_some()) {
                            t.set_cloexec(fd as i64, true);
                        }
                    } else if let Some(slot) = t.fds.get_mut(fd) {
                        if let Some(f) = slot.take() {
                            closed.push(f);
                        }
                        t.cloexec.remove(&fd);
                    }
                }
                drop(t);
                drop(closed); // last references flush on drop
                0
            }
            74 | 75 => {
                // fsync, fdatasync: write a file's changes back now
                let file = fds().get(a0 as i64)?;
                flush(&mut file.lock());
                0
            }
            73 | 306 => 0, // flock (advisory locks: one system, nothing to arbitrate), syncfs
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
                // socket(domain, type, protocol): AF_UNIX, AF_INET TCP and UDP
                if a0 == 1 {
                    let kind = sock_type(a1)?;
                    let mut t = fds();
                    let fd = t.add(new_file(Kind::Unix(UnixSocket { kind, listener: None, name: Vec::new() }), (a1 & O_NONBLOCK) | 2), 0);
                    t.set_cloexec(fd, a1 & O_CLOEXEC != 0);
                    return Ok(fd);
                }
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
                let unix_kind = match &file.lock().kind {
                    Kind::Unix(u) if u.listener.is_none() => Some(u.kind),
                    Kind::Unix(_) | Kind::Channel(_) => return Err(EISCONN),
                    _ => None,
                };
                if let Some(kind) = unix_kind {
                    let sa = read_user(a1, (a2 as usize).min(110))?;
                    let (key, abstract_name) = unix_key(&sa)?;
                    if !abstract_name {
                        let end = sa[2..].iter().position(|&b| b == 0).unwrap_or(sa.len() - 2);
                        stat_path(&resolve(&String::from_utf8_lossy(&sa[2..2 + end])))?;
                    }
                    let end = ipc::connect_name(&key, kind, None)?;
                    file.lock().kind = Kind::Channel(end);
                    return Ok(0);
                }
                let (ip, port) = sockaddr_in(&read_user(a1, (a2 as usize).min(128))?)?;
                if ip.octets()[0] == 127 && matches!(&file.lock().kind, Kind::Inet(s) if !s.udp) {
                    // Loopback TCP: an in-kernel stream to a socket listening on this machine.
                    let local = loopback_port();
                    let end = ipc::connect_name(&format!("tcp:{port}"), ipc::SockType::Stream, Some((local, port)))?;
                    file.lock().kind = Kind::Channel(end);
                    return Ok(0);
                }
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
                need_socket(&file)?;
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
                need_socket(&file)?;
                if is_inet(&file) {
                    // flags: MSG_PEEK 0x2, MSG_DONTWAIT 0x40
                    let (data, from) = inet_recv(&file, a2 as usize, a3 & 0x40 != 0, a3 & 0x2 != 0)?;
                    write_user(a1, &data)?;
                    if let Some((ip, port)) = from {
                        put_addr(a4, a5, &encode_sockaddr_in(ip, port))?;
                    }
                    data.len() as i64
                } else {
                    let (data, _, _) = channel_read_flags(&file, a2 as usize, a3)?;
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
                // bind(fd, addr, len): AF_UNIX names, UDP ports; TCP client sockets ignore it
                let file = fds().get(a0 as i64)?;
                let unix_kind = match &file.lock().kind {
                    Kind::Unix(u) if u.listener.is_some() => return Err(EINVAL),
                    Kind::Unix(u) => Some(u.kind),
                    _ => None,
                };
                if let Some(kind) = unix_kind {
                    let sa = read_user(a1, (a2 as usize).min(110))?;
                    let (key, abstract_name) = unix_key(&sa)?;
                    if !abstract_name {
                        // The socket file: a node at the path (EADDRINUSE if it exists).
                        let end = sa[2..].iter().position(|&b| b == 0).unwrap_or(sa.len() - 2);
                        let p = resolve(&String::from_utf8_lossy(&sa[2..2 + end]));
                        if stat_path(&p).is_ok() {
                            return Err(EADDRINUSE);
                        }
                        let (m, rel) = ns().writable(&p)?;
                        m.fs.create(&rel, 0o755)?;
                    }
                    let l = ipc::bind_name(&key, kind).map_err(|_| EADDRINUSE)?;
                    if let Kind::Unix(u) = &mut file.lock().kind {
                        u.listener = Some(l);
                        u.name = sa;
                    }
                    return Ok(0);
                }
                let mut f = file.lock();
                if let Kind::Inet(s) = &f.kind
                    && !s.udp
                {
                    // TCP servers on 127.0.0.1 or any address listen on loopback (connections
                    // from the network are not supported).
                    let sa = read_user(a1, (a2 as usize).min(128))?;
                    let (ip, port) = sockaddr_in(&sa)?;
                    let o = ip.octets();
                    if o[0] == 127 || (o == [0; 4] && port != 0) {
                        let port = if port == 0 { loopback_port() } else { port };
                        let l = ipc::bind_name(&format!("tcp:{port}"), ipc::SockType::Stream).map_err(|_| EADDRINUSE)?;
                        f.kind = Kind::Unix(UnixSocket { kind: ipc::SockType::Stream, listener: Some(l), name: encode_sockaddr_in(ip, port) });
                    }
                    return Ok(0);
                }
                if let Kind::Inet(s) = &mut f.kind
                    && s.udp
                {
                    let (_, port) = sockaddr_in(&read_user(a1, (a2 as usize).min(128))?)?;
                    s.local_port = crate::net::udp_bind(s.handle, port).map_err(sock_err)?;
                }
                0
            }
            50 => {
                // listen(fd, backlog)
                let file = fds().get(a0 as i64)?;
                if matches!(&file.lock().kind, Kind::Inet(s) if !s.udp) {
                    // A TCP socket not bound to a port: listen on a free loopback port.
                    let port = loopback_port();
                    let l = ipc::bind_name(&format!("tcp:{port}"), ipc::SockType::Stream).map_err(|_| EADDRINUSE)?;
                    file.lock().kind = Kind::Unix(UnixSocket { kind: ipc::SockType::Stream, listener: Some(l), name: encode_sockaddr_in(smoltcp::wire::Ipv4Address::new(0, 0, 0, 0), port) });
                }
                if let Kind::Unix(u) = &file.lock().kind {
                    let l = u.listener.as_ref().ok_or(EINVAL)?;
                    let mut l = l.lock();
                    l.listening = true;
                    l.backlog = (a1 as usize).clamp(1, 4096);
                }
                0
            }
            43 | 288 => {
                // accept(fd, addr, len), accept4(fd, addr, len, flags)
                let file = fds().get(a0 as i64)?;
                let end = loop {
                    let (got, nonblock) = {
                        let f = file.lock();
                        let Kind::Unix(UnixSocket { listener: Some(l), .. }) = &f.kind else { return Err(EINVAL) };
                        let mut l = l.lock();
                        let e = l.queue.pop_front();
                        if e.is_some() {
                            l.generation = ipc::next_gen();
                        }
                        (e, f.flags & O_NONBLOCK != 0)
                    };
                    if let Some(e) = got {
                        break e;
                    }
                    if nonblock {
                        return Err(EAGAIN);
                    }
                    ipc::wait(None);
                    process::check_exiting();
                    if signal::has_deliverable(thread()) {
                        return Err(EINTR);
                    }
                };
                let flags = if nr == 288 { a3 } else { 0 };
                match end.inet {
                    Some((_, peer)) => put_addr(a1, a2, &encode_sockaddr_in(smoltcp::wire::Ipv4Address::new(127, 0, 0, 1), peer))?,
                    None => put_addr(a1, a2, &1u16.to_le_bytes())?,
                }
                let mut t = fds();
                let fd = t.add(new_file(Kind::Channel(end), (flags & O_NONBLOCK) | 2), 0);
                t.set_cloexec(fd, flags & O_CLOEXEC != 0);
                fd
            }
            51 | 52 => {
                // getsockname, getpeername
                let file = fds().get(a0 as i64)?;
                let sa = match &file.lock().kind {
                    Kind::Inet(s) if nr == 51 => encode_sockaddr_in(crate::net::local_ip(), s.local_port),
                    Kind::Inet(s) => {
                        let (ip, port) = s.peer.ok_or(ENOTCONN)?;
                        encode_sockaddr_in(ip, port)
                    }
                    Kind::Unix(u) if nr == 51 && !u.name.is_empty() => u.name.clone(),
                    Kind::Channel(ipc::End { inet: Some((local, peer)), .. }) => encode_sockaddr_in(smoltcp::wire::Ipv4Address::new(127, 0, 0, 1), if nr == 51 { *local } else { *peer }),
                    _ => 1u16.to_le_bytes().to_vec(), // unnamed AF_UNIX
                };
                put_addr(a1, a2, &sa)?;
                0
            }
            54 => {
                // setsockopt: SO_PASSCRED is kept (credentials come with received
                // messages); other options are accepted and ignored.
                if a1 == 1 && a2 == 16 {
                    let on = read_u32(a3)? != 0;
                    let file = fds().get(a0 as i64)?;
                    let mut f = file.lock();
                    f.flags = if on { f.flags | PASSCRED } else { f.flags & !PASSCRED };
                }
                0
            }
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
                    (1, 16) => ((fds().get(a0 as i64)?.lock().flags & PASSCRED != 0) as i32).to_le_bytes().to_vec(),
                    (1, 3) => {
                        let t = match &fds().get(a0 as i64)?.lock().kind {
                            Kind::Channel(ipc::End { kind, .. }) | Kind::Unix(UnixSocket { kind, .. }) => match kind { ipc::SockType::Stream => 1, ipc::SockType::Datagram => 2, ipc::SockType::SeqPacket => 5 },
                            _ => 1,
                        };
                        (t as i32).to_le_bytes().to_vec()
                    }
                    (1, 7) | (1, 8) => 212992i32.to_le_bytes().to_vec(), // SO_SNDBUF, SO_RCVBUF
                    (1, 17) => {
                        // SO_PEERCRED: struct ucred { pid, uid, gid }
                        let mut c = Vec::new();
                        // The peer is not tracked; programs talking over local sockets
                        // run as the same user, so the caller's own ids stand in.
                        let (uid, gid) = { let cr = proc().cred.lock(); (cr.show(cr.euid, false), cr.show(cr.egid, true)) };
                        c.extend_from_slice(&(process::pid_view(proc(), proc().pid) as i32).to_le_bytes());
                        c.extend_from_slice(&uid.to_le_bytes());
                        c.extend_from_slice(&gid.to_le_bytes());
                        c
                    }
                    _ => 0i32.to_le_bytes().to_vec(),
                };
                // Never more than the caller's buffer holds (*optlen).
                let cap = read_u32(a4)? as usize;
                let n = v.len().min(cap);
                write_user(a3, &v[..n])?;
                write_user(a4, &(n as u32).to_le_bytes())?;
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
            253 | 294 => {
                // inotify_init(), inotify_init1(flags: IN_NONBLOCK, IN_CLOEXEC)
                let flags = if nr == 294 { a0 } else { 0 };
                if flags & !(0o4000 | 0o2000000) != 0 {
                    return Err(EINVAL);
                }
                fds().add(new_file(Kind::Inotify(inotify::new()), (flags & (O_NONBLOCK | 0o2000000)) | 2), 0)
            }
            254 => {
                // inotify_add_watch(fd, path, mask)
                let file = fds().get(a0 as i64)?;
                let Kind::Inotify(i) = &file.lock().kind else { return Err(EINVAL) };
                let i = i.clone();
                let mask = a2 as u32;
                let raw = read_cstr(a1)?;
                let p = if mask & inotify::IN_DONT_FOLLOW != 0 { resolve_nofollow(&raw) } else { resolve(&raw) };
                let st = stat_path(&p)?;
                let is_dir = st.get(24..28).is_some_and(|m| u32::from_le_bytes(m.try_into().unwrap()) & 0o170000 == 0o040000);
                if mask & inotify::IN_ONLYDIR != 0 && !is_dir {
                    return Err(ENOTDIR);
                }
                i.lock().add_watch(p, mask)? as i64
            }
            255 => {
                // inotify_rm_watch(fd, wd)
                let file = fds().get(a0 as i64)?;
                let Kind::Inotify(i) = &file.lock().kind else { return Err(EINVAL) };
                let i = i.clone();
                i.lock().rm_watch(a1 as i32)?;
                ipc::notify();
                0
            }
            283 => {
                // timerfd_create(clockid, flags): REALTIME, MONOTONIC, BOOTTIME and the
                // _ALARM forms; TFD_NONBLOCK, TFD_CLOEXEC
                if !matches!(a0, 0 | 1 | 7 | 8 | 9) || a1 & !(0o4000 | 0o2000000) != 0 {
                    return Err(EINVAL);
                }
                let t = ipc::timerfd(matches!(a0, 0 | 8));
                fds().add(new_file(Kind::TimerFd(t), (a1 & (O_NONBLOCK | 0o2000000)) | 2), 0)
            }
            286 | 287 => {
                // timerfd_settime(fd, flags, new, old), timerfd_gettime(fd, cur)
                let file = fds().get(a0 as i64)?;
                let Kind::TimerFd(t) = &file.lock().kind else { return Err(EINVAL) };
                let t = t.clone();
                let (interval, value) = if nr == 286 {
                    // TFD_TIMER_ABSTIME 1, TFD_TIMER_CANCEL_ON_SET 2 (accepted; clock changes are not signalled)
                    if a1 & !3 != 0 {
                        return Err(EINVAL);
                    }
                    let (interval, value) = read_itimerspec(a2)?;
                    t.lock().set(value, interval, a1 & 1 != 0)
                } else {
                    t.lock().get()
                };
                let out = if nr == 286 { a3 } else { a1 };
                if out != 0 {
                    write_user(out, &[timespec(interval), timespec(value)].concat())?;
                }
                0
            }
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
                let size = if nr == 77 { a1 } else { a2.saturating_add(a3) };
                let mut fl = file.lock();
                match &mut fl.kind {
                    Kind::Memfd(m, _) => {
                        let mut m = m.lock();
                        if nr == 77 || size > m.size {
                            m.truncate(size);
                        }
                    }
                    Kind::File { data, dirty, .. } => {
                        vfs::resize_file(data.to_mut(), size)?;
                        *dirty = true;
                    }
                    _ => return Err(EINVAL),
                }
                0
            }
            8 => {
                // lseek
                let file = fds().get(a0 as i64)?;
                #[cfg(feature = "linux-drivers")]
                if let Some(r) = lxfile::llseek(&file, a1 as i64, a2) {
                    return r;
                }
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
                        m.note_map(a0, a0 + len, "fixed");
                        a0
                    } else {
                        m.reserve_at(a0, len, true).ok_or(ENOMEM_ERR)?
                    }
                };
                if flags & 0x20 == 0 && fd >= 0 {
                    if let Ok(Some(p)) = fd_path(fd) {
                        mem().labels.insert(addr, (addr + len, p, a5));
                    }
                    // File mapping. Read-only system files: map their pages directly
                    // (copy on write). Other files: copy the contents in.
                    let file = fds().get(fd)?;
                    // A Linux driver's file (GPU buffers): the driver maps its pages.
                    #[cfg(feature = "linux-drivers")]
                    if let Some(r) = lxfile::mmap(&file, addr, len, a2, flags, a5) {
                        r?;
                        return Ok(addr as i64);
                    }
                    let fl = file.lock();
                    match &fl.kind {
                        Kind::Memfd(m, _) if flags & 3 != 2 => {
                            // MAP_SHARED: map its frames (every mapping sees the same pages).
                            mem().map_shared(addr, m.clone(), a5 as usize, len);
                        }
                        Kind::Memfd(m, _) => {
                            // MAP_PRIVATE (the dynamic loader mapping a library from tmpfs):
                            // a private copy, so writes (relocations) stay in this mapping.
                            let m = m.clone();
                            drop(fl);
                            let bytes = {
                                let mut m = m.lock();
                                let size = m.size as usize;
                                let off = (a5 as usize).min(size);
                                m.read(off, (len as usize).min(size - off))
                            };
                            write_user(addr, &bytes)?;
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
            25 => {
                // mremap(old, old_size, new_size, flags, new_address): shrink in place, grow
                // in place if the space after is free, else (MREMAP_MAYMOVE) move the pages
                // to a new range without copying; MREMAP_FIXED puts them at new_address.
                const MAYMOVE: u64 = 1;
                const FIXED: u64 = 2;
                let (old, old_len, new_len) = (a0, a1.next_multiple_of(PAGE), a2.next_multiple_of(PAGE));
                if old % PAGE != 0 || new_len == 0 || a3 & !(MAYMOVE | FIXED | 4) != 0 {
                    return Err(EINVAL);
                }
                let mut m = mem();
                let area = m.area_at(old).ok_or(EFAULT)?;
                if area.end < old + old_len.max(PAGE) {
                    return Err(EFAULT);
                }
                if a3 & FIXED == 0 {
                    if new_len <= old_len {
                        m.unmap(old + new_len, old_len - new_len);
                        return Ok(old as i64);
                    }
                    if m.is_free(old + old_len, new_len - old_len) {
                        m.areas.push(process::Area { start: old + old_len, end: old + new_len, writable: area.writable });
                        m.note_map(old + old_len, old + new_len, "mremap-grow");
                        return Ok(old as i64);
                    }
                    if a3 & MAYMOVE == 0 {
                        return Err(ENOMEM_ERR);
                    }
                }
                let to = if a3 & FIXED != 0 {
                    if a4 % PAGE != 0 {
                        return Err(EINVAL);
                    }
                    m.unmap(a4, new_len);
                    m.areas.push(process::Area { start: a4, end: a4 + new_len, writable: area.writable });
                    a4
                } else {
                    m.reserve_with(new_len, area.writable).ok_or(ENOMEM_ERR)?
                };
                m.move_pages(old, to, old_len.min(new_len));
                m.unmap(old, old_len);
                to as i64
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
                        // Never over a mapping: brk fails instead (glibc then uses mmap).
                        if !u.is_free(old, new - old) {
                            return Ok(u.brk as i64);
                        }
                        u.areas.push(process::Area { start: old, end: new, writable: true });
                        u.note_map(old, new, "brk");
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
                // A Linux driver's file: its unlocked_ioctl, or compat_ioctl for a
                // 32-bit program (FIONBIO is the VFS's, as on Linux).
                #[cfg(feature = "linux-drivers")]
                if a1 != 0x5421
                    && let Some(r) = lxfile::ioctl(&file, a1, a2, thread().compat)
                {
                    return r;
                }
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
                            Kind::Channel(e) => e.rx.as_ref().map_or(0, |c| c.lock().pending(e.kind != ipc::SockType::Stream)),
                            Kind::Inet(s) => crate::net::recv_pending(s.handle, s.udp),
                            Kind::Inotify(i) => i.lock().pending(),
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
                        #[cfg(feature = "linux-drivers")]
                        {
                            let flags = f.flags;
                            drop(f);
                            lxfile::set_flags(&file, flags);
                        }
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
            // Ids as the caller's PID namespace numbers them.
            39 => process::pid_view(proc(), proc().pid) as i64,
            186 => process::pid_view(proc(), thread().tid) as i64,
            110 => process::pid_view(proc(), proc().parent.load(core::sync::atomic::Ordering::Relaxed)) as i64, // getppid
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
                // Record locks (F_SETLK, F_SETLKW, F_OFD_SETLK, F_OFD_SETLKW; i386 *64 forms):
                // granted. Programs coordinate with them, but one system with one user has
                // nobody to contend with yet. F_GETLK reports the range unlocked.
                6 | 7 | 13 | 14 | 37 | 38 => 0,
                5 | 12 | 36 => {
                    write_user(a2, &2i16.to_le_bytes())?; // l_type = F_UNLCK
                    0
                }
                8 | 9 | 15 | 16 => 0,  // F_SETOWN, F_GETOWN, F_SETOWN_EX, F_GETOWN_EX
                1031 => a2 as i64,     // F_SETPIPE_SZ: pipes hold 512 KiB anyway
                1032 => 512 * 1024,    // F_GETPIPE_SZ
                1033 => 0, // F_ADD_SEALS (memfd sealing: accepted, not enforced)
                1034 => 0, // F_GET_SEALS
                3 => (fds().get(a0 as i64)?.lock().flags & !PASSCRED) as i64,
                4 => {
                    let file = fds().get(a0 as i64)?;
                    let mut f = file.lock();
                    f.flags = (f.flags & !O_NONBLOCK) | (a2 & O_NONBLOCK);
                    #[cfg(feature = "linux-drivers")]
                    {
                        let flags = f.flags;
                        drop(f);
                        lxfile::set_flags(&file, flags);
                    }
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
                for q in fs_sharers() {
                    q.fds.lock().cwd = path.clone();
                }
                fds().cwd = path;
                0
            }
            80 => {
                // chdir
                let name = resolve(&read_cstr(a0)?);
                if !is_dir(&name) {
                    return Err(ENOENT);
                }
                for q in fs_sharers() {
                    q.fds.lock().cwd = name.clone();
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
            133 | 259 => {
                // mknod(path, mode, dev), mknodat(dirfd, path, mode, dev): FIFOs and
                // regular files (device nodes need a driver: EPERM).
                let (p, mode) = if nr == 133 { (resolve_nofollow(&read_cstr(a0)?), a1) } else { (at_path_nf(a0 as i32 as i64, &read_cstr(a1)?, false)?, a2) };
                if stat_path(&p).is_ok() {
                    return Err(EEXIST);
                }
                let (m, rel) = ns().writable(&p)?;
                let perm = mode as u32 & 0o7777 & !0o022;
                match mode & 0o170000 {
                    0o010000 => m.fs.mkfifo(&rel, perm)?,
                    0 | 0o100000 | 0o140000 => m.fs.create(&rel, perm)?,
                    _ => return Err(EPERM),
                }
                0
            }
            84 => {
                // rmdir
                let p = resolve_nofollow(&read_cstr(a0)?);
                let (m, rel) = ns().writable(&p)?;
                m.fs.rmdir(&rel)?;
                inotify::removed(&p, true);
                0
            }
            221 => 0,                   // fadvise64: advice only
            444..=446 => return Err(ENOSYS), // landlock: no sandboxing rules (programs fall back)
            452 => {
                // fchmodat2(dirfd, path, mode, flags): a symbolic link itself cannot have a
                // mode (AT_SYMLINK_NOFOLLOW on one: EOPNOTSUPP, as on Linux)
                let p = at_path_nf(a0 as i32 as i64, &read_cstr(a1)?, a3 & 0x100 == 0)?;
                let (m, rel) = ns().writable(&p)?;
                if m.fs.lstat(&rel)?.ty == vfs::Type::Lnk {
                    return Err(EOPNOTSUPP);
                }
                m.fs.chmod(&rel, a2 as u32)?;
                0
            }
            90 | 268 => {
                // chmod(path, mode), fchmodat(dirfd, path, mode)
                let p = if nr == 90 { resolve(&read_cstr(a0)?) } else { at_path(a0 as i32 as i64, &read_cstr(a1)?)? };
                let mode = if nr == 90 { a1 } else { a2 };
                let (m, rel) = ns().writable(&p)?;
                m.fs.chmod(&rel, mode as u32)?;
                inotify::notify(&p, inotify::IN_ATTRIB, false);
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
                inotify::notify(&p, inotify::IN_CREATE, true);
                0
            }
            82 | 264 | 316 => {
                // rename, renameat, renameat2(olddirfd, old, newdirfd, new, flags)
                let (from, to) = if nr == 82 { (resolve_nofollow(&read_cstr(a0)?), resolve_nofollow(&read_cstr(a1)?)) } else { (at_path_nf(a0 as i32 as i64, &read_cstr(a1)?, false)?, at_path_nf(a2 as i32 as i64, &read_cstr(a3)?, false)?) };
                if nr == 316 && a4 & 1 != 0 && stat_path(&to).is_ok() {
                    return Err(EEXIST); // RENAME_NOREPLACE
                }
                let (m, rf, rt) = ns().same_mount(&from, &to)?;
                let is_dir = m.fs.lstat(&rf).is_ok_and(|st| st.ty == vfs::Type::Dir);
                m.fs.rename(&rf, &rt)?;
                inotify::moved(&from, &to, is_dir);
                0
            }
            87 | 263 => {
                // unlink(path), unlinkat(dirfd, path, flags: AT_REMOVEDIR)
                let p = if nr == 87 { resolve_nofollow(&read_cstr(a0)?) } else { at_path_nf(a0 as i32 as i64, &read_cstr(a1)?, false)? };
                if let Some(n) = p.strip_prefix("dev/shm/") {
                    return SHM.lock().remove(n).map(|_| 0).ok_or(ENOENT);
                }
                let (m, rel) = ns().writable(&p)?;
                let dir = nr == 263 && a2 & 0x200 != 0;
                if dir { m.fs.rmdir(&rel)? } else { m.fs.unlink(&rel)? }
                inotify::removed(&p, dir);
                ipc::unbind_name(&path_key(&p));
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
                // capget(header, data): Nova does not check capabilities, but programs look
                // at them. Version 3 headers carry two 32-bit sets.
                let version = read_u32(a0)?;
                if version != 0x2008_0522 && version != 0x2007_1026 {
                    write_user(a0, &0x2008_0522u32.to_le_bytes())?;
                    return if a1 == 0 { Ok(0) } else { Err(EINVAL) };
                }
                if a1 != 0 {
                    // Root, and a process in a user namespace of its own, have every
                    // capability; other users none (as on Linux).
                    let (full, set) = { let c = proc().cred.lock(); (c.euid == 0 || c.userns.is_some(), c.caps) };
                    let caps = set.unwrap_or(if full { [u32::MAX, u32::MAX, 0, (1u32 << (CAP_LAST - 31)) - 1, (1u32 << (CAP_LAST - 31)) - 1, 0] } else { [0; 6] });
                    write_user(a1, &caps.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>())?;
                }
                0
            }
            126 => {
                // capset(header, data): remembered, so capget reports what was set (programs
                // drop capabilities and check that they are gone)
                if a1 != 0 && read_u32(a0)? == 0x2008_0522 {
                    let raw = read_user(a1, 24)?;
                    let w: Vec<u32> = raw.chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect();
                    proc().cred.lock().caps = Some([w[0], w[1], w[2], w[3], w[4], w[5]]);
                }
                0
            }
            64 => sysv::semget(a0 as i32, a1, a2)?,
            65 => sysv::semop(a0 as i32, a1, a2, None)?,
            220 => {
                // semtimedop(id, sops, nsops, timeout)
                let d = if a3 == 0 { None } else { deadline(a3, false, 1)? };
                sysv::semop(a0 as i32, a1, a2, d)?
            }
            66 => sysv::semctl(a0 as i32, a1, a2, a3, false)?,
            29 => sysv::shmget(a0 as i32, a1, a2)?,
            30 => sysv::shmat(a0 as i32, a1, a2)? as i64,
            31 => sysv::shmctl(a0 as i32, a1, a2, false)?,
            67 => sysv::shmdt(a0)?,
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
                // /proc/self/... names the caller: once it is gone, so is the directory
                // (Chromium's sandbox chroots a CLONE_FS child into its own fdinfo).
                let p = match p.strip_prefix("proc/self/") {
                    Some(rest) => format!("proc/{}/{rest}", proc().pid),
                    None => p,
                };
                let new = ns().chroot(&p);
                *proc().ns.lock() = new.clone();
                let cwd = fds().cwd.clone();
                let cwd: String = if cwd == p { String::new() } else { cwd.strip_prefix(&format!("{p}/")).unwrap_or("").into() };
                fds().cwd = cwd.clone();
                for q in fs_sharers() {
                    *q.ns.lock() = new.clone();
                    q.fds.lock().cwd = cwd.clone();
                }
                0
            }
            272 => {
                // unshare(flags): CLONE_NEWNS gives the process its own mount table; the
                // other namespaces (user, pid, ipc, uts, net, cgroup) are accepted as is.
                if a0 & CLONE_NEWNS != 0 {
                    let copy = ns().copy();
                    *proc().ns.lock() = copy;
                }
                if a0 & CLONE_NEWUSER != 0 {
                    process::unshare_user(proc().pid);
                }
                if a0 & process::CLONE_NEWPID != 0 {
                    // The caller stays where it is; its next child is the new namespace's init.
                    let base = proc().child_pidns.lock().clone().or_else(|| proc().pidns.lock().clone());
                    *proc().child_pidns.lock() = Some(process::PidNs::new(base));
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
                let rel = read_cstr(path)?;
                let p = if nr == 89 {
                    resolve_nofollow(&rel)
                } else if rel.is_empty() && a0 as i32 as i64 != AT_FDCWD {
                    // readlinkat(fd, ""): the link an O_PATH descriptor names
                    fd_path(a0 as i32 as i64)?.ok_or(ENOENT)?
                } else {
                    at_path_nf(a0 as i32 as i64, &rel, false)?
                };
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
                24 | 47 => 0,                               // PR_CAPBSET_DROP, PR_CAP_AMBIENT
                1 | 4 | 8 | 36 | 0x53564d41 => 0, // PDEATHSIG, SET_DUMPABLE, KEEPCAPS, CHILD_SUBREAPER, SET_VMA
                21 => thread().seccomp.mode as i64,         // PR_GET_SECCOMP
                22 => seccomp::prctl_set(a1, a2)?,          // PR_SET_SECCOMP
                38 => {
                    // PR_SET_NO_NEW_PRIVS: one way only
                    if a1 != 1 || a2 | a3 | a4 != 0 {
                        return Err(EINVAL);
                    }
                    thread().no_new_privs = true;
                    0
                }
                39 => if a1 | a2 | a3 | a4 != 0 { return Err(EINVAL) } else { thread().no_new_privs as i64 }, // PR_GET_NO_NEW_PRIVS
                15 => {
                    // PR_SET_NAME: the thread's name (up to 15 bytes)
                    let mut n = read_cstr(a1).unwrap_or_default();
                    n.truncate(15);
                    thread().comm = n;
                    0
                }
                16 => {
                    // PR_GET_NAME
                    let mut n = [0u8; 16];
                    let comm = thread().comm.clone();
                    let name = if comm.is_empty() { proc().name.as_bytes() } else { comm.as_bytes() };
                    n[..name.len().min(15)].copy_from_slice(&name[..name.len().min(15)]);
                    write_user(a1, &n)?;
                    0
                }
                _ => return Err(EINVAL),
            },
            317 => seccomp::seccomp(a0, a1, a2)?,
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
            111 | 112 | 121 => 0, // getpgrp, setsid, getpgid
            109 => 0,             // setpgid
            102 | 104 | 107 | 108 => {
                // getuid, getgid, geteuid, getegid
                let c = proc().cred.lock();
                (match nr {
                    102 => c.show(c.uid, false),
                    107 => c.show(c.euid, false),
                    104 => c.show(c.gid, true),
                    _ => c.show(c.egid, true),
                }) as i64
            }
            105 | 106 | 113 | 114 | 117 | 119 => {
                // setuid, setgid, setreuid, setregid, setresuid, setresgid: allowed for any
                // id (Nova does not check permissions); -1 keeps a value.
                let gid = matches!(nr, 106 | 114 | 119);
                let mut guard = proc().cred.lock();
                let c = &mut *guard;
                let ids = match nr {
                    105 | 106 => [a0, a0, a0],
                    113 | 114 => [a0, a1, if a1 as u32 != u32::MAX { a1 } else { u32::MAX as u64 }],
                    _ => [a0, a1, a2],
                };
                let ids = ids.map(|v| (v as u32 != u32::MAX).then(|| c.take(v as u32, gid)));
                let (r, e, sv) = if gid { (&mut c.gid, &mut c.egid, &mut c.sgid) } else { (&mut c.uid, &mut c.euid, &mut c.suid) };
                for (slot, v) in [r, e, sv].into_iter().zip(ids) {
                    if let Some(v) = v {
                        *slot = v;
                    }
                }
                0
            }
            122 | 123 => {
                // setfsuid, setfsgid: the previous one (the effective id)
                let c = proc().cred.lock();
                (if nr == 122 { c.show(c.euid, false) } else { c.show(c.egid, true) }) as i64
            }
            115 => {
                // getgroups(size, list)
                let c = proc().cred.lock();
                let groups: Vec<u32> = c.groups.iter().map(|&g| c.show(g, true)).collect();
                drop(c);
                if a0 != 0 {
                    if (a0 as usize) < groups.len() {
                        return Err(EINVAL);
                    }
                    write_user(a1, &groups.iter().flat_map(|g| g.to_le_bytes()).collect::<Vec<u8>>())?;
                }
                groups.len() as i64
            }
            116 => {
                // setgroups(size, list)
                let raw = read_user(a1, (a0 as usize).min(65536) * 4)?;
                let mut c = proc().cred.lock();
                let groups = raw.chunks_exact(4).map(|b| c.take(u32::from_le_bytes(b.try_into().unwrap()), true)).collect();
                c.groups = groups;
                0
            }
            37 => {
                // alarm(seconds): SIGALRM later; returns the seconds left of the previous one
                let old = set_alarm(proc().pid, (a0 > 0).then(|| (a0 * 1_000_000, 0)));
                old.map_or(0, |(left, _)| left.div_ceil(1_000_000)) as i64
            }
            38 | 36 => {
                // setitimer(which, new, old), getitimer(which, cur): ITIMER_REAL (SIGALRM)
                if a0 != 0 {
                    return Err(EINVAL);
                }
                let tv = |p: u64| -> Result<u64, i64> {
                    let b = read_user(p, 16)?;
                    Ok(u64::from_le_bytes(b[0..8].try_into().unwrap()) * 1_000_000 + u64::from_le_bytes(b[8..16].try_into().unwrap()))
                };
                let old = if nr == 38 {
                    let (interval, value) = (tv(a1)?, tv(a1 + 16)?);
                    set_alarm(proc().pid, (value > 0).then_some((value, interval)))
                } else {
                    ALARMS.lock().get(&proc().pid).map(|&(at, interval)| (at.saturating_sub(pit::nanos() / 1000), interval))
                };
                let out = if nr == 38 { a2 } else { a1 };
                if out != 0 {
                    let (left, interval) = old.unwrap_or((0, 0));
                    let mut b = Vec::new();
                    for us in [interval, left] {
                        b.extend_from_slice(&(us / 1_000_000).to_le_bytes());
                        b.extend_from_slice(&(us % 1_000_000).to_le_bytes());
                    }
                    write_user(out, &b)?;
                }
                0
            }
            118 | 120 => {
                // getresuid, getresgid: real, effective and saved ids
                let c = proc().cred.lock();
                let gid = nr == 120;
                let ids = if gid { [c.gid, c.egid, c.sgid] } else { [c.uid, c.euid, c.suid] }.map(|v| c.show(v, gid));
                drop(c);
                for (p, v) in [a0, a1, a2].into_iter().zip(ids) {
                    write_user(p, &v.to_le_bytes())?;
                }
                0
            }
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
                process::pid_view(proc(), thread().tid) as i64
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
            273 => {
                // set_robust_list(head, len): remembered for get_robust_list (robust futexes
                // are not cleaned up at exit yet)
                thread().robust_list = a0;
                0
            }
            274 => {
                // get_robust_list(pid, head**, len*): the caller's own (pid 0 or its tid)
                if a0 != 0 && a0 != thread().tid {
                    return Err(EPERM);
                }
                write_user(a1, &thread().robust_list.to_le_bytes())?;
                write_user(a2, &24u64.to_le_bytes())?;
                0
            }
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
            57 | 58 => process::pid_view(proc(), process::fork(f, nr == 58, 0, None, 0)?) as i64, // fork, vfork
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
                let pid = if pid > 0 { process::pid_lookup(proc(), pid as u64).ok_or(ESRCH)? as i64 } else { pid };
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
                let tid = process::pid_lookup(proc(), tid).ok_or(ESRCH)?;
                let a0 = if nr == 234 { process::pid_lookup(proc(), a0).ok_or(ESRCH)? } else { a0 };
                if tid == thread().tid {
                    signal::send_self(thread(), sig);
                } else if signal::send_thread(proc(), tid, sig) {
                    // Another thread of ours, exactly that one.
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
                let want = a0 as i32 as i64;
                let want = if want > 0 { process::pid_lookup(proc(), want as u64).ok_or(10i64)? as i64 } else { want }; // ECHILD
                match process::wait_child(want, nohang, true)? {
                    Some((pid, status)) => {
                        if a1 != 0 {
                            write_user(a1, &status.to_le_bytes())?;
                        }
                        if a3 != 0 {
                            write_user(a3, &[0u8; 144])?;
                        }
                        process::pid_view(proc(), pid) as i64
                    }
                    None => 0,
                }
            }
            247 => {
                // waitid(idtype, id, infop, options): P_ALL 0, P_PID 1
                let pid = if a0 == 1 { process::pid_lookup(proc(), a1 as i32 as u64).ok_or(10i64)? as i64 } else { -1 }; // ECHILD
                let (nohang, reap) = (a3 & 1 != 0, a3 & 0x0100_0000 == 0); // WNOHANG, WNOWAIT
                match process::wait_child(pid, nohang, reap)? {
                    Some((child, status)) => {
                        if a2 != 0 {
                            // siginfo: si_signo = SIGCHLD, si_code = CLD_EXITED (1) / CLD_KILLED (2)
                            let mut info = [0u8; 128];
                            info[0..4].copy_from_slice(&(signal::SIGCHLD as i32).to_le_bytes());
                            let (code, value) = if status & 0x7F != 0 { (2, status & 0x7F) } else { (1, (status >> 8) & 0xFF) };
                            info[8..12].copy_from_slice(&(code as i32).to_le_bytes());
                            info[16..20].copy_from_slice(&(process::pid_view(proc(), child) as i32).to_le_bytes());
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
pub const CLONE_NEWUSER: u64 = 0x1000_0000;
/// CAP_CHECKPOINT_RESTORE: the highest capability number Linux 7.x has.
const CAP_LAST: u64 = 40;

/// clone: a thread with CLONE_VM | CLONE_THREAD; anything else creates a process. A
/// process always gets its own copy of the memory (CLONE_VM without CLONE_THREAD, as
/// glibc's posix_spawn uses with CLONE_VFORK, is served as a vfork-style copy).
fn clone(f: &SyscallFrame, flags: u64, stack: u64, parent_tid: u64, child_tid: u64, tls: u64) -> Result<i64, i64> {
    if flags & CLONE_VM == 0 || flags & CLONE_THREAD == 0 {
        let pid = process::fork(f, flags & CLONE_VFORK != 0, stack, (flags & CLONE_SETTLS != 0).then_some(tls), flags)?;
        if flags & CLONE_NEWNS != 0 {
            process::unshare_ns(pid);
        }
        if flags & CLONE_NEWUSER != 0 {
            process::unshare_user(pid);
        }
        let pid = process::pid_view(proc(), pid);
        if flags & CLONE_PARENT_SETTID != 0 {
            write_user(parent_tid, &(pid as u32).to_le_bytes())?;
        }
        return Ok(pid as i64);
    }
    let tid = process::pid_view(proc(), process::spawn_thread(f, stack, (flags & CLONE_SETTLS != 0).then_some(tls), if flags & CLONE_CHILD_CLEARTID != 0 { child_tid } else { 0 }));
    if flags & CLONE_PARENT_SETTID != 0 {
        write_user(parent_tid, &(tid as u32).to_le_bytes())?;
    }
    if flags & CLONE_CHILD_SETTID != 0 {
        write_user(child_tid, &(tid as u32).to_le_bytes())?;
    }
    Ok(tid as i64)
}

/// ITIMER_REAL timers: pid -> (deadline, interval), in microseconds of the monotonic clock.
static ALARMS: SpinLock<BTreeMap<u64, (u64, u64)>> = SpinLock::new(BTreeMap::new());

/// Arm (Some((after_us, interval_us))) or cancel the process's real-time timer. Returns
/// the previous one as (microseconds left, interval).
fn set_alarm(pid: u64, timer: Option<(u64, u64)>) -> Option<(u64, u64)> {
    static STARTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    let now = pit::nanos() / 1000;
    let mut a = ALARMS.lock();
    let old = match timer {
        Some((after, interval)) => a.insert(pid, (now + after, interval)),
        None => a.remove(&pid),
    };
    drop(a);
    if !STARTED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        task::spawn("alarms", || loop {
            let now = pit::nanos() / 1000;
            let due: Vec<u64> = ALARMS.lock().iter().filter(|(_, t)| t.0 <= now).map(|(&p, _)| p).collect();
            for pid in due {
                {
                    let mut a = ALARMS.lock();
                    match a.get(&pid).copied() {
                        Some((at, interval)) if interval > 0 => {
                            a.insert(pid, (at + interval, interval));
                        }
                        _ => {
                            a.remove(&pid);
                        }
                    }
                }
                match process::find(pid) {
                    Some(p) => signal::send_process(&p, signal::SIGALRM, 0),
                    None => {
                        ALARMS.lock().remove(&pid);
                    }
                }
            }
            // 1 ms resolution while timers run; idle checks every 10 ms.
            task::sleep_ticks(if ALARMS.lock().is_empty() { 10 } else { 1 });
        });
    }
    old.map(|(at, interval)| (at.saturating_sub(now), interval))
}

const ETIMEDOUT: i64 = 110;
const EINTR: i64 = 4;
const ESRCH: i64 = 3;

struct Waiter {
    /// The waiting thread's process (woken when it exits).
    process: usize,
    /// Futex key: (owner, address). Private futexes: the process and the virtual address;
    /// shared ones in shared memory: 0 and the physical address, so waiters in different
    /// processes (mapping it at different places) meet.
    owner: usize,
    addr: u64,
    bitset: u32,
    task: usize,
    woken: bool,
}

static WAITERS: SpinLock<Vec<Waiter>> = SpinLock::new(Vec::new());

fn key(p: &Process) -> usize {
    p as *const Process as usize
}

/// The futex key of `addr` in the calling process (see [`Waiter`]).
fn futex_key(addr: u64, shared: bool) -> (usize, u64) {
    if shared {
        let mut m = mem();
        if m.is_shared_page(addr) && m.ensure(addr, 4, false)
            && let Some(frame) = m.page_frame(addr)
        {
            return (0, frame as u64 + (addr & (PAGE - 1)));
        }
    }
    (key(proc()), addr)
}

fn wake_key(owner: usize, addr: u64, n: usize, bitset: u32) -> usize {
    let mut woken = 0;
    for w in WAITERS.lock().iter_mut() {
        if woken >= n {
            break;
        }
        if !w.woken && w.owner == owner && w.addr == addr && w.bitset & bitset != 0 {
            w.woken = true;
            task::wake(w.task);
            woken += 1;
        }
    }
    woken
}

/// Wake up to `n` threads of `p` waiting on `addr` (matching `bitset`). Returns how many.
fn wake_matching(p: &Process, addr: u64, n: usize, bitset: u32) -> usize {
    wake_key(key(p), addr, n, bitset)
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

fn futex_wait(addr: u64, val: u32, until: Option<u64>, bitset: u32, shared: bool) -> Result<i64, i64> {
    if read_u32(addr)? != val {
        return Err(EAGAIN);
    }
    let p = proc();
    let me = task::current_id();
    let (owner, kaddr) = futex_key(addr, shared);
    WAITERS.lock().push(Waiter { process: key(p), owner, addr: kaddr, bitset, task: me, woken: false });
    let remove = || {
        let mut ws = WAITERS.lock();
        let i = ws.iter().position(|w| w.task == me && w.process == key(p));
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
        // A signal interrupts the wait, as on Linux: glibc's pthread_cancel sends
        // SIGCANCEL and the thread acts on it when its wait returns and starts again.
        if signal::has_deliverable(thread()) {
            return if remove() { Ok(0) } else { Err(EINTR) };
        }
        if until.is_some_and(|u| pit::ticks() >= u) {
            return if remove() { Ok(0) } else { Err(ETIMEDOUT) };
        }
        task::wait_event();
    }
}

fn futex(addr: u64, op: u64, val: u32, a3: u64, addr2: u64, val3: u32) -> Result<i64, i64> {
    let realtime = op & 256 != 0;
    // FUTEX_PRIVATE_FLAG absent: the futex may be in memory shared between processes.
    let shared = op & 128 == 0;
    match op & 0x7F {
        0 => futex_wait(addr, val, deadline(a3, false, 1)?, u32::MAX, shared),
        9 => futex_wait(addr, val, deadline(a3, true, if realtime { 0 } else { 1 })?, val3, shared),
        1 => {
            let (o, k) = futex_key(addr, shared);
            Ok(wake_key(o, k, val as usize, u32::MAX) as i64)
        }
        10 => {
            let (o, k) = futex_key(addr, shared);
            Ok(wake_key(o, k, val as usize, val3) as i64)
        }
        3 | 4 => {
            // REQUEUE / CMP_REQUEUE: wake `val`, move up to `a3` waiters to addr2.
            if op & 0x7F == 4 && read_u32(addr)? != val3 {
                return Err(EAGAIN);
            }
            let ((o1, k1), (o2, k2)) = (futex_key(addr, shared), futex_key(addr2, shared));
            let woken = wake_key(o1, k1, val as usize, u32::MAX);
            let mut moved = 0;
            for w in WAITERS.lock().iter_mut() {
                if moved >= a3 as usize {
                    break;
                }
                if !w.woken && w.owner == o1 && w.addr == k1 {
                    (w.owner, w.addr) = (o2, k2);
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
    need_socket(&file)?;
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
    need_socket(&file)?;
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
        channel_read_flags(&file, total, flags)?
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
    let mut c = Vec::new();
    if file.lock().flags & PASSCRED != 0 {
        // SCM_CREDENTIALS: struct ucred { pid, uid, gid } of the sender. Programs talking
        // over local sockets run as the same user, so the receiver's ids stand in.
        let sender = match &file.lock().kind {
            Kind::Channel(e) => e.rx.as_ref().map_or(0, |rx| rx.lock().last_sender),
            _ => 0,
        };
        let (uid, gid) = { let cr = proc().cred.lock(); (cr.show(cr.euid, false), cr.show(cr.egid, true)) };
        c.extend_from_slice(&28u64.to_le_bytes());
        c.extend_from_slice(&1i32.to_le_bytes());
        c.extend_from_slice(&2i32.to_le_bytes());
        c.extend_from_slice(&(process::pid_view(proc(), sender as u64) as i32).to_le_bytes());
        c.extend_from_slice(&uid.to_le_bytes());
        c.extend_from_slice(&gid.to_le_bytes());
        c.resize(32, 0);
    }
    if !passed.is_empty() {
        let need = 16 + passed.len() * 4;
        if control == 0 || (controllen as usize) < c.len() + need {
            msg_flags |= 0x8; // MSG_CTRUNC: descriptors dropped
        } else {
            c.extend_from_slice(&(need as u64).to_le_bytes());
            c.extend_from_slice(&1i32.to_le_bytes());
            c.extend_from_slice(&1i32.to_le_bytes());
            for f in passed {
                let n = fds().add(f, 0);
                c.extend_from_slice(&(n as i32).to_le_bytes());
            }
            c.resize(c.len().next_multiple_of(8), 0);
        }
    }
    if control == 0 || (controllen as usize) < c.len() {
        if !c.is_empty() {
            msg_flags |= 0x8;
        }
        c.clear();
    }
    if !c.is_empty() {
        write_user(control, &c)?;
    }
    let used = c.len() as u64;
    write_user(msg + 40, &used.to_le_bytes())?;
    write_user(msg + 48, &msg_flags.to_le_bytes())?;
    if h[8..12] != [0, 0, 0, 0] && !is_inet(&file) {
        write_user(msg + 8, &0u32.to_le_bytes())?; // msg_namelen: no address
    }
    Ok(data.len() as i64)
}

/// select: fd_set bitmaps of `n` descriptors (read as bytes, so the same code serves
/// 32-bit programs); `deadline` in ticks.
pub fn select_fds(n: usize, rp: u64, wp: u64, ep: u64, deadline: Option<u64>) -> Result<i64, i64> {
    if n > 4096 {
        return Err(EINVAL);
    }
    let bytes = n.div_ceil(8);
    let get = |p: u64| -> Result<Vec<u8>, i64> { if p == 0 { Ok(vec![0; bytes]) } else { read_user(p, bytes) } };
    let (rin, win) = (get(rp)?, get(wp)?);
    loop {
        let (mut rout, mut wout) = (vec![0u8; bytes], vec![0u8; bytes]);
        let mut ready = 0;
        for fd in 0..n {
            let (r, w) = (rin[fd / 8] >> (fd % 8) & 1 != 0, win[fd / 8] >> (fd % 8) & 1 != 0);
            if !r && !w {
                continue;
            }
            let f = fds().get(fd as i64)?;
            let ev = poll_file(&f).0;
            if r && ev & (ipc::POLLIN | ipc::POLLHUP | ipc::POLLERR) != 0 {
                rout[fd / 8] |= 1 << (fd % 8);
                ready += 1;
            }
            if w && ev & (ipc::POLLOUT | ipc::POLLERR) != 0 {
                wout[fd / 8] |= 1 << (fd % 8);
                ready += 1;
            }
        }
        if ready > 0 || deadline.is_some_and(|d| pit::ticks() >= d) {
            for (p, v) in [(rp, &rout), (wp, &wout), (ep, &vec![0u8; bytes])] {
                if p != 0 {
                    write_user(p, v)?;
                }
            }
            return Ok(ready);
        }
        ipc::wait(deadline);
        process::check_exiting();
        if signal::has_deliverable(thread()) {
            return Err(EINTR);
        }
    }
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
