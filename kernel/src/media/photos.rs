//! Photos: Nova's image viewer. PNG, JPEG (EXIF orientation applied), GIF (animated) and,
//! through the media engine, WebP and other formats. Fit or zoom (wheel, +/-/0, drag to
//! pan), previous/next in the folder (arrows, the filmstrip), rotate (R), fullscreen (F),
//! an info panel (I), set as wallpaper, and a library of the Pictures folder when opened
//! without a file.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::chrome::{self, PHOTOS_ACCENT as ACCENT};
use super::engine::Engine;
use super::image::{self, Image};
use super::library::{self, Entry};
use crate::font::{self, Weight};
use crate::gfx::{blend, Canvas, Rect};
use crate::input::Key;
use crate::ui::{self, State};
use crate::wm::{self, Content, Event, WindowRef};
use crate::{fs, pit, task};

pub const ICON: char = '\u{25A3}';

const BAR_H: i32 = 52;
const STRIP_H: i32 = 84;
const INFO_W: i32 = 260;
const HIDE_AFTER: u64 = 2500;
const ZOOMS: [i32; 10] = [10, 25, 50, 75, 100, 150, 200, 300, 400, 800];

pub fn open(path: Option<String>) {
    let (sw, sh) = wm::work_area();
    let (w, h) = ((sw - 200).min(1000), (sh - 120).min(660));
    let win = wm::open("Photos", ICON, ACCENT, w, h, Content::Canvas(Canvas::new(w as usize, h as usize)));
    task::spawn("photos", move || {
        let mut app = Photos::new(win.clone(), path);
        app.run();
        wm::close(&win);
    });
}

#[derive(Clone, PartialEq)]
enum Hit {
    Prev,
    Next,
    Select(usize),
    ZoomIn,
    ZoomOut,
    Fit,
    Rotate,
    Fullscreen,
    Info,
    Wallpaper,
    Strip,
    Library,
    Picture,
    Swallow,
}

struct Photos {
    win: WindowRef,
    entries: Vec<Entry>,
    index: Option<usize>,
    image: Option<Arc<Image>>,
    error: Option<String>,
    /// GIF animation: current frame and when it started.
    frame: usize,
    frame_at: u64,
    rotation: u32,
    /// None = fit to the window, Some(percent).
    zoom: Option<i32>,
    pan: (i32, i32),
    fullscreen: bool,
    strip: bool,
    info: bool,
    controls_until: u64,
    engine: Option<Engine>,
    engine_tried: bool,
    toast: Option<(String, u64)>,
    drag: Option<(i32, i32, i32, i32)>,
    cursor: Option<(i32, i32)>,
    held: bool,
    hits: Vec<(Rect, Hit)>,
    fs_gen: u64,
    thumbs: Vec<(String, Option<Arc<Canvas>>)>,
}

impl Photos {
    fn new(win: WindowRef, path: Option<String>) -> Self {
        let mut p = Photos {
            win,
            entries: Vec::new(),
            index: None,
            image: None,
            error: None,
            frame: 0,
            frame_at: 0,
            rotation: 0,
            zoom: None,
            pan: (0, 0),
            fullscreen: false,
            strip: true,
            info: false,
            controls_until: pit::ticks() + HIDE_AFTER,
            engine: None,
            engine_tried: false,
            toast: None,
            drag: None,
            cursor: None,
            held: false,
            hits: Vec::new(),
            fs_gen: 0,
            thumbs: Vec::new(),
        };
        p.scan(path.as_deref());
        if let Some(path) = path {
            let i = p.entries.iter().position(|e| e.path == path);
            match i {
                Some(i) => p.show(i),
                None => {
                    p.entries.insert(0, Entry { path: path.clone(), size: 0, mtime: 0 });
                    p.show(0);
                }
            }
        }
        p
    }

