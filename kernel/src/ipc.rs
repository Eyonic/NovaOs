//! Inter-process (and inter-thread) objects behind Linux file descriptors:
//! pipes, Unix socket pairs (with file-descriptor passing), eventfd, epoll, and
//! memfd shared memory. linux.rs wires them to system calls.
//!
//! Blocking: a task that has to wait for one of these objects parks in
//! [`wait`]; every state change calls [`notify`], which wakes the parked tasks so
//! they re-check. Each object keeps a generation counter that changes with its
//! state, which is how edge-triggered epoll decides what is "new".

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::linux::FileRef;
use crate::sync::SpinLock;
use crate::{pit, task};

pub const POLLIN: u32 = 0x001;
pub const POLLOUT: u32 = 0x004;
pub const POLLERR: u32 = 0x008;
pub const POLLHUP: u32 = 0x010;
pub const POLLRDHUP: u32 = 0x2000;

/// Bytes a pipe or socket buffers before writers block.
const CAPACITY: usize = 512 * 1024;

static WAITERS: SpinLock<Vec<usize>> = SpinLock::new(Vec::new());
static GENERATION: AtomicU64 = AtomicU64::new(1);

pub fn next_gen() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Something changed: wake every task waiting for I/O.
pub fn notify() {
    let ws = core::mem::take(&mut *WAITERS.lock());
    for t in ws {
        task::wake(t);
    }
}

/// Park until [`notify`], an interrupt, or the deadline (in ticks).
pub fn wait(deadline: Option<u64>) {
    if deadline.is_some_and(|d| pit::ticks() >= d) {
        return;
    }
    WAITERS.lock().push(task::current_id());
    task::wait_event();
}

// ---------------------------------------------------------------------------
// Byte/message channels: one direction of a pipe or socket pair.

pub struct Message {
    pub data: Vec<u8>,
    pub fds: Vec<FileRef>,
    /// Sending process (SCM_CREDENTIALS); 0 for the kernel's own servers.
    pub pid: u32,
}

#[derive(Default)]
pub struct Channel {
    queue: VecDeque<Message>,
    bytes: usize,
    /// No writer left (all write ends closed / shut down).
    pub writer_closed: bool,
    /// No reader left.
    pub reader_closed: bool,
    pub generation: u64,
    /// Sender of the last message read (for SCM_CREDENTIALS).
    pub last_sender: u32,
}

pub type ChannelRef = Arc<SpinLock<Channel>>;

pub fn channel() -> ChannelRef {
    Arc::new(SpinLock::new(Channel { generation: next_gen(), ..Default::default() }))
}

