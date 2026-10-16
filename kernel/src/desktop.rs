//! Desktop entries: Linux GUI programs installed in the system image (such as the Craft
//! apps, `NOVA_CRAFT=1 scripts/build.sh`), described by freedesktop.org desktop entry
//! files in `usr/share/applications/*.desktop`. Home lists them, Files opens documents
//! with the program whose `MimeType` claims the file's type, and the window manager gives
//! their Wayland windows the program's icon.
//!
//! Read keys (Desktop Entry Specification 1.5): `Name`, `GenericName`, `Comment`, `Exec`
//! (field codes `%f %F %u %U` take the file; the others are dropped), `Icon`, `MimeType`,
//! `Categories`, `StartupWMClass`, `NoDisplay`, `Hidden`. Nova's own keys, written by the
//! image build: `X-Nova-Icon` (an icon in `usr/share/nova/icons/`), `X-Nova-Poster` (Home
//! artwork in `usr/share/nova/home/`). Without `X-Nova-Icon` the 128x128 hicolor icon is
//! used. The system image is read-only, so the entries are read once.
//!
//! Reference: https://specifications.freedesktop.org/desktop-entry-spec/latest/

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::sync::SpinLock;
use crate::{fs, gfx::Canvas, icons, process, task};

pub struct Entry {
    /// The file's name without `.desktop` (the desktop file ID).
    pub id: String,
    pub name: String,
    pub generic: String,
    pub comment: String,
    pub exec: String,
    /// The icon character for `icons::draw` (a private-use character mapped to the icon).
    pub icon: char,
    /// Accent colour: the icon's edge colour.
    pub color: u32,
    /// Home artwork (`usr/share/nova/home/<art>.png`).
    pub art: Option<&'static str>,
    pub mime: Vec<String>,
    pub categories: Vec<String>,
    pub wm_class: String,
}

const DIR: &str = "usr/share/applications/";

/// File extensions and their MIME types, for finding the program that opens a file. Only
/// types Nova has no app of its own for: text and Rhai files stay with Nova's apps.
const EXTENSIONS: &[(&str, &str)] = &[
    ("docx", "application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
    ("docm", "application/vnd.ms-word.document.macroEnabled.12"),
    ("dotx", "application/vnd.openxmlformats-officedocument.wordprocessingml.template"),
    ("doc", "application/msword"),
    ("odt", "application/vnd.oasis.opendocument.text"),
    ("rtf", "application/rtf"),
    ("xlsx", "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
    ("xlsm", "application/vnd.ms-excel.sheet.macroEnabled.12"),
    ("xlsb", "application/vnd.ms-excel.sheet.binary.macroEnabled.12"),
    ("ods", "application/vnd.oasis.opendocument.spreadsheet"),
    ("tsv", "text/tab-separated-values"),
    ("pptx", "application/vnd.openxmlformats-officedocument.presentationml.presentation"),
    ("potx", "application/vnd.openxmlformats-officedocument.presentationml.template"),
    ("ppsx", "application/vnd.openxmlformats-officedocument.presentationml.slideshow"),
    ("odp", "application/vnd.oasis.opendocument.presentation"),
    ("pdf", "application/pdf"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
    ("bmp", "image/bmp"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("psd", "image/vnd.adobe.photoshop"),
    ("ora", "image/openraster"),
    ("kra", "application/x-krita"),
    ("svg", "image/svg+xml"),
    ("wav", "audio/x-wav"),
    ("flac", "audio/flac"),
    ("mp3", "audio/mpeg"),
    ("ogg", "audio/ogg"),
    ("mp4", "video/mp4"),
    ("mkv", "video/x-matroska"),
    ("webm", "video/webm"),
];

static ENTRIES: SpinLock<Option<&'static [Entry]>> = SpinLock::new(None);

/// Every desktop entry to show, sorted by name (read once).
pub fn entries() -> &'static [Entry] {
    if let Some(e) = *ENTRIES.lock() {
        return e;
    }
    let mut v: Vec<Entry> = fs::list(DIR)
        .into_iter()
        .filter(|(name, ..)| name.ends_with(".desktop") && !name[DIR.len()..].contains('/'))
        .filter_map(|(name, ..)| parse(&name[DIR.len()..name.len() - 8], &fs::read_string(&name).ok()?))
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    let list: &'static [Entry] = Box::leak(v.into_boxed_slice());
    *ENTRIES.lock() = Some(list);
    list
}

fn parse(id: &str, text: &str) -> Option<Entry> {
    let mut in_entry = false;
    let mut keys: Vec<(&str, &str)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
        } else if in_entry && let Some((k, v)) = line.split_once('=') {
            // Localised keys (Name[de]=...) are skipped.
            keys.push((k.trim(), v.trim()));
        }
    }
    let get = |k: &str| keys.iter().find(|(key, _)| *key == k).map_or("", |(_, v)| *v);
    let list = |k: &str| get(k).split(';').filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>();
    if get("Type") != "Application" || get("Exec").is_empty() || get("NoDisplay") == "true" || get("Hidden") == "true" {
        return None;
    }
    let icon_path = match get("X-Nova-Icon") {
        "" => format!("usr/share/icons/hicolor/128x128/apps/{}.png", get("Icon")),
        name => format!("usr/share/nova/icons/{name}.png"),
    };
    let color = fs::system_file(&icon_path).and_then(|d| Canvas::from_png(d).ok()).map_or(0x5B6EE1, |c| edge_colour(&c));
    let art = match get("X-Nova-Poster") {
        "" => None,
        a => Some(&*Box::leak(String::from(a).into_boxed_str())),
    };
    Some(Entry {
        id: id.into(),
        name: get("Name").into(),
        generic: get("GenericName").into(),
        comment: get("Comment").into(),
        exec: get("Exec").into(),
        icon: icons::register(&icon_path),
        color,
        art,
        mime: list("MimeType"),
        categories: list("Categories"),
        wm_class: get("StartupWMClass").into(),
    })
}

/// The colour of an icon's tile: its pixel a little inside the top edge.
fn edge_colour(c: &Canvas) -> u32 {
    let (x, y) = (c.width / 2, c.height / 16);
    c.pixels.get(y * c.width + x).copied().unwrap_or(0x5B6EE1) & 0xFF_FFFF
}

/// The program that opens `path` (a NovaFS path), by its extension's MIME type.
pub fn handler_for(path: &str) -> Option<&'static Entry> {
    let name = path.rsplit('/').next()?;
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    let mime = EXTENSIONS.iter().find(|(e, _)| *e == ext)?.1;
    entries().iter().find(|e| e.mime.iter().any(|m| m == mime))
}

