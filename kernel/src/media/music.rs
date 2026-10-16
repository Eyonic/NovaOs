//! Music: Nova's music player. The library comes from the Music folder (tags and cover
//! art read by the media engine and cached), shown as Listen Again, Songs, Albums and
//! Artists; a queue with shuffle and repeat; and a now-playing bar that keeps playing
//! while you browse, with the transport, seek bar, volume wedge, equalizer presets and
//! the queue. Keys: Space play/pause, arrows seek/volume, N/P next/previous, S shuffle,
//! R repeat, Q queue, Ctrl-F search.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use super::chrome::{self, MUSIC_ACCENT as ACCENT, SIDEBAR_W, TOP_H};
use super::engine::{Engine, PlayState};
use super::library::{self, display_name, fmt_time, Entry, History, Prefs, TagCache, Tags, Thumbs};
use crate::font::{self, Weight};
use crate::gfx::{blend, Canvas, Rect};
use crate::input::Key;
use crate::ui::{self, State};
use crate::wm::{self, Content, Event, WindowRef};
use crate::{fs, pit, task};

pub const ICON: char = '\u{266A}';

const BAR_H: i32 = 84;
const ROW_H: i32 = 44;
const TILE: i32 = 150;
const TILE_PITCH_X: i32 = TILE + 24;
const TILE_PITCH_Y: i32 = TILE + 64;

