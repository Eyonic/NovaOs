//! Videos: Nova's video player. A library (Home with Continue Watching, Recently added
//! and Folders; the Videos folder as a grid or list with breadcrumbs, sort and search;
//! Recent) and a player with one slim control bar that hides while you watch: previous,
//! play/pause, next, elapsed, seek bar, length, the volume wedge, then audio-track,
//! subtitles, settings and queue popovers and fullscreen. Resume positions are
//! remembered ("You already watched till 12:34 - Continue watching"), the first visit to
//! the player shows the keyboard shortcuts, and Shift+S saves a snapshot to Pictures.
//!
//! Decoding is the media engine (engine.rs); the frames it renders are scaled into the
//! window here, so the player works like any other native app (no window embedding).

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use super::chrome::{self, BAR_H, SIDEBAR_W, TOP_H, VIDEO_ACCENT as ACCENT};
use super::engine::{Engine, PlayState};
use super::library::{self, display_name, fmt_time, folder_name, Entry, History, Prefs, Resume, TagCache, Tags, Thumbs};
use crate::font::{self, Weight};
use crate::gfx::{blend, Canvas, Rect};
use crate::input::Key;
use crate::ui::{self, State};
use crate::wm::{self, Content, Event, WindowRef};
use crate::{fs, pit, task};

pub const ICON: char = '\u{25B6}';

/// Cards are at least this wide and stretch so a row fills the width.
const CARD_MIN_W: i32 = 180;
const CARD_GAP: i32 = 20;
/// Below a card's picture: its title and caption.
const CARD_TEXT_H: i32 = 76;
const ROW_H: i32 = 40;
const SPEEDS: [i32; 6] = [50, 75, 100, 125, 150, 200];
const HIDE_AFTER: u64 = 2500;

