//! Wayland compositor for Linux programs.
//!
//! Linux GUI programs (and Xwayland, which gives X11 programs such as Steam a display)
//! connect to `$XDG_RUNTIME_DIR/wayland-0` = `/run/user/1000/wayland-0`. The compositor
//! is a kernel task that speaks the Wayland wire protocol over the Unix socket itself:
//! each `xdg_toplevel` becomes a Nova window (wm.rs) showing the client's shared-memory
//! buffer, and the window's raw key and pointer events go back as `wl_keyboard` and
//! `wl_pointer` events. Without the desktop (shell mode) it runs headless and keeps the
//! pixels, which the `wayland` shell command reports (for tests).
//!
//! Globals: wl_compositor 5, wl_subcompositor 1, wl_shm 1, wl_seat 5, wl_output 4,
//! xdg_wm_base 2, wl_data_device_manager 3. Buffers are copied when committed (and
//! released right away); frame callbacks fire at about 60 Hz.
//!
//! Protocol reference: wayland.xml and xdg-shell.xml (Wayland 1.x / wayland-protocols),
//! https://wayland.freedesktop.org/docs/html/ch04.html (wire format).

use alloc::collections::{BTreeMap, VecDeque};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::gfx::Canvas;
use crate::ipc::{self, End, ListenerRef, SharedRef};
use crate::linux::FileRef;
use crate::sync::SpinLock;
use crate::wm::{self, Event, WindowRef};
use crate::{pit, task};

pub const SOCKET_DIR: &str = "run/user/1000";
pub const SOCKET: &str = "run/user/1000/wayland-0";

/// US keyboard layout, compiled by `xkbcli compile-keymap --layout us` from
/// xkeyboard-config (MIT-style license).
static KEYMAP: &str = include_str!("wayland/us.xkb");

static STARTED: AtomicBool = AtomicBool::new(false);
static SERIAL: AtomicU32 = AtomicU32::new(1);

fn serial() -> u32 {
    SERIAL.fetch_add(1, Ordering::Relaxed)
}

fn now_ms() -> u32 {
    (pit::nanos() / 1_000_000) as u32
}

/// What the `wayland` shell command shows.
static STATUS: SpinLock<Vec<String>> = SpinLock::new(Vec::new());

/// Start the compositor (once): bind the socket and serve clients in a task.
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
        crate::log::warn("wayland: the socket address is in use");
        return;
    };
    listener.lock().listening = true;
    task::spawn("wayland", move || serve(listener));
}

/// A summary of clients and windows (the `wayland` shell command).
pub fn status() -> String {
    let s = STATUS.lock();
    if !STARTED.load(Ordering::Relaxed) {
        return "wayland: not running".into();
    }
    if s.is_empty() {
        return format!("wayland: listening on /{SOCKET}, no clients");
    }
    s.join("\n")
}

// ---------------------------------------------------------------- wire format