/// The entry whose window class or desktop file ID is `app_id` (a Wayland app ID).
pub fn by_app_id(app_id: &str) -> Option<&'static Entry> {
    if app_id.is_empty() {
        return None;
    }
    entries().iter().find(|e| e.wm_class == app_id || e.id == app_id)
}

/// The command line for `entry`, opening `file` (an absolute path) if given.
fn command(entry: &Entry, file: Option<&str>) -> Vec<String> {
    let mut argv = Vec::new();
    for word in entry.exec.split_whitespace() {
        match word {
            "%f" | "%F" | "%u" | "%U" => argv.extend(file.map(String::from)),
            w if w.starts_with('%') => {}
            w => argv.push(w.trim_matches('"').into()),
        }
    }
    argv
}

/// Start `entry`'s program, opening `file` (a NovaFS path; Linux programs see NovaFS at
/// the root) if given. The program is run by BusyBox `sh`, so launchers may be scripts.
pub fn launch(entry: &Entry, file: Option<&str>) -> Result<(), String> {
    let file = file.map(|f| format!("/{}", f.trim_start_matches('/')));
    let argv = command(entry, file.as_deref());
    if argv.is_empty() {
        return Err(format!("{}: empty Exec line", entry.id));
    }
    if !fs::exists("bin/sh") {
        return Err("no /bin/sh to start Linux programs with (BusyBox is not installed)".into());
    }
    // sh -c 'exec "$0" "$@"' program args...: the kernel runs scripts (#!) on exec.
    let mut args: Vec<String> = alloc::vec!["sh".into(), "-c".into(), "exec \"$0\" \"$@\"".into()];
    args.extend(argv);
    let name = entry.name.clone();
    task::spawn("launch", move || {
        if let Err(e) = process::spawn("bin/sh", args, Vec::new(), None) {
            crate::log::warn(&format!("{name}: {e}"));
        }
    });
    Ok(())
}
