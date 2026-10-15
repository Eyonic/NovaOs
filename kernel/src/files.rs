//! Files: Nova's native file manager (a clean-room design in the spirit of Finder and
//! Dolphin).
//!
//! NovaFS paths are flat strings; folders are the prefixes before a `/`. A sidebar of
//! places, back / forward / up, clickable breadcrumbs, live search, a grid (with image
//! thumbnails and app icons) or list view, and a preview pane with Open, Duplicate,
//! Copy path and Delete (asks first). Double-click or Enter opens: apps run, everything
//! else opens in the Text Editor. Keys: arrows, Enter, Backspace (up), Delete,
//! Ctrl-F search, Ctrl-L list/grid, Ctrl-N new note.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::font::{self, Weight};
use crate::gfx::{Canvas, Rect};
use crate::input::Key;
use crate::ui::{self, State};
use crate::wm::{self, Content, Event, WindowRef};
use crate::{apps, fs, icons, pit, rtc, task};

const SIDEBAR_W: i32 = 208;
const TOP_H: i32 = 56;
const STATUS_H: i32 = 28;
const PREVIEW_W: i32 = 264;
const TILE_W: i32 = 120;
const TILE_H: i32 = 124;
const ROW_H: i32 = 36;

/// Open a Files window (at `folder`, "" = everything).
pub fn open(folder: &str) {
    let (sw, sh) = wm::work_area();
    let (w, h) = ((sw - 200).min(1000), (sh - 140).min(620));
    let win = wm::open("Files", '\u{2261}', 0x3D7DD8, w, h, Content::Canvas(Canvas::new(w as usize, h as usize)));
    let folder = String::from(folder);
    task::spawn("files", move || {
        let mut f = Files::new(win.clone(), folder);
        f.run();
        wm::close(&win);
    });
}

#[derive(Clone)]
struct Entry {
    /// Full path (folders end without '/').
    path: String,
    name: String,
    folder: bool,
    size: u64,
    mtime: u64,
    /// Number of files inside (folders).
    count: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Folder,
    App,
    Image,
    Text,
    Code,
    Other,
}

fn kind_of(e: &Entry) -> Kind {
    if e.folder {
        return Kind::Folder;
    }
    match e.name.rsplit('.').next().unwrap_or("") {
        "rhai" => Kind::App,
        "png" => Kind::Image,
        "txt" | "md" | "log" | "conf" | "csv" => Kind::Text,
        "rs" | "c" | "h" | "cpp" | "py" | "sh" | "js" | "json" | "toml" => Kind::Code,
        _ if !e.name.contains('.') => Kind::Text,
        _ => Kind::Other,
    }
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Folder => "Folder",
        Kind::App => "Nova app",
        Kind::Image => "PNG image",
        Kind::Text => "Text document",
        Kind::Code => "Source code",
        Kind::Other => "File",
    }
}

fn fmt_size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{}.{} GB", n >> 30, ((n & ((1 << 30) - 1)) * 10) >> 30),
        n if n >= 1 << 20 => format!("{}.{} MB", n >> 20, ((n & ((1 << 20) - 1)) * 10) >> 20),
        n if n >= 1 << 10 => format!("{} KB", n >> 10),
        n => format!("{n} bytes"),
    }
}

fn items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} items") }
}

fn fmt_time(t: u64) -> String {
    if t == 0 {
        return "built in".into();
    }
    let now = rtc::unix_time();
    let ago = now.saturating_sub(t);
    match ago {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", ago / 60),
        3600..86400 => format!("{} h ago", ago / 3600),
        _ => format!("{} days ago", ago / 86400),
    }
}

/// Paths hidden from the file manager: the read-only system libraries and engine.
fn hidden(path: &str) -> bool {
    ["usr/", "lib/", "lib64/", "opt/", "etc/", "bin/", "var/", "tmp/", "appdata/"].iter().any(|h| path.starts_with(h)) || path.split('/').any(|p| p.starts_with('.'))
}

