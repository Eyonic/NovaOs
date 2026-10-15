//! System Monitor: a native app in Nova's style (a clean-room design in the spirit of
//! GNOME's Mission Center, Windows' Task Manager and macOS' Activity Monitor).
//!
//! A sidebar of live sparklines (CPU, memory, storage, network) selects a performance
//! view with a 60-second graph and stat cards; "Apps & tasks" lists open windows (with
//! End) and Nova's background tasks; "Hardware" shows the CPU, memory, disks and PCI
//! devices with their drivers. Samples once a second.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::font::{self, Weight};
use crate::gfx::{blend, Canvas, Rect};
use crate::input::Key;
use crate::ui;
use crate::wm::{self, Content, Event, WindowRef};
use crate::{drivers, fs, memory, net, pci, pit, storage, task};

const SIDEBAR_W: i32 = 240;
const HISTORY: usize = 60;

pub fn open() {
    let (sw, sh) = wm::work_area();
    let (w, h) = ((sw - 220).min(980), (sh - 140).min(620));
    let win = wm::open("System Monitor", '%', 0x2E9E6A, w, h, Content::Canvas(Canvas::new(w as usize, h as usize)));
    task::spawn("monitor", move || {
        let mut m = Monitor::new(win.clone());
        m.run();
        wm::close(&win);
    });
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Cpu,
    Memory,
    Storage,
    Network,
    Apps,
    Hardware,
}

const PAGES: [Page; 6] = [Page::Cpu, Page::Memory, Page::Storage, Page::Network, Page::Apps, Page::Hardware];

fn page_color(p: Page) -> u32 {
    match p {
        Page::Cpu => 0x7AA8FF,
        Page::Memory => 0xB89CFF,
        Page::Storage => 0xF2C46D,
        Page::Network => 0x5FD3D0,
        Page::Apps => 0x8FD694,
        Page::Hardware => 0xF0717E,
    }
}

fn fmt_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{}.{} GB", n >> 30, ((n % (1 << 30)) * 10) >> 30),
        n if n >= 1 << 20 => format!("{}.{} MB", n >> 20, ((n % (1 << 20)) * 10) >> 20),
        n if n >= 1 << 10 => format!("{} KB", n >> 10),
        n => format!("{n} B"),
    }
}

struct Monitor {
    win: WindowRef,
    page: Page,
    cpu: VecDeque<u32>,
    mem: VecDeque<u32>,
    disk: VecDeque<u32>,
    net: VecDeque<u32>,
    last_net: (u64, u64),
    net_rate: (u64, u64),
    peak_rate: u64,
    /// End buttons on the Apps page: (window id, rect).
    end_buttons: Vec<(usize, Rect)>,
    /// Pointer position inside the window (None when it is elsewhere), button held.
    cursor: Option<(i32, i32)>,
    held: bool,
}

impl Monitor {
    fn new(win: WindowRef) -> Self {
        let mut m = Self { win, page: Page::Cpu, cpu: VecDeque::new(), mem: VecDeque::new(), disk: VecDeque::new(), net: VecDeque::new(), last_net: net::counters(), net_rate: (0, 0), peak_rate: 1, end_buttons: Vec::new(), cursor: None, held: false };
        m.sample();
        m
    }

    fn push(q: &mut VecDeque<u32>, v: u32) {
        if q.len() >= HISTORY {
            q.pop_front();
        }
        q.push_back(v.min(100));
    }

    fn sample(&mut self) {
        Self::push(&mut self.cpu, task::cpu_usage() as u32);
        let (used, free) = memory::heap_usage();
        Self::push(&mut self.mem, (used as u64 * 100 / (used + free).max(1) as u64) as u32);
        let (du, dt) = fs::usage().unwrap_or((0, 1));
        Self::push(&mut self.disk, (du * 100 / dt.max(1)) as u32);
        let (rx, tx) = net::counters();
        self.net_rate = (rx.saturating_sub(self.last_net.0), tx.saturating_sub(self.last_net.1));
        self.last_net = (rx, tx);
        let rate = self.net_rate.0 + self.net_rate.1;
        self.peak_rate = self.peak_rate.max(rate).max(1);
        Self::push(&mut self.net, (rate * 100 / self.peak_rate) as u32);
    }

    fn history(&self, p: Page) -> &VecDeque<u32> {
        match p {
            Page::Memory => &self.mem,
            Page::Storage => &self.disk,
            Page::Network => &self.net,
            _ => &self.cpu,
        }
    }

