//! Shared look and helpers for Nova's native apps: the design tokens (colour, type
//! scale, spacing on a 4/8 px grid), a system-wide clipboard, and widgets drawn with
//! the UI font (buttons, fields, switches, sidebar rows, cards, empty states).
//!
//! The tokens follow what desktop toolkits converge on. Dark surfaces get lighter with
//! elevation (base, sidebar, card, popover) rather than relying on shadows; secondary
//! text keeps at least 4.5:1 contrast on the cards it sits on (WCAG AA) and the faint
//! colour is for hints and decoration only; the accent is reserved for the one primary
//! action, selection and focus, so it still means something. Controls are 32 px high,
//! boxed-list rows 44 px, sidebar rows 36 px; corners are 8 px on controls and 10-12 px
//! on cards and popovers. Interactive things show their state: hover and pressed fills,
//! a 2 px focus ring, disabled at reduced contrast.

use alloc::string::String;

use crate::font::{self, Weight};
use crate::gfx::{blend, Canvas, Rect};
use crate::icons;
use crate::sync::SpinLock;

// ----- Colour -------------------------------------------------------------------

/// Base layer (window content).
pub const BG: u32 = 0x16171F;
/// Navigation layer (sidebars, toolbars, status bars).
pub const SIDEBAR: u32 = 0x1B1C27;
/// Cards and boxed lists.
pub const PANEL: u32 = 0x20212E;
/// Popovers, menus, fields on cards.
pub const RAISED: u32 = 0x2A2C3C;
/// Hairlines (about 7% white over a card).
pub const BORDER: u32 = 0x2C2E40;
/// Hover state layer on the base and navigation layers.
pub const HOVER: u32 = 0x2B2D40;
pub const PRESSED: u32 = 0x34364C;
/// Primary text (13:1 on a card).
pub const TEXT: u32 = 0xE6E7F0;
/// Secondary text (5.9:1 on a card).
pub const DIM: u32 = 0x9A9CB2;
/// Tertiary: disabled controls, placeholders, decoration (3.4:1).
pub const FAINT: u32 = 0x6E7290;
/// The accent, for text, icons, selection and focus rings.
pub const ACCENT: u32 = 0x8B7CF6;
/// The accent as a fill behind white text (4.5:1).
pub const ACCENT_FILL: u32 = 0x7363E6;
pub const SELECTION: u32 = 0x3A3470;
pub const CURRENT_LINE: u32 = 0x1D1E2A;
pub const DANGER: u32 = 0xF0717E;
/// A destructive fill behind white text.
pub const DANGER_FILL: u32 = 0xB9394B;
pub const SUCCESS: u32 = 0x7FD69A;
pub const WARNING: u32 = 0xF2C46D;
pub const INFO: u32 = 0x7AA8FF;

// ----- Type scale (px, Noto Sans) -------------------------------------------------

/// Page titles (bold).
pub const TITLE: u16 = 24;
/// Section titles (bold).
pub const TITLE_2: u16 = 18;
/// Headings and emphasised rows (bold).
pub const HEADING: u16 = 15;
/// Body text and controls.
pub const BODY: u16 = 14;
/// Captions, secondary lines.
pub const CAPTION: u16 = 12;
/// Uppercase section labels and badges (bold).
pub const LABEL: u16 = 11;

// ----- Spacing and shape -----------------------------------------------------------

/// Surface edge to content.
pub const PAD: i32 = 16;
pub const GAP: i32 = 8;
pub const GAP_L: i32 = 12;
pub const CONTROL_H: i32 = 32;
/// Boxed-list rows.
pub const ROW_H: i32 = 44;
/// Sidebar rows.
pub const NAV_ROW_H: i32 = 36;
/// Controls.
pub const RADIUS_S: i32 = 8;
/// Cards.
pub const RADIUS: i32 = 10;
/// Popovers and menus.
pub const RADIUS_L: i32 = 12;

static CLIPBOARD: SpinLock<String> = SpinLock::new(String::new());

/// Copy text to the system clipboard (shared by all native apps).
pub fn copy(text: &str) {
    *CLIPBOARD.lock() = text.into();
}

pub fn paste() -> String {
    CLIPBOARD.lock().clone()
}

