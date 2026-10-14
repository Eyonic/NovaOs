//! Settings: a native app in Nova's style (a clean-room design in the spirit of GNOME
//! Settings and macOS System Settings): a sidebar of sections, each a column of cards.
//!
//! Assistant (provider, model, sign-in, image server), Network, Storage & install
//! (installs Nova onto a disk when running live), Privacy & Shields (filter rules,
//! sites with Shields down, each removable) and About (version, hardware, credits).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::font::{self, Weight};
use crate::gfx::{Canvas, Rect};
use crate::input::Key;
use crate::ui;
use crate::wm::{self, Content, Event, WindowRef};
use crate::{claude, fs, install, memory, net, pit, storage, task};

const SIDEBAR_W: i32 = 230;

pub fn open() {
    let (sw, sh) = wm::work_area();
    let (w, h) = ((sw - 260).min(900), (sh - 160).min(600));
    let win = wm::open("Settings", '\u{263c}', 0x6B7280, w, h, Content::Canvas(Canvas::new(w as usize, h as usize)));
    task::spawn("settings", move || {
        let mut s = Settings { win: win.clone(), section: 0, buttons: Vec::new(), message: String::new() };
        s.run();
        wm::close(&win);
    });
}

const SECTIONS: [(&str, char, &str); 5] = [
    ("Assistant", '*', "Your AI assistant and image generation"),
    ("Network", '@', "Connection, address and traffic"),
    ("Storage & install", '\u{2261}', "Disks, NovaFS and installing Nova"),
    ("Privacy & Shields", 'o', "Ad and tracker blocking in Nova Browser"),
    ("About", '%', "This computer and Nova"),
];

#[derive(Clone)]
enum Act {
    OpenAssistant,
    Install(usize),
    AllowSite(String),
    OpenMonitor,
}

struct Settings {
    win: WindowRef,
    section: usize,
    buttons: Vec<(Rect, Act)>,
    message: String,
}

/// Lays out cards top to bottom.
struct Column {
    x: i32,
    y: i32,
    w: i32,
}

impl Column {
    fn heading(&mut self, c: &mut Canvas, text: &str) {
        font::draw(c, self.x + 4, self.y, text, 12, Weight::Bold, ui::DIM);
        self.y += 24;
    }

    /// A card of label/value rows.
    fn rows(&mut self, c: &mut Canvas, rows: &[(&str, String)]) {
        let h = rows.len() as i32 * 44;
        let r = Rect::new(self.x, self.y, self.w, h);
        c.fill_round_rect(r, 12, ui::PANEL);
        for (i, (label, value)) in rows.iter().enumerate() {
            let y = r.y + i as i32 * 44;
            if i > 0 {
                c.fill_rect(Rect::new(r.x + 16, y, r.w - 32, 1), ui::BORDER);
            }
            font::draw(c, r.x + 18, y + 12, label, 15, Weight::Regular, ui::TEXT);
            let vw = font::width(value, 14, Weight::Regular).min(r.w / 2);
            font::draw_clipped(c, r.x + r.w - 18 - vw, y + 13, value, 14, Weight::Regular, ui::DIM, r.w / 2);
        }
        self.y += h + 22;
    }

