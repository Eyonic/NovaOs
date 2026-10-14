//! Nova Browser: the Chromium engine for the web, a Rust shell around it.
//!
//! The engine is Chromium's headless shell (`/opt/chromium`, see
//! scripts/fetch-chromium.sh), run as a Linux program through Nova's compatibility
//! layer and controlled over the DevTools protocol on a pipe (fds 3 and 4,
//! `--remote-debugging-pipe`): JSON messages separated by NUL bytes.
//!
//! Nova's side, all Rust: the window and toolbar (back, forward, reload, address
//! bar with search, Shields), input forwarding (mouse, wheel, keyboard), the page
//! picture (screencast frames, PNG), and Shields (shields.rs), which sees every
//! request the engine makes (Fetch domain) and blocks, upgrades or rewrites it.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use serde_json::{json, Value};

use crate::gfx::{Canvas, Rect};
use crate::input::Key;
use crate::process::Process;
use crate::shields::{self, Shields, Verdict};
use crate::wm::{self, Content, Event, WindowRef};
use crate::{fs, ipc, linux, pit, process, task};

const ENGINE: &str = "opt/chromium/chrome-headless-shell";
const HOME: &str = "https://example.com";
const TOOLBAR: i32 = 44;
const STATUS: i32 = 24;

pub fn engine_installed() -> bool {
    fs::exists(ENGINE)
}

/// DevTools protocol client over the engine's pipe.
struct Cdp {
    to_engine: ipc::End,
    from_engine: ipc::End,
    buf: Vec<u8>,
    next_id: u64,
    events: VecDeque<Value>,
    replies: Vec<Value>,
    process: Arc<Process>,
}

impl Cdp {
    fn start(width: i32, height: i32) -> Result<Cdp, String> {
        let (engine_reads, to_engine) = linux::kernel_pipe(true);
        let (engine_writes, from_engine) = linux::kernel_pipe(false);
        let args = [
            "chrome-headless-shell",
            "--no-sandbox",
            "--single-process",
            "--no-zygote",
            "--disable-gpu",
            "--remote-debugging-pipe",
            "--incognito",
            "--hide-scrollbars",
            // Nova has no sound device: play media into a silent sink instead of failing to
            // open ALSA (which breaks some players).
            "--disable-audio-output",
            &format!("--window-size={width},{height}"),
            "about:blank",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let process = process::spawn(ENGINE, args, alloc::vec![(3, engine_reads), (4, engine_writes)], None)?;
        Ok(Cdp { to_engine, from_engine, buf: Vec::new(), next_id: 1, events: VecDeque::new(), replies: Vec::new(), process })
    }

    fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        let mut bytes = serde_json::to_vec(&msg).unwrap_or_default();
        bytes.push(0);
        let _ = self.to_engine.send_bytes(&bytes);
        id
    }

    /// Read what the engine sent; Err when it is gone.
    fn pump(&mut self) -> Result<(), String> {
        let data = self.from_engine.recv_bytes().map_err(|_| String::from("the engine stopped"))?;
        self.buf.extend_from_slice(&data);
        while let Some(i) = self.buf.iter().position(|&b| b == 0) {
            let raw: Vec<u8> = self.buf.drain(..=i).collect();
            if let Ok(v) = serde_json::from_slice::<Value>(&raw[..raw.len() - 1]) {
                if v.get("id").is_some() { self.replies.push(v) } else { self.events.push_back(v) }
            }
        }
        Ok(())
    }

    /// Send and wait for the reply (events that arrive meanwhile are kept).
    fn call(&mut self, method: &str, params: Value, session: Option<&str>, timeout_s: u64) -> Result<Value, String> {
        let id = self.send(method, params, session);
        let end = pit::ticks() + timeout_s * pit::HZ;
        loop {
            self.pump()?;
            if let Some(i) = self.replies.iter().position(|r| r["id"].as_u64() == Some(id)) {
                let r = self.replies.remove(i);
                if let Some(e) = r.get("error") {
                    return Err(format!("{method}: {}", e["message"].as_str().unwrap_or("error")));
                }
                return Ok(r["result"].clone());
            }
            if pit::ticks() > end {
                return Err(format!("{method}: no answer from the engine"));
            }
            ipc::wait(Some(pit::ticks() + 2));
        }
    }
}

