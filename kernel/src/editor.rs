//! Text Editor: Nova's native code and text editor (a clean-room design in the spirit
//! of fast, minimal editors such as Zed and Sublime Text).
//!
//! A file sidebar (folders from the flat path names), tabs, line numbers, current-line
//! highlight, syntax colouring for common languages, mouse selection, the system
//! clipboard, undo/redo, an inline find bar (Ctrl-F) and a quick-open palette (Ctrl-P).
//!
//! Keys: Ctrl-S save, Ctrl-N new, Ctrl-W close tab, Ctrl-Z / Ctrl-Y undo / redo,
//! Ctrl-C / Ctrl-X / Ctrl-V copy / cut / paste, Ctrl-A select all, Ctrl-D duplicate
//! line, Ctrl-B toggle the sidebar, arrows, Home, End, Page Up / Down, Delete, Tab.
//! One editor window at a time: opening a file while it runs adds a tab.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::font::{self, Weight};
use crate::gfx::{Canvas, Rect};
use crate::input::Key;
use crate::sync::SpinLock;
use crate::ui;
use crate::wm::{self, Content, Event, WindowRef};
use crate::{fs, task};

const SIDEBAR_W: i32 = 220;
const TABS_H: i32 = 38;
const STATUS_H: i32 = 26;
const FIND_H: i32 = 44;
const CODE_SIZE: u16 = 14;
const UNDO_LIMIT: usize = 200;

/// Paths read by the running editor (Files and `launch` add to it).
static OPEN_QUEUE: SpinLock<Vec<String>> = SpinLock::new(Vec::new());
static RUNNING: AtomicBool = AtomicBool::new(false);
static WINDOW: SpinLock<Option<WindowRef>> = SpinLock::new(None);

/// Open the editor (with `path` in a tab), or add the tab to the running one.
pub fn open(path: Option<String>) {
    if let Some(p) = path {
        OPEN_QUEUE.lock().push(p);
    }
    if RUNNING.swap(true, Ordering::SeqCst) {
        if let Some(w) = WINDOW.lock().clone() {
            wm::launch(&wm::Action::Focus(w.lock().id));
            w.lock().dirty = true;
        }
        return;
    }
    let (sw, sh) = wm::work_area();
    let (w, h) = ((sw - 160).min(1040), (sh - 120).min(680));
    let win = wm::open("Text Editor", '\u{00b6}', 0xB0569C, w, h, Content::Canvas(Canvas::new(w as usize, h as usize)));
    *WINDOW.lock() = Some(win.clone());
    task::spawn("editor", move || {
        let mut ed = Editor::new(win.clone());
        ed.run();
        *WINDOW.lock() = None;
        RUNNING.store(false, Ordering::SeqCst);
        wm::close(&win);
    });
}

// ---------------------------------------------------------------------------
// Documents

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct Pos {
    row: usize,
    col: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum Lang {
    Rust,
    CLike,
    Rhai,
    Python,
    Shell,
    Markdown,
    Json,
    Plain,
}

impl Lang {
    fn of(path: &str) -> Self {
        match path.rsplit('.').next().unwrap_or("") {
            "rs" => Lang::Rust,
            "c" | "h" | "cpp" | "hpp" | "cc" | "js" | "ts" | "java" | "go" => Lang::CLike,
            "rhai" => Lang::Rhai,
            "py" => Lang::Python,
            "sh" | "bash" | "toml" | "conf" | "ini" => Lang::Shell,
            "md" => Lang::Markdown,
            "json" => Lang::Json,
            _ => Lang::Plain,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Lang::Rust => "Rust",
            Lang::CLike => "C-like",
            Lang::Rhai => "Rhai",
            Lang::Python => "Python",
            Lang::Shell => "Shell / config",
            Lang::Markdown => "Markdown",
            Lang::Json => "JSON",
            Lang::Plain => "Plain text",
        }
    }
    fn keywords(self) -> &'static [&'static str] {
        match self {
            Lang::Rust => &["fn", "let", "mut", "pub", "use", "mod", "struct", "enum", "impl", "trait", "for", "while", "loop", "if", "else", "match", "return", "break", "continue", "const", "static", "self", "Self", "crate", "super", "as", "in", "where", "unsafe", "async", "await", "move", "ref", "type", "dyn", "true", "false", "Some", "None", "Ok", "Err"],
            Lang::CLike => &["int", "char", "void", "long", "short", "unsigned", "signed", "float", "double", "struct", "enum", "union", "typedef", "static", "const", "return", "if", "else", "for", "while", "do", "switch", "case", "break", "continue", "default", "sizeof", "include", "define", "class", "public", "private", "new", "delete", "auto", "bool", "true", "false", "nullptr", "NULL", "function", "var", "let", "import", "export", "package", "func"],
            Lang::Rhai => &["fn", "let", "const", "if", "else", "for", "in", "while", "loop", "break", "continue", "return", "this", "true", "false", "switch", "throw", "try", "catch", "import", "export", "private"],
            Lang::Python => &["def", "class", "import", "from", "as", "if", "elif", "else", "for", "while", "return", "yield", "with", "try", "except", "finally", "raise", "pass", "break", "continue", "lambda", "None", "True", "False", "and", "or", "not", "in", "is", "self", "async", "await"],
            Lang::Shell => &["if", "then", "else", "elif", "fi", "for", "do", "done", "while", "case", "esac", "function", "return", "export", "local", "true", "false"],
            Lang::Json => &["true", "false", "null"],
            Lang::Markdown | Lang::Plain => &[],
        }
    }
    fn line_comment(self) -> Option<&'static str> {
        match self {
            Lang::Rust | Lang::CLike | Lang::Rhai => Some("//"),
            Lang::Python | Lang::Shell => Some("#"),
            _ => None,
        }
    }
    fn block_comments(self) -> bool {
        matches!(self, Lang::Rust | Lang::CLike | Lang::Rhai)
    }
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Pos,
}

