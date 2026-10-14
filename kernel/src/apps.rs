//! Script apps: Rhai programs that draw into a desktop window.
//!
//! An app is a `.rhai` file (user apps in `apps/`, bundled ones in `system/apps/`).
//! Header comments describe it:
//!
//! ```text
//! // title: Clock
//! // icon: C
//! // color: #4A90E2
//! // size: 360x240
//! ```
//!
//! The script defines callbacks; all are optional except `draw`:
//!
//! | Callback | When |
//! |---|---|
//! | `fn init()` | once at start; returns the app state (usually a map `#{...}`) |
//! | `fn draw(g)` | after every event; `g` is the window's graphics context |
//! | `fn click(x, y)` / `fn right_click(x, y)` | mouse button pressed in the window |
//! | `fn release(x, y)`, `fn drag(x, y)` | button released / moved while held |
//! | `fn scroll(n)` | mouse wheel |
//! | `fn key(k)` | key typed: `"a"`, `"Enter"`, `"Backspace"`, `"Up"`, ... |
//! | `fn tick()` | every 100 ms |
//!
//! Inside callbacks `this` is the state returned by `init`, so `this.count += 1`
//! updates it. Each app runs in its own task; long-running scripts yield to the
//! rest of the system every few thousand operations.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use rhai::{Array, CallFnOptions, Dynamic, Engine, Map, Scope, AST, INT};

use crate::gfx::{Canvas, Rect};
use crate::input::Key;
use crate::sync::SpinLock;
use crate::wm::{self, Content, Event, WindowRef};
use crate::{fs, net, pit, rand, rtc, task};

pub struct AppInfo {
    pub path: String,
    pub title: String,
    /// The first comment line after the header fields (a one-line description).
    pub about: String,
    pub icon: char,
    pub color: u32,
    pub width: i32,
    pub height: i32,
}

/// Bundled apps: (path, source). Written to the filesystem at boot if missing or older.
pub const BUNDLED: &[(&str, &str)] = &[
    ("system/apps/files.rhai", include_str!("apps/files.rhai")),
    ("system/apps/editor.rhai", include_str!("apps/editor.rhai")),
    ("system/apps/monitor.rhai", include_str!("apps/monitor.rhai")),
    ("system/apps/settings.rhai", include_str!("apps/settings.rhai")),
];

/// Install or update the bundled apps on the system filesystem.
pub fn install_bundled() {
    for (path, src) in BUNDLED {
        if fs::read(path).ok().as_deref() != Some(src.as_bytes()) {
            let _ = fs::write(path, src.as_bytes());
        }
    }
}

fn parse_color(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim().trim_start_matches('#').trim_start_matches("0x"), 16).ok()
}

pub fn info(path: &str, src: &str) -> AppInfo {
    let mut info = AppInfo {
        path: path.into(),
        title: path.rsplit('/').next().unwrap_or(path).trim_end_matches(".rhai").into(),
        about: String::new(),
        icon: '\u{2666}',
        color: 0x5B6EE1,
        width: 480,
        height: 320,
    };
    for line in src.lines().take(20) {
        let Some(rest) = line.trim().strip_prefix("//") else { continue };
        let known = ["title", "icon", "color", "size"];
        let Some((key, value)) = rest.split_once(':').filter(|(k, _)| known.contains(&k.trim())) else {
            if info.about.is_empty() && !rest.trim().is_empty() {
                info.about = rest.trim().into();
            }
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "title" => info.title = value.into(),
            "icon" => info.icon = value.chars().next().unwrap_or(info.icon),
            "color" => info.color = parse_color(value).unwrap_or(info.color),
            "size" => {
                if let Some((w, h)) = value.split_once('x') {
                    info.width = w.trim().parse().unwrap_or(info.width).clamp(160, 1600);
                    info.height = h.trim().parse().unwrap_or(info.height).clamp(100, 1000);
                }
            }
            _ => {}
        }
    }
    info
}

/// Bundled apps first, then the user's apps.
pub fn installed() -> Vec<AppInfo> {
    let mut v = Vec::new();
    for prefix in ["system/apps/", "apps/"] {
        for (name, _, _) in fs::list(prefix) {
            if name.ends_with(".rhai")
                && let Ok(src) = fs::read_string(&name)
            {
                v.push(info(&name, &src));
            }
        }
    }
    v
}