struct Files {
    win: WindowRef,
    folder: String,
    back: Vec<String>,
    forward: Vec<String>,
    entries: Vec<Entry>,
    selected: Option<usize>,
    grid: bool,
    search: String,
    searching: bool,
    scroll: i32,
    confirm_delete: bool,
    message: String,
    last_click: (u64, Option<usize>),
    thumbs: BTreeMap<String, Option<Arc<Canvas>>>,
    fs_gen: u64,
    /// Pointer position inside the window (None when it is elsewhere), button held.
    cursor: Option<(i32, i32)>,
    held: bool,
}

impl Files {
    fn new(win: WindowRef, folder: String) -> Self {
        let mut f = Self { win, folder, back: Vec::new(), forward: Vec::new(), entries: Vec::new(), selected: None, grid: true, search: String::new(), searching: false, scroll: 0, confirm_delete: false, message: String::new(), last_click: (0, None), thumbs: BTreeMap::new(), fs_gen: 0, cursor: None, held: false };
        f.reload();
        f
    }

    /// List the current folder (or search results across everything).
    fn reload(&mut self) {
        self.fs_gen = fs::generation();
        let prefix = if self.folder.is_empty() { String::new() } else { format!("{}/", self.folder) };
        let mut folders: BTreeMap<String, (usize, u64, u64)> = BTreeMap::new();
        let mut files = Vec::new();
        let q = self.search.to_lowercase();
        for (path, size, mtime) in fs::list(&prefix) {
            if hidden(&path) {
                continue;
            }
            if !q.is_empty() {
                // Search: matching files anywhere below this folder.
                let name = path.rsplit('/').next().unwrap_or(&path);
                if name.to_lowercase().contains(&q) {
                    files.push(Entry { name: name.into(), path: path.clone(), folder: false, size, mtime, count: 0 });
                }
                continue;
            }
            let rest = &path[prefix.len()..];
            match rest.split_once('/') {
                Some((dir, _)) => {
                    let e = folders.entry(format!("{prefix}{dir}")).or_insert((0, 0, 0));
                    e.0 += 1;
                    e.1 += size;
                    e.2 = e.2.max(mtime);
                }
                None => files.push(Entry { name: rest.into(), path: path.clone(), folder: false, size, mtime, count: 0 }),
            }
        }
        self.entries = folders
            .into_iter()
            .map(|(path, (count, size, mtime))| Entry { name: path.rsplit('/').next().unwrap_or(&path).into(), path, folder: true, size, mtime, count })
            .chain(files)
            .collect();
        self.selected = self.selected.filter(|&i| i < self.entries.len());
    }

    fn go(&mut self, folder: &str) {
        if folder != self.folder {
            self.back.push(core::mem::replace(&mut self.folder, folder.into()));
            self.forward.clear();
        }
        self.search.clear();
        self.searching = false;
        self.selected = None;
        self.scroll = 0;
        self.confirm_delete = false;
        self.reload();
    }

    fn up(&mut self) {
        if !self.folder.is_empty() {
            let parent = self.folder.rsplit_once('/').map_or(String::new(), |(p, _)| p.into());
            self.go(&parent);
        }
    }

