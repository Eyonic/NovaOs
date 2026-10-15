//! Settings: a native app in Nova's style (a clean-room design in the spirit of GNOME
//! Settings and macOS System Settings): a sidebar of sections, each a column of boxed
//! groups: a label, then a card of rows with the value or control at the right.
//!
//! Assistant (provider, model, sign-in, image server, a switch for asking before
//! commands), Network, Storage & install (installs Nova onto a disk when running live),
//! Privacy & Shields (filter rules, sites with Shields down, each removable) and About
//! (version, hardware, credits).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::font::{self, Weight};
use crate::gfx::{Canvas, Rect};
use crate::input::Key;
use crate::ui::{self, Kind};
use crate::wm::{self, Content, Event, WindowRef};
use crate::{claude, fs, install, memory, net, pit, storage, task};

const SIDEBAR_W: i32 = 240;
/// Boxed groups do not grow past this (long lines read badly).
const CONTENT_MAX_W: i32 = 600;

pub fn open() {
    let (sw, sh) = wm::work_area();
    let (w, h) = ((sw - 260).min(900), (sh - 160).min(600));
    let win = wm::open("Settings", '\u{263c}', 0x6B7280, w, h, Content::Canvas(Canvas::new(w as usize, h as usize)));
    task::spawn("settings", move || {
        let mut s = Settings { win: win.clone(), section: 0, buttons: Vec::new(), message: String::new(), message_until: 0, cursor: None, held: false };
        s.run();
        wm::close(&win);
    });
}

/// (title, glyph, tile colour, subtitle)
const SECTIONS: [(&str, &str, u32, &str); 5] = [
    ("Assistant", "sparkle", 0xE07A5F, "Your AI assistant and image generation"),
    ("Network", "network", 0x3B9CF6, "Connection, address and traffic"),
    ("Storage & install", "drive", 0x8E8EA0, "Disks, NovaFS and installing Nova"),
    ("Privacy & Shields", "shield", 0xF08A24, "Ad and tracker blocking in Nova Browser"),
    ("About", "info", 0x8B7CF6, "This computer and Nova"),
];

#[derive(Clone)]
enum Act {
    OpenAssistant,
    ToggleAsk,
    Install(usize),
    AllowSite(String),
    OpenMonitor,
}

struct Settings {
    win: WindowRef,
    section: usize,
    buttons: Vec<(Rect, Act)>,
    message: String,
    message_until: u64,
    /// Pointer position inside the window (None when it is elsewhere).
    cursor: Option<(i32, i32)>,
    held: bool,
}

/// Lays out groups top to bottom.
struct Column {
    x: i32,
    y: i32,
    w: i32,
}

impl Column {
    fn label(&mut self, c: &mut Canvas, text: &str) {
        self.y = ui::section_label(c, self.x + 4, self.y, text);
    }

    /// A card of label/value rows.
    fn rows(&mut self, c: &mut Canvas, rows: &[(&str, String)]) {
        let r = self.card(c, rows.len());
        for (i, (label, value)) in rows.iter().enumerate() {
            let y = r.y + i as i32 * ui::ROW_H;
            if i > 0 {
                ui::divider(c, Rect::new(r.x + ui::PAD, y, r.w - 2 * ui::PAD, 1));
            }
            let ty = y + (ui::ROW_H - font::line_height(ui::BODY)) / 2;
            font::draw(c, r.x + ui::PAD, ty, label, ui::BODY, Weight::Regular, ui::TEXT);
            let vw = font::width(value, ui::BODY, Weight::Regular).min(r.w / 2);
            font::draw_clipped(c, r.x + r.w - ui::PAD - vw, ty, value, ui::BODY, Weight::Regular, ui::DIM, r.w / 2);
        }
    }

    /// An empty card for `n` rows; returns it and moves on.
    fn card(&mut self, c: &mut Canvas, n: usize) -> Rect {
        let r = Rect::new(self.x, self.y, self.w, n.max(1) as i32 * ui::ROW_H);
        ui::card(c, r);
        self.y += r.h + 20;
        r
    }

    fn text(&mut self, c: &mut Canvas, text: &str) {
        for line in font::wrap(text, ui::CAPTION + 1, Weight::Regular, self.w - 8, 4) {
            font::draw(c, self.x + 4, self.y, &line, ui::CAPTION + 1, Weight::Regular, ui::DIM);
            self.y += 18;
        }
        self.y += 10;
    }
}