/// The graphics context passed to `draw`.
#[derive(Clone)]
pub struct Gfx(Arc<SpinLock<Canvas>>);

impl Gfx {
    fn with<R>(&mut self, f: impl FnOnce(&mut Canvas) -> R) -> R {
        let mut c = self.0.try_lock().expect("canvas in use");
        f(&mut c)
    }
}

fn color(c: INT) -> u32 {
    (c as u32) & 0xFF_FFFF
}

fn key_name(k: Key) -> String {
    match k {
        Key::Char(b'\n') => "Enter".into(),
        Key::Char(0x08) => "Backspace".into(),
        Key::Char(b'\t') => "Tab".into(),
        Key::Char(0x1B) => "Escape".into(),
        Key::Char(c) if c < 0x20 => format!("Ctrl-{}", (c + b'@') as char),
        Key::Char(c) => (c as char).to_string(),
        Key::Up => "Up".into(),
        Key::Down => "Down".into(),
        Key::Left => "Left".into(),
        Key::Right => "Right".into(),
        Key::Super => "Super".into(),
        Key::Home => "Home".into(),
        Key::End => "End".into(),
        Key::PageUp => "PageUp".into(),
        Key::PageDown => "PageDown".into(),
        Key::Delete => "Delete".into(),
    }
}

/// Per-app context captured by the builtins.
struct AppCtx {
    name: String,
    window: Option<WindowRef>,
    quit: AtomicBool,
}

fn app_data_key(app: &str, key: &str) -> String {
    let clean: String = key.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
    format!("appdata/{app}/{clean}")
}

/// Decoded PNG files, shared by all apps; dropped when the filesystem changes.
static IMAGES: SpinLock<(u64, Vec<(String, Option<Arc<Canvas>>)>)> = SpinLock::new((0, Vec::new()));

/// A PNG file from the filesystem, decoded once (None if missing or not a PNG).
pub fn image(path: &str) -> Option<Arc<Canvas>> {
    {
        let mut cache = IMAGES.lock();
        if cache.0 != fs::generation() {
            *cache = (fs::generation(), Vec::new());
        }
        if let Some((_, img)) = cache.1.iter().find(|(p, _)| p == path) {
            return img.clone();
        }
    }
    let data = fs::read(path).ok().or_else(|| fs::system_file(path).map(|s| s.to_vec()));
    let img = data.and_then(|d| Canvas::from_png(&d).ok()).map(Arc::new);
    let mut cache = IMAGES.lock();
    if cache.1.len() >= 48 {
        cache.1.remove(0);
    }
    cache.1.push((path.into(), img.clone()));
    img
}

