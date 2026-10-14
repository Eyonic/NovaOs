//! Nova Home: the desktop's full-screen launcher.
//!
//! A hero banner at the top shows the selected item large (artwork, title, description,
//! an Open button and a live preview); below it, rows of poster cards by category:
//! open windows (live thumbnails), the assistant and app ideas, apps, favourite web
//! sites, and system actions. Hovering a card enlarges it and, after a moment, plays a
//! live preview (apps are run headlessly and re-rendered every second). Mouse, wheel and
//! arrow keys + Enter all work.
//!
//! Artwork: `usr/share/nova/home/<name>.png` system files (generated images, see
//! assets/home/README.md); items without artwork get a procedural poster.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::gfx::{blend, Canvas, Rect};
use crate::input::Key;
use crate::sync::SpinLock;
use crate::wm::{self, Action};
use crate::{apps, fs, pit, rtc, task};

const CARD_W: i32 = 224;
const CARD_H: i32 = 126;
const ZOOM_W: i32 = 264;
const ZOOM_H: i32 = 148;
const GAP: i32 = 18;
const MARGIN: i32 = 44;
const TOP_BAR: i32 = 52;
const HERO_H: i32 = 300;
const ROW_H: i32 = 186;
const ROW_TITLE: i32 = 28;
/// Hover time before a card's preview starts (ticks).
const PREVIEW_DELAY: u64 = pit::HZ * 6 / 10;

const BG: u32 = 0x0B0C12;
const TEXT: u32 = 0xF2F2F7;
const DIM: u32 = 0xA2A4B8;
const ACCENT: u32 = 0x8B7CF6;

#[derive(Clone, PartialEq)]
enum Kind {
    /// An open window (live thumbnail).
    Window(usize),
    /// A Rhai app (live preview).
    App(String),
    Other,
}

#[derive(Clone)]
struct Item {
    title: String,
    category: &'static str,
    about: String,
    meta: String,
    icon: char,
    color: u32,
    art: Option<&'static str>,
    kind: Kind,
    action: Action,
}

impl Item {
    fn key(&self) -> String {
        format!("{}|{}|{}", self.category, self.title, self.art.unwrap_or(""))
    }
}

struct Row {
    title: &'static str,
    items: Vec<Item>,
    /// First visible card.
    scroll: usize,
}

/// A window the Continue row shows.
pub struct OpenWindow {
    pub id: usize,
    pub title: String,
    pub icon: char,
    pub accent: u32,
}

pub struct Home {
    width: i32,
    height: i32,
    rows: Vec<Row>,
    sel: (usize, usize),
    top_row: usize,
    /// The selection follows the mouse (zoom + preview) rather than the keyboard.
    hovering: bool,
    sel_since: u64,
    preview_on: bool,
    posters: Vec<(String, bool, Canvas)>,
    hero: Option<(String, Canvas)>,
    background: Canvas,
    /// Network, CPU, memory and clock, shown in the header.
    status: String,
    cursor: (i32, i32),
    /// Open context menu: its card, position and entries.
    menu: Option<Menu>,
}

