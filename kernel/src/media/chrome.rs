//! The look the media apps share, on top of ui.rs: the slim control bar (transport,
//! time, seek bar, the tapered volume wedge, the tool glyphs), popover panels with
//! tabs and check rows, sliders, media cards with progress, and the keyboard-shortcuts
//! sheet. Each app passes its signature colour (`accent`): tangerine for Videos,
//! magenta for Music, gold for Photos; the rest is Nova's palette.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::font::{self, Weight};
use crate::gfx::{blend, Canvas, Rect};
use crate::ui::{self, State};

pub const VIDEO_ACCENT: u32 = 0xF28A2E;
pub const MUSIC_ACCENT: u32 = 0xE0559A;
pub const PHOTOS_ACCENT: u32 = 0xE8B341;

/// Height of the control bar.
pub const BAR_H: i32 = 56;
/// Sidebar width of the library windows.
pub const SIDEBAR_W: i32 = 196;
/// Header (breadcrumb) height.
pub const TOP_H: i32 = 56;

/// A darker version of an accent that keeps white text at 4.5:1.
pub fn accent_fill(accent: u32) -> u32 {
    blend(accent, 0, 40)
}

/// The accent at low opacity over a surface (selection fills).
pub fn accent_tint(surface: u32, accent: u32) -> u32 {
    blend(surface, accent, 56)
}

// ----- control bar ----------------------------------------------------------------

/// The play/pause disc: `d` px, accent fill, white glyph.
pub fn play_disc(c: &mut Canvas, cx: i32, cy: i32, d: i32, playing: bool, state: State, accent: u32) {
    let fill = ui::fill_for(accent, state);
    c.fill_circle(cx, cy, d / 2, fill);
    let g = d * 9 / 20;
    let r = Rect::new(cx - g / 2 + if playing { 0 } else { d / 24 }, cy - g / 2, g, g);
    ui::symbol(c, if playing { "pause" } else { "play" }, 0xFFFFFF, r);
}

/// A flat glyph button in the bar: no plate until hovered; `on` tints it accent.
pub fn tool(c: &mut Canvas, r: Rect, glyph: &str, state: State, on: bool, accent: u32) {
    if matches!(state, State::Hover | State::Pressed) {
        c.fill_round_rect(r, ui::RADIUS_S, ui::fill_for(ui::RAISED, state));
    }
    let color = match state {
        State::Disabled => ui::FAINT,
        _ if on => accent,
        State::Normal => ui::DIM,
        _ => ui::TEXT,
    };
    let g = (r.h * 5 / 9).min(20);
    ui::symbol(c, glyph, color, Rect::new(r.x + (r.w - g) / 2, r.y + (r.h - g) / 2, g, g));
}

/// The seek bar: a hairline track that thickens under the pointer, the played part in
/// the accent, a knob while hovered or dragged. `hover_x` is the pointer's x if it is
/// over the bar. Returns nothing; hit-test with `seek_value`.
pub fn seek_bar(c: &mut Canvas, r: Rect, pos: i64, len: i64, hover: bool, accent: u32) {
    let th = if hover { 6 } else { 4 };
    let track = Rect::new(r.x, r.y + (r.h - th) / 2, r.w, th);
    c.fill_round_rect(track, th / 2, blend(ui::RAISED, 0xFFFFFF, 20));
    let fw = if len > 0 { (r.w as i64 * pos.clamp(0, len) / len) as i32 } else { 0 };
    if fw > 0 {
        c.fill_round_rect(Rect::new(track.x, track.y, fw.max(th), th), th / 2, accent);
    }
    if hover {
        let kx = r.x + fw;
        c.fill_circle(kx, r.y + r.h / 2, 7, 0xFFFFFF);
        c.fill_circle(kx, r.y + r.h / 2, 5, accent);
    }
}

/// The position (ms) under `x` on a seek bar `r` for a media `len` ms long.
pub fn seek_value(r: Rect, x: i32, len: i64) -> i64 {
    if r.w <= 0 || len <= 0 {
        return 0;
    }
    len * (x - r.x).clamp(0, r.w) as i64 / r.w as i64
}

/// The volume wedge: a tapered bar whose fill runs from the accent to a lighter tint.
pub fn volume_wedge(c: &mut Canvas, r: Rect, volume: i32, muted: bool, accent: u32) {
    let w = r.w.max(8);
    let filled = (w * volume.clamp(0, 200) / 100).min(w);
    let light = blend(accent, 0xFFFFFF, 90);
    for i in 0..w {
        let h = 3 + (r.h - 3) * i / w;
        let y = r.y + r.h - h;
        let col = if i < filled && !muted { blend(accent, light, (i * 255 / w) as u32) } else { blend(ui::RAISED, 0xFFFFFF, 24) };
        c.fill_rect(Rect::new(r.x + i, y, 1, h), col);
    }
    // Over 100 %: a mark where 100 sits.
    if volume > 100 && !muted {
        let x = r.x + w / 2;
        c.fill_rect(Rect::new(x, r.y, 1, r.h), 0xFFFFFF);
    }
}