fn make_engine(ctx: Arc<AppCtx>) -> Engine {
    let mut e = Engine::new();
    e.set_max_operations(50_000_000);
    e.set_max_call_levels(64);
    e.set_max_string_size(4 << 20);
    e.set_max_array_size(1 << 20);
    e.on_progress(|ops| {
        if ops % 20_000 == 0 {
            task::yield_now();
        }
        None
    });
    let name = ctx.name.clone();
    e.on_print(move |s| crate::log::info(&format!("app {name}: {s}")));
    e.on_debug(|s, _, _| crate::log::info(&format!("app debug: {s}")));

    e.register_type_with_name::<Gfx>("Gfx");
    e.register_get("width", |g: &mut Gfx| g.with(|c| c.width as INT));
    e.register_get("height", |g: &mut Gfx| g.with(|c| c.height as INT));
    e.register_fn("clear", |g: &mut Gfx, c: INT| g.with(|cv| cv.clear(color(c))));
    e.register_fn("rect", |g: &mut Gfx, x: INT, y: INT, w: INT, h: INT, c: INT| g.with(|cv| cv.fill_rect(Rect::new(x as i32, y as i32, w as i32, h as i32), color(c))));
    // Images (PNG files, e.g. ones the assistant generated under apps/assets/).
    e.register_fn("image", |g: &mut Gfx, x: INT, y: INT, path: &str| {
        if let Some(img) = image(path) {
            g.with(|cv| cv.blit(&img, img.bounds(), x as i32, y as i32));
        }
    });
    e.register_fn("image", |g: &mut Gfx, x: INT, y: INT, w: INT, h: INT, path: &str| {
        if let Some(img) = image(path) {
            g.with(|cv| cv.blit_scaled(&img, Rect::new(x as i32, y as i32, w as i32, h as i32), 0, 255));
        }
    });
    e.register_fn("image", |g: &mut Gfx, x: INT, y: INT, w: INT, h: INT, radius: INT, path: &str| {
        if let Some(img) = image(path) {
            g.with(|cv| cv.blit_scaled(&img, Rect::new(x as i32, y as i32, w as i32, h as i32), radius as i32, 255));
        }
    });
    e.register_fn("image_size", |path: &str| -> Map {
        let mut m = Map::new();
        let (w, h) = image(path).map_or((0, 0), |i| (i.width as INT, i.height as INT));
        m.insert("width".into(), w.into());
        m.insert("height".into(), h.into());
        m
    });
    e.register_fn("frame", |g: &mut Gfx, x: INT, y: INT, w: INT, h: INT, c: INT| g.with(|cv| cv.stroke_rect(Rect::new(x as i32, y as i32, w as i32, h as i32), color(c))));
    e.register_fn("round_rect", |g: &mut Gfx, x: INT, y: INT, w: INT, h: INT, r: INT, c: INT| g.with(|cv| cv.fill_round_rect(Rect::new(x as i32, y as i32, w as i32, h as i32), r as i32, color(c))));
    e.register_fn("line", |g: &mut Gfx, x0: INT, y0: INT, x1: INT, y1: INT, c: INT| g.with(|cv| cv.line(x0 as i32, y0 as i32, x1 as i32, y1 as i32, color(c))));
    e.register_fn("circle", |g: &mut Gfx, x: INT, y: INT, r: INT, c: INT| g.with(|cv| cv.fill_circle(x as i32, y as i32, r as i32, color(c))));
    e.register_fn("ring", |g: &mut Gfx, x: INT, y: INT, r: INT, c: INT| g.with(|cv| cv.stroke_circle(x as i32, y as i32, r as i32, color(c))));
    e.register_fn("pixel", |g: &mut Gfx, x: INT, y: INT, c: INT| g.with(|cv| cv.set(x as i32, y as i32, color(c))));
    e.register_fn("text", |g: &mut Gfx, x: INT, y: INT, s: &str, c: INT| g.with(|cv| cv.text(x as i32, y as i32, s, color(c)) as INT));
    e.register_fn("text_big", |g: &mut Gfx, x: INT, y: INT, s: &str, c: INT| g.with(|cv| cv.text_scaled(x as i32, y as i32, s, color(c), 2) as INT));
    e.register_fn("text_scaled", |g: &mut Gfx, x: INT, y: INT, s: &str, c: INT, scale: INT| g.with(|cv| cv.text_scaled(x as i32, y as i32, s, color(c), (scale as i32).clamp(1, 8)) as INT));
    // Smooth (anti-aliased, proportional) text in the UI font: label(x, y, s, color[, size]).
    use crate::font::{self, Weight};
    let size_of = |size: INT| (size.clamp(8, 96)) as u16;
    e.register_fn("label", |g: &mut Gfx, x: INT, y: INT, s: &str, c: INT| g.with(|cv| font::draw(cv, x as i32, y as i32, s, 15, Weight::Regular, color(c)) as INT));
    e.register_fn("label", move |g: &mut Gfx, x: INT, y: INT, s: &str, c: INT, size: INT| g.with(|cv| font::draw(cv, x as i32, y as i32, s, size_of(size), Weight::Regular, color(c)) as INT));
    e.register_fn("label_bold", |g: &mut Gfx, x: INT, y: INT, s: &str, c: INT| g.with(|cv| font::draw(cv, x as i32, y as i32, s, 15, Weight::Bold, color(c)) as INT));
    e.register_fn("label_bold", move |g: &mut Gfx, x: INT, y: INT, s: &str, c: INT, size: INT| g.with(|cv| font::draw(cv, x as i32, y as i32, s, size_of(size), Weight::Bold, color(c)) as INT));
    e.register_fn("label_width", |s: &str| font::width(s, 15, Weight::Regular) as INT);
    e.register_fn("label_width", move |s: &str, size: INT| font::width(s, size_of(size), Weight::Regular) as INT);
    let button = |g: &mut Gfx, x: INT, y: INT, w: INT, h: INT, label: &str, bg: INT| {
        g.with(|cv| {
            let r = Rect::new(x as i32, y as i32, w as i32, h as i32);
            cv.fill_round_rect(r, (r.h / 2).min(10), color(bg));
            let tw = font::width(label, 14, Weight::Bold);
            font::draw(cv, r.x + (r.w - tw) / 2, r.y + (r.h - font::line_height(14)) / 2, label, 14, Weight::Bold, 0xFFFFFF);
        })
    };
    e.register_fn("button", move |g: &mut Gfx, x: INT, y: INT, w: INT, h: INT, label: &str| button(g, x, y, w, h, label, 0x5B5FC7));
    e.register_fn("button", button);
    e.register_fn("progress", |g: &mut Gfx, x: INT, y: INT, w: INT, h: INT, percent: INT, c: INT| {
        g.with(|cv| {
            let r = Rect::new(x as i32, y as i32, w as i32, h as i32);
            cv.fill_round_rect(r, (h / 2) as i32, 0x2E3044);
            let fw = (w * percent.clamp(0, 100) / 100) as i32;
            if fw > 0 {
                cv.fill_round_rect(Rect::new(r.x, r.y, fw.max(h as i32), r.h), (h / 2) as i32, color(c));
            }
        })
    });

    e.register_fn("rgb", |r: INT, g: INT, b: INT| ((r.clamp(0, 255) << 16) | (g.clamp(0, 255) << 8) | b.clamp(0, 255)) as INT);
    e.register_fn("text_width", |s: &str| s.chars().count() as INT * 8);
    e.register_fn("ticks", || (pit::ticks() * 1000 / pit::HZ) as INT);
    e.register_fn("random", |n: INT| if n <= 0 { 0 } else { (rand::u64() % n as u64) as INT });
    e.register_fn("now", || {
        let (y, mo, d, h, mi, s) = rtc::now();
        let mut m = Map::new();
        m.insert("year".into(), (y as INT).into());
        m.insert("month".into(), (mo as INT).into());
        m.insert("day".into(), (d as INT).into());
        m.insert("hour".into(), (h as INT).into());
        m.insert("minute".into(), (mi as INT).into());
        m.insert("second".into(), (s as INT).into());
        m.insert("unix".into(), (rtc::unix_time() as INT).into());
        m
    });
    e.register_fn("sys_info", sys_info);
    e.register_fn("http_get", |url: &str| match crate::http::get(url) {
        Ok(r) => String::from_utf8_lossy(&r.body).into_owned(),
        Err(e) => format!("error: {e}"),
    });
    e.register_fn("fs_list", |prefix: &str| -> Array {
        fs::list(prefix)
            .into_iter()
            .map(|(name, size, mtime)| {
                let mut m = Map::new();
                m.insert("name".into(), name.into());
                m.insert("size".into(), (size as INT).into());
                m.insert("mtime".into(), (mtime as INT).into());
                Dynamic::from_map(m)
            })
            .collect()
    });
    e.register_fn("fs_read", |path: &str| fs::read_string(path).unwrap_or_default());
    e.register_fn("fs_write", |path: &str, text: &str| fs::write(path, text.as_bytes()).is_ok());
    e.register_fn("fs_delete", |path: &str| fs::delete(path).is_ok());
    e.register_fn("fs_exists", |path: &str| fs::exists(path));
    let n = ctx.name.clone();
    e.register_fn("load", move |key: &str| fs::read_string(&app_data_key(&n, key)).unwrap_or_default());
    let n = ctx.name.clone();
    e.register_fn("save", move |key: &str, value: &str| fs::write(&app_data_key(&n, key), value.as_bytes()).is_ok());
    e.register_fn("launch", |path: &str| launch(path).is_ok());
    e.register_fn("launch_terminal", || wm::launch(&wm::Action::Terminal));
    e.register_fn("launch_claude", || wm::launch(&wm::Action::Claude));
    e.register_fn("open_url", |url: &str| crate::browser::launch(Some(url.into())));
    let c = ctx.clone();
    e.register_fn("set_title", move |title: &str| {
        if let Some(w) = &c.window {
            wm::set_title(w, title);
        }
    });
    let c = ctx.clone();
    e.register_fn("quit", move || c.quit.store(true, Ordering::Relaxed));
    e.register_fn("install_targets", || -> Array {
        crate::install::targets()
            .into_iter()
            .map(|d| {
                let mut m = Map::new();
                m.insert("disk".into(), (d.index as INT).into());
                m.insert("name".into(), d.name.into());
                m.insert("mib".into(), ((d.sectors / 2048) as INT).into());
                Dynamic::from_map(m)
            })
            .collect()
    });
    e.register_fn("install_to", |disk: INT| match crate::install::install(disk as usize, |_, _| {}) {
        Ok(()) => "ok".to_string(),
        Err(e) => e,
    });
    e.register_fn("is_live", || crate::install::mode() == crate::install::Mode::Live);
    e.register_fn("claude_status", || crate::claude::status_line());
    e.register_fn("assistant_name", || crate::claude::app_name().to_string());
    e
}