impl Drop for Cdp {
    fn drop(&mut self) {
        process::kill(&self.process);
    }
}

struct Browser {
    win: WindowRef,
    cdp: Option<Cdp>,
    session: String,
    shields: Shields,
    url: String,
    title: String,
    /// Address bar text while editing.
    editing: Option<String>,
    page: Option<Canvas>,
    status: String,
    blocked: Vec<String>,
    upgraded: u32,
    /// Recent network requests (id, URL), to name the ones that fail.
    requests: Vec<(String, String)>,
    loaded_hosts: Vec<String>,
    probe_at: u64,
    probe_text: String,
    loading: bool,
    panel: bool,
    viewport: (i32, i32),
    error: Option<String>,
    dirty: bool,
}

fn log_loads() -> bool {
    crate::limine::EXECUTABLE_CMDLINE.get().is_some_and(|c| c.split_whitespace().any(|a| a == "nova.browser_log"))
}

/// Open a browser window (at `url`, or the home page).
pub fn launch(url: Option<String>) {
    let (sw, sh) = wm::work_area();
    let (w, h) = ((sw - 120).min(1180), (sh - 90).min(820));
    let win = wm::open("Nova Browser", '@', 0xFB542B, w, h, Content::Canvas(Canvas::new(w as usize, h as usize)));
    let win2 = win.clone();
    task::spawn("browser", move || {
        let mut b = Browser {
            win: win2.clone(),
            cdp: None,
            session: String::new(),
            shields: Shields::load(),
            url: url.unwrap_or_else(|| HOME.into()),
            title: String::new(),
            editing: None,
            page: None,
            status: "Starting the Chromium engine...".into(),
            blocked: Vec::new(),
            upgraded: 0,
            requests: Vec::new(),
            loaded_hosts: Vec::new(),
            probe_at: 0,
            probe_text: String::new(),
            loading: true,
            panel: false,
            viewport: (w, h - TOOLBAR - STATUS),
            error: None,
            dirty: true,
        };
        b.render();
        if !engine_installed() {
            b.fail("The Chromium engine is not installed. On the build machine run scripts/fetch-chromium.sh, then scripts/build.sh.".into());
        } else if let Err(e) = b.start() {
            b.fail(format!("Could not start the engine: {e}"));
        }
        b.run();
        drop(b.cdp.take());
        wm::close(&win2);
    });
    let _ = win;
}

fn b64decode(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes().filter_map(val) {
        acc = acc << 6 | c as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else if b == b' ' { "+".into() } else { format!("%{b:02X}") })
        .collect()
}

/// What the address bar text means: a URL, a host, or a search.
fn interpret(text: &str) -> String {
    let t = text.trim();
    if t.contains("://") || t.starts_with("about:") || t.starts_with("data:") {
        t.into()
    } else if t.contains('.') && !t.contains(' ') {
        format!("https://{t}")
    } else {
        format!("https://duckduckgo.com/html/?q={}", percent_encode(t))
    }
}

impl Browser {
    fn fail(&mut self, msg: String) {
        self.error = Some(msg);
        self.loading = false;
        self.dirty = true;
        self.render();
    }