// ----- States ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Normal,
    Hover,
    Pressed,
    Disabled,
}

/// The state of a control at `r` given the pointer (`cursor`, None when it is not over
/// the window) and whether the button is held.
pub fn state(cursor: Option<(i32, i32)>, held: bool, r: Rect) -> State {
    match cursor {
        Some((x, y)) if r.contains(x, y) => {
            if held { State::Pressed } else { State::Hover }
        }
        _ => State::Normal,
    }
}

/// A fill colour for `state`: hover lightens by about 8%, pressed by 14%.
pub fn fill_for(base: u32, state: State) -> u32 {
    match state {
        State::Normal => base,
        State::Hover => blend(base, 0xFFFFFF, 20),
        State::Pressed => blend(base, 0xFFFFFF, 36),
        State::Disabled => base,
    }
}

// ----- Widgets --------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The one affirmative action: accent fill, white label.
    Primary,
    /// Everything else: a neutral raised fill.
    Secondary,
    /// Destructive: red fill, white label.
    Danger,
    /// No fill until hovered (toolbars).
    Flat,
}

/// A rounded button with a centred label.
pub fn button_kind(c: &mut Canvas, r: Rect, label: &str, kind: Kind, state: State) {
    let (fill, text) = match kind {
        Kind::Primary => (ACCENT_FILL, 0xFFFFFF),
        Kind::Secondary => (RAISED, TEXT),
        Kind::Danger => (DANGER_FILL, 0xFFFFFF),
        Kind::Flat => (0, TEXT),
    };
    let disabled = state == State::Disabled;
    if kind != Kind::Flat || state == State::Hover || state == State::Pressed {
        let fill = if kind == Kind::Flat { fill_for(RAISED, state) } else { fill_for(fill, state) };
        c.fill_round_rect(r, RADIUS_S.min(r.h / 2), if disabled { blend(fill, BG, 128) } else { fill });
    }
    if kind == Kind::Secondary && !disabled {
        c.stroke_round_rect_w(r, RADIUS_S.min(r.h / 2), 1, blend(RAISED, 0xFFFFFF, 18));
    }
    let tw = font::width(label, BODY, Weight::Bold);
    let color = if disabled { FAINT } else { text };
    font::draw(c, r.x + (r.w - tw) / 2, r.y + (r.h - font::line_height(BODY)) / 2, label, BODY, Weight::Bold, color);
}

/// A rounded button with a centred label; `primary` uses the accent colour.
pub fn button(c: &mut Canvas, r: Rect, label: &str, primary: bool) {
    button_kind(c, r, label, if primary { Kind::Primary } else { Kind::Secondary }, State::Normal);
}

/// A square or round button holding one symbolic glyph (toolbars).
pub fn icon_button(c: &mut Canvas, r: Rect, glyph: &str, state: State, round: bool) {
    let radius = if round { r.w / 2 } else { RADIUS_S };
    let fill = match state {
        State::Normal | State::Disabled => RAISED,
        s => fill_for(RAISED, s),
    };
    c.fill_round_rect(r, radius, fill);
    let color = if state == State::Disabled { FAINT } else { TEXT };
    let inset = r.w / 4;
    symbol(c, glyph, color, Rect::new(r.x + inset, r.y + inset, r.w - 2 * inset, r.h - 2 * inset));
}

/// A text field: rounded, with a placeholder when empty and a caret plus focus ring
/// when focused. Optional symbolic glyph (`icon`) at the left.
pub fn field_icon(c: &mut Canvas, r: Rect, text: &str, placeholder: &str, focused: bool, icon: Option<&str>) {
    c.fill_round_rect(r, RADIUS_S, if focused { RAISED } else { PANEL });
    if focused {
        c.stroke_round_rect_w(r, RADIUS_S, 2, ACCENT);
    } else {
        c.stroke_round_rect_w(r, RADIUS_S, 1, BORDER);
    }
    let mut x = r.x + GAP_L;
    if let Some(name) = icon {
        symbol(c, name, if focused { TEXT } else { FAINT }, Rect::new(x, r.y + (r.h - 16) / 2, 16, 16));
        x += 24;
    }
    let ty = r.y + (r.h - font::line_height(BODY)) / 2;
    if text.is_empty() {
        font::draw(c, x, ty, placeholder, BODY, Weight::Regular, FAINT);
        if focused {
            c.fill_rect(Rect::new(x - 1, ty + 2, 2, 16), ACCENT);
        }
    } else {
        let end = font::draw_clipped(c, x, ty, text, BODY, Weight::Regular, TEXT, r.x + r.w - x - GAP_L);
        if focused {
            c.fill_rect(Rect::new(end + 1, ty + 2, 2, 16), ACCENT);
        }
    }
}