    /// The pictures of the opened file's folder, or the whole library.
    fn scan(&mut self, path: Option<&str>) {
        self.fs_gen = fs::generation();
        let all = library::pictures();
        self.entries = match path {
            Some(p) => {
                let dir = p.rsplit_once('/').map_or("", |(d, _)| d);
                let same: Vec<Entry> = all.iter().filter(|e| e.path.rsplit_once('/').map_or("", |(d, _)| d) == dir).cloned().collect();
                if same.is_empty() { all } else { same }
            }
            None => all,
        };
    }

    fn say(&mut self, text: &str) {
        self.toast = Some((text.into(), pit::ticks() + 3000));
    }

    fn show(&mut self, i: usize) {
        let Some(e) = self.entries.get(i).cloned() else { return };
        self.index = Some(i);
        self.rotation = 0;
        self.zoom = None;
        self.pan = (0, 0);
        self.frame = 0;
        self.frame_at = pit::ticks();
        self.error = None;
        self.image = None;
        self.controls_until = pit::ticks() + HIDE_AFTER;
        match library::read(&e.path) {
            None => self.error = Some("The file could not be read.".into()),
            Some(data) => match image::decode(&data) {
                Ok(img) => self.image = Some(Arc::new(img)),
                Err(why) if image::format_of(&data).is_some_and(|f| f == "webp") || image::format_of(&data).is_none() => {
                    // Formats this kernel does not decode: ask the engine.
                    if !self.engine_tried {
                        self.engine_tried = true;
                        self.engine = Engine::start().ok();
                    }
                    match self.engine.as_mut().and_then(|eng| eng.decode(&e.path)) {
                        Some(c) => self.image = Some(Arc::new(Image::still((*c).clone()))),
                        None => self.error = Some(if self.engine.is_none() { format!("{why}. Build the image with NOVA_MEDIA=1 for WebP and other formats.") } else { "The media engine could not decode this file.".into() }),
                    }
                }
                Err(why) => self.error = Some(why.into()),
            },
        }
        wm::set_title(&self.win, &format!("{} - Photos", e.path.rsplit('/').next().unwrap_or(&e.path)));
    }

    fn step(&mut self, dir: i32) {
        let n = self.entries.len() as i32;
        if n == 0 {
            return;
        }
        let i = self.index.map_or(0, |i| ((i as i32 + dir) % n + n) % n);
        self.show(i as usize);
    }

    /// The picture as shown now (current frame, rotated).
    fn picture(&self) -> Option<Canvas> {
        let img = self.image.as_ref()?;
        let f = &img.frames[self.frame.min(img.frames.len() - 1)].0;
        Some(if self.rotation == 0 { f.clone() } else { image::rotate(f, self.rotation) })
    }

    fn thumb(&mut self, path: &str) -> Option<Arc<Canvas>> {
        if let Some((_, t)) = self.thumbs.iter().find(|(p, _)| p == path) {
            return t.clone();
        }
        let t = library::Thumbs::get(path).or_else(|| {
            let data = library::read(path)?;
            if data.len() > 6 << 20 {
                return None; // too big to decode just for a strip
            }
            let img = image::decode(&data).ok()?;
            Some(library::Thumbs::put(path, img.first(), 160))
        });
        if self.thumbs.len() > 200 {
            self.thumbs.remove(0);
        }
        self.thumbs.push((path.into(), t.clone()));
        t
    }

    fn set_wallpaper(&mut self) {
        let Some(pic) = self.picture() else { return };
        let (sw, sh) = wm::work_area();
        // Cover the screen, cropping the overflow.
        let (iw, ih) = (pic.width as i32, pic.height as i32);
        let (cw, ch) = if iw as i64 * sh as i64 > ih as i64 * sw as i64 { (ih as i64 * sw as i64 / sh as i64, ih as i64) } else { (iw as i64, iw as i64 * sh as i64 / sw as i64) };
        let mut crop = Canvas::new(cw.max(1) as usize, ch.max(1) as usize);
        crop.blit(&pic, Rect::new(((iw as i64 - cw) / 2) as i32, ((ih as i64 - ch) / 2) as i32, cw as i32, ch as i32), 0, 0);
        let mut wall = Canvas::new(sw as usize, sh as usize);
        if cw >= sw as i64 {
            wall.blit_scaled(&crop, wall.bounds(), 0, 255);
        } else {
            wall = super::video::bilinear(&crop, sw, sh);
        }
        match fs::write("system/wallpaper.png", &wall.to_png()) {
            Ok(()) => {
                crate::home::wallpaper_changed();
                self.say("Set as the Home wallpaper");
            }
            Err(e) => self.say(&format!("Could not save the wallpaper: {e}")),
        }
    }

