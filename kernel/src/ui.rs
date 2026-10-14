//! Shared look and helpers for Nova's native apps (Text Editor, Files): the colour
//! palette, a system-wide clipboard, and small widgets drawn with the UI font.

use alloc::string::String;

use crate::font::{self, Weight};
use crate::gfx::{Canvas, Rect};
use crate::sync::SpinLock;

pub const BG: u32 = 0x16171F;
pub const SIDEBAR: u32 = 0x1B1C27;
pub const PANEL: u32 = 0x20212E;
pub const BORDER: u32 = 0x2C2E40;
pub const HOVER: u32 = 0x2B2D40;
pub const TEXT: u32 = 0xE6E7F0;
pub const DIM: u32 = 0x8E90A6;
pub const FAINT: u32 = 0x5D6078;
pub const ACCENT: u32 = 0x8B7CF6;
pub const SELECTION: u32 = 0x3A3470;
pub const CURRENT_LINE: u32 = 0x1D1E2A;
pub const DANGER: u32 = 0xF0717E;

static CLIPBOARD: SpinLock<String> = SpinLock::new(String::new());

/// Copy text to the system clipboard (shared by all native apps).
pub fn copy(text: &str) {
    *CLIPBOARD.lock() = text.into();
}

pub fn paste() -> String {
    CLIPBOARD.lock().clone()
}

/// A rounded button with a centred label; `primary` uses the accent colour.
pub fn button(c: &mut Canvas, r: Rect, label: &str, primary: bool) {
    c.fill_round_rect(r, r.h / 2, if primary { ACCENT } else { HOVER });
    let tw = font::width(label, 14, Weight::Bold);
    font::draw(c, r.x + (r.w - tw) / 2, r.y + (r.h - font::line_height(14)) / 2, label, 14, Weight::Bold, if primary { 0xFFFFFF } else { TEXT });
}

/// A text field: rounded, with a placeholder when empty and a caret when focused.
pub fn field(c: &mut Canvas, r: Rect, text: &str, placeholder: &str, focused: bool) {
    c.fill_round_rect(r, 8, if focused { 0x262839 } else { PANEL });
    if focused {
        c.stroke_round_rect(r, 8, ACCENT);
    }
    let ty = r.y + (r.h - font::line_height(14)) / 2;
    if text.is_empty() {
        font::draw(c, r.x + 12, ty, placeholder, 14, Weight::Regular, FAINT);
        if focused {
            c.fill_rect(Rect::new(r.x + 11, ty + 2, 2, 16), ACCENT);
        }
    } else {
        let end = font::draw_clipped(c, r.x + 12, ty, text, 14, Weight::Regular, TEXT, r.w - 24);
        if focused {
            c.fill_rect(Rect::new(end + 1, ty + 2, 2, 16), ACCENT);
        }
    }
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
