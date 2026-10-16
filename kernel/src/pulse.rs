//! A PulseAudio server for Linux programs, playing through the HD Audio driver.
//!
//! Programs that use libpulse (Steam among them) connect to
//! `$XDG_RUNTIME_DIR/pulse/native` = `/run/user/1000/pulse/native`. Without a server there
//! they retry for ever, and every failed attempt leaves a shared memory pool behind, so
//! Nova answers: a kernel task (like the Wayland compositor) that speaks the native
//! protocol. With an HD Audio output ([`crate::drivers::hda`]) it offers one sink,
//! `nova_output` (48 kHz s16le stereo), and plays clients' playback streams on it;
//! without one it is a server with no sinks or sources that refuses streams.
//!
//! Wire format (PulseAudio's pulsecore/pstream.c and tagstruct.c): each packet is a
//! 20-byte descriptor of big-endian u32s (length, channel, offset high, offset low,
//! flags) and `length` bytes; commands use channel 0xFFFFFFFF and carry a tagstruct,
//! a sequence of typed values ('L' u32, 't' string, 'N' null string, 'x' blob, ...)
//! that starts with the command number and a tag the reply repeats. Audio goes the other
//! way on the stream's channel number, as plain data packets: this server offers no
//! shared memory, so clients send samples inline.
//!
//! Playback (pulsecore/protocol-native.c behaviour): CREATE_PLAYBACK_STREAM answers with
//! buffer metrics and a number of bytes the client may send; REQUEST packets ask for more
//! as the queue drains below its target length. A stream starts once its prebuffer is
//! full (or on DRAIN), and the mixer, run from the server loop, keeps about 50 ms queued
//! in the HD Audio buffer: it converts each stream to 48 kHz stereo (u8, s16, s32 and
//! float32 samples, any rate, by linear interpolation), applies stream and sink volume,
//! and adds them up.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::drivers::hda;
use crate::ipc::{self, End, ListenerRef};
use crate::{pit, task};

pub const SOCKET_DIR: &str = "run/user/1000/pulse";
pub const SOCKET: &str = "run/user/1000/pulse/native";

static STARTED: AtomicBool = AtomicBool::new(false);

const CMD_ERROR: u32 = 0;
const CMD_REPLY: u32 = 2;
const CMD_CREATE_PLAYBACK_STREAM: u32 = 3;
const CMD_DELETE_PLAYBACK_STREAM: u32 = 4;
const CMD_AUTH: u32 = 8;
const CMD_SET_CLIENT_NAME: u32 = 9;
const CMD_LOOKUP_SINK: u32 = 10;
const CMD_DRAIN_PLAYBACK_STREAM: u32 = 12;
const CMD_STAT: u32 = 13;
const CMD_GET_PLAYBACK_LATENCY: u32 = 14;
const CMD_GET_SERVER_INFO: u32 = 20;
const CMD_GET_SINK_INFO: u32 = 21;
const CMD_GET_SINK_INFO_LIST: u32 = 22;
const CMD_SUBSCRIBE: u32 = 35;
const CMD_SET_SINK_VOLUME: u32 = 36;
const CMD_SET_SINK_INPUT_VOLUME: u32 = 37;
const CMD_SET_SINK_MUTE: u32 = 39;
const CMD_CORK_PLAYBACK_STREAM: u32 = 41;
const CMD_FLUSH_PLAYBACK_STREAM: u32 = 42;
const CMD_TRIGGER_PLAYBACK_STREAM: u32 = 43;
const CMD_SET_PLAYBACK_STREAM_NAME: u32 = 46;
const CMD_PREBUF_PLAYBACK_STREAM: u32 = 60;
const CMD_REQUEST: u32 = 61;
const CMD_UNDERFLOW: u32 = 63;
const CMD_SET_SINK_INPUT_MUTE: u32 = 69;
const CMD_SET_PLAYBACK_STREAM_BUFFER_ATTR: u32 = 72;
const CMD_UPDATE_PLAYBACK_STREAM_PROPLIST: u32 = 81;
const CMD_UPDATE_CLIENT_PROPLIST: u32 = 82;
const CMD_REMOVE_PLAYBACK_STREAM_PROPLIST: u32 = 84;
const CMD_STARTED: u32 = 86;
/// The other GET_*_INFO_LIST commands (sources, modules, clients, sink inputs, source
/// outputs, samples, cards): all empty here.
const LIST_COMMANDS: [u32; 7] = [24, 26, 28, 30, 32, 34, 89];
const ERR_INVALID: u32 = 3;
const ERR_NOENTITY: u32 = 5;
const ERR_NOTSUPPORTED: u32 = 19;
/// The newest protocol version this server speaks (PulseAudio 15).
const VERSION: u32 = 35;

