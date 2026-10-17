//! The media library: what is in the user's Videos, Music and Pictures folders (NovaFS)
//! plus the sample media the image may carry (`usr/share/nova/media/`), and the small
//! files the players remember between runs: resume positions, play history, decoded
//! thumbnails and song tags (all under `appdata/media/`).

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::gfx::Canvas;
use crate::sync::SpinLock;
use crate::{fs, rtc};

pub const VIDEO_EXT: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "m4v", "mpg", "mpeg", "ts", "ogv", "flv", "wmv", "3gp"];
pub const AUDIO_EXT: &[&str] = &["mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "wav", "wma", "aif", "aiff", "mka"];
pub const SAMPLES: &str = "usr/share/nova/media/";

pub fn ext_of(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

pub fn is_video(path: &str) -> bool {
    VIDEO_EXT.contains(&ext_of(path).as_str())
}

pub fn is_audio(path: &str) -> bool {
    AUDIO_EXT.contains(&ext_of(path).as_str())
}

/// A file name without its extension, underscores and dots turned into spaces.
pub fn display_name(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or(path);
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    stem.replace(['_', '.'], " ")
}

/// A folder's last component, "Samples" for the built-in media.
pub fn folder_name(path: &str) -> String {
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    if dir.starts_with(SAMPLES.trim_end_matches('/')) {
        return "Samples".into();
    }
    dir.rsplit('/').next().unwrap_or(dir).to_string()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub size: u64,
    pub mtime: u64,
}

/// Every file of a kind: `filter` tests the extension. User folders first (sorted by
/// path), then the samples.
pub fn scan(folders: &[&str], filter: fn(&str) -> bool) -> Vec<Entry> {
    let mut out = Vec::new();
    for folder in folders {
        for (path, size, mtime) in fs::list(folder) {
            if filter(&path) && !path.split('/').any(|p| p.starts_with('.')) {
                out.push(Entry { path, size, mtime });
            }
        }
    }
    out.sort_by(|a, b| a.path.to_lowercase().cmp(&b.path.to_lowercase()));
    let mut samples: Vec<Entry> = fs::system_list(SAMPLES).into_iter().filter(|p| filter(p)).map(|path| Entry { size: fs::system_file(&path).map_or(0, |d| d.len() as u64), mtime: 0, path }).collect();
    samples.sort_by(|a, b| a.path.cmp(&b.path));
    out.extend(samples);
    out
}

pub fn videos() -> Vec<Entry> {
    scan(&["Videos/", "videos/", "Movies/"], is_video)
}

pub fn songs() -> Vec<Entry> {
    scan(&["Music/", "music/"], is_audio)
}

pub fn pictures() -> Vec<Entry> {
    scan(&["Pictures/", "pictures/", "apps/assets/"], super::image::is_image_name)
}

/// Read a media file's bytes (NovaFS or a system file).
pub fn read(path: &str) -> Option<Vec<u8>> {
    fs::read(path).ok().or_else(|| fs::system_file(path).map(|d| d.to_vec()))
}

// ----- persistence --------------------------------------------------------------------

fn key(s: &str) -> String {
    // A short stable name for a path: FNV-1a in hex.
    let h = s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{h:016x}")
}

/// A tab-separated table file under appdata/media: path -> fields.
struct Table {
    file: &'static str,
    rows: BTreeMap<String, Vec<String>>,
}

impl Table {
    fn load(file: &'static str) -> Self {
        let mut rows = BTreeMap::new();
        if let Ok(text) = fs::read_string(&format!("appdata/media/{file}")) {
            for line in text.lines() {
                let mut it = line.split('\t');
                if let Some(p) = it.next() {
                    rows.insert(p.to_string(), it.map(String::from).collect());
                }
            }
        }
        Table { file, rows }
    }
    fn save(&self) {
        let mut out = String::new();
        for (p, f) in &self.rows {
            out.push_str(p);
            for v in f {
                out.push('\t');
                out.push_str(&v.replace(['\t', '\n'], " "));
            }
            out.push('\n');
        }
        let _ = fs::write(&format!("appdata/media/{}", self.file), out.as_bytes());
    }
}

/// Resume positions: where each video was stopped (ms) and how long it is.
pub struct Resume;

impl Resume {
    pub fn get(path: &str) -> Option<(i64, i64)> {
        let t = Table::load("resume.txt");
        let f = t.rows.get(path)?;
        Some((f.first()?.parse().ok()?, f.get(1)?.parse().ok()?))
    }
    /// Remember `pos` of `len`. Near the end (5 s, or a tenth of a short file) the video
    /// counts as watched and is forgotten; near the start an earlier position is kept
    /// (opening a video and leaving at once must not lose where it was stopped).
    pub fn set(path: &str, pos: i64, len: i64) {
        let mut t = Table::load("resume.txt");
        let margin = 5000.min(len / 10).max(500);
        if len > 0 && pos >= margin && pos <= len - margin {
            t.rows.insert(path.into(), alloc::vec![pos.to_string(), len.to_string(), rtc::unix_time().to_string()]);
        } else if len > 0 && pos >= margin {
            t.rows.remove(path);
        } else {
            return;
        }
        t.save();
    }
    /// Unfinished videos, most recently watched first: (path, pos, len).
    pub fn all() -> Vec<(String, i64, i64)> {
        let t = Table::load("resume.txt");
        let mut v: Vec<(String, i64, i64, u64)> = t
            .rows
            .iter()
            .filter_map(|(p, f)| Some((p.clone(), f.first()?.parse().ok()?, f.get(1)?.parse().ok()?, f.get(2).and_then(|s| s.parse().ok()).unwrap_or(0))))
            .collect();
        v.sort_by(|a, b| b.3.cmp(&a.3));
        v.into_iter().map(|(p, a, b, _)| (p, a, b)).collect()
    }
}

/// Play history: when each file was last played and how often.
pub struct History;

impl History {
    pub fn played(path: &str) {
        let mut t = Table::load("history.txt");
        let count = t.rows.get(path).and_then(|f| f.get(1)).and_then(|s| s.parse::<u32>().ok()).unwrap_or(0) + 1;
        t.rows.insert(path.into(), alloc::vec![rtc::unix_time().to_string(), count.to_string()]);
        if t.rows.len() > 200 {
            let oldest = t.rows.iter().min_by_key(|(_, f)| f.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)).map(|(p, _)| p.clone());
            if let Some(p) = oldest {
                t.rows.remove(&p);
            }
        }
        t.save();
    }
    /// Most recently played first: (path, when, count).
    pub fn recent(filter: fn(&str) -> bool) -> Vec<(String, u64, u32)> {
        let t = Table::load("history.txt");
        let mut v: Vec<(String, u64, u32)> = t
            .rows
            .iter()
            .filter(|(p, _)| filter(p))
            .map(|(p, f)| (p.clone(), f.first().and_then(|s| s.parse().ok()).unwrap_or(0), f.get(1).and_then(|s| s.parse().ok()).unwrap_or(1)))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v
    }
}

/// Song tags found by the engine: title, artist, album, length (ms), whether cover art
/// exists in the thumbnail cache.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub track: u32,
    pub length_ms: i64,
    pub has_art: bool,
}

