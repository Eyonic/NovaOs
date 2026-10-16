//! The desktop: window manager, compositor, taskbar and launcher.
//!
//! One task ("desktop") owns the screen. It reads the keyboard and mouse, routes
//! input to the focused window, and recomposes the scene when something changed:
//! Home (the launcher behind the windows, see home.rs) and the windows (bottom to
//! top). There is no taskbar: open windows are in Home's Continue row, and minimizing
//! a window shows Home. The
//! scene is copied to the framebuffer and the mouse pointer is drawn on top.
//!
//! Applications run in their own tasks. A window's content is either a canvas the
//! app draws into and hands over with [`present`], or a terminal (see tty.rs),
//! which the compositor renders from its text grid. Apps receive input as
//! [`Event`]s through [`wait_event`].

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::console::Screen;
use crate::gfx::{Canvas, Rect};
use crate::input::{self, Key};
use crate::mouse::{self, Motion};
use crate::sync::SpinLock;
use crate::textgrid::{FONT_HEIGHT, FONT_WIDTH, PALETTE};
use crate::tty::TtyRef;
use crate::{apps, fs, net, pit, rtc, task};

pub const TITLE_H: i32 = 40;
/// No taskbar: Home and full-screen windows use the whole screen.
pub const TASKBAR_H: i32 = 0;
/// Corner radius of window frames (the client area follows it at the bottom).
pub const FRAME_RADIUS: i32 = 12;
const TERM_PAD: i32 = 14;
/// Terminal text size (px) in the monospace font.
const TERM_FONT: u16 = 15;

pub mod theme {
    pub const ACCENT: u32 = 0x8B7CF6;
    pub const FRAME: u32 = 0x262837;
    pub const TEXT: u32 = 0xE8E8F2;
    pub const TEXT_DIM: u32 = 0x9A9CB0;
    pub const HOVER: u32 = 0x3A3D58;
    pub const CLOSE: u32 = 0xE5596A;
    pub const MINIMIZE: u32 = 0xF2C14E;
    pub const WINDOW_BG: u32 = 0x1E1E2E;
}

/// What Home's header shows about the machine (refreshed once a second).
#[derive(Clone, PartialEq, Default)]
pub struct Status {
    /// "10.0.2.15", "connecting" or "offline".
    pub net: String,
    pub online: bool,
    pub cpu: u32,
    /// "0.1 / 3.6 GB".
    pub mem: String,
    /// "14:37".
    pub clock: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    Key(Key),
    MouseDown { x: i32, y: i32, button: u8 },
    MouseUp { x: i32, y: i32 },
    MouseMove { x: i32, y: i32 },
    Wheel(i32),
    /// The client area changed size; redraw at `size()`.
    Resized,
    /// The user closed the window: clean up and return.
    Close,
    /// Raw input (windows opened with [`set_raw_input`], e.g. Wayland clients): a key
    /// pressed or released, as a Linux key code (KEY_*).
    RawKey { code: u16, pressed: bool },
    /// Raw input: the pointer over the client area, with the buttons held (bit 0 left,
    /// bit 1 right, bit 2 middle). Sent for every pointer report.
    Pointer { x: i32, y: i32, buttons: u8 },
    /// Raw input: the pointer left the client area.
    PointerLeave,
}

pub enum Content {
    Canvas(Canvas),
    Terminal(TtyRef),
}

pub struct Window {
    pub id: usize,
    pub title: String,
    pub icon: char,
    pub accent: u32,
    /// Top-left corner of the frame.
    pub x: i32,
    pub y: i32,
    /// Client area size.
    pub w: i32,
    pub h: i32,
    pub content: Content,
    events: VecDeque<Event>,
    pub dirty: bool,
    pub closed: bool,
    pub minimized: bool,
    /// Wants [`Event::RawKey`] and [`Event::Pointer`] (see [`set_raw_input`]).
    pub raw_input: bool,
    /// The close button asked the program to close (Wayland clients decide themselves).
    pub close_requested: bool,
}

pub type WindowRef = Arc<SpinLock<Window>>;

impl Window {
    fn frame(&self) -> Rect {
        Rect::new(self.x, self.y, self.w + 2, self.h + TITLE_H + 1)
    }
    fn client_origin(&self) -> (i32, i32) {
        (self.x + 1, self.y + TITLE_H)
    }
    fn title_bar(&self) -> Rect {
        Rect::new(self.x, self.y, self.w + 2, TITLE_H)
    }
    /// Caption buttons: 20 px discs on an 8 px rhythm from the right edge.
    fn close_button(&self) -> Rect {
        Rect::new(self.x + self.w + 2 - 34, self.y + (TITLE_H - 20) / 2, 20, 20)
    }
    fn minimize_button(&self) -> Rect {
        Rect::new(self.x + self.w + 2 - 62, self.y + (TITLE_H - 20) / 2, 20, 20)
    }
    fn resize_grip(&self) -> Rect {
        let f = self.frame();
        Rect::new(f.x + f.w - 14, f.y + f.h - 14, 14, 14)
    }
    fn push(&mut self, e: Event) {
        if self.events.len() < 256 {
            self.events.push_back(e);
        }
    }
}

static WINDOWS: SpinLock<Vec<WindowRef>> = SpinLock::new(Vec::new());
static FOCUS: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
static REDRAW: AtomicBool = AtomicBool::new(true);
static RUNNING: AtomicBool = AtomicBool::new(false);
static SCREEN_SIZE: SpinLock<(i32, i32)> = SpinLock::new((1280, 800));

pub fn running() -> bool {
    RUNNING.load(Ordering::Relaxed)
}

pub fn request_redraw() {
    REDRAW.store(true, Ordering::Relaxed);
}

/// Usable desktop area (screen minus taskbar).
pub fn work_area() -> (i32, i32) {
    let (w, h) = *SCREEN_SIZE.lock();
    (w, h - TASKBAR_H)
}