/// Open the Video Player; with a path, play it at once.
pub fn open(path: Option<String>) {
    let (sw, sh) = wm::work_area();
    let (w, h) = ((sw - 160).min(1080), (sh - 110).min(680));
    let win = wm::open("Videos", ICON, ACCENT, w, h, Content::Canvas(Canvas::new(w as usize, h as usize)));
    task::spawn("videos", move || {
        let mut app = Video::new(win.clone());
        if let Some(p) = path {
            app.play_path(&p, true);
        }
        app.run();
        app.leave_player(true);
        wm::close(&win);
    });
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Home,
    Library,
    Recent,
    Player,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pop {
    None,
    /// Tracks with the tab: 0 video, 1 audio, 2 subtitles.
    Tracks(usize),
    Settings,
    Queue,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    None,
    Seek,
    Volume,
    SubDelay,
    AudioDelay,
}

#[derive(Clone, PartialEq)]
enum Hit {
    Nav(View),
    Entry(String),
    Folder(String),
    Crumb(Option<String>),
    Search,
    ViewToggle,
    Sort,
    Back,
    PlayPause,
    Prev,
    Next,
    Seek(Rect),
    Volume(Rect),
    Mute,
    Pop(Pop),
    Fullscreen,
    Tab(usize),
    Track(char, i32),
    AddSub(String),
    Speed(i32),
    Slider(Drag, Rect),
    Loop,
    QueueItem(usize),
    QueueShuffle,
    QueueRepeat,
    QueueClear,
    Resume(i64),
    /// A Continue Watching card: play from the saved position.
    Continue(String, i64),
    Dismiss,
    Sheet,
    Replay,
    Screen,
    Swallow,
}

struct Video {
    win: WindowRef,
    engine: Option<Engine>,
    engine_error: Option<String>,
    view: View,
    back_view: View,
    all: Vec<Entry>,
    folder: Option<String>,
    grid: bool,
    by_name: bool,
    search: String,
    searching: bool,
    scroll: i32,
    selected: Option<usize>,
    fs_gen: u64,
    thumb_jobs: Vec<String>,
    queue: Vec<String>,
    qpos: usize,
    looping: bool,
    shuffled: bool,
    current: Option<String>,
    controls_until: u64,
    volume: i32,
    muted: bool,
    rate: i32,
    pop: Pop,
    sdelay: i64,
    adelay: i64,
    fullscreen: bool,
    resume_offer: Option<(i64, u64)>,
    sheet: bool,
    toast: Option<(String, u64)>,
    drag: Drag,
    cursor: Option<(i32, i32)>,
    held: bool,
    last_click: (u64, Option<Hit>),
    saved_at: u64,
    drawn_seq: u64,
    drawn_at: u64,
    drawn_state: PlayState,
    hits: Vec<(Rect, Hit)>,
    ended_handled: bool,
}

impl Video {
    fn new(win: WindowRef) -> Self {
        let mut v = Video {
            win,
            engine: None,
            engine_error: None,
            view: View::Home,
            back_view: View::Home,
            all: Vec::new(),
            folder: None,
            grid: true,
            by_name: true,
            search: String::new(),
            searching: false,
            scroll: 0,
            selected: None,
            fs_gen: 0,
            thumb_jobs: Vec::new(),
            queue: Vec::new(),
            qpos: 0,
            looping: false,
            shuffled: false,
            current: None,
            controls_until: 0,
            volume: Prefs::volume(),
            muted: false,
            rate: 100,
            pop: Pop::None,
            sdelay: 0,
            adelay: 0,
            fullscreen: false,
            resume_offer: None,
            sheet: false,
            toast: None,
            drag: Drag::None,
            cursor: None,
            held: false,
            last_click: (0, None),
            saved_at: 0,
            drawn_seq: 0,
            drawn_at: 0,
            drawn_state: PlayState::Idle,
            hits: Vec::new(),
            ended_handled: false,
        };
        v.rescan();
        v
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
        self.all = library::videos();
        self.thumb_jobs = self.all.iter().filter(|e| !Thumbs::exists(&e.path)).map(|e| e.path.clone()).collect();
    }

    fn folders(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        self.all.iter().map(|e| folder_name(&e.path)).filter(|f| seen.insert(f.clone())).collect()
    }

    /// The entries the Library view lists now.
    fn listed(&self) -> Vec<Entry> {
        let q = self.search.to_lowercase();
        let mut v: Vec<Entry> = self.all.iter().filter(|e| self.folder.as_ref().is_none_or(|f| &folder_name(&e.path) == f)).filter(|e| q.is_empty() || display_name(&e.path).to_lowercase().contains(&q)).cloned().collect();
        if self.by_name {
            v.sort_by_key(|e| display_name(&e.path).to_lowercase());
        } else {
            v.sort_by(|a, b| b.mtime.cmp(&a.mtime).then(a.path.cmp(&b.path)));
        }
        v
    }

    fn say(&mut self, text: &str) {
        self.toast = Some((text.into(), pit::ticks() + 3000));
    }

    // ----- playback -----

    /// Play `path`, queueing the rest of its folder after it.
    fn play_path(&mut self, path: &str, from_outside: bool) {
        self.play_path_from(path, from_outside, 0);
    }

    fn play_path_from(&mut self, path: &str, from_outside: bool, start_ms: i64) {
        let folder = folder_name(path);
        let mut q: Vec<String> = self.listed().into_iter().filter(|e| folder_name(&e.path) == folder).map(|e| e.path).collect();
        if !q.iter().any(|p| p == path) {
            q.insert(0, path.into());
        }
        if from_outside && !self.all.iter().any(|e| e.path == path) {
            q = alloc::vec![path.to_string()];
        }
        self.qpos = q.iter().position(|p| p == path).unwrap_or(0);
        self.queue = q;
        self.start_current(start_ms);
    }

    fn start_current(&mut self, start_ms: i64) {
        let Some(path) = self.queue.get(self.qpos).cloned() else { return };
        self.leave_player(false);
        if self.view != View::Player {
            self.back_view = self.view;
        }
        self.view = View::Player;
        self.pop = Pop::None;
        self.current = Some(path.clone());
        self.ended_handled = false;
        self.resume_offer = None;
        self.sdelay = 0;
        self.adelay = 0;
        self.controls_until = pit::ticks() + HIDE_AFTER + 1500;
        let volume = self.volume;
        let (muted, rate) = (self.muted, self.rate);
        if let Some(e) = self.engine() {
            e.open(&path, start_ms, false);
            e.set_volume(volume);
            e.set_mute(muted);
            if rate != 100 {
                e.set_rate(rate);
            }
        }
        History::played(&path);
        if start_ms == 0
            && let Some((pos, _)) = Resume::get(&path)
        {
            self.resume_offer = Some((pos, pit::ticks() + 9000));
        }
        if Prefs::get("video.sheet").is_none() {
            self.sheet = true;
        }
        wm::set_title(&self.win, &format!("{} - Videos", display_name(&path)));
    }

    /// Remember where we are and stop (when `stop`), e.g. when leaving the player.
    fn leave_player(&mut self, stop: bool) {
        if let (Some(path), Some(e)) = (&self.current, &mut self.engine) {
            let (pos, len) = (e.position_ms(), e.length_ms);
            if e.state == PlayState::Ended {
                Resume::set(path, len, len);
            } else if matches!(e.state, PlayState::Playing | PlayState::Paused) {
                Resume::set(path, pos, len);
            }
            if stop {
                e.stop();
            }
        }
        self.saved_at = pit::ticks();
    }

    fn step(&mut self, dir: i32) {
        if self.queue.is_empty() {
            return;
        }
        let n = self.queue.len() as i32;
        let next = self.qpos as i32 + dir;
        if next < 0 || next >= n {
            if self.looping {
                self.qpos = ((next % n) + n) as usize % n as usize;
            } else {
                return;
            }
        } else {
            self.qpos = next as usize;
        }
        self.start_current(0);
    }

    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        wm::set_fullscreen(&self.win, self.fullscreen);
        self.show_controls();
    }

    fn show_controls(&mut self) {
        self.controls_until = pit::ticks() + HIDE_AFTER;
    }

    fn controls_visible(&self) -> bool {
        let playing = self.engine.as_ref().is_some_and(|e| e.state == PlayState::Playing);
        !playing || pit::ticks() < self.controls_until || self.pop != Pop::None || self.drag != Drag::None || self.resume_offer.is_some()
    }

    fn snapshot(&mut self) {
        let Some(frame) = self.engine.as_ref().and_then(|e| e.frame.clone()) else { return };
        let name = self.current.as_deref().map(display_name).unwrap_or_else(|| "frame".into());
        let ms = self.engine.as_ref().map_or(0, |e| e.position_ms());
        let path = format!("Pictures/{} {}.png", name.replace(' ', "-"), fmt_time(ms).replace(':', "."));
        match fs::write(&path, &frame.to_png()) {
            Ok(()) => self.say("Snapshot saved to Pictures"),
            Err(e) => self.say(&format!("Snapshot failed: {e}")),
        }
    }

    fn cycle_track(&mut self, kind: char) {
        let Some(e) = self.engine.as_mut() else { return };
        let ids: Vec<i32> = e.tracks_of(kind).map(|t| t.id).collect();
        if ids.is_empty() {
            return;
        }
        let cur = if kind == 's' { e.subtitle_track } else { e.audio_track };
        let i = ids.iter().position(|&id| id == cur).map_or(0, |i| (i + 1) % ids.len());
        let id = ids[i];
        e.set_track(kind, id);
        let name = e.tracks_of(kind).find(|t| t.id == id).map(|t| t.name.clone()).unwrap_or_default();
        self.say(&format!("{}: {name}", if kind == 's' { "Subtitles" } else { "Audio" }));
    }

    fn set_volume(&mut self, v: i32) {
        self.volume = v.clamp(0, 200);
        self.muted = false;
        let vol = self.volume;
        if let Some(e) = self.engine.as_mut() {
            e.set_volume(vol);
            e.set_mute(false);
        }
        Prefs::set("volume", &vol.to_string());
    }

    fn seek_by(&mut self, ms: i64) {
        if let Some(e) = self.engine.as_mut() {
            let p = e.position_ms() + ms;
            e.seek(p);
        }
        self.show_controls();
    }

    /// Generate one missing thumbnail while nothing plays.
    fn idle_work(&mut self) {
        let playing = self.engine.as_ref().is_some_and(|e| matches!(e.state, PlayState::Playing | PlayState::Opening | PlayState::Paused));
        if playing || self.view == View::Player || self.engine_error.is_some() {
            return;
        }
        let Some(path) = self.thumb_jobs.pop() else { return };
        if Thumbs::exists(&path) {
            return;
        }
        if let Some(e) = self.engine() {
            let frame = e.thumbnail(&path, 20);
            let len = e.length_ms;
            if let Some(frame) = frame {
                Thumbs::put(&path, &frame, 384);
            }
            if len > 0 {
                TagCache::set(&path, &Tags { title: e.meta.get("title").cloned().unwrap_or_default(), length_ms: len, ..Default::default() });
            }
        }
    }

    // ----- events -----

    fn run(&mut self) {
        self.render();
        loop {
            let playing = self.engine.as_ref().is_some_and(|e| e.state == PlayState::Playing);
            let timeout = if self.view == View::Player && playing { pit::HZ / 60 } else if self.toast.is_some() || !self.thumb_jobs.is_empty() { pit::HZ / 10 } else { pit::HZ / 4 };
            let ev = wm::wait_event(&self.win, Some(timeout));
            let mut redraw = true;
            match ev {
                Some(Event::Close) => return,
                Some(Event::Key(k)) => self.key(k),
                Some(Event::MouseDown { x, y, button }) => {
                    self.held = true;
                    self.cursor = Some((x, y));
                    self.show_controls();
                    if button == 1 {
                        self.click(x, y);
                    }
                }
                Some(Event::MouseUp { x, y }) => {
                    self.held = false;
                    self.cursor = Some((x, y));
                    self.drag_to(x, y);
                    self.drag = Drag::None;
                }
                Some(Event::MouseMove { x, y }) => {
                    let was = self.cursor;
                    self.cursor = (x >= 0 && y >= 0).then_some((x, y));
                    if self.cursor != was && self.view == View::Player {
                        self.show_controls();
                    }
                    if self.held {
                        self.drag_to(x, y);
                    }
                }
                Some(Event::Wheel(n)) => {
                    if self.view == View::Player {
                        self.set_volume(self.volume - n * 5);
                        self.show_controls();
                    } else {
                        self.scroll = (self.scroll - n * 48).max(0);
                    }
                }
                Some(_) => {}
                None => {
                    redraw = false;
                    if fs::generation() != self.fs_gen {
                        self.rescan();
                        redraw = true;
                    }
                    if self.toast.as_ref().is_some_and(|(_, until)| pit::ticks() > *until) {
                        self.toast = None;
                        redraw = true;
                    }
                    if self.resume_offer.is_some_and(|(_, until)| pit::ticks() > until) {
                        self.resume_offer = None;
                        redraw = true;
                    }
                    self.idle_work();
                }
            }
            // Engine events: frames, time, state changes.
            let mut engine_changed = false;
            let mut ended = false;
            let mut save = false;
            if let Some(e) = self.engine.as_mut() {
                if !e.pump() {
                    self.engine_error = Some("the media engine stopped".into());
                    self.engine = None;
                    engine_changed = true;
                } else {
                    if e.frame_seq != self.drawn_seq && pit::ticks() - self.drawn_at >= 30 {
                        engine_changed = true; // at most ~30 frames a second reach the screen
                    }
                    if e.state != self.drawn_state || (self.view == View::Player && pit::ticks() - self.drawn_at >= 250) {
                        engine_changed = true; // the clock, the seek bar
                    }
                    ended = e.state == PlayState::Ended && !self.ended_handled;
                    save = e.state == PlayState::Playing && pit::ticks() - self.saved_at > 5000;
                }
            }
            if ended {
                self.ended_handled = true;
                if let (Some(p), Some(e)) = (&self.current, &self.engine) {
                    Resume::set(p, e.length_ms, e.length_ms);
                }
                if self.qpos + 1 < self.queue.len() || self.looping {
                    self.step(1);
                }
            }
            if save {
                self.leave_player(false);
            }
            if redraw || engine_changed {
                self.render();
            }
        }
    }

    fn key(&mut self, k: Key) {
        if self.sheet {
            if matches!(k, Key::Char(b'\n' | b' ' | 0x1B)) {
                self.sheet = false;
                Prefs::set("video.sheet", "seen");
            }
            return;
        }
        if self.searching && self.view != View::Player {
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
        if self.view == View::Player {
            self.show_controls();
            match k {
                Key::Char(b' ') => {
                    if let Some(e) = self.engine.as_mut() {
                        e.toggle();
                    }
                }
                Key::Char(b'f' | b'F') => self.toggle_fullscreen(),
                Key::Char(b'q' | b'Q') => self.pop = if self.pop == Pop::Queue { Pop::None } else { Pop::Queue },
                Key::Char(b'l' | b'L') => self.exit_player(),
                Key::Char(b'v') => self.cycle_track('s'),
                Key::Char(b'b') => self.cycle_track('a'),
                Key::Char(b'm') => {
                    self.muted = !self.muted;
                    let m = self.muted;
                    if let Some(e) = self.engine.as_mut() {
                        e.set_mute(m);
                    }
                }
                Key::Char(b'S') => self.snapshot(),
                Key::Char(b'n' | b'N') => self.step(1),
                Key::Char(b'p' | b'P') => self.step(-1),
                Key::Char(b'+' | b'=' | b']') => self.set_rate(1),
                Key::Char(b'-' | b'[') => self.set_rate(-1),
                Key::Char(c @ b'0'..=b'9') => {
                    if let Some(e) = self.engine.as_mut() {
                        let t = e.length_ms * (c - b'0') as i64 / 10;
                        e.seek(t);
                    }
                }
                Key::Left => self.seek_by(-10_000),
                Key::Right => self.seek_by(10_000),
                Key::Up => self.set_volume(self.volume + 5),
                Key::Down => self.set_volume(self.volume - 5),
                Key::Char(0x1B) => {
                    if self.pop != Pop::None {
                        self.pop = Pop::None;
                    } else if self.fullscreen {
                        self.toggle_fullscreen();
                    } else {
                        self.exit_player();
                    }
                }
                _ => {}
            }
            return;
        }
        let items = self.listed();
        let n = items.len();
        match k {
            Key::Char(0x06) => self.searching = true,
            Key::Char(0x0C) => self.grid = !self.grid,
            Key::Char(b'\n') => {
                if let Some(p) = self.selected.and_then(|i| items.get(i)).map(|e| e.path.clone()) {
                    self.play_path(&p, false);
                }
            }
            Key::Char(0x08 | 0x7F) => {
                if self.folder.is_some() {
                    self.folder = None;
                } else {
                    self.view = View::Home;
                }
            }
            Key::Char(0x1B) => self.selected = None,
            Key::Left | Key::Right | Key::Up | Key::Down if n > 0 => {
                if self.view != View::Library {
                    self.view = View::Library;
                }
                let cols = if self.grid { self.cards().0 } else { 1 };
                let i = self.selected.unwrap_or(0);
                let j = match k {
                    Key::Left => i.saturating_sub(1),
                    Key::Right => (i + 1).min(n - 1),
                    Key::Up => i.saturating_sub(cols),
                    _ => (i + cols).min(n - 1),
                };
                self.selected = Some(if self.selected.is_none() { 0 } else { j });
            }
            _ => {}
        }
    }

    fn set_rate(&mut self, dir: i32) {
        let i = SPEEDS.iter().position(|&s| s == self.rate).unwrap_or(2) as i32 + dir;
        self.rate = SPEEDS[i.clamp(0, SPEEDS.len() as i32 - 1) as usize];
        let r = self.rate;
        if let Some(e) = self.engine.as_mut() {
            e.set_rate(r);
        }
        self.say(&format!("Speed {}", speed_label(r)));
    }

    fn exit_player(&mut self) {
        self.leave_player(true);
        if self.fullscreen {
            self.toggle_fullscreen();
        }
        self.pop = Pop::None;
        self.view = if self.back_view == View::Player { View::Home } else { self.back_view };
        self.current = None;
        wm::set_title(&self.win, "Videos");
    }

    fn hit_at(&self, x: i32, y: i32) -> Option<Hit> {
        self.hits.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| h.clone())
    }

    fn click(&mut self, x: i32, y: i32) {
        let Some(hit) = self.hit_at(x, y) else { return };
        let now = pit::ticks();
        let double = self.last_click.1.as_ref() == Some(&hit) && now - self.last_click.0 < 400;
        self.last_click = (now, Some(hit.clone()));
        match hit {
            Hit::Swallow => {}
            Hit::Nav(v) => {
                self.view = v;
                self.folder = None;
                self.scroll = 0;
                self.selected = None;
                self.searching = false;
            }
            Hit::Entry(p) => self.play_path(&p, false),
            Hit::Continue(p, ms) => self.play_path_from(&p, false, ms),
            Hit::Folder(f) => {
                self.view = View::Library;
                self.folder = Some(f);
                self.scroll = 0;
            }
            Hit::Crumb(f) => {
                self.folder = f;
                self.scroll = 0;
            }
            Hit::Search => self.searching = true,
            Hit::ViewToggle => self.grid = !self.grid,
            Hit::Sort => self.by_name = !self.by_name,
            Hit::Back => self.exit_player(),
            Hit::PlayPause => {
                if let Some(e) = self.engine.as_mut() {
                    e.toggle();
                }
            }
            Hit::Replay => {
                if let Some(e) = self.engine.as_mut() {
                    e.seek(0);
                    e.play();
                }
                self.ended_handled = false;
            }
            Hit::Prev => {
                let pos = self.engine.as_ref().map_or(0, |e| e.position_ms());
                if pos > 4000 {
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
            Hit::Mute => {
                self.muted = !self.muted;
                let m = self.muted;
                if let Some(e) = self.engine.as_mut() {
                    e.set_mute(m);
                }
            }
            Hit::Pop(p) => self.pop = if self.pop == p { Pop::None } else { p },
            Hit::Fullscreen => self.toggle_fullscreen(),
            Hit::Tab(t) => self.pop = Pop::Tracks(t),
            Hit::Track(kind, id) => {
                if let Some(e) = self.engine.as_mut() {
                    e.set_track(kind, id);
                }
            }
            Hit::AddSub(p) => {
                if let Some(e) = self.engine.as_mut() {
                    e.add_subtitle(&p);
                }
                self.say(&format!("Added {}", p.rsplit('/').next().unwrap_or(&p)));
            }
            Hit::Speed(r) => {
                self.rate = r;
                if let Some(e) = self.engine.as_mut() {
                    e.set_rate(r);
                }
            }
            Hit::Slider(kind, track) => {
                self.drag = kind;
                self.slider_to(kind, track, x);
            }
            Hit::Loop => self.looping = !self.looping,
            Hit::QueueItem(i) => {
                if i < self.queue.len() {
                    self.qpos = i;
                    self.start_current(0);
                }
            }
            Hit::QueueShuffle => {
                self.shuffled = !self.shuffled;
                if self.shuffled && self.queue.len() > 2 {
                    let cur = self.queue.get(self.qpos).cloned();
                    let mut rest: Vec<String> = self.queue.iter().filter(|p| Some(*p) != cur.as_ref()).cloned().collect();
                    for i in (1..rest.len()).rev() {
                        let j = (crate::rand::u64() % (i as u64 + 1)) as usize;
                        rest.swap(i, j);
                    }
                    if let Some(c) = cur {
                        rest.insert(0, c);
                    }
                    self.queue = rest;
                    self.qpos = 0;
                } else if !self.shuffled {
                    let cur = self.queue.get(self.qpos).cloned();
                    self.queue.sort_by_key(|p| display_name(p).to_lowercase());
                    self.qpos = cur.and_then(|c| self.queue.iter().position(|p| *p == c)).unwrap_or(0);
                }
            }
            Hit::QueueRepeat => self.looping = !self.looping,
            Hit::QueueClear => {
                let cur = self.queue.get(self.qpos).cloned();
                self.queue = cur.into_iter().collect();
                self.qpos = 0;
            }
            Hit::Resume(ms) => {
                if let Some(e) = self.engine.as_mut() {
                    e.seek(ms);
                }
                self.resume_offer = None;
            }
            Hit::Dismiss => self.resume_offer = None,
            Hit::Sheet => {
                self.sheet = false;
                Prefs::set("video.sheet", "seen");
            }
            Hit::Screen => {
                if self.pop != Pop::None {
                    self.pop = Pop::None;
                } else if double {
                    self.toggle_fullscreen();
                    if let Some(e) = self.engine.as_mut() {
                        e.toggle(); // undo the single click's pause
                    }
                } else if let Some(e) = self.engine.as_mut() {
                    e.toggle();
                }
            }
        }
    }

    fn drag_to(&mut self, x: i32, _y: i32) {
        match self.drag {
            Drag::None => {}
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
            kind => {
                if let Some((_, Hit::Slider(_, track))) = self.hits.iter().find(|(_, h)| matches!(h, Hit::Slider(k, _) if *k == kind)).cloned() {
                    self.slider_to(kind, track, x);
                }
            }
        }
    }

    fn slider_to(&mut self, kind: Drag, track: Rect, x: i32) {
        let v = chrome::slider_value(track, x);
        let ms = ((v as i64 - 500) * 10 / 100) * 100; // -5 s .. +5 s in 100 ms steps
        match kind {
            Drag::SubDelay => {
                self.sdelay = ms;
                if let Some(e) = self.engine.as_mut() {
                    e.set_subtitle_delay(ms);
                }
            }
            Drag::AudioDelay => {
                self.adelay = ms;
                if let Some(e) = self.engine.as_mut() {
                    e.set_audio_delay(ms);
                }
            }
            _ => {}
        }
    }

    // ----- layout -----

    fn size(&self) -> (i32, i32) {
        wm::size(&self.win)
    }

    fn content(&self) -> Rect {
        let (w, h) = self.size();
        Rect::new(SIDEBAR_W, TOP_H, w - SIDEBAR_W, h - TOP_H)
    }

    /// Card grid of the content area: (columns, card width, card height); 16:9 cards
    /// between 24 px margins.
    fn cards(&self) -> (usize, i32, i32) {
        let avail = self.content().w - 48;
        let cols = ((avail + CARD_GAP) / (CARD_MIN_W + CARD_GAP)).max(1);
        let w = (avail - (cols - 1) * CARD_GAP) / cols;
        (cols as usize, w, w * 9 / 16)
    }

    fn state(&self, r: Rect) -> State {
        ui::state(self.cursor, self.held, r)
    }

    // ----- drawing -----

    fn render(&mut self) {
        let (w, h) = self.size();
        let mut c = Canvas::new(w as usize, h as usize);
        self.hits.clear();
        if self.view == View::Player {
            self.draw_player(&mut c);
        } else {
            c.clear(ui::BG);
            self.draw_sidebar(&mut c);
            self.draw_header(&mut c);
            match self.view {
                View::Home => self.draw_home(&mut c),
                View::Library => self.draw_library(&mut c),
                View::Recent => self.draw_recent(&mut c),
                View::Player => {}
            }
        }
        if let Some((text, _)) = &self.toast.clone() {
            let area = if self.view == View::Player { Rect::new(0, 0, w, h - BAR_H - 8) } else { self.content() };
            ui::toast(&mut c, area, text, false);
        }
        if self.sheet && self.view == View::Player {
            let rows: [(&str, &[&str]); 12] = [
                ("Play / Pause", &["Space"]),
                ("Toggle fullscreen", &["F"]),
                ("Seek back / forward", &["Left", "Right"]),
                ("Volume down / up", &["Down", "Up"]),
                ("Previous / next item", &["P", "N"]),
                ("Show / hide queue", &["Q"]),
                ("Back to the library", &["L"]),
                ("Cycle subtitle track", &["V"]),
                ("Cycle audio track", &["B"]),
                ("Mute", &["M"]),
                ("Slower / faster", &["-", "+"]),
                ("Take a snapshot", &["Shift", "S"]),
            ];
            let b = chrome::shortcuts_sheet(&mut c, Rect::new(0, 0, w, h), "Learn the basic keyboard shortcuts", &rows, self.cursor, ACCENT);
            self.hits.push((Rect::new(0, 0, w, h), Hit::Swallow));
            self.hits.push((b, Hit::Sheet));
        }
        wm::present(&self.win, &c);
    }

    fn draw_sidebar(&mut self, c: &mut Canvas) {
        let (_, h) = self.size();
        c.fill_rect(Rect::new(0, 0, SIDEBAR_W, h), ui::SIDEBAR);
        ui::divider(c, Rect::new(SIDEBAR_W - 1, 0, 1, h));
        let mut y = 18;
        y = ui::section_label(c, ui::PAD + 4, y, "Videos");
        let items: [(&str, &str, View); 3] = [("home", "Home", View::Home), ("film", "Library", View::Library), ("clock", "Recent", View::Recent)];
        for (glyph, label, view) in items {
            let r = Rect::new(ui::GAP, y, SIDEBAR_W - 2 * ui::GAP, ui::NAV_ROW_H);
            let active = self.view == view;
            ui::nav_row(c, r, Some((glyph, None)), label, active, self.state(r));
            if active {
                c.fill_round_rect(Rect::new(r.x, r.y + 8, 3, r.h - 16), 2, ACCENT);
            }
            self.hits.push((r, Hit::Nav(view)));
            y += ui::NAV_ROW_H + 2;
        }
        y += 14;
        y = ui::section_label(c, ui::PAD + 4, y, "Folders");
        for f in self.folders() {
            let r = Rect::new(ui::GAP, y, SIDEBAR_W - 2 * ui::GAP, ui::NAV_ROW_H);
            let active = self.view == View::Library && self.folder.as_deref() == Some(f.as_str());
            ui::nav_row(c, r, Some(("folder", None)), &f, active, self.state(r));
            self.hits.push((r, Hit::Folder(f.clone())));
            y += ui::NAV_ROW_H + 2;
            if y > h - 120 {
                break;
            }
        }
        // Now playing (paused in the background) card at the bottom.
        if let Some(p) = self.current.clone() {
            let r = Rect::new(ui::GAP, h - 70, SIDEBAR_W - 2 * ui::GAP, 58);
            c.fill_round_rect(r, ui::RADIUS, ui::fill_for(ui::PANEL, self.state(r)));
            let thumb = Thumbs::get(&p);
            let tr = Rect::new(r.x + 8, r.y + 8, 74, 42);
            match thumb {
                Some(t) => c.blit_scaled(&t, tr, 6, 255),
                None => c.fill_round_rect(tr, 6, ui::RAISED),
            }
            font::draw_clipped(c, r.x + 90, r.y + 10, &display_name(&p), 13, Weight::Bold, ui::TEXT, r.w - 98);
            font::draw(c, r.x + 90, r.y + 30, "Back to player", ui::CAPTION, Weight::Regular, ACCENT);
            self.hits.push((r, Hit::Nav(View::Player)));
        }
    }

    fn draw_header(&mut self, c: &mut Canvas) {
        let (w, _) = self.size();
        c.fill_rect(Rect::new(SIDEBAR_W, 0, w - SIDEBAR_W, TOP_H), ui::SIDEBAR);
        ui::divider(c, Rect::new(SIDEBAR_W, TOP_H - 1, w - SIDEBAR_W, 1));
        let x = SIDEBAR_W + 20;
        let mut parts: Vec<&str> = alloc::vec!["Videos"];
        let folder = self.folder.clone();
        match self.view {
            View::Home => parts = alloc::vec!["Home"],
            View::Recent => parts = alloc::vec!["Recent"],
            View::Library => {
                if let Some(f) = folder.as_deref() {
                    parts.push(f);
                }
            }
            View::Player => {}
        }
        let rects = chrome::crumbs(c, x, 18, &parts, self.cursor);
        if self.view == View::Library {
            if let Some(r) = rects.first() {
                self.hits.push((*r, Hit::Crumb(None)));
            }
            let n = self.listed().len();
            let note = format!("{n} item{}", if n == 1 { "" } else { "s" });
            let nx = rects.last().map_or(x, |r| r.x + r.w + 10);
            font::draw(c, nx, 22, &note, ui::CAPTION, Weight::Regular, ui::DIM);
            // Sort, view, search at the right.
            let sr = Rect::new(w - 296, (TOP_H - ui::CONTROL_H) / 2, 180, ui::CONTROL_H);
            ui::field_icon(c, sr, &self.search, "Search", self.searching, Some("search"));
            self.hits.push((sr, Hit::Search));
            let vt = Rect::new(w - 56, (TOP_H - ui::CONTROL_H) / 2, ui::CONTROL_H + 4, ui::CONTROL_H);
            ui::icon_button(c, vt, if self.grid { "grid" } else { "list" }, self.state(vt), false);
            self.hits.push((vt, Hit::ViewToggle));
            let st = Rect::new(w - 104, (TOP_H - ui::CONTROL_H) / 2, ui::CONTROL_H + 4, ui::CONTROL_H);
            ui::icon_button(c, st, "sort", self.state(st), false);
            self.hits.push((st, Hit::Sort));
        }
    }

    /// A horizontal row of cards for `items` (path, caption, progress); returns the y below.
    fn card_row(&mut self, c: &mut Canvas, y: i32, title: &str, note: &str, items: &[(String, String, Option<(i64, i64)>)]) -> i32 {
        let area = self.content();
        let x0 = area.x + 24;
        chrome::row_title(c, x0, y, area.w - 48, title, note);
        let y = y + 36;
        let (per, cw, ch) = self.cards();
        for (i, (path, caption, progress)) in items.iter().take(per).enumerate() {
            let r = Rect::new(x0 + i as i32 * (cw + CARD_GAP), y, cw, ch);
            let thumb = Thumbs::get(path);
            let hit = Rect::new(r.x - 6, r.y - 6, r.w + 12, r.h + 56);
            chrome::card(c, r, thumb.as_deref(), "film", &display_name(path), caption, *progress, self.state(hit), false, ACCENT);
            let target = match progress {
                Some((pos, _)) => Hit::Continue(path.clone(), *pos),
                None => Hit::Entry(path.clone()),
            };
            self.hits.push((hit, target));
        }
        y + ch + CARD_TEXT_H
    }

    fn draw_home(&mut self, c: &mut Canvas) {
        let area = self.content();
        let mut y = area.y + 24 - self.scroll;
        let cont: Vec<(String, String, Option<(i64, i64)>)> = Resume::all().into_iter().filter(|(p, ..)| self.all.iter().any(|e| &e.path == p)).take(8).map(|(p, pos, len)| (p, format!("{} left", library::fmt_duration(len - pos)), Some((pos, len)))).collect();
        if !cont.is_empty() {
            y = self.card_row(c, y, "Continue Watching", "", &cont);
        }
        let mut recent = self.all.clone();
        recent.sort_by(|a, b| b.mtime.cmp(&a.mtime).then(a.path.cmp(&b.path)));
        let recent: Vec<(String, String, Option<(i64, i64)>)> = recent.iter().take(8).map(|e| (e.path.clone(), caption_of(&e.path), None)).collect();
        if recent.is_empty() {
            let msg = if self.engine_error.is_some() { "Put videos in a Videos folder. The media engine is not in this image, so nothing can play yet." } else { "Put videos (mp4, mkv, webm, avi) in a Videos folder and they show up here." };
            ui::empty_state(c, area, "film", "No videos yet", msg);
            return;
        }
        y = self.card_row(c, y, "Recently added", &format!("{} videos", self.all.len()), &recent);
        // Folders as wide chips.
        let folders = self.folders();
        if !folders.is_empty() {
            let x0 = area.x + 24;
            chrome::row_title(c, x0, y, area.w - 48, "Folders", "");
            let y0 = y + 36;
            let per = ((area.w - 48 + 16) / 176).max(1) as usize;
            for (i, f) in folders.iter().enumerate() {
                let r = Rect::new(x0 + (i % per) as i32 * 176, y0 + (i / per) as i32 * 56, 160, 44);
                c.fill_round_rect(r, ui::RADIUS, ui::fill_for(ui::PANEL, self.state(r)));
                c.stroke_round_rect_w(r, ui::RADIUS, 1, ui::BORDER);
                ui::symbol(c, "folder", ACCENT, Rect::new(r.x + 12, r.y + 12, 20, 20));
                let n = self.all.iter().filter(|e| &folder_name(&e.path) == f).count();
                font::draw_clipped(c, r.x + 42, r.y + 5, f, 13, Weight::Bold, ui::TEXT, r.w - 50);
                font::draw(c, r.x + 42, r.y + 23, &format!("{n} video{}", if n == 1 { "" } else { "s" }), ui::CAPTION, Weight::Regular, ui::DIM);
                self.hits.push((r, Hit::Folder(f.clone())));
            }
        }
    }

    fn draw_library(&mut self, c: &mut Canvas) {
        let area = self.content();
        let items = self.listed();
        if items.is_empty() {
            if self.search.is_empty() {
                ui::empty_state(c, area, "film", "Nothing here", "This folder has no videos.");
            } else {
                ui::empty_state(c, area, "search", "No matches", &format!("Nothing is called \u{201c}{}\u{201d}.", self.search));
            }
            return;
        }
        let x0 = area.x + 24;
        let (cols, cw, ch) = self.cards();
        let total_h = if self.grid { (items.len().div_ceil(cols)) as i32 * (ch + CARD_TEXT_H) + 40 } else { items.len() as i32 * ROW_H + 60 };
        self.scroll = self.scroll.min((total_h - area.h).max(0));
        if !self.grid {
            let y = area.y + 12 - self.scroll;
            for (label, x) in [("Name", x0 + 44), ("Folder", area.x + area.w - 300), ("Size", area.x + area.w - 180), ("Watched", area.x + area.w - 100)] {
                font::draw(c, x, y, label, ui::CAPTION, Weight::Bold, ui::DIM);
            }
        }
        for (i, e) in items.iter().enumerate() {
            let sel = self.selected == Some(i);
            if self.grid {
                let r = Rect::new(x0 + (i % cols) as i32 * (cw + CARD_GAP), area.y + 20 + (i / cols) as i32 * (ch + CARD_TEXT_H) - self.scroll, cw, ch);
                if r.y + r.h + 50 < area.y || r.y > area.y + area.h {
                    continue;
                }
                let hit = Rect::new(r.x - 6, r.y - 6, r.w + 12, r.h + 56);
                let progress = Resume::get(&e.path);
                let thumb = Thumbs::get(&e.path);
                chrome::card(c, r, thumb.as_deref(), "film", &display_name(&e.path), &caption_of(&e.path), progress, self.state(hit), sel, ACCENT);
                self.hits.push((hit, Hit::Entry(e.path.clone())));
            } else {
                let r = Rect::new(x0 - 8, area.y + 40 + i as i32 * ROW_H - self.scroll, area.w - 40, ROW_H - 2);
                if r.y + r.h < area.y || r.y > area.y + area.h {
                    continue;
                }
                if sel {
                    c.fill_round_rect(r, ui::RADIUS_S, ui::SELECTION);
                } else if self.state(r) != State::Normal {
                    c.fill_round_rect(r, ui::RADIUS_S, ui::HOVER);
                } else if i % 2 == 1 {
                    c.fill_round_rect(r, ui::RADIUS_S, 0x191A24);
                }
                let ty = r.y + (r.h - font::line_height(ui::BODY)) / 2;
                let tr = Rect::new(r.x + 8, r.y + 4, 54, 30);
                match Thumbs::get(&e.path) {
                    Some(t) => c.blit_scaled(&t, tr, 4, 255),
                    None => {
                        c.fill_round_rect(tr, 4, ui::PANEL);
                        ui::symbol(c, "film", ui::FAINT, Rect::new(tr.x + 19, tr.y + 7, 16, 16));
                    }
                }
                font::draw_clipped(c, r.x + 72, ty, &display_name(&e.path), ui::BODY, Weight::Regular, ui::TEXT, area.w - 400);
                font::draw(c, area.x + area.w - 300, ty + 1, &folder_name(&e.path), 13, Weight::Regular, ui::DIM);
                font::draw(c, area.x + area.w - 180, ty + 1, &fmt_size(e.size), 13, Weight::Regular, ui::DIM);
                let watched = Resume::get(&e.path).map_or(String::new(), |(p, l)| if p >= l { "done".into() } else { format!("{}%", p * 100 / l.max(1)) });
                font::draw(c, area.x + area.w - 100, ty + 1, &watched, 13, Weight::Regular, ui::DIM);
                self.hits.push((r, Hit::Entry(e.path.clone())));
            }
        }
        ui::scrollbar(c, Rect::new(area.x + area.w - 10, area.y + 4, 6, area.h - 8), total_h, area.h, self.scroll);
    }

    fn draw_recent(&mut self, c: &mut Canvas) {
        let area = self.content();
        let recent = History::recent(library::is_video);
        if recent.is_empty() {
            ui::empty_state(c, area, "clock", "Nothing played yet", "Videos you play are listed here, newest first.");
            return;
        }
        let x0 = area.x + 24;
        for (i, (p, when, count)) in recent.iter().take(40).enumerate() {
            let r = Rect::new(x0 - 8, area.y + 16 + i as i32 * 52 - self.scroll, area.w - 40, 48);
            if r.y + r.h < area.y || r.y > area.y + area.h {
                continue;
            }
            c.fill_round_rect(r, ui::RADIUS_S, ui::fill_for(if i % 2 == 1 { 0x191A24 } else { ui::BG }, self.state(r)));
            let tr = Rect::new(r.x + 8, r.y + 5, 68, 38);
            match Thumbs::get(p) {
                Some(t) => c.blit_scaled(&t, tr, 4, 255),
                None => c.fill_round_rect(tr, 4, ui::PANEL),
            }
            font::draw_clipped(c, r.x + 88, r.y + 6, &display_name(p), ui::BODY, Weight::Bold, ui::TEXT, r.w - 200);
            let detail = format!("{}  \u{00b7}  played {} time{}", library::fmt_ago(*when), count, if *count == 1 { "" } else { "s" });
            font::draw(c, r.x + 88, r.y + 26, &detail, ui::CAPTION, Weight::Regular, ui::DIM);
            self.hits.push((r, Hit::Entry(p.clone())));
        }
    }

    // ----- the player -----

    fn draw_player(&mut self, c: &mut Canvas) {
        let (w, h) = self.size();
        c.clear(0x000000);
        let area = Rect::new(0, 0, w, h);
        self.hits.push((area, Hit::Screen));
        let (state, frame, pos, len, has_audio, error) = match &self.engine {
            Some(e) => (e.state, e.frame.clone(), e.position_ms(), e.length_ms, e.has_audio, e.error.clone()),
            None => (PlayState::Error, None, 0, 0, false, self.engine_error.clone()),
        };
        if let Some(e) = &self.engine {
            self.drawn_seq = e.frame_seq;
            self.drawn_state = e.state;
        }
        self.drawn_at = pit::ticks();
        match &frame {
            Some(f) => {
                let dst = fit_rect(area, f.width as i32, f.height as i32);
                draw_frame(c, f, dst);
            }
            None => {
                let title = self.current.as_deref().map(display_name).unwrap_or_default();
                if state == PlayState::Error || self.engine.is_none() {
                    ui::empty_state(c, area, "warning", "Cannot play this", error.as_deref().unwrap_or("The media engine reported an error."));
                } else if state == PlayState::Opening || state == PlayState::Idle {
                    ui::empty_state(c, area, "film", &title, "Opening\u{2026}");
                } else if has_audio || state == PlayState::Playing {
                    // Audio-only media: a big glyph and the title.
                    ui::empty_state(c, area, "audio", &title, "Audio");
                }
            }
        }
        let visible = self.controls_visible();
        if state == PlayState::Ended {
            // Replay / next overlay.
            c.blend_rect(area, 0, 120);
            let d = 72;
            let cx = w / 2;
            let cy = h / 2 - 20;
            let r = Rect::new(cx - d / 2, cy - d / 2, d, d);
            c.fill_circle(cx, cy, d / 2, ui::fill_for(ACCENT, self.state(r)));
            ui::symbol(c, "restart", 0xFFFFFF, Rect::new(cx - 16, cy - 16, 32, 32));
            self.hits.push((r, Hit::Replay));
            let t = "Watch again";
            let tw = font::width(t, ui::BODY, Weight::Bold);
            font::draw(c, cx - tw / 2, cy + d / 2 + 12, t, ui::BODY, Weight::Bold, ui::TEXT);
        }
        if !visible {
            return;
        }
        // Top scrim: back and title.
        let top = Rect::new(0, 0, w, 72);
        for y in 0..top.h {
            c.blend_rect(Rect::new(0, y, w, 1), 0, (160 * (top.h - y) / top.h) as u32);
        }
        let back = Rect::new(16, 16, 36, 36);
        let bs = self.state(back);
        if bs != State::Normal {
            c.fill_round_rect(back, 18, blend(0xFFFFFF, 0, 200));
        }
        ui::symbol(c, "back", 0xFFFFFF, Rect::new(back.x + 8, back.y + 8, 20, 20));
        self.hits.push((back, Hit::Back));
        let title = self.current.as_deref().map(display_name).unwrap_or_default();
        font::draw_clipped(c, 64, 24, &title, ui::HEADING, Weight::Bold, 0xFFFFFF, w - 200);
        if self.queue.len() > 1 {
            let t = format!("{} of {}", self.qpos + 1, self.queue.len());
            let tw = font::width(&t, ui::CAPTION, Weight::Regular);
            font::draw(c, w - 24 - tw, 28, &t, ui::CAPTION, Weight::Regular, blend(0xFFFFFF, 0, 60));
        }

        // Bottom scrim and the bar.
        let bar = Rect::new(0, h - BAR_H, w, BAR_H);
        for y in 0..BAR_H + 40 {
            let yy = h - BAR_H - 40 + y;
            c.blend_rect(Rect::new(0, yy, w, 1), 0, (190 * y / (BAR_H + 40)) as u32);
        }
        let cy = bar.y + BAR_H / 2;
        let mut x = 16;
        let b = |x: i32| Rect::new(x, cy - 16, 32, 32);
        chrome::tool(c, b(x), "skip-back", if self.queue.len() > 1 || pos > 4000 { self.state(b(x)) } else { State::Disabled }, false, ACCENT);
        self.hits.push((b(x), Hit::Prev));
        x += 36;
        let playing = state == PlayState::Playing;
        let pr = Rect::new(x, cy - 20, 40, 40);
        chrome::play_disc(c, x + 20, cy, 40, playing, self.state(pr), ACCENT);
        self.hits.push((pr, Hit::PlayPause));
        x += 48;
        chrome::tool(c, b(x), "skip-forward", if self.qpos + 1 < self.queue.len() || self.looping { self.state(b(x)) } else { State::Disabled }, false, ACCENT);
        self.hits.push((b(x), Hit::Next));
        x += 44;
        // Times and the seek bar.
        let elapsed = fmt_time(pos);
        let total = fmt_time(len);
        let ew = font::width(&elapsed, 13, Weight::Regular);
        chrome::time_text(c, x, cy - 8, &elapsed, ui::TEXT);
        x += ew + 12;
        let right_tools = 5;
        let tools_w = right_tools * 36 + 8;
        let vol_w = 86;
        let tw = font::width(&total, 13, Weight::Regular);
        let seek_end = w - 16 - tools_w - vol_w - 40 - tw - 12;
        let seek = Rect::new(x, cy - 10, (seek_end - x).max(60), 20);
        let seek_hover = self.cursor.is_some_and(|(mx, my)| seek.contains(mx, my)) || self.drag == Drag::Seek;
        chrome::seek_bar(c, seek, pos, len, seek_hover, ACCENT);
        self.hits.push((seek, Hit::Seek(seek)));
        if seek_hover && let Some((mx, _)) = self.cursor {
            let t = fmt_time(chrome::seek_value(seek, mx, len));
            let tw2 = font::width(&t, ui::CAPTION, Weight::Bold);
            let tip = Rect::new((mx - tw2 / 2 - 8).clamp(8, w - tw2 - 24), seek.y - 30, tw2 + 16, 22);
            c.fill_round_rect(tip, 6, ui::RAISED);
            font::draw(c, tip.x + 8, tip.y + 3, &t, ui::CAPTION, Weight::Bold, ui::TEXT);
        }
        x = seek.x + seek.w + 12;
        chrome::time_text(c, x, cy - 8, &total, ui::DIM);
        x += tw + 16;
        // Volume: glyph (mute) and the wedge.
        let mr = Rect::new(x, cy - 14, 28, 28);
        chrome::tool(c, mr, if self.muted || self.volume == 0 { "volume-off" } else { "volume" }, self.state(mr), false, ACCENT);
        self.hits.push((mr, Hit::Mute));
        x += 30;
        let vr = Rect::new(x, cy - 7, vol_w - 10, 14);
        chrome::volume_wedge(c, vr, self.volume, self.muted, ACCENT);
        let vhit = Rect::new(vr.x - 4, cy - 14, vr.w + 8, 28);
        self.hits.push((vhit, Hit::Volume(vr)));
        x += vol_w;
        // Tools at the right.
        let (has_subs, n_audio) = self.engine.as_ref().map_or((false, 0), |e| (e.tracks_of('s').count() > 1, e.tracks_of('a').count()));
        let tools: [(&str, Hit, bool); 5] = [
            ("audio", Hit::Pop(Pop::Tracks(1)), matches!(self.pop, Pop::Tracks(1)) || n_audio > 2),
            ("subtitles", Hit::Pop(Pop::Tracks(2)), matches!(self.pop, Pop::Tracks(2)) || (has_subs && self.engine.as_ref().is_some_and(|e| e.subtitle_track >= 0))),
            ("sliders", Hit::Pop(Pop::Settings), self.pop == Pop::Settings || self.rate != 100),
            ("queue", Hit::Pop(Pop::Queue), self.pop == Pop::Queue),
            ("fullscreen", Hit::Fullscreen, self.fullscreen),
        ];
        let mut tx = w - 16 - tools_w + 8;
        let _ = x;
        let mut anchors = Vec::new();
        for (glyph, hit, on) in tools {
            let r = b(tx);
            chrome::tool(c, r, glyph, self.state(r), on, ACCENT);
            anchors.push((hit.clone(), r));
            self.hits.push((r, hit));
            tx += 36;
        }
        // Resume toast.
        if let Some((ms, _)) = self.resume_offer
            && self.pop == Pop::None
        {
            let text = format!("You already watched till {}", fmt_time(ms));
            let tw = font::width(&text, 13, Weight::Regular);
            let bw = font::width("Continue watching", ui::CAPTION, Weight::Bold) + 20;
            let r = Rect::new((w - (tw + bw + 70)) / 2, h - BAR_H - 52, tw + bw + 70, 36);
            c.shadow_round(r, ui::RADIUS, 12, 110);
            c.fill_round_rect(r, ui::RADIUS, ui::RAISED);
            font::draw(c, r.x + 14, r.y + 9, &text, 13, Weight::Regular, ui::TEXT);
            let br = Rect::new(r.x + 14 + tw + 12, r.y + 6, bw, 24);
            c.fill_round_rect(br, 6, ui::fill_for(chrome::accent_fill(ACCENT), self.state(br)));
            font::draw(c, br.x + 10, br.y + 4, "Continue watching", ui::CAPTION, Weight::Bold, 0xFFFFFF);
            self.hits.push((r, Hit::Swallow));
            self.hits.push((br, Hit::Resume(ms)));
            let xr = Rect::new(r.x + r.w - 30, r.y + 8, 20, 20);
            ui::symbol(c, "close", ui::DIM, Rect::new(xr.x + 4, xr.y + 4, 12, 12));
            self.hits.push((xr, Hit::Dismiss));
        }
        // Popovers.
        let bounds = Rect::new(0, 0, w, h - BAR_H);
        match self.pop {
            Pop::None => {}
            Pop::Tracks(tab) => {
                let anchor = anchors.iter().find(|(h, _)| matches!(h, Hit::Pop(Pop::Tracks(_)))).map(|(_, r)| *r).unwrap_or(bar);
                self.draw_tracks_pop(c, anchor, tab, bounds);
            }
            Pop::Settings => {
                let anchor = anchors.iter().find(|(h, _)| *h == Hit::Pop(Pop::Settings)).map(|(_, r)| *r).unwrap_or(bar);
                self.draw_settings_pop(c, anchor, bounds);
            }
            Pop::Queue => {
                let anchor = anchors.iter().find(|(h, _)| *h == Hit::Pop(Pop::Queue)).map(|(_, r)| *r).unwrap_or(bar);
                self.draw_queue_pop(c, anchor, bounds);
            }
        }
    }

    fn draw_tracks_pop(&mut self, c: &mut Canvas, anchor: Rect, tab: usize, bounds: Rect) {
        let (video, audio, subs, cur_a, cur_s) = match &self.engine {
            Some(e) => (e.tracks_of('v').cloned().collect::<Vec<_>>(), e.tracks_of('a').cloned().collect::<Vec<_>>(), e.tracks_of('s').cloned().collect::<Vec<_>>(), e.audio_track, e.subtitle_track),
            None => (Vec::new(), Vec::new(), Vec::new(), -1, -1),
        };
        let sidecars: Vec<String> = if tab == 2 { self.sidecar_subtitles() } else { Vec::new() };
        let list = match tab {
            0 => &video,
            1 => &audio,
            _ => &subs,
        };
        let rows = list.len().max(1) as i32 + if tab == 2 { sidecars.len().max(1) as i32 } else { 0 };
        let h = 60 + 26 + rows * 36 + 12 + if tab == 2 { 34 } else { 0 };
        let r = chrome::popover_rect(anchor, 320, h.min(bounds.h - 16), bounds);
        chrome::popover(c, r);
        self.hits.push((r, Hit::Swallow));
        let tabs = chrome::tabs(c, Rect::new(r.x, r.y + 4, r.w, 44), &[("film", "Video"), ("audio", "Audio"), ("subtitles", "Subtitles")], tab, self.cursor, ACCENT);
        for (i, t) in tabs.iter().enumerate() {
            self.hits.push((*t, Hit::Tab(i)));
        }
        let mut y = r.y + 60;
        y = ui::section_label(c, r.x + 16, y, "Tracks");
        let kind = ['v', 'a', 's'][tab];
        let cur = match tab {
            1 => cur_a,
            2 => cur_s,
            _ => list.iter().find(|t| t.id >= 0).map_or(-1, |t| t.id),
        };
        if list.is_empty() {
            font::draw(c, r.x + 44, y + 8, "None in this file", ui::BODY, Weight::Regular, ui::DIM);
            y += 36;
        }
        for t in list {
            let row = Rect::new(r.x, y, r.w, 36);
            let name = if t.id < 0 { "None".to_string() } else { t.name.clone() };
            chrome::check_row(c, row, &name, "", t.id == cur, self.state(row), ACCENT);
            self.hits.push((row, Hit::Track(kind, t.id)));
            y += 36;
        }
        if tab == 2 {
            y += 8;
            y = ui::section_label(c, r.x + 16, y, "Add a subtitle file");
            if sidecars.is_empty() {
                font::draw(c, r.x + 44, y + 8, "Put .srt or .vtt files next to the video", ui::CAPTION, Weight::Regular, ui::DIM);
                y += 36;
            }
            for p in sidecars {
                let row = Rect::new(r.x, y, r.w, 36);
                ui::symbol(c, "plus", ACCENT, Rect::new(row.x + 16, row.y + 10, 16, 16));
                if self.state(row) != State::Normal {
                    c.fill_round_rect(Rect::new(row.x + 6, row.y, row.w - 12, row.h), ui::RADIUS_S, ui::HOVER);
                }
                font::draw_clipped(c, row.x + 44, row.y + 8, p.rsplit('/').next().unwrap_or(&p), ui::BODY, Weight::Regular, ui::TEXT, row.w - 60);
                self.hits.push((row, Hit::AddSub(p)));
                y += 36;
            }
        }
    }

    /// Subtitle files next to the current video.
    fn sidecar_subtitles(&self) -> Vec<String> {
        let Some(cur) = &self.current else { return Vec::new() };
        let dir = cur.rsplit_once('/').map_or(String::new(), |(d, _)| format!("{d}/"));
        let stem = display_name(cur).to_lowercase();
        let mut v: Vec<String> = fs::list(&dir).into_iter().map(|(p, ..)| p).chain(fs::system_list(&dir)).filter(|p| matches!(library::ext_of(p).as_str(), "srt" | "vtt" | "ass" | "ssa" | "sub")).collect();
        v.sort_by_key(|p| !display_name(p).to_lowercase().starts_with(&stem));
        v.dedup();
        v.truncate(6);
        v
    }

    fn draw_settings_pop(&mut self, c: &mut Canvas, anchor: Rect, bounds: Rect) {
        let r = chrome::popover_rect(anchor, 320, 262, bounds);
        chrome::popover(c, r);
        self.hits.push((r, Hit::Swallow));
        let mut y = r.y + 16;
        y = ui::section_label(c, r.x + 16, y, "Playback speed");
        let labels: Vec<String> = SPEEDS.iter().map(|&s| speed_label(s)).collect();
        let labels_ref: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        let active = SPEEDS.iter().position(|&s| s == self.rate).unwrap_or(2);
        let segs = chrome::segments(c, Rect::new(r.x + 16, y, r.w - 32, 32), &labels_ref, active, self.cursor, ACCENT);
        for (i, s) in segs.iter().enumerate() {
            self.hits.push((*s, Hit::Speed(SPEEDS[i])));
        }
        y += 48;
        let sfrac = ((self.sdelay + 5000) * 1000 / 10000) as i32;
        let track = chrome::slider(c, Rect::new(r.x + 16, y, r.w - 32, 44), "Subtitle delay", &delay_label(self.sdelay), sfrac, self.state(Rect::new(r.x + 16, y + 22, r.w - 32, 20)), ACCENT);
        self.hits.push((Rect::new(track.x - 8, track.y - 4, track.w + 16, track.h + 8), Hit::Slider(Drag::SubDelay, track)));
        y += 56;
        let afrac = ((self.adelay + 5000) * 1000 / 10000) as i32;
        let track = chrome::slider(c, Rect::new(r.x + 16, y, r.w - 32, 44), "Audio delay", &delay_label(self.adelay), afrac, self.state(Rect::new(r.x + 16, y + 22, r.w - 32, 20)), ACCENT);
        self.hits.push((Rect::new(track.x - 8, track.y - 4, track.w + 16, track.h + 8), Hit::Slider(Drag::AudioDelay, track)));
        y += 60;
        // Loop toggle.
        let row = Rect::new(r.x + 16, y, r.w - 32, 32);
        font::draw(c, row.x, row.y + 6, "Repeat the queue", ui::BODY, Weight::Regular, ui::TEXT);
        let sw = Rect::new(row.x + row.w - 40, row.y + 5, 40, 22);
        ui::switch(c, sw, self.looping, self.state(sw));
        self.hits.push((row, Hit::Loop));
    }

    fn draw_queue_pop(&mut self, c: &mut Canvas, anchor: Rect, bounds: Rect) {
        let n = self.queue.len();
        let total: i64 = self.queue.iter().map(|p| length_of(p)).sum();
        let rows = n.clamp(1, 9) as i32;
        let h = 60 + rows * 40 + 16;
        let r = chrome::popover_rect(anchor, 360, h.min(bounds.h - 16), bounds);
        chrome::popover(c, r);
        self.hits.push((r, Hit::Swallow));
        let head = if total > 0 { format!("{n} item{} \u{00b7} {}", if n == 1 { "" } else { "s" }, library::fmt_duration(total)) } else { format!("{n} item{}", if n == 1 { "" } else { "s" }) };
        font::draw(c, r.x + 16, r.y + 18, &head, ui::HEADING, Weight::Bold, ui::TEXT);
        let mut bx = r.x + r.w - 16 - 28;
        for (glyph, hit, on) in [("minus", Hit::QueueClear, false), ("shuffle", Hit::QueueShuffle, self.shuffled), ("repeat", Hit::QueueRepeat, self.looping)] {
            let b = Rect::new(bx, r.y + 14, 28, 28);
            chrome::tool(c, b, glyph, self.state(b), on, ACCENT);
            self.hits.push((b, hit));
            bx -= 32;
        }
        let playing = self.engine.as_ref().is_some_and(|e| e.state == PlayState::Playing);
        let first = self.qpos.saturating_sub(4).min(n.saturating_sub(rows as usize));
        for (i, p) in self.queue.iter().enumerate().skip(first).take(rows as usize) {
            let row = Rect::new(r.x + 8, r.y + 58 + (i - first) as i32 * 40, r.w - 16, 38);
            let len = length_of(p);
            let dur = if len > 0 { fmt_time(len) } else { String::new() };
            chrome::queue_row(c, row, &display_name(p), &dur, i == self.qpos, playing, self.state(row), ACCENT);
            self.hits.push((row, Hit::QueueItem(i)));
        }
        if n == 0 {
            font::draw(c, r.x + 40, r.y + 68, "The queue is empty", ui::BODY, Weight::Regular, ui::DIM);
        }
    }
}

/// A video's length from the resume table or the tag cache (0 if unknown yet).
fn length_of(path: &str) -> i64 {
    Resume::get(path).map(|(_, l)| l).or_else(|| TagCache::get(path).map(|t| t.length_ms)).unwrap_or(0)
}

/// "Samples · 5:00" under a card.
fn caption_of(path: &str) -> String {
    let len = length_of(path);
    if len > 0 { format!("{} \u{00b7} {}", folder_name(path), fmt_time(len)) } else { folder_name(path) }
}

fn speed_label(percent: i32) -> String {
    if percent % 100 == 0 { format!("{}x", percent / 100) } else { format!("{}.{:02}x", percent / 100, percent % 100).trim_end_matches('0').to_string() }
}

fn delay_label(ms: i64) -> String {
    if ms == 0 { "0 s".into() } else { format!("{}{}.{} s", if ms < 0 { "-" } else { "+" }, ms.abs() / 1000, ms.abs() % 1000 / 100) }
}

fn fmt_size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{}.{} GB", n >> 30, ((n & ((1 << 30) - 1)) * 10) >> 30),
        n if n >= 1 << 20 => format!("{}.{} MB", n >> 20, ((n & ((1 << 20) - 1)) * 10) >> 20),
        n if n >= 1 << 10 => format!("{} KB", n >> 10),
        n => format!("{n} bytes"),
    }
}