pub struct TagCache;

impl TagCache {
    pub fn get(path: &str) -> Option<Tags> {
        let t = Table::load("tags.txt");
        let f = t.rows.get(path)?;
        Some(Tags {
            title: f.first().cloned().unwrap_or_default(),
            artist: f.get(1).cloned().unwrap_or_default(),
            album: f.get(2).cloned().unwrap_or_default(),
            track: f.get(3).and_then(|s| s.parse().ok()).unwrap_or(0),
            length_ms: f.get(4).and_then(|s| s.parse().ok()).unwrap_or(0),
            has_art: f.get(5).is_some_and(|s| s == "1"),
        })
    }
    pub fn set(path: &str, tags: &Tags) {
        let mut t = Table::load("tags.txt");
        t.rows.insert(path.into(), alloc::vec![tags.title.clone(), tags.artist.clone(), tags.album.clone(), tags.track.to_string(), tags.length_ms.to_string(), (tags.has_art as u8).to_string()]);
        t.save();
    }
}

/// Decoded thumbnails and cover art, as small PNGs in appdata/media/thumbs/ and in memory.
pub struct Thumbs;

static THUMBS: SpinLock<Vec<(String, Arc<Canvas>)>> = SpinLock::new(Vec::new());

impl Thumbs {
    fn file(path: &str) -> String {
        format!("appdata/media/thumbs/{}.png", key(path))
    }
    pub fn get(path: &str) -> Option<Arc<Canvas>> {
        if let Some((_, c)) = THUMBS.lock().iter().find(|(p, _)| p == path) {
            return Some(c.clone());
        }
        let c = Arc::new(Canvas::from_png(&fs::read(&Self::file(path)).ok()?).ok()?);
        Self::remember(path, c.clone());
        Some(c)
    }
    pub fn exists(path: &str) -> bool {
        THUMBS.lock().iter().any(|(p, _)| p == path) || fs::exists(&Self::file(path))
    }
    fn remember(path: &str, c: Arc<Canvas>) {
        let mut t = THUMBS.lock();
        t.retain(|(p, _)| p != path);
        if t.len() >= 120 {
            t.remove(0);
        }
        t.push((path.into(), c));
    }
    /// Keep `img` shrunk to at most `max_w` wide as the thumbnail of `path`.
    pub fn put(path: &str, img: &Canvas, max_w: i32) -> Arc<Canvas> {
        let (iw, ih) = (img.width as i32, img.height as i32);
        let (tw, th) = if iw > max_w { (max_w, (ih * max_w / iw.max(1)).max(1)) } else { (iw.max(1), ih.max(1)) };
        let mut small = Canvas::new(tw as usize, th as usize);
        small.blit_scaled(img, small.bounds(), 0, 255);
        let small = Arc::new(small);
        // Persist a bounded number of them (NovaFS is small in a live session).
        let have = fs::list("appdata/media/thumbs/").len();
        if have < 80 {
            let _ = fs::write(&Self::file(path), &small.to_png());
        }
        Self::remember(path, small.clone());
        small
    }
}