fn sys_info() -> Map {
    let mut m = Map::new();
    let (used, free) = crate::memory::heap_usage();
    m.insert("cpu".into(), (task::cpu_usage() as INT).into());
    m.insert("cpu_name".into(), crate::cpu::brand().into());
    m.insert("mem_used".into(), (used as INT).into());
    m.insert("mem_total".into(), ((used + free) as INT).into());
    m.insert("uptime".into(), ((pit::ticks() / pit::HZ) as INT).into());
    m.insert("tasks".into(), (task::list().len() as INT).into());
    let net_info = net::info();
    m.insert("ip".into(), net_info.as_ref().and_then(|i| i.ip).map(|ip| format!("{ip}")).unwrap_or_else(|| "-".into()).into());
    m.insert("gateway".into(), net_info.as_ref().and_then(|i| i.gateway).map(|ip| format!("{ip}")).unwrap_or_else(|| "-".into()).into());
    let (rx, tx) = net::counters();
    m.insert("net_rx".into(), (rx as INT).into());
    m.insert("net_tx".into(), (tx as INT).into());
    if let Some((used, total)) = fs::usage() {
        m.insert("disk_used".into(), (used as INT).into());
        m.insert("disk_total".into(), (total as INT).into());
    }
    let devices: Array = crate::drivers::records()
        .into_iter()
        .map(|r| {
            let status = match &r.status {
                crate::drivers::Status::Bound { driver, .. } => driver.to_string(),
                crate::drivers::Status::Failed { driver, .. } => format!("{driver} (failed)"),
                crate::drivers::Status::NotNeeded => "built in".into(),
                crate::drivers::Status::Missing => "missing".into(),
                crate::drivers::Status::Ported { .. } => "port drafted".into(),
            };
            let mut d = Map::new();
            d.insert("name".into(), format!("{} {}", crate::pci::vendor_name(r.device.vendor), r.device.class_name()).into());
            d.insert("id".into(), format!("{:04x}:{:04x}", r.device.vendor, r.device.device).into());
            d.insert("driver".into(), status.into());
            Dynamic::from_map(d)
        })
        .collect();
    m.insert("devices".into(), devices.into());
    m
}

