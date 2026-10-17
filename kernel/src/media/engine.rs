//! The decoding engine: `bin/novamedia` (userland/media/novamedia.c, a libVLC program
//! that the image carries when it was built with NOVA_MEDIA=1), one process per player.
//!
//! The kernel starts it with two pipes (commands on fd 3, events on fd 4, both one line
//! per message) and reads its video frames straight out of the memfd the program maps
//! for them: the program renders into one of two slots, writes `frame <slot>` and the
//! app copies that slot into a `Canvas` ([`Engine::frame`]). Audio never passes through
//! here: libVLC plays it through Nova's PulseAudio server (pulse.rs).

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::gfx::Canvas;
use crate::ipc::{self, SharedRef};
use crate::process::{self, Process};
use crate::{fs, linux, pit};

const PROGRAM: &str = "bin/novamedia";

/// The engine's playback state.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PlayState {
    #[default]
    Idle,
    Opening,
    Playing,
    Paused,
    Stopped,
    Ended,
    Error,
}

/// One selectable track of the open media (audio, subtitle or video).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    pub kind: char,
    pub id: i32,
    pub name: String,
}

/// `nova.media_trace=1` on the kernel command line logs every message between the apps
/// and their engines to the serial console.
fn trace() -> bool {
    static ON: spin::Once<bool> = spin::Once::new();
    *ON.call_once(|| crate::setup::cmdline_flag("nova.media_trace") == Some("1"))
}

/// Whether the engine program is installed in this image.
pub fn available() -> bool {
    fs::exists(PROGRAM)
}

pub struct Engine {
    process: Arc<Process>,
    to: ipc::End,
    from: ipc::End,
    buf: Vec<u8>,
    shm: Option<SharedRef>,
    /// (width, height, pitch, slot bytes) of the frames in the shared buffer.
    format: Option<(usize, usize, usize, usize)>,
    /// The visible picture size from the media's video track (frames carry padding).
    pub video_size: Option<(usize, usize)>,
    pub state: PlayState,
    pub time_ms: i64,
    pub length_ms: i64,
    /// Time the last `time` event arrived (ticks), to interpolate between events.
    time_at: u64,
    pub has_audio: bool,
    pub meta: BTreeMap<String, String>,
    pub tracks: Vec<Track>,
    pub audio_track: i32,
    pub subtitle_track: i32,
    pub presets: Vec<String>,
    /// The newest frame and its sequence number (apps redraw when it changes).
    pub frame: Option<Arc<Canvas>>,
    pub frame_seq: u64,
    pub frames: u64,
    pub ready: bool,
    pub error: Option<String>,
    /// `done <what>` lines not yet consumed by a synchronous command.
    done: Vec<String>,
    tracks_dirty: bool,
}

impl Engine {
    /// Start the engine program. Err if it is not installed or did not start.
    pub fn start() -> Result<Engine, String> {
        if !available() {
            return Err("the media engine is not installed in this image (build with NOVA_MEDIA=1)".into());
        }
        crate::pulse::start();
        let (program_reads, to) = linux::kernel_pipe(true);
        let (program_writes, from) = linux::kernel_pipe(false);
        let process = process::spawn(PROGRAM, alloc::vec!["novamedia".to_string()], alloc::vec![(3, program_reads), (4, program_writes)], None)?;
        let mut e = Engine {
            process,
            to,
            from,
            buf: Vec::new(),
            shm: None,
            format: None,
            video_size: None,
            state: PlayState::Idle,
            time_ms: 0,
            length_ms: 0,
            time_at: 0,
            has_audio: false,
            meta: BTreeMap::new(),
            tracks: Vec::new(),
            audio_track: -1,
            subtitle_track: -1,
            presets: Vec::new(),
            frame: None,
            frame_seq: 0,
            frames: 0,
            ready: false,
            error: None,
            done: Vec::new(),
            tracks_dirty: false,
        };
        // Wait for it to come up (libVLC loads its plugins), at most a few seconds.
        let end = pit::ticks() + 8 * pit::HZ;
        while !e.ready && pit::ticks() < end {
            if !e.pump() {
                break;
            }
            if e.error.is_some() {
                break;
            }
            ipc::wait(Some(pit::ticks() + pit::HZ / 50));
        }
        if !e.ready {
            return Err(e.error.clone().unwrap_or_else(|| "the media engine did not start".into()));
        }
        e.send("presets");
        Ok(e)
    }