pub fn field(c: &mut Canvas, r: Rect, text: &str, placeholder: &str, focused: bool) {
    field_icon(c, r, text, placeholder, focused, None);
}

/// A toggle switch (40x22): accent when on.
pub fn switch(c: &mut Canvas, r: Rect, on: bool, state: State) {
    let track = if on { fill_for(ACCENT_FILL, state) } else { fill_for(blend(RAISED, 0xFFFFFF, 24), state) };
    c.fill_round_rect(r, r.h / 2, track);
    let knob_r = r.h / 2 - 3;
    let kx = if on { r.x + r.w - 3 - knob_r - 1 } else { r.x + 3 + knob_r };
    c.fill_circle(kx, r.y + r.h / 2, knob_r, 0xFFFFFF);
}

/// A card: the boxed-list surface with a hairline.
pub fn card(c: &mut Canvas, r: Rect) {
    c.fill_round_rect(r, RADIUS, PANEL);
    c.stroke_round_rect_w(r, RADIUS, 1, BORDER);
}

/// An uppercase section label; returns the y below it.
pub fn section_label(c: &mut Canvas, x: i32, y: i32, text: &str) -> i32 {
    let up = text.to_uppercase();
    font::draw(c, x, y, &up, LABEL, Weight::Bold, DIM);
    y + font::line_height(LABEL) + 6
}

/// A symbolic glyph (`assets/glyphs/<name>.png`) tinted `color` in `r`. True if drawn.
pub fn symbol(c: &mut Canvas, name: &str, color: u32, r: Rect) -> bool {
    icons::glyph(c, name, color, r)
}

/// A coloured rounded square with a white symbolic glyph (sidebar section icons).
pub fn tile(c: &mut Canvas, r: Rect, color: u32, glyph: &str) {
    c.fill_round_rect(r, (r.w * 27 / 100).max(4), color);
    // A touch of top light so the tiles read as objects.
    c.fill_round_rect_corners(Rect::new(r.x, r.y, r.w, r.h / 2), (r.w * 27 / 100).max(4), 0xFFFFFF, crate::gfx::TOP_CORNERS, 22);
    let inset = r.w * 3 / 16;
    symbol(c, glyph, 0xFFFFFF, Rect::new(r.x + inset, r.y + inset, r.w - 2 * inset, r.h - 2 * inset));
}

/// A sidebar row: optional tile or glyph, label; selected rows get the selection fill,
/// hovered rows a lighter one.
pub fn nav_row(c: &mut Canvas, r: Rect, icon: Option<(&str, Option<u32>)>, label: &str, active: bool, state: State) {
    let fill = if active {
        Some(SELECTION)
    } else {
        match state {
            State::Hover => Some(HOVER),
            State::Pressed => Some(PRESSED),
            _ => None,
        }
    };
    if let Some(f) = fill {
        c.fill_round_rect(r, RADIUS_S, f);
    }
    let mut x = r.x + GAP_L;
    if let Some((glyph, tint)) = icon {
        let s = 20;
        let ir = Rect::new(x, r.y + (r.h - s) / 2, s, s);
        match tint {
            Some(color) => tile(c, ir, color, glyph),
            None => {
                symbol(c, glyph, if active { TEXT } else { DIM }, ir);
            }
        }
        x += s + GAP_L;
    }
    let weight = if active { Weight::Bold } else { Weight::Regular };
    font::draw_clipped(c, x, r.y + (r.h - font::line_height(BODY)) / 2, label, BODY, weight, TEXT, r.x + r.w - x - GAP);
}