/// Open the Music player; with a path, play it at once.
pub fn open(path: Option<String>) {
    let (sw, sh) = wm::work_area();
    let (w, h) = ((sw - 200).min(1000), (sh - 120).min(660));
    let win = wm::open("Music", ICON, ACCENT, w, h, Content::Canvas(Canvas::new(w as usize, h as usize)));
    task::spawn("music", move || {
        let mut app = Music::new(win.clone());
        if let Some(p) = path {
            app.play_outside(&p);
        }
        app.run();
        wm::close(&win);
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    Songs,
    Albums,
    Artists,
    Album,
    Artist,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Repeat {
    Off,
    All,
    One,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pop {
    None,
    Queue,
    Equalizer,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    None,
    Seek,
    Volume,
}

#[derive(Clone, PartialEq)]
enum Hit {
    Nav(View),
    Song(usize),
    Album(String),
    Artist(String),
    PlayAll(Vec<usize>),
    Search,
    Back,
    PlayPause,
    Prev,
    Next,
    Seek(Rect),
    Volume(Rect),
    Mute,
    Shuffle,
    Repeat,
    Pop(Pop),
    QueueItem(usize),
    QueueClear,
    Preset(i32),
    Swallow,
}

#[derive(Clone)]
struct Song {
    entry: Entry,
    tags: Tags,
}

impl Song {
    fn title(&self) -> String {
        if self.tags.title.is_empty() { display_name(&self.entry.path) } else { self.tags.title.clone() }
    }
    fn artist(&self) -> String {
        if self.tags.artist.is_empty() { "Unknown artist".into() } else { self.tags.artist.clone() }
    }
    fn album(&self) -> String {
        if self.tags.album.is_empty() { library::folder_name(&self.entry.path) } else { self.tags.album.clone() }
    }
}

struct Music {
    win: WindowRef,
    engine: Option<Engine>,
    engine_error: Option<String>,
    view: View,
    detail: String,
    songs: Vec<Song>,
    tag_jobs: Vec<usize>,
    fs_gen: u64,
    search: String,
    searching: bool,
    scroll: i32,
    queue: Vec<usize>,
    qpos: usize,
    shuffle: bool,
    repeat: Repeat,
    current: Option<usize>,
    volume: i32,
    muted: bool,
    preset: i32,
    pop: Pop,
    drag: Drag,
    toast: Option<(String, u64)>,
    cursor: Option<(i32, i32)>,
    held: bool,
    hits: Vec<(Rect, Hit)>,
    ended_handled: bool,
    /// Cover art keyed by album name (decoded once).
    art: BTreeMap<String, Option<Arc<Canvas>>>,
}

impl Music {
    fn new(win: WindowRef) -> Self {
        let mut m = Music {
            win,
            engine: None,
            engine_error: None,
            view: View::Home,
            detail: String::new(),
            songs: Vec::new(),
            tag_jobs: Vec::new(),
            fs_gen: 0,
            search: String::new(),
            searching: false,
            scroll: 0,
            queue: Vec::new(),
            qpos: 0,
            shuffle: false,
            repeat: Repeat::Off,
            current: None,
            volume: Prefs::volume(),
            muted: false,
            preset: -1,
            pop: Pop::None,
            drag: Drag::None,
            toast: None,
            cursor: None,
            held: false,
            hits: Vec::new(),
            ended_handled: false,
            art: BTreeMap::new(),
        };
        m.rescan();
        m
    }

    fn engine(&mut self) -> Option<&mut Engine> {
        if self.engine.is_none() && self.engine_error.is_none() {
            match Engine::start() {
                Ok(mut e) => {
                    e.set_volume(self.volume);
                    self.engine = Some(e);
                }
                Err(err) => self.engine_error = Some(err),
            }
        }
        self.engine.as_mut()
    }

    fn rescan(&mut self) {
        self.fs_gen = fs::generation();
        let cur = self.current.and_then(|i| self.songs.get(i)).map(|s| s.entry.path.clone());
        let queue_paths: Vec<String> = self.queue.iter().filter_map(|&i| self.songs.get(i)).map(|s| s.entry.path.clone()).collect();
        self.songs = library::songs().into_iter().map(|entry| Song { tags: TagCache::get(&entry.path).unwrap_or_default(), entry }).collect();
        self.tag_jobs = self.songs.iter().enumerate().filter(|(_, s)| s.tags.length_ms == 0 && !s.tags.has_art).map(|(i, _)| i).rev().collect();
        self.queue = queue_paths.iter().filter_map(|p| self.songs.iter().position(|s| &s.entry.path == p)).collect();
        self.current = cur.and_then(|p| self.songs.iter().position(|s| s.entry.path == p));
        self.qpos = self.current.and_then(|c| self.queue.iter().position(|&i| i == c)).unwrap_or(0);
    }

    fn say(&mut self, text: &str) {
        self.toast = Some((text.into(), pit::ticks() + 3000));
    }

    /// Read one song's tags and cover art while nothing else needs the engine.
    fn idle_work(&mut self) {
        if self.engine_error.is_some() {
            return;
        }
        let Some(i) = self.tag_jobs.pop() else { return };
        let Some(path) = self.songs.get(i).map(|s| s.entry.path.clone()) else { return };
        let Some(e) = self.engine() else { return };
        let meta = e.probe(&path);
        let mut tags = Tags {
            title: meta.get("title").cloned().unwrap_or_default(),
            artist: meta.get("artist").cloned().unwrap_or_default(),
            album: meta.get("album").cloned().unwrap_or_default(),
            track: meta.get("track").and_then(|t| t.split('/').next()).and_then(|t| t.trim().parse().ok()).unwrap_or(0),
            length_ms: meta.get("length").and_then(|l| l.parse().ok()).unwrap_or(1),
            has_art: false,
        };
        if tags.title == path.rsplit('/').next().unwrap_or(&path) {
            tags.title.clear(); // libVLC's fallback title is the file name
        }
        // Cover art: libVLC extracts embedded pictures to its cache (a tmpfs file).
        if let Some(url) = meta.get("artwork") {
            let file = url.trim_start_matches("file://").to_string();
            if let Some(data) = crate::vfs::read_file(file.trim_start_matches('/')).ok().filter(|d| !d.is_empty())
                && let Ok(img) = super::image::decode(&data)
            {
                Thumbs::put(&path, img.first(), 320);
                tags.has_art = true;
            }
        }
        TagCache::set(&path, &tags);
        if let Some(s) = self.songs.get_mut(i) {
            s.tags = tags;
        }
        self.art.clear();
    }

    /// The cover for an album: the first song of it that has art.
    fn album_art(&mut self, album: &str) -> Option<Arc<Canvas>> {
        if let Some(a) = self.art.get(album) {
            return a.clone();
        }
        let art = self.songs.iter().filter(|s| s.album() == album && s.tags.has_art).find_map(|s| Thumbs::get(&s.entry.path));
        self.art.insert(album.into(), art.clone());
        art
    }

    // ----- playback -----

    fn play_outside(&mut self, path: &str) {
        if let Some(i) = self.songs.iter().position(|s| s.entry.path == path) {
            let folder = library::folder_name(path);
            let q: Vec<usize> = self.songs.iter().enumerate().filter(|(_, s)| library::folder_name(&s.entry.path) == folder).map(|(i, _)| i).collect();
            self.play_queue(q, i);
        } else {
            self.songs.push(Song { entry: Entry { path: path.into(), size: 0, mtime: 0 }, tags: TagCache::get(path).unwrap_or_default() });
            let i = self.songs.len() - 1;
            self.play_queue(alloc::vec![i], i);
        }
    }

    /// Play song `start` with `queue` (song indices) as the queue.
    fn play_queue(&mut self, queue: Vec<usize>, start: usize) {
        let mut q = queue;
        if self.shuffle && q.len() > 2 {
            q.retain(|&i| i != start);
            for i in (1..q.len()).rev() {
                let j = (crate::rand::u64() % (i as u64 + 1)) as usize;
                q.swap(i, j);
            }
            q.insert(0, start);
        }
        self.qpos = q.iter().position(|&i| i == start).unwrap_or(0);
        self.queue = q;
        self.start_current();
    }

    fn start_current(&mut self) {
        let Some(&i) = self.queue.get(self.qpos) else { return };
        let Some(path) = self.songs.get(i).map(|s| s.entry.path.clone()) else { return };
        self.current = Some(i);
        self.ended_handled = false;
        let (volume, muted, preset) = (self.volume, self.muted, self.preset);
        if let Some(e) = self.engine() {
            e.open(&path, 0, false);
            e.set_volume(volume);
            e.set_mute(muted);
            if preset >= 0 {
                e.set_equalizer(preset);
            }
        }
        History::played(&path);
        let title = self.songs.get(i).map(|s| s.title()).unwrap_or_default();
        wm::set_title(&self.win, &format!("{title} - Music"));
    }

    fn step(&mut self, dir: i32) {
        if self.queue.is_empty() {
            return;
        }
        let n = self.queue.len() as i32;
        let next = self.qpos as i32 + dir;
        if next < 0 || next >= n {
            if self.repeat == Repeat::All {
                self.qpos = (((next % n) + n) % n) as usize;
            } else {
                if let Some(e) = self.engine.as_mut() {
                    e.stop();
                }
                return;
            }
        } else {
            self.qpos = next as usize;
        }
        self.start_current();
    }

    fn set_volume(&mut self, v: i32) {
        self.volume = v.clamp(0, 150);
        self.muted = false;
        let vol = self.volume;
        if let Some(e) = self.engine.as_mut() {
            e.set_volume(vol);
            e.set_mute(false);
        }
        Prefs::set("volume", &vol.to_string());
    }

    fn playing(&self) -> bool {
        self.engine.as_ref().is_some_and(|e| e.state == PlayState::Playing)
    }

    // ----- events -----

    fn run(&mut self) {
        self.render();
        loop {
            let timeout = if self.playing() { pit::HZ / 4 } else if !self.tag_jobs.is_empty() || self.toast.is_some() { pit::HZ / 10 } else { pit::HZ / 2 };
            let ev = wm::wait_event(&self.win, Some(timeout));
            let mut redraw = true;
            match ev {
                Some(Event::Close) => return,
                Some(Event::Key(k)) => self.key(k),
                Some(Event::MouseDown { x, y, button }) => {
                    self.held = true;
                    self.cursor = Some((x, y));
                    if button == 1 {
                        self.click(x, y);
                    }
                }
                Some(Event::MouseUp { x, y }) => {
                    self.held = false;
                    self.cursor = Some((x, y));
                    self.drag_to(x);
                    self.drag = Drag::None;
                }
                Some(Event::MouseMove { x, y }) => {
                    self.cursor = (x >= 0 && y >= 0).then_some((x, y));
                    if self.held {
                        self.drag_to(x);
                    }
                }
                Some(Event::Wheel(n)) => self.scroll = (self.scroll - n * 48).max(0),
                Some(_) => {}
                None => {
                    redraw = self.playing();
                    if fs::generation() != self.fs_gen {
                        self.rescan();
                        redraw = true;
                    }
                    if self.toast.as_ref().is_some_and(|(_, until)| pit::ticks() > *until) {
                        self.toast = None;
                        redraw = true;
                    }
                    if !self.tag_jobs.is_empty() {
                        self.idle_work();
                        redraw = true;
                    }
                }
            }
            if let Some(e) = self.engine.as_mut() {
                if !e.pump() {
                    self.engine_error = Some("the media engine stopped".into());
                    self.engine = None;
                    redraw = true;
                } else if e.state == PlayState::Ended && !self.ended_handled {
                    self.ended_handled = true;
                    if self.repeat == Repeat::One {
                        self.start_current();
                    } else {
                        self.step(1);
                    }
                    redraw = true;
                }
            }
            if redraw {
                self.render();
            }
        }
    }

    fn key(&mut self, k: Key) {
        if self.searching {
            match k {
                Key::Char(0x1B) => {
                    self.searching = false;
                    self.search.clear();
                }
                Key::Char(0x08 | 0x7F) => {
                    self.search.pop();
                }
                Key::Char(b'\n') => self.searching = false,
                Key::Char(c) if (0x20..0x7F).contains(&c) => self.search.push(c as char),
                _ => {}
            }
            self.scroll = 0;
            return;
        }
        match k {
            Key::Char(b' ') => {
                if self.current.is_some() {
                    if let Some(e) = self.engine.as_mut() {
                        e.toggle();
                    }
                } else if !self.songs.is_empty() {
                    let all: Vec<usize> = (0..self.songs.len()).collect();
                    self.play_queue(all, 0);
                }
            }
            Key::Char(0x06) => self.searching = true,
            Key::Char(b'n' | b'N') => self.step(1),
            Key::Char(b'p' | b'P') => self.step(-1),
            Key::Char(b's' | b'S') => self.toggle_shuffle(),
            Key::Char(b'r' | b'R') => self.cycle_repeat(),
            Key::Char(b'q' | b'Q') => self.pop = if self.pop == Pop::Queue { Pop::None } else { Pop::Queue },
            Key::Char(b'm' | b'M') => self.toggle_mute(),
            Key::Left => {
                if let Some(e) = self.engine.as_mut() {
                    let p = e.position_ms() - 5000;
                    e.seek(p);
                }
            }
            Key::Right => {
                if let Some(e) = self.engine.as_mut() {
                    let p = e.position_ms() + 5000;
                    e.seek(p);
                }
            }
            Key::Up => self.set_volume(self.volume + 5),
            Key::Down => self.set_volume(self.volume - 5),
            Key::Char(0x08 | 0x7F) => self.go_back(),
            Key::Char(0x1B) => {
                if self.pop != Pop::None {
                    self.pop = Pop::None;
                } else {
                    self.go_back();
                }
            }
            _ => {}
        }
    }

    fn go_back(&mut self) {
        self.view = match self.view {
            View::Album => View::Albums,
            View::Artist => View::Artists,
            _ => View::Home,
        };
        self.scroll = 0;
    }

    fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        if self.shuffle && self.queue.len() > 2 {
            let cur = self.queue.get(self.qpos).copied();
            let mut rest: Vec<usize> = self.queue.iter().copied().filter(|&i| Some(i) != cur).collect();
            for i in (1..rest.len()).rev() {
                let j = (crate::rand::u64() % (i as u64 + 1)) as usize;
                rest.swap(i, j);
            }
            if let Some(c) = cur {
                rest.insert(0, c);
            }
            self.queue = rest;
            self.qpos = 0;
        } else if !self.shuffle {
            let cur = self.queue.get(self.qpos).copied();
            self.queue.sort();
            self.qpos = cur.and_then(|c| self.queue.iter().position(|&i| i == c)).unwrap_or(0);
        }
        self.say(if self.shuffle { "Shuffle on" } else { "Shuffle off" });
    }

    fn cycle_repeat(&mut self) {
        self.repeat = match self.repeat {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        };
        self.say(match self.repeat {
            Repeat::Off => "Repeat off",
            Repeat::All => "Repeat the queue",
            Repeat::One => "Repeat this song",
        });
    }

    fn toggle_mute(&mut self) {
        self.muted = !self.muted;
        let m = self.muted;
        if let Some(e) = self.engine.as_mut() {
            e.set_mute(m);
        }
    }

    fn hit_at(&self, x: i32, y: i32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| h.clone())
    }

    fn click(&mut self, x: i32, y: i32) {
        let Some(hit) = self.hit_at(x, y) else { return };
        match hit {
            Hit::Swallow => {}
            Hit::Nav(v) => {
                self.view = v;
                self.scroll = 0;
                self.searching = false;
                self.pop = Pop::None;
            }
            Hit::Song(i) => {
                let q = self.visible_songs();
                self.play_queue(q, i);
            }
            Hit::Album(a) => {
                self.view = View::Album;
                self.detail = a;
                self.scroll = 0;
            }
            Hit::Artist(a) => {
                self.view = View::Artist;
                self.detail = a;
                self.scroll = 0;
            }
            Hit::PlayAll(q) => {
                if let Some(&first) = q.first() {
                    self.play_queue(q, first);
                }
            }
            Hit::Search => self.searching = true,
            Hit::Back => self.go_back(),
            Hit::PlayPause => {
                if self.current.is_some() {
                    if let Some(e) = self.engine.as_mut() {
                        e.toggle();
                    }
                } else {
                    let q = self.visible_songs();
                    if let Some(&f) = q.first() {
                        self.play_queue(q, f);
                    }
                }
            }
            Hit::Prev => {
                let pos = self.engine.as_ref().map_or(0, |e| e.position_ms());
                if pos > 3000 {
                    if let Some(e) = self.engine.as_mut() {
                        e.seek(0);
                    }
                } else {
                    self.step(-1);
                }
            }
            Hit::Next => self.step(1),
            Hit::Seek(r) => {
                self.drag = Drag::Seek;
                if let Some(e) = self.engine.as_mut() {
                    let t = chrome::seek_value(r, x, e.length_ms);
                    e.seek(t);
                }
            }
            Hit::Volume(r) => {
                self.drag = Drag::Volume;
                self.set_volume(chrome::volume_value(r, x));
            }
            Hit::Mute => self.toggle_mute(),
            Hit::Shuffle => self.toggle_shuffle(),
            Hit::Repeat => self.cycle_repeat(),
            Hit::Pop(p) => self.pop = if self.pop == p { Pop::None } else { p },
            Hit::QueueItem(i) => {
                if i < self.queue.len() {
                    self.qpos = i;
                    self.start_current();
                }
            }
            Hit::QueueClear => {
                let cur = self.queue.get(self.qpos).copied();
                self.queue = cur.into_iter().collect();
                self.qpos = 0;
            }
            Hit::Preset(p) => {
                self.preset = p;
                if let Some(e) = self.engine.as_mut() {
                    e.set_equalizer(p);
                }
            }
        }
    }

    fn drag_to(&mut self, x: i32) {
        match self.drag {
            Drag::Seek => {
                if let Some((r, _)) = self.hits.iter().find(|(_, h)| matches!(h, Hit::Seek(_))).cloned()
                    && let Some(e) = self.engine.as_mut()
                {
                    let t = chrome::seek_value(r, x, e.length_ms);
                    e.seek(t);
                }
            }
            Drag::Volume => {
                if let Some((r, _)) = self.hits.iter().find(|(_, h)| matches!(h, Hit::Volume(_))).cloned() {
                    self.set_volume(chrome::volume_value(r, x));
                }
            }
            Drag::None => {}
        }
    }

    // ----- lists -----

    /// Song indices the current view shows, in order.
    fn visible_songs(&self) -> Vec<usize> {
        let q = self.search.to_lowercase();
        let mut v: Vec<usize> = (0..self.songs.len())
            .filter(|&i| {
                let s = &self.songs[i];
                let in_view = match self.view {
                    View::Album => s.album() == self.detail,
                    View::Artist => s.artist() == self.detail,
                    _ => true,
                };
                in_view && (q.is_empty() || s.title().to_lowercase().contains(&q) || s.artist().to_lowercase().contains(&q) || s.album().to_lowercase().contains(&q))
            })
            .collect();
        if matches!(self.view, View::Album) {
            v.sort_by_key(|&i| (self.songs[i].tags.track, self.songs[i].title().to_lowercase()));
        } else {
            v.sort_by_key(|&i| (self.songs[i].artist().to_lowercase(), self.songs[i].album().to_lowercase(), self.songs[i].tags.track, self.songs[i].title().to_lowercase()));
        }
        v
    }

    fn albums(&self) -> Vec<(String, String, usize)> {
        let mut m: BTreeMap<String, (String, usize)> = BTreeMap::new();
        for s in &self.songs {
            let e = m.entry(s.album()).or_insert((s.artist(), 0));
            e.1 += 1;
        }
        m.into_iter().map(|(a, (ar, n))| (a, ar, n)).collect()
    }

    fn artists(&self) -> Vec<(String, usize, usize)> {
        let mut m: BTreeMap<String, (alloc::collections::BTreeSet<String>, usize)> = BTreeMap::new();
        for s in &self.songs {
            let e = m.entry(s.artist()).or_default();
            e.0.insert(s.album());
            e.1 += 1;
        }
        m.into_iter().map(|(a, (al, n))| (a, al.len(), n)).collect()
    }

    // ----- drawing -----

    fn size(&self) -> (i32, i32) {
        wm::size(&self.win)
    }

    fn content(&self) -> Rect {
        let (w, h) = self.size();
        Rect::new(SIDEBAR_W, TOP_H, w - SIDEBAR_W, h - TOP_H - BAR_H)
    }

    fn state(&self, r: Rect) -> State {
        ui::state(self.cursor, self.held, r)
    }

    fn render(&mut self) {
        let (w, h) = self.size();
        let mut c = Canvas::new(w as usize, h as usize);
        c.clear(ui::BG);
        self.hits.clear();
        self.draw_sidebar(&mut c);
        self.draw_header(&mut c);
        match self.view {
            View::Home => self.draw_home(&mut c),
            View::Songs | View::Album | View::Artist => self.draw_songs(&mut c),
            View::Albums => self.draw_albums(&mut c),
            View::Artists => self.draw_artists(&mut c),
        }
        self.draw_bar(&mut c);
        match self.pop {
            Pop::None => {}
            Pop::Queue => self.draw_queue_pop(&mut c),
            Pop::Equalizer => self.draw_eq_pop(&mut c),
        }
        if let Some((text, _)) = self.toast.clone() {
            ui::toast(&mut c, self.content(), &text, false);
        }
        wm::present(&self.win, &c);
    }

    fn draw_sidebar(&mut self, c: &mut Canvas) {
        let (_, h) = self.size();
        c.fill_rect(Rect::new(0, 0, SIDEBAR_W, h - BAR_H), ui::SIDEBAR);
        ui::divider(c, Rect::new(SIDEBAR_W - 1, 0, 1, h - BAR_H));
        let mut y = ui::section_label(c, ui::PAD + 4, 18, "Music");
        let items: [(&str, &str, View); 4] = [("home", "Listen", View::Home), ("music", "Songs", View::Songs), ("disc", "Albums", View::Albums), ("user", "Artists", View::Artists)];
        for (glyph, label, view) in items {
            let r = Rect::new(ui::GAP, y, SIDEBAR_W - 2 * ui::GAP, ui::NAV_ROW_H);
            let active = self.view == view || matches!((self.view, view), (View::Album, View::Albums) | (View::Artist, View::Artists));
            ui::nav_row(c, r, Some((glyph, None)), label, active, self.state(r));
            if active {
                c.fill_round_rect(Rect::new(r.x, r.y + 8, 3, r.h - 16), 2, ACCENT);
            }
            self.hits.push((r, Hit::Nav(view)));
            y += ui::NAV_ROW_H + 2;
        }
        // Library facts and the tag scan progress.
        let n = self.songs.len();
        let y2 = h - BAR_H - 60;
        font::draw(c, ui::PAD, y2, &format!("{n} song{}", if n == 1 { "" } else { "s" }), ui::CAPTION, Weight::Bold, ui::DIM);
        let todo = self.tag_jobs.len();
        let line = if todo > 0 {
            format!("Reading tags\u{2026} {} left", todo)
        } else if let Some(err) = &self.engine_error {
            chrome::fit_text(err, ui::CAPTION, Weight::Regular, SIDEBAR_W - 2 * ui::PAD)
        } else {
            format!("{} albums", self.albums().len())
        };
        font::draw_clipped(c, ui::PAD, y2 + 18, &line, ui::CAPTION, Weight::Regular, ui::DIM, SIDEBAR_W - 2 * ui::PAD);
    }

    fn draw_header(&mut self, c: &mut Canvas) {
        let (w, _) = self.size();
        c.fill_rect(Rect::new(SIDEBAR_W, 0, w - SIDEBAR_W, TOP_H), ui::SIDEBAR);
        ui::divider(c, Rect::new(SIDEBAR_W, TOP_H - 1, w - SIDEBAR_W, 1));
        let detail = self.detail.clone();
        let parts: Vec<&str> = match self.view {
            View::Home => alloc::vec!["Listen"],
            View::Songs => alloc::vec!["Music", "Songs"],
            View::Albums => alloc::vec!["Music", "Albums"],
            View::Artists => alloc::vec!["Music", "Artists"],
            View::Album => alloc::vec!["Albums", detail.as_str()],
            View::Artist => alloc::vec!["Artists", detail.as_str()],
        };
        let mut x = SIDEBAR_W + 20;
        if matches!(self.view, View::Album | View::Artist) {
            let b = Rect::new(x, (TOP_H - 32) / 2, 32, 32);
            ui::icon_button(c, b, "back", self.state(b), true);
            self.hits.push((b, Hit::Back));
            x += 44;
        }
        let rects = chrome::crumbs(c, x, 18, &parts, self.cursor);
        if let Some(r) = rects.first()
            && parts.len() > 1
        {
            let target = match self.view {
                View::Album => View::Albums,
                View::Artist => View::Artists,
                _ => View::Home,
            };
            self.hits.push((*r, Hit::Nav(target)));
        }
        let n = self.visible_songs().len();
        let note = format!("{n} song{}", if n == 1 { "" } else { "s" });
        let nx = rects.last().map_or(x, |r| r.x + r.w + 10);
        font::draw(c, nx, 22, &note, ui::CAPTION, Weight::Regular, ui::DIM);
        let sr = Rect::new(w - 232, (TOP_H - ui::CONTROL_H) / 2, 216, ui::CONTROL_H);
        ui::field_icon(c, sr, &self.search, "Search songs, artists, albums", self.searching, Some("search"));
        self.hits.push((sr, Hit::Search));
    }

    fn draw_home(&mut self, c: &mut Canvas) {
        let area = self.content();
        if self.songs.is_empty() {
            let msg = if self.engine_error.is_some() { "Put songs in a Music folder. The media engine is not in this image, so nothing can play yet." } else { "Put songs (mp3, flac, ogg, opus, m4a, wav) in a Music folder and they show up here." };
            ui::empty_state(c, area, "music", "No music yet", msg);
            return;
        }
        let x0 = area.x + 24;
        let mut y = area.y + 24 - self.scroll;
        // Listen Again: recently played songs as covers.
        let recent: Vec<usize> = History::recent(library::is_audio).into_iter().filter_map(|(p, ..)| self.songs.iter().position(|s| s.entry.path == p)).take(12).collect();
        let per = ((area.w - 48 + 24) / TILE_PITCH_X).max(1) as usize;
        if !recent.is_empty() {
            chrome::row_title(c, x0, y, area.w - 48, "Listen Again", "");
            for (k, &i) in recent.iter().take(per).enumerate() {
                let r = Rect::new(x0 + k as i32 * TILE_PITCH_X, y + 36, TILE, TILE);
                self.song_tile(c, r, i);
            }
            y += 36 + TILE_PITCH_Y;
        }
        // Albums row.
        let albums = self.albums();
        chrome::row_title(c, x0, y, area.w - 48, "Albums", &format!("{}", albums.len()));
        for (k, (album, artist, n)) in albums.iter().take(per).enumerate() {
            let r = Rect::new(x0 + k as i32 * TILE_PITCH_X, y + 36, TILE, TILE);
            self.album_tile(c, r, album, artist, *n);
        }
        y += 36 + TILE_PITCH_Y;
        // Recently added songs as a short list.
        let mut newest: Vec<usize> = (0..self.songs.len()).collect();
        newest.sort_by(|&a, &b| self.songs[b].entry.mtime.cmp(&self.songs[a].entry.mtime).then(self.songs[a].entry.path.cmp(&self.songs[b].entry.path)));
        chrome::row_title(c, x0, y, area.w - 48, "Recently added", "");
        y += 36;
        let all: Vec<usize> = newest.clone();
        for (k, &i) in newest.iter().take(6).enumerate() {
            let r = Rect::new(x0 - 8, y + k as i32 * ROW_H, area.w - 40, ROW_H - 2);
            if r.y > area.y + area.h {
                break;
            }
            self.song_row(c, r, i, k, &all);
        }
    }

    fn song_tile(&mut self, c: &mut Canvas, r: Rect, i: usize) {
        let (title, artist, album, path) = {
            let s = &self.songs[i];
            (s.title(), s.artist(), s.album(), s.entry.path.clone())
        };
        let art = if self.songs[i].tags.has_art { Thumbs::get(&path) } else { self.album_art(&album) };
        let hit = Rect::new(r.x - 6, r.y - 6, r.w + 12, r.h + 56);
        let hot = self.state(hit) != State::Normal || self.current == Some(i);
        if hot {
            c.fill_round_rect(hit, ui::RADIUS + 4, if self.current == Some(i) { chrome::accent_tint(ui::BG, ACCENT) } else { ui::HOVER });
        }
        chrome::cover(c, r, art.as_deref(), &album, ACCENT);
        if hot {
            let d = 40;
            chrome::play_disc(c, r.x + r.w - d / 2 - 10, r.y + r.h - d / 2 - 10, d, self.current == Some(i) && self.playing(), State::Normal, ACCENT);
        }
        font::draw_clipped(c, r.x + 2, r.y + r.h + 8, &title, 13, Weight::Bold, if self.current == Some(i) { ACCENT } else { ui::TEXT }, r.w - 4);
        font::draw_clipped(c, r.x + 2, r.y + r.h + 26, &artist, ui::CAPTION, Weight::Regular, ui::DIM, r.w - 4);
        self.hits.push((hit, Hit::Song(i)));
    }

    fn album_tile(&mut self, c: &mut Canvas, r: Rect, album: &str, artist: &str, n: usize) {
        let art = self.album_art(album);
        let hit = Rect::new(r.x - 6, r.y - 6, r.w + 12, r.h + 56);
        if self.state(hit) != State::Normal {
            c.fill_round_rect(hit, ui::RADIUS + 4, ui::HOVER);
        }
        chrome::cover(c, r, art.as_deref(), album, ACCENT);
        font::draw_clipped(c, r.x + 2, r.y + r.h + 8, album, 13, Weight::Bold, ui::TEXT, r.w - 4);
        font::draw_clipped(c, r.x + 2, r.y + r.h + 26, &format!("{artist} \u{00b7} {n} song{}", if n == 1 { "" } else { "s" }), ui::CAPTION, Weight::Regular, ui::DIM, r.w - 4);
        self.hits.push((hit, Hit::Album(album.into())));
    }

    fn song_row(&mut self, c: &mut Canvas, r: Rect, i: usize, k: usize, queue: &[usize]) {
        let current = self.current == Some(i);
        let st = self.state(r);
        if current {
            c.fill_round_rect(r, ui::RADIUS_S, chrome::accent_tint(ui::BG, ACCENT));
        } else if st != State::Normal {
            c.fill_round_rect(r, ui::RADIUS_S, ui::HOVER);
        } else if k % 2 == 1 {
            c.fill_round_rect(r, ui::RADIUS_S, 0x191A24);
        }
        let (title, artist, album, len, track, path, has_art) = {
            let s = &self.songs[i];
            (s.title(), s.artist(), s.album(), s.tags.length_ms, s.tags.track, s.entry.path.clone(), s.tags.has_art)
        };
        let ty = r.y + (r.h - font::line_height(ui::BODY)) / 2;
        // Number or playing mark, small cover, title/artist, album, time.
        if current {
            ui::symbol(c, if self.playing() { "audio" } else { "pause" }, ACCENT, Rect::new(r.x + 12, r.y + (r.h - 16) / 2, 16, 16));
        } else if st != State::Normal {
            ui::symbol(c, "play", ui::TEXT, Rect::new(r.x + 12, r.y + (r.h - 16) / 2, 16, 16));
        } else {
            let num = if track > 0 && self.view == View::Album { track.to_string() } else { (k + 1).to_string() };
            let nw = font::width(&num, 13, Weight::Regular);
            font::draw(c, r.x + 28 - nw, ty + 1, &num, 13, Weight::Regular, ui::FAINT);
        }
        let art = if has_art { Thumbs::get(&path) } else { self.album_art(&album) };
        let ar = Rect::new(r.x + 44, r.y + 5, r.h - 10, r.h - 10);
        chrome::cover(c, ar, art.as_deref(), &album, ACCENT);
        let tx = r.x + 44 + r.h + 4;
        let right = 70;
        let album_w = (r.w / 4).min(220);
        font::draw_clipped(c, tx, r.y + 4, &title, ui::BODY, if current { Weight::Bold } else { Weight::Regular }, if current { ACCENT } else { ui::TEXT }, r.w - (tx - r.x) - album_w - right);
        font::draw_clipped(c, tx, r.y + 23, &artist, ui::CAPTION, Weight::Regular, ui::DIM, r.w - (tx - r.x) - album_w - right);
        font::draw_clipped(c, r.x + r.w - right - album_w, ty + 1, &album, 13, Weight::Regular, ui::DIM, album_w - 12);
        let t = if len > 1 { fmt_time(len) } else { String::new() };
        let tw = font::width(&t, 13, Weight::Regular);
        font::draw(c, r.x + r.w - 12 - tw, ty + 1, &t, 13, Weight::Regular, ui::DIM);
        self.hits.push((r, Hit::PlayAll(queue_from(queue, i))));
    }

    fn draw_songs(&mut self, c: &mut Canvas) {
        let area = self.content();
        let songs = self.visible_songs();
        if songs.is_empty() {
            if self.search.is_empty() {
                ui::empty_state(c, area, "music", "No songs", "Nothing to list here.");
            } else {
                ui::empty_state(c, area, "search", "No matches", &format!("Nothing matches \u{201c}{}\u{201d}.", self.search));
            }
            return;
        }
        let x0 = area.x + 24;
        let mut top = area.y + 12;
        if matches!(self.view, View::Album | View::Artist) {
            // A header with the cover, the name and Play all.
            let art = if self.view == View::Album { self.album_art(&self.detail.clone()) } else { None };
            let cr = Rect::new(x0, top + 4, 112, 112);
            let detail = self.detail.clone();
            if self.view == View::Album {
                chrome::cover(c, cr, art.as_deref(), &detail, ACCENT);
            } else {
                c.fill_circle(cr.x + 56, cr.y + 56, 56, blend(ui::PANEL, ACCENT, 40));
                ui::symbol(c, "user", blend(ui::PANEL, 0xFFFFFF, 140), Rect::new(cr.x + 28, cr.y + 28, 56, 56));
            }
            font::draw_clipped(c, cr.x + 128, top + 14, &detail, ui::TITLE, Weight::Bold, ui::TEXT, area.w - 200);
            let total: i64 = songs.iter().map(|&i| self.songs[i].tags.length_ms.max(0)).sum();
            let sub = if self.view == View::Album {
                let artist = songs.first().map(|&i| self.songs[i].artist()).unwrap_or_default();
                format!("{artist}  \u{00b7}  {} song{}  \u{00b7}  {}", songs.len(), if songs.len() == 1 { "" } else { "s" }, library::fmt_duration(total))
            } else {
                format!("{} song{}  \u{00b7}  {}", songs.len(), if songs.len() == 1 { "" } else { "s" }, library::fmt_duration(total))
            };
            font::draw(c, cr.x + 128, top + 50, &sub, ui::BODY, Weight::Regular, ui::DIM);
            let b = Rect::new(cr.x + 128, top + 80, 110, ui::CONTROL_H);
            c.fill_round_rect(b, ui::RADIUS_S, ui::fill_for(chrome::accent_fill(ACCENT), self.state(b)));
            ui::symbol(c, "play", 0xFFFFFF, Rect::new(b.x + 14, b.y + 8, 16, 16));
            font::draw(c, b.x + 38, b.y + (b.h - font::line_height(ui::BODY)) / 2, "Play all", ui::BODY, Weight::Bold, 0xFFFFFF);
            self.hits.push((b, Hit::PlayAll(songs.clone())));
            let sb = Rect::new(b.x + 120, top + 80, 36, ui::CONTROL_H);
            ui::icon_button(c, sb, "shuffle", self.state(sb), false);
            self.hits.push((sb, Hit::PlayAll(shuffled(&songs))));
            top += 140;
        }
        let total_h = songs.len() as i32 * ROW_H + (top - area.y) + 24;
        self.scroll = self.scroll.min((total_h - area.h).max(0));
        for (k, &i) in songs.iter().enumerate() {
            let r = Rect::new(x0 - 8, top + k as i32 * ROW_H - self.scroll, area.w - 40, ROW_H - 2);
            if r.y + r.h < area.y || r.y > area.y + area.h {
                continue;
            }
            self.song_row(c, r, i, k, &songs);
        }
        ui::scrollbar(c, Rect::new(area.x + area.w - 10, area.y + 4, 6, area.h - 8), total_h, area.h, self.scroll);
    }

    fn draw_albums(&mut self, c: &mut Canvas) {
        let area = self.content();
        let albums = self.albums();
        let x0 = area.x + 24;
        let per = ((area.w - 48 + 24) / TILE_PITCH_X).max(1) as usize;
        let total_h = (albums.len().div_ceil(per)) as i32 * TILE_PITCH_Y + 40;
        self.scroll = self.scroll.min((total_h - area.h).max(0));
        for (k, (album, artist, n)) in albums.iter().enumerate() {
            let r = Rect::new(x0 + (k % per) as i32 * TILE_PITCH_X, area.y + 20 + (k / per) as i32 * TILE_PITCH_Y - self.scroll, TILE, TILE);
            if r.y + r.h + 50 < area.y || r.y > area.y + area.h {
                continue;
            }
            self.album_tile(c, r, album, artist, *n);
        }
        ui::scrollbar(c, Rect::new(area.x + area.w - 10, area.y + 4, 6, area.h - 8), total_h, area.h, self.scroll);
    }

    fn draw_artists(&mut self, c: &mut Canvas) {
        let area = self.content();
        let artists = self.artists();
        let x0 = area.x + 24;
        for (k, (artist, albums, n)) in artists.iter().enumerate() {
            let r = Rect::new(x0 - 8, area.y + 12 + k as i32 * 56 - self.scroll, area.w - 40, 52);
            if r.y + r.h < area.y || r.y > area.y + area.h {
                continue;
            }
            if self.state(r) != State::Normal {
                c.fill_round_rect(r, ui::RADIUS_S, ui::HOVER);
            }
            c.fill_circle(r.x + 30, r.y + 26, 20, blend(ui::PANEL, ACCENT, 40));
            ui::symbol(c, "user", blend(ui::PANEL, 0xFFFFFF, 140), Rect::new(r.x + 20, r.y + 16, 20, 20));
            font::draw_clipped(c, r.x + 64, r.y + 8, artist, ui::BODY, Weight::Bold, ui::TEXT, r.w - 100);
            font::draw(c, r.x + 64, r.y + 28, &format!("{albums} album{} \u{00b7} {n} song{}", if *albums == 1 { "" } else { "s" }, if *n == 1 { "" } else { "s" }), ui::CAPTION, Weight::Regular, ui::DIM);
            ui::symbol(c, "chevron-right", ui::FAINT, Rect::new(r.x + r.w - 28, r.y + 18, 16, 16));
            self.hits.push((r, Hit::Artist(artist.clone())));
        }
    }

    /// The now-playing bar across the bottom.
    fn draw_bar(&mut self, c: &mut Canvas) {
        let (w, h) = self.size();
        let bar = Rect::new(0, h - BAR_H, w, BAR_H);
        c.fill_rect(bar, ui::SIDEBAR);
        ui::divider(c, Rect::new(0, bar.y, w, 1));
        let (state, pos, len) = self.engine.as_ref().map_or((PlayState::Idle, 0, 0), |e| (e.state, e.position_ms(), e.length_ms));
        let playing = state == PlayState::Playing;
        // Left: cover, title, artist.
        let left_w = (w / 4).max(220);
        if let Some(i) = self.current {
            let (title, artist, album, path, has_art) = {
                let s = &self.songs[i];
                (s.title(), s.artist(), s.album(), s.entry.path.clone(), s.tags.has_art)
            };
            let art = if has_art { Thumbs::get(&path) } else { self.album_art(&album) };
            let ar = Rect::new(16, bar.y + 14, 56, 56);
            chrome::cover(c, ar, art.as_deref(), &album, ACCENT);
            font::draw_clipped(c, 84, bar.y + 22, &title, ui::BODY, Weight::Bold, ui::TEXT, left_w - 96);
            font::draw_clipped(c, 84, bar.y + 42, &artist, ui::CAPTION, Weight::Regular, ui::DIM, left_w - 96);
            let hit = Rect::new(8, bar.y + 8, left_w - 16, BAR_H - 16);
            self.hits.push((hit, Hit::Album(album)));
        } else {
            let ar = Rect::new(16, bar.y + 14, 56, 56);
            c.fill_round_rect(ar, ui::RADIUS, ui::PANEL);
            ui::symbol(c, "music", ui::FAINT, Rect::new(ar.x + 16, ar.y + 16, 24, 24));
            font::draw(c, 84, bar.y + 22, "Nothing playing", ui::BODY, Weight::Bold, ui::DIM);
            font::draw(c, 84, bar.y + 42, "Pick a song, or press Space", ui::CAPTION, Weight::Regular, ui::FAINT);
        }
        // Centre: transport and the seek bar.
        let cx = w / 2;
        let ty = bar.y + 14;
        let b = |x: i32| Rect::new(x - 16, ty, 32, 32);
        chrome::tool(c, b(cx - 112), "shuffle", self.state(b(cx - 112)), self.shuffle, ACCENT);
        self.hits.push((b(cx - 112), Hit::Shuffle));
        chrome::tool(c, b(cx - 64), "skip-back", if self.queue.len() > 1 || pos > 3000 { self.state(b(cx - 64)) } else { State::Disabled }, false, ACCENT);
        self.hits.push((b(cx - 64), Hit::Prev));
        let pr = Rect::new(cx - 20, ty - 4, 40, 40);
        chrome::play_disc(c, cx, ty + 16, 40, playing, self.state(pr), ACCENT);
        self.hits.push((pr, Hit::PlayPause));
        chrome::tool(c, b(cx + 64), "skip-forward", if self.qpos + 1 < self.queue.len() || self.repeat == Repeat::All { self.state(b(cx + 64)) } else { State::Disabled }, false, ACCENT);
        self.hits.push((b(cx + 64), Hit::Next));
        chrome::tool(c, b(cx + 112), if self.repeat == Repeat::One { "repeat-one" } else { "repeat" }, self.state(b(cx + 112)), self.repeat != Repeat::Off, ACCENT);
        self.hits.push((b(cx + 112), Hit::Repeat));
        let seek_w = (w / 2 - 80).max(200).min(520);
        let elapsed = fmt_time(pos);
        let total = fmt_time(len);
        let ew = font::width(&elapsed, 13, Weight::Regular);
        let tw = font::width(&total, 13, Weight::Regular);
        let seek = Rect::new(cx - seek_w / 2, bar.y + 54, seek_w, 20);
        chrome::time_text(c, seek.x - ew - 10, seek.y + 2, &elapsed, ui::DIM);
        chrome::time_text(c, seek.x + seek.w + 10, seek.y + 2, &total, ui::DIM);
        let hover = self.cursor.is_some_and(|(mx, my)| seek.contains(mx, my)) || self.drag == Drag::Seek;
        chrome::seek_bar(c, seek, pos, len, hover, ACCENT);
        self.hits.push((seek, Hit::Seek(seek)));
        let _ = tw;
        // Right: volume, equalizer, queue.
        let mut x = w - 16 - 32;
        for (glyph, hit, on) in [("queue", Hit::Pop(Pop::Queue), self.pop == Pop::Queue), ("sliders", Hit::Pop(Pop::Equalizer), self.pop == Pop::Equalizer || self.preset >= 0)] {
            let r = Rect::new(x, bar.y + 26, 32, 32);
            chrome::tool(c, r, glyph, self.state(r), on, ACCENT);
            self.hits.push((r, hit));
            x -= 36;
        }
        let vr = Rect::new(x - 76, bar.y + 35, 76, 14);
        chrome::volume_wedge(c, vr, self.volume, self.muted, ACCENT);
        self.hits.push((Rect::new(vr.x - 4, bar.y + 26, vr.w + 8, 32), Hit::Volume(vr)));
        let mr = Rect::new(vr.x - 32, bar.y + 28, 28, 28);
        chrome::tool(c, mr, if self.muted || self.volume == 0 { "volume-off" } else { "volume" }, self.state(mr), false, ACCENT);
        self.hits.push((mr, Hit::Mute));
    }

    fn draw_queue_pop(&mut self, c: &mut Canvas) {
        let (w, h) = self.size();
        let n = self.queue.len();
        let total: i64 = self.queue.iter().filter_map(|&i| self.songs.get(i)).map(|s| s.tags.length_ms.max(0)).sum();
        let rows = n.clamp(1, 10) as i32;
        let ph = 60 + rows * 40 + 16;
        let r = Rect::new(w - 380 - 12, h - BAR_H - ph - 10, 380, ph);
        chrome::popover(c, r);
        self.hits.push((r, Hit::Swallow));
        let head = format!("Queue \u{00b7} {n} song{} \u{00b7} {}", if n == 1 { "" } else { "s" }, library::fmt_duration(total));
        font::draw(c, r.x + 16, r.y + 18, &head, ui::HEADING, Weight::Bold, ui::TEXT);
        let mut bx = r.x + r.w - 16 - 28;
        for (glyph, hit, on) in [("minus", Hit::QueueClear, false), ("shuffle", Hit::Shuffle, self.shuffle), ("repeat", Hit::Repeat, self.repeat != Repeat::Off)] {
            let b = Rect::new(bx, r.y + 14, 28, 28);
            chrome::tool(c, b, glyph, self.state(b), on, ACCENT);
            self.hits.push((b, hit));
            bx -= 32;
        }
        let first = self.qpos.saturating_sub(4).min(n.saturating_sub(rows as usize));
        let playing = self.playing();
        for (k, &i) in self.queue.iter().enumerate().skip(first).take(rows as usize) {
            let row = Rect::new(r.x + 8, r.y + 58 + (k - first) as i32 * 40, r.w - 16, 38);
            let (title, artist, len) = self.songs.get(i).map(|s| (s.title(), s.artist(), s.tags.length_ms)).unwrap_or_default();
            chrome::queue_row(c, row, &format!("{title}  \u{00b7}  {artist}"), &if len > 1 { fmt_time(len) } else { String::new() }, k == self.qpos, playing, self.state(row), ACCENT);
            self.hits.push((row, Hit::QueueItem(k)));
        }
        if n == 0 {
            font::draw(c, r.x + 40, r.y + 68, "The queue is empty", ui::BODY, Weight::Regular, ui::DIM);
        }
    }

    fn draw_eq_pop(&mut self, c: &mut Canvas) {
        let (w, h) = self.size();
        let presets: Vec<String> = self.engine.as_ref().map(|e| e.presets.clone()).unwrap_or_default();
        let rows = (presets.len() as i32 + 1).min(12);
        let ph = 56 + rows * 32 + 12;
        let r = Rect::new(w - 280 - 56, h - BAR_H - ph - 10, 280, ph);
        chrome::popover(c, r);
        self.hits.push((r, Hit::Swallow));
        font::draw(c, r.x + 16, r.y + 16, "Equalizer", ui::HEADING, Weight::Bold, ui::TEXT);
        let mut y = r.y + 48;
        let row = Rect::new(r.x, y, r.w, 32);
        chrome::check_row(c, row, "Off", "", self.preset < 0, self.state(row), ACCENT);
        self.hits.push((row, Hit::Preset(-1)));
        y += 32;
        for (i, name) in presets.iter().enumerate().take(11) {
            let row = Rect::new(r.x, y, r.w, 32);
            chrome::check_row(c, row, name, "", self.preset == i as i32, self.state(row), ACCENT);
            self.hits.push((row, Hit::Preset(i as i32)));
            y += 32;
        }
        if presets.is_empty() {
            font::draw(c, r.x + 44, y + 6, "No presets (engine not running)", ui::CAPTION, Weight::Regular, ui::DIM);
        }
    }
}

/// `queue` rotated so playing starts at `start` while keeping the list order.
fn queue_from(queue: &[usize], start: usize) -> Vec<usize> {
    let mut v = queue.to_vec();
    if let Some(p) = v.iter().position(|&i| i == start) {
        v.rotate_left(p);
    }
    v
}

fn shuffled(songs: &[usize]) -> Vec<usize> {
    let mut v = songs.to_vec();
    for i in (1..v.len()).rev() {
        let j = (crate::rand::u64() % (i as u64 + 1)) as usize;
        v.swap(i, j);
    }
    v
}