    fn zoom_step(&mut self, dir: i32) {
        let fit = self.fit_percent();
        let cur = self.zoom.unwrap_or(fit);
        let next = if dir > 0 { ZOOMS.iter().copied().find(|&z| z > cur) } else { ZOOMS.iter().rev().copied().find(|&z| z < cur) };
        if let Some(z) = next {
            self.zoom = Some(z);
            if z == fit {
                self.zoom = None;
            }
        }
        self.say(&format!("{}%", self.zoom.unwrap_or(fit)));
    }

    fn fit_percent(&self) -> i32 {
        let (Some(img), area) = (self.image.as_ref(), self.picture_area()) else { return 100 };
        let f = img.first();
        let (iw, ih) = if self.rotation % 2 == 1 { (f.height as i32, f.width as i32) } else { (f.width as i32, f.height as i32) };
        if iw == 0 || ih == 0 {
            return 100;
        }
        let p = (area.w * 100 / iw).min(area.h * 100 / ih);
        p.clamp(1, 100)
    }

    // ----- layout -----

    fn size(&self) -> (i32, i32) {
        wm::size(&self.win)
    }

    fn controls_visible(&self) -> bool {
        !self.fullscreen || pit::ticks() < self.controls_until
    }

    fn picture_area(&self) -> Rect {
        let (w, h) = self.size();
        if self.fullscreen {
            return Rect::new(0, 0, w, h);
        }
        let strip = if self.strip && self.entries.len() > 1 { STRIP_H } else { 0 };
        let info = if self.info { INFO_W } else { 0 };
        Rect::new(0, 0, w - info, h - BAR_H - strip)
    }

    fn state(&self, r: Rect) -> State {
        ui::state(self.cursor, self.held, r)
    }

    // ----- events -----

    fn run(&mut self) {
        self.render();
        loop {
            let animated = self.image.as_ref().is_some_and(|i| i.animated());
            let timeout = if animated { pit::HZ / 50 } else if self.toast.is_some() || self.fullscreen { pit::HZ / 5 } else { pit::HZ };
            let ev = wm::wait_event(&self.win, Some(timeout));
            let mut redraw = true;
            match ev {
                Some(Event::Close) => return,
                Some(Event::Key(k)) => self.key(k),
                Some(Event::MouseDown { x, y, button }) => {
                    self.held = true;
                    self.cursor = Some((x, y));
                    self.controls_until = pit::ticks() + HIDE_AFTER;
                    if button == 1 {
                        self.click(x, y);
                    }
                }
                Some(Event::MouseUp { x, y }) => {
                    self.held = false;
                    self.cursor = Some((x, y));
                    self.drag = None;
                }
                Some(Event::MouseMove { x, y }) => {
                    let was = self.cursor;
                    self.cursor = (x >= 0 && y >= 0).then_some((x, y));
                    if self.cursor != was {
                        self.controls_until = pit::ticks() + HIDE_AFTER;
                    }
                    if let Some((sx, sy, px, py)) = self.drag {
                        self.pan = (px + x - sx, py + y - sy);
                    }
                }
                Some(Event::Wheel(n)) => {
                    if self.cursor.is_some_and(|(_, y)| y >= self.picture_area().y + self.picture_area().h) && !self.fullscreen {
                        // Over the strip: step through the folder.
                        self.step(n.signum());
                    } else {
                        self.zoom_step(-n.signum());
                    }
                }
                Some(_) => {}
                None => {
                    redraw = false;
                    if fs::generation() != self.fs_gen {
                        let cur = self.index.and_then(|i| self.entries.get(i)).map(|e| e.path.clone());
                        self.scan(cur.as_deref());
                        self.index = cur.and_then(|p| self.entries.iter().position(|e| e.path == p));
                        redraw = true;
                    }
                    if self.toast.as_ref().is_some_and(|(_, until)| pit::ticks() > *until) {
                        self.toast = None;
                        redraw = true;
                    }
                    if self.fullscreen && !self.controls_visible() {
                        redraw = true;
                    }
                    if let Some(img) = &self.image
                        && img.animated()
                    {
                        let delay = img.frames[self.frame].1.max(20) as u64;
                        if pit::ticks() - self.frame_at >= delay {
                            self.frame = (self.frame + 1) % img.frames.len();
                            self.frame_at = pit::ticks();
                            redraw = true;
                        }
                    }
                }
            }
            if redraw {
                self.render();
            }
        }
    }

