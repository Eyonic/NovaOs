//! A PulseAudio server with no sound devices, for Linux programs.
//!
//! Programs that use libpulse (Steam among them) connect to
//! `$XDG_RUNTIME_DIR/pulse/native` = `/run/user/1000/pulse/native`. Without a server there
//! they retry for ever, and every failed attempt leaves a shared memory pool behind, so
//! Nova answers instead: a kernel task (like the Wayland compositor) that speaks the
//! native protocol well enough for a client to connect and see a server with no sinks or
//! sources. Streams are refused; there is no audio output yet.
//!
//! Wire format (PulseAudio's pulsecore/pstream.c and tagstruct.c): each packet is a
//! 20-byte descriptor of big-endian u32s (length, channel, offset high, offset low,
//! flags) and `length` bytes; commands use channel 0xFFFFFFFF and carry a tagstruct,
//! a sequence of typed values ('L' u32, 't' string, 'N' null string, 'x' blob, ...)
//! that starts with the command number and a tag the reply repeats.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::ipc::{self, End, ListenerRef};
use crate::{pit, task};

pub const SOCKET_DIR: &str = "run/user/1000/pulse";
pub const SOCKET: &str = "run/user/1000/pulse/native";

static STARTED: AtomicBool = AtomicBool::new(false);

const CMD_ERROR: u32 = 0;
const CMD_REPLY: u32 = 2;
const CMD_AUTH: u32 = 8;
const CMD_SET_CLIENT_NAME: u32 = 9;
const CMD_GET_SERVER_INFO: u32 = 20;
const CMD_SUBSCRIBE: u32 = 35;
/// The GET_*_INFO_LIST commands (sinks, sources, modules, clients, sink inputs, source
/// outputs, samples, cards): all empty here.
const LIST_COMMANDS: [u32; 8] = [22, 24, 26, 28, 30, 32, 34, 89];
const ERR_NOENTITY: u32 = 5;
const ERR_NOTSUPPORTED: u32 = 19;
/// The newest protocol version this server speaks (PulseAudio 15).
const VERSION: u32 = 35;

/// Start the server (once): bind the socket and serve clients in a task.
pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let ns = crate::vfs::root();
    let (m, rel) = ns.find_mount(SOCKET_DIR);
    let mut dir = String::new();
    for part in rel.split('/') {
        dir = crate::vfs::join(&dir, part);
        let _ = m.fs.mkdir(&dir, 0o700);
    }
    let (m, rel) = ns.find_mount(SOCKET);
    let _ = m.fs.create(&rel, 0o755);
    let Ok(listener) = ipc::bind_name(&crate::linux::root_socket_key(SOCKET), ipc::SockType::Stream) else {
        crate::log::warn("pulse: the socket address is in use");
        return;
    };
    listener.lock().listening = true;
    task::spawn("pulse", move || serve(listener));
}

/// A tagstruct being built.
struct Tags(Vec<u8>);

impl Tags {
    fn new(command: u32, tag: u32) -> Self {
        let mut t = Tags(Vec::new());
        t.u32(command).u32(tag);
        t
    }

    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.push(b'L');
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn str(&mut self, s: Option<&str>) -> &mut Self {
        match s {
            Some(s) => {
                self.0.push(b't');
                self.0.extend_from_slice(s.as_bytes());
                self.0.push(0);
            }
            None => self.0.push(b'N'),
        }
        self
    }
}

/// Reads the leading values of a client's tagstruct.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn u32(&mut self) -> Option<u32> {
        if self.data.get(self.at) != Some(&b'L') {
            return None;
        }
        let v = u32::from_be_bytes(self.data.get(self.at + 1..self.at + 5)?.try_into().ok()?);
        self.at += 5;
        Some(v)
    }
}

struct Client {
    end: End,
    inbuf: Vec<u8>,
    out: Vec<u8>,
    /// Protocol version agreed in AUTH.
    version: u32,
    index: u32,
    dead: bool,
}

impl Client {
    fn send(&mut self, t: Tags) {
        for v in [t.0.len() as u32, u32::MAX, 0, 0, 0] {
            self.out.extend_from_slice(&v.to_be_bytes());
        }
        self.out.extend_from_slice(&t.0);
    }

    fn error(&mut self, tag: u32, code: u32) {
        let mut t = Tags::new(CMD_ERROR, tag);
        t.u32(code);
        self.send(t);
    }

