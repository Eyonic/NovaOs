//! Nova's AI assistant: an agentic client in the terminal. It speaks the Anthropic
//! Messages API, against one of two providers: DeepSeek (the default; DeepSeek serves
//! that API at api.deepseek.com/anthropic) as "Nova Assistant", or Anthropic as
//! "Claude Code for Nova".
//!
//! Talks to the Messages API directly (HTTPS via net/tls/http), streams
//! responses (server-sent events), and runs a tool loop: Claude can read and write
//! files, run shell commands, fetch URLs, inspect hardware, and create desktop apps,
//! checking them with a headless run that returns a screenshot.
//!
//! Request defaults follow Anthropic's current guidance: model `claude-opus-5-5`,
//! adaptive thinking (summaries shown dimmed), `effort: high`, streaming with
//! `eager_input_streaming` on client tools, automatic prompt caching, and
//! server-side refusal fallbacks (`fallbacks: "default"`).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use core::sync::atomic::{AtomicU8, Ordering};

use serde_json::{json, Map, Value};

use crate::input::{self, Key};
use crate::lineedit::{self, History, ReadResult};
use crate::{apps, fs, fwcfg, gfx, http, limine, net, pit, print, println, task};

const API_VERSION: &str = "2023-06-01";
const CONFIG_PATH: &str = "system/claude.json";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// fw_cfg file scripts/run.sh fills from the host's `ant auth print-credentials --access-token`.
const HOST_CREDENTIAL: &str = "opt/nova/claude-credential";
const MAX_TOKENS: u64 = 64000;

const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const ORANGE: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";

/// Which service answers. Both speak the Messages API; DeepSeek's version differs in
/// models, effort levels, how it authenticates /models, and that it ignores images
/// inside tool results (Nova sends those beside the result instead).
#[derive(Clone, Copy, PartialEq)]
pub enum Provider {
    DeepSeek,
    Anthropic,
}

impl Provider {
    fn id(self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek",
            Self::Anthropic => "anthropic",
        }
    }
    fn from_id(s: &str) -> Option<Self> {
        match s {
            "deepseek" => Some(Self::DeepSeek),
            "anthropic" | "claude" => Some(Self::Anthropic),
            _ => None,
        }
    }
    /// Service name shown to the user.
    pub fn name(self) -> &'static str {
        match self {
            Self::DeepSeek => "DeepSeek",
            Self::Anthropic => "Claude",
        }
    }
    /// Name of the desktop app.
    pub fn app_name(self) -> &'static str {
        match self {
            Self::DeepSeek => "Nova Assistant",
            Self::Anthropic => "Claude Code",
        }
    }
    fn banner(self) -> &'static str {
        match self {
            Self::DeepSeek => "Nova Assistant \u{00b7} DeepSeek",
            Self::Anthropic => "Claude Code for Nova",
        }
    }
    fn default_base_url(self) -> &'static str {
        match self {
            Self::DeepSeek => "https://api.deepseek.com/anthropic",
            Self::Anthropic => "https://api.anthropic.com",
        }
    }
    /// deepseek-flash reads images (app screenshots); deepseek-v4-pro is text only.
    fn default_model(self) -> &'static str {
        match self {
            Self::DeepSeek => "deepseek-flash",
            Self::Anthropic => "claude-opus-5-5",
        }
    }
    fn owns_model(self, model: &str) -> bool {
        match self {
            Self::DeepSeek => model.starts_with("deepseek"),
            Self::Anthropic => model.starts_with("claude"),
        }
    }
    fn keys_url(self) -> &'static str {
        match self {
            Self::DeepSeek => "https://platform.deepseek.com/api_keys",
            Self::Anthropic => "https://console.anthropic.com/settings/keys",
        }
    }
    fn efforts(self) -> &'static [&'static str] {
        match self {
            Self::DeepSeek => &["low", "high", "max"],
            Self::Anthropic => &["low", "medium", "high", "xhigh", "max"],
        }
    }
}

/// Provider of the most recently loaded or changed configuration (0 DeepSeek, 1 Anthropic).
static CURRENT: AtomicU8 = AtomicU8::new(u8::MAX);

fn current() -> Provider {
    match CURRENT.load(Ordering::Relaxed) {
        0 => Provider::DeepSeek,
        1 => Provider::Anthropic,
        _ => Config::load().provider,
    }
}

/// Name of the assistant app for the configured provider (desktop launcher, Settings).
pub fn app_name() -> &'static str {
    current().app_name()
}

/// Name of the AI service ("DeepSeek", "Claude") for messages.
pub fn provider_name() -> &'static str {
    current().name()
}

/// A credential the host passed in for this boot (never written to the Nova disk).
#[derive(Clone)]
pub struct HostCredential {
    pub provider: Provider,
    /// true: an OAuth access token (Authorization: Bearer); false: an API key.
    pub oauth: bool,
    pub secret: String,
    /// Where the host got it, for messages ("ant auth login", "ANTHROPIC_API_KEY", ...).
    pub source: String,
}

impl HostCredential {
    /// File format (written by scripts/run.sh): line 1 "oauth" or "api-key" (Anthropic) or
    /// "deepseek-key", line 2 the secret, line 3 the source description.
    /// Read once per boot: the host cannot change it while Nova runs.
    fn from_fw_cfg() -> Option<Self> {
        static HOST: spin::Once<Option<HostCredential>> = spin::Once::new();
        HOST.call_once(Self::read).clone()
    }

    fn read() -> Option<Self> {
        let data = fwcfg::read_file(HOST_CREDENTIAL)?;
        let text = core::str::from_utf8(&data).ok()?;
        let mut lines = text.lines().map(str::trim);
        let (provider, oauth) = match lines.next()? {
            "oauth" => (Provider::Anthropic, true),
            "api-key" => (Provider::Anthropic, false),
            "deepseek-key" => (Provider::DeepSeek, false),
            _ => return None,
        };
        let secret = lines.next().filter(|s| !s.is_empty() && s.is_ascii())?.into();
        let source = lines.next().filter(|s| !s.is_empty()).unwrap_or("the host").into();
        Some(Self { provider, oauth, secret, source })
    }
}

#[derive(Clone)]
pub struct Config {
    pub provider: Provider,
    /// API key entered with /login (saved in system/claude.json, for `provider`).
    pub api_key: String,
    /// The host's login for this boot; used instead of api_key when present.
    pub host: Option<HostCredential>,
    pub model: String,
    pub effort: String,
    pub base_url: String,
    pub auto_approve: bool,
}