    fn summary(&self, p: Page) -> (String, String) {
        match p {
            Page::Cpu => ("CPU".into(), format!("{}%", self.cpu.back().copied().unwrap_or(0))),
            Page::Memory => {
                let (u, f) = memory::heap_usage();
                ("Memory".into(), format!("{} / {}", fmt_bytes(u as u64), fmt_bytes((u + f) as u64)))
            }
            Page::Storage => {
                let (u, t) = fs::usage().unwrap_or((0, 0));
                ("Storage".into(), format!("{} / {}", fmt_bytes(u), fmt_bytes(t)))
            }
            Page::Network => ("Network".into(), format!("{}/s in, {}/s out", fmt_bytes(self.net_rate.0), fmt_bytes(self.net_rate.1))),
            Page::Apps => ("Apps & tasks".into(), format!("{} tasks", task::list().len())),
            Page::Hardware => ("Hardware".into(), format!("{} devices", drivers::records().len())),
        }
    }

    fn nav_rect(i: usize) -> Rect {
        if i < 4 { Rect::new(ui::GAP, 48 + i as i32 * 76, SIDEBAR_W - 2 * ui::GAP, 70) } else { Rect::new(ui::GAP, 48 + 4 * 76 + 36 + (i - 4) as i32 * (ui::NAV_ROW_H + 2), SIDEBAR_W - 2 * ui::GAP, ui::NAV_ROW_H) }
    }

    fn state(&self, r: Rect) -> ui::State {
        ui::state(self.cursor, self.held, r)
    }

    fn run(&mut self) {
        let mut next = pit::ticks() + pit::HZ;
        self.render();
        loop {
            match wm::wait_event(&self.win, Some(pit::HZ / 4)) {
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
                Some(Event::Key(k)) => {
                    let i = PAGES.iter().position(|p| *p == self.page).unwrap_or(0);
                    match k {
                        Key::Up => self.page = PAGES[i.saturating_sub(1)],
                        Key::Down => self.page = PAGES[(i + 1).min(PAGES.len() - 1)],
                        _ => {}
                    }
                }
                _ => {}
            }
            if pit::ticks() >= next {
                next = pit::ticks() + pit::HZ;
                self.sample();
            }
            self.render();
        }
    }

    fn click(&mut self, x: i32, y: i32) {
        for (i, p) in PAGES.iter().enumerate() {
            if Self::nav_rect(i).contains(x, y) {
                self.page = *p;
                return;
            }
        }
        for (id, r) in self.end_buttons.clone() {
            if r.contains(x, y) {
                wm::launch(&wm::Action::Close(id));
            }
        }
    }

    /// An area chart of `data` (0..=100) filling `r`.
    fn chart(c: &mut Canvas, r: Rect, data: &VecDeque<u32>, color: u32, grid: bool) {
        if grid {
            for i in 1..4 {
                c.fill_rect(Rect::new(r.x, r.y + r.h * i / 4, r.w, 1), 0x24263A);
            }
        }
        if data.is_empty() {
            return;
        }
        let n = HISTORY as i32;
        let value_at = |x: i32| -> i32 {
            // Interpolate between samples; the newest is at the right edge.
            let pos = (x * (n - 1) * 16) / r.w.max(1);
            let idx = pos / 16;
            let frac = pos % 16;
            let len = data.len() as i32;
            let get = |i: i32| -> i32 {
                let k = i - (n - len);
                if k < 0 { 0 } else { data[(k as usize).min(data.len() - 1)] as i32 }
            };
            (get(idx) * (16 - frac) + get(idx + 1) * frac) / 16
        };
        let mut prev_y = None;
        for x in 0..r.w {
            let v = value_at(x);
            let top = r.y + r.h - (v * r.h / 100).max(if v > 0 { 1 } else { 0 });
            // Fill fades towards the bottom.
            for y in top..r.y + r.h {
                let t = ((y - top) * 140 / (r.y + r.h - top).max(1)) as u32;
                let i = y as usize * c.width + (r.x + x) as usize;
                if (r.x + x) >= 0 && (r.x + x) < c.width as i32 && y >= 0 && y < c.height as i32 {
                    c.pixels[i] = blend(c.pixels[i], color, 150 - t);
                }
            }
            let py = prev_y.unwrap_or(top);
            let (a, b) = if py < top { (py, top) } else { (top, py) };
            c.fill_rect(Rect::new(r.x + x, a, 1, b - a + 2), color);
            prev_y = Some(top);
        }
    }

    fn card(c: &mut Canvas, r: Rect, label: &str, value: &str) {
        ui::card(c, r);
        ui::section_label(c, r.x + ui::PAD, r.y + 12, label);
        font::draw_clipped(c, r.x + ui::PAD, r.y + 34, value, ui::TITLE_2, Weight::Bold, ui::TEXT, r.w - 2 * ui::PAD);
    }