impl Channel {
    pub fn readable(&self) -> bool {
        !self.queue.is_empty()
    }
    /// FIONREAD: queued bytes (streams), or the next message's length (datagrams).
    pub fn pending(&self, datagram: bool) -> usize {
        if datagram { self.queue.front().map_or(0, |m| m.data.len()) } else { self.bytes }
    }
    pub fn writable(&self) -> bool {
        self.bytes < CAPACITY
    }
    pub fn push(&mut self, m: Message) {
        self.bytes += m.data.len();
        self.queue.push_back(m);
        self.generation = next_gen();
    }
    /// Stream read: up to `max` bytes, crossing message boundaries; fds of every
    /// message touched are returned.
    pub fn read_stream(&mut self, max: usize) -> (Vec<u8>, Vec<FileRef>) {
        let mut out = Vec::new();
        let mut fds = Vec::new();
        while out.len() < max {
            let Some(front) = self.queue.front_mut() else { break };
            if out.is_empty() && fds.is_empty() {
                self.last_sender = front.pid;
            }
            // Like Linux, never merge data across a message that carries descriptors:
            // they must arrive with the bytes they were sent with.
            if !out.is_empty() && !front.fds.is_empty() {
                break;
            }
            let had_fds = !front.fds.is_empty();
            fds.append(&mut front.fds);
            let n = (max - out.len()).min(front.data.len());
            out.extend(front.data.drain(..n));
            if front.data.is_empty() {
                self.queue.pop_front();
            }
            if had_fds {
                break;
            }
        }
        self.bytes -= out.len();
        self.generation = next_gen();
        (out, fds)
    }
    /// Datagram read: one whole message (truncated to `max`).
    pub fn read_message(&mut self, max: usize) -> Option<(Vec<u8>, Vec<FileRef>, bool)> {
        let m = self.queue.pop_front()?;
        self.last_sender = m.pid;
        self.bytes -= m.data.len();
        self.generation = next_gen();
        let truncated = m.data.len() > max;
        let mut data = m.data;
        data.truncate(max);
        Some((data, m.fds, truncated))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SockType {
    Stream,
    Datagram,
    SeqPacket,
}

/// One end of a pipe or Unix socket pair. Dropping it closes that end.
pub struct End {
    pub rx: Option<ChannelRef>,
    pub tx: Option<ChannelRef>,
    pub kind: SockType,
    /// Loopback TCP connection: (local port, peer port), for getsockname/getpeername.
    pub inet: Option<(u16, u16)>,
}

impl Drop for End {
    fn drop(&mut self) {
        if let Some(rx) = &self.rx {
            let mut c = rx.lock();
            c.reader_closed = true;
            c.queue.clear(); // drop any fds in flight
            c.generation = next_gen();
        }
        if let Some(tx) = &self.tx {
            let mut c = tx.lock();
            c.writer_closed = true;
            c.generation = next_gen();
        }
        notify();
    }
}

impl End {
    pub fn poll(&self) -> (u32, u64) {
        if let (Some(rx), Some(tx)) = (&self.rx, &self.tx) {
            // A socket (Linux unix_poll): each direction can be shut down on its own
            // (shutdown(2) or the peer closing). A shut receive side reads EOF (POLLIN |
            // POLLRDHUP); POLLHUP only once both directions are shut.
            let (r, t) = (rx.lock(), tx.lock());
            let rcv_shut = r.writer_closed || r.reader_closed;
            let snd_shut = t.reader_closed || t.writer_closed;
            let mut ev = 0;
            if r.readable() || rcv_shut {
                ev |= POLLIN;
            }
            if rcv_shut {
                ev |= POLLRDHUP;
            }
            if !snd_shut && t.writable() {
                ev |= POLLOUT;
            }
            if rcv_shut && snd_shut {
                ev |= POLLHUP;
            }
            return (ev, r.generation ^ t.generation.rotate_left(17));
        }
        let mut ev = 0;
        let mut generation_mark = 0;
        if let Some(rx) = &self.rx {
            let c = rx.lock();
            if c.readable() {
                ev |= POLLIN;
            }
            if c.writer_closed {
                ev |= POLLHUP | POLLRDHUP | POLLIN;
            }
            generation_mark ^= c.generation;
        }
        if let Some(tx) = &self.tx {
            let c = tx.lock();
            if c.writable() && !c.reader_closed {
                ev |= POLLOUT;
            }
            if c.reader_closed {
                ev |= POLLERR | POLLHUP;
            }
            generation_mark ^= c.generation.rotate_left(17);
        }
        (ev, generation_mark)
    }
}

/// A pipe: (read end, write end).
pub fn pipe() -> (End, End) {
    let c = channel();
    (End { rx: Some(c.clone()), tx: None, kind: SockType::Stream, inet: None }, End { rx: None, tx: Some(c), kind: SockType::Stream, inet: None })
}

/// A connected pair of Unix sockets.
pub fn socketpair(kind: SockType) -> (End, End) {
    let (a, b) = (channel(), channel());
    (End { rx: Some(a.clone()), tx: Some(b.clone()), kind, inet: None }, End { rx: Some(b), tx: Some(a), kind, inet: None })
}

// ---------------------------------------------------------------------------
// Named Unix sockets: a listening socket queues the server ends of new connections.

pub struct Listener {
    pub kind: SockType,
    /// Server ends of connections not accepted yet.
    pub queue: VecDeque<End>,
    /// listen() was called (before that, connect gets ECONNREFUSED).
    pub listening: bool,
    pub backlog: usize,
    pub generation: u64,
    /// Registry key while bound.
    pub key: alloc::string::String,
}

pub type ListenerRef = Arc<SpinLock<Listener>>;

impl Listener {
    pub fn poll(&self) -> (u32, u64) {
        (if self.queue.is_empty() { 0 } else { POLLIN }, self.generation)
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let mut names = NAMES.lock();
        if names.get(&self.key).is_some_and(|w| w.strong_count() == 0) {
            names.remove(&self.key);
        }
    }
}

/// Bound socket addresses: abstract names as "@name", paths by file system and path.
static NAMES: SpinLock<alloc::collections::BTreeMap<alloc::string::String, alloc::sync::Weak<SpinLock<Listener>>>> = SpinLock::new(alloc::collections::BTreeMap::new());

/// Bind a socket to `key`; Err(()) if the address is in use.
pub fn bind_name(key: &str, kind: SockType) -> Result<ListenerRef, ()> {
    let mut names = NAMES.lock();
    if names.get(key).is_some_and(|w| w.strong_count() > 0) {
        return Err(());
    }
    let l = Arc::new(SpinLock::new(Listener { kind, queue: VecDeque::new(), listening: false, backlog: 128, generation: next_gen(), key: key.into() }));
    names.insert(key.into(), Arc::downgrade(&l));
    Ok(l)
}

/// Whether `key` is bound (a live socket holds it).
pub fn name_bound(key: &str) -> bool {
    NAMES.lock().get(key).is_some_and(|w| w.strong_count() > 0)
}

/// Forget the name (the socket file was removed): new connections fail.
pub fn unbind_name(key: &str) {
    NAMES.lock().remove(key);
}

/// Connect to the socket bound at `key`: the client's end, or the error number
/// (ECONNREFUSED 111, EPROTOTYPE 91, EAGAIN 11 with a full backlog).
/// `inet`: a loopback TCP connection's (client port, server port).
pub fn connect_name(key: &str, kind: SockType, inet: Option<(u16, u16)>) -> Result<End, i64> {
    let l = NAMES.lock().get(key).and_then(|w| w.upgrade()).ok_or(111i64)?;
    let mut l = l.lock();
    if !l.listening {
        return Err(111);
    }
    if l.kind != kind {
        return Err(91);
    }
    if l.queue.len() >= l.backlog {
        return Err(11);
    }
    let (mut client, mut server) = socketpair(kind);
    client.inet = inet;
    server.inet = inet.map(|(c, s)| (s, c));
    l.queue.push_back(server);
    l.generation = next_gen();
    drop(l);
    notify();
    Ok(client)
}

// ---------------------------------------------------------------------------
// eventfd

pub struct EventFd {
    pub value: u64,
    pub semaphore: bool,
    pub generation: u64,
}

pub fn eventfd(initial: u64, semaphore: bool) -> Arc<SpinLock<EventFd>> {
    Arc::new(SpinLock::new(EventFd { value: initial, semaphore, generation: next_gen() }))
}

impl EventFd {
    pub fn poll(&self) -> (u32, u64) {
        let mut ev = 0;
        if self.value > 0 {
            ev |= POLLIN;
        }
        if self.value < u64::MAX - 1 {
            ev |= POLLOUT;
        }
        (ev, self.generation)
    }
    pub fn add(&mut self, v: u64) {
        self.value = self.value.saturating_add(v).min(u64::MAX - 1);
        self.generation = next_gen();
    }
    pub fn take(&mut self) -> Option<u64> {
        if self.value == 0 {
            return None;
        }
        let v = if self.semaphore { 1 } else { self.value };
        self.value -= v;
        self.generation = next_gen();
        Some(v)
    }
}

// ---------------------------------------------------------------------------
// epoll

pub struct Interest {
    pub fd: i64,
    pub file: alloc::sync::Weak<SpinLock<crate::linux::OpenFile>>,
    pub events: u32,
    pub data: u64,
    /// Generation last reported (edge-triggered) or u64::MAX (never).
    pub reported: u64,
}

#[derive(Default)]
pub struct Epoll {
    pub interests: Vec<Interest>,
}

pub const EPOLLET: u32 = 1 << 31;
pub const EPOLLONESHOT: u32 = 1 << 30;

// ---------------------------------------------------------------------------
// memfd: shared memory whose pages can be mapped into several places at once.

pub struct SharedMemory {
    /// Page frames (kernel pointers), allocated on first use.
    pub pages: Vec<*mut u8>,
    pub size: u64,
}

// SAFETY: the page pointers are heap pages owned by this object.
unsafe impl Send for SharedMemory {}

pub type SharedRef = Arc<SpinLock<SharedMemory>>;

pub fn memfd() -> SharedRef {
    Arc::new(SpinLock::new(SharedMemory { pages: Vec::new(), size: 0 }))
}

const PAGE: usize = 4096;

impl SharedMemory {
    pub fn page(&mut self, index: usize) -> *mut u8 {
        if index >= self.pages.len() {
            self.pages.resize(index + 1, core::ptr::null_mut());
        }
        if self.pages[index].is_null() {
            // Zeroed like fresh shared memory; out of memory stops the program instead.
            let p = if crate::memory::heap_usage().1 < crate::process::USER_RESERVE { None } else { crate::memory::alloc_page() };
            let Some(p) = p else { return crate::process::oom_scratch_page() };
            self.pages[index] = p;
        }
        self.pages[index]
    }

    pub fn truncate(&mut self, size: u64) {
        self.size = size;
        let keep = (size as usize).div_ceil(PAGE);
        for p in self.pages.drain(keep.min(self.pages.len())..) {
            if !p.is_null() {
                // SAFETY: allocated in `page` with this layout. Mappings beyond the new
                // size are the program's problem, as on Linux (SIGBUS there).
                unsafe { crate::memory::free_page(p) };
            }
        }
    }

    pub fn read(&mut self, off: usize, len: usize) -> Vec<u8> {
        let end = (off + len).min(self.size as usize);
        let mut out = Vec::with_capacity(end.saturating_sub(off));
        let mut pos = off;
        while pos < end {
            let n = (PAGE - pos % PAGE).min(end - pos);
            let p = self.page(pos / PAGE);
            // SAFETY: inside one 4 KiB page.
            out.extend_from_slice(unsafe { core::slice::from_raw_parts(p.add(pos % PAGE), n) });
            pos += n;
        }
        out
    }

    pub fn write(&mut self, off: usize, data: &[u8]) {
        if (off + data.len()) as u64 > self.size {
            self.size = (off + data.len()) as u64;
        }
        let mut done = 0;
        while done < data.len() {
            let pos = off + done;
            let n = (PAGE - pos % PAGE).min(data.len() - done);
            let p = self.page(pos / PAGE);
            // SAFETY: inside one 4 KiB page.
            unsafe { core::ptr::copy_nonoverlapping(data[done..].as_ptr(), p.add(pos % PAGE), n) };
            done += n;
        }
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        for p in self.pages.drain(..) {
            if !p.is_null() {
                // SAFETY: allocated in `page` with this layout; no mapping holds the object
                // any more (mappings keep an Arc).
                unsafe { crate::memory::free_page(p) };
            }
        }
    }
}

/// Kernel-side use of a pipe end (the browser talks to Chromium this way).
impl End {
    /// Queue bytes for the reader. Err(()) if the reader is gone.
    pub fn send_bytes(&self, data: &[u8]) -> Result<(), ()> {
        let tx = self.tx.as_ref().ok_or(())?;
        let mut c = tx.lock();
        if c.reader_closed {
            return Err(());
        }
        c.push(Message { data: data.to_vec(), fds: Vec::new(), pid: 0 });
        drop(c);
        notify();
        Ok(())
    }

    /// Whatever bytes are waiting (possibly none). Err(()) at end of stream.
    pub fn recv_bytes(&self) -> Result<Vec<u8>, ()> {
        let rx = self.rx.as_ref().ok_or(())?;
        let mut c = rx.lock();
        if c.readable() {
            let (d, _) = c.read_stream(usize::MAX);
            drop(c);
            notify();
            return Ok(d);
        }
        if c.writer_closed { Err(()) } else { Ok(Vec::new()) }
    }
}