struct Doc {
    path: Option<String>,
    name: String,
    lines: Vec<String>,
    cursor: Pos,
    /// Selection anchor (the other end is the cursor).
    anchor: Option<Pos>,
    /// Column the cursor wants when moving up and down.
    want_col: usize,
    scroll: usize,
    dirty: bool,
    lang: Lang,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Kind of the last edit (typing is grouped into one undo step).
    last_edit: u8,
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Byte index of character `col` in `s`.
fn byte_at(s: &str, col: usize) -> usize {
    s.char_indices().nth(col).map_or(s.len(), |(i, _)| i)
}

impl Doc {
    fn new(path: Option<String>, text: &str) -> Self {
        let lines: Vec<String> = if text.is_empty() { alloc::vec![String::new()] } else { text.replace('\t', "    ").replace('\r', "").split('\n').map(String::from).collect() };
        let name = path.as_deref().map_or("untitled".into(), |p| p.rsplit('/').next().unwrap_or(p).to_string());
        let lang = path.as_deref().map_or(Lang::Plain, Lang::of);
        Self { path, name, lines, cursor: Pos { row: 0, col: 0 }, anchor: None, want_col: 0, scroll: 0, dirty: false, lang, undo: Vec::new(), redo: Vec::new(), last_edit: 0 }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn line_len(&self, row: usize) -> usize {
        char_len(&self.lines[row])
    }

    fn selection(&self) -> Option<(Pos, Pos)> {
        let a = self.anchor?;
        (a != self.cursor).then(|| if a < self.cursor { (a, self.cursor) } else { (self.cursor, a) })
    }

    fn selected_text(&self) -> Option<String> {
        let (a, b) = self.selection()?;
        let mut out = String::new();
        for row in a.row..=b.row {
            let line = &self.lines[row];
            let from = if row == a.row { byte_at(line, a.col) } else { 0 };
            let to = if row == b.row { byte_at(line, b.col) } else { line.len() };
            out.push_str(&line[from..to]);
            if row != b.row {
                out.push('\n');
            }
        }
        Some(out)
    }

    /// Record an undo step before an edit of `kind` (1 typing, 2 other); typing in a row
    /// is one step.
    fn checkpoint(&mut self, kind: u8) {
        if kind == 1 && self.last_edit == 1 {
            return;
        }
        self.last_edit = kind;
        self.undo.push(Snapshot { lines: self.lines.clone(), cursor: self.cursor });
        if self.undo.len() > UNDO_LIMIT {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn delete_selection(&mut self) -> bool {
        let Some((a, b)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let tail = self.lines[b.row][byte_at(&self.lines[b.row], b.col)..].to_string();
        let head_end = byte_at(&self.lines[a.row], a.col);
        self.lines[a.row].truncate(head_end);
        self.lines[a.row].push_str(&tail);
        self.lines.drain(a.row + 1..=b.row);
        self.cursor = a;
        self.anchor = None;
        self.dirty = true;
        true
    }

    fn insert(&mut self, text: &str) {
        self.delete_selection();
        let mut parts = text.split('\n');
        let row = self.cursor.row;
        let at = byte_at(&self.lines[row], self.cursor.col);
        let tail = self.lines[row][at..].to_string();
        self.lines[row].truncate(at);
        let first = parts.next().unwrap_or("");
        self.lines[row].push_str(first);
        let mut r = row;
        let mut col = self.cursor.col + char_len(first);
        for part in parts {
            r += 1;
            self.lines.insert(r, part.to_string());
            col = char_len(part);
        }
        self.lines[r].push_str(&tail);
        self.cursor = Pos { row: r, col };
        self.want_col = col;
        self.dirty = true;
    }

    fn newline(&mut self) {
        let line = &self.lines[self.cursor.row];
        let mut indent: String = line.chars().take_while(|c| *c == ' ').collect();
        let before = &line[..byte_at(line, self.cursor.col)];
        if before.trim_end().ends_with(['{', '(', '[', ':']) {
            indent.push_str("    ");
        }
        self.insert(&format!("\n{indent}"));
    }

    fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let Pos { row, col } = self.cursor;
        if col > 0 {
            let line = &self.lines[row];
            // In leading indentation, delete back to the previous multiple of 4.
            let n = if line[..byte_at(line, col)].chars().all(|c| c == ' ') { ((col - 1) % 4) + 1 } else { 1 };
            let from = byte_at(line, col - n);
            let to = byte_at(line, col);
            self.lines[row].replace_range(from..to, "");
            self.cursor.col -= n;
        } else if row > 0 {
            let line = self.lines.remove(row);
            let prev_len = self.line_len(row - 1);
            self.lines[row - 1].push_str(&line);
            self.cursor = Pos { row: row - 1, col: prev_len };
        }
        self.want_col = self.cursor.col;
        self.dirty = true;
    }

    fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let Pos { row, col } = self.cursor;
        if col < self.line_len(row) {
            let line = &self.lines[row];
            let (from, to) = (byte_at(line, col), byte_at(line, col + 1));
            self.lines[row].replace_range(from..to, "");
        } else if row + 1 < self.lines.len() {
            let next = self.lines.remove(row + 1);
            self.lines[row].push_str(&next);
        }
        self.dirty = true;
    }

    fn clamp(&mut self) {
        self.cursor.row = self.cursor.row.min(self.lines.len() - 1);
        self.cursor.col = self.cursor.col.min(self.line_len(self.cursor.row));
    }

    fn move_to(&mut self, p: Pos, select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = p;
        self.clamp();
        self.last_edit = 0;
    }
}

// ---------------------------------------------------------------------------
// Syntax colouring

const C_TEXT: u32 = 0xDCDEEA;
const C_KEYWORD: u32 = 0xB89CFF;
const C_STRING: u32 = 0x8FD694;
const C_COMMENT: u32 = 0x6A6E85;
const C_NUMBER: u32 = 0xF2C46D;
const C_FUNCTION: u32 = 0x7AA8FF;
const C_TYPE: u32 = 0x5FD3D0;
const C_HEADING: u32 = 0xB89CFF;

/// Colours for each character of `line`. `in_block` is true when the line starts
/// inside a /* */ comment; returns whether it ends inside one.
fn highlight(line: &str, lang: Lang, mut in_block: bool, out: &mut Vec<u32>) -> bool {
    let chars: Vec<char> = line.chars().collect();
    out.clear();
    out.resize(chars.len(), C_TEXT);
    if lang == Lang::Markdown {
        let t = line.trim_start();
        let color = if t.starts_with('#') { C_HEADING } else if t.starts_with("```") { C_COMMENT } else if t.starts_with(['-', '*', '>']) { C_FUNCTION } else { C_TEXT };
        out.fill(color);
        return false;
    }
    let keywords = lang.keywords();
    let lc: Vec<char> = lang.line_comment().map(|s| s.chars().collect()).unwrap_or_default();
    let mut i = 0;
    while i < chars.len() {
        if in_block {
            let start = i;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            let end = (i + 2).min(chars.len());
            out[start..end].fill(C_COMMENT);
            if i < chars.len() {
                in_block = false;
            }
            i = end;
            continue;
        }
        let c = chars[i];
        if !lc.is_empty() && chars[i..].starts_with(&lc) {
            out[i..].fill(C_COMMENT);
            break;
        }
        if lang.block_comments() && c == '/' && chars.get(i + 1) == Some(&'*') {
            in_block = true;
            continue;
        }
        if c == '"' || c == '\'' || c == '`' {
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != c {
                if chars[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            let end = (i + 1).min(chars.len());
            // Rust lifetimes ('a) are not strings.
            if !(c == '\'' && lang == Lang::Rust && end - start > 3 && chars.get(start + 2) != Some(&'\'')) {
                out[start..end].fill(C_STRING);
            }
            i = end;
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_') {
                i += 1;
            }
            out[start..i].fill(C_NUMBER);
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let next = chars[i..].iter().find(|c| **c != ' ');
            let color = if keywords.contains(&word.as_str()) {
                C_KEYWORD
            } else if next == Some(&'(') || next == Some(&'!') {
                C_FUNCTION
            } else if word.starts_with(|c: char| c.is_ascii_uppercase()) && lang != Lang::Plain {
                C_TYPE
            } else {
                C_TEXT
            };
            out[start..i].fill(color);
            continue;
        }
        i += 1;
    }
    in_block
}

/// A small disclosure triangle (pointing right, or down when `open`) at (x, y).
pub fn disclosure(c: &mut Canvas, x: i32, y: i32, open: bool, color: u32) {
    for i in 0..5 {
        if open {
            c.fill_rect(Rect::new(x + i, y + 2 + i, 10 - 2 * i, 1), color);
        } else {
            c.fill_rect(Rect::new(x + 2 + i, y + i, 1, 10 - 2 * i), color);
        }
    }
}

// ---------------------------------------------------------------------------
// The editor

#[derive(PartialEq)]
enum Overlay {
    None,
    Find,
    QuickOpen,
}

struct Editor {
    win: WindowRef,
    docs: Vec<Doc>,
    active: usize,
    sidebar: bool,
    expanded: BTreeSet<String>,
    overlay: Overlay,
    query: String,
    pick: usize,
    message: String,
    /// Last click (for double-click word selection): (tick, position).
    last_click: (u64, Pos),
    dragging: bool,
}

impl Editor {
    fn new(win: WindowRef) -> Self {
        let mut expanded = BTreeSet::new();
        expanded.insert("apps".into());
        expanded.insert("notes".into());
        Self { win, docs: Vec::new(), active: 0, sidebar: true, expanded, overlay: Overlay::None, query: String::new(), pick: 0, message: String::new(), last_click: (0, Pos { row: 0, col: 0 }), dragging: false }
    }

    fn doc(&mut self) -> &mut Doc {
        if self.docs.is_empty() {
            self.docs.push(Doc::new(None, ""));
            self.active = 0;
        }
        let i = self.active.min(self.docs.len() - 1);
        &mut self.docs[i]
    }

    fn open_path(&mut self, path: &str) {
        if let Some(i) = self.docs.iter().position(|d| d.path.as_deref() == Some(path)) {
            self.active = i;
            return;
        }
        match fs::read(path) {
            Ok(bytes) => {
                if bytes.iter().take(4096).any(|&b| b == 0) {
                    self.message = format!("{path} is a binary file");
                    return;
                }
                let text = String::from_utf8_lossy(&bytes).into_owned();
                // Replace an untouched empty untitled tab.
                if self.docs.len() == 1 && self.docs[0].path.is_none() && !self.docs[0].dirty && self.docs[0].text().is_empty() {
                    self.docs.clear();
                }
                self.docs.push(Doc::new(Some(path.into()), &text));
                self.active = self.docs.len() - 1;
            }
            Err(e) => self.message = format!("could not open {path}: {e}"),
        }
    }

    fn save(&mut self) {
        let d = self.doc();
        if d.path.is_none() {
            let mut n = 1;
            while fs::exists(&format!("notes/untitled-{n}.txt")) {
                n += 1;
            }
            let p = format!("notes/untitled-{n}.txt");
            d.name = p.rsplit('/').next().unwrap_or(&p).into();
            d.path = Some(p);
        }
        let path = d.path.clone().unwrap_or_default();
        let text = d.text();
        match fs::write(&path, text.as_bytes()) {
            Ok(()) => {
                self.doc().dirty = false;
                self.message = format!("Saved {path}");
            }
            Err(e) => self.message = format!("Could not save: {e}"),
        }
    }

    fn close_tab(&mut self, i: usize) {
        if i < self.docs.len() {
            self.docs.remove(i);
            self.active = self.active.min(self.docs.len().saturating_sub(1));
        }
    }

    /// Files shown in the sidebar and quick open: the user's files and the bundled
    /// apps (not the read-only libraries and engine).
    fn files() -> Vec<String> {
        let hidden = ["usr/", "lib/", "lib64/", "opt/", "etc/", "bin/", "var/", "tmp/", "appdata/", "system/shields/"];
        fs::list("").into_iter().map(|(n, ..)| n).filter(|n| !hidden.iter().any(|h| n.starts_with(h)) && !n.split('/').any(|p| p.starts_with('.'))).collect()
    }

    // ----- layout -----

    fn size(&self) -> (i32, i32) {
        wm::size(&self.win)
    }

    fn cell() -> (i32, i32) {
        font::mono_cell(CODE_SIZE).unwrap_or((8, 16))
    }

    fn text_area(&self) -> Rect {
        let (w, h) = self.size();
        let x = if self.sidebar { SIDEBAR_W } else { 0 };
        let top = TABS_H + if self.overlay == Overlay::Find { FIND_H } else { 0 };
        Rect::new(x, top, w - x, h - top - STATUS_H)
    }

    fn gutter_w(&mut self) -> i32 {
        let digits = self.doc().lines.len().to_string().len().max(3) as i32;
        digits * Self::cell().0 + 28
    }

    fn visible_rows(&self) -> usize {
        (self.text_area().h / Self::cell().1).max(1) as usize
    }

    fn ensure_cursor_visible(&mut self) {
        let rows = self.visible_rows();
        let d = self.doc();
        if d.cursor.row < d.scroll {
            d.scroll = d.cursor.row;
        } else if d.cursor.row >= d.scroll + rows {
            d.scroll = d.cursor.row + 1 - rows;
        }
    }

    fn pos_at(&mut self, x: i32, y: i32) -> Pos {
        let area = self.text_area();
        let gutter = self.gutter_w();
        let (cw, ch) = Self::cell();
        let d = self.doc();
        let row = (d.scroll + ((y - area.y).max(0) / ch) as usize).min(d.lines.len() - 1);
        let col = (((x - area.x - gutter + cw / 2).max(0)) / cw) as usize;
        Pos { row, col: col.min(d.line_len(row)) }
    }

    // ----- sidebar tree -----

    /// Visible sidebar entries: (depth, label, full path, is_folder).
    fn tree(&self) -> Vec<(usize, String, String, bool)> {
        let mut out = Vec::new();
        let mut seen_dirs = BTreeSet::new();
        for f in Self::files() {
            let parts: Vec<&str> = f.split('/').collect();
            // Every ancestor folder must be expanded for the entry to show.
            let mut prefix = String::new();
            let mut visible = true;
            for (depth, part) in parts.iter().enumerate().take(parts.len() - 1) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                if !visible {
                    break;
                }
                if seen_dirs.insert(prefix.clone()) {
                    out.push((depth, (*part).to_string(), prefix.clone(), true));
                }
                visible = self.expanded.contains(&prefix);
            }
            if visible {
                out.push((parts.len() - 1, parts[parts.len() - 1].to_string(), f.clone(), false));
            }
        }
        out
    }

    // ----- input -----

    fn key(&mut self, k: Key) {
        self.message.clear();
        match self.overlay {
            Overlay::QuickOpen => return self.quick_open_key(k),
            Overlay::Find if !matches!(k, Key::Char(0x13)) => return self.find_key(k),
            _ => {}
        }
        let rows = self.visible_rows();
        match k {
            Key::Char(0x13) => self.save(),                  // Ctrl-S
            Key::Char(0x06) => {
                // Ctrl-F: a fresh search, seeded with the selection if there is one.
                self.query = self.doc().selected_text().filter(|t| !t.contains('\n')).unwrap_or_default();
                self.overlay = Overlay::Find;
            }
            Key::Char(0x10) | Key::Char(0x0F) => {
                // Ctrl-P / Ctrl-O
                self.overlay = Overlay::QuickOpen;
                self.query.clear();
                self.pick = 0;
            }
            Key::Char(0x0E) => {
                // Ctrl-N
                self.docs.push(Doc::new(None, ""));
                self.active = self.docs.len() - 1;
            }
            Key::Char(0x17) => {
                // Ctrl-W
                let i = self.active;
                self.close_tab(i);
            }
            Key::Char(0x02) => self.sidebar = !self.sidebar, // Ctrl-B
            Key::Char(0x1A) => {
                // Ctrl-Z
                let d = self.doc();
                if let Some(s) = d.undo.pop() {
                    d.redo.push(Snapshot { lines: d.lines.clone(), cursor: d.cursor });
                    d.lines = s.lines;
                    d.cursor = s.cursor;
                    d.anchor = None;
                    d.dirty = true;
                    d.last_edit = 0;
                }
            }
            Key::Char(0x19) => {
                // Ctrl-Y
                let d = self.doc();
                if let Some(s) = d.redo.pop() {
                    d.undo.push(Snapshot { lines: d.lines.clone(), cursor: d.cursor });
                    d.lines = s.lines;
                    d.cursor = s.cursor;
                    d.anchor = None;
                    d.dirty = true;
                }
            }
            Key::Char(0x01) => {
                // Ctrl-A
                let d = self.doc();
                let last = d.lines.len() - 1;
                d.anchor = Some(Pos { row: 0, col: 0 });
                d.cursor = Pos { row: last, col: d.line_len(last) };
            }
            Key::Char(0x03) | Key::Char(0x18) => {
                // Ctrl-C / Ctrl-X: the selection, or the whole line without one.
                let cut = k == Key::Char(0x18);
                let d = self.doc();
                let text = d.selected_text().unwrap_or_else(|| format!("{}\n", d.lines[d.cursor.row]));
                ui::copy(&text);
                if cut {
                    d.checkpoint(2);
                    if !d.delete_selection() {
                        let row = d.cursor.row;
                        if d.lines.len() > 1 {
                            d.lines.remove(row);
                        } else {
                            d.lines[0].clear();
                        }
                        d.cursor = Pos { row: row.min(d.lines.len() - 1), col: 0 };
                        d.dirty = true;
                    }
                }
                self.message = if cut { "Cut".into() } else { "Copied".into() };
            }
            Key::Char(0x16) => {
                // Ctrl-V
                let text = ui::paste();
                let d = self.doc();
                d.checkpoint(2);
                d.insert(&text.replace('\t', "    "));
            }
            Key::Char(0x04) => {
                // Ctrl-D: duplicate the line
                let d = self.doc();
                d.checkpoint(2);
                let line = d.lines[d.cursor.row].clone();
                d.lines.insert(d.cursor.row + 1, line);
                d.cursor.row += 1;
                d.dirty = true;
            }
            Key::Char(0x1B) => self.doc().anchor = None,
            Key::Char(b'\n' | b'\r') => {
                let d = self.doc();
                d.checkpoint(2);
                d.newline();
            }
            Key::Char(0x08 | 0x7F) => {
                let d = self.doc();
                d.checkpoint(1);
                d.backspace();
            }
            Key::Delete => {
                let d = self.doc();
                d.checkpoint(2);
                d.delete_forward();
            }
            Key::Char(b'\t') => {
                let d = self.doc();
                d.checkpoint(1);
                let n = 4 - d.cursor.col % 4;
                d.insert(&" ".repeat(n));
            }
            Key::Char(c) if (0x20..0x7F).contains(&c) => {
                let d = self.doc();
                d.checkpoint(if c == b' ' { 2 } else { 1 });
                // Close brackets and quotes as you type them.
                let pair = match c {
                    b'(' => Some(')'),
                    b'[' => Some(']'),
                    b'{' => Some('}'),
                    b'"' if d.lang != Lang::Plain && d.lang != Lang::Markdown => Some('"'),
                    _ => None,
                };
                let next = d.lines[d.cursor.row].chars().nth(d.cursor.col);
                if matches!(c, b')' | b']' | b'}' | b'"') && next == Some(c as char) && d.selection().is_none() {
                    d.cursor.col += 1;
                } else if let Some(p) = pair.filter(|_| d.selection().is_none()) {
                    d.insert(&format!("{}{p}", c as char));
                    d.cursor.col -= 1;
                } else {
                    d.insert(&(c as char).to_string());
                }
            }
            Key::Left | Key::Right | Key::Up | Key::Down | Key::Home | Key::End | Key::PageUp | Key::PageDown => {
                let d = self.doc();
                let Pos { row, col } = d.cursor;
                // With a selection, Left/Right jump to its start/end.
                if let (Some((a, b)), Key::Left | Key::Right) = (d.selection(), k) {
                    let p = if k == Key::Left { a } else { b };
                    d.move_to(p, false);
                    d.want_col = p.col;
                } else {
                    let p = match k {
                        Key::Left if col > 0 => Pos { row, col: col - 1 },
                        Key::Left if row > 0 => Pos { row: row - 1, col: d.line_len(row - 1) },
                        Key::Right if col < d.line_len(row) => Pos { row, col: col + 1 },
                        Key::Right if row + 1 < d.lines.len() => Pos { row: row + 1, col: 0 },
                        Key::Up if row > 0 => Pos { row: row - 1, col: d.want_col },
                        Key::Down if row + 1 < d.lines.len() => Pos { row: row + 1, col: d.want_col },
                        Key::Home => {
                            let indent = d.lines[row].chars().take_while(|c| *c == ' ').count();
                            Pos { row, col: if col == indent { 0 } else { indent } }
                        }
                        Key::End => Pos { row, col: d.line_len(row) },
                        Key::PageUp => Pos { row: row.saturating_sub(rows), col: d.want_col },
                        Key::PageDown => Pos { row: (row + rows).min(d.lines.len() - 1), col: d.want_col },
                        _ => d.cursor,
                    };
                    d.move_to(p, false);
                    if !matches!(k, Key::Up | Key::Down | Key::PageUp | Key::PageDown) {
                        d.want_col = d.cursor.col;
                    }
                }
            }
            _ => {}
        }
        self.ensure_cursor_visible();
    }

    fn find_matches(&mut self) -> Vec<Pos> {
        let q = self.query.to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (row, line) in self.doc().lines.iter().enumerate() {
            let lower = line.to_lowercase();
            let mut from = 0;
            while let Some(i) = lower[from..].find(&q) {
                out.push(Pos { row, col: char_len(&lower[..from + i]) });
                from += i + q.len().max(1);
            }
            if out.len() > 5000 {
                break;
            }
        }
        out
    }

    fn find_key(&mut self, k: Key) {
        match k {
            Key::Char(0x1B) => {
                self.overlay = Overlay::None;
                self.ensure_cursor_visible();
            }
            Key::Char(0x08 | 0x7F) => {
                self.query.pop();
            }
            Key::Char(b'\n' | b'\r') | Key::Down => {
                // Next match after the cursor (wrapping), selected.
                let matches = self.find_matches();
                let qlen = char_len(&self.query);
                let cur = self.doc().cursor;
                if let Some(m) = matches.iter().find(|m| **m > cur).or(matches.first()).copied() {
                    let d = self.doc();
                    d.anchor = Some(m);
                    d.cursor = Pos { row: m.row, col: m.col + qlen };
                    self.ensure_cursor_visible();
                    self.message = format!("{} matches", matches.len());
                } else {
                    self.message = "No matches".into();
                }
            }
            Key::Char(c) if (0x20..0x7F).contains(&c) => self.query.push(c as char),
            _ => {}
        }
    }

    fn quick_open_list(&self) -> Vec<String> {
        // Matches inside the file name rank well above matches spread over the path.
        let mut scored: Vec<(i32, String)> = Self::files()
            .into_iter()
            .filter_map(|f| {
                let name = f.rsplit('/').next().unwrap_or(&f);
                let s = ui::fuzzy(&self.query, name).map(|s| s + 100).or_else(|| ui::fuzzy(&self.query, &f))?;
                Some((s, f))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.into_iter().take(12).map(|(_, f)| f).collect()
    }

    fn quick_open_key(&mut self, k: Key) {
        let list = self.quick_open_list();
        match k {
            Key::Char(0x1B) => self.overlay = Overlay::None,
            Key::Up => self.pick = self.pick.saturating_sub(1),
            Key::Down => self.pick = (self.pick + 1).min(list.len().saturating_sub(1)),
            Key::Char(0x08 | 0x7F) => {
                self.query.pop();
                self.pick = 0;
            }
            Key::Char(b'\n' | b'\r') => {
                if let Some(p) = list.get(self.pick) {
                    let p = p.clone();
                    self.open_path(&p);
                }
                self.overlay = Overlay::None;
            }
            Key::Char(c) if (0x20..0x7F).contains(&c) => {
                self.query.push(c as char);
                self.pick = 0;
            }
            _ => {}
        }
    }

    fn mouse_down(&mut self, x: i32, y: i32) {
        let (w, _) = self.size();
        if self.overlay == Overlay::QuickOpen {
            let list = self.quick_open_list();
            let r = self.palette_rect(w, list.len());
            let i = (y - r.y - 56) / 32;
            if r.contains(x, y) && i >= 0 && (i as usize) < list.len() {
                let p = list[i as usize].clone();
                self.open_path(&p);
            }
            self.overlay = Overlay::None;
            return;
        }
        // Tabs.
        if y < TABS_H {
            let mut tx = if self.sidebar { SIDEBAR_W } else { 0 } + 6;
            for i in 0..self.docs.len() {
                let tw = self.tab_width(i);
                if x >= tx && x < tx + tw {
                    if x >= tx + tw - 26 {
                        self.close_tab(i);
                    } else {
                        self.active = i;
                    }
                    return;
                }
                tx += tw + 4;
            }
            return;
        }
        // Sidebar.
        if self.sidebar && x < SIDEBAR_W {
            let i = (y - TABS_H - 8) / 28;
            let tree = self.tree();
            if i >= 0
                && let Some((_, _, path, folder)) = tree.get(i as usize)
            {
                if *folder {
                    if !self.expanded.remove(path) {
                        self.expanded.insert(path.clone());
                    }
                } else {
                    let p = path.clone();
                    self.open_path(&p);
                }
            }
            return;
        }
        // Text.
        if self.text_area().contains(x, y) {
            let p = self.pos_at(x, y);
            let now = crate::pit::ticks();
            let double = now - self.last_click.0 < crate::pit::HZ / 3 && self.last_click.1 == p;
            self.last_click = (now, p);
            let d = self.doc();
            if double {
                // Select the word under the pointer.
                let chars: Vec<char> = d.lines[p.row].chars().collect();
                let is_word = |c: char| c.is_alphanumeric() || c == '_';
                let mut a = p.col.min(chars.len());
                let mut b = a;
                while a > 0 && is_word(chars[a - 1]) {
                    a -= 1;
                }
                while b < chars.len() && is_word(chars[b]) {
                    b += 1;
                }
                d.anchor = Some(Pos { row: p.row, col: a });
                d.cursor = Pos { row: p.row, col: b };
            } else {
                d.move_to(p, false);
                d.anchor = Some(p);
                d.want_col = p.col;
                self.dragging = true;
            }
        }
    }

    fn mouse_drag(&mut self, x: i32, y: i32) {
        if self.dragging {
            let area = self.text_area();
            // Dragging past the edges scrolls.
            if y < area.y {
                let d = self.doc();
                d.scroll = d.scroll.saturating_sub(1);
            } else if y > area.y + area.h {
                let d = self.doc();
                d.scroll = (d.scroll + 1).min(d.lines.len().saturating_sub(1));
            }
            let p = self.pos_at(x, y);
            let d = self.doc();
            d.cursor = p;
            d.want_col = p.col;
        }
    }

    fn run(&mut self) {
        self.render();
        loop {
            let Some(e) = wm::wait_event(&self.win, Some(crate::pit::HZ / 4)) else {
                // Files opened from elsewhere while running.
                let queued: Vec<String> = core::mem::take(&mut *OPEN_QUEUE.lock());
                if !queued.is_empty() {
                    for p in queued {
                        self.open_path(&p);
                    }
                    self.render();
                }
                continue;
            };
            match e {
                Event::Close => {
                    if self.docs.iter().any(|d| d.dirty) {
                        // Keep unsaved work: save it before closing.
                        for i in 0..self.docs.len() {
                            if self.docs[i].dirty {
                                self.active = i;
                                self.save();
                            }
                        }
                    }
                    return;
                }
                Event::Key(k) => self.key(k),
                Event::MouseDown { x, y, .. } => self.mouse_down(x, y),
                Event::MouseMove { x, y } => self.mouse_drag(x, y),
                Event::MouseUp { .. } => {
                    self.dragging = false;
                    let d = self.doc();
                    if d.anchor == Some(d.cursor) {
                        d.anchor = None;
                    }
                }
                Event::Wheel(n) => {
                    let d = self.doc();
                    d.scroll = (d.scroll as i32 + n * -3).clamp(0, d.lines.len().saturating_sub(1) as i32) as usize;
                }
                Event::Resized => {}
            }
            self.render();
        }
    }

    // ----- drawing -----

    fn tab_width(&self, i: usize) -> i32 {
        font::width(&self.docs[i].name, 14, Weight::Regular) + 58
    }

    fn palette_rect(&self, w: i32, n: usize) -> Rect {
        let pw = (w - 80).min(560);
        Rect::new((w - pw) / 2, 70, pw, 64 + n as i32 * 32)
    }

    fn render(&mut self) {
        // Pick up queued files first.
        let queued: Vec<String> = core::mem::take(&mut *OPEN_QUEUE.lock());
        for p in queued {
            self.open_path(&p);
        }
        let (w, h) = self.size();
        let mut c = Canvas::new(w as usize, h as usize);
        c.clear(ui::BG);
        self.doc(); // make sure there is a document
        let (cw, ch) = Self::cell();

        // Sidebar.
        if self.sidebar {
            c.fill_rect(Rect::new(0, 0, SIDEBAR_W, h), ui::SIDEBAR);
            c.fill_rect(Rect::new(SIDEBAR_W - 1, 0, 1, h), ui::BORDER);
            font::draw(&mut c, 16, 11, "EXPLORER", 12, Weight::Bold, ui::DIM);
            let active_path = self.docs.get(self.active).and_then(|d| d.path.clone());
            for (i, (depth, label, path, folder)) in self.tree().iter().enumerate() {
                let y = TABS_H + 8 + i as i32 * 28;
                if y > h - STATUS_H - 24 {
                    break;
                }
                let x = 14 + *depth as i32 * 16;
                if active_path.as_deref() == Some(path.as_str()) {
                    c.fill_round_rect(Rect::new(6, y, SIDEBAR_W - 12, 26), 6, ui::HOVER);
                }
                if *folder {
                    let open = self.expanded.contains(path);
                    disclosure(&mut c, x + 1, y + 8, open, ui::DIM);
                    font::draw_clipped(&mut c, x + 16, y + 3, label, 14, Weight::Bold, ui::TEXT, SIDEBAR_W - x - 26);
                } else {
                    let dot = match Lang::of(path) {
                        Lang::Rust => 0xE8833A,
                        Lang::Rhai => ui::ACCENT,
                        Lang::Markdown => 0x7AA8FF,
                        Lang::Json | Lang::Shell => 0xF2C46D,
                        Lang::CLike | Lang::Python => 0x5FD3D0,
                        Lang::Plain => ui::FAINT,
                    };
                    c.fill_circle(x + 5, y + 13, 3, dot);
                    font::draw_clipped(&mut c, x + 16, y + 3, label, 14, Weight::Regular, ui::TEXT, SIDEBAR_W - x - 26);
                }
            }
        }

        // Tabs.
        let tabs_x = if self.sidebar { SIDEBAR_W } else { 0 };
        c.fill_rect(Rect::new(tabs_x, 0, w - tabs_x, TABS_H), ui::SIDEBAR);
        c.fill_rect(Rect::new(tabs_x, TABS_H - 1, w - tabs_x, 1), ui::BORDER);
        let mut tx = tabs_x + 6;
        for i in 0..self.docs.len() {
            let tw = self.tab_width(i);
            let active = i == self.active;
            let r = Rect::new(tx, 5, tw, TABS_H - 5);
            if active {
                c.fill_round_rect(Rect::new(r.x, r.y, r.w, r.h + 8), 8, ui::BG);
                c.fill_rect(Rect::new(r.x + 10, r.y, r.w - 20, 2), ui::ACCENT);
            }
            let d = &self.docs[i];
            font::draw(&mut c, r.x + 14, r.y + 8, &d.name, 14, if active { Weight::Bold } else { Weight::Regular }, if active { ui::TEXT } else { ui::DIM });
            // A dot for unsaved changes, else the close cross.
            if d.dirty {
                c.fill_circle(r.x + r.w - 16, r.y + 16, 4, ui::ACCENT);
            } else {
                font::draw(&mut c, r.x + r.w - 21, r.y + 6, "\u{00d7}", 16, Weight::Regular, ui::FAINT);
            }
            tx += tw + 4;
        }

        // Find bar.
        if self.overlay == Overlay::Find {
            let fb = Rect::new(tabs_x, TABS_H, w - tabs_x, FIND_H);
            c.fill_rect(fb, ui::PANEL);
            c.fill_rect(Rect::new(fb.x, fb.y + fb.h - 1, fb.w, 1), ui::BORDER);
            ui::field(&mut c, Rect::new(fb.x + 12, fb.y + 7, 320, 30), &self.query, "Find", true);
            let n = self.find_matches().len();
            font::draw(&mut c, fb.x + 346, fb.y + 13, &format!("{n} matches   Enter: next   Esc: close"), 13, Weight::Regular, ui::DIM);
        }

        // Text.
        let area = self.text_area();
        let gutter = self.gutter_w();
        let rows = self.visible_rows();
        let matches = if self.overlay == Overlay::Find { self.find_matches() } else { Vec::new() };
        let qlen = char_len(&self.query);
        let d = &self.docs[self.active.min(self.docs.len() - 1)];
        let sel = d.selection();
        // Block-comment state at the first visible line.
        let mut colors = Vec::new();
        let mut in_block = false;
        if d.lang.block_comments() {
            for line in &d.lines[..d.scroll.min(d.lines.len())] {
                in_block = highlight(line, d.lang, in_block, &mut colors);
            }
        }
        for vr in 0..rows {
            let row = d.scroll + vr;
            if row >= d.lines.len() {
                break;
            }
            let y = area.y + vr as i32 * ch;
            let line = &d.lines[row];
            if row == d.cursor.row {
                c.fill_rect(Rect::new(area.x, y, area.w, ch), ui::CURRENT_LINE);
            }
            // Line number.
            let num = (row + 1).to_string();
            let nx = area.x + gutter - 16 - num.len() as i32 * cw;
            font::draw(&mut c, nx, y + 1, &num, CODE_SIZE, Weight::Mono, if row == d.cursor.row { ui::TEXT } else { ui::FAINT });
            let tx0 = area.x + gutter;
            // Find matches and the selection behind the text.
            for m in matches.iter().filter(|m| m.row == row) {
                c.fill_round_rect(Rect::new(tx0 + m.col as i32 * cw, y + 1, qlen as i32 * cw, ch - 2), 3, 0x4A3F20);
            }
            if let Some((a, b)) = sel
                && row >= a.row
                && row <= b.row
            {
                let from = if row == a.row { a.col } else { 0 };
                let to = if row == b.row { b.col } else { char_len(line) + 1 };
                c.fill_rect(Rect::new(tx0 + from as i32 * cw, y, (to - from) as i32 * cw, ch), ui::SELECTION);
            }
            in_block = highlight(line, d.lang, in_block, &mut colors);
            let max_cols = ((area.w - gutter - 8) / cw).max(0) as usize;
            let mut buf = [0u8; 4];
            for (col, chr) in line.chars().enumerate().take(max_cols) {
                if chr != ' ' {
                    font::draw(&mut c, tx0 + col as i32 * cw, y + 1, chr.encode_utf8(&mut buf), CODE_SIZE, Weight::Mono, colors[col]);
                }
            }
            if row == d.cursor.row && self.overlay != Overlay::QuickOpen {
                c.fill_rect(Rect::new(tx0 + d.cursor.col as i32 * cw, y + 1, 2, ch - 2), ui::ACCENT);
            }
        }
        // Scrollbar.
        if d.lines.len() > rows {
            let track = area.h - 8;
            let thumb = (track * rows as i32 / d.lines.len() as i32).max(24);
            let ty = area.y + 4 + (track - thumb) * d.scroll as i32 / (d.lines.len() - rows).max(1) as i32;
            c.fill_round_rect(Rect::new(area.x + area.w - 8, ty, 5, thumb), 3, ui::BORDER);
        }

        // Status bar.
        let sb = Rect::new(0, h - STATUS_H, w, STATUS_H);
        c.fill_rect(sb, ui::SIDEBAR);
        c.fill_rect(Rect::new(0, sb.y, w, 1), ui::BORDER);
        let path = d.path.clone().unwrap_or_else(|| "untitled (Ctrl-S saves under notes/)".into());
        let left = if self.message.is_empty() { path } else { self.message.clone() };
        font::draw_clipped(&mut c, 14, sb.y + 5, &left, 13, Weight::Regular, if self.message.is_empty() { ui::DIM } else { ui::TEXT }, w / 2);
        let right = format!("Ln {}, Col {}    {} lines    {}    Ctrl-P open   Ctrl-F find", d.cursor.row + 1, d.cursor.col + 1, d.lines.len(), d.lang.name());
        let rw = font::width(&right, 13, Weight::Regular);
        font::draw(&mut c, w - rw - 14, sb.y + 5, &right, 13, Weight::Regular, ui::DIM);

        // Quick open palette.
        if self.overlay == Overlay::QuickOpen {
            c.blend_rect(c.bounds(), 0x000000, 110);
            let list = self.quick_open_list();
            let r = self.palette_rect(w, list.len());
            c.shadow(r, 16);
            c.fill_round_rect(r, 12, ui::PANEL);
            ui::field(&mut c, Rect::new(r.x + 12, r.y + 12, r.w - 24, 34), &self.query, "Open a file by name...", true);
            for (i, f) in list.iter().enumerate() {
                let y = r.y + 56 + i as i32 * 32;
                if i == self.pick {
                    c.fill_round_rect(Rect::new(r.x + 8, y, r.w - 16, 30), 7, ui::SELECTION);
                }
                let (dir, name) = f.rsplit_once('/').map_or(("", f.as_str()), |(d, n)| (d, n));
                let end = font::draw(&mut c, r.x + 20, y + 6, name, 14, Weight::Bold, ui::TEXT);
                font::draw_clipped(&mut c, end + 10, y + 7, dir, 13, Weight::Regular, ui::DIM, r.x + r.w - end - 30);
            }
        }
        wm::present(&self.win, &c);
        let title = format!("{}{} - Text Editor", d.name, if d.dirty { " \u{2022}" } else { "" });
        wm::set_title(&self.win, &title);
    }
}