struct Menu {
    at: (i32, i32),
    title: String,
    entries: Vec<(&'static str, Action)>,
}

const MENU_W: i32 = 200;
const MENU_ROW: i32 = 30;

impl Menu {
    fn rect(&self) -> Rect {
        Rect::new(self.at.0, self.at.1, MENU_W, 36 + self.entries.len() as i32 * MENU_ROW + 8)
    }
    fn entry_at(&self, x: i32, y: i32) -> Option<usize> {
        let r = self.rect();
        if !r.contains(x, y) || y < r.y + 36 {
            return None;
        }
        let i = ((y - r.y - 36) / MENU_ROW) as usize;
        (i < self.entries.len()).then_some(i)
    }
}

// ---------------------------------------------------------------------------
// Artwork and live app previews (rendered by a background task).

fn art(name: &str) -> Option<Arc<Canvas>> {
    static CACHE: SpinLock<Vec<(String, Option<Arc<Canvas>>)>> = SpinLock::new(Vec::new());
    if let Some((_, c)) = CACHE.lock().iter().find(|(n, _)| n == name) {
        return c.clone();
    }
    let path = format!("usr/share/nova/home/{name}.png");
    let c = fs::system_file(&path).and_then(|d| Canvas::from_png(d).ok()).map(Arc::new);
    CACHE.lock().push((name.into(), c.clone()));
    c
}

/// Latest rendered frame per app path.
static PREVIEWS: SpinLock<Vec<(String, u64, Arc<Canvas>)>> = SpinLock::new(Vec::new());
/// The app the preview task should render next.
static WANTED: SpinLock<Option<String>> = SpinLock::new(None);

fn preview_of(path: &str) -> Option<(u64, Arc<Canvas>)> {
    PREVIEWS.lock().iter().find(|(p, ..)| p == path).map(|(_, t, c)| (*t, c.clone()))
}

fn preview_task() {
    let mut frame = 0u32;
    loop {
        if let Some(path) = WANTED.lock().take() {
            frame = frame.wrapping_add(10);
            if let Some(c) = apps::snapshot(&path, frame % 50) {
                let mut p = PREVIEWS.lock();
                p.retain(|(q, ..)| q != &path);
                if p.len() > 24 {
                    p.remove(0);
                }
                p.push((path, pit::ticks(), Arc::new(c)));
                drop(p);
                wm::request_redraw();
            }
        }
        task::sleep_ticks(pit::HZ / 10);
    }
}

// ---------------------------------------------------------------------------
// Drawing helpers

fn darken(c: u32, amount: u32) -> u32 {
    blend(c, 0, amount)
}

fn hash(s: &str) -> u32 {
    s.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619))
}

/// Scale `src` to cover `w`x`h` (cropping the overflow, centred).
fn cover(src: &Canvas, w: i32, h: i32) -> Canvas {
    let (sw, sh) = (src.width as i64, src.height as i64);
    let (cw, ch) = if sw * h as i64 > sh * w as i64 { (sh * w as i64 / h as i64, sh) } else { (sw, sw * h as i64 / w as i64) };
    let (cx, cy) = (((sw - cw) / 2) as i32, ((sh - ch) / 2) as i32);
    let mut crop = Canvas::new(cw as usize, ch as usize);
    crop.blit(src, Rect::new(cx, cy, cw as i32, ch as i32), 0, 0);
    let mut out = Canvas::new(w as usize, h as usize);
    out.blit_scaled(&crop, out.bounds(), 0, 255);
    out
}

/// Bottom-to-top darkening so text on artwork stays readable.
fn scrim(c: &mut Canvas, from: i32, strength: u32) {
    let h = c.height as i32;
    for y in from.max(0)..h {
        let t = ((y - from) * 255 / (h - from).max(1)) as u32;
        c.blend_rect(Rect::new(0, y, c.width as i32, 1), 0, t * strength / 255);
    }
}

fn wrap(text: &str, max_chars: usize, max_lines: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > max_chars {
            lines.push(core::mem::take(&mut cur));
            if lines.len() == max_lines {
                if let Some(l) = lines.last_mut() {
                    l.push_str("...");
                }
                return lines;
            }
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// A poster for an item: its artwork, or a generated design (gradient, pattern, big
/// icon), with the title on a scrim.
fn make_poster(item: &Item, w: i32, h: i32) -> Canvas {
    let mut c = match item.art.and_then(art) {
        Some(a) => cover(&a, w, h),
        None => {
            let mut c = Canvas::new(w as usize, h as usize);
            let (a, b) = (blend(item.color, 0xFFFFFF, 40), darken(item.color, 150));
            for y in 0..h {
                for x in 0..w {
                    let t = ((x + y) * 255 / (w + h)) as u32;
                    c.set(x, y, blend(a, b, t));
                }
            }
            let seed = hash(&item.title);
            match seed % 3 {
                0 => {
                    for i in 0..7 {
                        let r = 18 + i * 22;
                        c.stroke_circle(w - 40, h / 2, r, blend(item.color, 0xFFFFFF, 70));
                    }
                }
                1 => {
                    for i in (-h..w).step_by(18) {
                        c.line(i, h, i + h, 0, blend(item.color, 0xFFFFFF, 50));
                    }
                }
                _ => {
                    for gy in (10..h).step_by(16) {
                        for gx in (10..w).step_by(16) {
                            c.fill_circle(gx, gy, 2, blend(item.color, 0xFFFFFF, 60));
                        }
                    }
                }
            }
            let g = crate::textgrid::glyph(item.icon);
            let scale = h / 22;
            c.glyph(w - 8 * scale - 18, (h - 16 * scale) / 2 - 6, g, blend(item.color, 0xFFFFFF, 150), None, scale);
            c
        }
    };
    scrim(&mut c, h / 3, 230);
    let scale = if item.title.chars().count() as i32 * 16 <= w - 24 { 2 } else { 1 };
    let ty = h - 16 * scale - 12;
    c.text_scaled(13, ty + 1, &item.title, 0x000000, scale);
    c.text_scaled(12, ty, &item.title, TEXT, scale);
    c
}

/// Draw `src` at (x, y) with rounded corners.
fn blit_round(scene: &mut Canvas, src: &Canvas, x: i32, y: i32, radius: i32) {
    scene.blit_scaled(src, Rect::new(x, y, src.width as i32, src.height as i32), radius, 255);
}

fn weekday(unix: u64) -> &'static str {
    ["Thursday", "Friday", "Saturday", "Sunday", "Monday", "Tuesday", "Wednesday"][(unix / 86400 % 7) as usize]
}

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];