/// A loaded app: engine, compiled script, state.
struct Running {
    engine: Engine,
    ast: AST,
    scope: Scope<'static>,
    state: Dynamic,
    canvas: Arc<SpinLock<Canvas>>,
    error: Option<String>,
}

impl Running {
    fn load(name: &str, src: &str, window: Option<WindowRef>, w: i32, h: i32) -> Result<(Self, Arc<AppCtx>), String> {
        let ctx = Arc::new(AppCtx { name: name.into(), window, quit: AtomicBool::new(false) });
        let engine = make_engine(ctx.clone());
        let ast = engine.compile(src).map_err(|e| format!("syntax error: {e}"))?;
        if !Self::has_fn(&ast, "draw", 1) {
            return Err("the app has no `fn draw(g)`".into());
        }
        let mut r = Self { engine, ast, scope: Scope::new(), state: Dynamic::from_map(Map::new()), canvas: Arc::new(SpinLock::new(Canvas::new(w as usize, h as usize))), error: None };
        r.engine.run_ast_with_scope(&mut r.scope, &r.ast).map_err(|e| format!("error at top level: {e}"))?;
        if Self::has_fn(&r.ast, "init", 0) {
            let state = r.engine.call_fn_with_options::<Dynamic>(CallFnOptions::new().eval_ast(false), &mut r.scope, &r.ast, "init", ()).map_err(|e| format!("error in init(): {e}"))?;
            r.state = state;
        }
        Ok((r, ctx))
    }

