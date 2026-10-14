//! Nova Home: the desktop's full-screen launcher (in the design language of TV and
//! console launchers such as Steam Big Picture, implemented for Nova).
//!
//! Tabs in the header (Home, Library, Web); the focused item's artwork fills the
//! screen behind everything; its title, description, an Open button and a live preview
//! sit at the top; below, shelves of portrait covers: open windows (live thumbnails),
//! your apps, apps, all apps in the Library grid, web sites, and system actions. The
//! assistant is not here: it is the chat bubble in the corner. Hovering a card enlarges it and, after a moment, plays a
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
use crate::font::{self, Weight};
use crate::{apps, fs, pit, rtc, task};

// Portrait covers (2:3-ish), like game capsules.
const CARD_W: i32 = 150;
const CARD_H: i32 = 214;
const ZOOM_W: i32 = 172;
const ZOOM_H: i32 = 246;
const GAP: i32 = 20;
const MARGIN: i32 = 44;
const TOP_BAR: i32 = 68;
const HERO_H: i32 = 232;
const ROW_H: i32 = 292;
const ROW_TITLE: i32 = 42;
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

#[derive(Clone)]
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
    /// The current tab's rows.
    rows: Vec<Row>,
    tab: usize,
    tab_names: Vec<&'static str>,
    tab_rows: Vec<Vec<Row>>,
    sel: (usize, usize),
    top_row: usize,
    /// The selection follows the mouse (zoom + preview) rather than the keyboard.
    hovering: bool,
    sel_since: u64,
    preview_on: bool,
    posters: Vec<(String, bool, Canvas)>,
    /// Full-screen backdrop for the focused item (cached by item).
    backdrop: Option<(String, Canvas)>,
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
    let size: u16 = if w > CARD_W { 22 } else { 19 };
    let size = if font::width(&item.title, size, Weight::Bold) <= w - 28 { size } else { size - 4 };
    let ty = h - font::line_height(size) - 10;
    font::draw_clipped(&mut c, 15, ty + 1, &item.title, size, Weight::Bold, 0x000000, w - 28);
    font::draw_clipped(&mut c, 14, ty, &item.title, size, Weight::Bold, TEXT, w - 28);
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
        Self {
            width,
            height,
            rows: Vec::new(),
            tab: 0,
            tab_names: Vec::new(),
            tab_rows: Vec::new(),
            sel: (0, 0),
            top_row: 0,
            hovering: false,
            sel_since: 0,
            preview_on: false,
            posters: Vec::new(),
            backdrop: None,
            background,
            status: String::new(),
            cursor: (-1, -1),
            menu: None,
        }
    }

    pub fn set_status(&mut self, status: &str) {
        self.status = status.into();
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

        // The assistant has no row: it is the bubble in the bottom-right corner.

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

        // Tabs: Home (what you use), Library (everything, as a grid), Web.
        let per_row = self.visible_cards();
        let grid = |title: &'static str, items: Vec<Item>| -> Vec<Row> {
            items.chunks(per_row).enumerate().map(|(i, chunk)| Row { title: if i == 0 { title } else { "" }, items: chunk.to_vec(), scroll: 0 }).collect()
        };
        let take = |t: &str| rows.iter().find(|r| r.title == t).map(|r| r.items.clone()).unwrap_or_default();
        let mut library_items = take("Apps");
        library_items.extend(take("Your apps"));
        let mut library = grid("All apps", library_items);
        library.push(Row { title: "System", items: take("System"), scroll: 0 });
        let home: Vec<Row> = rows.iter().filter(|r| matches!(r.title, "Continue" | "Your apps" | "Apps")).cloned().collect();
        self.tab_names = alloc::vec!["Home", "Library"];
        self.tab_rows = alloc::vec![home, library];
        if browser {
            self.tab_names.push("Web");
            self.tab_rows.push(grid("Sites", take("Web")));
        }
        self.tab = self.tab.min(self.tab_rows.len() - 1);
        let mut rows = self.tab_rows[self.tab].clone();

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
        let keys: Vec<String> = self.tab_rows.iter().flatten().flat_map(|r| r.items.iter().map(|i| i.key())).collect();
        self.posters.retain(|(k, ..)| keys.contains(k));
        self.ensure_visible();
    }

    fn switch_tab(&mut self, tab: usize) {
        if tab >= self.tab_rows.len() || tab == self.tab {
            return;
        }
        self.tab = tab;
        self.rows = self.tab_rows[tab].clone();
        self.top_row = 0;
        self.select((0, 0));
        self.menu = None;
    }

    fn tab_rects(&self) -> Vec<Rect> {
        let widths: Vec<i32> = self.tab_names.iter().map(|t| font::width(t, 16, Weight::Bold) + 36).collect();
        let mut x = MARGIN + font::width("nova", 28, Weight::Bold) + 36;
        widths
            .iter()
            .map(|tw| {
                let r = Rect::new(x, 16, *tw, 36);
                x += tw + 8;
                r
            })
            .collect()
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
        // Keyboard and programmatic moves scroll; the mouse only picks what is visible.
        if !self.hovering {
            self.ensure_visible();
        }
        true
    }

    // ----- layout -----

    fn rows_y(&self) -> i32 {
        TOP_BAR + HERO_H + 10
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
        Rect::new(MARGIN, TOP_BAR + HERO_H - 54, 140, 44)
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
        let fits = (((self.height - 60 - self.rows_y()) / ROW_H).max(1)) as usize;
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
        if let Some(t) = self.tab_rects().iter().position(|r| r.contains(x, y)) {
            self.switch_tab(t);
            return None;
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
            Key::Char(b'\t') => {
                // Tab: next section.
                let next = (self.tab + 1) % self.tab_rows.len().max(1);
                self.switch_tab(next);
                return None;
            }
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

        // Backdrop: the focused item's artwork across the whole screen, darkened, with a
        // fade to the shelves and a darker left side under the hero text.
        let key = item.key();
        if self.backdrop.as_ref().is_none_or(|(k, _)| *k != key) {
            let mut c = match item.art.and_then(art) {
                Some(a) => cover(&a, w, h),
                // No artwork: the wallpaper, tinted towards the item's colour.
                None => {
                    let mut c = self.background.clone();
                    c.blend_rect(c.bounds(), darken(item.color, 100), 60);
                    c
                }
            };
            c.blend_rect(c.bounds(), BG, 120);
            for x in 0..w * 3 / 5 {
                let t = 200 - (x * 200 / (w * 3 / 5)) as u32;
                c.blend_rect(Rect::new(x, 0, 1, h), BG, t);
            }
            scrim(&mut c, TOP_BAR + HERO_H / 2, 250);
            self.backdrop = Some((key, c));
        }
        if let Some((_, c)) = &self.backdrop {
            scene.blit(c, c.bounds(), 0, 0);
        }
        scene.gradient(Rect::new(0, 0, w, TOP_BAR), 0x08090E, 0x0B0C12);
        scene.blend_rect(Rect::new(0, 0, w, TOP_BAR), BG, 60);

        // Tabs.
        for (i, (r, name)) in self.tab_rects().iter().zip(self.tab_names.iter()).enumerate() {
            let active = i == self.tab;
            if active {
                scene.fill_round_rect(*r, r.h / 2, 0xF2F2F7);
            }
            let tw = font::width(name, 16, Weight::Bold);
            font::draw(scene, r.x + (r.w - tw) / 2, r.y + 7, name, 16, Weight::Bold, if active { 0x12131A } else { DIM });
        }

        // Top bar: wordmark, date and time.
        font::draw(scene, MARGIN, 14, "nova", 28, Weight::Bold, ACCENT);
        let (_, mo, d, ..) = rtc::now();
        let when = format!("{} {} {}", weekday(rtc::unix_time()), d, MONTHS[(mo as usize).clamp(1, 12) - 1]);
        let when_w = font::width(&when, 15, Weight::Bold);
        font::draw(scene, w - MARGIN - when_w, 18, &when, 15, Weight::Bold, TEXT);
        let status_w = font::width(&self.status, 14, Weight::Regular);
        font::draw(scene, w - MARGIN - when_w - 28 - status_w, 19, &self.status, 14, Weight::Regular, DIM);

        // Hero text.
        let tx = MARGIN;
        let chip = item.category.to_uppercase();
        let chip_w = font::width(&chip, 12, Weight::Bold) + 22;
        scene.fill_round_rect(Rect::new(tx, TOP_BAR + 14, chip_w, 24), 12, ACCENT);
        font::draw(scene, tx + 11, TOP_BAR + 18, &chip, 12, Weight::Bold, 0xFFFFFF);
        let tmax = w * 11 / 20;
        let tsize: u16 = if font::width(&item.title, 46, Weight::Bold) <= tmax { 46 } else { 34 };
        font::draw_clipped(scene, tx + 2, TOP_BAR + 44 + 2, &item.title, tsize, Weight::Bold, 0x000000, tmax);
        font::draw_clipped(scene, tx, TOP_BAR + 44, &item.title, tsize, Weight::Bold, TEXT, tmax);
        let mut ty = TOP_BAR + 50 + font::line_height(tsize);
        for line in font::wrap(&item.about, 16, Weight::Regular, w * 9 / 20, 2) {
            font::draw(scene, tx, ty, &line, 16, Weight::Regular, 0xDCDCE6);
            ty += 22;
        }
        if !item.meta.is_empty() && ty + 18 < self.open_button().y {
            font::draw_clipped(scene, tx, ty + 2, &item.meta, 13, Weight::Regular, DIM, w * 9 / 20);
        }
        let open = self.open_button();
        scene.fill_round_rect(open, open.h / 2, 0xFFFFFF);
        // Play triangle.
        for i in 0..16 {
            let half = if i < 8 { i } else { 15 - i };
            scene.fill_rect(Rect::new(open.x + 26, open.y + 12 + i, half + 1, 1), 0x111111);
        }
        font::draw(scene, open.x + 46, open.y + 10, "Open", 17, Weight::Bold, 0x111111);


        // Live preview panel on the right of the hero.
        let pw = (w * 3 / 10).min(384);
        let panel = Rect::new(w - MARGIN - pw, TOP_BAR + 14, pw, pw * 9 / 16);
        if matches!(item.kind, Kind::Window(_) | Kind::App(_)) {
            scene.shadow(panel, 14);
            scene.fill_round_rect(Rect::new(panel.x - 2, panel.y - 2, panel.w + 4, panel.h + 4), 12, 0x2A2C3E);
            if !Self::draw_live(scene, &item, panel, 10) {
                let i = self.poster(&item, true);
                scene.blit_scaled(&self.posters[i].2, panel, 10, 255);
            }
            let badge = if matches!(item.kind, Kind::Window(_)) { "LIVE WINDOW" } else { "LIVE PREVIEW" };
            let bw = font::width(badge, 11, Weight::Bold) + 16;
            scene.fill_round_rect(Rect::new(panel.x + 10, panel.y + 10, bw, 20), 10, 0xE5394B);
            font::draw(scene, panel.x + 18, panel.y + 12, badge, 11, Weight::Bold, 0xFFFFFF);
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
            if !title.is_empty() {
                let end = font::draw(scene, MARGIN, ry - 2, title, 20, Weight::Bold, TEXT);
                let count: usize = if self.tab == 1 && title == "All apps" { self.rows.iter().filter(|r| r.title != "System").map(|r| r.items.len()).sum() } else { self.rows[r].items.len() };
                font::draw(scene, end + 10, ry + 1, &format!("{count}"), 15, Weight::Regular, DIM);
            }
            for c in 0..self.rows[r].items.len() {
                let Some(rect) = self.card_rect(r, c) else { continue };
                if (r, c) == self.sel && zoomed {
                    zoom_card = Some((r, c, rect));
                    continue;
                }
                let it = self.rows[r].items[c].clone();
                scene.shadow(rect, 6);
                if matches!(it.kind, Kind::Window(_)) {
                    scene.fill_round_rect(rect, 12, 0x1A1B26);
                    Self::draw_live(scene, &it, Rect::new(rect.x, rect.y, rect.w, rect.h - 30), 12);
                    crate::icons::draw(scene, it.icon, it.color, Rect::new(rect.x + 10, rect.y + rect.h - 27, 22, 22));
                    font::draw_clipped(scene, rect.x + 40, rect.y + rect.h - 26, &it.title, 15, Weight::Bold, TEXT, rect.w - 52);
                } else {
                    let i = self.poster(&it, false);
                    blit_round(scene, &self.posters[i].2, rect.x, rect.y, 12);
                }
                if (r, c) == self.sel {
                    // Keyboard selection without zoom: an outline.
                    scene.stroke_round_rect(Rect::new(rect.x - 4, rect.y - 4, rect.w + 8, rect.h + 8), 15, 0xFFFFFF);
                }
            }
            let vis = self.visible_cards();
            let row = &self.rows[r];
            if let Some((left, right)) = self.chevrons(r) {
                if row.scroll > 0 {
                    font::draw(scene, left.x + 6, left.y + CARD_H / 2 - 22, "\u{2039}", 40, Weight::Bold, TEXT);
                }
                if row.scroll + vis < row.items.len() {
                    font::draw(scene, right.x + 8, right.y + CARD_H / 2 - 22, "\u{203a}", 40, Weight::Bold, TEXT);
                }
            }
        }

        // The selected card, enlarged on top, with its live preview once it has been
        // selected for a moment.
        if let Some((r, c, rect)) = zoom_card {
            let it = self.rows[r].items[c].clone();
            let z = Rect::new(rect.x - (ZOOM_W - CARD_W) / 2, rect.y - (ZOOM_H - CARD_H) / 2, ZOOM_W, ZOOM_H);
            let z = Rect::new(z.x.max(4), z.y, z.w, z.h);
            // Soft white glow around the focused card.
            for i in (4..=14).rev() {
                let a = (14 - i) as u32 * 5;
                scene.blend_rect(Rect::new(z.x - i, z.y - i + 6, z.w + 2 * i, z.h + 2 * i - 12), 0xFFFFFF, a / 3);
            }
            scene.shadow(z, 18);
            scene.fill_round_rect(Rect::new(z.x - 4, z.y - 4, z.w + 8, z.h + 8), 16, 0xFFFFFF);
            let live = self.preview_on && Self::draw_live(scene, &it, z, 13);
            if !live {
                let i = self.poster(&it, true);
                blit_round(scene, &self.posters[i].2, z.x, z.y, 13);
            } else {
                scene.blend_rect(Rect::new(z.x, z.y + z.h - 32, z.w, 32), 0, 170);
                font::draw_clipped(scene, z.x + 12, z.y + z.h - 27, &it.title, 16, Weight::Bold, TEXT, z.w - 24);
            }
        }

        // Key hints, bottom-left (no bar).
        let mut hx = MARGIN;
        let hy = h - 40;
        scene.blend_rect(Rect::new(0, h - 60, w, 60), BG, 120);
        for (key, label) in [("Enter", "Open"), ("Tab", "Section"), ("Arrows", "Move"), ("Windows key", "Home"), ("F1", "Assistant"), ("Right-click", "Options")] {
            let kw = font::width(key, 12, Weight::Bold) + 16;
            scene.fill_round_rect(Rect::new(hx, hy, kw, 24), 7, 0x2C2E40);
            font::draw(scene, hx + 8, hy + 4, key, 12, Weight::Bold, TEXT);
            hx += kw + 8;
            hx = font::draw(scene, hx, hy + 3, label, 13, Weight::Regular, DIM) + 22;
        }

        // Context menu.
        if let Some(menu) = &self.menu {
            let r = menu.rect();
            scene.shadow(r, 12);
            scene.fill_round_rect(r, 12, 0x1C1D2A);
            font::draw_clipped(scene, r.x + 14, r.y + 10, &menu.title, 14, Weight::Regular, DIM, r.w - 28);
            for (i, (label, _)) in menu.entries.iter().enumerate() {
                let e = Rect::new(r.x + 6, r.y + 36 + i as i32 * MENU_ROW, r.w - 12, MENU_ROW - 2);
                if menu.entry_at(self.cursor.0, self.cursor.1) == Some(i) {
                    scene.fill_round_rect(e, 6, 0x34364C);
                }
                let color = if *label == "Stop running" { 0xFF7A85 } else { TEXT };
                font::draw(scene, e.x + 10, e.y + 4, label, 15, Weight::Bold, color);
            }
        }
    }
}