// ---------------------------------------------------------------------------

impl Home {
    pub fn new(width: i32, height: i32) -> Self {
        static STARTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        if !STARTED.swap(true, core::sync::atomic::Ordering::Relaxed) {
            task::spawn("home-previews", preview_task);
        }
        let mut background = Canvas::new(width as usize, height as usize);
        match art("wall") {
            Some(a) => {
                background = cover(&a, width, height);
                background.blend_rect(background.bounds(), BG, 175);
            }
            None => background.gradient(background.bounds(), 0x161827, BG),
        }
        Self { width, height, rows: Vec::new(), sel: (0, 0), top_row: 0, hovering: false, sel_since: 0, preview_on: false, posters: Vec::new(), hero: None, background, status: String::new(), cursor: (-1, -1), menu: None }
    }

    pub fn set_status(&mut self, status: &str) {
        self.status = status.into();
    }

    /// The static page background (drawn under everything).
    pub fn background(&self) -> &Canvas {
        &self.background
    }

    /// Rebuild the rows from the installed apps and open windows.
    pub fn refresh(&mut self, windows: &[OpenWindow]) {
        let selected = self.selected().map(|i| i.key());
        let browser = crate::browser::engine_installed();
        let mut rows = Vec::new();

        if !windows.is_empty() {
            let items = windows
                .iter()
                .map(|w| Item {
                    title: w.title.clone(),
                    category: "Continue",
                    about: "An open window. Open brings it to the front.".into(),
                    meta: "Running now".into(),
                    icon: w.icon,
                    color: w.accent,
                    art: None,
                    kind: Kind::Window(w.id),
                    action: Action::Focus(w.id),
                })
                .collect();
            rows.push(Row { title: "Continue", items, scroll: 0 });
        }

        let assistant = crate::claude::app_name();
        let idea = |title: &str, art: &'static str, about: &str, prompt: &str, color: u32| Item {
            title: title.into(),
            category: "Create with AI",
            about: about.into(),
            meta: format!("Opens {assistant} with this request"),
            icon: '*',
            color,
            art: Some(art),
            kind: Kind::Other,
            action: Action::Ask(prompt.into()),
        };
        rows.push(Row {
            title: "Create with AI",
            items: alloc::vec![
                Item {
                    title: assistant.into(),
                    category: "Create with AI",
                    about: format!("Your AI assistant. Ask it to build apps, explain this computer, write files or port drivers. {}", crate::claude::status_line()),
                    meta: "Builds apps with generated artwork".into(),
                    icon: '*',
                    color: 0xD97757,
                    art: Some("assistant"),
                    kind: Kind::Other,
                    action: Action::Claude,
                },
                idea("Build a game", "idea_game", "A small arcade game with generated artwork, made for you in a minute or two.", "Make me a fun little arcade game app with generated artwork and a high score.", 0x3FA7D6),
                idea("Weather app", "idea_weather", "A weather dashboard for your city, with illustrations for each kind of weather.", "Make me a weather app that shows the current weather and forecast for my city, with generated illustrations.", 0x4CAF7A),
                idea("Day planner", "idea_planner", "Plan your day: tasks, times and a progress bar, saved on this computer.", "Make me a day planner app where I can add tasks with times and check them off. Save them.", 0xC9A227),
                idea("Light show", "idea_music", "An animated, colourful visualiser to leave running.", "Make me an animated light show app with flowing neon waves.", 0x9C5BD6),
            ],
            scroll: 0,
        });