/// Open a window with a client area of `w` x `h`. It gets focus.
pub fn open(title: &str, icon: char, accent: u32, w: i32, h: i32, content: Content) -> WindowRef {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let (sw, sh) = work_area();
    let (w, h) = (w.min(sw - 4), h.min(sh - TITLE_H - 4));
    let n = WINDOWS.lock().len() as i32;
    let x = ((sw - w) / 2 + (n % 6) * 28 - 70).clamp(0, (sw - w - 2).max(0));
    let y = ((sh - h - TITLE_H) / 2 + (n % 6) * 24 - 50).clamp(0, (sh - h - TITLE_H).max(0));
    let win = Arc::new(SpinLock::new(Window {
        id,
        title: title.into(),
        icon,
        accent,
        x,
        y,
        w,
        h,
        content,
        events: VecDeque::new(),
        dirty: true,
        closed: false,
        minimized: false,
        raw_input: false,
        close_requested: false,
    }));
    WINDOWS.lock().push(win.clone());
    FOCUS.store(id, Ordering::Relaxed);
    request_redraw();
    win
}

/// Remove a window (its app has finished).
/// Close a window as its close button does: tell the program, hang up a terminal.
fn close_window(win: &WindowRef) {
    {
        let mut w = win.lock();
        // A Linux program (Wayland client) may ask about unsaved work first: the window
        // stays until it destroys it. A second click closes it anyway (a hung program).
        if w.raw_input && !w.close_requested {
            w.close_requested = true;
            w.push(Event::Close);
            return;
        }
        w.closed = true;
        w.push(Event::Close);
        if let Content::Terminal(t) = &w.content {
            t.lock().hung_up = true;
        }
    }
    close(win);
}

pub fn close(win: &WindowRef) {
    let id = win.lock().id;
    WINDOWS.lock().retain(|w| w.lock().id != id);
    if FOCUS.load(Ordering::Relaxed) == id {
        let top = WINDOWS.lock().iter().rev().find(|w| !w.lock().minimized).map(|w| w.lock().id).unwrap_or(0);
        FOCUS.store(top, Ordering::Relaxed);
    }
    request_redraw();
}

/// Next event for `win`, waiting up to `timeout_ticks` (None = forever).
pub fn wait_event(win: &WindowRef, timeout_ticks: Option<u64>) -> Option<Event> {
    let end = timeout_ticks.map(|t| pit::ticks() + t);
    loop {
        {
            let mut w = win.lock();
            if let Some(e) = w.events.pop_front() {
                return Some(e);
            }
            if w.closed {
                return Some(Event::Close);
            }
        }
        if end.is_some_and(|e| pit::ticks() >= e) {
            return None;
        }
        task::wait_event();
    }
}

/// Client size of `win`.
pub fn size(win: &WindowRef) -> (i32, i32) {
    let w = win.lock();
    (w.w, w.h)
}

/// Show `canvas` as the window's content.
pub fn present(win: &WindowRef, canvas: &Canvas) {
    let mut w = win.lock();
    // Still drawing, so still responding: a later close click asks it again.
    w.close_requested = false;
    if let Content::Canvas(c) = &mut w.content {
        c.clone_from(canvas);
    }
    w.dirty = true;
}

/// Deliver raw key and pointer events to this window (besides the usual ones).
pub fn set_raw_input(win: &WindowRef) {
    win.lock().raw_input = true;
}

/// Resize a window's client area (a Wayland client chose its size).
pub fn set_size(win: &WindowRef, w: i32, h: i32) {
    let (sw, sh) = work_area();
    let mut win = win.lock();
    win.w = w.clamp(1, sw - 2);
    win.h = h.clamp(1, sh - TITLE_H - 1);
    win.dirty = true;
    request_redraw();
}

pub fn set_title(win: &WindowRef, title: &str) {
    win.lock().title = title.into();
    request_redraw();
}

// ---------------------------------------------------------------------------
// Actions (what Home's cards do)

#[derive(Clone)]
pub enum Action {
    Terminal,
    Claude,
    Browser,
    Script(String),
    /// Open a web page in Nova Browser.
    Url(String),
    /// Start a Linux program from its desktop entry (by ID, see desktop.rs).
    Desktop(String),
    /// Bring an open window to the front.
    Focus(usize),
    /// Close an open window (stop the program running in it).
    Close(usize),
    Reboot,
    Shutdown,
}

pub fn launch(action: &Action) {
    match action {
        Action::Terminal => {
            spawn_terminal("Terminal", '>', 0x3B4252, false, |_| crate::shell::run());
        }
        Action::Claude => toggle_assistant(),
        Action::Browser => crate::browser::launch(None),
        Action::Url(url) => crate::browser::launch(Some(url.clone())),
        Action::Focus(id) => {
            if let Some(w) = window_by_id(*id) {
                w.lock().minimized = false;
                let mut list = WINDOWS.lock();
                if let Some(i) = list.iter().position(|w| w.lock().id == *id) {
                    let w = list.remove(i);
                    list.push(w);
                }
                FOCUS.store(*id, Ordering::Relaxed);
            }
            request_redraw();
        }
        Action::Close(id) => {
            if let Some(win) = window_by_id(*id) {
                close_window(&win);
            }
            request_redraw();
        }
        Action::Script(path) => {
            if let Err(e) = apps::launch(path) {
                crate::log::warn(&format!("could not start {path}: {e}"));
            }
        }
        Action::Desktop(id) => {
            if let Some(e) = crate::desktop::entries().iter().find(|e| e.id == *id)
                && let Err(err) = crate::desktop::launch(e, None)
            {
                crate::log::warn(&format!("could not start {}: {err}", e.name));
            }
        }
        Action::Reboot => crate::power::reboot(),
        Action::Shutdown => crate::power::shutdown(),
    }
}