    fn key(&mut self, k: Key) {
        self.controls_until = pit::ticks() + HIDE_AFTER;
        match k {
            Key::Left | Key::PageUp | Key::Char(0x08 | 0x7F) => self.step(-1),
            Key::Right | Key::PageDown | Key::Char(b' ') => self.step(1),
            Key::Home => self.show(0),
            Key::End => {
                let n = self.entries.len();
                if n > 0 {
                    self.show(n - 1);
                }
            }
            Key::Char(b'+' | b'=') => self.zoom_step(1),
            Key::Char(b'-') => self.zoom_step(-1),
            Key::Char(b'0') => {
                self.zoom = None;
                self.pan = (0, 0);
            }
            Key::Char(b'1') => {
                self.zoom = Some(100);
                self.pan = (0, 0);
            }
            Key::Char(b'r') => self.rotation = (self.rotation + 1) % 4,
            Key::Char(b'R') => self.rotation = (self.rotation + 3) % 4,
            Key::Char(b'f' | b'F') => self.toggle_fullscreen(),
            Key::Char(b'i' | b'I') => self.info = !self.info,
            Key::Char(b't' | b'T') => self.strip = !self.strip,
            Key::Char(b'w' | b'W') => self.set_wallpaper(),
            Key::Char(0x1B) => {
                if self.fullscreen {
                    self.toggle_fullscreen();
                } else if self.info {
                    self.info = false;
                }
            }
            _ => {}
        }
    }

    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        wm::set_fullscreen(&self.win, self.fullscreen);
        self.controls_until = pit::ticks() + HIDE_AFTER;
    }

    fn click(&mut self, x: i32, y: i32) {
        let hit = self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| h.clone());
        match hit {
            None | Some(Hit::Swallow) => {}
            Some(Hit::Prev) => self.step(-1),
            Some(Hit::Next) => self.step(1),
            Some(Hit::Select(i)) => self.show(i),
            Some(Hit::ZoomIn) => self.zoom_step(1),
            Some(Hit::ZoomOut) => self.zoom_step(-1),
            Some(Hit::Fit) => {
                self.zoom = if self.zoom.is_none() { Some(100) } else { None };
                self.pan = (0, 0);
            }
            Some(Hit::Rotate) => self.rotation = (self.rotation + 1) % 4,
            Some(Hit::Fullscreen) => self.toggle_fullscreen(),
            Some(Hit::Info) => self.info = !self.info,
            Some(Hit::Wallpaper) => self.set_wallpaper(),
            Some(Hit::Strip) => self.strip = !self.strip,
            Some(Hit::Library) => {
                self.index = None;
                self.image = None;
                self.scan(None);
                wm::set_title(&self.win, "Photos");
            }
            Some(Hit::Picture) => {
                if self.zoom.is_some() {
                    self.drag = Some((x, y, self.pan.0, self.pan.1));
                }
            }
        }
    }

    // ----- drawing -----

    fn render(&mut self) {
        let (w, h) = self.size();
        let mut c = Canvas::new(w as usize, h as usize);
        self.hits.clear();
        if self.index.is_none() {
            self.draw_library(&mut c);
        } else {
            self.draw_viewer(&mut c);
        }
        if let Some((text, _)) = self.toast.clone() {
            ui::toast(&mut c, Rect::new(0, 0, w, h - if self.index.is_some() && !self.fullscreen { BAR_H } else { 0 }), &text, false);
        }
        wm::present(&self.win, &c);
    }

    fn draw_library(&mut self, c: &mut Canvas) {
        let (w, h) = self.size();
        c.clear(ui::BG);
        c.fill_rect(Rect::new(0, 0, w, chrome::TOP_H), ui::SIDEBAR);
        ui::divider(c, Rect::new(0, chrome::TOP_H - 1, w, 1));
        chrome::crumbs(c, 20, 18, &["Pictures"], None);
        let n = self.entries.len();
        font::draw(c, 20 + font::width("Pictures", ui::HEADING, Weight::Bold) + 14, 22, &format!("{n} picture{}", if n == 1 { "" } else { "s" }), ui::CAPTION, Weight::Regular, ui::DIM);
        let area = Rect::new(0, chrome::TOP_H, w, h - chrome::TOP_H);
        if n == 0 {
            ui::empty_state(c, area, "image", "No pictures yet", "Put pictures (png, jpg, gif, webp) in a Pictures folder, or open one from Files.");
            return;
        }
        let tile = 156;
        let pitch = tile + 20;
        let per = ((w - 40 + 20) / pitch).max(1) as usize;
        for (i, e) in self.entries.clone().iter().enumerate() {
            let r = Rect::new(20 + (i % per) as i32 * pitch, area.y + 20 + (i / per) as i32 * (pitch + 24), tile, tile);
            if r.y > h {
                break;
            }
            let t = self.thumb(&e.path);
            let hit = Rect::new(r.x - 6, r.y - 6, r.w + 12, r.h + 34);
            if self.state(hit) != State::Normal {
                c.fill_round_rect(hit, ui::RADIUS + 4, ui::HOVER);
            }
            match t {
                Some(t) => {
                    c.fill_round_rect(r, ui::RADIUS, 0x0B0C12);
                    let fit = super::video::fit_rect(r, t.width as i32, t.height as i32);
                    c.blit_scaled(&t, fit, ui::RADIUS_S, 255);
                }
                None => {
                    c.fill_round_rect(r, ui::RADIUS, ui::PANEL);
                    ui::symbol(c, "image", ui::FAINT, Rect::new(r.x + r.w / 2 - 20, r.y + r.h / 2 - 20, 40, 40));
                }
            }
            font::draw_clipped(c, r.x + 2, r.y + r.h + 8, e.path.rsplit('/').next().unwrap_or(&e.path), 13, Weight::Regular, ui::TEXT, r.w - 4);
            self.hits.push((hit, Hit::Select(i)));
        }
    }

    fn draw_viewer(&mut self, c: &mut Canvas) {
        let (w, h) = self.size();
        c.clear(0x0B0C12);
        let area = self.picture_area();
        self.hits.push((area, Hit::Picture));
        let pic = self.picture();
        let fit = self.fit_percent();
        let zoom = self.zoom.unwrap_or(fit);
        if let Some(p) = &pic {
            let (pw, ph) = ((p.width as i32 * zoom / 100).max(1), (p.height as i32 * zoom / 100).max(1));
            let mut dst = Rect::new(area.x + (area.w - pw) / 2, area.y + (area.h - ph) / 2, pw, ph);
            if self.zoom.is_some() {
                // Pan within limits: the picture never leaves the area entirely.
                let (mx, my) = (((pw - area.w) / 2).max(0), ((ph - area.h) / 2).max(0));
                self.pan = (self.pan.0.clamp(-mx, mx), self.pan.1.clamp(-my, my));
                dst.x += self.pan.0;
                dst.y += self.pan.1;
            }
            if zoom <= 100 {
                c.blit_scaled(p, dst, 0, 255);
            } else if zoom < 300 {
                let vis = dst.intersect(&area).unwrap_or(dst);
                // Enlarge only the visible part (bilinear).
                let sx0 = ((vis.x - dst.x) as i64 * p.width as i64 / pw as i64) as i32;
                let sy0 = ((vis.y - dst.y) as i64 * p.height as i64 / ph as i64) as i32;
                let sx1 = ((((vis.x + vis.w - dst.x) as i64 * p.width as i64 + pw as i64 - 1) / pw as i64) as i32).min(p.width as i32);
                let sy1 = ((((vis.y + vis.h - dst.y) as i64 * p.height as i64 + ph as i64 - 1) / ph as i64) as i32).min(p.height as i32);
                let mut crop = Canvas::new((sx1 - sx0).max(1) as usize, (sy1 - sy0).max(1) as usize);
                crop.blit(p, Rect::new(sx0, sy0, sx1 - sx0, sy1 - sy0), 0, 0);
                let up = super::video::bilinear(&crop, (sx1 - sx0) * zoom / 100, (sy1 - sy0) * zoom / 100);
                c.blit(&up, up.bounds(), dst.x + sx0 * zoom / 100, dst.y + sy0 * zoom / 100);
            } else {
                // Big zooms show the pixels (nearest), the visible part only.
                let vis = dst.intersect(&area).unwrap_or(dst);
                for y in vis.y..vis.y + vis.h {
                    let sy = ((y - dst.y) as i64 * p.height as i64 / ph as i64) as usize;
                    let row = &p.pixels[sy.min(p.height - 1) * p.width..];
                    for x in vis.x..vis.x + vis.w {
                        let sx = ((x - dst.x) as i64 * p.width as i64 / pw as i64) as usize;
                        c.pixels[y as usize * c.width + x as usize] = row[sx.min(p.width - 1)];
                    }
                }
            }
        } else {
            let msg = self.error.clone().unwrap_or_else(|| "Decoding\u{2026}".into());
            ui::empty_state(c, area, "image", "Cannot show this picture", &msg);
        }
        if !self.controls_visible() {
            return;
        }
        let entry = self.index.and_then(|i| self.entries.get(i)).cloned();
        let name = entry.as_ref().map(|e| e.path.rsplit('/').next().unwrap_or(&e.path).to_string()).unwrap_or_default();
        // Top overlay: name and position in the folder.
        for y in 0..64 {
            c.blend_rect(Rect::new(area.x, y, area.w, 1), 0, (150 * (64 - y) / 64) as u32);
        }
        let lib = Rect::new(16, 14, 36, 36);
        if self.state(lib) != State::Normal {
            c.fill_round_rect(lib, 18, blend(0xFFFFFF, 0, 200));
        }
        ui::symbol(c, "grid", 0xFFFFFF, Rect::new(lib.x + 8, lib.y + 8, 20, 20));
        self.hits.push((lib, Hit::Library));
        font::draw_clipped(c, 64, 22, &name, ui::HEADING, Weight::Bold, 0xFFFFFF, area.w - 200);
        if let Some(i) = self.index {
            let t = format!("{} of {}", i + 1, self.entries.len());
            let tw = font::width(&t, ui::CAPTION, Weight::Regular);
            font::draw(c, area.x + area.w - 20 - tw, 26, &t, ui::CAPTION, Weight::Regular, blend(0xFFFFFF, 0, 60));
        }
        // Previous / next arrows at the sides while hovering near them.
        if self.entries.len() > 1 {
            for (dir, x, glyph) in [(-1, 16, "chevron-left"), (1, area.x + area.w - 56, "chevron-right")] {
                let r = Rect::new(x, area.y + area.h / 2 - 24, 40, 48);
                let near = self.cursor.is_some_and(|(cx, _)| (cx - (r.x + 20)).abs() < 120);
                if near || self.fullscreen {
                    c.fill_round_rect(r, 12, blend(0, 0xFFFFFF, if self.state(r) != State::Normal { 70 } else { 40 }));
                    ui::symbol(c, glyph, 0xFFFFFF, Rect::new(r.x + 10, r.y + 14, 20, 20));
                }
                self.hits.push((r, if dir < 0 { Hit::Prev } else { Hit::Next }));
            }
        }
        if self.fullscreen {
            self.draw_bar(c, Rect::new(0, h - BAR_H, w, BAR_H), true, zoom, fit);
            return;
        }
        // Filmstrip.
        let strip_y = area.y + area.h;
        if self.strip && self.entries.len() > 1 {
            let sr = Rect::new(0, strip_y, area.w, STRIP_H);
            c.fill_rect(sr, ui::SIDEBAR);
            ui::divider(c, Rect::new(0, sr.y, sr.w, 1));
            let th = STRIP_H - 20;
            let tw = th * 4 / 3;
            let pitch = tw + 10;
            let per = (area.w / pitch).max(1) as usize;
            let cur = self.index.unwrap_or(0);
            let first = cur.saturating_sub(per / 2).min(self.entries.len().saturating_sub(per));
            let x0 = (area.w - (per.min(self.entries.len()) as i32 * pitch - 10)) / 2;
            for (k, i) in (first..self.entries.len()).take(per).enumerate() {
                let path = self.entries[i].path.clone();
                let r = Rect::new(x0 + k as i32 * pitch, sr.y + 10, tw, th);
                let t = self.thumb(&path);
                c.fill_round_rect(r, 6, 0x0B0C12);
                if let Some(t) = t {
                    let f = super::video::fit_rect(r, t.width as i32, t.height as i32);
                    c.blit_scaled(&t, f, 4, 255);
                }
                if i == cur {
                    c.stroke_round_rect_w(Rect::new(r.x - 2, r.y - 2, r.w + 4, r.h + 4), 8, 2, ACCENT);
                } else if self.state(r) != State::Normal {
                    c.stroke_round_rect_w(r, 6, 1, blend(ui::TEXT, 0, 60));
                }
                self.hits.push((r, Hit::Select(i)));
            }
        }
        // Info panel.
        if self.info {
            let ir = Rect::new(w - INFO_W, 0, INFO_W, h - BAR_H);
            c.fill_rect(ir, ui::SIDEBAR);
            ui::divider(c, Rect::new(ir.x, 0, 1, ir.h));
            self.hits.push((ir, Hit::Swallow));
            let mut y = ui::section_label(c, ir.x + ui::PAD, 18, "Info");
            let dims = self.image.as_ref().map(|i| format!("{} x {} px", i.first().width, i.first().height)).unwrap_or_default();
            let kind = entry.as_ref().and_then(|e| library::read(&e.path)).and_then(|d| image::format_of(&d)).map(|f| f.to_uppercase()).unwrap_or_else(|| "Image".into());
            let frames = self.image.as_ref().map_or(0, |i| i.frames.len());
            let rows: Vec<(&str, String)> = alloc::vec![
                ("Name", name.clone()),
                ("Folder", entry.as_ref().map(|e| e.path.rsplit_once('/').map_or("", |(d, _)| d).to_string()).unwrap_or_default()),
                ("Kind", if frames > 1 { format!("{kind}, {frames} frames") } else { kind }),
                ("Size", dims),
                ("File size", entry.as_ref().map(|e| fmt_size(e.size)).unwrap_or_default()),
                ("Zoom", format!("{zoom}%{}", if self.zoom.is_none() { " (fit)" } else { "" })),
                ("Rotation", format!("{}\u{00b0}", self.rotation * 90)),
            ];
            for (k, v) in rows {
                font::draw(c, ir.x + ui::PAD, y, k, ui::CAPTION, Weight::Bold, ui::DIM);
                y += 18;
                for line in font::wrap(&v, ui::BODY, Weight::Regular, INFO_W - 2 * ui::PAD, 3) {
                    font::draw(c, ir.x + ui::PAD, y, &line, ui::BODY, Weight::Regular, ui::TEXT);
                    y += font::line_height(ui::BODY);
                }
                y += 10;
            }
            y += 8;
            chrome::hint_rows(c, ir.x + ui::PAD, y, &[("\u{2190} \u{2192}", "Previous / next"), ("+ -", "Zoom"), ("0", "Fit"), ("R", "Rotate"), ("F", "Fullscreen"), ("W", "Set as wallpaper")]);
        }
        self.draw_bar(c, Rect::new(0, h - BAR_H, w, BAR_H), false, zoom, fit);
    }

    fn draw_bar(&mut self, c: &mut Canvas, bar: Rect, overlay: bool, zoom: i32, fit: i32) {
        if overlay {
            for y in 0..bar.h + 24 {
                c.blend_rect(Rect::new(0, bar.y - 24 + y, bar.w, 1), 0, (170 * y / (bar.h + 24)) as u32);
            }
        } else {
            c.fill_rect(bar, ui::SIDEBAR);
            ui::divider(c, Rect::new(0, bar.y, bar.w, 1));
        }
        self.hits.push((bar, Hit::Swallow));
        let cy = bar.y + bar.h / 2;
        let b = |x: i32| Rect::new(x, cy - 16, 32, 32);
        let mut x = 12;
        let many = self.entries.len() > 1;
        chrome::tool(c, b(x), "chevron-left", if many { self.state(b(x)) } else { State::Disabled }, false, ACCENT);
        self.hits.push((b(x), Hit::Prev));
        x += 34;
        chrome::tool(c, b(x), "chevron-right", if many { self.state(b(x)) } else { State::Disabled }, false, ACCENT);
        self.hits.push((b(x), Hit::Next));
        x += 46;
        // Zoom group.
        chrome::tool(c, b(x), "minus", self.state(b(x)), false, ACCENT);
        self.hits.push((b(x), Hit::ZoomOut));
        x += 34;
        let zt = format!("{zoom}%");
        let zw = font::width(&zt, 13, Weight::Bold).max(44);
        let zr = Rect::new(x, cy - 14, zw + 12, 28);
        if self.state(zr) != State::Normal {
            c.fill_round_rect(zr, ui::RADIUS_S, ui::HOVER);
        }
        font::draw(c, zr.x + (zr.w - font::width(&zt, 13, Weight::Bold)) / 2, cy - 8, &zt, 13, Weight::Bold, if self.zoom.is_none() { ui::DIM } else { ui::TEXT });
        self.hits.push((zr, Hit::Fit));
        x += zr.w + 2;
        chrome::tool(c, b(x), "plus", self.state(b(x)), false, ACCENT);
        self.hits.push((b(x), Hit::ZoomIn));
        x += 34;
        chrome::tool(c, b(x), "fullscreen", self.state(b(x)), self.zoom.is_some() && zoom == fit, ACCENT);
        self.hits.push((b(x), Hit::Fit));
        let _ = x;
        // Right group.
        let mut rx = bar.w - 12 - 32;
        let tools: [(&str, Hit, bool); 5] = [
            ("fullscreen", Hit::Fullscreen, self.fullscreen),
            ("info", Hit::Info, self.info),
            ("wallpaper", Hit::Wallpaper, false),
            ("rotate", Hit::Rotate, self.rotation != 0),
            ("film", Hit::Strip, self.strip && many),
        ];
        for (glyph, hit, on) in tools {
            if hit == Hit::Strip && (overlay || !many) {
                continue;
            }
            let r = b(rx);
            chrome::tool(c, r, glyph, self.state(r), on, ACCENT);
            self.hits.push((r, hit));
            rx -= 36;
        }
    }
}

fn fmt_size(n: u64) -> String {
    match n {
        0 => "built in".into(),
        n if n >= 1 << 20 => format!("{}.{} MB", n >> 20, ((n & ((1 << 20) - 1)) * 10) >> 20),
        n if n >= 1 << 10 => format!("{} KB", n >> 10),
        n => format!("{n} bytes"),
    }
}