        let mut user_apps = Vec::new();
        let mut system_apps = Vec::new();
        if browser {
            system_apps.push(Item {
                title: "Browser".into(),
                category: "Apps",
                about: "Nova Browser: the Chromium engine with Shields, which blocks ads and trackers.".into(),
                meta: "Web".into(),
                icon: '@',
                color: 0xFB542B,
                art: Some("browser"),
                kind: Kind::Other,
                action: Action::Browser,
            });
        }
        system_apps.push(Item {
            title: "Terminal".into(),
            category: "Apps",
            about: "Nova's shell: files, network tools and Linux programs (C and C++).".into(),
            meta: "Type help for commands".into(),
            icon: '>',
            color: 0x3B4252,
            art: Some("terminal"),
            kind: Kind::Other,
            action: Action::Terminal,
        });
        for app in apps::installed() {
            let bundled = app.path.starts_with("system/");
            let art = match app.title.as_str() {
                "Files" if bundled => Some("files"),
                "Text Editor" if bundled => Some("editor"),
                "System Monitor" if bundled => Some("monitor"),
                "Settings" if bundled => Some("settings"),
                _ => None,
            };
            let item = Item {
                about: if app.about.is_empty() { "An app made on this computer.".into() } else { app.about.clone() },
                meta: format!("{}  -  {}x{}", app.path, app.width, app.height),
                category: if bundled { "Apps" } else { "Your apps" },
                title: app.title,
                icon: app.icon,
                color: app.color,
                art,
                kind: Kind::App(app.path.clone()),
                action: Action::Script(app.path),
            };
            if bundled { system_apps.push(item) } else { user_apps.push(item) }
        }
        if !user_apps.is_empty() {
            rows.push(Row { title: "Your apps", items: user_apps, scroll: 0 });
        }
        rows.push(Row { title: "Apps", items: system_apps, scroll: 0 });