// ---------------------------------------------------------------------------
// The assistant: a chat panel docked bottom-right, behind a bubble that is always on
// screen (like a chat widget on a website), so it never takes space until wanted.

/// Window id of the assistant's chat panel (0 = not running).
static ASSISTANT: AtomicUsize = AtomicUsize::new(0);
const BUBBLE_R: i32 = 30;

fn bubble_center() -> (i32, i32) {
    let (w, h) = *SCREEN_SIZE.lock();
    (w - 28 - BUBBLE_R, h - 28 - BUBBLE_R)
}

/// Start the assistant in its chat panel (`visible` false: behind the bubble).
pub fn spawn_assistant(mirror_serial: bool, visible: bool, prompt: Option<String>) {
    let win = spawn_terminal_sized(crate::claude::app_name(), '*', 0xD97757, mirror_serial, 64, 25, move |_| crate::claude::run_cli_with(prompt));
    let (sw, sh) = *SCREEN_SIZE.lock();
    let id = {
        let mut w = win.lock();
        w.x = (sw - w.w - 2 - 24).max(0);
        w.y = (sh - w.h - TITLE_H - 28 - 2 * BUBBLE_R - 16).max(0);
        w.minimized = !visible;
        w.id
    };
    ASSISTANT.store(id, Ordering::Relaxed);
    if visible {
        launch(&Action::Focus(id));
    } else if FOCUS.load(Ordering::Relaxed) == id {
        FOCUS.store(0, Ordering::Relaxed);
    }
    request_redraw();
}

/// The bubble: show the assistant's panel, or tuck it away again.
pub fn toggle_assistant() {
    let id = ASSISTANT.load(Ordering::Relaxed);
    let Some(win) = (id != 0).then(|| window_by_id(id)).flatten() else {
        spawn_assistant(false, true, None);
        return;
    };
    let showing = !win.lock().minimized && FOCUS.load(Ordering::Relaxed) == id;
    if showing {
        win.lock().minimized = true;
        FOCUS.store(0, Ordering::Relaxed);
        request_redraw();
    } else {
        launch(&Action::Focus(id));
    }
}

/// Open a terminal window and run `f` attached to it in a new task. The window
/// closes when `f` returns.
/// Size of one terminal character cell: the monospace font's, else the bitmap font's.
fn term_cell() -> (i32, i32) {
    crate::font::mono_cell(TERM_FONT).unwrap_or((FONT_WIDTH as i32, FONT_HEIGHT as i32))
}

pub fn spawn_terminal(title: &str, icon: char, accent: u32, mirror_serial: bool, f: impl FnOnce(&TtyRef) + Send + 'static) -> WindowRef {
    spawn_terminal_sized(title, icon, accent, mirror_serial, 96, 30, f)
}

fn spawn_terminal_sized(title: &str, icon: char, accent: u32, mirror_serial: bool, cols: usize, rows: usize, f: impl FnOnce(&TtyRef) + Send + 'static) -> WindowRef {
    let tty = crate::tty::new(cols, rows, title, mirror_serial);
    let (cw, ch) = term_cell();
    let w = cols as i32 * cw + 2 * TERM_PAD;
    let h = rows as i32 * ch + 2 * TERM_PAD;
    let win = open(title, icon, accent, w, h, Content::Terminal(tty.clone()));
    let win2 = win.clone();
    let tty2 = tty.clone();
    task::spawn_with_tty(title, Some(tty), move || {
        f(&tty2);
        close(&win2);
    });
    win
}

// ---------------------------------------------------------------------------
// The desktop task

struct Desktop {
    screen: Screen,
    scene: Canvas,
    home: crate::home::Home,
    /// Open windows as Home last saw them (id, title).
    home_windows: Vec<(usize, String)>,
    cursor: (i32, i32),
    buttons: (bool, bool),
    drag: Option<Drag>,
    /// The window the pointer hovers (gets MouseMove events so it can show states).
    hover_win: usize,
    /// Filesystem generation Home was built from (apps appear when written).
    launchers_gen: u64,
    status: Status,
    last_status_tick: u64,
    last_compose_tick: u64,
    /// Cursor position drawn on the framebuffer (to restore the scene under it).
    drawn_cursor: Option<(i32, i32)>,
    /// The raw-input window the pointer is over.
    raw_pointer: Option<usize>,
}

#[derive(Clone, Copy)]
enum Drag {
    Move { id: usize, dx: i32, dy: i32 },
    Resize { id: usize },
    /// Mouse button went down in a window's client area: moves go to it.
    Client { id: usize },
}

const CURSOR: [&str; 19] = [
    "X           ",
    "XX          ",
    "X.X         ",
    "X..X        ",
    "X...X       ",
    "X....X      ",
    "X.....X     ",
    "X......X    ",
    "X.......X   ",
    "X........X  ",
    "X.........X ",
    "X......XXXXX",
    "X...X..X    ",
    "X..XX..X    ",
    "X.X  X..X   ",
    "XX   X..X   ",
    "X     X..X  ",
    "      X..X  ",
    "       XX   ",
];

/// Start the desktop on the framebuffer. Returns false if there is no usable screen.
pub fn start() -> bool {
    let Some(screen) = crate::console::screen() else { return false };
    *SCREEN_SIZE.lock() = (screen.width as i32, screen.height as i32);
    RUNNING.store(true, Ordering::Relaxed);
    crate::wayland::start();
    crate::pulse::start();
    crate::console::suspend();
    task::spawn("desktop", move || {
        let mut home = crate::home::Home::new(screen.width as i32, screen.height as i32);
        home.refresh(&[]);
        let mut d = Desktop {
            screen,
            scene: Canvas::new(screen.width, screen.height),
            home,
            home_windows: Vec::new(),
            cursor: (screen.width as i32 / 2, screen.height as i32 / 2),
            buttons: (false, false),
            drag: None,
            hover_win: 0,
            launchers_gen: fs::generation(),
            status: Status::default(),
            last_status_tick: 0,
            last_compose_tick: 0,
            drawn_cursor: None,
            raw_pointer: None,
        };
        d.run();
    });
    true
}

