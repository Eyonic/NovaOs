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
const TERM_PAD: i32 = 6;

pub mod theme {
    pub const WALL_TOP: u32 = 0x1B1D33;
    pub const WALL_BOTTOM: u32 = 0x3A2459;
    pub const ACCENT: u32 = 0x8B7CF6;
    pub const FRAME: u32 = 0x262837;
    pub const TITLE_ACTIVE: u32 = 0x343750;
    pub const TITLE_INACTIVE: u32 = 0x2A2C3C;
    pub const TEXT: u32 = 0xE8E8F2;
    pub const TEXT_DIM: u32 = 0x9A9CB0;
    pub const TASKBAR: u32 = 0x12131D;
    pub const PANEL: u32 = 0x1E2030;
    pub const HOVER: u32 = 0x3A3D58;
    pub const CLOSE: u32 = 0xEF5B6B;
    pub const MINIMIZE: u32 = 0xF2C14E;
    pub const WINDOW_BG: u32 = 0x1E1E2E;
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
    fn close_button(&self) -> Rect {
        Rect::new(self.x + self.w + 2 - 32, self.y + (TITLE_H - 18) / 2, 18, 18)
    }
    fn minimize_button(&self) -> Rect {
        Rect::new(self.x + self.w + 2 - 60, self.y + (TITLE_H - 18) / 2, 18, 18)
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
    if let Content::Canvas(c) = &mut w.content {
        c.clone_from(canvas);
    }
    w.dirty = true;
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
    /// Bring an open window to the front.
    Focus(usize),
    /// Open the assistant with a first request.
    Ask(String),
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
        Action::Claude => {
            spawn_terminal(crate::claude::app_name(), '*', 0xD97757, false, |_| crate::claude::run_cli());
        }
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
        Action::Ask(prompt) => {
            let p = prompt.clone();
            spawn_terminal(crate::claude::app_name(), '*', 0xD97757, false, move |_| crate::claude::run_cli_with(Some(p)));
        }
        Action::Script(path) => {
            if let Err(e) = apps::launch(path) {
                crate::log::warn(&format!("could not start {path}: {e}"));
            }
        }
        Action::Reboot => crate::power::reboot(),
        Action::Shutdown => crate::power::shutdown(),
    }
}