    fn render(&mut self) {
        let (w, h) = wm::size(&self.win);
        let mut c = Canvas::new(w as usize, h as usize);
        c.clear(ui::BG);
        self.end_buttons.clear();

        // Sidebar with sparklines.
        c.fill_rect(Rect::new(0, 0, SIDEBAR_W, h), ui::SIDEBAR);
        ui::divider(&mut c, Rect::new(SIDEBAR_W - 1, 0, 1, h));
        ui::section_label(&mut c, ui::PAD + 4, 18, "Performance");
        for (i, p) in PAGES.iter().enumerate() {
            let r = Self::nav_rect(i);
            let active = *p == self.page;
            let (label, value) = self.summary(*p);
            if i < 4 {
                let state = self.state(r);
                if active {
                    c.fill_round_rect(r, ui::RADIUS, ui::SELECTION);
                } else if state != ui::State::Normal {
                    c.fill_round_rect(r, ui::RADIUS, ui::HOVER);
                }
                let spark = Rect::new(r.x + 12, r.y + 12, 64, r.h - 24);
                c.fill_round_rect(Rect::new(spark.x - 2, spark.y - 2, spark.w + 4, spark.h + 4), 6, 0x15161F);
                Self::chart(&mut c, spark, self.history(*p), page_color(*p), false);
                font::draw(&mut c, r.x + 88, r.y + 14, &label, ui::HEADING, Weight::Bold, ui::TEXT);
                font::draw_clipped(&mut c, r.x + 88, r.y + 36, &value, 13, Weight::Regular, ui::DIM, r.w - 96);
            } else {
                let glyph = if *p == Page::Apps { "grid" } else { "cpu" };
                ui::nav_row(&mut c, r, Some((glyph, None)), &label, active, self.state(r));
            }
        }
        if 4 < PAGES.len() {
            ui::section_label(&mut c, ui::PAD + 4, Self::nav_rect(4).y - 24, "System");
        }

        // Main pane.
        let mx = SIDEBAR_W + 28;
        let mw = w - mx - 28;
        let (title, value) = self.summary(self.page);
        font::draw(&mut c, mx, 24, &title, ui::TITLE, Weight::Bold, ui::TEXT);
        let vw = font::width(&value, 16, Weight::Bold);
        font::draw(&mut c, w - 28 - vw, 30, &value, 16, Weight::Bold, page_color(self.page));
        let color = page_color(self.page);
        match self.page {
            Page::Cpu | Page::Memory | Page::Storage | Page::Network => {
                let graph = Rect::new(mx, 74, mw, h - 74 - 130);
                c.fill_round_rect(Rect::new(graph.x - 1, graph.y - 1, graph.w + 2, graph.h + 2), ui::RADIUS, 0x1A1B26);
                c.stroke_round_rect_w(Rect::new(graph.x - 1, graph.y - 1, graph.w + 2, graph.h + 2), ui::RADIUS, 1, ui::BORDER);
                Self::chart(&mut c, Rect::new(graph.x + 1, graph.y + 8, graph.w - 2, graph.h - 9), self.history(self.page), color, true);
                font::draw(&mut c, graph.x + 12, graph.y + graph.h - 22, "60 seconds", ui::CAPTION, Weight::Regular, ui::DIM);
                let cards: Vec<(String, String)> = match self.page {
                    Page::Cpu => {
                        let avg = self.cpu.iter().sum::<u32>() / self.cpu.len().max(1) as u32;
                        let peak = self.cpu.iter().max().copied().unwrap_or(0);
                        let up = pit::ticks() / pit::HZ;
                        alloc::vec![("Processor".into(), crate::cpu::brand()), ("Average / peak".into(), format!("{avg}% / {peak}%")), ("Uptime".into(), format!("{}h {:02}m {:02}s", up / 3600, up / 60 % 60, up % 60))]
                    }
                    Page::Memory => {
                        let (u, f) = memory::heap_usage();
                        alloc::vec![("In use".into(), fmt_bytes(u as u64)), ("Available".into(), fmt_bytes(f as u64)), ("Usable RAM".into(), fmt_bytes(memory::heap_size() as u64))]
                    }
                    Page::Storage => {
                        let (u, t) = fs::usage().unwrap_or((0, 0));
                        let (label, disk) = fs::mount_info().unwrap_or(("-".into(), "-".into()));
                        alloc::vec![("Used".into(), fmt_bytes(u)), ("Free".into(), fmt_bytes(t.saturating_sub(u))), (format!("NovaFS \u{201c}{label}\u{201d}"), disk)]
                    }
                    _ => {
                        let i = net::info();
                        let ip = i.as_ref().and_then(|i| i.ip).map_or("-".to_string(), |ip| format!("{ip}"));
                        let (rx, tx) = net::counters();
                        alloc::vec![("Address".into(), ip), ("Received".into(), fmt_bytes(rx)), ("Sent".into(), fmt_bytes(tx))]
                    }
                };
                let cw = (mw - 2 * 16) / 3;
                for (i, (label, value)) in cards.iter().enumerate() {
                    Self::card(&mut c, Rect::new(mx + i as i32 * (cw + 16), h - 112, cw, 84), label, value);
                }
            }
            Page::Apps => {
                let mut y = ui::section_label(&mut c, mx, 74, "Open apps");
                let windows = wm::windows();
                if windows.is_empty() {
                    font::draw(&mut c, mx, y + 4, "No apps are open. Home lists what you can start.", ui::BODY, Weight::Regular, ui::DIM);
                    y += 30;
                } else {
                    let card = Rect::new(mx, y, mw, windows.len() as i32 * ui::ROW_H);
                    ui::card(&mut c, card);
                    for (i, wnd) in windows.iter().enumerate() {
                        let r = Rect::new(card.x, card.y + i as i32 * ui::ROW_H, card.w, ui::ROW_H);
                        if i > 0 {
                            ui::divider(&mut c, Rect::new(r.x + ui::PAD, r.y, r.w - 2 * ui::PAD, 1));
                        }
                        crate::icons::draw(&mut c, wnd.icon, wnd.accent, Rect::new(r.x + ui::PAD, r.y + (ui::ROW_H - 24) / 2, 24, 24));
                        font::draw_clipped(&mut c, r.x + ui::PAD + 36, r.y + (ui::ROW_H - font::line_height(ui::BODY)) / 2, &wnd.title, ui::BODY, Weight::Regular, ui::TEXT, r.w - 160);
                        let b = Rect::new(r.x + r.w - ui::PAD - 72, r.y + (ui::ROW_H - 28) / 2, 72, 28);
                        ui::button_kind(&mut c, b, "End", ui::Kind::Secondary, self.state(b));
                        self.end_buttons.push((wnd.id, b));
                    }
                    y = card.y + card.h + 24;
                }
                y = ui::section_label(&mut c, mx, y, "Background tasks") + 4;
                let tasks = task::list();
                let col_w = mw / 2;
                for (i, (id, name, state)) in tasks.iter().enumerate() {
                    let (cx, cy) = (mx + (i % 2) as i32 * col_w, y + (i / 2) as i32 * 26);
                    if cy > h - 30 {
                        break;
                    }
                    let dot = match *state {
                        "running" => 0x8FD694,
                        "ready" => 0xF2C46D,
                        _ => ui::FAINT,
                    };
                    c.fill_circle(cx + 6, cy + 9, 4, dot);
                    font::draw_clipped(&mut c, cx + 18, cy, &format!("{name}  \u{00b7}  #{id}  \u{00b7}  {state}"), 13, Weight::Regular, ui::DIM, col_w - 24);
                }
            }
            Page::Hardware => {
                let (u, f) = memory::heap_usage();
                let cw = (mw - 16) / 2;
                Self::card(&mut c, Rect::new(mx, 74, cw, 84), "Processor", &crate::cpu::brand());
                Self::card(&mut c, Rect::new(mx + cw + 16, 74, cw, 84), "Memory", &fmt_bytes((u + f) as u64));
                let mut y = ui::section_label(&mut c, mx, 178, "Disks");
                for d in storage::list() {
                    ui::symbol(&mut c, "drive", ui::DIM, Rect::new(mx, y + 1, 16, 16));
                    font::draw_clipped(&mut c, mx + 24, y, &format!("{}  \u{00b7}  {}{}", d.name, fmt_bytes(d.sectors * 512), if d.is_boot { "  \u{00b7}  boot disk" } else { "" }), ui::BODY, Weight::Regular, ui::TEXT, mw);
                    y += 24;
                }
                y = ui::section_label(&mut c, mx, y + 12, "Devices and drivers");
                for r in drivers::records() {
                    if y > h - 30 {
                        break;
                    }
                    let (chip, chip_color) = match &r.status {
                        drivers::Status::Bound { driver, .. } => (driver.to_string(), ui::SUCCESS),
                        drivers::Status::Failed { driver, .. } => (format!("{driver} failed"), ui::DANGER),
                        drivers::Status::NotNeeded => ("built in".into(), ui::DIM),
                        drivers::Status::Missing => ("no driver".into(), ui::WARNING),
                        drivers::Status::Ported { .. } => ("port drafted".into(), ui::INFO),
                    };
                    let name = format!("{} {}", pci::vendor_name(r.device.vendor), r.device.class_name());
                    font::draw_clipped(&mut c, mx, y, &name, ui::BODY, Weight::Regular, ui::TEXT, mw - 160);
                    let tw = font::width(&chip, ui::LABEL, Weight::Bold) + 16;
                    ui::badge(&mut c, mx + mw - tw, y - 1, &chip, chip_color);
                    y += 28;
                }
            }
        }
        wm::present(&self.win, &c);
    }
}