impl Desktop {
    fn run(&mut self) -> ! {
        loop {
            let mut changed = REDRAW.swap(false, Ordering::Relaxed);
            while let Some(key) = input::poll_keyboard() {
                self.key(key);
                changed = true;
            }
            for (code, pressed) in input::take_raw_keys() {
                if let Some(w) = self.focused() {
                    let mut w = w.lock();
                    if w.raw_input {
                        w.push(Event::RawKey { code, pressed });
                    }
                }
            }
            let mut moved = false;
            while let Some(report) = mouse::poll() {
                changed |= self.mouse(report);
                moved = true;
            }
            // Window contents (terminals, app canvases) that changed.
            for w in WINDOWS.lock().iter() {
                let mut w = w.lock();
                let term_dirty = match &w.content {
                    Content::Terminal(t) => t.try_lock().is_some_and(|t| t.grid.dirty),
                    _ => false,
                };
                if w.dirty || term_dirty {
                    w.dirty = false;
                    changed = true;
                }
            }
            // Home: the Continue row follows the open windows; hover previews.
            let wins: Vec<(usize, String)> = WINDOWS.lock().iter().map(|w| { let w = w.lock(); (w.id, w.title.clone()) }).collect();
            if wins != self.home_windows {
                self.home_windows = wins;
                self.refresh_home();
                changed = true;
            }
            changed |= self.home.tick();
            let now = pit::ticks();
            if now - self.last_status_tick >= pit::HZ {
                self.last_status_tick = now;
                let status = status_now();
                if status != self.status {
                    self.home.set_status(status.clone());
                    self.status = status;
                    changed = true;
                }
                if fs::generation() != self.launchers_gen {
                    self.launchers_gen = fs::generation();
                    self.refresh_home();
                    changed = true;
                }
            }
            // Cap the frame rate at HZ/2 (50 fps).
            if changed && now - self.last_compose_tick >= 2 {
                self.last_compose_tick = now;
                self.compose();
                self.present_all();
            } else if changed {
                REDRAW.store(true, Ordering::Relaxed); // retry next tick
            } else if moved {
                self.draw_cursor();
            }
            task::wait_event();
        }
    }

    // ----- input -----

    fn refresh_home(&mut self) {
        let assistant = ASSISTANT.load(Ordering::Relaxed);
        let list: Vec<crate::home::OpenWindow> = taskbar_order(&WINDOWS.lock())
            .iter()
            .filter(|w| w.lock().id != assistant)
            .map(|w| {
                let w = w.lock();
                crate::home::OpenWindow { id: w.id, title: w.title.clone(), icon: w.icon, accent: w.accent }
            })
            .collect();
        self.home.refresh(&list);
    }

    fn focused(&self) -> Option<WindowRef> {
        let id = FOCUS.load(Ordering::Relaxed);
        WINDOWS.lock().iter().find(|w| w.lock().id == id).cloned()
    }

    fn key(&mut self, key: Key) {
        // The Windows key always brings back Home: every window steps aside (minimized;
        // they stay in Home's Continue row).
        if key == Key::F1 {
            toggle_assistant();
            return;
        }
        if key == Key::Super {
            for w in WINDOWS.lock().iter() {
                w.lock().minimized = true;
            }
            FOCUS.store(0, Ordering::Relaxed);
            return;
        }
        let Some(win) = self.focused() else {
            // No window has the focus: the keys browse Home.
            if let Some(action) = self.home.key(key) {
                launch(&action);
            }
            return;
        };
        let mut w = win.lock();
        match &w.content {
            Content::Terminal(t) => t.lock().push_key(key),
            Content::Canvas(_) => w.push(Event::Key(key)),
        }
    }

    fn raise(&self, id: usize) {
        let mut list = WINDOWS.lock();
        if let Some(i) = list.iter().position(|w| w.lock().id == id) {
            let w = list.remove(i);
            w.lock().minimized = false;
            list.push(w);
        }
        FOCUS.store(id, Ordering::Relaxed);
    }