impl Config {
    fn default() -> Self {
        let p = Provider::DeepSeek;
        CURRENT.store(p as u8, Ordering::Relaxed);
        Self { provider: p, api_key: String::new(), host: None, model: p.default_model().into(), effort: "high".into(), base_url: p.default_base_url().into(), auto_approve: false }
    }

    pub fn load() -> Self {
        let mut c = Self::default();
        if let Ok(text) = fs::read_string(CONFIG_PATH)
            && let Ok(v) = serde_json::from_str::<Value>(&text)
        {
            let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
            // Settings from before providers existed were Anthropic's.
            c.set_provider(s("provider").and_then(|p| Provider::from_id(&p)).unwrap_or(Provider::Anthropic));
            c.api_key = s("api_key").unwrap_or_default();
            c.model = s("model").unwrap_or(c.model);
            c.effort = s("effort").unwrap_or(c.effort);
            c.base_url = s("base_url").unwrap_or(c.base_url);
            c.auto_approve = v.get("auto_approve").and_then(|x| x.as_bool()).unwrap_or(false);
        }
        c.host = HostCredential::from_fw_cfg();
        if let Some(h) = &c.host
            && h.provider != c.provider
        {
            c.set_provider(h.provider);
            c.api_key.clear(); // a saved key belongs to the other provider
        }
        if !c.provider.owns_model(&c.model) {
            c.model = c.provider.default_model().into();
        }
        if !c.provider.efforts().contains(&c.effort.as_str()) {
            c.effort = "high".into();
        }
        // Kernel command line overrides (used by the automated tests' mock API server).
        let cmdline: &str = limine::EXECUTABLE_CMDLINE.get().map_or("", |c| c);
        for arg in cmdline.split_whitespace() {
            if let Some(url) = arg.strip_prefix("nova.claude_api=") {
                c.set_provider(Provider::Anthropic);
                c.base_url = url.into();
            } else if let Some(url) = arg.strip_prefix("nova.deepseek_api=") {
                c.set_provider(Provider::DeepSeek);
                c.base_url = url.into();
            }
        }
        if !c.provider.owns_model(&c.model) {
            c.model = c.provider.default_model().into();
        }
        c
    }

    /// Switch provider; the base URL follows unless it was customised.
    fn set_provider(&mut self, p: Provider) {
        if self.base_url == self.provider.default_base_url() {
            self.base_url = p.default_base_url().into();
        }
        self.provider = p;
        CURRENT.store(p as u8, Ordering::Relaxed);
    }

    pub fn save(&self) -> Result<(), String> {
        let v = json!({
            "provider": self.provider.id(), "api_key": self.api_key, "model": self.model, "effort": self.effort,
            "base_url": self.base_url, "auto_approve": self.auto_approve,
        });
        fs::write(CONFIG_PATH, serde_json::to_string_pretty(&v).unwrap_or_default().as_bytes())
    }

    pub fn logged_in(&self) -> bool {
        self.host.is_some() || !self.api_key.is_empty()
    }

    /// Authentication headers: x-api-key, or Bearer plus the OAuth beta for a host login.
    fn auth_headers(&self) -> Vec<(&'static str, String)> {
        match &self.host {
            Some(h) if h.oauth => vec![("authorization", format!("Bearer {}", h.secret))],
            Some(h) => vec![("x-api-key", h.secret.clone())],
            None => vec![("x-api-key", self.api_key.clone())],
        }
    }

    /// anthropic-beta values for a request (comma-separated), if any.
    fn betas(&self, fallbacks: bool) -> Option<String> {
        let mut b = Vec::new();
        if fallbacks {
            b.push(FALLBACK_BETA);
        }
        if self.host.as_ref().is_some_and(|h| h.oauth) {
            b.push(OAUTH_BETA);
        }
        (!b.is_empty()).then(|| b.join(","))
    }

    fn describe_login(&self) -> String {
        match &self.host {
            Some(h) if h.oauth => format!("your Anthropic CLI login ({})", h.source),
            Some(h) => format!("a {} API key from the host ({})", h.provider.name(), h.source),
            None => format!("API key {}", self.masked_key()),
        }
    }

    fn masked_key(&self) -> String {
        let k = &self.api_key;
        if k.len() > 14 { format!("{}...{}", &k[..10], &k[k.len() - 4..]) } else { "****".into() }
    }
}

/// One line for the Settings app.
pub fn status_line() -> String {
    let c = Config::load();
    if c.logged_in() { format!("Signed in with {} - model {}", c.describe_login(), c.model) } else { format!("Not signed in. Open {} to log in.", c.provider.app_name()) }
}

// ---------------------------------------------------------------------------
// Output formatting: word wrap plus a little Markdown (bold, inline code, headings).

struct Writer {
    width: usize,
    col: usize,
    indent: usize,
    word: String,
    word_len: usize,
    bold: bool,
    code: bool,
    pending_star: bool,
    line_start: bool,
    style: &'static str,
}

impl Writer {
    fn new(indent: usize, style: &'static str) -> Self {
        let width = task::current_tty().map(|t| t.lock().grid.cols).unwrap_or(100);
        Self { width: width.max(20), col: indent, indent, word: String::new(), word_len: 0, bold: false, code: false, pending_star: false, line_start: true, style }
    }

    fn flush_word(&mut self) {
        if self.word.is_empty() {
            return;
        }
        if self.col + self.word_len > self.width && self.col > self.indent {
            print!("\n{}", " ".repeat(self.indent));
            self.col = self.indent;
        }
        print!("{}", self.word);
        self.col += self.word_len;
        self.word.clear();
        self.word_len = 0;
    }

    fn restyle(&self) -> String {
        let mut s = String::from(RESET);
        s.push_str(self.style);
        if self.bold {
            s.push_str(BOLD);
        }
        if self.code {
            s.push_str(CYAN);
        }
        s
    }

    fn push(&mut self, text: &str) {
        for c in text.chars() {
            if self.pending_star {
                self.pending_star = false;
                if c == '*' {
                    self.bold = !self.bold;
                    let st = self.restyle();
                    self.word.push_str(&st);
                    continue;
                }
                self.word.push('*');
                self.word_len += 1;
            }
            match c {
                '*' if !self.code => self.pending_star = true,
                '`' => {
                    self.code = !self.code;
                    let st = self.restyle();
                    self.word.push_str(&st);
                }
                '#' if self.line_start => {} // heading markers: drop, keep the text
                '\n' => {
                    self.flush_word();
                    print!("\n{}", " ".repeat(self.indent));
                    self.col = self.indent;
                    self.line_start = true;
                    continue;
                }
                ' ' => {
                    self.flush_word();
                    if self.col < self.width {
                        print!(" ");
                        self.col += 1;
                    }
                }
                c => {
                    self.word.push(c);
                    self.word_len += 1;
                }
            }
            self.line_start = false;
        }
    }