pub fn volume_value(r: Rect, x: i32) -> i32 {
    ((x - r.x).clamp(0, r.w) * 100 / r.w.max(1)).clamp(0, 100)
}

/// "1:23 / 4:56"-style time in the caption size; returns the width.
pub fn time_text(c: &mut Canvas, x: i32, y: i32, text: &str, color: u32) -> i32 {
    font::draw(c, x, y, text, 13, Weight::Regular, color);
    font::width(text, 13, Weight::Regular)
}

// ----- popovers -------------------------------------------------------------------

/// A popover panel (raised surface, 12 px corners, soft shadow) anchored above `anchor`
/// inside `bounds`; returns its rect.
pub fn popover_rect(anchor: Rect, w: i32, h: i32, bounds: Rect) -> Rect {
    let x = (anchor.x + anchor.w / 2 - w / 2).clamp(bounds.x + 8, (bounds.x + bounds.w - w - 8).max(bounds.x + 8));
    let y = (anchor.y - h - 10).max(bounds.y + 8);
    Rect::new(x, y, w, h)
}

pub fn popover(c: &mut Canvas, r: Rect) {
    c.shadow_round(r, ui::RADIUS_L, 18, 120);
    c.fill_round_rect(r, ui::RADIUS_L, ui::RAISED);
    c.stroke_round_rect_w(r, ui::RADIUS_L, 1, blend(ui::RAISED, 0xFFFFFF, 22));
}

/// Tabs across the top of a popover: label and glyph per tab; the active one is
/// underlined in the accent. Returns the tab rects.
pub fn tabs(c: &mut Canvas, r: Rect, items: &[(&str, &str)], active: usize, cursor: Option<(i32, i32)>, accent: u32) -> Vec<Rect> {
    let n = items.len().max(1) as i32;
    let w = r.w / n;
    let mut rects = Vec::new();
    for (i, (glyph, label)) in items.iter().enumerate() {
        let t = Rect::new(r.x + i as i32 * w, r.y, w, r.h);
        let hot = cursor.is_some_and(|(x, y)| t.contains(x, y));
        let on = i == active;
        let color = if on { ui::TEXT } else if hot { ui::TEXT } else { ui::DIM };
        let tw = font::width(label, ui::BODY, if on { Weight::Bold } else { Weight::Regular });
        let total = tw + 22;
        let x0 = t.x + (t.w - total) / 2;
        ui::symbol(c, glyph, if on { accent } else { color }, Rect::new(x0, t.y + (t.h - 16) / 2, 16, 16));
        font::draw(c, x0 + 22, t.y + (t.h - font::line_height(ui::BODY)) / 2, label, ui::BODY, if on { Weight::Bold } else { Weight::Regular }, color);
        if on {
            c.fill_round_rect(Rect::new(x0 - 4, t.y + t.h - 3, total + 8, 3), 1, accent);
        }
        rects.push(t);
    }
    c.fill_rect(Rect::new(r.x + 12, r.y + r.h - 1, r.w - 24, 1), blend(ui::RAISED, 0xFFFFFF, 20));
    rects
}

/// A choice row inside a popover: a check mark when selected, hover fill.
pub fn check_row(c: &mut Canvas, r: Rect, label: &str, detail: &str, checked: bool, state: State, accent: u32) {
    if matches!(state, State::Hover | State::Pressed) {
        c.fill_round_rect(Rect::new(r.x + 6, r.y, r.w - 12, r.h), ui::RADIUS_S, ui::fill_for(ui::RAISED, state));
    }
    if checked {
        ui::symbol(c, "check", accent, Rect::new(r.x + 16, r.y + (r.h - 16) / 2, 16, 16));
    }
    let ty = r.y + (r.h - font::line_height(ui::BODY)) / 2;
    let dw = if detail.is_empty() { 0 } else { font::width(detail, ui::CAPTION, Weight::Regular) + 12 };
    font::draw_clipped(c, r.x + 44, ty, label, ui::BODY, if checked { Weight::Bold } else { Weight::Regular }, ui::TEXT, r.w - 60 - dw);
    if !detail.is_empty() {
        font::draw(c, r.x + r.w - 16 - dw + 12, ty + 1, detail, ui::CAPTION, Weight::Regular, ui::DIM);
    }
}