const SINK_NAME: &str = "nova_output";
const INVALID: u32 = u32::MAX;
/// PA_VOLUME_NORM: 100 %, 0 dB.
const NORM: u32 = 0x10000;
/// Sink flags: HARDWARE | LATENCY.
const SINK_FLAGS: u32 = 0x4 | 0x2;
/// Frames the mixer keeps queued in the HD Audio buffer (50 ms), and the least it mixes
/// at once (5 ms).
const TARGET_FRAMES: usize = hda::RATE as usize / 20;
const MIN_MIX: usize = hda::RATE as usize / 200;
/// Sample formats (pa_sample_format_t) the mixer reads.
const U8: u8 = 0;
const S16LE: u8 = 3;
const S16BE: u8 = 4;
const FLOAT32LE: u8 = 5;
const S32LE: u8 = 7;

static SINK_VOLUME: AtomicU32 = AtomicU32::new(NORM);
static SINK_MUTED: AtomicBool = AtomicBool::new(false);
static NEXT_STREAM: AtomicU32 = AtomicU32::new(0);

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
    // libpulse refuses a runtime directory that belongs to another user than its own
    // (programs run as uid 1000).
    for owned in ["run/user/1000", SOCKET_DIR] {
        let (m, rel) = ns.find_mount(owned);
        let _ = m.fs.chown(&rel, 1000, 1000);
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct Spec {
    format: u8,
    channels: u8,
    rate: u32,
}

impl Spec {
    const SINK: Spec = Spec { format: S16LE, channels: 2, rate: hda::RATE };

    fn sample_size(&self) -> Option<usize> {
        match self.format {
            U8 => Some(1),
            S16LE | S16BE => Some(2),
            FLOAT32LE | S32LE => Some(4),
            _ => None,
        }
    }

    fn supported(&self) -> bool {
        self.sample_size().is_some() && (1..=8).contains(&self.channels) && (8000..=192_000).contains(&self.rate)
    }

    fn frame(&self) -> usize {
        self.sample_size().unwrap_or(1) * self.channels as usize
    }

    /// Bytes for `ms` milliseconds, in whole frames.
    fn bytes(&self, ms: u32) -> u32 {
        (self.rate as u64 * ms as u64 / 1000) as u32 * self.frame() as u32
    }

    fn format_name(&self) -> &'static str {
        match self.format {
            U8 => "u8",
            S16LE => "s16le",
            S16BE => "s16be",
            FLOAT32LE => "float32le",
            _ => "s32le",
        }
    }
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

    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.extend_from_slice(&[b'B', v]);
        self
    }

    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.push(b'R');
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn s64(&mut self, v: i64) -> &mut Self {
        self.0.push(b'r');
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn usec(&mut self, v: u64) -> &mut Self {
        self.0.push(b'U');
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn bool(&mut self, v: bool) -> &mut Self {
        self.0.push(if v { b'1' } else { b'0' });
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

    fn spec(&mut self, s: Spec) -> &mut Self {
        self.0.extend_from_slice(&[b'a', s.format, s.channels]);
        self.0.extend_from_slice(&s.rate.to_be_bytes());
        self
    }

    fn map(&mut self, positions: &[u8]) -> &mut Self {
        self.0.extend_from_slice(&[b'm', positions.len() as u8]);
        self.0.extend_from_slice(positions);
        self
    }

    fn cvolume(&mut self, volume: u32, channels: u8) -> &mut Self {
        self.0.extend_from_slice(&[b'v', channels]);
        for _ in 0..channels {
            self.0.extend_from_slice(&volume.to_be_bytes());
        }
        self
    }

    fn volume(&mut self, v: u32) -> &mut Self {
        self.0.push(b'V');
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }

    fn timeval(&mut self, (sec, usec): (u32, u32)) -> &mut Self {
        self.0.push(b'T');
        self.0.extend_from_slice(&sec.to_be_bytes());
        self.0.extend_from_slice(&usec.to_be_bytes());
        self
    }

    /// A property list of string values.
    fn proplist(&mut self, props: &[(&str, &str)]) -> &mut Self {
        self.0.push(b'P');
        for (key, value) in props {
            self.str(Some(key));
            let len = value.len() as u32 + 1;
            self.u32(len);
            self.0.push(b'x');
            self.0.extend_from_slice(&len.to_be_bytes());
            self.0.extend_from_slice(value.as_bytes());
            self.0.push(0);
        }
        self.str(None)
    }

    /// A PCM format description (pa_format_info); its properties are JSON values.
    fn format_info(&mut self, spec: Option<Spec>) -> &mut Self {
        self.0.push(b'f');
        self.u8(1); // PA_ENCODING_PCM
        match spec {
            Some(s) => {
                let format = alloc::format!("\"{}\"", s.format_name());
                let (rate, channels) = (alloc::format!("{}", s.rate), alloc::format!("{}", s.channels));
                self.proplist(&[("format.sample_format", &format), ("format.rate", &rate), ("format.channels", &channels)])
            }
            None => self.proplist(&[]),
        }
    }
}

/// Reads a client's tagstruct, value by value.
struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn tag(&mut self, t: u8) -> Option<()> {
        (self.data.get(self.at) == Some(&t)).then(|| self.at += 1)
    }

    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let bytes = self.data.get(self.at..self.at + n)?;
        self.at += n;
        Some(bytes)
    }

    fn be32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        self.tag(b'L')?;
        self.be32()
    }

    fn u8(&mut self) -> Option<u8> {
        self.tag(b'B')?;
        Some(self.take(1)?[0])
    }

    fn bool(&mut self) -> Option<bool> {
        match self.data.get(self.at)? {
            b'1' => {
                self.at += 1;
                Some(true)
            }
            b'0' => {
                self.at += 1;
                Some(false)
            }
            _ => None,
        }
    }

    /// A string or null string ('N').
    fn str(&mut self) -> Option<Option<String>> {
        if self.tag(b'N').is_some() {
            return Some(None);
        }
        self.tag(b't')?;
        let rest = self.data.get(self.at..)?;
        let end = rest.iter().position(|&b| b == 0)?;
        let s = String::from_utf8_lossy(&rest[..end]).into_owned();
        self.at += end + 1;
        Some(Some(s))
    }

    fn spec(&mut self) -> Option<Spec> {
        self.tag(b'a')?;
        let b = self.take(2)?;
        let (format, channels) = (b[0], b[1]);
        Some(Spec { format, channels, rate: self.be32()? })
    }

    fn map(&mut self) -> Option<Vec<u8>> {
        self.tag(b'm')?;
        let n = self.take(1)?[0] as usize;
        Some(self.take(n)?.to_vec())
    }

    /// A channel volume list, as its average.
    fn cvolume(&mut self) -> Option<u32> {
        self.tag(b'v')?;
        let n = self.take(1)?[0] as usize;
        let mut sum = 0u64;
        for _ in 0..n {
            sum += self.be32()? as u64;
        }
        Some(if n == 0 { NORM } else { (sum / n as u64) as u32 })
    }

    fn timeval(&mut self) -> Option<(u32, u32)> {
        self.tag(b'T')?;
        Some((self.be32()?, self.be32()?))
    }

    /// Skip a property list.
    fn proplist(&mut self) -> Option<()> {
        self.tag(b'P')?;
        while self.str()?.is_some() {
            self.u32()?;
            self.tag(b'x')?;
            let len = self.be32()? as usize;
            self.take(len)?;
        }
        Some(())
    }

    /// Skip a format description.
    fn format_info(&mut self) -> Option<()> {
        self.tag(b'f')?;
        self.u8()?;
        self.proplist()
    }
}