    pub fn send(&mut self, cmd: &str) {
        if trace() {
            crate::log::info(&format!("media engine [{}] <- {cmd}", self.process.pid));
        }
        let mut line = String::from(cmd);
        line.push('\n');
        let _ = self.to.send_bytes(line.as_bytes());
    }

    /// Open `path` (a NovaFS or system path without the leading slash) and start playing
    /// at `start_ms`, paused if `paused`.
    pub fn open(&mut self, path: &str, start_ms: i64, paused: bool) {
        self.reset_media();
        self.frame = None;
        self.state = PlayState::Opening;
        let cmd = if paused { "open-paused" } else { "open" };
        self.send(&format!("{cmd} /{} {}", path.trim_start_matches('/'), start_ms.max(0)));
        if start_ms > 0 {
            self.time_ms = start_ms;
            self.time_at = pit::ticks();
        }
    }

    fn reset_media(&mut self) {
        self.meta.clear();
        self.tracks.clear();
        self.video_size = None;
        self.has_audio = false;
        self.time_ms = 0;
        self.length_ms = 0;
        self.audio_track = -1;
        self.subtitle_track = -1;
        self.error = None;
        self.frames = 0;
    }

    pub fn play(&mut self) {
        self.send("play");
    }
    pub fn toggle(&mut self) {
        match self.state {
            PlayState::Playing => self.send("pause"),
            PlayState::Ended => {
                self.send("seek 0");
                self.send("play");
            }
            _ => self.send("play"),
        }
    }
    pub fn stop(&mut self) {
        self.send("stop");
        self.state = PlayState::Stopped;
    }
    pub fn seek(&mut self, ms: i64) {
        let ms = ms.clamp(0, self.length_ms.max(0));
        self.send(&format!("seek {ms}"));
        self.time_ms = ms;
        self.time_at = pit::ticks();
    }
    pub fn set_rate(&mut self, percent: i32) {
        self.send(&format!("rate {}", percent.clamp(25, 400)));
    }
    pub fn set_volume(&mut self, percent: i32) {
        self.send(&format!("volume {}", percent.clamp(0, 200)));
    }
    pub fn set_mute(&mut self, mute: bool) {
        self.send(&format!("mute {}", mute as i32));
    }
    pub fn set_track(&mut self, kind: char, id: i32) {
        self.send(&format!("{}track {id}", kind));
        match kind {
            'a' => self.audio_track = id,
            's' => self.subtitle_track = id,
            _ => {}
        }
    }
    pub fn set_subtitle_delay(&mut self, ms: i64) {
        self.send(&format!("sdelay {ms}"));
    }
    pub fn set_audio_delay(&mut self, ms: i64) {
        self.send(&format!("adelay {ms}"));
    }
    pub fn set_equalizer(&mut self, preset: i32) {
        self.send(&format!("eq {preset}"));
    }
    /// Forward libVLC's messages to the kernel log: 0 errors, 1 warnings, 2 everything.
    pub fn set_debug(&mut self, level: u32) {
        self.send(&format!("debug {level}"));
    }
    pub fn add_subtitle(&mut self, path: &str) {
        self.send(&format!("sub /{}", path.trim_start_matches('/')));
    }

    /// The playback position now: the last reported time plus what has elapsed since.
    pub fn position_ms(&self) -> i64 {
        if self.state == PlayState::Playing && self.time_at > 0 {
            (self.time_ms + (pit::ticks() - self.time_at) as i64).min(self.length_ms.max(self.time_ms))
        } else {
            self.time_ms
        }
    }

    /// Tracks of one kind (`'a'` audio, `'s'` subtitles, `'v'` video).
    pub fn tracks_of(&self, kind: char) -> impl Iterator<Item = &Track> {
        self.tracks.iter().filter(move |t| t.kind == kind)
    }