    fn finish(&mut self) {
        if self.pending_star {
            self.word.push('*');
            self.word_len += 1;
            self.pending_star = false;
        }
        self.flush_word();
        print!("{RESET}");
    }
}

// ---------------------------------------------------------------------------
// Tools

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "eager_input_streaming": true,
        "input_schema": { "type": "object", "properties": properties, "required": required },
    })
}

fn tools() -> Value {
    json!([
        tool("read_file", "Read a file from the Nova filesystem. Paths are flat strings like 'apps/clock.rhai' (directories are just prefixes).",
            json!({"path": {"type": "string"}}), &["path"]),
        tool("write_file", "Create or overwrite a file on the Nova filesystem with the full contents.",
            json!({"path": {"type": "string"}, "content": {"type": "string", "description": "Full file contents"}}), &["path", "content"]),
        tool("edit_file", "Replace exactly one occurrence of old_string with new_string in a file.",
            json!({"path": {"type": "string"}, "old_string": {"type": "string"}, "new_string": {"type": "string"}}), &["path", "old_string", "new_string"]),
        tool("list_files", "List files whose path starts with a prefix ('' lists everything), with sizes.",
            json!({"prefix": {"type": "string"}}), &[]),
        tool("delete_file", "Delete a file. Asks the user for permission.",
            json!({"path": {"type": "string"}}), &["path"]),
        tool("run_command", "Run a Nova shell command (see the system prompt for the list) and return its output. Commands that change the system ask the user first.",
            json!({"command": {"type": "string"}}), &["command"]),
        tool("check_app", "Load a Rhai app without a window: compile it, run init(), replay optional simulated events, run draw(), and return any errors plus a PNG screenshot of the window contents. Use it after every change to an app. Pass `reference` (the app's design mockup) to get it back next to the screenshot for comparison.",
            json!({"path": {"type": "string"}, "events": {"type": "array", "items": {"type": "string"}, "description": "Simulated events before drawing: \"click X Y\", \"key K\", \"tick\", \"tick N\""},
                   "reference": {"type": "string", "description": "Optional PNG path of the design mockup to compare against"}}), &["path"]),
        tool("launch_app", "Open an app (a .rhai file) in a window on the user's desktop.",
            json!({"path": {"type": "string"}}), &["path"]),
        tool("http_get", "Fetch a URL (http or https) from Nova's own network stack and return the status and body (truncated to 60 KB).",
            json!({"url": {"type": "string"}}), &["url"]),
        tool("system_info", "Describe this machine: CPU, memory, storage, network, PCI devices and which ones have a Nova driver.",
            json!({}), &[]),
        tool("pci_config", "Dump the 256-byte PCI configuration space and decoded BARs of a device, for writing drivers. Address like '00:1f.2'.",
            json!({"address": {"type": "string"}}), &["address"]),
        tool("open_url", "Open a web page in Nova Browser (Chromium engine) on the user's desktop.",
            json!({"url": {"type": "string"}}), &["url"]),
        tool("generate_image", "Generate an image with the image model (ComfyUI, Z-Image Turbo) and save it as a PNG on the Nova filesystem; you also get to see it. Use it for app artwork: backgrounds, illustrations, icons, sprites, textures, and for visual inspiration before designing an app. Apps draw saved images with g.image(...). Takes roughly 5 to 60 seconds per image.",
            json!({
                "prompt": {"type": "string", "description": "Detailed visual description: subject, style, colors, lighting, composition. Say 'no text' unless you want lettering (the model spells poorly)."},
                "path": {"type": "string", "description": "Where to save it, ending in .png; use apps/assets/<app>-<name>.png"},
                "width": {"type": "integer", "description": "Pixels, 64-1024 (rounded to a multiple of 16). Generate at the size the app draws it, or smaller for icons and sprites."},
                "height": {"type": "integer", "description": "Pixels, 64-1024"}
            }), &["prompt", "path", "width", "height"]),
        {"type": "web_search_20260209", "name": "web_search", "max_uses": 5},
    ])
}

fn system_prompt(provider: Provider, extra: &str) -> String {
    let who = match provider {
        Provider::DeepSeek => "Nova Assistant, an AI agent built into Nova (powered by a DeepSeek model)",
        Provider::Anthropic => "Claude Code running inside Nova",
    };
    let mut s = String::from(include_str!("claude_prompt.md")).replacen("{assistant}", who, 1);
    let (y, m, d, ..) = crate::rtc::now();
    s.push_str(&format!("\n\nToday's date: {y:04}-{m:02}-{d:02}. Install mode: {:?}.\n", crate::install::mode()));
    if !extra.is_empty() {
        s.push_str("\n");
        s.push_str(extra);
    }
    s
}

/// Commands the agent may run without asking (they only read).
const SAFE_COMMANDS: &[&str] = &["help", "echo", "info", "mem", "uptime", "history", "ps", "lspci", "ifconfig", "host", "fetch", "disks", "ls", "cat"];

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n[... truncated]");
    }
    s
}

struct ToolOutcome {
    /// The tool_result content (string or content blocks).
    content: Value,
    is_error: bool,
    /// One-line summary shown under the tool call.
    summary: String,
}

fn ok(content: impl Into<String>, summary: impl Into<String>) -> ToolOutcome {
    ToolOutcome { content: Value::String(content.into()), is_error: false, summary: summary.into() }
}

fn err(message: impl Into<String>) -> ToolOutcome {
    let m: String = message.into();
    ToolOutcome { content: Value::String(m.clone()), is_error: true, summary: format!("{RED}{m}{RESET}") }
}

fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input.get(key).and_then(|v| v.as_str()).ok_or_else(|| format!("missing string parameter '{key}'"))
}