    fn has_fn(ast: &AST, name: &str, params: usize) -> bool {
        ast.iter_functions().any(|f| f.name == name && f.params.len() == params)
    }

    fn call(&mut self, name: &str, args: impl rhai::FuncArgs, params: usize) {
        if self.error.is_some() || !Self::has_fn(&self.ast, name, params) {
            return;
        }
        let opts = CallFnOptions::new().eval_ast(false).bind_this_ptr(&mut self.state);
        if let Err(e) = self.engine.call_fn_with_options::<Dynamic>(opts, &mut self.scope, &self.ast, name, args) {
            self.error = Some(format!("error in {name}(): {e}"));
        }
    }

    fn draw(&mut self) {
        if self.error.is_none() {
            let g = Gfx(self.canvas.clone());
            self.canvas.try_lock().expect("canvas").clear(wm::theme::WINDOW_BG);
            self.call("draw", (g,), 1);
        }
        if let Some(err) = &self.error {
            let mut c = self.canvas.try_lock().expect("canvas");
            c.clear(0x2A1620);
            c.text(12, 12, "This app stopped because of an error:", 0xFF8A8A);
            let mut y = 40;
            for line in wrap(err, (c.width.saturating_sub(24) / 8).max(10)) {
                c.text(12, y, &line, 0xF0D0D0);
                y += 18;
            }
        }
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let chars: Vec<char> = raw.chars().collect();
        if chars.is_empty() {
            lines.push(String::new());
        }
        for chunk in chars.chunks(width) {
            lines.push(chunk.iter().collect());
        }
    }
    lines
}

/// Start the app at `path` in a new window and task.
/// Bundled apps that are native Rust programs now (their .rhai files remain as the
/// entries Home lists and other apps launch).
const NATIVE_EDITOR: &str = "system/apps/editor.rhai";
const NATIVE_FILES: &str = "system/apps/files.rhai";

pub fn launch(path: &str) -> Result<(), String> {
    if path == NATIVE_EDITOR {
        // Files hands the document over in system/editor-open.
        let doc = fs::read_string("system/editor-open").ok().filter(|p| !p.trim().is_empty());
        let _ = fs::delete("system/editor-open");
        crate::editor::open(doc.map(|p| p.trim().into()));
        return Ok(());
    }
    if path == NATIVE_FILES {
        crate::files::open("");
        return Ok(());
    }
    if !path.ends_with(".rhai") {
        // A document: open it in the Text Editor.
        crate::editor::open(Some(path.into()));
        return Ok(());
    }
    let src = fs::read_string(path)?;
    let meta = info(path, &src);
    let win = wm::open(&meta.title, meta.icon, meta.color, meta.width, meta.height, Content::Canvas(Canvas::new(meta.width as usize, meta.height as usize)));
    let name: String = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".rhai").into();
    let title = meta.title.clone();
    task::spawn(&title, move || {
        run(&name, &src, win.clone(), meta.width, meta.height);
        wm::close(&win);
    });
    Ok(())
}