    /// Handle one pointer report; returns true if the scene must be redrawn.
    fn mouse(&mut self, r: mouse::Report) -> bool {
        let (sw, sh) = (self.screen.width as i32, self.screen.height as i32);
        let before = self.cursor;
        match r.motion {
            Motion::Relative(dx, dy) => {
                self.cursor = ((self.cursor.0 + dx).clamp(0, sw - 1), (self.cursor.1 + dy).clamp(0, sh - 1));
            }
            Motion::Absolute(x, y) => {
                self.cursor = ((x as i64 * sw as i64 / 0x10000) as i32, (y as i64 * sh as i64 / 0x10000) as i32);
            }
        }
        let moved = self.cursor != before;
        let (x, y) = self.cursor;
        let mut changed = false;
        let pressed = r.left && !self.buttons.0;
        let released = !r.left && self.buttons.0;
        let right_pressed = r.right && !self.buttons.1;
        self.buttons = (r.left, r.right);

        if pressed || right_pressed {
            changed |= self.press(x, y, if pressed { 1 } else { 2 });
        } else if released {
            if let Some(Drag::Client { id }) = self.drag
                && let Some(w) = window_by_id(id)
            {
                let mut w = w.lock();
                let (ox, oy) = w.client_origin();
                w.push(Event::MouseUp { x: x - ox, y: y - oy });
            }
            self.drag = None;
        } else if let Some(drag) = self.drag {
            match drag {
                Drag::Move { id, dx, dy } => {
                    if let Some(w) = window_by_id(id) {
                        let mut w = w.lock();
                        w.x = (x - dx).clamp(-w.w + 60, sw - 60);
                        w.y = (y - dy).clamp(0, sh - TASKBAR_H - TITLE_H);
                        changed = true;
                    }
                }
                Drag::Resize { id } => {
                    if let Some(w) = window_by_id(id) {
                        let mut w = w.lock();
                        let nw = (x - w.x - 2).max(220);
                        let nh = (y - w.y - TITLE_H - 1).max(120);
                        if (nw, nh) != (w.w, w.h) {
                            w.w = nw;
                            w.h = nh;
                            match &w.content {
                                Content::Terminal(t) => {
                                    let (cw, ch) = term_cell();
                                    let cols = ((nw - 2 * TERM_PAD) / cw).max(10) as usize;
                                    let rows = ((nh - 2 * TERM_PAD) / ch).max(3) as usize;
                                    t.lock().grid.resize(cols, rows);
                                }
                                Content::Canvas(_) => w.push(Event::Resized),
                            }
                            changed = true;
                        }
                    }
                }
                Drag::Client { id } => {
                    if let Some(w) = window_by_id(id) {
                        let mut w = w.lock();
                        let (ox, oy) = w.client_origin();
                        w.push(Event::MouseMove { x: x - ox, y: y - oy });
                    }
                }
            }
        }
        let over_window = self.window_at(x, y);
        // Raw-input windows see every pointer report while the pointer is over their
        // client area (or they hold a drag), and a leave event when it goes.
        let raw_target = match self.drag {
            Some(Drag::Client { id }) => window_by_id(id),
            _ => over_window.clone(),
        }
        .filter(|w| {
            let w = w.lock();
            let (ox, oy) = w.client_origin();
            w.raw_input && (matches!(self.drag, Some(Drag::Client { .. })) || Rect::new(ox, oy, w.w, w.h).contains(x, y))
        });
        let raw_id = raw_target.as_ref().map(|w| w.lock().id);
        if self.raw_pointer != raw_id {
            if let Some(old) = self.raw_pointer.and_then(window_by_id) {
                old.lock().push(Event::PointerLeave);
            }
            self.raw_pointer = raw_id;
        }
        if let Some(w) = raw_target {
            let mut w = w.lock();
            let (ox, oy) = w.client_origin();
            w.push(Event::Pointer { x: x - ox, y: y - oy, buttons: r.left as u8 | (r.right as u8) << 1 | (r.middle as u8) << 2 });
        }
        if r.wheel != 0 {
            match &over_window {
                Some(w) => {
                    let mut w = w.lock();
                    match &w.content {
                        Content::Terminal(t) => {
                            t.lock().grid.scroll_view(r.wheel * -3);
                            changed = true;
                        }
                        Content::Canvas(_) => w.push(Event::Wheel(r.wheel)),
                    }
                }
                None if y < sh - TASKBAR_H => {
                    self.home.wheel(r.wheel);
                    changed = true;
                }
                None => {}
            }
        }
        if self.drag.is_none() {
            if over_window.is_none() && y < sh - TASKBAR_H {
                changed |= self.home.hover(x, y);
            } else {
                changed |= self.home.hover(-1, -1);
            }
            // Hover: the window under the pointer hears where it is (apps show hover
            // states); the one it just left hears that it is gone.
            let over_id = over_window.as_ref().map_or(0, |w| w.lock().id);
            if over_id != self.hover_win
                && let Some(old) = window_by_id(self.hover_win)
            {
                let mut old = old.lock();
                if matches!(old.content, Content::Canvas(_)) {
                    old.push(Event::MouseMove { x: -1, y: -1 });
                }
            }
            self.hover_win = over_id;
            if moved && let Some(w) = &over_window {
                let mut w = w.lock();
                let (ox, oy) = w.client_origin();
                let (cx, cy) = (x - ox, y - oy);
                if matches!(w.content, Content::Canvas(_)) && cy >= 0 {
                    w.push(Event::MouseMove { x: cx, y: cy });
                } else if cy < 0 {
                    changed = true; // the caption buttons highlight
                }
            }
        }
        changed
    }

    fn window_at(&self, x: i32, y: i32) -> Option<WindowRef> {
        WINDOWS.lock().iter().rev().find(|w| {
            let w = w.lock();
            !w.minimized && w.frame().contains(x, y)
        }).cloned()
    }

    fn press(&mut self, x: i32, y: i32, button: u8) -> bool {
        let sw = self.screen.width as i32;
        let (bx, by) = bubble_center();
        if (x - bx) * (x - bx) + (y - by) * (y - by) <= (BUBBLE_R + 4) * (BUBBLE_R + 4) {
            toggle_assistant();
            return true;
        }
        // Windows, top to bottom.
        if let Some(win) = self.window_at(x, y) {
            let id = win.lock().id;
            self.raise(id);
            let mut w = win.lock();
            if w.close_button().contains(x, y) {
                drop(w);
                close_window(&win);
            } else if w.minimize_button().contains(x, y) {
                w.minimized = true;
                FOCUS.store(0, Ordering::Relaxed);
            } else if w.resize_grip().contains(x, y) {
                self.drag = Some(Drag::Resize { id });
            } else if w.title_bar().contains(x, y) {
                self.drag = Some(Drag::Move { id, dx: x - w.x, dy: y - w.y });
            } else {
                let (ox, oy) = w.client_origin();
                w.push(Event::MouseDown { x: x - ox, y: y - oy, button });
                self.drag = Some(Drag::Client { id });
            }
            return true;
        }
        // Home: left click opens, right click shows a card's menu.
        FOCUS.store(0, Ordering::Relaxed);
        let action = if button == 1 { self.home.click(x, y) } else { self.home.right_click(x, y) };
        if let Some(action) = action {
            launch(&action);
        }
        let _ = sw;
        true
    }

    // ----- drawing -----