enum Arg<'a> {
    U(u32),
    I(i32),
    /// 24.8 fixed point.
    F(i32),
    S(&'a str),
    A(&'a [u8]),
    Fd(FileRef),
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn u32(&mut self) -> Result<u32, ()> {
        let v = self.b.get(self.pos..self.pos + 4).ok_or(())?;
        self.pos += 4;
        Ok(u32::from_le_bytes(v.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32, ()> {
        self.u32().map(|v| v as i32)
    }
    fn array(&mut self) -> Result<&'a [u8], ()> {
        let len = self.u32()? as usize;
        let v = self.b.get(self.pos..self.pos + len).ok_or(())?;
        self.pos += len.next_multiple_of(4);
        Ok(v)
    }
    fn string(&mut self) -> Result<String, ()> {
        let a = self.array()?;
        Ok(String::from_utf8_lossy(a.strip_suffix(&[0]).unwrap_or(a)).into_owned())
    }
}

// ---------------------------------------------------------------- objects

struct Buffer {
    mem: SharedRef,
    offset: usize,
    width: i32,
    height: i32,
    stride: i32,
    format: u32,
}

enum Obj {
    Display,
    Registry,
    Callback,
    Compositor,
    Subcompositor,
    Subsurface,
    Region,
    Surface,
    Shm,
    Pool { mem: SharedRef },
    Buffer(Buffer),
    Seat,
    Pointer,
    Keyboard,
    Touch,
    Output,
    WmBase,
    Positioner,
    XdgSurface { surface: u32 },
    Toplevel { surface: u32 },
    Popup { xdg: u32 },
    DataDeviceManager,
    DataSource,
    DataDevice,
    DataOffer,
}

#[derive(Default)]
struct Surface {
    /// attach() since the last commit: Some(None) detaches.
    pending: Option<Option<u32>>,
    pending_frames: Vec<u32>,
    /// Frame callbacks to complete at the next frame.
    frames: Vec<u32>,
    /// The xdg_surface and role objects, once it has one.
    xdg: Option<u32>,
    toplevel: Option<u32>,
    /// The first commit got its configure.
    configured: bool,
    title: String,
    canvas: Option<Canvas>,
    window: Option<WindowRef>,
}

struct Global {
    name: u32,
    interface: &'static str,
    version: u32,
}

const GLOBALS: &[Global] = &[
    Global { name: 1, interface: "wl_compositor", version: 5 },
    Global { name: 2, interface: "wl_subcompositor", version: 1 },
    Global { name: 3, interface: "wl_shm", version: 1 },
    Global { name: 4, interface: "wl_seat", version: 5 },
    Global { name: 5, interface: "wl_output", version: 4 },
    Global { name: 6, interface: "xdg_wm_base", version: 2 },
    Global { name: 7, interface: "wl_data_device_manager", version: 3 },
];

struct Client {
    id: usize,
    end: End,
    inbuf: Vec<u8>,
    infds: VecDeque<FileRef>,
    out: Vec<u8>,
    outfds: Vec<FileRef>,
    objects: BTreeMap<u32, Obj>,
    versions: BTreeMap<u32, u32>,
    surfaces: BTreeMap<u32, Surface>,
    pointers: Vec<u32>,
    keyboards: Vec<u32>,
    /// Surface the pointer / keyboard is in.
    pointer_focus: Option<u32>,
    keyboard_focus: Option<u32>,
    buttons: u8,
    mods: u32,
    dead: bool,
}

impl Client {
    fn new(id: usize, end: End) -> Self {
        let mut objects = BTreeMap::new();
        objects.insert(1, Obj::Display);
        Self {
            id,
            end,
            inbuf: Vec::new(),
            infds: VecDeque::new(),
            out: Vec::new(),
            outfds: Vec::new(),
            objects,
            versions: BTreeMap::new(),
            surfaces: BTreeMap::new(),
            pointers: Vec::new(),
            keyboards: Vec::new(),
            pointer_focus: None,
            keyboard_focus: None,
            buttons: 0,
            mods: 0,
            dead: false,
        }
    }

    fn event(&mut self, obj: u32, op: u16, args: &[Arg]) {
        let mut body = Vec::new();
        for a in args {
            match a {
                Arg::U(v) => body.extend_from_slice(&v.to_le_bytes()),
                Arg::I(v) | Arg::F(v) => body.extend_from_slice(&v.to_le_bytes()),
                Arg::S(s) => {
                    body.extend_from_slice(&(s.len() as u32 + 1).to_le_bytes());
                    body.extend_from_slice(s.as_bytes());
                    body.push(0);
                    body.resize(body.len().next_multiple_of(4), 0);
                }
                Arg::A(a) => {
                    body.extend_from_slice(&(a.len() as u32).to_le_bytes());
                    body.extend_from_slice(a);
                    body.resize(body.len().next_multiple_of(4), 0);
                }
                Arg::Fd(f) => self.outfds.push(f.clone()),
            }
        }
        self.out.extend_from_slice(&obj.to_le_bytes());
        self.out.extend_from_slice(&(((body.len() as u32 + 8) << 16) | op as u32).to_le_bytes());
        self.out.extend_from_slice(&body);
    }

    fn error(&mut self, obj: u32, code: u32, msg: &str) {
        self.event(1, 0, &[Arg::U(obj), Arg::U(code), Arg::S(msg)]);
        self.dead = true;
    }

    /// The object is gone (destroyed by the client, or a callback done): the id may be
    /// reused.
    fn delete(&mut self, id: u32) {
        self.objects.remove(&id);
        self.versions.remove(&id);
        self.event(1, 1, &[Arg::U(id)]);
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
        c.push(ipc::Message { data: core::mem::take(&mut self.out), fds: core::mem::take(&mut self.outfds) });
        drop(c);
        ipc::notify();
    }

    /// Read what the client sent; false once it hung up.
    fn receive(&mut self) -> bool {
        let Some(rx) = &self.end.rx else { return false };
        loop {
            let mut c = rx.lock();
            if !c.readable() {
                return !c.writer_closed;
            }
            let (data, fds) = c.read_stream(1 << 16);
            drop(c);
            self.inbuf.extend_from_slice(&data);
            self.infds.extend(fds);
        }
    }

    /// Handle every complete message in the input buffer.
    fn dispatch(&mut self) {
        let mut at = 0;
        while self.inbuf.len() - at >= 8 && !self.dead {
            let id = u32::from_le_bytes(self.inbuf[at..at + 4].try_into().unwrap());
            let word = u32::from_le_bytes(self.inbuf[at + 4..at + 8].try_into().unwrap());
            let (size, op) = ((word >> 16) as usize, (word & 0xFFFF) as u16);
            if size < 8 {
                self.error(1, 1, "bad message size");
                break;
            }
            if self.inbuf.len() - at < size {
                break;
            }
            let body = self.inbuf[at + 8..at + size].to_vec();
            at += size;
            if self.request(id, op, &body).is_err() {
                self.error(id, 1, "malformed request");
            }
        }
        self.inbuf.drain(..at);
    }

    fn fd(&mut self) -> Result<FileRef, ()> {
        self.infds.pop_front().ok_or(())
    }

    fn request(&mut self, id: u32, op: u16, body: &[u8]) -> Result<(), ()> {
        let mut r = Reader { b: body, pos: 0 };
        let Some(obj) = self.objects.get(&id) else {
            // Requests to an object that just died are ignored (it may be in flight).
            return Ok(());
        };
        match (obj, op) {
            (Obj::Display, 0) => {
                // sync(callback)
                let cb = r.u32()?;
                self.event(cb, 0, &[Arg::U(serial())]);
                self.event(1, 1, &[Arg::U(cb)]);
            }
            (Obj::Display, 1) => {
                // get_registry(registry)
                let reg = r.u32()?;
                self.objects.insert(reg, Obj::Registry);
                for g in GLOBALS {
                    self.event(reg, 0, &[Arg::U(g.name), Arg::S(g.interface), Arg::U(g.version)]);
                }
            }
            (Obj::Registry, 0) => {
                // bind(name, interface, version, id)
                let name = r.u32()?;
                let _interface = r.string()?;
                let version = r.u32()?;
                let new = r.u32()?;
                let Some(g) = GLOBALS.iter().find(|g| g.name == name) else {
                    self.error(id, 0, "no such global");
                    return Ok(());
                };
                let version = version.min(g.version);
                self.versions.insert(new, version);
                self.bind(g.interface, new, version);
            }
            (Obj::Compositor, 0) => {
                let s = r.u32()?;
                self.objects.insert(s, Obj::Surface);
                self.surfaces.insert(s, Surface::default());
            }
            (Obj::Compositor, 1) => {
                let reg = r.u32()?;
                self.objects.insert(reg, Obj::Region);
            }
            (Obj::Region, 0) | (Obj::Subcompositor, 0) | (Obj::Subsurface, 0) | (Obj::Positioner, 0) | (Obj::DataSource, 1) | (Obj::DataOffer, 4) => self.delete(id),
            (Obj::Region | Obj::Subsurface | Obj::Positioner | Obj::DataSource | Obj::DataDevice | Obj::DataOffer, _) => {}
            (Obj::Subcompositor, 1) => {
                let sub = r.u32()?;
                self.objects.insert(sub, Obj::Subsurface);
            }
            (Obj::Surface, _) => self.surface_request(id, op, &mut r)?,
            (Obj::Shm, 0) => {
                // create_pool(id, fd, size)
                let pool = r.u32()?;
                let fd = self.fd()?;
                let _size = r.i32()?;
                match crate::linux::shared_memory(&fd) {
                    Some(mem) => {
                        self.objects.insert(pool, Obj::Pool { mem });
                    }
                    None => self.error(id, 2, "the pool must be shared memory (memfd, shm, tmpfs)"),
                }
            }
            (Obj::Pool { mem }, 0) => {
                // create_buffer(id, offset, width, height, stride, format)
                let mem = mem.clone();
                let (b, offset, width, height, stride, format) = (r.u32()?, r.i32()?, r.i32()?, r.i32()?, r.i32()?, r.u32()?);
                if format > 1 || width <= 0 || height <= 0 || stride < width * 4 || offset < 0 {
                    self.error(id, 1, "unsupported buffer");
                    return Ok(());
                }
                self.objects.insert(b, Obj::Buffer(Buffer { mem, offset: offset as usize, width, height, stride, format }));
            }
            (Obj::Pool { .. }, 1) | (Obj::Buffer(_), 0) => self.delete(id),
            (Obj::Pool { .. }, _) => {} // resize: the shared memory grows by itself
            (Obj::Seat, 0) => {
                let p = r.u32()?;
                self.objects.insert(p, Obj::Pointer);
                self.pointers.push(p);
            }
            (Obj::Seat, 1) => {
                let k = r.u32()?;
                self.objects.insert(k, Obj::Keyboard);
                self.keyboards.push(k);
                let map = crate::linux::memfd_with(&[KEYMAP.as_bytes(), &[0]].concat());
                self.event(k, 0, &[Arg::U(1), Arg::Fd(map), Arg::U(KEYMAP.len() as u32 + 1)]);
                if self.versions.get(&k).copied().unwrap_or(5) >= 4 {
                    self.event(k, 5, &[Arg::I(25), Arg::I(600)]);
                }
            }
            (Obj::Seat, 2) => {
                let t = r.u32()?;
                self.objects.insert(t, Obj::Touch);
            }
            (Obj::Seat, 3) | (Obj::Output, 0) | (Obj::WmBase, 0) => self.delete(id),
            (Obj::Pointer, 0) => {} // set_cursor: Nova draws its own pointer
            (Obj::Pointer, 1) => {
                self.pointers.retain(|&p| p != id);
                self.delete(id);
            }
            (Obj::Keyboard, 0) => {
                self.keyboards.retain(|&k| k != id);
                self.delete(id);
            }
            (Obj::Touch, 0) => self.delete(id),
            (Obj::WmBase, 1) => {
                let p = r.u32()?;
                self.objects.insert(p, Obj::Positioner);
            }
            (Obj::WmBase, 2) => {
                // get_xdg_surface(id, surface)
                let (x, s) = (r.u32()?, r.u32()?);
                self.objects.insert(x, Obj::XdgSurface { surface: s });
                if let Some(surf) = self.surfaces.get_mut(&s) {
                    surf.xdg = Some(x);
                }
            }
            (Obj::WmBase, _) => {} // pong
            (Obj::XdgSurface { surface }, 0) => {
                let s = *surface;
                if let Some(surf) = self.surfaces.get_mut(&s) {
                    surf.xdg = None;
                }
                self.delete(id);
            }
            (Obj::XdgSurface { surface }, 1) => {
                // get_toplevel(id)
                let s = *surface;
                let t = r.u32()?;
                self.objects.insert(t, Obj::Toplevel { surface: s });
                if let Some(surf) = self.surfaces.get_mut(&s) {
                    surf.toplevel = Some(t);
                }
            }
            (Obj::XdgSurface { .. }, 2) => {
                // get_popup(id, parent, positioner): shown nowhere yet, but configured.
                let p = r.u32()?;
                self.objects.insert(p, Obj::Popup { xdg: id });
                self.event(p, 0, &[Arg::I(0), Arg::I(0), Arg::I(1), Arg::I(1)]);
                self.event(id, 0, &[Arg::U(serial())]);
            }
            (Obj::XdgSurface { .. }, _) => {} // set_window_geometry, ack_configure
            (Obj::Toplevel { surface }, 0) => {
                let s = *surface;
                if let Some(surf) = self.surfaces.get_mut(&s) {
                    surf.toplevel = None;
                    if let Some(w) = surf.window.take() {
                        wm::close(&w);
                    }
                }
                self.delete(id);
            }
            (Obj::Toplevel { surface }, 2) => {
                // set_title
                let s = *surface;
                let title = r.string()?;
                if let Some(surf) = self.surfaces.get_mut(&s) {
                    if let Some(w) = &surf.window {
                        wm::set_title(w, &title);
                    }
                    surf.title = title;
                }
            }
            (Obj::Toplevel { .. }, _) => {} // app id, sizes, (un)maximize, move, resize
            (Obj::Popup { .. }, 0) => self.delete(id),
            (Obj::Popup { .. }, _) => {}
            (Obj::DataDeviceManager, 0) => {
                let s = r.u32()?;
                self.objects.insert(s, Obj::DataSource);
            }
            (Obj::DataDeviceManager, 1) => {
                let d = r.u32()?;
                self.objects.insert(d, Obj::DataDevice);
            }
            (Obj::Callback | Obj::Registry | Obj::Display | Obj::Output | Obj::Shm | Obj::Seat | Obj::Buffer(_) | Obj::Compositor | Obj::Subcompositor | Obj::Keyboard | Obj::Touch | Obj::Pointer | Obj::DataDeviceManager, _) => {}
        }
        Ok(())
    }

    fn bind(&mut self, interface: &str, id: u32, version: u32) {
        match interface {
            "wl_compositor" => {
                self.objects.insert(id, Obj::Compositor);
            }
            "wl_subcompositor" => {
                self.objects.insert(id, Obj::Subcompositor);
            }
            "wl_shm" => {
                self.objects.insert(id, Obj::Shm);
                self.event(id, 0, &[Arg::U(0)]); // ARGB8888
                self.event(id, 0, &[Arg::U(1)]); // XRGB8888
            }
            "wl_seat" => {
                self.objects.insert(id, Obj::Seat);
                self.event(id, 0, &[Arg::U(3)]); // pointer | keyboard
                if version >= 2 {
                    self.event(id, 1, &[Arg::S("seat0")]);
                }
            }
            "wl_output" => {
                self.objects.insert(id, Obj::Output);
                let (w, h) = screen();
                self.event(id, 0, &[Arg::I(0), Arg::I(0), Arg::I(w * 254 / 960), Arg::I(h * 254 / 960), Arg::I(0), Arg::S("Nova"), Arg::S("Display"), Arg::I(0)]);
                self.event(id, 1, &[Arg::U(3), Arg::I(w), Arg::I(h), Arg::I(60000)]);
                if version >= 2 {
                    self.event(id, 3, &[Arg::I(1)]);
                }
                if version >= 4 {
                    self.event(id, 4, &[Arg::S("NOVA-1")]);
                    self.event(id, 5, &[Arg::S("Nova display")]);
                }
                if version >= 2 {
                    self.event(id, 2, &[]);
                }
            }
            "xdg_wm_base" => {
                self.objects.insert(id, Obj::WmBase);
            }
            "wl_data_device_manager" => {
                self.objects.insert(id, Obj::DataDeviceManager);
            }
            _ => {}
        }
    }

    fn surface_request(&mut self, id: u32, op: u16, r: &mut Reader) -> Result<(), ()> {
        match op {
            0 => {
                // destroy
                if let Some(s) = self.surfaces.remove(&id)
                    && let Some(w) = s.window
                {
                    wm::close(&w);
                }
                if self.pointer_focus == Some(id) {
                    self.pointer_focus = None;
                }
                if self.keyboard_focus == Some(id) {
                    self.keyboard_focus = None;
                }
                self.delete(id);
            }
            1 => {
                // attach(buffer, x, y)
                let b = r.u32()?;
                if let Some(s) = self.surfaces.get_mut(&id) {
                    s.pending = Some((b != 0).then_some(b));
                }
            }
            3 => {
                // frame(callback)
                let cb = r.u32()?;
                self.objects.insert(cb, Obj::Callback);
                if let Some(s) = self.surfaces.get_mut(&id) {
                    s.pending_frames.push(cb);
                }
            }
            6 => self.commit(id),
            _ => {} // damage, regions, transform, scale, offset
        }
        Ok(())
    }

    fn commit(&mut self, id: u32) {
        let Some(s) = self.surfaces.get_mut(&id) else { return };
        let frames = core::mem::take(&mut s.pending_frames);
        s.frames.extend(frames);
        // The first commit of an xdg toplevel gets its configure; then buffers follow.
        if let (Some(xdg), Some(top)) = (s.xdg, s.toplevel)
            && !s.configured
        {
            s.configured = true;
            self.event(top, 0, &[Arg::I(0), Arg::I(0), Arg::A(&4u32.to_le_bytes())]); // activated
            self.event(xdg, 0, &[Arg::U(serial())]);
        }
        let Some(s) = self.surfaces.get_mut(&id) else { return };
        let Some(attached) = s.pending.take() else { return };
        let Some(bid) = attached else {
            s.canvas = None;
            return;
        };
        let Some(Obj::Buffer(b)) = self.objects.get(&bid) else { return };
        let (w, h) = (b.width as usize, b.height as usize);
        let mut canvas = Canvas::new(w, h);
        {
            let mut mem = b.mem.lock();
            // XRGB8888 and ARGB8888 (alpha is not blended: windows are opaque).
            for y in 0..h {
                let row = mem.read(b.offset + y * b.stride as usize, w * 4);
                for (x, px) in row.chunks_exact(4).enumerate() {
                    let v = u32::from_le_bytes(px.try_into().unwrap());
                    canvas.pixels[y * w + x] = v & 0xFF_FFFF;
                }
            }
        }
        // Copied: the client may reuse the buffer at once.
        self.event(bid, 0, &[]);
        let s = self.surfaces.get_mut(&id).unwrap();
        if s.toplevel.is_some() && wm::running() {
            let win = s.window.get_or_insert_with(|| {
                let title = if s.title.is_empty() { "Linux program".to_string() } else { s.title.clone() };
                let win = wm::open(&title, '\u{25EB}', wm::theme::ACCENT, w as i32, h as i32, wm::Content::Canvas(Canvas::new(w, h)));
                wm::set_raw_input(&win);
                win
            });
            if wm::size(win) != (w as i32, h as i32) {
                wm::set_size(win, w as i32, h as i32);
            }
            wm::present(win, &canvas);
        }
        s.canvas = Some(canvas);
    }

    /// Complete the frame callbacks (about 60 times a second).
    fn frame_done(&mut self) {
        let t = now_ms();
        let cbs: Vec<u32> = self.surfaces.values_mut().flat_map(|s| core::mem::take(&mut s.frames)).collect();
        for cb in cbs {
            self.event(cb, 0, &[Arg::U(t)]);
            self.delete(cb);
        }
    }

    /// Input from the Nova windows of this client's toplevels.
    fn window_events(&mut self) {
        let wins: Vec<(u32, WindowRef, Option<u32>, Option<u32>)> = self.surfaces.iter().filter_map(|(&id, s)| s.window.clone().map(|w| (id, w, s.xdg, s.toplevel))).collect();
        for (sid, win, xdg, top) in wins {
            while let Some(e) = wm::wait_event(&win, Some(0)) {
                match e {
                    Event::Close => {
                        if let Some(t) = top {
                            self.event(t, 1, &[]);
                        }
                    }
                    Event::Resized => {
                        let (w, h) = wm::size(&win);
                        if let (Some(t), Some(x)) = (top, xdg) {
                            self.event(t, 0, &[Arg::I(w), Arg::I(h), Arg::A(&4u32.to_le_bytes())]);
                            self.event(x, 0, &[Arg::U(serial())]);
                        }
                    }
                    Event::RawKey { code, pressed } => self.key(sid, code, pressed),
                    Event::Pointer { x, y, buttons } => self.pointer(sid, x, y, buttons),
                    Event::PointerLeave => self.pointer_leave(),
                    Event::Wheel(n) => {
                        for p in self.pointers.clone() {
                            self.event(p, 4, &[Arg::U(now_ms()), Arg::U(0), Arg::F(n * 10 * 256)]);
                            self.event(p, 5, &[]);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn key(&mut self, surface: u32, code: u16, pressed: bool) {
        if self.keyboard_focus != Some(surface) {
            if let Some(old) = self.keyboard_focus {
                for k in self.keyboards.clone() {
                    self.event(k, 2, &[Arg::U(serial()), Arg::U(old)]);
                }
            }
            self.keyboard_focus = Some(surface);
            for k in self.keyboards.clone() {
                self.event(k, 1, &[Arg::U(serial()), Arg::U(surface), Arg::A(&[])]);
                self.event(k, 4, &[Arg::U(serial()), Arg::U(self.mods & !2), Arg::U(0), Arg::U(self.mods & 2), Arg::U(0)]);
            }
        }
        // XKB modifier bits of the us keymap: Shift 0, Lock 1, Control 2, Mod1 (Alt) 3,
        // Mod4 (Super) 6.
        let bit = match code {
            42 | 54 => 1,
            29 | 97 => 4,
            56 | 100 => 8,
            125 | 126 => 64,
            _ => 0,
        };
        let before = self.mods;
        if bit != 0 {
            if pressed { self.mods |= bit } else { self.mods &= !bit }
        } else if code == 58 && pressed {
            self.mods ^= 2; // Caps Lock toggles
        }
        for k in self.keyboards.clone() {
            self.event(k, 3, &[Arg::U(serial()), Arg::U(now_ms()), Arg::U(code as u32), Arg::U(pressed as u32)]);
            if self.mods != before {
                self.event(k, 4, &[Arg::U(serial()), Arg::U(self.mods & !2), Arg::U(0), Arg::U(self.mods & 2), Arg::U(0)]);
            }
        }
    }

    fn pointer(&mut self, surface: u32, x: i32, y: i32, buttons: u8) {
        let entering = self.pointer_focus != Some(surface);
        if entering {
            self.pointer_leave();
            self.pointer_focus = Some(surface);
        }
        for p in self.pointers.clone() {
            if entering {
                self.event(p, 0, &[Arg::U(serial()), Arg::U(surface), Arg::F(x * 256), Arg::F(y * 256)]);
            } else {
                self.event(p, 2, &[Arg::U(now_ms()), Arg::F(x * 256), Arg::F(y * 256)]);
            }
            for (i, btn) in [(0, 0x110u32), (1, 0x111), (2, 0x112)] {
                let (was, is) = (self.buttons >> i & 1, buttons >> i & 1);
                if was != is {
                    self.event(p, 3, &[Arg::U(serial()), Arg::U(now_ms()), Arg::U(btn), Arg::U(is as u32)]);
                }
            }
            self.event(p, 5, &[]);
        }
        self.buttons = buttons;
    }

    fn pointer_leave(&mut self) {
        if let Some(old) = self.pointer_focus.take() {
            for p in self.pointers.clone() {
                self.event(p, 1, &[Arg::U(serial()), Arg::U(old)]);
                self.event(p, 5, &[]);
            }
        }
    }

    fn close_windows(&mut self) {
        for s in self.surfaces.values_mut() {
            if let Some(w) = s.window.take() {
                wm::close(&w);
            }
        }
    }

    fn describe(&self) -> Vec<String> {
        let mut v = alloc::vec![format!("client {}: {} objects, {} surfaces", self.id, self.objects.len(), self.surfaces.len())];
        for (id, s) in &self.surfaces {
            if let Some(c) = &s.canvas {
                let crc = crate::storage::crc32(&c.pixels.iter().flat_map(|p| p.to_le_bytes()).collect::<Vec<u8>>());
                let center = c.pixels[(c.height / 2) * c.width + c.width / 2];
                v.push(format!("  surface {id}: {}x{} {} \"{}\" center #{center:06x} crc {crc:08x}", c.width, c.height, if s.toplevel.is_some() { "toplevel" } else { "surface" }, s.title));
            }
        }
        v
    }
}

fn screen() -> (i32, i32) {
    if wm::running() { wm::work_area() } else { (1280, 800) }
}

fn serve(listener: ListenerRef) {
    let mut clients: Vec<Client> = Vec::new();
    let mut next_id = 1;
    let mut next_frame = pit::ticks();
    let mut next_status = 0;
    loop {
        while let Some(end) = listener.lock().queue.pop_front() {
            clients.push(Client::new(next_id, end));
            next_id += 1;
        }
        let frame = pit::ticks() >= next_frame;
        if frame {
            next_frame = pit::ticks() + pit::HZ / 60;
        }
        for c in clients.iter_mut() {
            if !c.receive() {
                c.dead = true;
            }
            c.dispatch();
            c.window_events();
            if frame {
                c.frame_done();
            }
            c.flush();
        }
        for c in clients.iter_mut().filter(|c| c.dead) {
            c.close_windows();
        }
        clients.retain(|c| !c.dead);
        if pit::ticks() >= next_status {
            next_status = pit::ticks() + pit::HZ / 2;
            *STATUS.lock() = clients.iter().flat_map(|c| c.describe()).collect();
        }
        ipc::wait(Some(next_frame));
    }
}