/// Linear gain (16.16 fixed point) for a PulseAudio volume, which is cubic: 0x10000 is
/// 0 dB, half of that about -18 dB.
fn gain(volume: u32, muted: bool) -> u64 {
    if muted {
        return 0;
    }
    let v = volume.min(4 * NORM) as u64;
    v * v / NORM as u64 * v / NORM as u64
}

/// A client's playback stream.
struct Stream {
    channel: u32,
    spec: Spec,
    map: Vec<u8>,
    /// Received samples not yet mixed: `data[head..]`.
    data: Vec<u8>,
    head: usize,
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    /// Bytes asked for with REQUEST (or at creation) that have not arrived yet.
    requested: u64,
    corked: bool,
    /// Prebuffering is over; the mixer takes samples.
    started: bool,
    /// Tag of a DRAIN to answer, and when (in ticks) once the queue is empty.
    drain: Option<u32>,
    drain_at: Option<u64>,
    write_index: i64,
    read_index: i64,
    /// Resampler: position in source frames (32.32 fixed point) relative to the first
    /// queued frame, and the frame before it.
    pos: u64,
    last: [i32; 2],
    volume: u32,
    muted: bool,
}

impl Stream {
    fn queued(&self) -> usize {
        self.data.len() - self.head
    }

    /// Frames still to be played (beyond the resampler's position).
    fn frames_left(&self) -> usize {
        (self.queued() / self.spec.frame()).saturating_sub((self.pos >> 32) as usize)
    }