    fn compose(&mut self) {
        self.home.draw(&mut self.scene); // includes the full-screen backdrop

        let focus = FOCUS.load(Ordering::Relaxed);
        let list: Vec<WindowRef> = WINDOWS.lock().clone();
        let hover = (self.drag.is_none()).then_some(self.cursor);
        for win in &list {
            let w = win.lock();
            if !w.minimized {
                draw_window(&mut self.scene, &w, w.id == focus, if w.id == self.hover_win { hover } else { None });
            }
        }
        self.draw_bubble();
    }

    /// The assistant bubble (bottom-right, above everything).
    fn draw_bubble(&mut self) {
        let (cx, cy) = bubble_center();
        let id = ASSISTANT.load(Ordering::Relaxed);
        let win = (id != 0).then(|| window_by_id(id)).flatten();
        let open = win.as_ref().is_some_and(|w| !w.lock().minimized) && FOCUS.load(Ordering::Relaxed) == id;
        // New output while tucked away.
        let unread = win.as_ref().is_some_and(|w| {
            let w = w.lock();
            w.minimized && matches!(&w.content, Content::Terminal(t) if t.try_lock().is_some_and(|t| t.grid.dirty))
        });
        let hovered = self.drag.is_none() && {
            let (x, y) = self.cursor;
            (x - cx) * (x - cx) + (y - cy) * (y - cy) <= (BUBBLE_R + 4) * (BUBBLE_R + 4)
        };
        // The app icon tile is the button itself (no plate behind it): its shadow, hover
        // tint and open outline follow the tile's own rounded corners.
        let tile = Rect::new(cx - BUBBLE_R, cy - BUBBLE_R, 2 * BUBBLE_R, 2 * BUBBLE_R);
        let radius = tile.w * 23 / 100; // icons::draw's corner
        self.scene.shadow_round(tile, radius, 14, 110);
        if open {
            let out = Rect::new(tile.x - 4, tile.y - 4, tile.w + 8, tile.h + 8);
            self.scene.stroke_round_rect_w(out, radius + 4, 2, theme::ACCENT);
        }
        crate::icons::draw(&mut self.scene, '*', 0xD97757, tile);
        if hovered {
            self.scene.blend_round_rect(tile, radius, 0xFFFFFF, 28);
        }
        if open {
            // A small down chevron: click to tuck the chat away.
            let (bx, by) = (cx + BUBBLE_R - 4, cy - BUBBLE_R + 4);
            self.scene.fill_circle(bx, by, 10, theme::ACCENT);
            crate::icons::glyph(&mut self.scene, "chevron-down", 0xFFFFFF, Rect::new(bx - 6, by - 6, 12, 12));
        } else if unread {
            let (bx, by) = (cx + BUBBLE_R - 6, cy - BUBBLE_R + 6);
            self.scene.fill_circle(bx, by, 9, 0x16171F);
            self.scene.fill_circle(bx, by, 7, 0xE5394B);
        }
    }

    fn put_row(&self, y: i32, x0: i32, x1: i32, src: &[u32]) {
        let s = &self.screen;
        let dst = (s.addr + y as usize * s.pitch + x0 as usize * 4) as *mut u32;
        let n = (x1 - x0) as usize;
        // SAFETY: the row segment lies within the framebuffer Limine mapped.
        unsafe {
            if s.is_xrgb() {
                core::ptr::copy_nonoverlapping(src.as_ptr(), dst, n);
            } else {
                for (i, &p) in src.iter().take(n).enumerate() {
                    dst.add(i).write_volatile(s.pixel(p));
                }
            }
        }
    }

    fn present_all(&mut self) {
        let w = self.screen.width;
        for y in 0..self.screen.height {
            self.put_row(y as i32, 0, w as i32, &self.scene.pixels[y * w..(y + 1) * w]);
        }
        self.drawn_cursor = None;
        self.draw_cursor();
    }

    /// Restore the scene under the old pointer and draw the pointer at its new place.
    fn draw_cursor(&mut self) {
        let (sw, sh) = (self.screen.width as i32, self.screen.height as i32);
        if let Some((ox, oy)) = self.drawn_cursor {
            for row in 0..CURSOR.len() as i32 {
                let y = oy + row;
                if y >= 0 && y < sh {
                    let x0 = ox.clamp(0, sw);
                    let x1 = (ox + 12).clamp(0, sw);
                    if x1 > x0 {
                        let start = y as usize * self.screen.width + x0 as usize;
                        self.put_row(y, x0, x1, &self.scene.pixels[start..start + (x1 - x0) as usize]);
                    }
                }
            }
        }
        let (cx, cy) = self.cursor;
        for (row, line) in CURSOR.iter().enumerate() {
            let y = cy + row as i32;
            if y >= sh {
                break;
            }
            for (col, ch) in line.bytes().enumerate() {
                let x = cx + col as i32;
                if x >= sw {
                    break;
                }
                let color = match ch {
                    b'X' => 0x101010,
                    b'.' => 0xFFFFFF,
                    _ => continue,
                };
                let s = &self.screen;
                // SAFETY: (x, y) is inside the framebuffer.
                unsafe { ((s.addr + y as usize * s.pitch + x as usize * 4) as *mut u32).write_volatile(s.pixel(color)) };
            }
        }
        self.drawn_cursor = Some((cx, cy));
    }
}

/// Taskbar buttons stay in the order windows were opened, whatever the stacking.
fn taskbar_order(list: &[WindowRef]) -> Vec<WindowRef> {
    let mut v = list.to_vec();
    v.sort_by_key(|w| w.lock().id);
    v
}

fn window_by_id(id: usize) -> Option<WindowRef> {
    WINDOWS.lock().iter().find(|w| w.lock().id == id).cloned()
}

/// An open window, as listed by [`windows`].
pub struct WindowInfo {
    pub id: usize,
    pub title: String,
    pub icon: char,
    pub accent: u32,
}