    fn start(&mut self) -> Result<(), String> {
        let (w, h) = self.viewport;
        let mut cdp = Cdp::start(w, h)?;
        let targets = cdp.call("Target.getTargets", json!({}), None, 60)?;
        let target = targets["targetInfos"].as_array().and_then(|t| t.iter().find(|t| t["type"] == "page")).and_then(|t| t["targetId"].as_str()).map(String::from);
        let target = match target {
            Some(t) => t,
            None => cdp.call("Target.createTarget", json!({"url": "about:blank"}), None, 30)?["targetId"].as_str().unwrap_or("").into(),
        };
        let att = cdp.call("Target.attachToTarget", json!({"targetId": target, "flatten": true}), None, 30)?;
        let session = att["sessionId"].as_str().ok_or("no session")?.to_string();
        let s = Some(session.as_str());
        cdp.call("Page.enable", json!({}), s, 30)?;
        // The engine is the headless build of Chromium and says "HeadlessChrome", which
        // some sites (YouTube's player) refuse to serve. Nova Browser is an interactive
        // browser, so identify as the plain Chromium it is.
        let version = cdp.call("Browser.getVersion", json!({}), None, 30)?;
        let ua = version["userAgent"].as_str().unwrap_or("").replace("HeadlessChrome/", "Chrome/");
        let full = version["product"].as_str().and_then(|p| p.split('/').nth(1)).unwrap_or("0").to_string();
        let major = full.split('.').next().unwrap_or("0").to_string();
        cdp.call(
            "Emulation.setUserAgentOverride",
            json!({
                "userAgent": ua,
                "userAgentMetadata": {
                    "brands": [{"brand": "Chromium", "version": major}, {"brand": "Not=A?Brand", "version": "24"}],
                    "fullVersionList": [{"brand": "Chromium", "version": full}, {"brand": "Not=A?Brand", "version": "24.0.0.0"}],
                    "platform": "Linux", "platformVersion": "", "architecture": "x86", "bitness": "64", "model": "", "mobile": false,
                },
            }),
            s,
            30,
        )?;
        // Network events: failed requests are logged (with Chromium's reason) for diagnosis.
        cdp.call("Network.enable", json!({"maxTotalBufferSize": 0, "maxResourceBufferSize": 0}), s, 30)?;
        cdp.call("Fetch.enable", json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}), s, 30)?;
        cdp.call("Emulation.setDeviceMetricsOverride", json!({"width": w, "height": h, "deviceScaleFactor": 1, "mobile": false}), s, 30)?;
        cdp.call("Page.startScreencast", json!({"format": "png", "everyNthFrame": 1}), s, 30)?;
        self.session = session;
        self.cdp = Some(cdp);
        let url = self.url.clone();
        self.navigate(&url);
        Ok(())
    }

    fn cdp_send(&mut self, method: &str, params: Value) {
        let session = self.session.clone();
        if let Some(c) = &mut self.cdp {
            c.send(method, params, Some(&session));
        }
    }

    fn navigate(&mut self, url: &str) {
        self.url = url.into();
        self.loading = true;
        self.status = format!("Loading {url}");
        self.blocked.clear();
        self.upgraded = 0;
        self.cdp_send("Page.navigate", json!({"url": url}));
        self.dirty = true;
    }

    fn history_go(&mut self, delta: i64) {
        let session = self.session.clone();
        let Some(c) = &mut self.cdp else { return };
        if let Ok(h) = c.call("Page.getNavigationHistory", json!({}), Some(&session), 10) {
            let idx = h["currentIndex"].as_i64().unwrap_or(0) + delta;
            if let Some(entry) = h["entries"].as_array().and_then(|e| e.get(idx as usize)) {
                let id = entry["id"].clone();
                c.send("Page.navigateToHistoryEntry", json!({"entryId": id}), Some(&session));
            }
        }
    }

    fn handle_engine_event(&mut self, ev: Value) {
        let params = &ev["params"];
        match ev["method"].as_str().unwrap_or("") {
            "Fetch.requestPaused" => {
                let id = params["requestId"].clone();
                let url = params["request"]["url"].as_str().unwrap_or("");
                let is_doc = params["resourceType"] == "Document" && params.get("redirectedRequestId").is_none();
                let page = if is_doc { "" } else { self.url.as_str() };
                match self.shields.check(url, page, is_doc) {
                    Verdict::Allow => self.cdp_send("Fetch.continueRequest", json!({"requestId": id})),
                    Verdict::Block(host) => {
                        crate::log::info(&format!("shields: blocked {host}"));
                        self.blocked.push(host);
                        self.cdp_send("Fetch.failRequest", json!({"requestId": id, "errorReason": "BlockedByClient"}));
                        self.dirty = true;
                    }
                    Verdict::Rewrite(new) => {
                        crate::log::info(&format!("shields: rewrote {url} -> {new}"));
                        self.upgraded += 1;
                        self.cdp_send("Fetch.continueRequest", json!({"requestId": id, "url": new}));
                    }
                }
            }
            "Network.requestWillBeSent" => {
                let id = params["requestId"].as_str().unwrap_or("").to_string();
                let url = params["request"]["url"].as_str().unwrap_or("");
                if self.requests.len() > 512 {
                    self.requests.remove(0);
                }
                self.requests.push((id, url.chars().take(160).collect()));
            }
            "Network.responseReceived" if log_loads() => {
                // Diagnostics (kernel option nova.browser_log): every host that served a
                // request, so filter-list gaps show up (a tracker host listed here got through).
                let host = crate::shields::host_of(params["response"]["url"].as_str().unwrap_or("")).to_string();
                if !host.is_empty() && !self.loaded_hosts.contains(&host) {
                    crate::log::info(&format!("browser: loaded from {host}"));
                    self.loaded_hosts.push(host);
                }
            }
            "Network.loadingFailed" => {
                let id = params["requestId"].as_str().unwrap_or("");
                let url = self.requests.iter().find(|(r, _)| r == id).map_or("?", |(_, u)| u.as_str());
                let why = params["errorText"].as_str().unwrap_or("");
                let blocked = params["blockedReason"].as_str().or(params["corsErrorStatus"]["corsError"].as_str()).unwrap_or("");
                if !params["canceled"].as_bool().unwrap_or(false) {
                    crate::log::warn(&format!("browser: {url}: {why} {blocked}"));
                }
            }
            "Page.screencastFrame" => {
                let sid = params["sessionId"].clone();
                let png = b64decode(params["data"].as_str().unwrap_or(""));
                if let Ok(c) = Canvas::from_png(&png) {
                    self.page = Some(c);
                    self.dirty = true;
                }
                self.cdp_send("Page.screencastFrameAck", json!({"sessionId": sid}));
            }
            "Page.frameNavigated" => {
                if params["frame"].get("parentId").is_none() {
                    self.url = params["frame"]["url"].as_str().unwrap_or("").into();
                    self.dirty = true;
                }
            }
            "Page.frameStartedLoading" => {
                self.loading = true;
                self.dirty = true;
            }
            "Page.loadEventFired" | "Page.frameStoppedLoading" => {
                self.loading = false;
                self.status = format!("Done. Shields blocked {} request(s){}", self.blocked.len(), if self.upgraded > 0 { format!(", {} upgraded to HTTPS", self.upgraded) } else { String::new() });
                let session = self.session.clone();
                if let Some(c) = &mut self.cdp
                    && let Ok(r) = c.call("Runtime.evaluate", json!({"expression": "document.title", "returnByValue": true}), Some(&session), 5)
                {
                    self.title = r["result"]["value"].as_str().unwrap_or("").into();
                    let t = if self.title.is_empty() { String::from("Nova Browser") } else { format!("{} - Nova Browser", self.title) };
                    wm::set_title(&self.win, &t);
                }
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn key_event(&mut self, k: Key) {
        let (key, code, vk, text): (&str, &str, i64, &str) = match k {
            Key::Char(b'\n') => ("Enter", "Enter", 13, "\r"),
            Key::Char(0x08) => ("Backspace", "Backspace", 8, ""),
            Key::Char(b'\t') => ("Tab", "Tab", 9, ""),
            Key::Char(0x1B) => ("Escape", "Escape", 27, ""),
            Key::Up => ("ArrowUp", "ArrowUp", 38, ""),
            Key::Down => ("ArrowDown", "ArrowDown", 40, ""),
            Key::Left => ("ArrowLeft", "ArrowLeft", 37, ""),
            Key::Right => ("ArrowRight", "ArrowRight", 39, ""),
            Key::Char(c) if c >= 0x20 => {
                let s = (c as char).to_string();
                self.cdp_send("Input.dispatchKeyEvent", json!({"type": "keyDown", "text": s, "key": s, "unmodifiedText": s}));
                self.cdp_send("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": s}));
                return;
            }
            _ => return,
        };
        self.cdp_send("Input.dispatchKeyEvent", json!({"type": "keyDown", "key": key, "code": code, "windowsVirtualKeyCode": vk, "text": text}));
        self.cdp_send("Input.dispatchKeyEvent", json!({"type": "keyUp", "key": key, "code": code, "windowsVirtualKeyCode": vk}));
    }

    fn page_host(&self) -> String {
        shields::host_of(&self.url).into()
    }

    fn input(&mut self, e: Event) -> bool {
        let (w, _) = wm::size(&self.win);
        match e {
            Event::Close => return false,
            Event::Key(Key::Char(0x0C)) => self.editing = Some(self.url.clone()), // Ctrl-L
            Event::Key(Key::Char(0x12)) => self.cdp_send("Page.reload", json!({})), // Ctrl-R
            Event::Key(k) if self.editing.is_some() => {
                let text = self.editing.as_mut().unwrap();
                match k {
                    Key::Char(b'\n') => {
                        let target = interpret(text);
                        self.editing = None;
                        self.navigate(&target);
                    }
                    Key::Char(0x1B) => self.editing = None,
                    Key::Char(0x08) => {
                        text.pop();
                    }
                    Key::Char(c) if c >= 0x20 => text.push(c as char),
                    _ => {}
                }
            }
            Event::Key(k) => self.key_event(k),
            Event::MouseDown { x, y, .. } if y < TOOLBAR => {
                self.panel = false;
                if (8..40).contains(&x) {
                    self.history_go(-1);
                } else if (44..76).contains(&x) {
                    self.history_go(1);
                } else if (80..112).contains(&x) {
                    self.cdp_send("Page.reload", json!({}));
                } else if x >= w - 60 && x < w - 14 {
                    self.panel = true;
                } else if x >= 120 && x < w - 70 {
                    self.editing = Some(String::new());
                }
            }
            Event::MouseDown { x, y, .. } if self.panel => {
                // The panel's toggle button.
                let p = self.panel_rect(w);
                if Rect::new(p.x + 16, p.y + p.h - 46, p.w - 32, 30).contains(x, y) {
                    let host = self.page_host();
                    self.shields.toggle_site(&host);
                    self.cdp_send("Page.reload", json!({}));
                }
                self.panel = false;
            }
            Event::MouseDown { x, y, button } => {
                self.editing = None;
                let b = if button == 2 { "right" } else { "left" };
                self.cdp_send("Input.dispatchMouseEvent", json!({"type": "mousePressed", "x": x, "y": y - TOOLBAR, "button": b, "clickCount": 1}));
            }
            Event::MouseUp { x, y } => {
                self.cdp_send("Input.dispatchMouseEvent", json!({"type": "mouseReleased", "x": x, "y": y - TOOLBAR, "button": "left", "clickCount": 1}));
            }
            Event::MouseMove { x, y } => {
                self.cdp_send("Input.dispatchMouseEvent", json!({"type": "mouseMoved", "x": x, "y": y - TOOLBAR, "button": "left"}));
            }
            Event::Wheel(n) => {
                let (vw, vh) = self.viewport;
                self.cdp_send("Input.dispatchMouseEvent", json!({"type": "mouseWheel", "x": vw / 2, "y": vh / 2, "deltaX": 0, "deltaY": n * 120}));
            }
            Event::Resized => {
                let (w, h) = wm::size(&self.win);
                self.viewport = (w, h - TOOLBAR - STATUS);
                let (vw, vh) = self.viewport;
                self.cdp_send("Emulation.setDeviceMetricsOverride", json!({"width": vw, "height": vh, "deviceScaleFactor": 1, "mobile": false}));
            }
        }
        self.dirty = true;
        true
    }

    fn run(&mut self) {
        loop {
            if let Some(e) = wm::wait_event(&self.win, Some(1)) {
                if !self.input(e) {
                    return;
                }
                // Drain queued input quickly.
                while let Some(e) = wm::wait_event(&self.win, Some(0)) {
                    if !self.input(e) {
                        return;
                    }
                }
            }
            if let Some(c) = &mut self.cdp {
                if let Err(e) = c.pump() {
                    self.cdp = None;
                    self.fail(format!("The Chromium engine stopped ({e}). Close this window and open the browser again."));
                } else {
                    while let Some(ev) = self.cdp.as_mut().and_then(|c| c.events.pop_front()) {
                        self.handle_engine_event(ev);
                    }
                }
            }
            if log_loads() && pit::ticks() > self.probe_at {
                self.probe_page_text();
            }
            if self.dirty {
                self.render();
            }
        }
    }

    /// Diagnostics (nova.browser_log): log the page's visible text when it changes, every
    /// 10 seconds, so test pages' results (adblock-tester.com's score) can be read from
    /// the serial log.
    fn probe_page_text(&mut self) {
        self.probe_at = pit::ticks() + 10 * pit::HZ;
        let session = self.session.clone();
        let Some(c) = &mut self.cdp else { return };
        let expr = "document.body ? document.body.innerText.replace(/\\s+/g, ' ').slice(0, 6000) : ''";
        if let Ok(r) = c.call("Runtime.evaluate", json!({"expression": expr, "returnByValue": true}), Some(&session), 5) {
            let text = r["result"]["value"].as_str().unwrap_or("").to_string();
            if text != self.probe_text {
                crate::log::info(&format!("browser: page text: {text}"));
                self.probe_text = text;
            }
        }
    }

    fn panel_rect(&self, w: i32) -> Rect {
        Rect::new(w - 330, TOOLBAR + 6, 320, 250)
    }

    fn render(&mut self) {
        self.dirty = false;
        let (w, h) = wm::size(&self.win);
        let mut c = Canvas::new(w as usize, h as usize);
        c.clear(0xFFFFFF);
        // Page.
        match (&self.page, &self.error) {
            (_, Some(err)) => {
                c.fill_rect(Rect::new(0, TOOLBAR, w, h - TOOLBAR), 0x1E1F24);
                c.text(24, TOOLBAR + 30, "Nova Browser", 0xFFFFFF);
                let mut y = TOOLBAR + 60;
                for chunk in err.as_bytes().chunks(((w - 48) / 8).max(10) as usize) {
                    c.text(24, y, core::str::from_utf8(chunk).unwrap_or(""), 0xFF9C8A);
                    y += 18;
                }
            }
            (Some(p), None) => c.blit(p, p.bounds(), 0, TOOLBAR),
            (None, None) => {
                c.fill_rect(Rect::new(0, TOOLBAR, w, h - TOOLBAR), 0x1E1F24);
                c.text_scaled(w / 2 - 96, h / 2 - 40, "Nova", 0xFB542B, 4);
                c.text(w / 2 - (self.status.len() as i32 * 4), h / 2 + 40, &self.status, 0xC8C8D0);
            }
        }
        // Toolbar.
        c.fill_rect(Rect::new(0, 0, w, TOOLBAR), 0x2B2C31);
        for (i, label) in ["\u{2190}", "\u{2192}", "\u{25cb}"].iter().enumerate() {
            let x = 8 + i as i32 * 36;
            c.fill_round_rect(Rect::new(x, 8, 32, 28), 6, 0x3A3B41);
            c.text(x + 12, 14, label, 0xE8E8EE);
        }
        let bar = Rect::new(120, 7, w - 120 - 70, 30);
        let editing = self.editing.is_some();
        c.fill_round_rect(bar, 15, if editing { 0x4A4B53 } else { 0x3A3B41 });
        let shown = self.editing.clone().unwrap_or_else(|| self.url.clone());
        let secure = self.url.starts_with("https://");
        let mut tx = bar.x + 14;
        if !editing {
            c.text(tx, bar.y + 7, if secure { "\u{25a0}" } else { "!" }, if secure { 0x7BD88F } else { 0xFFB86C });
            tx += 16;
        }
        c.text_clipped(tx, bar.y + 7, &shown, 0xF2F2F5, bar.w - (tx - bar.x) - 12);
        if editing {
            let cx = tx + shown.chars().count() as i32 * 8;
            c.fill_rect(Rect::new(cx.min(bar.x + bar.w - 12), bar.y + 6, 2, 18), 0xFB542B);
        }
        // Shields button: a shield with the blocked count.
        let host = self.page_host();
        let up = !self.shields.site_allowed(&host);
        let sx = w - 58;
        let col = if up { 0xFB542B } else { 0x6B6C75 };
        c.fill_round_rect(Rect::new(sx, 8, 26, 22), 5, col);
        for i in 0..8 {
            c.fill_rect(Rect::new(sx + 3 + i, 30 + i, 20 - 2 * i, 1), col);
        }
        let n = self.blocked.len().to_string();
        c.text(sx + 13 - n.len() as i32 * 4, 11, &n, 0xFFFFFF);
        // Status bar.
        c.fill_rect(Rect::new(0, h - STATUS, w, STATUS), 0x2B2C31);
        let status = if self.loading { format!("{} ...", self.status) } else { self.status.clone() };
        c.text_clipped(10, h - STATUS + 4, &status, 0xB0B0BA, w - 20);
        // Shields panel.
        if self.panel {
            let p = self.panel_rect(w);
            c.shadow(p, 10);
            c.fill_round_rect(p, 10, 0x2B2C31);
            c.text(p.x + 16, p.y + 14, &format!("Shields {} for {}", if up { "UP" } else { "DOWN" }, host), 0xFFFFFF);
            c.text(p.x + 16, p.y + 40, &format!("{} trackers and ads blocked", self.blocked.len()), 0xFB542B);
            c.text(p.x + 16, p.y + 60, &format!("{} connection(s) upgraded to HTTPS", self.upgraded), 0xC8C8D0);
            let mut hosts: Vec<&String> = self.blocked.iter().collect();
            hosts.sort();
            hosts.dedup();
            for (i, hst) in hosts.iter().take(6).enumerate() {
                c.text_clipped(p.x + 24, p.y + 86 + i as i32 * 18, hst, 0x9A9AA5, p.w - 40);
            }
            c.text(p.x + 16, p.y + p.h - 70, &format!("{} filter list rules + built-in list", self.shields.list_rules), 0x7A7A85);
            let btn = Rect::new(p.x + 16, p.y + p.h - 46, p.w - 32, 30);
            c.fill_round_rect(btn, 8, if up { 0x4A4B53 } else { 0xFB542B });
            let label = if up { "Turn Shields down for this site" } else { "Turn Shields up for this site" };
            c.text(btn.x + (btn.w - label.len() as i32 * 8) / 2, btn.y + 7, label, 0xFFFFFF);
        }
        wm::present(&self.win, &c);
    }
}