/// Settings the players share: volume, theme accent choice, first-run flags.
pub struct Prefs;

impl Prefs {
    pub fn get(name: &str) -> Option<String> {
        Table::load("prefs.txt").rows.get(name).and_then(|f| f.first().cloned())
    }
    pub fn set(name: &str, value: &str) {
        let mut t = Table::load("prefs.txt");
        t.rows.insert(name.into(), alloc::vec![value.into()]);
        t.save();
    }
    pub fn volume() -> i32 {
        Self::get("volume").and_then(|v| v.parse().ok()).unwrap_or(80)
    }
}

/// "3:07" or "1:02:30".
pub fn fmt_time(ms: i64) -> String {
    let s = (ms.max(0) + 500) / 1000;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// "47m", "1h 12m", "2 h" for row headers.
pub fn fmt_duration(ms: i64) -> String {
    if ms < 60_000 {
        return format!("{}s", (ms.max(0) + 999) / 1000);
    }
    let m = (ms + 30_000) / 60_000;
    if m >= 60 { format!("{}h {:02}m", m / 60, m % 60) } else { format!("{m}m") }
}

pub fn fmt_ago(t: u64) -> String {
    if t == 0 {
        return String::new();
    }
    let ago = rtc::unix_time().saturating_sub(t);
    match ago {
        0..120 => "just now".into(),
        120..3600 => format!("{} min ago", ago / 60),
        3600..86400 => format!("{} h ago", ago / 3600),
        _ => format!("{} days ago", ago / 86400),
    }
}