/// Short description of a call for the transcript, e.g. `Write(apps/clock.rhai)`.
fn describe_call(name: &str, input: &Value) -> String {
    let arg = |k: &str| input.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let short = |s: String| if s.chars().count() > 60 { format!("{}...", s.chars().take(57).collect::<String>()) } else { s };
    match name {
        "read_file" => format!("Read({})", arg("path")),
        "write_file" => format!("Write({})", arg("path")),
        "edit_file" => format!("Edit({})", arg("path")),
        "list_files" => format!("List({})", arg("prefix")),
        "delete_file" => format!("Delete({})", arg("path")),
        "run_command" => format!("Shell({})", short(arg("command"))),
        "check_app" => format!("CheckApp({})", arg("path")),
        "launch_app" => format!("Launch({})", arg("path")),
        "http_get" => format!("Fetch({})", short(arg("url"))),
        "system_info" => "SystemInfo".into(),
        "pci_config" => format!("PciConfig({})", arg("address")),
        "open_url" => format!("OpenUrl({})", short(arg("url"))),
        "generate_image" => format!("GenerateImage({})", arg("path")),
        "web_search" => format!("WebSearch({})", short(arg("query"))),
        other => other.into(),
    }
}

impl Agent {
    /// Ask before risky actions. Returns true if allowed.
    fn permit(&mut self, what: &str) -> bool {
        if self.cfg.auto_approve {
            return true;
        }
        print!("  {ORANGE}Allow {what}?{RESET} [y]es / [a]lways / [n]o ");
        loop {
            match input::read_key() {
                Key::Char(b'y' | b'Y' | b'\n') => {
                    println!("yes");
                    return true;
                }
                Key::Char(b'a' | b'A') => {
                    println!("always");
                    self.cfg.auto_approve = true;
                    let _ = self.cfg.save();
                    return true;
                }
                Key::Char(b'n' | b'N' | 0x03 | 0x1B | 0x04) => {
                    println!("no");
                    return false;
                }
                _ => {}
            }
        }
    }