/// A placeholder for an empty view: a large faint glyph, a heading and one line.
pub fn empty_state(c: &mut Canvas, area: Rect, glyph: &str, title: &str, text: &str) {
    let cy = area.y + area.h / 2 - 48;
    let cx = area.x + area.w / 2;
    c.fill_circle(cx, cy, 36, PANEL);
    symbol(c, glyph, FAINT, Rect::new(cx - 20, cy - 20, 40, 40));
    let tw = font::width(title, HEADING, Weight::Bold);
    font::draw(c, cx - tw / 2, cy + 52, title, HEADING, Weight::Bold, TEXT);
    let mut y = cy + 52 + font::line_height(HEADING) + 2;
    for line in font::wrap(text, BODY, Weight::Regular, area.w - 2 * PAD, 2) {
        let lw = font::width(&line, BODY, Weight::Regular);
        font::draw(c, cx - lw / 2, y, &line, BODY, Weight::Regular, DIM);
        y += font::line_height(BODY);
    }
}

/// A vertical scrollbar in `track` for `total` units of content of which `visible`
/// show, scrolled by `offset`. Nothing is drawn when everything fits.
pub fn scrollbar(c: &mut Canvas, track: Rect, total: i32, visible: i32, offset: i32) {
    if total <= visible || total <= 0 {
        return;
    }
    let thumb = (track.h * visible / total).max(24).min(track.h);
    let y = track.y + (track.h - thumb) * offset.clamp(0, total - visible) / (total - visible).max(1);
    c.fill_round_rect(Rect::new(track.x, y, track.w, thumb), track.w / 2, blend(BORDER, 0xFFFFFF, 30));
}

/// A small rounded badge with `text` in `color` (status chips); returns its width.
pub fn badge(c: &mut Canvas, x: i32, y: i32, text: &str, color: u32) -> i32 {
    let w = font::width(text, LABEL, Weight::Bold) + 16;
    c.fill_round_rect(Rect::new(x, y, w, 20), 10, blend(PANEL, color, 50));
    font::draw(c, x + 8, y + 3, text, LABEL, Weight::Bold, color);
    w
}

/// A keyboard key cap with `key` on it (key hints); returns the x after it.
pub fn kbd(c: &mut Canvas, x: i32, y: i32, key: &str) -> i32 {
    let w = font::width(key, CAPTION, Weight::Bold) + 14;
    c.fill_round_rect(Rect::new(x, y, w, 22), 6, RAISED);
    c.fill_rect(Rect::new(x + 2, y + 21, w - 4, 1), blend(RAISED, 0, 90));
    font::draw(c, x + 7, y + 3, key, CAPTION, Weight::Bold, TEXT);
    x + w
}

/// A hairline between two areas.
pub fn divider(c: &mut Canvas, r: Rect) {
    c.fill_rect(r, BORDER);
}

/// A transient message pill at the bottom of a window.
pub fn toast(c: &mut Canvas, area: Rect, text: &str, danger: bool) {
    let tw = font::width(text, BODY, Weight::Bold).min(area.w - 2 * PAD - 32);
    let w = tw + 32;
    let r = Rect::new(area.x + (area.w - w) / 2, area.y + area.h - PAD - 36, w, 36);
    c.shadow_round(r, RADIUS, 12, 110);
    c.fill_round_rect(r, RADIUS, if danger { DANGER_FILL } else { RAISED });
    font::draw_clipped(c, r.x + 16, r.y + (36 - font::line_height(BODY)) / 2, text, BODY, Weight::Bold, TEXT, tw);
}

/// Fuzzy match: every character of `query` appears in `text` in order (case-insensitive).
/// Returns a score (higher is better) or None.
pub fn fuzzy(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    let t: alloc::vec::Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
    let mut score = 0;
    let mut pos = 0;
    let mut last: Option<usize> = None;
    for q in query.chars().map(|c| c.to_ascii_lowercase()) {
        let i = (pos..t.len()).find(|&i| t[i] == q)?;
        // Consecutive matches and matches at word starts score higher.
        score += if last == Some(i.wrapping_sub(1)) { 8 } else { 1 };
        if i == 0 || matches!(t[i - 1], '/' | '.' | '_' | '-' | ' ') {
            score += 5;
        }
        last = Some(i);
        pos = i + 1;
    }
    Some(score - t.len() as i32 / 8)
}