    fn command(&mut self, packet: &[u8]) {
        let mut r = Reader { data: packet, at: 0 };
        let (Some(cmd), Some(tag)) = (r.u32(), r.u32()) else {
            self.dead = true;
            return;
        };
        match cmd {
            CMD_AUTH => {
                // The client's version, with flag bits for shared memory (which this
                // server does not offer, so audio data would go over the socket).
                let theirs = r.u32().unwrap_or(VERSION) & 0xFFFF;
                self.version = theirs.min(VERSION);
                let mut t = Tags::new(CMD_REPLY, tag);
                t.u32(self.version);
                self.send(t);
            }
            CMD_SET_CLIENT_NAME => {
                let mut t = Tags::new(CMD_REPLY, tag);
                if self.version >= 13 {
                    t.u32(self.index);
                }
                self.send(t);
            }
            CMD_GET_SERVER_INFO => {
                let mut t = Tags::new(CMD_REPLY, tag);
                t.str(Some("pulseaudio")).str(Some("15.0.0 (Nova)")).str(Some("nova")).str(Some("nova"));
                // Sample spec: s16le (3), 2 channels, 48 kHz.
                t.0.extend_from_slice(&[b'a', 3, 2]);
                t.0.extend_from_slice(&48000u32.to_be_bytes());
                t.str(None).str(None).u32(0x4e6f7661);
                if self.version >= 15 {
                    // Channel map: front-left (1), front-right (2).
                    t.0.extend_from_slice(&[b'm', 2, 1, 2]);
                }
                self.send(t);
            }
            CMD_SUBSCRIBE => self.send(Tags::new(CMD_REPLY, tag)),
            c if LIST_COMMANDS.contains(&c) => self.send(Tags::new(CMD_REPLY, tag)),
            // GET_SINK_INFO .. GET_SAMPLE_INFO, GET_CARD_INFO, LOOKUP_SINK/SOURCE: nothing.
            10 | 11 | 21 | 23 | 25 | 27 | 29 | 31 | 33 | 88 => self.error(tag, ERR_NOENTITY),
            _ => self.error(tag, ERR_NOTSUPPORTED),
        }
    }

    /// Read what the client sent; false once it hung up.
    fn receive(&mut self) -> bool {
        let Some(rx) = &self.end.rx else { return false };
        loop {
            let mut c = rx.lock();
            if !c.readable() {
                return !c.writer_closed;
            }
            let (data, _fds) = c.read_stream(1 << 16);
            drop(c);
            self.inbuf.extend_from_slice(&data);
        }
    }

    /// Handle every complete packet in the input buffer (audio data is dropped).
    fn dispatch(&mut self) {
        while self.inbuf.len() >= 20 && !self.dead {
            let word = |i: usize| u32::from_be_bytes(self.inbuf[i * 4..i * 4 + 4].try_into().unwrap());
            let (len, channel) = (word(0) as usize, word(1));
            if len > 1 << 24 {
                self.dead = true;
                break;
            }
            if self.inbuf.len() < 20 + len {
                break;
            }
            let packet: Vec<u8> = self.inbuf[20..20 + len].to_vec();
            self.inbuf.drain(..20 + len);
            if channel == u32::MAX {
                self.command(&packet);
            }
        }
    }

    fn flush(&mut self) {
        if self.out.is_empty() {
            return;
        }
        let Some(tx) = &self.end.tx else { return };
        let mut c = tx.lock();
        if c.reader_closed {
            self.dead = true;
            return;
        }
        c.push(ipc::Message { data: core::mem::take(&mut self.out), fds: Vec::new(), pid: 0 });
        drop(c);
        ipc::notify();
    }
}

fn serve(listener: ListenerRef) {
    let mut clients: Vec<Client> = Vec::new();
    let mut next_index = 0;
    loop {
        while let Some(end) = listener.lock().queue.pop_front() {
            clients.push(Client { end, inbuf: Vec::new(), out: Vec::new(), version: VERSION, index: next_index, dead: false });
            next_index += 1;
        }
        for c in clients.iter_mut() {
            if !c.receive() {
                c.dead = true;
            }
            c.dispatch();
            c.flush();
        }
        clients.retain(|c| !c.dead);
        ipc::wait(Some(pit::ticks() + pit::HZ));
    }
}