/// Open a terminal window and run `f` attached to it in a new task. The window
/// closes when `f` returns.
pub fn spawn_terminal(title: &str, icon: char, accent: u32, mirror_serial: bool, f: impl FnOnce(&TtyRef) + Send + 'static) -> WindowRef {
    let (cols, rows) = (96usize, 30usize);
    let tty = crate::tty::new(cols, rows, title, mirror_serial);
    let w = cols as i32 * FONT_WIDTH as i32 + 2 * TERM_PAD;
    let h = rows as i32 * FONT_HEIGHT as i32 + 2 * TERM_PAD;
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
    /// Filesystem generation Home was built from (apps appear when written).
    launchers_gen: u64,
    status: String,
    last_status_tick: u64,
    last_compose_tick: u64,
    /// Cursor position drawn on the framebuffer (to restore the scene under it).
    drawn_cursor: Option<(i32, i32)>,
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
            launchers_gen: fs::generation(),
            status: String::new(),
            last_status_tick: 0,
            last_compose_tick: 0,
            drawn_cursor: None,
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
                let status = status_line();
                if status != self.status {
                    self.home.set_status(&status);
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
        let list: Vec<crate::home::OpenWindow> = taskbar_order(&WINDOWS.lock())
            .iter()
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
        match r.motion {
            Motion::Relative(dx, dy) => {
                self.cursor = ((self.cursor.0 + dx).clamp(0, sw - 1), (self.cursor.1 + dy).clamp(0, sh - 1));
            }
            Motion::Absolute(x, y) => {
                self.cursor = ((x as i64 * sw as i64 / 0x10000) as i32, (y as i64 * sh as i64 / 0x10000) as i32);
            }
        }
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
                                    let cols = ((nw - 2 * TERM_PAD) / FONT_WIDTH as i32).max(10) as usize;
                                    let rows = ((nh - 2 * TERM_PAD) / FONT_HEIGHT as i32).max(3) as usize;
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
        if r.wheel != 0 {
            match &over_window {
                Some(w) => w.lock().push(Event::Wheel(r.wheel)),
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
        let bg = self.home.background();
        self.scene.blit(bg, bg.bounds(), 0, 0);
        self.home.draw(&mut self.scene);

        let focus = FOCUS.load(Ordering::Relaxed);
        let list: Vec<WindowRef> = WINDOWS.lock().clone();
        for win in &list {
            let w = win.lock();
            if !w.minimized {
                draw_window(&mut self.scene, &w, w.id == focus);
            }
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

fn draw_window(scene: &mut Canvas, w: &Window, focused: bool) {
    let f = w.frame();
    scene.shadow(f, if focused { 22 } else { 12 });
    // Translucent header over whatever is behind the window, rounded on top.
    let header = if focused { 0x2C2E42 } else { 0x23242F };
    let radius = 14;
    scene.fill_round_rect(Rect::new(f.x, f.y + TITLE_H / 2, f.w, f.h - TITLE_H / 2), radius, theme::FRAME);
    for y in 0..TITLE_H {
        // Rows of the rounded top: inset like fill_round_rect does.
        let dy = radius - y;
        let inset = if dy > 0 { radius - crate::gfx::isqrt(radius * radius - dy * dy) } else { 0 };
        scene.blend_rect(Rect::new(f.x + inset, f.y + y, f.w - 2 * inset, 1), header, if focused { 236 } else { 220 });
    }
    scene.fill_rect(Rect::new(f.x, f.y + TITLE_H - 1, f.w, 1), if focused { 0x3A3D58 } else { 0x2C2E3C });
    crate::icons::draw(scene, w.icon, w.accent, Rect::new(f.x + 12, f.y + (TITLE_H - 24) / 2, 24, 24));
    let title_color = if focused { theme::TEXT } else { theme::TEXT_DIM };
    crate::font::draw_clipped(scene, f.x + 46, f.y + (TITLE_H - 20) / 2, &w.title, 15, crate::font::Weight::Bold, title_color, f.w - 130);
    let close = w.close_button();
    scene.fill_circle(close.x + 9, close.y + 9, 9, if focused { 0xE5596A } else { 0x45475A });
    let min = w.minimize_button();
    scene.fill_circle(min.x + 9, min.y + 9, 9, if focused { 0x5A5D78 } else { 0x45475A });
    // A minus on minimize, a cross on close (focused windows only).
    if focused {
        scene.fill_rect(Rect::new(min.x + 5, min.y + 8, 8, 2), 0xE8E8F2);
        for i in 0..7 {
            scene.fill_rect(Rect::new(close.x + 5 + i, close.y + 5 + i, 2, 2), 0xFFFFFF);
            scene.fill_rect(Rect::new(close.x + 11 - i, close.y + 5 + i, 2, 2), 0xFFFFFF);
        }
    }

    let (ox, oy) = w.client_origin();
    match &w.content {
        Content::Canvas(c) => {
            scene.fill_rect(Rect::new(ox, oy, w.w, w.h), theme::WINDOW_BG);
            scene.blit(c, Rect::new(0, 0, w.w.min(c.width as i32), w.h.min(c.height as i32)), ox, oy);
        }
        Content::Terminal(t) => {
            scene.fill_rect(Rect::new(ox, oy, w.w, w.h), PALETTE[0]);
            let mut t = t.lock();
            let g = &mut t.grid;
            let cursor = g.cursor_index();
            for (i, cell) in g.cells.iter().enumerate() {
                let (col, row) = ((i % g.cols) as i32, (i / g.cols) as i32);
                let x = ox + TERM_PAD + col * FONT_WIDTH as i32;
                let y = oy + TERM_PAD + row * FONT_HEIGHT as i32;
                if x + FONT_WIDTH as i32 > ox + w.w || y + FONT_HEIGHT as i32 > oy + w.h {
                    continue;
                }
                let bg = PALETTE[cell.bg as usize & 15];
                if cell.bg != 0 {
                    scene.fill_rect(Rect::new(x, y, FONT_WIDTH as i32, FONT_HEIGHT as i32), bg);
                }
                if cell.ch != b' ' {
                    scene.glyph(x, y, cell.ch, PALETTE[cell.fg as usize & 15], None, 1);
                }
                if i == cursor && focused {
                    scene.fill_rect(Rect::new(x, y + FONT_HEIGHT as i32 - 2, FONT_WIDTH as i32, 2), theme::ACCENT);
                }
            }
            g.dirty = false;
        }
    }
    // Resize grip.
    let grip = w.resize_grip();
    for i in 0..3 {
        scene.line(grip.x + 4 + i * 3, grip.y + 12, grip.x + 12, grip.y + 4 + i * 3, 0x55586E);
    }
}

/// Status for Home's header: network, CPU, memory, clock.
fn status_line() -> String {
    let net = match net::info() {
        Some(i) => match i.ip {
            Some(ip) => format!("{}", ip.address()),
            None => "connecting".to_string(),
        },
        None => "offline".to_string(),
    };
    let cpu = task::cpu_usage();
    let (used, free) = crate::memory::heap_usage();
    let tenths = |b: usize| {
        let t = (b as u64 * 10 + (1 << 29)) >> 30; // GiB in tenths, rounded
        format!("{}.{}", t / 10, t % 10)
    };
    let mem = format!("{} / {} GB", tenths(used), tenths(used + free));
    let (_, _, _, h, m, _) = rtc::now();
    format!("net {net}   cpu {cpu:>2}%   mem {mem}   {h:02}:{m:02} UTC")
}