/// A labelled slider with its value at the right: returns the knob track rect.
pub fn slider(c: &mut Canvas, r: Rect, label: &str, value_text: &str, frac: i32, state: State, accent: u32) -> Rect {
    font::draw(c, r.x, r.y, label, ui::CAPTION, Weight::Bold, ui::DIM);
    let vw = font::width(value_text, ui::CAPTION, Weight::Regular);
    font::draw(c, r.x + r.w - vw, r.y, value_text, ui::CAPTION, Weight::Regular, ui::TEXT);
    let track = Rect::new(r.x, r.y + 22, r.w, 20);
    let line = Rect::new(track.x, track.y + 8, track.w, 4);
    c.fill_round_rect(line, 2, blend(ui::RAISED, 0xFFFFFF, 24));
    let kx = track.x + track.w * frac.clamp(0, 1000) / 1000;
    // Centre mark for symmetric ranges (delays).
    c.fill_rect(Rect::new(track.x + track.w / 2, track.y + 4, 1, 12), blend(ui::RAISED, 0xFFFFFF, 50));
    c.fill_circle(kx, track.y + 10, if state == State::Normal { 7 } else { 8 }, 0xFFFFFF);
    c.fill_circle(kx, track.y + 10, 5, accent);
    track
}

pub fn slider_value(track: Rect, x: i32) -> i32 {
    ((x - track.x).clamp(0, track.w) * 1000 / track.w.max(1)).clamp(0, 1000)
}

/// A segmented choice (Dark / Light / System, speeds): returns the segment rects.
pub fn segments(c: &mut Canvas, r: Rect, items: &[&str], active: usize, cursor: Option<(i32, i32)>, accent: u32) -> Vec<Rect> {
    c.fill_round_rect(r, ui::RADIUS_S, blend(ui::RAISED, 0, 60));
    let n = items.len().max(1) as i32;
    let w = (r.w - 4) / n;
    let mut out = Vec::new();
    for (i, label) in items.iter().enumerate() {
        let s = Rect::new(r.x + 2 + i as i32 * w, r.y + 2, w, r.h - 4);
        let hot = cursor.is_some_and(|(x, y)| s.contains(x, y));
        if i == active {
            c.fill_round_rect(s, ui::RADIUS_S - 2, accent_tint(ui::RAISED, accent));
            c.stroke_round_rect_w(s, ui::RADIUS_S - 2, 1, blend(accent, ui::RAISED, 110));
        } else if hot {
            c.fill_round_rect(s, ui::RADIUS_S - 2, ui::HOVER);
        }
        let tw = font::width(label, ui::CAPTION, Weight::Bold);
        font::draw(c, s.x + (s.w - tw) / 2, s.y + (s.h - font::line_height(ui::CAPTION)) / 2, label, ui::CAPTION, Weight::Bold, if i == active { ui::TEXT } else { ui::DIM });
        out.push(s);
    }
    out
}

// ----- library pieces -------------------------------------------------------------

/// A media card: a 16:9 (or square) picture with rounded corners, a placeholder glyph
/// while there is no picture, an optional progress line (Continue Watching), a title
/// and a caption below. `r` is the picture rect; text goes under it.
pub fn card(c: &mut Canvas, r: Rect, img: Option<&Canvas>, glyph: &str, title: &str, caption: &str, progress: Option<(i64, i64)>, state: State, selected: bool, accent: u32) {
    let hot = matches!(state, State::Hover | State::Pressed) || selected;
    let outer = Rect::new(r.x - 6, r.y - 6, r.w + 12, r.h + 12 + 44);
    if hot {
        c.fill_round_rect(outer, ui::RADIUS + 4, if selected { ui::SELECTION } else { ui::HOVER });
    }
    match img {
        Some(i) => fill_with(c, r, i, ui::RADIUS),
        None => {
            c.fill_round_rect(r, ui::RADIUS, ui::PANEL);
            c.stroke_round_rect_w(r, ui::RADIUS, 1, ui::BORDER);
            let g = (r.h / 3).min(40);
            ui::symbol(c, glyph, ui::FAINT, Rect::new(r.x + (r.w - g) / 2, r.y + (r.h - g) / 2, g, g));
        }
    }
    if hot {
        c.stroke_round_rect_w(r, ui::RADIUS, 1, blend(ui::TEXT, 0, 40));
    }
    if let Some((pos, len)) = progress
        && len > 0
    {
        let bar = Rect::new(r.x + 8, r.y + r.h - 10, r.w - 16, 4);
        c.fill_round_rect(bar, 2, blend(0, 0xFFFFFF, 90));
        let fw = (bar.w as i64 * pos.clamp(0, len) / len) as i32;
        c.fill_round_rect(Rect::new(bar.x, bar.y, fw.max(4), 4), 2, accent);
    }
    font::draw_clipped(c, r.x + 2, r.y + r.h + 8, title, 13, Weight::Bold, ui::TEXT, r.w - 4);
    font::draw_clipped(c, r.x + 2, r.y + r.h + 26, caption, ui::CAPTION, Weight::Regular, ui::DIM, r.w - 4);
}