    fn playing(&self) -> bool {
        self.started && !self.corked
    }

    /// One sample of queued frame `frame`, channel `ch`, scaled to 16 bits.
    fn sample(&self, frame: usize, ch: usize) -> i32 {
        let size = self.spec.sample_size().unwrap_or(2);
        let ch = ch.min(self.spec.channels as usize - 1);
        let at = self.head + frame * self.spec.frame() + ch * size;
        let b = &self.data[at..at + size];
        match self.spec.format {
            U8 => (b[0] as i32 - 128) << 8,
            S16LE => i16::from_le_bytes([b[0], b[1]]) as i32,
            S16BE => i16::from_be_bytes([b[0], b[1]]) as i32,
            FLOAT32LE => (f32::from_le_bytes([b[0], b[1], b[2], b[3]]).clamp(-1.0, 1.0) * 32767.0) as i32,
            _ => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) >> 16,
        }
    }

    fn frame(&self, frame: usize) -> [i32; 2] {
        [self.sample(frame, 0), self.sample(frame, 1)]
    }

    /// Add up to `frames` frames, converted to 48 kHz stereo and scaled by `gain`, into
    /// `out`; returns the frames produced (fewer if the queue runs out).
    fn mix_into(&mut self, out: &mut [i32], frames: usize, sink_gain: u64) -> usize {
        let avail = self.queued() / self.spec.frame();
        let step = ((self.spec.rate as u64) << 32) / hda::RATE as u64;
        let g = (gain(self.volume, self.muted) * sink_gain >> 16) as i64;
        let mut n = 0;
        while n < frames {
            let i = (self.pos >> 32) as usize;
            if i >= avail {
                break;
            }
            let a = if i == 0 { self.last } else { self.frame(i - 1) };
            let b = self.frame(i);
            let frac = ((self.pos >> 16) & 0xFFFF) as i64;
            for ch in 0..2 {
                let s = a[ch] as i64 + ((b[ch] - a[ch]) as i64 * frac >> 16);
                out[n * 2 + ch] += (s * g >> 16) as i32;
            }
            self.pos += step;
            n += 1;
        }
        let used = ((self.pos >> 32) as usize).min(avail);
        if used > 0 {
            self.last = self.frame(used - 1);
            self.head += used * self.spec.frame();
            self.pos -= (used as u64) << 32;
            self.read_index += (used * self.spec.frame()) as i64;
        }
        if self.head > 1 << 16 && self.head * 2 > self.data.len() {
            self.data.drain(..self.head);
            self.head = 0;
        }
        n
    }

    /// Buffer metrics from what the client asked for (u32::MAX: the server's choice).
    fn set_attr(&mut self, maxlength: u32, tlength: u32, prebuf: u32, minreq: u32) {
        let s = self.spec;
        let frame = s.frame() as u32;
        let round = |v: u32| (v / frame).max(1) * frame;
        self.tlength = if tlength == INVALID || tlength == 0 { s.bytes(200) } else { tlength.clamp(s.bytes(10), s.bytes(2000)) };
        self.tlength = round(self.tlength);
        self.minreq = if minreq == INVALID || minreq == 0 { self.tlength / 4 } else { minreq.min(self.tlength / 2) };
        self.minreq = round(self.minreq);
        self.prebuf = if prebuf == INVALID { self.tlength } else { prebuf.min(self.tlength) };
        self.prebuf = if self.prebuf == 0 { 0 } else { round(self.prebuf) };
        self.maxlength = if maxlength == INVALID || maxlength < self.tlength { 4 << 20 } else { maxlength };
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
    streams: Vec<Stream>,
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

    fn reply(&mut self, tag: u32) {
        self.send(Tags::new(CMD_REPLY, tag));
    }

    fn stream(&mut self, channel: Option<u32>) -> Option<&mut Stream> {
        self.streams.iter_mut().find(|s| Some(s.channel) == channel)
    }

    fn command(&mut self, packet: &[u8]) {
        let mut r = Reader { data: packet, at: 0 };
        let (Some(cmd), Some(tag)) = (r.u32(), r.u32()) else {
            self.dead = true;
            return;
        };
        let audio = hda::present();
        match cmd {
            CMD_AUTH => {
                // The client's version, with flag bits for shared memory (which this
                // server does not offer, so audio data comes over the socket).
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
                t.spec(Spec::SINK);
                t.str(audio.then_some(SINK_NAME)).str(None).u32(0x4e6f7661);
                if self.version >= 15 {
                    t.map(&[1, 2]); // front-left, front-right
                }
                self.send(t);
            }
            CMD_GET_SINK_INFO_LIST => {
                let mut t = Tags::new(CMD_REPLY, tag);
                if audio {
                    self.sink_info(&mut t);
                }
                self.send(t);
            }
            CMD_GET_SINK_INFO => {
                let (index, name) = (r.u32(), r.str());
                if audio && is_sink(index, name.flatten().as_deref()) {
                    let mut t = Tags::new(CMD_REPLY, tag);
                    self.sink_info(&mut t);
                    self.send(t);
                } else {
                    self.error(tag, ERR_NOENTITY);
                }
            }
            CMD_LOOKUP_SINK => match r.str() {
                Some(name) if audio && is_sink(Some(INVALID), name.as_deref()) => {
                    let mut t = Tags::new(CMD_REPLY, tag);
                    t.u32(0);
                    self.send(t);
                }
                _ => self.error(tag, ERR_NOENTITY),
            },
            CMD_CREATE_PLAYBACK_STREAM if audio => match self.create_stream(&mut r) {
                Ok(i) => self.stream_created(tag, i),
                Err(code) => self.error(tag, code),
            },
            CMD_DELETE_PLAYBACK_STREAM => {
                let channel = r.u32();
                let before = self.streams.len();
                self.streams.retain(|s| Some(s.channel) != channel);
                if self.streams.len() < before { self.reply(tag) } else { self.error(tag, ERR_NOENTITY) }
            }
            CMD_DRAIN_PLAYBACK_STREAM => match self.stream(r.u32()) {
                Some(s) => {
                    // Answered by `service` once everything queued has played.
                    if let Some(old) = s.drain.replace(tag) {
                        self.reply(old);
                    }
                }
                None => self.error(tag, ERR_NOENTITY),
            },
            CMD_CORK_PLAYBACK_STREAM => {
                let (channel, cork) = (r.u32(), r.bool());
                match (self.stream(channel), cork) {
                    (Some(s), Some(cork)) => {
                        s.corked = cork;
                        self.reply(tag);
                    }
                    _ => self.error(tag, ERR_NOENTITY),
                }
            }
            CMD_FLUSH_PLAYBACK_STREAM | CMD_TRIGGER_PLAYBACK_STREAM | CMD_PREBUF_PLAYBACK_STREAM => match self.stream(r.u32()) {
                Some(s) => {
                    match cmd {
                        CMD_FLUSH_PLAYBACK_STREAM => {
                            s.read_index += s.queued() as i64;
                            s.head = s.data.len();
                            s.pos = 0;
                            s.started = s.prebuf == 0;
                        }
                        CMD_TRIGGER_PLAYBACK_STREAM => s.started = true,
                        _ => s.started = false,
                    }
                    self.reply(tag);
                }
                None => self.error(tag, ERR_NOENTITY),
            },
            CMD_GET_PLAYBACK_LATENCY => {
                let (channel, local) = (r.u32(), r.timeval().unwrap_or((0, 0)));
                let version = self.version;
                let Some(s) = self.stream(channel) else { return self.error(tag, ERR_NOENTITY) };
                let sink_usec = hda::queued() as u64 * 1_000_000 / hda::RATE as u64;
                let mut t = Tags::new(CMD_REPLY, tag);
                // Sink latency, source latency, playing, the client's time echoed as both
                // its own and the server's (Nova keeps no wall clock in common with it).
                t.usec(sink_usec).usec(0).bool(s.playing()).timeval(local).timeval(local);
                t.s64(s.write_index).s64(s.read_index);
                if version >= 13 {
                    t.u64(0).u64(if s.playing() { s.read_index as u64 } else { 0 }); // underrun for, playing for
                }
                self.send(t);
            }
            CMD_SET_PLAYBACK_STREAM_BUFFER_ATTR => {
                let channel = r.u32();
                let attr = (r.u32(), r.u32(), r.u32(), r.u32());
                let version = self.version;
                let (Some(s), (Some(max), Some(tl), Some(pre), Some(min))) = (self.stream(channel), attr) else {
                    return self.error(tag, ERR_NOENTITY);
                };
                s.set_attr(max, tl, pre, min);
                let mut t = Tags::new(CMD_REPLY, tag);
                t.u32(s.maxlength).u32(s.tlength).u32(s.prebuf).u32(s.minreq);
                if version >= 13 {
                    t.usec(sink_latency_usec());
                }
                self.send(t);
            }
            CMD_SET_SINK_VOLUME | CMD_SET_SINK_MUTE if audio => {
                let (index, name) = (r.u32(), r.str());
                if !is_sink(index, name.flatten().as_deref()) {
                    return self.error(tag, ERR_NOENTITY);
                }
                match cmd {
                    CMD_SET_SINK_VOLUME => r.cvolume().map(|v| SINK_VOLUME.store(v, Ordering::Relaxed)),
                    _ => r.bool().map(|m| SINK_MUTED.store(m, Ordering::Relaxed)),
                };
                self.reply(tag);
            }
            CMD_SET_SINK_INPUT_VOLUME | CMD_SET_SINK_INPUT_MUTE => {
                let index = r.u32();
                let (volume, mute) = if cmd == CMD_SET_SINK_INPUT_VOLUME { (r.cvolume(), None) } else { (None, r.bool()) };
                match self.stream(index) {
                    Some(s) => {
                        s.volume = volume.unwrap_or(s.volume);
                        s.muted = mute.unwrap_or(s.muted);
                        self.reply(tag);
                    }
                    None => self.error(tag, ERR_NOENTITY),
                }
            }
            CMD_SET_PLAYBACK_STREAM_NAME | CMD_UPDATE_PLAYBACK_STREAM_PROPLIST | CMD_REMOVE_PLAYBACK_STREAM_PROPLIST | CMD_UPDATE_CLIENT_PROPLIST => {
                self.reply(tag)
            }
            CMD_STAT => {
                let mut t = Tags::new(CMD_REPLY, tag);
                t.u32(0).u32(0).u32(0).u32(0).u32(0); // memory block statistics
                self.send(t);
            }
            CMD_SUBSCRIBE => self.reply(tag),
            c if LIST_COMMANDS.contains(&c) => self.reply(tag),
            // LOOKUP_SOURCE, GET_SOURCE_INFO .. GET_SAMPLE_INFO, GET_CARD_INFO: nothing.
            11 | 23 | 25 | 27 | 29 | 31 | 33 | 88 => self.error(tag, ERR_NOENTITY),
            _ => self.error(tag, ERR_NOTSUPPORTED),
        }
    }

    /// The one sink's description (a GET_SINK_INFO reply, or the list's only entry).
    fn sink_info(&self, t: &mut Tags) {
        let description = alloc::format!("Built-in audio ({})", hda::codec_name().unwrap_or_default());
        let latency = hda::queued() as u64 * 1_000_000 / hda::RATE as u64;
        let volume = SINK_VOLUME.load(Ordering::Relaxed);
        t.u32(0).str(Some(SINK_NAME)).str(Some(&description)).spec(Spec::SINK).map(&[1, 2]);
        t.u32(INVALID).cvolume(volume, 2).bool(SINK_MUTED.load(Ordering::Relaxed));
        t.u32(INVALID).str(None).usec(latency).str(Some("nova-hda")).u32(SINK_FLAGS);
        if self.version >= 13 {
            t.proplist(&[("device.description", &description), ("device.class", "sound"), ("device.api", "hda")]);
            t.usec(sink_latency_usec());
        }
        if self.version >= 15 {
            let running = latency > 0;
            // Base volume, state (running/idle), volume steps, card index.
            t.volume(NORM).u32(if running { 0 } else { 1 }).u32(NORM + 1).u32(INVALID);
        }
        if self.version >= 16 {
            t.u32(0).str(None); // no ports, no active port
        }
        if self.version >= 21 {
            t.u8(1).format_info(None);
        }
    }

    /// Parse CREATE_PLAYBACK_STREAM; the new stream's position in `streams`.
    fn create_stream(&mut self, r: &mut Reader) -> Result<usize, u32> {
        let v = self.version;
        let bad = ERR_INVALID;
        if v < 13 {
            r.str().ok_or(bad)?; // name (in the property list since)
        }
        let spec = r.spec().ok_or(bad)?;
        let map = r.map().ok_or(bad)?;
        let (sink_index, sink_name) = (r.u32().ok_or(bad)?, r.str().ok_or(bad)?);
        let maxlength = r.u32().ok_or(bad)?;
        let corked = r.bool().ok_or(bad)?;
        let (tlength, prebuf, minreq) = (r.u32().ok_or(bad)?, r.u32().ok_or(bad)?, r.u32().ok_or(bad)?);
        r.u32().ok_or(bad)?; // sync group
        let mut volume = r.cvolume().ok_or(bad)?;
        let mut muted = false;
        if v >= 12 {
            for _ in 0..7 {
                r.bool().ok_or(bad)?; // no_remap .. variable_rate
            }
        }
        if v >= 13 {
            muted = r.bool().ok_or(bad)?;
            r.bool().ok_or(bad)?; // adjust latency
            r.proplist().ok_or(bad)?;
        }
        if v >= 14 {
            if !r.bool().ok_or(bad)? {
                volume = NORM; // volume_set: false means the client sent no volume
            }
            r.bool().ok_or(bad)?; // early requests
        }
        if v >= 15 {
            if !r.bool().ok_or(bad)? {
                muted = false; // muted_set
            }
            r.bool().ok_or(bad)?; // don't inhibit auto suspend
            r.bool().ok_or(bad)?; // fail on suspend
        }
        if v >= 17 {
            r.bool().ok_or(bad)?; // relative volume
        }
        if v >= 18 {
            r.bool().ok_or(bad)?; // passthrough
        }
        if v >= 21 {
            for _ in 0..r.u8().ok_or(bad)? {
                r.format_info().ok_or(bad)?;
            }
        }
        if sink_index != INVALID && sink_index != 0 || !is_sink(Some(INVALID), sink_name.as_deref()) {
            return Err(ERR_NOENTITY);
        }
        if !spec.supported() || map.len() != spec.channels as usize {
            return Err(ERR_NOTSUPPORTED);
        }
        let mut s = Stream {
            channel: NEXT_STREAM.fetch_add(1, Ordering::Relaxed),
            spec,
            map,
            data: Vec::new(),
            head: 0,
            maxlength: 0,
            tlength: 0,
            prebuf: 0,
            minreq: 0,
            requested: 0,
            corked,
            started: false,
            drain: None,
            drain_at: None,
            write_index: 0,
            read_index: 0,
            pos: 0,
            last: [0, 0],
            volume,
            muted,
        };
        s.set_attr(maxlength, tlength, prebuf, minreq);
        s.started = s.prebuf == 0;
        s.requested = s.tlength as u64;
        self.streams.push(s);
        Ok(self.streams.len() - 1)
    }

    fn stream_created(&mut self, tag: u32, i: usize) {
        let v = self.version;
        let s = &self.streams[i];
        let mut t = Tags::new(CMD_REPLY, tag);
        // Channel, sink input index, bytes the client may send now.
        t.u32(s.channel).u32(s.channel).u32(s.tlength);
        if v >= 9 {
            t.u32(s.maxlength).u32(s.tlength).u32(s.prebuf).u32(s.minreq);
        }
        if v >= 12 {
            t.spec(s.spec).map(&s.map).u32(0).str(Some(SINK_NAME)).bool(false);
        }
        if v >= 13 {
            t.usec(sink_latency_usec());
        }
        if v >= 21 {
            t.format_info(Some(s.spec));
        }
        self.send(t);
    }

    /// Audio data for one of this client's streams.
    fn data(&mut self, channel: u32, flags: u32, payload: &[u8]) {
        let Some(s) = self.stream(Some(channel)) else { return };
        if flags & 0x8000_0000 != 0 {
            return; // a shared memory block reference: never offered, so not expected
        }
        let room = (s.maxlength as usize).saturating_sub(s.queued());
        let n = payload.len().min(room);
        s.data.extend_from_slice(&payload[..n]);
        s.write_index += payload.len() as i64;
        s.requested = s.requested.saturating_sub(payload.len() as u64);
    }

    /// Per-stream bookkeeping after mixing: start, underflow, drain replies, requests.
    fn service(&mut self) {
        let now = pit::ticks();
        let mut out: Vec<Tags> = Vec::new();
        let version = self.version;
        for s in self.streams.iter_mut() {
            let empty = s.frames_left() == 0;
            if !s.started && !s.corked && !empty && (s.queued() >= s.prebuf as usize || s.drain.is_some()) {
                s.started = true;
                if version >= 13 {
                    let mut t = Tags::new(CMD_STARTED, u32::MAX);
                    t.u32(s.channel);
                    out.push(t);
                }
            } else if s.playing() && empty && s.drain.is_none() && s.prebuf > 0 {
                // Ran dry: prebuffer again and tell the client.
                s.started = false;
                let mut t = Tags::new(CMD_UNDERFLOW, u32::MAX);
                t.u32(s.channel);
                if version >= 23 {
                    t.s64(s.read_index);
                }
                out.push(t);
            }
            if let Some(tag) = s.drain {
                if !empty || s.corked {
                    s.drain_at = None;
                } else {
                    // Everything is in the HD Audio buffer: answer once it has played.
                    let at = *s.drain_at.get_or_insert(now + hda::queued() as u64 * pit::HZ / hda::RATE as u64 + 1);
                    if now >= at {
                        s.drain = None;
                        s.drain_at = None;
                        out.push(Tags::new(CMD_REPLY, tag));
                    }
                }
            }
            let missing = (s.tlength as u64).saturating_sub(s.queued() as u64 + s.requested);
            if missing >= s.minreq as u64 && missing > 0 {
                s.requested += missing;
                let mut t = Tags::new(CMD_REQUEST, u32::MAX);
                t.u32(s.channel).u32(missing as u32);
                out.push(t);
            }
        }
        for t in out {
            self.send(t);
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

    /// Handle every complete packet in the input buffer.
    fn dispatch(&mut self) {
        let mut at = 0;
        while self.inbuf.len() - at >= 20 && !self.dead {
            let word = |i: usize| u32::from_be_bytes(self.inbuf[at + i * 4..at + i * 4 + 4].try_into().unwrap());
            let (len, channel, flags) = (word(0) as usize, word(1), word(4));
            if len > 1 << 24 {
                self.dead = true;
                break;
            }
            if self.inbuf.len() - at < 20 + len {
                break;
            }
            let packet: Vec<u8> = self.inbuf[at + 20..at + 20 + len].to_vec();
            at += 20 + len;
            if channel == u32::MAX {
                self.command(&packet);
            } else {
                self.data(channel, flags, &packet);
            }
        }
        self.inbuf.drain(..at);
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

/// Whether a sink index/name pair names `nova_output` (no name and no index: the default).
fn is_sink(index: Option<u32>, name: Option<&str>) -> bool {
    match (index, name) {
        (Some(0), _) => true,
        (Some(INVALID), None) => true,
        (Some(INVALID), Some(n)) => n == SINK_NAME || n == "@DEFAULT_SINK@",
        _ => false,
    }
}

/// The latency the mixer aims for, in microseconds.
fn sink_latency_usec() -> u64 {
    TARGET_FRAMES as u64 * 1_000_000 / hda::RATE as u64
}

/// Top up the HD Audio buffer with the playing streams, mixed. True if any stream plays.
fn mix(clients: &mut [Client]) -> bool {
    let playing = clients.iter().any(|c| c.streams.iter().any(|s| s.playing()));
    if !playing {
        return false;
    }
    let queued = hda::queued();
    if queued >= TARGET_FRAMES {
        return true;
    }
    let frames = (TARGET_FRAMES - queued).min(hda::available());
    if frames < MIN_MIX {
        return true;
    }
    let mut acc = vec![0i32; frames * 2];
    let sink_gain = gain(SINK_VOLUME.load(Ordering::Relaxed), SINK_MUTED.load(Ordering::Relaxed));
    for s in clients.iter_mut().flat_map(|c| c.streams.iter_mut()).filter(|s| s.playing()) {
        s.mix_into(&mut acc, frames, sink_gain);
    }
    let samples: Vec<i16> = acc.iter().map(|&v| v.clamp(i16::MIN as i32, i16::MAX as i32) as i16).collect();
    hda::write(&samples);
    true
}

fn serve(listener: ListenerRef) {
    let mut clients: Vec<Client> = Vec::new();
    let mut next_index = 0;
    loop {
        while let Some(end) = listener.lock().queue.pop_front() {
            clients.push(Client {
                end,
                inbuf: Vec::new(),
                out: Vec::new(),
                version: VERSION,
                index: next_index,
                dead: false,
                streams: Vec::new(),
            });
            next_index += 1;
        }
        for c in clients.iter_mut() {
            if !c.receive() {
                c.dead = true;
            }
            c.dispatch();
        }
        let active = mix(&mut clients);
        let streams = clients.iter().any(|c| !c.streams.is_empty());
        for c in clients.iter_mut() {
            c.service();
            c.flush();
        }
        clients.retain(|c| !c.dead);
        // While streams exist the mixer runs every few milliseconds.
        let wait = if active || streams { pit::HZ / 200 } else { pit::HZ };
        ipc::wait(Some(pit::ticks() + wait.max(1)));
    }
}
