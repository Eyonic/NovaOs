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
//! assets/README.md): a portrait capsule per item for the cards and a wide
//! `hero_<name>.png` for the backdrop, composed with its subject on the right so the
//! title, description and Open button sit on empty space. Items without artwork get a
//! procedural poster.
//!
//! Typing on Home searches: the query shows in the header and the shelves are
//! replaced by one row of matches across every tab (Escape clears it).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::gfx::{blend, Canvas, Rect};
use crate::input::Key;
use crate::sync::SpinLock;
use crate::wm::{self, Action};
use crate::font::{self, Weight};
use crate::{apps, fs, pit, rtc, task, ui};

// Portrait covers (2:3-ish), like game capsules. Everything sits on an 8 px grid.
const CARD_W: i32 = 152;
const CARD_H: i32 = 216;
const ZOOM_W: i32 = 176;
const ZOOM_H: i32 = 250;
const GAP: i32 = 20;
const MARGIN: i32 = 48;
const TOP_BAR: i32 = 64;
const HERO_H: i32 = 232;
const ROW_H: i32 = 304;
/// Space above a row's cards for its title (the zoomed card grows 21 px upwards).
const ROW_TITLE: i32 = 48;
/// Hover time before a card's preview starts (ticks).
const PREVIEW_DELAY: u64 = pit::HZ * 6 / 10;

const BG: u32 = 0x0B0C12;
const TEXT: u32 = 0xF2F2F7;
const DIM: u32 = 0xA2A4B8;
const ACCENT: u32 = 0x8B7CF6;

/// What the pointer is over besides cards (for hover states).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hot {
    None,
    Open,
    Tab(usize),
    Left(usize),
    Right(usize),
}

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
    status: wm::Status,
    cursor: (i32, i32),
    hot: Hot,
    /// Open context menu: its card, position and entries.
    menu: Option<Menu>,
    /// Type-to-search: the query (empty = browsing the tabs).
    query: String,
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