    /// Read everything the engine sent; false once it is gone. Returns true while the
    /// connection lives (whether or not something arrived; see `frame_seq`, `state`).
    pub fn pump(&mut self) -> bool {
        let Ok(data) = self.from.recv_bytes() else { return false };
        if data.is_empty() {
            return true;
        }
        self.buf.extend_from_slice(&data);
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            self.handle(&line);
        }
        true
    }

    fn handle(&mut self, line: &str) {
        if trace() {
            crate::log::info(&format!("media engine [{}]: {line}", self.process.pid));
        }
        let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
        match cmd {
            "ready" => self.ready = true,
            "shm" => {
                let fd: i64 = rest.split(' ').next().and_then(|s| s.parse().ok()).unwrap_or(5);
                let file = self.process.fds.lock().get(fd).ok();
                self.shm = file.as_ref().and_then(linux::shared_memory);
                if self.shm.is_none() {
                    self.error = Some("no shared frame buffer".into());
                }
            }
            "format" => {
                let v: Vec<usize> = rest.split(' ').filter_map(|s| s.parse().ok()).collect();
                if v.len() == 4 {
                    self.format = Some((v[0], v[1], v[2], v[3]));
                }
            }
            "video" => {
                let v: Vec<usize> = rest.split(' ').filter_map(|s| s.parse().ok()).collect();
                if v.len() == 2 && v[0] > 0 && v[1] > 0 {
                    self.video_size = Some((v[0], v[1]));
                }
            }
            "hasaudio" => self.has_audio = true,
            "frame" => {
                let v: Vec<usize> = rest.split(' ').filter_map(|s| s.parse().ok()).collect();
                if v.len() == 3 {
                    self.take_frame(v[0], v[1], v[2]);
                }
            }
            "time" => {
                if let Ok(t) = rest.parse::<i64>() {
                    self.time_ms = t;
                    self.time_at = pit::ticks();
                }
            }
            "length" => {
                if let Ok(t) = rest.parse::<i64>()
                    && t > 0
                {
                    self.length_ms = t;
                }
            }
            "state" => {
                self.state = match rest {
                    "opening" => PlayState::Opening,
                    "playing" => PlayState::Playing,
                    "paused" => PlayState::Paused,
                    "stopped" => PlayState::Stopped,
                    "ended" => PlayState::Ended,
                    "error" => PlayState::Error,
                    _ => self.state,
                };
                if self.state == PlayState::Ended {
                    self.time_ms = self.length_ms;
                }
                if self.state == PlayState::Playing && self.tracks.is_empty() {
                    self.send("meta");
                    self.send("tracks");
                }
            }
            "meta" => {
                if let Some((k, v)) = rest.split_once(' ') {
                    self.meta.insert(k.into(), v.into());
                }
            }
            "track" => {
                let mut it = rest.splitn(3, ' ');
                if let (Some(k), Some(id), name) = (it.next(), it.next(), it.next().unwrap_or("")) {
                    if !self.tracks_dirty {
                        self.tracks.clear();
                        self.tracks_dirty = true;
                    }
                    self.tracks.push(Track { kind: k.chars().next().unwrap_or('a'), id: id.parse().unwrap_or(-1), name: name.into() });
                }
            }
            "tracks-end" => {
                self.tracks_dirty = false;
                let v: Vec<i32> = rest.split(' ').filter_map(|s| s.parse().ok()).collect();
                if v.len() == 2 {
                    self.audio_track = v[0];
                    self.subtitle_track = v[1];
                }
            }
            "tracks-changed" => {
                if self.state == PlayState::Playing {
                    self.send("tracks");
                }
            }
            "preset" => {
                if let Some((_, name)) = rest.split_once(' ') {
                    self.presets.push(name.into());
                }
            }
            "done" => self.done.push(rest.into()),
            "log" => crate::log::info(&format!("media engine: {rest}")),
            "error" => {
                crate::log::warn(&format!("media engine: {rest}"));
                self.error = Some(rest.into());
            }
            _ => {}
        }
    }

    /// Copy slot `slot` of the shared buffer into a new frame.
    fn take_frame(&mut self, slot: usize, w: usize, h: usize) {
        let (Some(shm), Some((fw, fh, pitch, slot_bytes))) = (&self.shm, self.format) else { return };
        let (w, h) = (w.min(fw), h.min(fh));
        // Crop to the visible picture: libVLC pads the picture to its alignment.
        let (vw, vh) = self.video_size.map_or((w, h), |(a, b)| (a.min(w), b.min(h)));
        if vw == 0 || vh == 0 {
            return;
        }
        let mut canvas = Canvas::new(vw, vh);
        let base = slot * slot_bytes;
        let mut mem = shm.lock();
        copy_rows(&mut mem, base, pitch, vw, vh, &mut canvas.pixels);
        drop(mem);
        self.frame = Some(Arc::new(canvas));
        self.frame_seq += 1;
        self.frames += 1;
    }

    /// Wait (pumping) until the engine reports `done <what>`, at most `timeout_ms`.
    fn wait_done(&mut self, what: &str, timeout_ms: u64) -> bool {
        let end = pit::ticks() + timeout_ms;
        loop {
            if let Some(i) = self.done.iter().position(|d| d == what) {
                self.done.remove(i);
                return true;
            }
            if !self.pump() || pit::ticks() > end {
                return false;
            }
            ipc::wait(Some(pit::ticks() + pit::HZ / 50));
        }
    }

    /// One frame of `path` at `percent` of its length (a thumbnail), and its metadata.
    /// Blocks for up to a few seconds; stops whatever was playing.
    pub fn thumbnail(&mut self, path: &str, percent: u32) -> Option<Arc<Canvas>> {
        self.reset_media();
        self.frame = None;
        self.send(&format!("thumb /{} {}", path.trim_start_matches('/'), percent.min(99)));
        self.wait_done("thumb", 8000);
        self.frame.clone()
    }

    /// Decode an image file (anything libVLC/ffmpeg reads: WebP, BMP, TIFF, ...).
    pub fn decode(&mut self, path: &str) -> Option<Arc<Canvas>> {
        self.reset_media();
        self.frame = None;
        self.send(&format!("decode /{}", path.trim_start_matches('/')));
        self.wait_done("decode", 8000);
        self.frame.clone()
    }

    /// Metadata of `path` without playing it: title, artist, album, length, ...
    pub fn probe(&mut self, path: &str) -> BTreeMap<String, String> {
        self.reset_media();
        self.send(&format!("probe /{}", path.trim_start_matches('/')));
        self.wait_done("probe", 5000);
        let mut m = self.meta.clone();
        if self.length_ms > 0 {
            m.insert("length".into(), self.length_ms.to_string());
        }
        if let Some((w, h)) = self.video_size {
            m.insert("video".into(), format!("{w}x{h}"));
        }
        if self.has_audio {
            m.insert("audio".into(), "1".into());
        }
        m
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.send("quit");
        process::kill(&self.process);
    }
}

/// Copy `h` rows of `w` BGRA pixels, `pitch` bytes apart starting at byte `base` of the
/// shared memory, into `out` as 0xRRGGBB words.
fn copy_rows(mem: &mut ipc::SharedMemory, base: usize, pitch: usize, w: usize, h: usize, out: &mut [u32]) {
    const PAGE: usize = 4096;
    for y in 0..h {
        let mut pos = base + y * pitch;
        let row = &mut out[y * w..(y + 1) * w];
        let mut x = 0;
        while x < w {
            let in_page = PAGE - pos % PAGE;
            let n = (in_page / 4).min(w - x);
            if n == 0 {
                // A pixel straddling a page boundary cannot happen: rows start 32-byte aligned.
                break;
            }
            let p = mem.page(pos / PAGE);
            // SAFETY: n pixels lie inside this 4 KiB page, which `page` returned mapped.
            let src = unsafe { core::slice::from_raw_parts(p.add(pos % PAGE) as *const u32, n) };
            for (d, s) in row[x..x + n].iter_mut().zip(src) {
                *d = *s & 0xFF_FFFF;
            }
            x += n;
            pos += n * 4;
        }
    }
}