impl Settings {
    fn run(&mut self) {
        self.render();
        loop {
            match wm::wait_event(&self.win, Some(pit::HZ)) {
                Some(Event::Close) => return,
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
                Some(Event::Key(Key::Up)) => self.section = self.section.saturating_sub(1),
                Some(Event::Key(Key::Down)) => self.section = (self.section + 1).min(SECTIONS.len() - 1),
                _ => {}
            }
            self.render();
        }
    }

    fn say(&mut self, text: String) {
        self.message = text;
        self.message_until = pit::ticks() + 4 * pit::HZ;
    }

    fn nav_rect(i: usize) -> Rect {
        Rect::new(ui::GAP, 52 + i as i32 * (ui::NAV_ROW_H + 2), SIDEBAR_W - 2 * ui::GAP, ui::NAV_ROW_H)
    }

    fn click(&mut self, x: i32, y: i32) {
        for i in 0..SECTIONS.len() {
            if Self::nav_rect(i).contains(x, y) {
                self.section = i;
                return;
            }
        }
        for (r, act) in self.buttons.clone() {
            if !r.contains(x, y) {
                continue;
            }
            match act {
                Act::OpenAssistant => wm::toggle_assistant(),
                Act::OpenMonitor => crate::monitor::open(),
                Act::ToggleAsk => {
                    let mut cfg = claude::Config::load();
                    cfg.auto_approve = !cfg.auto_approve;
                    match cfg.save() {
                        Ok(()) => self.say(if cfg.auto_approve { "The assistant runs commands without asking from its next session".into() } else { "The assistant asks before commands from its next session".into() }),
                        Err(e) => self.say(format!("Could not save: {e}")),
                    }
                }
                Act::Install(disk) => {
                    self.say(format!("Installing onto disk {disk}..."));
                    self.render();
                    match install::install(disk, |_, _| {}) {
                        Ok(()) => self.say("Installed. Restart and boot from the new disk.".into()),
                        Err(e) => self.say(format!("Install failed: {e}")),
                    }
                }
                Act::AllowSite(site) => {
                    let text = fs::read_string("system/shields/allowed-sites").unwrap_or_default();
                    let kept: String = text.lines().filter(|l| l.trim() != site).map(|l| format!("{l}\n")).collect();
                    let _ = fs::write("system/shields/allowed-sites", kept.as_bytes());
                    self.say(format!("Shields are up again on {site}"));
                }
            }
        }
    }

    fn button(&mut self, c: &mut Canvas, r: Rect, label: &str, kind: Kind, act: Act) {
        ui::button_kind(c, r, label, kind, ui::state(self.cursor, self.held, r));
        self.buttons.push((r, act));
    }

    /// A row whose control is a switch.
    fn switch_row(&mut self, c: &mut Canvas, card: Rect, i: usize, label: &str, hint: &str, on: bool, act: Act) {
        let y = card.y + i as i32 * ui::ROW_H;
        if i > 0 {
            ui::divider(c, Rect::new(card.x + ui::PAD, y, card.w - 2 * ui::PAD, 1));
        }
        font::draw(c, card.x + ui::PAD, y + 7, label, ui::BODY, Weight::Regular, ui::TEXT);
        font::draw_clipped(c, card.x + ui::PAD, y + 25, hint, ui::CAPTION, Weight::Regular, ui::DIM, card.w - 2 * ui::PAD - 60);
        let sw = Rect::new(card.x + card.w - ui::PAD - 40, y + (ui::ROW_H - 22) / 2, 40, 22);
        let row = Rect::new(card.x, y, card.w, ui::ROW_H);
        ui::switch(c, sw, on, ui::state(self.cursor, self.held, row));
        self.buttons.push((row, act));
    }