/// The wide backdrop artwork for an item (`hero_<name>.png`), or its capsule if there is none.
fn hero(name: &str) -> Option<Arc<Canvas>> {
    art(&format!("hero_{name}")).or_else(|| art(name))
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

/// Scale `src` to cover `w`x`h` (cropping the overflow, centred). Shrinking box-filters
/// (`blit_scaled`); enlarging, which only the full-screen backdrop does, samples
/// bilinearly so the artwork stays smooth instead of blocky.
fn cover(src: &Canvas, w: i32, h: i32) -> Canvas {
    let (sw, sh) = (src.width as i64, src.height as i64);
    let (cw, ch) = if sw * h as i64 > sh * w as i64 { (sh * w as i64 / h as i64, sh) } else { (sw, sw * h as i64 / w as i64) };
    let (cx, cy) = ((sw - cw) / 2, (sh - ch) / 2);
    let mut out = Canvas::new(w as usize, h as usize);
    if cw >= w as i64 {
        let mut crop = Canvas::new(cw as usize, ch as usize);
        crop.blit(src, Rect::new(cx as i32, cy as i32, cw as i32, ch as i32), 0, 0);
        out.blit_scaled(&crop, out.bounds(), 0, 255);
        return out;
    }
    // 24.8 fixed point source coordinates of each output pixel's centre.
    let pos = |o: i32, n: i32, c: i64, off: i64, limit: i64| -> (usize, u32) {
        let p = (((2 * o as i64 + 1) * c * 256) / (2 * n as i64) - 128 + off * 256).clamp(0, (limit - 1) * 256);
        ((p >> 8) as usize, (p & 255) as u32)
    };
    for y in 0..h {
        let (y0, fy) = pos(y, h, ch, cy, sh);
        let y1 = (y0 + 1).min(sh as usize - 1);
        for x in 0..w {
            let (x0, fx) = pos(x, w, cw, cx, sw);
            let x1 = (x0 + 1).min(sw as usize - 1);
            let top = blend(src.pixels[y0 * src.width + x0], src.pixels[y0 * src.width + x1], fx);
            let bottom = blend(src.pixels[y1 * src.width + x0], src.pixels[y1 * src.width + x1], fx);
            out.pixels[y as usize * w as usize + x as usize] = blend(top, bottom, fy);
        }
    }
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
    // The capsules keep their bottom quarter quiet for the title; the scrim covers it.
    scrim(&mut c, h * 2 / 5, 230);
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
            status: wm::Status::default(),
            cursor: (-1, -1),
            hot: Hot::None,
            menu: None,
            query: String::new(),
        }
    }

    pub fn set_status(&mut self, status: wm::Status) {
        self.status = status;
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

        let power = |title: &str, about: &str, art: &'static str, action: Action| Item {
            title: title.into(),
            category: "System",
            about: about.into(),
            meta: String::new(),
            icon: 'o',
            color: 0x5B4FA8,
            art: Some(art),
            kind: Kind::Other,
            action,
        };
        rows.push(Row {
            title: "System",
            items: alloc::vec![
                power("Restart", "Restart the computer.", "restart", Action::Reboot),
                power("Shut down", "Turn the computer off.", "shutdown", Action::Shutdown),
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
        let mut rows = if self.query.is_empty() { self.tab_rows[self.tab].clone() } else { self.search_rows() };

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
        if tab >= self.tab_rows.len() || (tab == self.tab && self.query.is_empty()) {
            return;
        }
        self.tab = tab;
        self.query.clear();
        self.rows = self.tab_rows[tab].clone();
        self.top_row = 0;
        self.select((0, 0));
        self.menu = None;
    }

    /// One row of everything that matches the query, best match first.
    fn search_rows(&self) -> Vec<Row> {
        let mut found: Vec<(i32, Item)> = Vec::new();
        for item in self.tab_rows.iter().flatten().flat_map(|r| r.items.iter()) {
            if found.iter().any(|(_, i)| i.key() == item.key()) {
                continue;
            }
            // Fuzzy on the title; descriptions only match as a whole word or phrase.
            let q = self.query.to_lowercase();
            let score = ui::fuzzy(&q, &item.title).map(|s| s + 20).or_else(|| item.about.to_lowercase().contains(&q).then_some(0));
            if let Some(s) = score {
                found.push((s, item.clone()));
            }
        }
        found.sort_by_key(|(s, _)| -s);
        let items: Vec<Item> = found.into_iter().map(|(_, i)| i).collect();
        alloc::vec![Row { title: "Results", items, scroll: 0 }]
    }

    /// The query changed: show matches (or the tab again when it is empty).
    fn apply_query(&mut self) {
        self.rows = if self.query.is_empty() { self.tab_rows[self.tab].clone() } else { self.search_rows() };
        self.top_row = 0;
        self.sel = (0, 0);
        self.sel_since = pit::ticks();
        self.preview_on = false;
        self.menu = None;
    }

    fn tab_rects(&self) -> Vec<Rect> {
        let widths: Vec<i32> = self.tab_names.iter().map(|t| font::width(t, 16, Weight::Bold) + 36).collect();
        let mut x = MARGIN + font::width("nova", 28, Weight::Bold) + 36;
        widths
            .iter()
            .map(|tw| {
                let r = Rect::new(x, 14, *tw, 36);
                x += tw + 8;
                r
            })
            .collect()
    }

    /// The search pill in the header, right of the tabs.
    fn search_rect(&self) -> Rect {
        let tabs = self.tab_rects();
        let x = tabs.last().map_or(MARGIN, |r| r.x + r.w) + 24;
        let w = if self.query.is_empty() { 120 } else { (font::width(&self.query, 15, Weight::Regular) + 56).max(120) };
        Rect::new(x, 14, w, 36)
    }

    /// What is under the pointer besides cards.
    fn hot_at(&self, x: i32, y: i32) -> Hot {
        if self.open_button().contains(x, y) {
            return Hot::Open;
        }
        if let Some(t) = self.tab_rects().iter().position(|r| r.contains(x, y)) {
            return Hot::Tab(t);
        }
        for r in 0..self.rows.len() {
            if let Some((left, right)) = self.chevrons(r) {
                if left.contains(x, y) {
                    return Hot::Left(r);
                }
                if right.contains(x, y) {
                    return Hot::Right(r);
                }
            }
        }
        Hot::None
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
        TOP_BAR + HERO_H + 8
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
        Rect::new(MARGIN, TOP_BAR + HERO_H - 56, 140, 44)
    }

    /// Round scroll buttons at both ends of a row (only drawn when there is more).
    fn chevrons(&self, r: usize) -> Option<(Rect, Rect)> {
        let y = self.row_y(r)? + ROW_TITLE + CARD_H / 2 - 18;
        Some((Rect::new(6, y, 36, 36), Rect::new(self.width - 42, y, 36, 36)))
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
        let hot = self.hot_at(x, y);
        let hot_changed = hot != self.hot;
        self.hot = hot;
        for r in 0..self.rows.len() {
            for c in 0..self.rows[r].items.len() {
                if self.card_rect(r, c).is_some_and(|rect| rect.contains(x, y)) {
                    let changed = self.select((r, c)) | !self.hovering;
                    self.hovering = true;
                    return changed | hot_changed;
                }
            }
        }
        let was = self.hovering;
        self.hovering = false;
        was | hot_changed
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
        if self.search_rect().contains(x, y) && !self.query.is_empty() {
            self.query.clear();
            self.apply_query();
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
        if self.rows.is_empty() || self.rows[r].items.is_empty() {
            // Only the search can be empty: keep editing it.
            match key {
                Key::Char(0x1B) | Key::Char(b'\n' | b'\r') => self.query.clear(),
                Key::Char(0x08 | 0x7F) => {
                    self.query.pop();
                }
                Key::Char(ch) if (0x20..0x7F).contains(&ch) => self.query.push(ch as char),
                _ => {}
            }
            self.apply_query();
            return None;
        }
        match key {
            Key::Char(b'\t') => {
                // Tab: next section.
                let next = if self.query.is_empty() { (self.tab + 1) % self.tab_rows.len().max(1) } else { self.tab };
                self.switch_tab(next);
                return None;
            }
            // Type to search; Backspace edits, Escape clears.
            Key::Char(ch) if (0x20..0x7F).contains(&ch) => {
                self.query.push(ch as char);
                self.apply_query();
                return None;
            }
            Key::Char(0x08 | 0x7F) => {
                self.query.pop();
                self.apply_query();
                return None;
            }
            Key::Char(0x1B) if !self.query.is_empty() => {
                self.query.clear();
                self.apply_query();
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
                self.select((nr, col.min(self.rows[nr].items.len().saturating_sub(1))));
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

    /// The header: wordmark, tabs, the search pill, machine status, date and time.
    fn draw_header(&self, scene: &mut Canvas) {
        let w = self.width;
        scene.gradient(Rect::new(0, 0, w, TOP_BAR), 0x08090E, 0x0B0C12);
        scene.blend_rect(Rect::new(0, 0, w, TOP_BAR), BG, 60);
        scene.fill_rect(Rect::new(0, TOP_BAR - 1, w, 1), blend(BG, 0xFFFFFF, 14));
        font::draw(scene, MARGIN, 12, "nova", 28, Weight::Bold, ACCENT);

        // Tabs: the active one is a light pill; the others brighten under the pointer.
        let searching = !self.query.is_empty();
        for (i, (r, name)) in self.tab_rects().iter().zip(self.tab_names.iter()).enumerate() {
            let active = i == self.tab && !searching;
            let hot = self.hot == Hot::Tab(i);
            if active {
                scene.fill_round_rect(*r, r.h / 2, 0xF2F2F7);
            } else if hot {
                scene.fill_round_rect(*r, r.h / 2, blend(BG, 0xFFFFFF, 24));
            }
            let tw = font::width(name, 16, Weight::Bold);
            let color = if active { 0x12131A } else if hot { TEXT } else { DIM };
            font::draw(scene, r.x + (r.w - tw) / 2, r.y + (r.h - font::line_height(16)) / 2, name, 16, Weight::Bold, color);
        }

        // Search: a pill that shows the query as it is typed.
        let sr = self.search_rect();
        scene.fill_round_rect(sr, sr.h / 2, if searching { blend(BG, 0xFFFFFF, 30) } else { blend(BG, 0xFFFFFF, 12) });
        if searching {
            scene.stroke_round_rect_w(sr, sr.h / 2, 2, ACCENT);
        }
        ui::symbol(scene, "search", if searching { TEXT } else { DIM }, Rect::new(sr.x + 13, sr.y + 10, 16, 16));
        let ty = sr.y + (sr.h - font::line_height(15)) / 2;
        if searching {
            let end = font::draw(scene, sr.x + 38, ty, &self.query, 15, Weight::Regular, TEXT);
            scene.fill_rect(Rect::new(end + 2, ty + 3, 2, 16), ACCENT);
        } else {
            font::draw(scene, sr.x + 38, ty, "Search", 15, Weight::Regular, DIM);
        }

        // Right side: date and time, then the machine status with small glyphs.
        let (_, mo, d, ..) = rtc::now();
        let when = format!("{} {} {}", weekday(rtc::unix_time()), d, MONTHS[(mo as usize).clamp(1, 12) - 1]);
        let clock_w = font::width(&self.status.clock, 15, Weight::Bold);
        let mut x = w - MARGIN - clock_w;
        let y = (TOP_BAR - font::line_height(15)) / 2;
        font::draw(scene, x, y, &self.status.clock, 15, Weight::Bold, TEXT);
        x -= font::width(&when, 15, Weight::Regular) + 14;
        font::draw(scene, x, y, &when, 15, Weight::Regular, DIM);
        x -= 28;
        let s = &self.status;
        let mem = s.mem.clone();
        let cpu = format!("{}%", s.cpu);
        for (glyph, text, color) in [("memory", mem.as_str(), DIM), ("cpu", cpu.as_str(), DIM), ("network", s.net.as_str(), if s.online { DIM } else { ui::WARNING })] {
            let tw = font::width(text, 13, Weight::Regular);
            x -= tw;
            font::draw(scene, x, y + 1, text, 13, Weight::Regular, color);
            x -= 20;
            ui::symbol(scene, glyph, color, Rect::new(x, y + 2, 15, 15));
            x -= 18;
        }
    }

    pub fn draw(&mut self, scene: &mut Canvas) {
        let (w, h) = (self.width, self.height);
        let item = self.selected().cloned();

        // Backdrop: the focused item's hero artwork across the whole screen. The art
        // keeps its subject on the right, so the darkening is light overall, a gradient
        // on the left under the hero text, and a fade down to the shelves.
        let key = item.as_ref().map_or(String::new(), |i| i.key());
        if self.backdrop.as_ref().is_none_or(|(k, _)| *k != key) {
            let (mut c, generated) = match item.as_ref().and_then(|i| i.art).and_then(hero) {
                Some(a) => (cover(&a, w, h), false),
                // No artwork: the wallpaper, tinted towards the item's colour.
                None => {
                    let mut c = self.background.clone();
                    if let Some(i) = &item {
                        c.blend_rect(c.bounds(), darken(i.color, 100), 60);
                    }
                    (c, true)
                }
            };
            c.blend_rect(c.bounds(), BG, if generated { 120 } else { 70 });
            for x in 0..w * 3 / 5 {
                let t = 190 - (x * 190 / (w * 3 / 5)) as u32;
                c.blend_rect(Rect::new(x, 0, 1, h), BG, t);
            }
            scrim(&mut c, TOP_BAR + HERO_H / 2, 250);
            // The shelves sit on the lower half: a second, steeper fade there keeps the
            // cards and row titles legible on bright art while the hero stays vivid above.
            scrim(&mut c, TOP_BAR + HERO_H, 150);
            self.backdrop = Some((key, c));
        }
        if let Some((_, c)) = &self.backdrop {
            scene.blit(c, c.bounds(), 0, 0);
        }
        self.draw_header(scene);

        let Some(item) = item else {
            // Nothing matches the search.
            let msg = format!("Nothing matches \u{201c}{}\u{201d}", self.query);
            let tw = font::width(&msg, 20, Weight::Bold);
            font::draw(scene, (w - tw) / 2, h / 2 - 40, &msg, 20, Weight::Bold, TEXT);
            let hint = "Keep typing, or press Escape to go back.";
            let hw = font::width(hint, 15, Weight::Regular);
            font::draw(scene, (w - hw) / 2, h / 2 - 6, hint, 15, Weight::Regular, DIM);
            return;
        };

        // Hero text.
        let tx = MARGIN;
        let chip = item.category.to_uppercase();
        let chip_w = font::width(&chip, 11, Weight::Bold) + 20;
        scene.fill_round_rect(Rect::new(tx, TOP_BAR + 16, chip_w, 22), 11, ACCENT);
        font::draw(scene, tx + 10, TOP_BAR + 16 + (22 - font::line_height(11)) / 2, &chip, 11, Weight::Bold, 0xFFFFFF);
        let tmax = w * 11 / 20;
        let tsize: u16 = if font::width(&item.title, 44, Weight::Bold) <= tmax { 44 } else { 32 };
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
        let open_hot = self.hot == Hot::Open;
        scene.shadow_round(open, open.h / 2, 10, if open_hot { 90 } else { 60 });
        scene.fill_round_rect(open, open.h / 2, if open_hot { 0xFFFFFF } else { 0xF2F2F7 });
        ui::symbol(scene, "play", 0x111111, Rect::new(open.x + 24, open.y + 13, 18, 18));
        font::draw(scene, open.x + 50, open.y + (open.h - font::line_height(17)) / 2, "Open", 17, Weight::Bold, 0x111111);

        // Live preview panel on the right of the hero.
        let pw = (w * 3 / 10).min(384);
        let panel = Rect::new(w - MARGIN - pw, TOP_BAR + 16, pw, pw * 9 / 16);
        if matches!(item.kind, Kind::Window(_) | Kind::App(_)) {
            scene.shadow_round(panel, 12, 20, 110);
            scene.fill_round_rect(Rect::new(panel.x - 2, panel.y - 2, panel.w + 4, panel.h + 4), 12, 0x2A2C3E);
            if !Self::draw_live(scene, &item, panel, 10) {
                // Until the first preview frame: the wide hero art (cropped to the panel),
                // or the generated poster for items without art.
                match item.art.and_then(hero) {
                    Some(a) => scene.blit_scaled(&cover(&a, panel.w, panel.h), panel, 10, 255),
                    None => {
                        let i = self.poster(&item, true);
                        scene.blit_scaled(&self.posters[i].2, panel, 10, 255);
                    }
                }
            }
            // A quiet "live" pill: a green dot, not a red banner.
            let badge = if matches!(item.kind, Kind::Window(_)) { "Live" } else { "Preview" };
            let bw = font::width(badge, 12, Weight::Bold) + 30;
            let br = Rect::new(panel.x + 10, panel.y + 10, bw, 22);
            scene.blend_round_rect(br, 11, 0x000000, 150);
            scene.fill_circle(br.x + 11, br.y + 11, 4, ui::SUCCESS);
            font::draw(scene, br.x + 20, br.y + (22 - font::line_height(12)) / 2, badge, 12, Weight::Bold, 0xFFFFFF);
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
                let end = font::draw(scene, MARGIN, ry, title, 20, Weight::Bold, TEXT);
                let count: usize = if self.tab == 1 && title == "All apps" { self.rows.iter().filter(|r| r.title != "System").map(|r| r.items.len()).sum() } else { self.rows[r].items.len() };
                // The count sits on the title's baseline.
                font::draw(scene, end + 10, ry + 5, &format!("{count}"), 15, Weight::Regular, DIM);
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
                    // Keyboard selection without zoom: a 3 px focus ring.
                    scene.stroke_round_rect_w(Rect::new(rect.x - 5, rect.y - 5, rect.w + 10, rect.h + 10), 16, 3, 0xFFFFFF);
                }
            }
            let vis = self.visible_cards();
            let row = &self.rows[r];
            if let Some((left, right)) = self.chevrons(r) {
                for (b, glyph, show, hot) in [(left, "chevron-left", row.scroll > 0, self.hot == Hot::Left(r)), (right, "chevron-right", row.scroll + vis < row.items.len(), self.hot == Hot::Right(r))] {
                    if !show {
                        continue;
                    }
                    let (cx, cy) = (b.x + b.w / 2, b.y + b.h / 2);
                    scene.fill_circle(cx, cy, b.w / 2, if hot { blend(BG, 0xFFFFFF, 70) } else { blend(BG, 0xFFFFFF, 36) });
                    ui::symbol(scene, glyph, TEXT, Rect::new(cx - 9, cy - 9, 18, 18));
                }
            }
        }

        // The selected card, enlarged on top, with its live preview once it has been
        // selected for a moment.
        if let Some((r, c, rect)) = zoom_card {
            let it = self.rows[r].items[c].clone();
            let z = Rect::new(rect.x - (ZOOM_W - CARD_W) / 2, rect.y - (ZOOM_H - CARD_H) / 2, ZOOM_W, ZOOM_H);
            let z = Rect::new(z.x.max(4), z.y, z.w, z.h);
            // The focused card lifts: a deeper shadow and a white border.
            scene.shadow_round(z, 16, 24, 140);
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
        let hints: &[(&str, &str)] = if self.query.is_empty() {
            &[("Enter", "Open"), ("Tab", "Section"), ("Arrows", "Move"), ("A-Z", "Search"), ("Windows key", "Home"), ("F1", "Assistant"), ("Right-click", "Options")]
        } else {
            &[("Enter", "Open"), ("Arrows", "Move"), ("Esc", "Clear search")]
        };
        for (key, label) in hints {
            hx = ui::kbd(scene, hx, hy, key) + 8;
            hx = font::draw(scene, hx, hy + 3, label, 13, Weight::Regular, DIM) + 22;
        }

        // Context menu: a raised popover with the card's name and its actions.
        if let Some(menu) = &self.menu {
            let r = menu.rect();
            scene.shadow_round(r, 12, 16, 130);
            scene.fill_round_rect(r, 12, ui::RAISED);
            scene.stroke_round_rect_w(r, 12, 1, blend(ui::RAISED, 0xFFFFFF, 22));
            font::draw_clipped(scene, r.x + 14, r.y + 10, &menu.title, 13, Weight::Bold, DIM, r.w - 28);
            for (i, (label, _)) in menu.entries.iter().enumerate() {
                let e = Rect::new(r.x + 6, r.y + 36 + i as i32 * MENU_ROW, r.w - 12, MENU_ROW - 2);
                if menu.entry_at(self.cursor.0, self.cursor.1) == Some(i) {
                    scene.fill_round_rect(e, 6, blend(ui::RAISED, 0xFFFFFF, 30));
                }
                let color = if *label == "Stop running" { ui::DANGER } else { TEXT };
                font::draw(scene, e.x + 10, e.y + (MENU_ROW - 2 - font::line_height(15)) / 2, label, 15, Weight::Regular, color);
            }
        }
    }
}