/// The largest rect of `w:h` proportions centred in `area`.
pub fn fit_rect(area: Rect, w: i32, h: i32) -> Rect {
    if w <= 0 || h <= 0 {
        return area;
    }
    let (fw, fh) = if w as i64 * area.h as i64 > h as i64 * area.w as i64 { (area.w, (area.w as i64 * h as i64 / w as i64) as i32) } else { ((area.h as i64 * w as i64 / h as i64) as i32, area.h) };
    Rect::new(area.x + (area.w - fw) / 2, area.y + (area.h - fh) / 2, fw.max(1), fh.max(1))
}

/// Draw a video frame into `dst`: box-filtered when shrinking, bilinear when enlarging.
pub fn draw_frame(c: &mut Canvas, f: &Canvas, dst: Rect) {
    if dst.w <= f.width as i32 && dst.h <= f.height as i32 {
        c.blit_scaled(f, dst, 0, 255);
    } else {
        let up = bilinear(f, dst.w, dst.h);
        c.blit(&up, up.bounds(), dst.x, dst.y);
    }
}

/// Enlarge `src` to `w`x`h` with bilinear sampling.
pub fn bilinear(src: &Canvas, w: i32, h: i32) -> Canvas {
    let (sw, sh) = (src.width as i64, src.height as i64);
    let mut out = Canvas::new(w.max(1) as usize, h.max(1) as usize);
    if sw == 0 || sh == 0 {
        return out;
    }
    let xs: Vec<(usize, usize, u32)> = (0..w as i64)
        .map(|x| {
            let p = ((2 * x + 1) * sw * 128 / w as i64 - 128).max(0);
            let x0 = ((p >> 8) as usize).min(sw as usize - 1);
            (x0, (x0 + 1).min(sw as usize - 1), (p & 255) as u32)
        })
        .collect();
    for y in 0..h as i64 {
        let p = ((2 * y + 1) * sh * 128 / h as i64 - 128).max(0);
        let y0 = ((p >> 8) as usize).min(sh as usize - 1);
        let y1 = (y0 + 1).min(sh as usize - 1);
        let fy = (p & 255) as u32;
        let r0 = &src.pixels[y0 * src.width..(y0 + 1) * src.width];
        let r1 = &src.pixels[y1 * src.width..(y1 + 1) * src.width];
        let row = &mut out.pixels[y as usize * w as usize..(y as usize + 1) * w as usize];
        for (d, &(x0, x1, fx)) in row.iter_mut().zip(&xs) {
            let top = blend(r0[x0], r0[x1], fx);
            let bottom = blend(r1[x0], r1[x1], fx);
            *d = blend(top, bottom, fy);
        }
    }
    out
}