    fn render(&mut self) {
        let (w, h) = wm::size(&self.win);
        let mut c = Canvas::new(w as usize, h as usize);
        c.clear(ui::BG);
        self.buttons.clear();
        if !self.message.is_empty() && pit::ticks() > self.message_until {
            self.message.clear();
        }

        // Sidebar: sections with tinted symbolic tiles.
        c.fill_rect(Rect::new(0, 0, SIDEBAR_W, h), ui::SIDEBAR);
        ui::divider(&mut c, Rect::new(SIDEBAR_W - 1, 0, 1, h));
        ui::section_label(&mut c, ui::PAD + 4, 22, "Settings");
        for (i, (label, glyph, tint, _)) in SECTIONS.iter().enumerate() {
            let r = Self::nav_rect(i);
            ui::nav_row(&mut c, r, Some((glyph, Some(*tint))), label, i == self.section, ui::state(self.cursor, self.held, r));
        }

        let (title, _, _, subtitle) = SECTIONS[self.section];
        let x = SIDEBAR_W + 32;
        font::draw(&mut c, x, 24, title, ui::TITLE, Weight::Bold, ui::TEXT);
        font::draw(&mut c, x, 24 + font::line_height(ui::TITLE) + 2, subtitle, ui::BODY, Weight::Regular, ui::DIM);
        let mut col = Column { x, y: 96, w: (w - x - 32).min(CONTENT_MAX_W) };

        match self.section {
            0 => {
                let cfg = claude::Config::load();
                let signed = if cfg.logged_in() { "Signed in" } else { "Not signed in" };
                let source = match &cfg.host {
                    Some(h) => format!("from the host ({})", if h.oauth { "Anthropic CLI login" } else { "API key" }),
                    None if cfg.logged_in() => "API key saved on this disk".into(),
                    None => "-".into(),
                };
                col.label(&mut c, "Account");
                col.rows(&mut c, &[("App", claude::app_name().into()), ("Service", cfg.provider.name().into()), ("Model", cfg.model.clone()), ("Account", signed.into()), ("Sign-in", source)]);
                col.label(&mut c, "Behaviour");
                let images = if crate::imagegen::server().is_some() { "ComfyUI server connected" } else { "No image server (set one on the host)" };
                let card = col.card(&mut c, 2);
                let y = card.y;
                let ty = y + (ui::ROW_H - font::line_height(ui::BODY)) / 2;
                font::draw(&mut c, card.x + ui::PAD, ty, "Image generation", ui::BODY, Weight::Regular, ui::TEXT);
                let vw = font::width(images, ui::BODY, Weight::Regular);
                font::draw(&mut c, card.x + card.w - ui::PAD - vw, ty, images, ui::BODY, Weight::Regular, ui::DIM);
                self.switch_row(&mut c, card, 1, "Ask before running commands", "Commands and deletions wait for your approval", !cfg.auto_approve, Act::ToggleAsk);
                let r = Rect::new(col.x, col.y, 200, ui::CONTROL_H + 4);
                self.button(&mut c, r, &format!("Open {}", claude::app_name()), Kind::Primary, Act::OpenAssistant);
                col.y += ui::CONTROL_H + 4 + 16;
                col.text(&mut c, "Type /provider, /model or /login in the assistant to change the account. It is also always one click away in the corner of the screen.");
            }
            1 => {
                let info = net::info();
                let mac = info.as_ref().map_or("-".into(), |i| i.mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"));
                let ip = info.as_ref().and_then(|i| i.ip).map_or("-".into(), |ip| format!("{ip}"));
                let gw = info.as_ref().and_then(|i| i.gateway).map_or("-".into(), |g| format!("{g}"));
                let dns = info.as_ref().map_or("-".into(), |i| i.dns.iter().map(|d| format!("{d}")).collect::<Vec<_>>().join(", "));
                let link = info.as_ref().is_some_and(|i| i.link);
                let (rx, tx) = net::counters();
                col.label(&mut c, "Connection");
                col.rows(&mut c, &[("Status", if link { "Wired (Intel e1000), connected".into() } else { "Not connected".into() }), ("Address", ip), ("Gateway", gw), ("DNS", dns), ("Hardware address", mac)]);
                col.label(&mut c, "Traffic");
                col.rows(&mut c, &[("Received", format!("{} KB", rx / 1024)), ("Sent", format!("{} KB", tx / 1024))]);
            }
            2 => {
                let (used, total) = fs::usage().unwrap_or((0, 0));
                let mode = match install::mode() {
                    install::Mode::Live => "Live session (files in memory)",
                    install::Mode::Installed => "Installed on disk",
                };
                col.label(&mut c, "This system");
                col.rows(&mut c, &[("Nova is running", mode.into()), ("NovaFS used", format!("{} KB of {} KB", used / 1024, total / 1024))]);
                col.label(&mut c, "Disks");
                let disks: Vec<(String, String)> = storage::list().iter().map(|d| (d.name.clone(), format!("{} MB{}", d.sectors / 2048, if d.is_boot { ", boot disk" } else { "" }))).collect();
                let rows: Vec<(&str, String)> = disks.iter().map(|(n, v)| (n.as_str(), v.clone())).collect();
                col.rows(&mut c, &rows);
                if install::mode() == install::Mode::Live {
                    col.label(&mut c, "Install Nova");
                    let targets = install::targets();
                    if targets.is_empty() {
                        col.text(&mut c, "No disk large enough to install onto was found.");
                    }
                    let card = if targets.is_empty() { Rect::new(col.x, col.y, col.w, 0) } else { col.card(&mut c, targets.len()) };
                    for (i, t) in targets.iter().enumerate() {
                        let y = card.y + i as i32 * ui::ROW_H;
                        if i > 0 {
                            ui::divider(&mut c, Rect::new(card.x + ui::PAD, y, card.w - 2 * ui::PAD, 1));
                        }
                        let ty = y + (ui::ROW_H - font::line_height(ui::BODY)) / 2;
                        font::draw(&mut c, card.x + ui::PAD, ty, &format!("{} ({} MB)", t.name, t.sectors / 2048), ui::BODY, Weight::Regular, ui::TEXT);
                        let b = Rect::new(card.x + card.w - ui::PAD - 96, y + (ui::ROW_H - ui::CONTROL_H) / 2 + 2, 96, ui::CONTROL_H - 4);
                        self.button(&mut c, b, "Install...", Kind::Primary, Act::Install(t.index));
                    }
                    col.text(&mut c, "Installing erases that disk and copies Nova and your files onto it.");
                }
            }
            3 => {
                let shields = crate::shields::Shields::load();
                col.label(&mut c, "Shields");
                col.rows(&mut c, &[("Shields", "Up on every site by default".into()), ("Filter list rules", format!("{}", shields.list_rules)), ("HTTPS upgrades", "On".into()), ("Tracking parameters", "Removed from links".into())]);
                col.label(&mut c, "Sites with Shields down");
                let sites: Vec<String> = fs::read_string("system/shields/allowed-sites").unwrap_or_default().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                if sites.is_empty() {
                    col.text(&mut c, "None. Turn Shields down for a site from the shield button in Nova Browser.");
                } else {
                    let card = col.card(&mut c, sites.len());
                    for (i, site) in sites.iter().enumerate() {
                        let y = card.y + i as i32 * ui::ROW_H;
                        if i > 0 {
                            ui::divider(&mut c, Rect::new(card.x + ui::PAD, y, card.w - 2 * ui::PAD, 1));
                        }
                        font::draw(&mut c, card.x + ui::PAD, y + (ui::ROW_H - font::line_height(ui::BODY)) / 2, site, ui::BODY, Weight::Regular, ui::TEXT);
                        let b = Rect::new(card.x + card.w - ui::PAD - 110, y + (ui::ROW_H - ui::CONTROL_H) / 2 + 2, 110, ui::CONTROL_H - 4);
                        self.button(&mut c, b, "Shields up", Kind::Secondary, Act::AllowSite(site.clone()));
                    }
                }
            }
            _ => {
                let (u, f) = memory::heap_usage();
                let up = pit::ticks() / pit::HZ;
                col.label(&mut c, "This computer");
                col.rows(&mut c, &[("Nova", env!("CARGO_PKG_VERSION").into()), ("Processor", crate::cpu::brand()), ("Memory", format!("{} MB", (u + f) >> 20)), ("Devices", format!("{} PCI devices", crate::drivers::records().len())), ("Uptime", format!("{}h {:02}m", up / 3600, up / 60 % 60))]);
                let r = Rect::new(col.x, col.y, 200, ui::CONTROL_H + 4);
                self.button(&mut c, r, "Open System Monitor", Kind::Secondary, Act::OpenMonitor);
                col.y += ui::CONTROL_H + 4 + 16;
                col.text(&mut c, "Fonts: Noto Sans and Adwaita Mono (SIL Open Font License), Spleen (BSD). Artwork and icons generated for Nova. Browser engine: Chromium. Filter lists: EasyList and EasyPrivacy (GPLv3 / CC BY-SA).");
            }
        }
        if !self.message.is_empty() {
            ui::toast(&mut c, Rect::new(SIDEBAR_W, 0, w - SIDEBAR_W, h), &self.message, self.message.starts_with("Could not") || self.message.starts_with("Install failed"));
        }
        wm::present(&self.win, &c);
    }
}