        if browser {
            let site = |title: &str, url: &str, art: &'static str, about: &str, color: u32| Item {
                title: title.into(),
                category: "Web",
                about: about.into(),
                meta: url.into(),
                icon: '@',
                color,
                art: Some(art),
                kind: Kind::Other,
                action: Action::Url(url.into()),
            };
            rows.push(Row {
                title: "Web",
                items: alloc::vec![
                    site("YouTube", "https://www.youtube.com", "web_video", "Videos. Opens in Nova Browser; Shields blocks the ads it can.", 0xD93A3A),
                    site("Wikipedia", "https://en.wikipedia.org", "web_wiki", "The free encyclopedia.", 0x8A8F98),
                    site("GitHub", "https://github.com", "web_code", "Code hosting, including this operating system.", 0x6E5BD6),
                    site("DuckDuckGo", "https://duckduckgo.com", "web_search", "Private web search.", 0xE37A35),
                    site("Hacker News", "https://news.ycombinator.com", "web_news", "Technology news and discussion.", 0xF08A24),
                    site("Rust", "https://www.rust-lang.org", "web_forge", "The language Nova is written in.", 0xB7410E),
                ],
                scroll: 0,
            });
        }

        let power = |title: &str, about: &str, action: Action| Item {
            title: title.into(),
            category: "System",
            about: about.into(),
            meta: String::new(),
            icon: 'o',
            color: 0x4C5470,
            art: Some("power"),
            kind: Kind::Other,
            action,
        };
        rows.push(Row {
            title: "System",
            items: alloc::vec![
                power("Restart", "Restart the computer.", Action::Reboot),
                power("Shut down", "Turn the computer off.", Action::Shutdown),
            ],
            scroll: 0,
        });

        // Keep the scroll positions and the selection where possible.
        for row in rows.iter_mut() {
            if let Some(old) = self.rows.iter().find(|r| r.title == row.title) {
                row.scroll = old.scroll.min(row.items.len().saturating_sub(1));
            }
        }
        self.rows = rows;
        let found = selected.and_then(|k| {
            self.rows.iter().enumerate().find_map(|(r, row)| row.items.iter().position(|i| i.key() == k).map(|c| (r, c)))
        });
        self.sel = found.unwrap_or((self.sel.0.min(self.rows.len() - 1), 0));
        self.sel.1 = self.sel.1.min(self.rows[self.sel.0].items.len().saturating_sub(1));
        let keys: Vec<String> = self.rows.iter().flat_map(|r| r.items.iter().map(|i| i.key())).collect();
        self.posters.retain(|(k, ..)| keys.contains(k));
    }

    fn selected(&self) -> Option<&Item> {
        self.rows.get(self.sel.0)?.items.get(self.sel.1)
    }

    fn select(&mut self, sel: (usize, usize)) -> bool {
        if sel == self.sel {
            return false;
        }
        self.sel = sel;
        self.sel_since = pit::ticks();
        self.preview_on = false;
        true
    }

    // ----- layout -----

    fn rows_y(&self) -> i32 {
        TOP_BAR + HERO_H + 14
    }

    fn visible_cards(&self) -> usize {
        ((self.width - 2 * MARGIN + GAP) / (CARD_W + GAP)).max(1) as usize
    }

    fn row_y(&self, r: usize) -> Option<i32> {
        (r >= self.top_row).then(|| self.rows_y() + (r - self.top_row) as i32 * ROW_H)
    }

    fn card_rect(&self, r: usize, c: usize) -> Option<Rect> {
        let row = self.rows.get(r)?;
        let y = self.row_y(r)? + ROW_TITLE;
        if c < row.scroll || c >= row.scroll + self.visible_cards() || y > self.height {
            return None;
        }
        Some(Rect::new(MARGIN + (c - row.scroll) as i32 * (CARD_W + GAP), y, CARD_W, CARD_H))
    }

    fn open_button(&self) -> Rect {
        Rect::new(MARGIN, TOP_BAR + HERO_H - 66, 132, 40)
    }

    fn chevrons(&self, r: usize) -> Option<(Rect, Rect)> {
        let y = self.row_y(r)? + ROW_TITLE;
        Some((Rect::new(4, y, MARGIN - 10, CARD_H), Rect::new(self.width - MARGIN + 6, y, MARGIN - 10, CARD_H)))
    }

    fn ensure_visible(&mut self) {
        let vis = self.visible_cards();
        let (r, c) = self.sel;
        if let Some(row) = self.rows.get_mut(r) {
            if c < row.scroll {
                row.scroll = c;
            } else if c >= row.scroll + vis {
                row.scroll = c + 1 - vis;
            }
        }
        let fits = (((self.height - wm::TASKBAR_H - self.rows_y()) / ROW_H).max(1)) as usize;
        if r < self.top_row {
            self.top_row = r;
        } else if r >= self.top_row + fits {
            self.top_row = r + 1 - fits;
        }
    }

    // ----- input -----

    /// The pointer moved over Home (not over a window). True if the scene changed.
    pub fn hover(&mut self, x: i32, y: i32) -> bool {
        let old = self.cursor;
        self.cursor = (x, y);
        if let Some(m) = &self.menu {
            // While a menu is open, only its highlight follows the pointer.
            return m.entry_at(old.0, old.1) != m.entry_at(x, y);
        }
        for r in 0..self.rows.len() {
            for c in 0..self.rows[r].items.len() {
                if self.card_rect(r, c).is_some_and(|rect| rect.contains(x, y)) {
                    let changed = self.select((r, c)) | !self.hovering;
                    self.hovering = true;
                    return changed;
                }
            }
        }
        let was = self.hovering;
        self.hovering = false;
        was
    }

    /// A right click on Home: open the card's menu (running windows can be stopped).
    pub fn right_click(&mut self, x: i32, y: i32) -> Option<Action> {
        self.menu = None;
        for r in 0..self.rows.len() {
            for c in 0..self.rows[r].items.len() {
                if !self.card_rect(r, c).is_some_and(|rect| rect.contains(x, y)) {
                    continue;
                }
                self.select((r, c));
                let item = &self.rows[r].items[c];
                let mut entries = alloc::vec![("Open", item.action.clone())];
                if let Kind::Window(id) = item.kind {
                    entries.push(("Stop running", Action::Close(id)));
                }
                let at = ((x).min(self.width - MENU_W - 8), y);
                self.menu = Some(Menu { at, title: item.title.clone(), entries });
                let h = self.menu.as_ref().map_or(0, |m| m.rect().h);
                if let Some(m) = self.menu.as_mut() {
                    m.at.1 = m.at.1.min(self.height - h - 8);
                }
                return None;
            }
        }
        None
    }

    /// A click on Home. Returns the action to run, if any.
    pub fn click(&mut self, x: i32, y: i32) -> Option<Action> {
        if let Some(menu) = self.menu.take() {
            return menu.entry_at(x, y).map(|i| menu.entries[i].1.clone());
        }
        if self.open_button().contains(x, y) {
            return self.selected().map(|i| i.action.clone());
        }
        for r in 0..self.rows.len() {
            if let Some((left, right)) = self.chevrons(r) {
                let vis = self.visible_cards();
                let row = &mut self.rows[r];
                if left.contains(x, y) && row.scroll > 0 {
                    row.scroll = row.scroll.saturating_sub(vis);
                    return None;
                }
                if right.contains(x, y) && row.scroll + vis < row.items.len() {
                    row.scroll = (row.scroll + vis).min(row.items.len() - 1);
                    return None;
                }
            }
            for c in 0..self.rows[r].items.len() {
                if self.card_rect(r, c).is_some_and(|rect| rect.contains(x, y)) {
                    self.select((r, c));
                    return Some(self.rows[r].items[c].action.clone());
                }
            }
        }
        None
    }

    pub fn wheel(&mut self, steps: i32) {
        let max = self.rows.len().saturating_sub(1);
        self.top_row = (self.top_row as i32 + steps.signum()).clamp(0, max as i32) as usize;
    }

    /// A key while no window has the focus. Returns the action to run, if any.
    pub fn key(&mut self, key: Key) -> Option<Action> {
        self.hovering = false;
        if self.menu.take().is_some() && key == Key::Char(0x1B) {
            return None;
        }
        let (r, c) = self.sel;
        match key {
            Key::Left => {
                self.select((r, c.saturating_sub(1)));
            }
            Key::Right => {
                let n = self.rows[r].items.len();
                self.select((r, (c + 1).min(n - 1)));
            }
            Key::Up | Key::Down => {
                let nr = if key == Key::Up { r.saturating_sub(1) } else { (r + 1).min(self.rows.len() - 1) };
                // Keep the same screen column.
                let col = c.saturating_sub(self.rows[r].scroll) + self.rows[nr].scroll;
                self.select((nr, col.min(self.rows[nr].items.len() - 1)));
            }
            Key::Char(b'\n' | b'\r') => return self.selected().map(|i| i.action.clone()),
            _ => return None,
        }
        self.ensure_visible();
        None
    }

    /// Called every tick; true if a redraw is needed (the hover preview starts, or the
    /// previewed app is due for a new frame).
    pub fn tick(&mut self) -> bool {
        let now = pit::ticks();
        let due = now - self.sel_since >= PREVIEW_DELAY;
        if let Some(Kind::App(path)) = self.selected().map(|i| i.kind.clone())
            && due
        {
            let fresh = preview_of(&path).is_some_and(|(t, _)| now - t < pit::HZ);
            if !fresh {
                let mut w = WANTED.lock();
                if w.is_none() {
                    *w = Some(path);
                }
            }
        }
        if due && !self.preview_on {
            self.preview_on = true;
            return true;
        }
        false
    }

    // ----- drawing -----

    fn poster(&mut self, item: &Item, zoom: bool) -> usize {
        let key = item.key();
        if let Some(i) = self.posters.iter().position(|(k, z, _)| *k == key && *z == zoom) {
            return i;
        }
        let (w, h) = if zoom { (ZOOM_W, ZOOM_H) } else { (CARD_W, CARD_H) };
        self.posters.push((key, zoom, make_poster(item, w, h)));
        self.posters.len() - 1
    }

    /// Live content for an item (window thumbnail or app preview), drawn into `r`.
    fn draw_live(scene: &mut Canvas, item: &Item, r: Rect, radius: i32) -> bool {
        match &item.kind {
            Kind::Window(id) => wm::draw_window_thumb(scene, *id, r, radius),
            Kind::App(path) => match preview_of(path) {
                Some((_, c)) => {
                    scene.blit_scaled(&c, r, radius, 255);
                    true
                }
                None => false,
            },
            Kind::Other => false,
        }
    }

    pub fn draw(&mut self, scene: &mut Canvas) {
        let (w, h) = (self.width, self.height);
        let Some(item) = self.selected().cloned() else { return };

        // Hero: full-width artwork with scrims, cached per item.
        let key = item.key();
        if self.hero.as_ref().is_none_or(|(k, _)| *k != key) {
            let mut c = match item.art.and_then(art) {
                Some(a) => cover(&a, w, HERO_H),
                None => {
                    let mut c = Canvas::new(w as usize, HERO_H as usize);
                    c.gradient(c.bounds(), darken(item.color, 90), darken(item.color, 200));
                    c
                }
            };
            for x in 0..w * 3 / 5 {
                let t = 235 - (x * 235 / (w * 3 / 5)) as u32;
                c.blend_rect(Rect::new(x, 0, 1, HERO_H), BG, t);
            }
            scrim(&mut c, HERO_H - 90, 255);
            self.hero = Some((key, c));
        }
        if let Some((_, c)) = &self.hero {
            scene.blit(c, c.bounds(), 0, TOP_BAR);
        }
        // Blend the hero's top edge into the top bar.
        for y in 0..TOP_BAR + 30 {
            let t = if y < TOP_BAR { 255 } else { (255 - (y - TOP_BAR) * 255 / 30) as u32 };
            scene.blend_rect(Rect::new(0, y, w, 1), BG, t * 3 / 5);
        }

        // Top bar: wordmark, date and time.
        scene.text_scaled(MARGIN, 12, "nova", ACCENT, 2);
        scene.text(MARGIN + 74, 22, "home", DIM);
        let (_, mo, d, ..) = rtc::now();
        let when = format!("{} {} {}", weekday(rtc::unix_time()), d, MONTHS[(mo as usize).clamp(1, 12) - 1]);
        let right = w - MARGIN - (when.len() + 3 + self.status.len()) as i32 * 8;
        scene.text(right, 20, &self.status, DIM);
        scene.text(w - MARGIN - when.len() as i32 * 8, 20, &when, TEXT);

        // Hero text.
        let tx = MARGIN;
        let chip = item.category.to_uppercase();
        scene.fill_round_rect(Rect::new(tx, TOP_BAR + 30, chip.len() as i32 * 8 + 20, 22), 11, ACCENT);
        scene.text(tx + 10, TOP_BAR + 33, &chip, 0xFFFFFF);
        let scale = if item.title.chars().count() <= 18 { 4 } else { 3 };
        scene.text_scaled(tx + 2, TOP_BAR + 66 + 2, &item.title, 0x000000, scale);
        scene.text_scaled(tx, TOP_BAR + 66, &item.title, TEXT, scale);
        let mut ty = TOP_BAR + 70 + 16 * scale;
        for line in wrap(&item.about, 64, 3) {
            scene.text(tx, ty, &line, 0xD8D8E4);
            ty += 18;
        }
        if !item.meta.is_empty() {
            scene.text_clipped(tx, ty + 4, &item.meta, DIM, 560);
        }
        let open = self.open_button();
        scene.fill_round_rect(open, 8, 0xFFFFFF);
        // Play triangle.
        for i in 0..14 {
            let half = if i < 7 { i } else { 13 - i };
            scene.fill_rect(Rect::new(open.x + 24, open.y + 13 + i, half + 1, 1), 0x111111);
        }
        scene.text(open.x + 42, open.y + 12, "Open", 0x111111);
        scene.text(open.x + open.w + 18, open.y + 12, "Enter to open, arrows to browse", DIM);

        // Live preview panel on the right of the hero.
        let pw = (w * 7 / 20).min(448);
        let panel = Rect::new(w - MARGIN - pw, TOP_BAR + 26, pw, pw * 9 / 16);
        if matches!(item.kind, Kind::Window(_) | Kind::App(_)) {
            scene.shadow(panel, 14);
            scene.fill_round_rect(Rect::new(panel.x - 2, panel.y - 2, panel.w + 4, panel.h + 4), 12, 0x2A2C3E);
            if !Self::draw_live(scene, &item, panel, 10) {
                let i = self.poster(&item, true);
                scene.blit_scaled(&self.posters[i].2, panel, 10, 255);
            }
            scene.text(panel.x, panel.y + panel.h + 10, if matches!(item.kind, Kind::Window(_)) { "Live window" } else { "Live preview" }, DIM);
        }

        // Rows.
        let zoomed = self.hovering || self.preview_on;
        let mut zoom_card: Option<(usize, usize, Rect)> = None;
        for r in self.top_row..self.rows.len() {
            let Some(ry) = self.row_y(r) else { continue };
            if ry > h - wm::TASKBAR_H {
                break;
            }
            let title = self.rows[r].title;
            scene.text_scaled(MARGIN, ry, title, TEXT, 1);
            scene.text(MARGIN + title.len() as i32 * 8 + 10, ry, &format!("{}", self.rows[r].items.len()), DIM);
            for c in 0..self.rows[r].items.len() {
                let Some(rect) = self.card_rect(r, c) else { continue };
                if (r, c) == self.sel && zoomed {
                    zoom_card = Some((r, c, rect));
                    continue;
                }
                let it = self.rows[r].items[c].clone();
                scene.shadow(rect, 6);
                if matches!(it.kind, Kind::Window(_)) {
                    scene.fill_round_rect(rect, 8, 0x1A1B26);
                    Self::draw_live(scene, &it, Rect::new(rect.x, rect.y, rect.w, rect.h - 26), 8);
                    scene.text_clipped(rect.x + 10, rect.y + rect.h - 21, &it.title, TEXT, rect.w - 20);
                } else {
                    let i = self.poster(&it, false);
                    blit_round(scene, &self.posters[i].2, rect.x, rect.y, 8);
                }
                if (r, c) == self.sel {
                    // Keyboard selection without zoom: an outline.
                    scene.stroke_round_rect(Rect::new(rect.x - 3, rect.y - 3, rect.w + 6, rect.h + 6), 10, 0xFFFFFF);
                }
            }
            let vis = self.visible_cards();
            let row = &self.rows[r];
            if let Some((left, right)) = self.chevrons(r) {
                if row.scroll > 0 {
                    scene.glyph(left.x + 8, left.y + CARD_H / 2 - 16, crate::textgrid::glyph('<'), TEXT, None, 2);
                }
                if row.scroll + vis < row.items.len() {
                    scene.glyph(right.x + 6, right.y + CARD_H / 2 - 16, crate::textgrid::glyph('>'), TEXT, None, 2);
                }
            }
        }

        // The selected card, enlarged on top, with its live preview once it has been
        // selected for a moment.
        if let Some((r, c, rect)) = zoom_card {
            let it = self.rows[r].items[c].clone();
            let z = Rect::new(rect.x - (ZOOM_W - CARD_W) / 2, rect.y - (ZOOM_H - CARD_H) / 2, ZOOM_W, ZOOM_H);
            let z = Rect::new(z.x.max(4), z.y, z.w, z.h);
            scene.shadow(z, 18);
            scene.fill_round_rect(Rect::new(z.x - 3, z.y - 3, z.w + 6, z.h + 6), 12, 0xFFFFFF);
            let live = self.preview_on && Self::draw_live(scene, &it, z, 10);
            if !live {
                let i = self.poster(&it, true);
                blit_round(scene, &self.posters[i].2, z.x, z.y, 10);
            } else {
                scene.blend_rect(Rect::new(z.x, z.y + z.h - 28, z.w, 28), 0, 160);
                scene.text_clipped(z.x + 10, z.y + z.h - 22, &it.title, TEXT, z.w - 20);
            }
        }

        // Context menu.
        if let Some(menu) = &self.menu {
            let r = menu.rect();
            scene.shadow(r, 12);
            scene.fill_round_rect(r, 10, 0x1C1D2A);
            scene.text_clipped(r.x + 14, r.y + 12, &menu.title, DIM, r.w - 28);
            for (i, (label, _)) in menu.entries.iter().enumerate() {
                let e = Rect::new(r.x + 6, r.y + 36 + i as i32 * MENU_ROW, r.w - 12, MENU_ROW - 2);
                if menu.entry_at(self.cursor.0, self.cursor.1) == Some(i) {
                    scene.fill_round_rect(e, 6, 0x34364C);
                }
                let color = if *label == "Stop running" { 0xFF7A85 } else { TEXT };
                scene.text(e.x + 10, e.y + 6, label, color);
            }
        }
    }
}