    fn open_entry(&mut self, i: usize) {
        let Some(e) = self.entries.get(i).cloned() else { return };
        if e.folder {
            self.go(&e.path);
        } else if let Err(err) = apps::launch(&e.path) {
            self.message = format!("Could not open {}: {err}", e.name);
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.selected.and_then(|i| self.entries.get(i))
    }

    fn thumb(&mut self, path: &str) -> Option<Arc<Canvas>> {
        if !self.thumbs.contains_key(path) {
            let img = fs::read(path).ok().and_then(|d| Canvas::from_png(&d).ok()).map(Arc::new);
            self.thumbs.insert(path.into(), img);
        }
        self.thumbs.get(path).cloned().flatten()
    }

    // ----- layout -----

    fn size(&self) -> (i32, i32) {
        wm::size(&self.win)
    }

    fn content(&self) -> Rect {
        let (w, h) = self.size();
        Rect::new(SIDEBAR_W, TOP_H, w - SIDEBAR_W - PREVIEW_W, h - TOP_H - STATUS_H)
    }

    fn columns(&self) -> usize {
        ((self.content().w - 24) / TILE_W).max(1) as usize
    }

    fn item_rect(&self, i: usize) -> Rect {
        let c = self.content();
        if self.grid {
            let cols = self.columns();
            Rect::new(c.x + 16 + (i % cols) as i32 * TILE_W, c.y + 12 + (i / cols) as i32 * TILE_H - self.scroll, TILE_W - 8, TILE_H - 8)
        } else {
            Rect::new(c.x + 8, c.y + 40 + i as i32 * ROW_H - self.scroll, c.w - 16, ROW_H - 2)
        }
    }

    /// (label, folder, symbolic glyph)
    fn places() -> [(&'static str, &'static str, &'static str); 5] {
        [("Everything", "", "home"), ("Apps", "apps", "app"), ("Notes", "notes", "document"), ("Pictures", "apps/assets", "image"), ("System", "system", "gear")]
    }

    fn place_rect(i: usize) -> Rect {
        Rect::new(ui::GAP, 48 + i as i32 * (ui::NAV_ROW_H + 2), SIDEBAR_W - 2 * ui::GAP, ui::NAV_ROW_H)
    }

    fn new_note_rect(&self) -> Rect {
        let (_, h) = self.size();
        Rect::new(ui::PAD, h - STATUS_H - 104, SIDEBAR_W - 2 * ui::PAD, ui::CONTROL_H + 2)
    }

    fn toolbar_buttons(&self) -> [Rect; 3] {
        let x = SIDEBAR_W + 12;
        let y = (TOP_H - ui::CONTROL_H) / 2;
        [Rect::new(x, y, ui::CONTROL_H, ui::CONTROL_H), Rect::new(x + 40, y, ui::CONTROL_H, ui::CONTROL_H), Rect::new(x + 80, y, ui::CONTROL_H, ui::CONTROL_H)]
    }

    fn search_rect(&self) -> Rect {
        let (w, _) = self.size();
        Rect::new(w - 296, (TOP_H - ui::CONTROL_H) / 2, 232, ui::CONTROL_H)
    }

    fn view_toggle(&self) -> Rect {
        let (w, _) = self.size();
        Rect::new(w - 52, (TOP_H - ui::CONTROL_H) / 2, ui::CONTROL_H + 4, ui::CONTROL_H)
    }

    fn preview_buttons(&self) -> [Rect; 4] {
        let (w, h) = self.size();
        let x = w - PREVIEW_W + ui::PAD;
        let bw = PREVIEW_W - 2 * ui::PAD;
        let y = h - STATUS_H - 4 * 40 - 8;
        [Rect::new(x, y, bw, ui::CONTROL_H), Rect::new(x, y + 40, bw, ui::CONTROL_H), Rect::new(x, y + 80, bw, ui::CONTROL_H), Rect::new(x, y + 120, bw, ui::CONTROL_H)]
    }

    fn state(&self, r: Rect) -> State {
        ui::state(self.cursor, self.held, r)
    }

    // ----- input -----

    fn key(&mut self, k: Key) {
        self.message.clear();
        if self.searching {
            match k {
                Key::Char(0x1B) => {
                    self.searching = false;
                    self.search.clear();
                }
                Key::Char(0x08 | 0x7F) => {
                    self.search.pop();
                }
                Key::Char(b'\n' | b'\r') => self.searching = false,
                Key::Char(c) if (0x20..0x7F).contains(&c) => self.search.push(c as char),
                _ => {}
            }
            self.selected = None;
            self.reload();
            return;
        }
        let n = self.entries.len();
        let step = if self.grid { self.columns() } else { 1 };
        match k {
            Key::Char(0x06) => self.searching = true,      // Ctrl-F
            Key::Char(0x0C) => self.grid = !self.grid,     // Ctrl-L
            Key::Char(0x0E) => self.new_note(),            // Ctrl-N
            Key::Char(b'\n' | b'\r') => {
                if let Some(i) = self.selected {
                    self.open_entry(i);
                }
            }
            Key::Char(0x08 | 0x7F) => self.up(),
            Key::Delete => self.confirm_delete = self.selected_entry().is_some_and(|e| !e.folder),
            Key::Char(0x1B) => {
                self.confirm_delete = false;
                self.selected = None;
            }
            Key::Left | Key::Right | Key::Up | Key::Down if n > 0 => {
                let i = self.selected.unwrap_or(0);
                let j = match k {
                    Key::Left => i.saturating_sub(1),
                    Key::Right => (i + 1).min(n - 1),
                    Key::Up => i.saturating_sub(step),
                    _ => (i + step).min(n - 1),
                };
                self.selected = Some(if self.selected.is_none() { 0 } else { j });
                self.confirm_delete = false;
                // Keep it in view.
                let r = self.item_rect(self.selected.unwrap_or(0));
                let c = self.content();
                if r.y < c.y + 8 {
                    self.scroll -= c.y + 8 - r.y;
                } else if r.y + r.h > c.y + c.h - 8 {
                    self.scroll += r.y + r.h - (c.y + c.h - 8);
                }
            }
            _ => {}
        }
    }

    fn new_note(&mut self) {
        let mut n = 1;
        while fs::exists(&format!("notes/note-{n}.txt")) {
            n += 1;
        }
        let path = format!("notes/note-{n}.txt");
        let _ = fs::write(&path, b"");
        crate::editor::open(Some(path));
    }

    fn click(&mut self, x: i32, y: i32) {
        self.message.clear();
        let (w, h) = self.size();
        // Sidebar places.
        if x < SIDEBAR_W {
            for (i, (_, folder, _)) in Self::places().iter().enumerate() {
                if Self::place_rect(i).contains(x, y) {
                    self.go(folder);
                    return;
                }
            }
            if self.new_note_rect().contains(x, y) {
                self.new_note();
            }
            let _ = h;
            return;
        }
        // Toolbar.
        if y < TOP_H {
            let [b, f, u] = self.toolbar_buttons();
            if b.contains(x, y) {
                if let Some(p) = self.back.pop() {
                    self.forward.push(core::mem::replace(&mut self.folder, p));
                    self.selected = None;
                    self.reload();
                }
            } else if f.contains(x, y) {
                if let Some(p) = self.forward.pop() {
                    self.back.push(core::mem::replace(&mut self.folder, p));
                    self.selected = None;
                    self.reload();
                }
            } else if u.contains(x, y) {
                self.up();
            } else if self.search_rect().contains(x, y) {
                self.searching = true;
            } else if self.view_toggle().contains(x, y) {
                self.grid = !self.grid;
            } else {
                // Breadcrumbs.
                for (path, r) in self.crumbs() {
                    if r.contains(x, y) {
                        self.go(&path);
                        return;
                    }
                }
            }
            return;
        }
        self.searching = false;
        // Preview pane buttons.
        if x >= w - PREVIEW_W {
            let Some(e) = self.selected_entry().cloned() else { return };
            let [open, dup, copy, del] = self.preview_buttons();
            if open.contains(x, y) {
                if let Some(i) = self.selected {
                    self.open_entry(i);
                }
            } else if dup.contains(x, y) && !e.folder {
                let (stem, ext) = e.path.rsplit_once('.').map_or((e.path.as_str(), ""), |(s, x)| (s, x));
                let copy_path = if ext.is_empty() { format!("{stem} copy") } else { format!("{stem} copy.{ext}") };
                match fs::read(&e.path).and_then(|d| fs::write(&copy_path, &d)) {
                    Ok(()) => self.message = format!("Duplicated as {copy_path}"),
                    Err(err) => self.message = format!("Could not duplicate: {err}"),
                }
                self.reload();
            } else if copy.contains(x, y) {
                ui::copy(&e.path);
                self.message = "Path copied".into();
            } else if del.contains(x, y) && !e.folder {
                if self.confirm_delete {
                    match fs::delete(&e.path) {
                        Ok(()) => self.message = format!("Deleted {}", e.name),
                        Err(err) => self.message = format!("Could not delete: {err}"),
                    }
                    self.confirm_delete = false;
                    self.selected = None;
                    self.reload();
                } else {
                    self.confirm_delete = true;
                }
            }
            return;
        }
        // Items.
        let hit = (0..self.entries.len()).find(|&i| self.item_rect(i).contains(x, y));
        let now = pit::ticks();
        if let Some(i) = hit {
            if self.last_click.1 == Some(i) && now - self.last_click.0 < pit::HZ / 2 {
                self.open_entry(i);
                self.last_click = (0, None);
                return;
            }
            self.selected = Some(i);
        } else {
            self.selected = None;
        }
        self.confirm_delete = false;
        self.last_click = (now, hit);
    }

    /// Breadcrumb segments: (folder path, clickable rect).
    fn crumbs(&self) -> Vec<(String, Rect)> {
        let mut out = Vec::new();
        let mut x = self.toolbar_buttons()[2].x + 52;
        let mut add = |label: &str, path: String, x: &mut i32| {
            let tw = font::width(label, 15, Weight::Bold);
            out.push((path, Rect::new(*x - 6, 14, tw + 12, 28)));
            *x += tw + 26;
        };
        add("Nova", String::new(), &mut x);
        let mut acc = String::new();
        for part in self.folder.split('/').filter(|p| !p.is_empty()) {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(part);
            add(part, acc.clone(), &mut x);
        }
        out
    }

    fn run(&mut self) {
        self.render();
        loop {
            let e = wm::wait_event(&self.win, Some(pit::HZ));
            match e {
                Some(Event::Close) => return,
                Some(Event::Key(k)) => self.key(k),
                Some(Event::MouseDown { x, y, .. }) => {
                    self.held = true;
                    self.cursor = Some((x, y));
                    self.click(x, y);
                }
                Some(Event::MouseUp { x, y }) => {
                    self.held = false;
                    self.cursor = Some((x, y));
                }
                Some(Event::MouseMove { x, y }) => self.cursor = (x >= 0 && y >= 0).then_some((x, y)),
                Some(Event::Wheel(n)) => self.scroll = (self.scroll - n * 40).max(0),
                Some(_) => {}
                None => {
                    // Refresh when files change elsewhere.
                    if fs::generation() == self.fs_gen {
                        continue;
                    }
                    self.reload();
                }
            }
            self.render();
        }
    }

    // ----- drawing -----

    fn draw_icon(&mut self, c: &mut Canvas, e: &Entry, r: Rect) {
        match kind_of(e) {
            Kind::Folder => icons::draw(c, '\u{2261}', 0x3D7DD8, r),
            Kind::App => {
                let info = fs::read_string(&e.path).map(|src| apps::info(&e.path, &src)).ok();
                let (icon, color) = info.map_or(('\u{2666}', 0x5B6EE1), |i| (i.icon, i.color));
                icons::draw(c, icon, color, r);
            }
            Kind::Image => match self.thumb(&e.path) {
                Some(img) => {
                    // Fit the image inside the square, keeping its aspect ratio.
                    let (iw, ih) = (img.width as i32, img.height as i32);
                    let (tw, th) = if iw >= ih { (r.w, (r.w * ih / iw.max(1)).max(1)) } else { ((r.h * iw / ih.max(1)).max(1), r.h) };
                    c.blit_scaled(&img, Rect::new(r.x + (r.w - tw) / 2, r.y + (r.h - th) / 2, tw, th), 6, 255);
                }
                None => icons::draw(c, '@', 0xD07A3A, r),
            },
            k => {
                // A document tile: page with a folded corner and the extension.
                let color = match k {
                    Kind::Code => 0x5FD3D0,
                    Kind::Text => 0xD5D7E5,
                    _ => 0x8E90A6,
                };
                let page = Rect::new(r.x + r.w / 6, r.y + 2, r.w * 2 / 3, r.h - 4);
                c.fill_round_rect(page, 6, 0x2A2C3E);
                c.fill_rect(Rect::new(page.x, page.y, page.w, 4), color);
                for i in 0..4 {
                    c.fill_rect(Rect::new(page.x + 8, page.y + 14 + i * 7, page.w - 16 - (i % 2) * 10, 2), 0x45475A);
                }
                let ext = e.name.rsplit_once('.').map_or("", |(_, x)| x).to_uppercase();
                if !ext.is_empty() && r.w >= 40 {
                    let tw = font::width(&ext, 11, Weight::Bold);
                    let badge = Rect::new(page.x + (page.w - tw - 10) / 2, page.y + page.h - 22, tw + 10, 18);
                    c.fill_round_rect(badge, 5, color);
                    font::draw(c, badge.x + 5, badge.y + 1, &ext, 11, Weight::Bold, 0x14151F);
                }
            }
        }
    }

    fn render(&mut self) {
        let (w, h) = self.size();
        let mut c = Canvas::new(w as usize, h as usize);
        c.clear(ui::BG);

        // Sidebar: places with symbolic glyphs, the one action, the storage gauge.
        c.fill_rect(Rect::new(0, 0, SIDEBAR_W, h), ui::SIDEBAR);
        ui::divider(&mut c, Rect::new(SIDEBAR_W - 1, 0, 1, h));
        ui::section_label(&mut c, ui::PAD + 4, 18, "Places");
        for (i, (label, folder, glyph)) in Self::places().iter().enumerate() {
            let r = Self::place_rect(i);
            let active = self.folder == *folder && self.search.is_empty();
            ui::nav_row(&mut c, r, Some((glyph, None)), label, active, self.state(r));
        }
        let nn = self.new_note_rect();
        ui::button_kind(&mut c, nn, "New note", ui::Kind::Primary, self.state(nn));
        ui::symbol(&mut c, "plus", 0xFFFFFF, Rect::new(nn.x + 14, nn.y + (nn.h - 14) / 2, 14, 14));
        // Storage gauge.
        if let Some((used, total)) = fs::usage() {
            let y = h - STATUS_H - 52;
            font::draw(&mut c, ui::PAD, y, "Storage", ui::CAPTION, Weight::Bold, ui::DIM);
            let bar = Rect::new(ui::PAD, y + 22, SIDEBAR_W - 2 * ui::PAD, 6);
            c.fill_round_rect(bar, 3, ui::BORDER);
            let fill = (bar.w as u64 * used / total.max(1)) as i32;
            c.fill_round_rect(Rect::new(bar.x, bar.y, fill.max(6), bar.h), 3, ui::ACCENT);
            font::draw(&mut c, ui::PAD, y + 32, &format!("{} of {}", fmt_size(used), fmt_size(total)), ui::CAPTION, Weight::Regular, ui::DIM);
        }

        // Toolbar: back / forward / up, breadcrumbs, search, view toggle.
        c.fill_rect(Rect::new(SIDEBAR_W, 0, w - SIDEBAR_W, TOP_H), ui::SIDEBAR);
        ui::divider(&mut c, Rect::new(SIDEBAR_W, TOP_H - 1, w - SIDEBAR_W, 1));
        let enabled = [!self.back.is_empty(), !self.forward.is_empty(), !self.folder.is_empty()];
        for (i, (b, glyph)) in self.toolbar_buttons().iter().zip(["back", "forward", "up"]).enumerate() {
            let state = if enabled[i] { self.state(*b) } else { State::Disabled };
            ui::icon_button(&mut c, *b, glyph, state, true);
        }
        let crumbs = self.crumbs();
        let last = crumbs.len() - 1;
        for (i, (path, r)) in crumbs.iter().enumerate() {
            let label = if path.is_empty() { "Nova" } else { path.rsplit('/').next().unwrap_or(path) };
            let hot = i != last && self.state(*r) != State::Normal;
            if hot {
                c.fill_round_rect(*r, 6, ui::HOVER);
            }
            font::draw(&mut c, r.x + 6, r.y + 4, label, ui::HEADING, Weight::Bold, if i == last || hot { ui::TEXT } else { ui::DIM });
            if i < last {
                ui::symbol(&mut c, "chevron-right", ui::FAINT, Rect::new(r.x + r.w + 1, r.y + 8, 12, 12));
            }
        }
        ui::field_icon(&mut c, self.search_rect(), &self.search, "Search", self.searching, Some("search"));
        let vt = self.view_toggle();
        ui::icon_button(&mut c, vt, if self.grid { "grid" } else { "list" }, self.state(vt), false);

        // Items.
        let area = self.content();
        let entries = self.entries.clone();
        if entries.is_empty() {
            if self.search.is_empty() {
                ui::empty_state(&mut c, area, "folder", "This folder is empty", "Files you create here will show up in this view.");
            } else {
                ui::empty_state(&mut c, area, "search", "No matches", &format!("Nothing here is called \u{201c}{}\u{201d}.", self.search));
            }
        }
        if !self.grid {
            // Column headers.
            let y = area.y + 10;
            for (label, x) in [("Name", area.x + 56), ("Kind", area.x + area.w - 330), ("Size", area.x + area.w - 200), ("Modified", area.x + area.w - 110)] {
                font::draw(&mut c, x, y, label, ui::CAPTION, Weight::Bold, ui::DIM);
            }
        }
        for (i, e) in entries.iter().enumerate() {
            let r = self.item_rect(i);
            if r.y + r.h < area.y || r.y > area.y + area.h {
                continue;
            }
            let sel = self.selected == Some(i);
            let hot = self.state(r) != State::Normal;
            if self.grid {
                if sel {
                    c.fill_round_rect(r, ui::RADIUS, ui::SELECTION);
                } else if hot {
                    c.fill_round_rect(r, ui::RADIUS, ui::HOVER);
                }
                self.draw_icon(&mut c, e, Rect::new(r.x + (r.w - 64) / 2, r.y + 8, 64, 64));
                let lines = font::wrap(&e.name, 13, Weight::Regular, r.w - 10, 2);
                for (li, line) in lines.iter().enumerate() {
                    let tw = font::width(line, 13, Weight::Regular);
                    font::draw(&mut c, r.x + (r.w - tw) / 2, r.y + 80 + li as i32 * 17, line, 13, if sel { Weight::Bold } else { Weight::Regular }, ui::TEXT);
                }
            } else {
                if sel {
                    c.fill_round_rect(r, ui::RADIUS_S, ui::SELECTION);
                } else if hot {
                    c.fill_round_rect(r, ui::RADIUS_S, ui::HOVER);
                } else if i % 2 == 1 {
                    c.fill_round_rect(r, ui::RADIUS_S, 0x191A24);
                }
                let ty = r.y + (r.h - font::line_height(ui::BODY)) / 2;
                self.draw_icon(&mut c, e, Rect::new(r.x + 10, r.y + (r.h - 24) / 2, 24, 24));
                font::draw_clipped(&mut c, r.x + 46, ty, &e.name, ui::BODY, Weight::Regular, ui::TEXT, r.w - 400);
                let kind = kind_name(kind_of(e));
                font::draw(&mut c, area.x + area.w - 330, ty + 1, kind, 13, Weight::Regular, ui::DIM);
                let size = if e.folder { items(e.count) } else { fmt_size(e.size) };
                font::draw(&mut c, area.x + area.w - 200, ty + 1, &size, 13, Weight::Regular, ui::DIM);
                font::draw(&mut c, area.x + area.w - 110, ty + 1, &fmt_time(e.mtime), 13, Weight::Regular, ui::DIM);
            }
        }
        // Scrollbar for long listings.
        let content_h = if self.grid { (entries.len().div_ceil(self.columns())) as i32 * TILE_H + 24 } else { entries.len() as i32 * ROW_H + 48 };
        ui::scrollbar(&mut c, Rect::new(area.x + area.w - 10, area.y + 4, 6, area.h - 8), content_h, area.h, self.scroll);

        // Preview pane.
        let px = w - PREVIEW_W;
        c.fill_rect(Rect::new(px, TOP_H, PREVIEW_W, h - TOP_H - STATUS_H), ui::SIDEBAR);
        ui::divider(&mut c, Rect::new(px, TOP_H, 1, h - TOP_H - STATUS_H));
        match self.selected_entry().cloned() {
            None => {
                let msg = items(self.entries.len());
                font::draw(&mut c, px + ui::PAD + 4, TOP_H + 24, &msg, ui::HEADING, Weight::Bold, ui::TEXT);
                font::draw(&mut c, px + ui::PAD + 4, TOP_H + 50, "Select a file to preview it.", 13, Weight::Regular, ui::DIM);
                let mut y = TOP_H + 92;
                for (key, what) in [("Enter", "Open"), ("Ctrl-F", "Search"), ("Ctrl-L", "List or grid"), ("Ctrl-N", "New note"), ("Delete", "Delete")] {
                    let end = ui::kbd(&mut c, px + ui::PAD + 4, y, key);
                    font::draw(&mut c, end + 10, y + 3, what, 13, Weight::Regular, ui::DIM);
                    y += 30;
                }
            }
            Some(e) => {
                let kind = kind_of(&e);
                let big = Rect::new(px + ui::PAD + 4, TOP_H + ui::PAD + 4, PREVIEW_W - 2 * ui::PAD - 8, PREVIEW_W - 2 * ui::PAD - 8);
                match kind {
                    Kind::Image => self.draw_icon(&mut c, &e, big),
                    Kind::Text | Kind::Code => {
                        c.fill_round_rect(big, ui::RADIUS, ui::BG);
                        c.stroke_round_rect_w(big, ui::RADIUS, 1, ui::BORDER);
                        let text = fs::read_string(&e.path).unwrap_or_default();
                        for (i, line) in text.lines().take(12).enumerate() {
                            font::draw_clipped(&mut c, big.x + 10, big.y + 10 + i as i32 * 17, line, 12, Weight::Mono, ui::DIM, big.w - 20);
                        }
                    }
                    _ => self.draw_icon(&mut c, &e, Rect::new(big.x + big.w / 4, big.y + big.h / 4, big.w / 2, big.h / 2)),
                }
                let mut y = big.y + big.h + 16;
                for line in font::wrap(&e.name, 16, Weight::Bold, big.w, 2) {
                    font::draw(&mut c, big.x, y, &line, 16, Weight::Bold, ui::TEXT);
                    y += 22;
                }
                let detail = if e.folder { format!("Folder  \u{00b7}  {}", items(e.count)) } else { format!("{}  \u{00b7}  {}", kind_name(kind), fmt_size(e.size)) };
                font::draw(&mut c, big.x, y + 2, &detail, 13, Weight::Regular, ui::DIM);
                font::draw(&mut c, big.x, y + 22, &format!("Modified {}", fmt_time(e.mtime)), 13, Weight::Regular, ui::DIM);
                let [open, dup, copy, del] = self.preview_buttons();
                ui::button_kind(&mut c, open, if kind == Kind::App { "Run app" } else if e.folder { "Open folder" } else { "Open" }, ui::Kind::Primary, self.state(open));
                if !e.folder {
                    ui::button_kind(&mut c, dup, "Duplicate", ui::Kind::Secondary, self.state(dup));
                    ui::button_kind(&mut c, copy, "Copy path", ui::Kind::Secondary, self.state(copy));
                    if self.confirm_delete {
                        ui::button_kind(&mut c, del, "Click again to delete", ui::Kind::Danger, self.state(del));
                    } else {
                        ui::button_kind(&mut c, del, "Delete", ui::Kind::Secondary, self.state(del));
                    }
                }
            }
        }

        // Status bar.
        let sb = Rect::new(SIDEBAR_W, h - STATUS_H, w - SIDEBAR_W, STATUS_H);
        c.fill_rect(sb, ui::SIDEBAR);
        ui::divider(&mut c, Rect::new(sb.x, sb.y, sb.w, 1));
        let left = if !self.message.is_empty() {
            self.message.clone()
        } else if !self.search.is_empty() {
            format!("{} results for \u{201c}{}\u{201d}", self.entries.len(), self.search)
        } else {
            let files = self.entries.iter().filter(|e| !e.folder).count();
            let folders = self.entries.len() - files;
            format!("{} folder{}, {} file{}", folders, if folders == 1 { "" } else { "s" }, files, if files == 1 { "" } else { "s" })
        };
        font::draw(&mut c, sb.x + 14, sb.y + 6, &left, 13, Weight::Regular, if self.message.is_empty() { ui::DIM } else { ui::TEXT });
        wm::present(&self.win, &c);
        let title = if self.folder.is_empty() { "Files".to_string() } else { format!("{} - Files", self.folder.rsplit('/').next().unwrap_or(&self.folder)) };
        wm::set_title(&self.win, &title);
    }
}