/// Open windows in opening order (System Monitor's app list).
pub fn windows() -> Vec<WindowInfo> {
    taskbar_order(&WINDOWS.lock())
        .iter()
        .map(|w| {
            let w = w.lock();
            WindowInfo { id: w.id, title: w.title.clone(), icon: w.icon, accent: w.accent }
        })
        .collect()
}

/// Box-drawing characters drawn as lines that fill the whole cell, so frames join up
/// across rows (font glyphs are shorter than the line height). True if `c` was one.
fn draw_box_char(scene: &mut Canvas, c: char, r: Rect, color: u32) -> bool {
    // (left, right, up, down) arms; double-line characters are drawn as single lines.
    let (l, rt, u, d) = match c {
        '\u{2500}' | '\u{2550}' => (true, true, false, false),
        '\u{2502}' | '\u{2551}' => (false, false, true, true),
        '\u{250c}' | '\u{2554}' | '\u{256d}' | '\u{2552}' | '\u{2553}' => (false, true, false, true),
        '\u{2510}' | '\u{2557}' | '\u{256e}' | '\u{2555}' | '\u{2556}' => (true, false, false, true),
        '\u{2514}' | '\u{255a}' | '\u{2570}' | '\u{2558}' | '\u{2559}' => (false, true, true, false),
        '\u{2518}' | '\u{255d}' | '\u{256f}' | '\u{255b}' | '\u{255c}' => (true, false, true, false),
        '\u{251c}' | '\u{2560}' | '\u{255e}' | '\u{255f}' => (false, true, true, true),
        '\u{2524}' | '\u{2563}' | '\u{2561}' | '\u{2562}' => (true, false, true, true),
        '\u{252c}' | '\u{2566}' | '\u{2564}' | '\u{2565}' => (true, true, false, true),
        '\u{2534}' | '\u{2569}' | '\u{2567}' | '\u{2568}' => (true, true, true, false),
        '\u{253c}' | '\u{256c}' | '\u{256a}' | '\u{256b}' => (true, true, true, true),
        _ => return false,
    };
    let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
    if l {
        scene.fill_rect(Rect::new(r.x, cy, cx - r.x + 1, 1), color);
    }
    if rt {
        scene.fill_rect(Rect::new(cx, cy, r.x + r.w - cx, 1), color);
    }
    if u {
        scene.fill_rect(Rect::new(cx, r.y, 1, cy - r.y + 1), color);
    }
    if d {
        scene.fill_rect(Rect::new(cx, cy, 1, r.y + r.h - cy), color);
    }
    true
}

/// Draw a small live picture of window `id` into `r` (for Home). False if it is gone.
pub fn draw_window_thumb(scene: &mut Canvas, id: usize, r: Rect, radius: i32) -> bool {
    let Some(win) = window_by_id(id) else { return false };
    let Some(w) = win.try_lock() else { return false };
    match &w.content {
        Content::Canvas(c) => scene.blit_scaled(c, r, radius, 255),
        Content::Terminal(t) => {
            scene.fill_round_rect(r, radius, PALETTE[0]);
            let Some(t) = t.try_lock() else { return true };
            let g = &t.grid;
            let (cols, rows) = (g.cols.max(1) as i32, g.rows.max(1) as i32);
            for (i, cell) in g.cells.iter().enumerate() {
                if cell.ch == b' ' {
                    continue;
                }
                let (col, row) = (i as i32 % cols, i as i32 / cols);
                let x0 = r.x + 6 + col * (r.w - 12) / cols;
                let x1 = r.x + 6 + (col + 1) * (r.w - 12) / cols;
                let y0 = r.y + 6 + row * (r.h - 12) / rows;
                let y1 = r.y + 6 + (row + 1) * (r.h - 12) / rows;
                scene.fill_rect(Rect::new(x0, y0 + 1, (x1 - x0).max(1), (y1 - y0 - 2).max(1)), PALETTE[cell.fg as usize & 15]);
            }
        }
    }
    true
}