    fn run_tool(&mut self, name: &str, input: &Value) -> ToolOutcome {
        let r: Result<ToolOutcome, String> = (|| {
            Ok(match name {
                "generate_image" => {
                    let prompt = str_arg(input, "prompt")?;
                    let path = str_arg(input, "path")?;
                    if !path.ends_with(".png") || path.starts_with("system/") {
                        return Ok(err("path must end in .png and not be under system/ (use apps/assets/...)"));
                    }
                    let dim = |k: &str| input.get(k).and_then(|v| v.as_u64()).unwrap_or(512) as u32;
                    print!("{DIM}  ... generating image{RESET}");
                    let result = crate::imagegen::generate(prompt, dim("width"), dim("height"));
                    print!("\r\x1b[K");
                    let png = result?;
                    let (w, h) = gfx::Canvas::from_png(&png).map(|c| (c.width, c.height)).unwrap_or((0, 0));
                    fs::write(path, &png)?;
                    let text = format!("Saved a {w}x{h} PNG to {path} ({} KB). Draw it in an app with g.image(x, y, \"{path}\") or g.image(x, y, w, h, \"{path}\").", png.len() / 1024);
                    let blocks = vec![json!({"type": "text", "text": text}), json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": gfx::base64(&png)}})];
                    ToolOutcome { content: Value::Array(blocks), is_error: false, summary: format!("{w}x{h} image saved, {} KB", png.len() / 1024) }
                }
                "read_file" => {
                    let path = str_arg(input, "path")?;
                    let text = fs::read_string(path)?;
                    let lines = text.lines().count();
                    ok(truncate(text, 200_000), format!("Read {lines} lines"))
                }
                "write_file" => {
                    let path = str_arg(input, "path")?;
                    let content = str_arg(input, "content")?;
                    if path.starts_with("system/") && !self.permit(&format!("writing system file {path}")) {
                        return Ok(err("the user declined"));
                    }
                    fs::write(path, content.as_bytes())?;
                    ok(format!("wrote {} bytes to {path}", content.len()), format!("Wrote {} lines to {path}", content.lines().count()))
                }
                "edit_file" => {
                    let path = str_arg(input, "path")?;
                    let old = str_arg(input, "old_string")?;
                    let new = str_arg(input, "new_string")?;
                    let text = fs::read_string(path)?;
                    match text.matches(old).count() {
                        0 => return Ok(err(format!("old_string not found in {path}"))),
                        1 => {}
                        n => return Ok(err(format!("old_string occurs {n} times in {path}; include more context"))),
                    }
                    fs::write(path, text.replacen(old, new, 1).as_bytes())?;
                    let (removed, added) = (old.lines().count(), new.lines().count());
                    ok(format!("edited {path}"), format!("Updated {path}: {GREEN}+{added}{RESET} {RED}-{removed}{RESET} lines"))
                }
                "list_files" => {
                    let prefix = input.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
                    let files = fs::list(prefix);
                    let text: String = files.iter().map(|(n, s, _)| format!("{n}\t{s} bytes\n")).collect();
                    ok(if text.is_empty() { "(no files)".into() } else { text }, format!("{} files", files.len()))
                }
                "delete_file" => {
                    let path = str_arg(input, "path")?;
                    if !self.permit(&format!("deleting {path}")) {
                        return Ok(err("the user declined"));
                    }
                    fs::delete(path)?;
                    ok(format!("deleted {path}"), "Deleted")
                }
                "run_command" => {
                    let command = str_arg(input, "command")?.trim().to_string();
                    let first = command.split_whitespace().next().unwrap_or("");
                    if !SAFE_COMMANDS.contains(&first) && !self.permit(&format!("running `{command}`")) {
                        return Ok(err("the user declined"));
                    }
                    if ["halt", "reboot", "shutdown", "crash"].contains(&first) {
                        return Ok(err(format!("`{first}` would stop the system; ask the user to run it themselves")));
                    }
                    let output = crate::tty::capture(|| {
                        crate::shell::execute(&command, &History::new(1));
                    });
                    let output = strip_ansi(&output);
                    let n = output.lines().count();
                    ok(truncate(if output.is_empty() { "(no output)".into() } else { output }, 60_000), format!("{n} lines of output"))
                }
                "check_app" => {
                    let path = str_arg(input, "path")?;
                    let events: Vec<String> = input.get("events").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|e| e.as_str().map(String::from)).collect()).unwrap_or_default();
                    let report = apps::check(path, &events)?;
                    let mut text = if report.problems.is_empty() {
                        format!("\"{}\" loaded and drew without errors.", report.title)
                    } else {
                        format!("Problems:\n- {}", report.problems.join("\n- "))
                    };
                    let mut blocks = vec![];
                    if let Some(png) = &report.png {
                        text.push_str("\nScreenshot of the window contents attached.");
                        blocks.push(json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": gfx::base64(png)}}));
                    }
                    if let Some(reference) = input.get("reference").and_then(|v| v.as_str())
                        && let Ok(png) = fs::read(reference)
                    {
                        text.push_str(&format!("\nThe design mockup ({reference}) is attached after the screenshot: compare layout, colors, spacing and controls, and fix what differs."));
                        blocks.push(json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": gfx::base64(&png)}}));
                    }
                    blocks.insert(0, json!({"type": "text", "text": text}));
                    let summary = if report.problems.is_empty() { format!("{GREEN}OK{RESET}, screenshot attached") } else { format!("{RED}{} problem(s){RESET}: {}", report.problems.len(), report.problems[0].lines().next().unwrap_or("")) };
                    ToolOutcome { content: Value::Array(blocks), is_error: false, summary }
                }
                "launch_app" => {
                    let path = str_arg(input, "path")?;
                    if !crate::wm::running() {
                        return Ok(err("the desktop is not running"));
                    }
                    apps::launch(path)?;
                    ok(format!("opened {path} on the desktop"), "Opened on the desktop")
                }
                "http_get" => {
                    let url = str_arg(input, "url")?;
                    let r = http::get(url)?;
                    let body = String::from_utf8_lossy(&r.body).into_owned();
                    ok(truncate(format!("HTTP {}\n\n{body}", r.status), 60_000), format!("HTTP {}, {} bytes", r.status, r.body.len()))
                }
                "system_info" => {
                    let output = crate::tty::capture(|| {
                        let h = History::new(1);
                        for c in ["info", "mem", "lspci", "disks", "ifconfig"] {
                            crate::println!("$ {c}");
                            crate::shell::execute(c, &h);
                        }
                        crate::println!("cpu: {}", crate::cpu::brand());
                    });
                    ok(strip_ansi(&output), "Collected")
                }
                "pci_config" => {
                    let a = str_arg(input, "address")?;
                    let dev = crate::drivers::records().into_iter().find(|r| format!("{}", r.device.address) == a).ok_or_else(|| format!("no PCI device at {a}"))?.device;
                    let mut text = format!("{} {:04x}:{:04x} class {:02x}.{:02x}.{:02x} rev {:02x} irq line {}\n", dev.address, dev.vendor, dev.device, dev.class, dev.subclass, dev.prog_if, dev.revision, dev.irq_line);
                    for off in (0..256u16).step_by(16) {
                        text.push_str(&format!("{off:02x}:"));
                        for i in 0..16 {
                            text.push_str(&format!(" {:02x}", dev.address.read8((off + i) as u8)));
                        }
                        text.push('\n');
                    }
                    for b in 0..6 {
                        if let Some(bar) = dev.bar(b) {
                            text.push_str(&format!("BAR{b}: {bar:?}\n"));
                        }
                    }
                    ok(text, "Dumped configuration space")
                }
                "open_url" => {
                    let url = str_arg(input, "url")?;
                    if !crate::wm::running() {
                        return Ok(err("the desktop is not running"));
                    }
                    if !crate::browser::engine_installed() {
                        return Ok(err("the Chromium engine is not installed on this system"));
                    }
                    crate::browser::launch(Some(url.into()));
                    ok(format!("opened {url} in Nova Browser"), "Opened in Nova Browser")
                }
                other => err(format!("unknown tool {other}")),
            })
        })();
        r.unwrap_or_else(err)
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            while let Some(&n) = chars.peek() {
                chars.next();
                if ('@'..='~').contains(&n) {
                    break;
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The agent loop

pub struct Agent {
    pub cfg: Config,
    messages: Vec<Value>,
    system: String,
    usage: [u64; 4], // input, output, cache read, cache write
}

enum ApiError {
    /// Worth retrying (overloaded, rate limited, network).
    Transient(String),
    Fatal(String),
}

struct Reply {
    content: Vec<Value>,
    stop_reason: String,
    stop_details: Value,
}

struct Block {
    value: Value,
    text: String,
    signature: String,
    json: String,
    writer: Option<Writer>,
}

impl Agent {
    pub fn new(cfg: Config, extra_system: &str) -> Self {
        Self { system: system_prompt(cfg.provider, extra_system), cfg, messages: Vec::new(), usage: [0; 4] }
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = self.cfg.auth_headers();
        h.push(("anthropic-version", API_VERSION.into()));
        h.push(("content-type", "application/json".into()));
        if let Some(b) = self.cfg.betas(supports_fallbacks(&self.cfg.model)) {
            h.push(("anthropic-beta", b));
        }
        h
    }

    fn body(&self) -> Value {
        let mut body = json!({
            "model": self.cfg.model,
            "max_tokens": MAX_TOKENS,
            "stream": true,
            "system": self.system,
            "messages": self.messages,
            "tools": tools(),
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": self.cfg.effort},
            "cache_control": {"type": "ephemeral"},
        });
        if supports_fallbacks(&self.cfg.model) {
            body["fallbacks"] = json!("default");
        }
        if self.cfg.provider == Provider::DeepSeek {
            body["messages"] = Value::Array(self.messages.iter().map(images_beside_tool_results).collect());
        }
        body
    }

    /// One streamed API request; prints the response as it arrives.
    fn request(&mut self) -> Result<Reply, ApiError> {
        let url = format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'));
        let body = serde_json::to_vec(&self.body()).map_err(|e| ApiError::Fatal(e.to_string()))?;
        let headers = self.headers();
        let hdrs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        print!("{DIM}  ... thinking{RESET}");
        let mut resp = http::request("POST", &url, &hdrs, &body).map_err(ApiError::Transient)?;
        print!("\r\x1b[K");
        if resp.status != 200 {
            let status = resp.status;
            let full = resp.read_all().map_err(ApiError::Transient)?;
            let text = String::from_utf8_lossy(&full.body).into_owned();
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.pointer("/error/message").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or(text);
            return Err(match status {
                401 if self.cfg.host.as_ref().is_some_and(|h| h.oauth) => ApiError::Fatal(format!(
                    "authentication failed ({msg}). The host login token has probably expired: restart Nova with scripts/run.sh to refresh it, or use /login."
                )),
                401 => ApiError::Fatal(format!("authentication failed ({msg}). Use /login to enter a valid API key.")),
                429 | 500 | 502 | 503 | 529 => ApiError::Transient(format!("HTTP {status}: {msg}")),
                _ => ApiError::Fatal(format!("HTTP {status}: {msg}")),
            });
        }

        let mut blocks: Vec<Block> = Vec::new();
        let mut stop_reason = String::new();
        let mut stop_details = Value::Null;
        let mut event = String::new();
        let mut data = String::new();
        loop {
            let Some(line) = resp.next_line().map_err(ApiError::Transient)? else { break };
            if let Some(e) = line.strip_prefix("event:") {
                event = e.trim().into();
                continue;
            }
            if let Some(d) = line.strip_prefix("data:") {
                data.push_str(d.trim_start());
                continue;
            }
            if !line.is_empty() || data.is_empty() {
                continue;
            }
            let v: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
            data.clear();
            let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or(event.as_str()).to_string();
            match kind.as_str() {
                "message_start" => {
                    if let Some(u) = v.pointer("/message/usage") {
                        self.add_usage(u);
                    }
                }
                "content_block_start" => {
                    let block = v.get("content_block").cloned().unwrap_or(Value::Null);
                    let btype = block.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    let writer = match btype {
                        "text" => {
                            print!("\n{BOLD}\u{23fa}{RESET} ");
                            Some(Writer::new(2, ""))
                        }
                        "thinking" => {
                            print!("\n{DIM}\u{273b} ");
                            Some(Writer::new(2, DIM))
                        }
                        "fallback" => {
                            println!("\n{DIM}(the request was continued by a fallback model){RESET}");
                            None
                        }
                        _ => None,
                    };
                    blocks.push(Block { value: block, text: String::new(), signature: String::new(), json: String::new(), writer });
                }
                "content_block_delta" => {
                    let Some(b) = blocks.last_mut() else { continue };
                    let delta = v.get("delta").cloned().unwrap_or(Value::Null);
                    match delta.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                        "text_delta" => {
                            let t = delta.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            b.text.push_str(t);
                            if let Some(w) = &mut b.writer {
                                w.push(t);
                            }
                        }
                        "thinking_delta" => {
                            let t = delta.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
                            b.text.push_str(t);
                            if let Some(w) = &mut b.writer {
                                w.push(t);
                            }
                        }
                        "signature_delta" => b.signature.push_str(delta.get("signature").and_then(|t| t.as_str()).unwrap_or("")),
                        "input_json_delta" => b.json.push_str(delta.get("partial_json").and_then(|t| t.as_str()).unwrap_or("")),
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let Some(b) = blocks.last_mut() else { continue };
                    if let Some(w) = &mut b.writer {
                        w.finish();
                        println!();
                    }
                    let btype = b.value.get("type").and_then(|t| t.as_str()).unwrap_or("").to_string();
                    match btype.as_str() {
                        "text" => b.value["text"] = Value::String(core::mem::take(&mut b.text)),
                        "thinking" => {
                            b.value["thinking"] = Value::String(core::mem::take(&mut b.text));
                            b.value["signature"] = Value::String(core::mem::take(&mut b.signature));
                        }
                        "tool_use" | "server_tool_use" => {
                            if !b.json.is_empty() {
                                b.value["input"] = serde_json::from_str(&b.json).unwrap_or(Value::Null);
                            }
                            if btype == "server_tool_use" {
                                let name = b.value.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                println!("\n{BOLD}\u{23fa}{RESET} {}", describe_call(name, &b.value["input"]));
                            }
                        }
                        "web_search_tool_result" => {
                            let n = b.value.get("content").and_then(|c| c.as_array()).map_or(0, |a| a.len());
                            println!("  \u{23bf}  {n} results");
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    if let Some(r) = v.pointer("/delta/stop_reason").and_then(|r| r.as_str()) {
                        stop_reason = r.into();
                    }
                    if let Some(d) = v.pointer("/delta/stop_details") {
                        stop_details = d.clone();
                    }
                    if let Some(u) = v.get("usage") {
                        self.add_usage(u);
                    }
                }
                "error" => {
                    let msg = v.pointer("/error/message").and_then(|m| m.as_str()).unwrap_or("stream error").to_string();
                    let etype = v.pointer("/error/type").and_then(|m| m.as_str()).unwrap_or("");
                    return Err(if etype == "overloaded_error" || etype == "api_error" { ApiError::Transient(msg) } else { ApiError::Fatal(msg) });
                }
                _ => {} // ping, message_stop
            }
        }
        if stop_reason.is_empty() {
            return Err(ApiError::Transient("the response stream ended early".into()));
        }
        Ok(Reply { content: blocks.into_iter().map(|b| b.value).collect(), stop_reason, stop_details })
    }

    fn add_usage(&mut self, u: &Value) {
        let n = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        self.usage[0] += n("input_tokens");
        self.usage[1] += n("output_tokens");
        self.usage[2] += n("cache_read_input_tokens");
        self.usage[3] += n("cache_creation_input_tokens");
    }

    /// Request with retries for transient failures.
    fn request_with_retry(&mut self) -> Result<Reply, String> {
        let mut delay = 2;
        for attempt in 0..5 {
            match self.request() {
                Ok(r) => return Ok(r),
                Err(ApiError::Fatal(e)) => return Err(e),
                Err(ApiError::Transient(e)) if attempt < 4 => {
                    println!("\r\x1b[K{DIM}  {e} - retrying in {delay} s{RESET}");
                    task::sleep_ticks(delay * pit::HZ);
                    delay *= 2;
                }
                Err(ApiError::Transient(e)) => return Err(e),
            }
        }
        Err("gave up".into())
    }

    /// Echo-safe assistant content: after a mid-output fallback, drop the declined
    /// model's thinking and tool_use blocks before the last `fallback` block.
    fn echo_content(content: &[Value]) -> Vec<Value> {
        let last_fallback = content.iter().rposition(|b| b.get("type").and_then(|t| t.as_str()) == Some("fallback"));
        content
            .iter()
            .enumerate()
            .filter(|(i, b)| {
                let t = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match last_fallback {
                    Some(f) if *i < f => !matches!(t, "thinking" | "redacted_thinking" | "tool_use"),
                    _ => true,
                }
            })
            .map(|(_, b)| b.clone())
            .collect()
    }

    /// Send a user message and run the tool loop until Claude is done.
    pub fn turn(&mut self, user: Value) {
        self.messages.push(json!({"role": "user", "content": user}));
        let start_len = self.messages.len() - 1;
        loop {
            let reply = match self.request_with_retry() {
                Ok(r) => r,
                Err(e) => {
                    println!("\n{RED}\u{23fa} API error: {e}{RESET}");
                    // Forget this exchange so the conversation stays valid.
                    self.messages.truncate(start_len);
                    return;
                }
            };
            match reply.stop_reason.as_str() {
                "refusal" => {
                    let why = reply.stop_details.get("explanation").and_then(|e| e.as_str()).unwrap_or("");
                    println!("\n{ORANGE}\u{23fa} The model declined this request.{RESET} {why}");
                    self.messages.truncate(start_len);
                    return;
                }
                "pause_turn" => {
                    self.messages.push(json!({"role": "assistant", "content": Self::echo_content(&reply.content)}));
                    continue;
                }
                _ => {}
            }
            let content = Self::echo_content(&reply.content);
            let calls: Vec<Value> = content.iter().filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use")).cloned().collect();
            self.messages.push(json!({"role": "assistant", "content": content}));
            if reply.stop_reason == "max_tokens" {
                println!("\n{DIM}(response reached the output limit){RESET}");
            }
            if calls.is_empty() {
                return;
            }
            // All results go back in one user message.
            let mut results = Vec::new();
            for call in calls {
                let id = call.get("id").and_then(|i| i.as_str()).unwrap_or("").to_string();
                let name = call.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let input = call.get("input").cloned().unwrap_or(Value::Null);
                println!("\n{BOLD}{GREEN}\u{23fa}{RESET} {BOLD}{}{RESET}", describe_call(&name, &input));
                let outcome = if !input.is_object() {
                    err("INVALID_JSON: the tool input was not a valid JSON object (it may have been cut off); send the call again")
                } else {
                    self.run_tool(&name, &input)
                };
                println!("  \u{23bf}  {}", outcome.summary);
                let mut r = Map::new();
                r.insert("type".into(), json!("tool_result"));
                r.insert("tool_use_id".into(), json!(id));
                r.insert("content".into(), outcome.content);
                if outcome.is_error {
                    r.insert("is_error".into(), json!(true));
                }
                results.push(Value::Object(r));
            }
            self.messages.push(json!({"role": "user", "content": results}));
        }
    }

    fn cost_line(&self) -> String {
        let [i, o, r, w] = self.usage;
        format!("tokens: {i} input, {o} output, {r} read from cache, {w} written to cache")
    }
}

/// DeepSeek drops images nested in tool_result content; move them to image blocks right
/// after the user message's tool results (where it does read them).
fn images_beside_tool_results(msg: &Value) -> Value {
    let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else { return msg.clone() };
    let mut out = Vec::new();
    let mut images = Vec::new();
    for b in blocks {
        let mut b = b.clone();
        if b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
            && let Some(inner) = b.get_mut("content").and_then(|c| c.as_array_mut())
        {
            let (imgs, rest): (Vec<Value>, Vec<Value>) = inner.drain(..).partition(|x| x.get("type").and_then(|t| t.as_str()) == Some("image"));
            *inner = rest;
            images.extend(imgs);
        }
        out.push(b);
    }
    let tool_results_end = out.iter().rposition(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result")).map_or(0, |i| i + 1);
    out.splice(tool_results_end..tool_results_end, images);
    let mut m = msg.clone();
    m["content"] = Value::Array(out);
    m
}

fn supports_fallbacks(model: &str) -> bool {
    // Server-side fallbacks: Opus 5.x, Fable 5.1 and Sonnet 5.5 (not Haiku).
    model.starts_with("claude-opus-5") || model.starts_with("claude-fable-5") || model == "claude-sonnet-5-5"
}

// ---------------------------------------------------------------------------
// Login

/// Check the credential by listing models. Ok(()) if the API accepts it.
pub fn verify(cfg: &Config) -> Result<(), String> {
    let base = cfg.base_url.trim_end_matches('/');
    if cfg.provider == Provider::DeepSeek {
        // DeepSeek lists models at its OpenAI-style root, with a Bearer key.
        let key = cfg.host.as_ref().map_or(cfg.api_key.as_str(), |h| h.secret.as_str());
        let url = format!("{}/models", base.trim_end_matches("/anthropic"));
        let auth = format!("Bearer {key}");
        let r = http::request("GET", &url, &[("authorization", &auth)], &[])?.read_all()?;
        return match r.status {
            200 => Ok(()),
            401 => Err("DeepSeek rejected this key (401)".into()),
            s => Err(format!("unexpected response HTTP {s}: {}", String::from_utf8_lossy(&r.body).chars().take(200).collect::<String>())),
        };
    }
    let url = format!("{base}/v1/models?limit=1");
    let mut h = cfg.auth_headers();
    h.push(("anthropic-version", API_VERSION.into()));
    if let Some(b) = cfg.betas(false) {
        h.push(("anthropic-beta", b));
    }
    let h: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let r = http::request("GET", &url, &h, &[])?.read_all()?;
    match r.status {
        200 => Ok(()),
        401 if cfg.host.as_ref().is_some_and(|h| h.oauth) => Err("the API rejected the login token (401); run `ant auth login` on the host".into()),
        401 => Err("the API rejected this key (401)".into()),
        s => Err(format!("unexpected response HTTP {s}: {}", String::from_utf8_lossy(&r.body).chars().take(200).collect::<String>())),
    }
}

/// Sign in at startup: use the host's login when it passed one in (checking it first),
/// otherwise the saved API key, otherwise ask for one. Returns true when signed in.
pub fn sign_in(cfg: &mut Config) -> bool {
    if let Some(h) = cfg.host.clone() {
        print!("  {DIM}signing in with {}...{RESET}", cfg.describe_login());
        let result = if net::wait_configured(10 * pit::HZ) { verify(cfg) } else { Err("no network connection".into()) };
        match result {
            Ok(()) => {
                println!("\r\x1b[K  {GREEN}\u{2713} Signed in with {}.{RESET}", cfg.describe_login());
                let what = if h.oauth { "token stays in memory; the host refreshes it at every boot" } else { "key stays in memory; nothing is saved on this disk" };
                println!("  {DIM}The {what}.{RESET}");
                return true;
            }
            Err(e) => {
                println!("\r\x1b[K  {RED}\u{2717} Host login from {} not usable: {e}{RESET}", h.source);
                cfg.host = None;
            }
        }
    }
    cfg.logged_in() || login(cfg)
}

/// Interactive login. Returns true when a working key was saved.
pub fn login(cfg: &mut Config) -> bool {
    println!();
    let p = cfg.provider;
    println!("  To use {}, sign in with a {} API key.", p.app_name(), p.name().replace("Claude", "Anthropic"));
    println!("  Create one at {CYAN}{}{RESET}", p.keys_url());
    println!("  (In the QEMU window, type it; over the serial console you can paste it.)");
    match p {
        Provider::DeepSeek => println!("  {DIM}Or put it in DEEPSEEK_API_KEY on the host; scripts/run.sh passes it in.{RESET}"),
        Provider::Anthropic => println!("  {DIM}No key? Sign in on the host with `ant auth login` and start Nova with scripts/run.sh.{RESET}"),
    }
    println!();
    loop {
        let opts = lineedit::Options { prompt: "  API key: ", max_len: 300, masked: true, complete: None };
        let key = match lineedit::read_line(&opts, None) {
            ReadResult::Line(k) => k.trim().to_string(),
            _ => return false,
        };
        if key.is_empty() {
            println!("  {DIM}Skipped. Use /login later.{RESET}");
            return false;
        }
        let mut candidate = cfg.clone();
        candidate.api_key = key;
        candidate.host = None;
        print!("  {DIM}checking the key with {}...{RESET}", candidate.base_url);
        if !net::wait_configured(10 * pit::HZ) {
            println!("\r\x1b[K  {RED}No network connection.{RESET}");
            return false;
        }
        match verify(&candidate) {
            Ok(()) => {
                println!("\r\x1b[K  {GREEN}\u{2713} Signed in.{RESET} Key saved to {CONFIG_PATH}.");
                *cfg = candidate;
                if let Err(e) = cfg.save() {
                    println!("  {RED}could not save: {e}{RESET}");
                }
                return true;
            }
            Err(e) => println!("\r\x1b[K  {RED}\u{2717} {e}{RESET} Try again, or press Enter to skip."),
        }
    }
}

pub fn welcome(cfg: &Config) {
    const W: usize = 54;
    let line = |styled: &str, plain_len: usize| println!("{ORANGE}\u{2502}{RESET}{styled}{}{ORANGE}\u{2502}{RESET}", " ".repeat(W.saturating_sub(plain_len)));
    println!();
    println!("{ORANGE}\u{256d}{}\u{256e}{RESET}", "\u{2500}".repeat(W));
    let title = format!(" \u{273b} {}", cfg.provider.banner());
    line(&format!("{BOLD}{title}{RESET}"), title.chars().count());
    line("", 0);
    let info = format!("   /help for commands - model {}", cfg.model);
    line(&format!("{DIM}{info}{RESET}"), info.chars().count());
    println!("{ORANGE}\u{2570}{}\u{256f}{RESET}", "\u{2500}".repeat(W));
}

fn help(p: Provider) {
    println!("  Ask for anything: \"make me a clock app\", \"what hardware does this machine have?\"");
    println!("  {BOLD}/clear{RESET}        start a new conversation");
    println!("  {BOLD}/model{RESET} [id]   show or set the model (default {})", p.default_model());
    println!("  {BOLD}/effort{RESET} [l]   {}", p.efforts().join(", "));
    println!("  {BOLD}/provider{RESET} [p] deepseek or anthropic (switching asks for that service's key)");
    println!("  {BOLD}/login{RESET}        enter an API key   {BOLD}/logout{RESET}  sign out");
    println!("  {BOLD}/permissions{RESET}  toggle asking before commands and deletions");
    println!("  {BOLD}/cost{RESET}         token usage of this session");
    println!("  {BOLD}/exit{RESET}         close {}", p.app_name());
}

/// The interactive Claude Code session (runs in the current terminal).
pub fn run_cli() {
    run_cli_with(None);
}

/// Like [`run_cli`], starting with `prompt` as the first request (Home's idea cards).
pub fn run_cli_with(prompt: Option<String>) {
    let mut cfg = Config::load();
    welcome(&cfg);
    sign_in(&mut cfg);
    run_session_with(cfg, "", prompt);
}

/// Prompt loop for an already set-up client.
pub fn run_session(cfg: Config, extra_system: &str) {
    run_session_with(cfg, extra_system, None);
}

fn run_session_with(cfg: Config, extra_system: &str, first: Option<String>) {
    let mut agent = Agent::new(cfg, extra_system);
    let mut history = History::new(100);
    if let Some(p) = first {
        println!();
        println!("{BOLD}>{RESET} {p}");
        history.add(&p);
        if agent.cfg.logged_in() {
            agent.turn(json!(p));
        } else {
            println!("  {ORANGE}Not signed in.{RESET} Use /login first.");
        }
    }
    loop {
        println!();
        let opts = lineedit::Options { prompt: "\x1b[1m>\x1b[0m ", max_len: 4000, masked: false, complete: None };
        let line = match lineedit::read_line(&opts, Some(&history)) {
            ReadResult::Line(l) => l,
            ReadResult::Cancelled => continue,
            ReadResult::Eof => return,
        };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        history.add(text);
        if let Some(cmd) = text.strip_prefix('/') {
            let (name, arg) = cmd.split_once(' ').map_or((cmd, ""), |(a, b)| (a, b.trim()));
            match name {
                "help" => help(agent.cfg.provider),
                "exit" | "quit" => return,
                "clear" => {
                    agent.messages.clear();
                    println!("  {DIM}conversation cleared{RESET}");
                }
                "cost" => println!("  {}", agent.cost_line()),
                "model" => {
                    if !arg.is_empty() {
                        agent.cfg.model = arg.into();
                        let _ = agent.cfg.save();
                    }
                    println!("  model: {}", agent.cfg.model);
                }
                "effort" => {
                    if agent.cfg.provider.efforts().contains(&arg) {
                        agent.cfg.effort = arg.into();
                        let _ = agent.cfg.save();
                    }
                    println!("  effort: {}", agent.cfg.effort);
                }
                "login" => {
                    login(&mut agent.cfg);
                }
                "provider" => {
                    if let Some(p) = Provider::from_id(arg)
                        && p != agent.cfg.provider
                    {
                        agent.cfg.set_provider(p);
                        agent.cfg.model = p.default_model().into();
                        agent.cfg.effort = "high".into();
                        agent.cfg.api_key.clear();
                        agent.cfg.host = None;
                        let _ = agent.cfg.save();
                        agent = Agent::new(agent.cfg.clone(), extra_system);
                        login(&mut agent.cfg);
                    }
                    println!("  provider: {} (model {})", agent.cfg.provider.name(), agent.cfg.model);
                }
                "logout" => {
                    agent.cfg.api_key.clear();
                    agent.cfg.host = None;
                    let _ = agent.cfg.save();
                    println!("  signed out");
                }
                "permissions" => {
                    agent.cfg.auto_approve = !agent.cfg.auto_approve;
                    let _ = agent.cfg.save();
                    println!("  {}", if agent.cfg.auto_approve { "commands and deletions run without asking" } else { "the assistant asks before commands and deletions" });
                }
                _ => println!("  unknown command /{name} (try /help)"),
            }
            continue;
        }
        if !agent.cfg.logged_in() {
            println!("  {ORANGE}Not signed in.{RESET} Use /login first.");
            continue;
        }
        agent.turn(json!(text));
    }
}