    fn text(&mut self, c: &mut Canvas, text: &str) {
        for line in font::wrap(text, 14, Weight::Regular, self.w - 8, 4) {
            font::draw(c, self.x + 4, self.y, &line, 14, Weight::Regular, ui::DIM);
            self.y += 20;
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
                Some(Event::MouseDown { x, y, .. }) => self.click(x, y),
                Some(Event::Key(Key::Up)) => self.section = self.section.saturating_sub(1),
                Some(Event::Key(Key::Down)) => self.section = (self.section + 1).min(SECTIONS.len() - 1),
                _ => {}
            }
            self.render();
        }
    }

    fn click(&mut self, x: i32, y: i32) {
        for i in 0..SECTIONS.len() {
            if Rect::new(10, 56 + i as i32 * 44, SIDEBAR_W - 20, 38).contains(x, y) {
                self.section = i;
                self.message.clear();
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
                Act::Install(disk) => {
                    self.message = format!("Installing onto disk {disk}...");
                    self.render();
                    self.message = match install::install(disk, |_, _| {}) {
                        Ok(()) => "Installed. Restart and boot from the new disk.".into(),
                        Err(e) => format!("Install failed: {e}"),
                    };
                }
                Act::AllowSite(site) => {
                    let text = fs::read_string("system/shields/allowed-sites").unwrap_or_default();
                    let kept: String = text.lines().filter(|l| l.trim() != site).map(|l| format!("{l}\n")).collect();
                    let _ = fs::write("system/shields/allowed-sites", kept.as_bytes());
                    self.message = format!("Shields are up again on {site}");
                }
            }
        }
    }

    fn button(&mut self, c: &mut Canvas, r: Rect, label: &str, primary: bool, act: Act) {
        ui::button(c, r, label, primary);
        self.buttons.push((r, act));
    }

    fn render(&mut self) {
        let (w, h) = wm::size(&self.win);
        let mut c = Canvas::new(w as usize, h as usize);
        c.clear(ui::BG);
        self.buttons.clear();

        c.fill_rect(Rect::new(0, 0, SIDEBAR_W, h), ui::SIDEBAR);
        c.fill_rect(Rect::new(SIDEBAR_W - 1, 0, 1, h), ui::BORDER);
        font::draw(&mut c, 20, 18, "SETTINGS", 12, Weight::Bold, ui::DIM);
        for (i, (label, icon, _)) in SECTIONS.iter().enumerate() {
            let r = Rect::new(10, 56 + i as i32 * 44, SIDEBAR_W - 20, 38);
            let active = i == self.section;
            if active {
                c.fill_round_rect(r, 9, ui::SELECTION);
            }
            crate::icons::draw(&mut c, *icon, ui::ACCENT, Rect::new(r.x + 8, r.y + 6, 26, 26));
            font::draw(&mut c, r.x + 44, r.y + 9, label, 15, if active { Weight::Bold } else { Weight::Regular }, ui::TEXT);
        }

        let (title, _, subtitle) = SECTIONS[self.section];
        let x = SIDEBAR_W + 32;
        font::draw(&mut c, x, 22, title, 26, Weight::Bold, ui::TEXT);
        font::draw(&mut c, x, 58, subtitle, 14, Weight::Regular, ui::DIM);
        let mut col = Column { x, y: 96, w: (w - x - 32).min(600) };

        match self.section {
            0 => {
                let cfg = claude::Config::load();
                let signed = if cfg.logged_in() { "Signed in" } else { "Not signed in" };
                let source = match &cfg.host {
                    Some(h) => format!("from the host ({})", if h.oauth { "Anthropic CLI login" } else { "API key" }),
                    None if cfg.logged_in() => "API key saved on this disk".into(),
                    None => "-".into(),
                };
                col.rows(&mut c, &[("App", claude::app_name().into()), ("Service", cfg.provider.name().into()), ("Model", cfg.model.clone()), ("Account", signed.into()), ("Sign-in", source)]);
                let images = if crate::imagegen::server().is_some() { "ComfyUI server connected" } else { "No image server (set one on the host)" };
                col.rows(&mut c, &[("Image generation", images.into()), ("Asks before commands", if cfg.auto_approve { "No".into() } else { "Yes".into() })]);
                let r = Rect::new(col.x, col.y, 200, 36);
                self.button(&mut c, r, &format!("Open {}", claude::app_name()), true, Act::OpenAssistant);
                col.y += 52;
                col.text(&mut c, "Type /provider, /model, /login or /permissions in the assistant to change these. It is also always one click away in the corner of the screen.");
            }
            1 => {
                let info = net::info();
                let mac = info.as_ref().map_or("-".into(), |i| i.mac.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":"));
                let ip = info.as_ref().and_then(|i| i.ip).map_or("-".into(), |ip| format!("{ip}"));
                let gw = info.as_ref().and_then(|i| i.gateway).map_or("-".into(), |g| format!("{g}"));
                let dns = info.as_ref().map_or("-".into(), |i| i.dns.iter().map(|d| format!("{d}")).collect::<Vec<_>>().join(", "));
                let link = info.as_ref().is_some_and(|i| i.link);
                let (rx, tx) = net::counters();
                col.rows(&mut c, &[("Connection", if link { "Wired (Intel e1000), connected".into() } else { "Not connected".into() }), ("Address", ip), ("Gateway", gw), ("DNS", dns), ("Hardware address", mac)]);
                col.rows(&mut c, &[("Received", format!("{} KB", rx / 1024)), ("Sent", format!("{} KB", tx / 1024))]);
            }
            2 => {
                let (used, total) = fs::usage().unwrap_or((0, 0));
                let mode = match install::mode() {
                    install::Mode::Live => "Live session (files in memory)",
                    install::Mode::Installed => "Installed on disk",
                };
                col.rows(&mut c, &[("Nova is running", mode.into()), ("NovaFS used", format!("{} KB of {} KB", used / 1024, total / 1024))]);
                col.heading(&mut c, "DISKS");
                let disks: Vec<(String, String)> = storage::list().iter().map(|d| (d.name.clone(), format!("{} MB{}", d.sectors / 2048, if d.is_boot { ", boot disk" } else { "" }))).collect();
                let rows: Vec<(&str, String)> = disks.iter().map(|(n, v)| (n.as_str(), v.clone())).collect();
                col.rows(&mut c, &rows);
                if install::mode() == install::Mode::Live {
                    col.heading(&mut c, "INSTALL NOVA");
                    let targets = install::targets();
                    if targets.is_empty() {
                        col.text(&mut c, "No disk large enough to install onto was found.");
                    }
                    for t in targets {
                        let r = Rect::new(col.x, col.y, 300, 36);
                        self.button(&mut c, r, &format!("Install onto {} ({} MB)", t.name, t.sectors / 2048), true, Act::Install(t.index));
                        col.y += 46;
                    }
                    col.text(&mut c, "Installing erases that disk and copies Nova and your files onto it.");
                }
            }
            3 => {
                let shields = crate::shields::Shields::load();
                col.rows(&mut c, &[("Shields", "Up on every site by default".into()), ("Filter list rules", format!("{}", shields.list_rules)), ("HTTPS upgrades", "On".into()), ("Tracking parameters", "Removed from links".into())]);
                col.heading(&mut c, "SITES WITH SHIELDS DOWN");
                let sites: Vec<String> = fs::read_string("system/shields/allowed-sites").unwrap_or_default().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
                if sites.is_empty() {
                    col.text(&mut c, "None. Turn Shields down for a site from the shield button in Nova Browser.");
                }
                for site in sites {
                    let r = Rect::new(col.x, col.y, col.w, 44);
                    c.fill_round_rect(r, 12, ui::PANEL);
                    font::draw(&mut c, r.x + 18, r.y + 12, &site, 15, Weight::Regular, ui::TEXT);
                    let b = Rect::new(r.x + r.w - 150, r.y + 6, 136, 32);
                    self.button(&mut c, b, "Shields up", false, Act::AllowSite(site.clone()));
                    col.y += 52;
                }
            }
            _ => {
                let (u, f) = memory::heap_usage();
                let up = pit::ticks() / pit::HZ;
                col.rows(&mut c, &[("Nova", env!("CARGO_PKG_VERSION").into()), ("Processor", crate::cpu::brand()), ("Memory", format!("{} MB", (u + f) >> 20)), ("Devices", format!("{} PCI devices", crate::drivers::records().len())), ("Uptime", format!("{}h {:02}m", up / 3600, up / 60 % 60))]);
                let r = Rect::new(col.x, col.y, 220, 36);
                self.button(&mut c, r, "Open System Monitor", false, Act::OpenMonitor);
                col.y += 52;
                col.text(&mut c, "Fonts: Noto Sans and Adwaita Mono (SIL Open Font License), Spleen (BSD). Artwork and icons generated for Nova. Browser engine: Chromium. Filter lists: EasyList and EasyPrivacy (GPLv3 / CC BY-SA).");
            }
        }
        if !self.message.is_empty() {
            let r = Rect::new(x, h - 52, (w - x - 32).min(600), 36);
            c.fill_round_rect(r, 10, ui::SELECTION);
            font::draw_clipped(&mut c, r.x + 16, r.y + 9, &self.message, 14, Weight::Bold, ui::TEXT, r.w - 32);
        }
        wm::present(&self.win, &c);
    }
}