fn run(name: &str, src: &str, win: WindowRef, w: i32, h: i32) {
    let (mut app, ctx) = match Running::load(name, src, Some(win.clone()), w, h) {
        Ok(r) => r,
        Err(e) => {
            // Show the error in the window until it is closed.
            let mut c = Canvas::new(w as usize, h as usize);
            c.clear(0x2A1620);
            c.text(12, 12, "This app could not start:", 0xFF8A8A);
            for (i, line) in wrap(&e, (w as usize / 8).saturating_sub(3)).iter().enumerate() {
                c.text(12, 40 + i as i32 * 18, line, 0xF0D0D0);
            }
            wm::present(&win, &c);
            while wm::wait_event(&win, None) != Some(Event::Close) {}
            return;
        }
    };
    let has_tick = Running::has_fn(&app.ast, "tick", 0);
    app.draw();
    wm::present(&win, &app.canvas.try_lock().expect("canvas"));
    loop {
        let event = wm::wait_event(&win, has_tick.then_some(pit::HZ / 10));
        match event {
            None => app.call("tick", (), 0),
            Some(Event::Close) => return,
            Some(Event::Key(k)) => app.call("key", (key_name(k),), 1),
            Some(Event::MouseDown { x, y, button }) => {
                app.call(if button == 2 { "right_click" } else { "click" }, (x as INT, y as INT), 2);
            }
            Some(Event::MouseUp { x, y }) => app.call("release", (x as INT, y as INT), 2),
            Some(Event::MouseMove { x, y }) => app.call("drag", (x as INT, y as INT), 2),
            Some(Event::Wheel(n)) => app.call("scroll", (n as INT,), 1),
            Some(Event::Resized) => {
                let (w, h) = wm::size(&win);
                app.canvas.try_lock().expect("canvas").resize(w as usize, h as usize);
            }
        }
        if ctx.quit.load(Ordering::Relaxed) {
            return;
        }
        app.draw();
        wm::present(&win, &app.canvas.try_lock().expect("canvas"));
    }
}

/// Result of [`check`]: problems found, and a rendering of the first frame.
pub struct CheckReport {
    pub problems: Vec<String>,
    pub png: Option<Vec<u8>>,
    pub title: String,
}

/// Load an app without a window, run `init`, an optional list of simulated
/// events, and `draw`; report errors and the rendered frame.
pub fn check(path: &str, events: &[String]) -> Result<CheckReport, String> {
    let src = fs::read_string(path)?;
    let meta = info(path, &src);
    let mut problems = Vec::new();
    if !src.lines().take(20).any(|l| l.trim_start().starts_with("// title:")) {
        problems.push("missing `// title:` header comment".into());
    }
    let (mut app, _) = match Running::load("check", &src, None, meta.width, meta.height) {
        Ok(r) => r,
        Err(e) => return Ok(CheckReport { problems: alloc::vec![e], png: None, title: meta.title }),
    };
    for ev in events {
        let parts: Vec<&str> = ev.split_whitespace().collect();
        match parts.as_slice() {
            ["click", x, y] => app.call("click", (x.parse::<INT>().unwrap_or(0), y.parse::<INT>().unwrap_or(0)), 2),
            ["key", k] => app.call("key", (k.to_string(),), 1),
            ["tick"] => app.call("tick", (), 0),
            ["tick", n] => {
                for _ in 0..n.parse::<u32>().unwrap_or(1).min(600) {
                    app.call("tick", (), 0);
                }
            }
            _ => problems.push(format!("unknown simulated event {ev:?} (use \"click X Y\", \"key K\", \"tick\", \"tick N\")")),
        }
    }
    app.draw();
    if let Some(e) = &app.error {
        problems.push(e.clone());
    }
    let png = Some(app.canvas.try_lock().expect("canvas").to_png());
    Ok(CheckReport { problems, png, title: meta.title })
}

/// Render an app's current frame without a window (for desktop previews): load it,
/// run init, `ticks` ticks and draw. None if it fails.
pub fn snapshot(path: &str, ticks: u32) -> Option<Canvas> {
    if path == NATIVE_EDITOR || path == NATIVE_FILES {
        return None; // native app: Home shows its poster
    }
    let src = fs::read_string(path).ok()?;
    let meta = info(path, &src);
    let (mut app, _) = Running::load("preview", &src, None, meta.width, meta.height).ok()?;
    for _ in 0..ticks.min(50) {
        app.call("tick", (), 0);
    }
    app.draw();
    if app.error.is_some() {
        return None;
    }
    let c = app.canvas.try_lock()?.clone();
    Some(c)
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<Box<Running>>();
}
