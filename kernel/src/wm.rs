//! The desktop: window manager, compositor, taskbar and launcher.
//!
//! One task ("desktop") owns the screen. It reads the keyboard and mouse, routes
//! input to the focused window, and recomposes the scene when something changed:
//! wallpaper, desktop icons, windows (bottom to top), taskbar, start menu. The
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

pub const TITLE_H: i32 = 30;
pub const TASKBAR_H: i32 = 44;
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
        Rect::new(self.x + self.w + 2 - 26, self.y + 7, 16, 16)
    }
    fn minimize_button(&self) -> Rect {
        Rect::new(self.x + self.w + 2 - 48, self.y + 7, 16, 16)
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

fn request_redraw() {
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
// Launchers (desktop icons and start menu)

#[derive(Clone)]
pub enum Action {
    Terminal,
    Claude,
    Browser,
    Script(String),
    Reboot,
    Shutdown,
}

#[derive(Clone)]
pub struct Launcher {
    pub name: String,
    pub icon: char,
    pub color: u32,
    pub action: Action,
}

fn launchers() -> Vec<Launcher> {
    let mut v = alloc::vec![
        Launcher { name: crate::claude::app_name().into(), icon: '*', color: 0xD97757, action: Action::Claude },
        Launcher { name: "Terminal".into(), icon: '>', color: 0x3B4252, action: Action::Terminal },
    ];
    if crate::browser::engine_installed() {
        v.insert(1, Launcher { name: "Browser".into(), icon: '@', color: 0xFB542B, action: Action::Browser });
    }
    for app in apps::installed() {
        v.push(Launcher { name: app.title, icon: app.icon, color: app.color, action: Action::Script(app.path) });
    }
    v
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
    wallpaper: Canvas,
    cursor: (i32, i32),
    buttons: (bool, bool),
    drag: Option<Drag>,
    menu_open: bool,
    launchers: Vec<Launcher>,
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
        let mut wallpaper = Canvas::new(screen.width, screen.height);
        draw_wallpaper(&mut wallpaper);
        let mut d = Desktop {
            screen,
            scene: Canvas::new(screen.width, screen.height),
            wallpaper,
            cursor: (screen.width as i32 / 2, screen.height as i32 / 2),
            buttons: (false, false),
            drag: None,
            menu_open: false,
            launchers: launchers(),
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

fn draw_wallpaper(c: &mut Canvas) {
    let r = c.bounds();
    c.gradient(r, theme::WALL_TOP, theme::WALL_BOTTOM);
    // A soft glow and the Nova wordmark in the lower right.
    let (cx, cy) = (r.w * 3 / 4, r.h * 2 / 5);
    for i in (0..12).rev() {
        let rad = 60 + i * 22;
        let alpha = 6;
        for dy in -rad..=rad {
            let half = crate::gfx::isqrt(rad * rad - dy * dy);
            c.blend_rect(Rect::new(cx - half, cy + dy, 2 * half, 1), theme::ACCENT, alpha);
        }
    }
    let label = "nova";
    let scale = 6;
    let tw = label.len() as i32 * FONT_WIDTH as i32 * scale;
    c.text_scaled(cx - tw / 2 + 3, cy - 48 + 3, label, 0x14152A, scale);
    c.text_scaled(cx - tw / 2, cy - 48, label, 0xF3F0FF, scale);
    let sub = "an operating system built with Claude";
    c.text(cx - (sub.len() as i32 * 8) / 2, cy + 56, sub, 0xB8B0E8);
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
            let now = pit::ticks();
            if now - self.last_status_tick >= pit::HZ {
                self.last_status_tick = now;
                let status = status_line();
                if status != self.status {
                    self.status = status;
                    changed = true;
                }
                if fs::generation() != self.launchers_gen {
                    self.launchers_gen = fs::generation();
                    self.launchers = launchers();
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

    fn focused(&self) -> Option<WindowRef> {
        let id = FOCUS.load(Ordering::Relaxed);
        WINDOWS.lock().iter().find(|w| w.lock().id == id).cloned()
    }

    fn key(&mut self, key: Key) {
        if key == Key::Char(0x1B) && self.menu_open {
            self.menu_open = false;
            return;
        }
        let Some(win) = self.focused() else { return };
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
        if r.wheel != 0
            && let Some(w) = self.window_at(x, y)
        {
            w.lock().push(Event::Wheel(r.wheel));
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
        let (sw, sh) = (self.screen.width as i32, self.screen.height as i32);
        // Start menu.
        if self.menu_open {
            self.menu_open = false;
            let items = self.menu_items();
            let menu = menu_rect(items.len(), sh);
            if menu.contains(x, y) {
                let i = ((y - menu.y - 40) / 32) as usize;
                if let Some(item) = items.get(i) {
                    launch(&item.action);
                }
                return true;
            }
            if Rect::new(0, sh - TASKBAR_H, 120, TASKBAR_H).contains(x, y) {
                return true; // the start button toggles the menu closed
            }
        }
        // Taskbar.
        if y >= sh - TASKBAR_H {
            if x < 120 {
                self.menu_open = !self.menu_open;
                return true;
            }
            let list: Vec<WindowRef> = WINDOWS.lock().clone();
            for (i, w) in taskbar_order(&list).iter().enumerate() {
                if taskbar_button(i).contains(x, y) {
                    let id = w.lock().id;
                    let focused = FOCUS.load(Ordering::Relaxed) == id && !w.lock().minimized;
                    if focused {
                        w.lock().minimized = true;
                        FOCUS.store(0, Ordering::Relaxed);
                    } else {
                        self.raise(id);
                    }
                    return true;
                }
            }
            return false;
        }
        // Windows, top to bottom.
        if let Some(win) = self.window_at(x, y) {
            let id = win.lock().id;
            self.raise(id);
            let mut w = win.lock();
            if w.close_button().contains(x, y) {
                w.closed = true;
                w.push(Event::Close);
                if let Content::Terminal(t) = &w.content {
                    t.lock().hung_up = true;
                }
                drop(w);
                close(&win);
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
        // Desktop icons.
        FOCUS.store(0, Ordering::Relaxed);
        for (i, l) in self.launchers.iter().enumerate() {
            if icon_rect(i, sh).contains(x, y) {
                let action = l.action.clone();
                launch(&action);
                return true;
            }
        }
        let _ = sw;
        true
    }

    fn menu_items(&self) -> Vec<Launcher> {
        let mut items = self.launchers.clone();
        items.push(Launcher { name: "Restart".into(), icon: '\u{21bb}', color: 0x4C5470, action: Action::Reboot });
        items.push(Launcher { name: "Shut down".into(), icon: 'o', color: 0x4C5470, action: Action::Shutdown });
        items
    }

    // ----- drawing -----

    fn compose(&mut self) {
        let sh = self.screen.height as i32;
        let sw = self.screen.width as i32;
        let wall = self.wallpaper.bounds();
        self.scene.blit(&self.wallpaper, wall, 0, 0);

        for (i, l) in self.launchers.iter().enumerate() {
            let r = icon_rect(i, sh);
            let tile = Rect::new(r.x + (r.w - 52) / 2, r.y + 6, 52, 52);
            self.scene.shadow(tile, 4);
            self.scene.fill_round_rect(tile, 12, l.color);
            let g = crate::textgrid::glyph(l.icon);
            self.scene.glyph(tile.x + 14, tile.y + 2, g, 0xFFFFFF, None, 3);
            let label_w = l.name.chars().count() as i32 * 8;
            let lx = r.x + (r.w - label_w.min(r.w)) / 2;
            self.scene.text_clipped(lx + 1, r.y + 65, &l.name, 0x101018, r.w);
            self.scene.text_clipped(lx, r.y + 64, &l.name, theme::TEXT, r.w);
        }

        let focus = FOCUS.load(Ordering::Relaxed);
        let list: Vec<WindowRef> = WINDOWS.lock().clone();
        for win in &list {
            let w = win.lock();
            if !w.minimized {
                draw_window(&mut self.scene, &w, w.id == focus);
            }
        }

        // Taskbar.
        let bar = Rect::new(0, sh - TASKBAR_H, sw, TASKBAR_H);
        self.scene.blend_rect(bar, theme::TASKBAR, 235);
        self.scene.fill_rect(Rect::new(0, sh - TASKBAR_H, sw, 1), 0x2E3048);
        let start = Rect::new(8, sh - TASKBAR_H + 7, 104, TASKBAR_H - 14);
        self.scene.fill_round_rect(start, 8, if self.menu_open { theme::ACCENT } else { 0x2B2D42 });
        self.scene.glyph(start.x + 10, start.y + 7, crate::textgrid::glyph('\u{263c}'), 0xFFFFFF, None, 1);
        self.scene.text(start.x + 28, start.y + 7, "Nova", 0xFFFFFF);
        for (i, win) in taskbar_order(&list).iter().enumerate() {
            let w = win.lock();
            let r = taskbar_button(i);
            if r.x + r.w > sw - 380 {
                break;
            }
            let active = w.id == focus && !w.minimized;
            self.scene.fill_round_rect(r, 6, if active { theme::HOVER } else { 0x1F2133 });
            self.scene.fill_round_rect(Rect::new(r.x + 6, r.y + 6, 18, 18), 5, w.accent);
            self.scene.glyph(r.x + 11, r.y + 7, crate::textgrid::glyph(w.icon), 0xFFFFFF, None, 1);
            self.scene.text_clipped(r.x + 30, r.y + 7, &w.title, if active { 0xFFFFFF } else { theme::TEXT_DIM }, r.w - 36);
            if active {
                self.scene.fill_rect(Rect::new(r.x + 8, r.y + r.h - 2, r.w - 16, 2), theme::ACCENT);
            }
        }
        let tw = self.status.chars().count() as i32 * 8;
        self.scene.text(sw - tw - 14, sh - TASKBAR_H + 14, &self.status, theme::TEXT);

        if self.menu_open {
            let items = self.menu_items();
            let menu = menu_rect(items.len(), sh);
            self.scene.shadow(menu, 10);
            self.scene.fill_round_rect(menu, 10, theme::PANEL);
            self.scene.text(menu.x + 16, menu.y + 14, "Applications", theme::TEXT_DIM);
            for (i, item) in items.iter().enumerate() {
                let r = Rect::new(menu.x + 8, menu.y + 40 + i as i32 * 32, menu.w - 16, 30);
                if r.contains(self.cursor.0, self.cursor.1) {
                    self.scene.fill_round_rect(r, 6, theme::HOVER);
                }
                self.scene.fill_round_rect(Rect::new(r.x + 6, r.y + 5, 20, 20), 5, item.color);
                self.scene.glyph(r.x + 12, r.y + 7, crate::textgrid::glyph(item.icon), 0xFFFFFF, None, 1);
                self.scene.text(r.x + 36, r.y + 7, &item.name, theme::TEXT);
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

fn icon_rect(i: usize, screen_h: i32) -> Rect {
    let per_col = ((screen_h - TASKBAR_H - 20) / 96).max(1) as usize;
    Rect::new(16 + (i / per_col) as i32 * 96, 16 + (i % per_col) as i32 * 96, 88, 90)
}

fn taskbar_button(i: usize) -> Rect {
    let (_, h) = *SCREEN_SIZE.lock();
    Rect::new(124 + i as i32 * 178, h - TASKBAR_H + 7, 170, 30)
}

fn menu_rect(items: usize, screen_h: i32) -> Rect {
    let h = 48 + items as i32 * 32;
    Rect::new(8, screen_h - TASKBAR_H - h - 8, 260, h)
}

fn draw_window(scene: &mut Canvas, w: &Window, focused: bool) {
    let f = w.frame();
    scene.shadow(f, if focused { 14 } else { 8 });
    scene.fill_round_rect(f, 8, theme::FRAME);
    let title = Rect::new(f.x, f.y, f.w, TITLE_H);
    scene.fill_round_rect(title, 8, if focused { theme::TITLE_ACTIVE } else { theme::TITLE_INACTIVE });
    scene.fill_rect(Rect::new(f.x, f.y + TITLE_H - 8, f.w, 8), if focused { theme::TITLE_ACTIVE } else { theme::TITLE_INACTIVE });
    scene.fill_round_rect(Rect::new(f.x + 8, f.y + 6, 18, 18), 5, w.accent);
    scene.glyph(f.x + 13, f.y + 7, crate::textgrid::glyph(w.icon), 0xFFFFFF, None, 1);
    scene.text_clipped(f.x + 34, f.y + 7, &w.title, if focused { theme::TEXT } else { theme::TEXT_DIM }, f.w - 100);
    let close = w.close_button();
    scene.fill_circle(close.x + 8, close.y + 8, 7, if focused { theme::CLOSE } else { 0x55586E });
    let min = w.minimize_button();
    scene.fill_circle(min.x + 8, min.y + 8, 7, if focused { theme::MINIMIZE } else { 0x55586E });

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

/// Taskbar status: network, CPU, memory, clock.
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
    let mem = used * 100 / (used + free).max(1);
    let (_, _, _, h, m, _) = rtc::now();
    format!("net {net}   cpu {cpu:>2}%   mem {mem:>2}%   {h:02}:{m:02} UTC")
}