/// `img` scaled to fill `r` with the overflow cropped (thumbnails, tiles), rounded.
pub fn fill_with(c: &mut Canvas, r: Rect, img: &Canvas, radius: i32) {
    c.fill_round_rect(r, radius, 0x000000);
    let (iw, ih) = (img.width as i32, img.height as i32);
    let (tw, th) = if iw * r.h > ih * r.w { ((iw * r.h / ih.max(1)).max(1), r.h) } else { (r.w, (ih * r.w / iw.max(1)).max(1)) };
    let mut tmp = Canvas::new(tw as usize, th as usize);
    tmp.blit_scaled(img, tmp.bounds(), 0, 255);
    let mut crop = Canvas::new(r.w as usize, r.h as usize);
    crop.blit(&tmp, Rect::new((tw - r.w) / 2, (th - r.h) / 2, r.w, r.h), 0, 0);
    c.blit_scaled(&crop, r, radius, 255);
}

/// A row title with an optional right-hand note ("Continue Watching", "4 items").
pub fn row_title(c: &mut Canvas, x: i32, y: i32, w: i32, title: &str, note: &str) {
    font::draw(c, x, y, title, ui::TITLE_2, Weight::Bold, ui::TEXT);
    if !note.is_empty() {
        let nw = font::width(note, ui::CAPTION, Weight::Regular);
        font::draw(c, x + w - nw, y + 6, note, ui::CAPTION, Weight::Regular, ui::DIM);
    }
}

/// Breadcrumbs "Videos > Shows > S01": returns (segment index, rect) pairs.
pub fn crumbs(c: &mut Canvas, x: i32, y: i32, parts: &[&str], cursor: Option<(i32, i32)>) -> Vec<Rect> {
    let mut rects = Vec::new();
    let mut cx = x;
    let last = parts.len().saturating_sub(1);
    for (i, p) in parts.iter().enumerate() {
        let tw = font::width(p, ui::HEADING, Weight::Bold);
        let r = Rect::new(cx - 6, y - 4, tw + 12, 28);
        let hot = i != last && cursor.is_some_and(|(mx, my)| r.contains(mx, my));
        if hot {
            c.fill_round_rect(r, 6, ui::HOVER);
        }
        font::draw(c, cx, y, p, ui::HEADING, Weight::Bold, if i == last || hot { ui::TEXT } else { ui::DIM });
        rects.push(r);
        cx += tw + 26;
        if i < last {
            ui::symbol(c, "chevron-right", ui::FAINT, Rect::new(cx - 20, y + 4, 12, 12));
        }
    }
    rects
}

/// A queue row: drag handle or play mark, title, duration; the current one in the accent.
pub fn queue_row(c: &mut Canvas, r: Rect, title: &str, duration: &str, current: bool, playing: bool, state: State, accent: u32) {
    if matches!(state, State::Hover | State::Pressed) {
        c.fill_round_rect(r, ui::RADIUS_S, ui::fill_for(ui::RAISED, state));
    }
    let color = if current { accent } else { ui::TEXT };
    if current {
        ui::symbol(c, if playing { "audio" } else { "play" }, accent, Rect::new(r.x + 12, r.y + (r.h - 14) / 2, 14, 14));
    } else {
        ui::symbol(c, "menu", ui::FAINT, Rect::new(r.x + 12, r.y + (r.h - 14) / 2, 14, 14));
    }
    let ty = r.y + (r.h - font::line_height(ui::BODY)) / 2;
    let dw = font::width(duration, 13, Weight::Regular);
    font::draw_clipped(c, r.x + 40, ty, title, ui::BODY, if current { Weight::Bold } else { Weight::Regular }, color, r.w - 56 - dw);
    font::draw(c, r.x + r.w - 12 - dw, ty + 1, duration, 13, Weight::Regular, ui::DIM);
}