/// Draw a window: a rounded frame with a translucent header, the caption buttons
/// (highlighted under `hover`), and the client area with rounded bottom corners. The
/// focused window has a deeper shadow and full-contrast chrome; others sit back.
fn draw_window(scene: &mut Canvas, w: &Window, focused: bool, hover: Option<(i32, i32)>) {
    use crate::gfx::{blend, BOTTOM_CORNERS, TOP_CORNERS};
    let f = w.frame();
    let radius = FRAME_RADIUS;
    // Shadow: same extent whether focused or not (nothing jumps), lighter when inactive.
    scene.shadow_round(f, radius, 28, if focused { 120 } else { 55 });
    scene.fill_round_rect(f, radius, theme::FRAME);
    // Translucent header over whatever is behind the window, rounded on top, with a
    // 1 px highlight along the top edge so the window separates from the backdrop.
    let header = if focused { 0x2C2E42 } else { 0x23242F };
    scene.fill_round_rect_corners(Rect::new(f.x, f.y, f.w, TITLE_H), radius, header, TOP_CORNERS, if focused { 236 } else { 220 });
    scene.blend_rect(Rect::new(f.x + radius, f.y, f.w - 2 * radius, 1), 0xFFFFFF, if focused { 26 } else { 14 });
    scene.fill_rect(Rect::new(f.x, f.y + TITLE_H - 1, f.w, 1), if focused { 0x3A3D58 } else { 0x2C2E3C });
    crate::icons::draw(scene, w.icon, w.accent, Rect::new(f.x + 12, f.y + (TITLE_H - 22) / 2, 22, 22));
    let title_color = if focused { theme::TEXT } else { theme::TEXT_DIM };
    crate::font::draw_clipped(scene, f.x + 44, f.y + (TITLE_H - crate::font::line_height(14)) / 2, &w.title, 14, crate::font::Weight::Bold, title_color, f.w - 118);
    // Caption buttons: a red close and a neutral minimize; both lift on hover, the
    // glyphs show on the focused window and on hover.
    let close = w.close_button();
    let min = w.minimize_button();
    let over = |r: Rect| hover.is_some_and(|(x, y)| r.contains(x, y));
    let (close_fill, min_fill) = if focused { (theme::CLOSE, 0x5A5D78) } else { (0x45475A, 0x45475A) };
    for (r, fill, glyph) in [(close, close_fill, "close"), (min, min_fill, "minus")] {
        let fill = if over(r) { blend(fill, 0xFFFFFF, 40) } else { fill };
        scene.fill_circle(r.x + r.w / 2, r.y + r.h / 2, r.w / 2, fill);
        if focused || over(r) {
            crate::icons::glyph(scene, glyph, 0xFFFFFF, Rect::new(r.x + 5, r.y + 5, r.w - 10, r.h - 10));
        }
    }

    let (ox, oy) = w.client_origin();
    let client = Rect::new(ox, oy, w.w, w.h);
    let inner = radius - 1;
    match &w.content {
        Content::Canvas(c) => {
            scene.fill_round_rect_corners(client, inner, theme::WINDOW_BG, BOTTOM_CORNERS, 255);
            scene.blit_corners(c, Rect::new(0, 0, w.w.min(c.width as i32), w.h.min(c.height as i32)), ox, oy, inner, BOTTOM_CORNERS);
        }
        Content::Terminal(t) => {
            scene.fill_round_rect_corners(client, inner, PALETTE[0], BOTTOM_CORNERS, 255);
            let mut t = t.lock();
            let g = &mut t.grid;
            let (cw, ch) = term_cell();
            let mono = crate::font::mono_cell(TERM_FONT).is_some();
            let (cur_col, cur_row) = (g.col.min(g.cols - 1), g.row);
            let back = g.view_offset.min(g.history.len());
            for row in 0..g.rows {
                let y = oy + TERM_PAD + row as i32 * ch;
                if y + ch > oy + w.h {
                    break;
                }
                let line = g.view_row(row);
                // Warp-style command blocks: a prompt line ("nova> " / the assistant's "> ")
                // starts a block: a hairline above it and the prompt in the accent colour.
                let prompt_len = if line.len() >= 6 && line[..6].iter().map(|c| c.ch).eq(*b"nova> ") {
                    5
                } else if line.len() >= 2 && line[0].ch == b'>' && line[1].ch == b' ' {
                    1
                } else {
                    0
                };
                if prompt_len > 0 && row > 0 {
                    scene.fill_rect(Rect::new(ox + TERM_PAD - 4, y - 3, w.w - 2 * TERM_PAD + 8, 1), 0x2A2C3E);
                }
                for (col, cell) in line.iter().enumerate() {
                    let x = ox + TERM_PAD + col as i32 * cw;
                    if x + cw > ox + w.w - TERM_PAD + cw {
                        break;
                    }
                    if cell.bg != 0 {
                        scene.fill_rect(Rect::new(x, y, cw, ch), PALETTE[cell.bg as usize & 15]);
                    }
                    let is_cursor = focused && back == 0 && row == cur_row && col == cur_col;
                    if is_cursor {
                        scene.fill_round_rect(Rect::new(x, y + 1, cw, ch - 2), 2, theme::ACCENT);
                    }
                    if cell.ch == b' ' {
                        continue;
                    }
                    let mut fg = PALETTE[cell.fg as usize & 15];
                    if col < prompt_len {
                        fg = theme::ACCENT;
                    }
                    if is_cursor {
                        fg = PALETTE[0];
                    }
                    let c = crate::textgrid::char_of(cell.ch);
                    if mono && draw_box_char(scene, c, Rect::new(x, y, cw, ch), fg) {
                        continue;
                    }
                    if mono {
                        let mut buf = [0u8; 4];
                        let text = c.encode_utf8(&mut buf);
                        let weight = if col < prompt_len || cell.fg >= 8 { crate::font::Weight::MonoBold } else { crate::font::Weight::Mono };
                        crate::font::draw(scene, x, y + 1, text, TERM_FONT, weight, fg);
                    } else {
                        scene.glyph(x, y, cell.ch, fg, None, 1);
                    }
                }
            }
            // Scrollback position indicator.
            if back > 0 {
                let label = alloc::format!("{back} lines up  \u{00b7}  scroll down for live output");
                let lw = crate::font::width(&label, 13, crate::font::Weight::Bold) + 24;
                let pill = Rect::new(ox + w.w - lw - 16, oy + 10, lw, 26);
                scene.fill_round_rect(pill, 13, theme::ACCENT);
                crate::font::draw(scene, pill.x + 12, pill.y + 5, &label, 13, crate::font::Weight::Bold, 0xFFFFFF);
            }
            g.dirty = false;
        }
    }
    // Resize grip: two short diagonal lines tucked inside the rounded corner.
    let grip = w.resize_grip();
    for i in 0..2 {
        scene.line(grip.x + 6 + i * 4, grip.y + 11, grip.x + 11, grip.y + 6 + i * 4, if focused { 0x6A6E8A } else { 0x4A4D62 });
    }
}

/// Status for Home's header: network, CPU, memory, clock.
fn status_now() -> Status {
    let (net, online) = match net::info() {
        Some(i) => match i.ip {
            Some(ip) => (format!("{}", ip.address()), true),
            None => ("connecting".to_string(), false),
        },
        None => ("offline".to_string(), false),
    };
    let cpu = task::cpu_usage() as u32;
    let (used, free) = crate::memory::heap_usage();
    let tenths = |b: usize| {
        let t = (b as u64 * 10 + (1 << 29)) >> 30; // GiB in tenths, rounded
        format!("{}.{}", t / 10, t % 10)
    };
    let mem = format!("{} / {} GB", tenths(used), tenths(used + free));
    let (_, _, _, h, m, _) = rtc::now();
    Status { net, online, cpu, mem, clock: format!("{h:02}:{m:02}") }
}