/// The first-run sheet: "Learn the basic keyboard shortcuts" with key caps in two
/// columns and one button. Returns the button rect.
pub fn shortcuts_sheet(c: &mut Canvas, area: Rect, title: &str, rows: &[(&str, &[&str])], cursor: Option<(i32, i32)>, accent: u32) -> Rect {
    c.blend_rect(area, 0, 150);
    let cols = 2;
    let per_col = rows.len().div_ceil(cols);
    let w = 620.min(area.w - 32);
    let h = 110 + per_col as i32 * 34 + 40;
    let r = Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h);
    popover(c, r);
    let tw = font::width(title, ui::TITLE_2, Weight::Bold);
    font::draw(c, r.x + (r.w - tw) / 2, r.y + 24, title, ui::TITLE_2, Weight::Bold, ui::TEXT);
    let sub = "You can also use the mouse; these are the shortcuts people use most.";
    let sw = font::width(sub, 13, Weight::Regular);
    font::draw(c, r.x + (r.w - sw) / 2, r.y + 54, sub, 13, Weight::Regular, ui::DIM);
    let col_w = (r.w - 48) / cols as i32;
    for (i, (what, keys)) in rows.iter().enumerate() {
        let x = r.x + 24 + (i / per_col) as i32 * col_w;
        let y = r.y + 92 + (i % per_col) as i32 * 34;
        font::draw(c, x, y + 3, what, 13, Weight::Regular, ui::TEXT);
        let mut kx = x + col_w - 16;
        for k in keys.iter().rev() {
            let kw = font::width(k, ui::CAPTION, Weight::Bold) + 14;
            kx -= kw;
            ui::kbd(c, kx, y, k);
            kx -= 6;
        }
    }
    let button = Rect::new(r.x + r.w - 24 - 96, r.y + r.h - 24 - ui::CONTROL_H, 96, ui::CONTROL_H);
    let state = ui::state(cursor, false, button);
    c.fill_round_rect(button, ui::RADIUS_S, ui::fill_for(accent_fill(accent), state));
    let bw = font::width("Got it", ui::BODY, Weight::Bold);
    font::draw(c, button.x + (button.w - bw) / 2, button.y + (button.h - font::line_height(ui::BODY)) / 2, "Got it", ui::BODY, Weight::Bold, 0xFFFFFF);
    button
}

/// A placeholder picture for music without cover art: a disc on a tinted tile.
pub fn art_placeholder(c: &mut Canvas, r: Rect, seed: &str, accent: u32) {
    let h = seed.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let tint = blend(ui::PANEL, accent, 30 + (h % 40));
    c.fill_round_rect(r, ui::RADIUS, tint);
    c.gradient(Rect::new(r.x, r.y, r.w, r.h / 2), blend(tint, 0xFFFFFF, 10), tint);
    let g = r.w / 2;
    ui::symbol(c, "music", blend(tint, 0xFFFFFF, 120), Rect::new(r.x + (r.w - g) / 2, r.y + (r.h - g) / 2, g, g));
}

/// A square cover (fit, rounded), or the placeholder.
pub fn cover(c: &mut Canvas, r: Rect, img: Option<&Canvas>, seed: &str, accent: u32) {
    match img {
        Some(i) => {
            let (iw, ih) = (i.width as i32, i.height as i32);
            let (tw, th) = if iw * r.h > ih * r.w { ((iw * r.h / ih.max(1)).max(1), r.h) } else { (r.w, (ih * r.w / iw.max(1)).max(1)) };
            let mut tmp = Canvas::new(tw as usize, th as usize);
            tmp.blit_scaled(i, tmp.bounds(), 0, 255);
            let mut crop = Canvas::new(r.w as usize, r.h as usize);
            crop.blit(&tmp, Rect::new((tw - r.w) / 2, (th - r.h) / 2, r.w, r.h), 0, 0);
            c.blit_scaled(&crop, r, ui::RADIUS, 255);
        }
        None => art_placeholder(c, r, seed, accent),
    }
}

/// Keyboard hint text "Space  Play/Pause" rows for empty states.
pub fn hint_rows(c: &mut Canvas, x: i32, y: i32, rows: &[(&str, &str)]) -> i32 {
    let mut y = y;
    for (k, what) in rows {
        let end = ui::kbd(c, x, y, k);
        font::draw(c, end + 10, y + 3, what, 13, Weight::Regular, ui::DIM);
        y += 30;
    }
    y
}

/// Ellipsis-truncate to `max_w` for places without clipping helpers.
pub fn fit_text(text: &str, size: u16, weight: Weight, max_w: i32) -> String {
    if font::width(text, size, weight) <= max_w {
        return text.into();
    }
    let mut s: String = text.into();
    while !s.is_empty() && font::width(&format!("{s}\u{2026}"), size, weight) > max_w {
        s.pop();
    }
    format!("{s}\u{2026}")
}
