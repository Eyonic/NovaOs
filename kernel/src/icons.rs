//! App icons: generated artwork (`assets/icons/`, shipped as
//! `usr/share/nova/icons/<name>.png`) for the built-in apps, keyed by the icon
//! character apps and windows already carry. Apps without an icon image get a
//! rounded tile in their colour with the character on it.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::gfx::{Canvas, Rect};
use crate::sync::SpinLock;
use crate::{fs, textgrid};

fn name_for(icon: char) -> Option<&'static str> {
    Some(match icon {
        '*' => "assistant",
        '@' => "browser",
        '>' => "terminal",
        '\u{2261}' => "files",  // ≡
        '\u{00b6}' => "editor", // ¶
        '%' => "monitor",
        '\u{263c}' => "settings", // ☼
        'o' => "power",
        '\u{25B6}' => "video",  // ▶
        '\u{266A}' => "music",  // ♪
        '\u{25A3}' => "photos", // ▣
        _ => return None,
    })
}

fn image(name: &str) -> Option<Arc<Canvas>> {
    static CACHE: SpinLock<Vec<(String, Option<Arc<Canvas>>)>> = SpinLock::new(Vec::new());
    if let Some((_, c)) = CACHE.lock().iter().find(|(n, _)| n == name) {
        return c.clone();
    }
    let c = fs::system_file(&format!("usr/share/nova/icons/{name}.png")).and_then(|d| Canvas::from_png(d).ok()).map(Arc::new);
    CACHE.lock().push((name.into(), c.clone()));
    c
}

/// Draw the icon for `icon` (an app's icon character) into the square `r`.
pub fn draw(scene: &mut Canvas, icon: char, color: u32, r: Rect) {
    let radius = r.w * 23 / 100;
    if let Some(img) = name_for(icon).and_then(image) {
        scene.blit_scaled(&img, r, radius, 255);
        return;
    }
    scene.fill_round_rect(r, radius, color);
    let scale = (r.w / 18).max(1);
    let g = textgrid::glyph(icon);
    scene.glyph(r.x + (r.w - 8 * scale) / 2, r.y + (r.h - 16 * scale) / 2, g, 0xFFFFFF, None, scale);
}

/// Draw a toolbar glyph (`assets/glyphs/<name>.png`: back, forward, reload, home, lock,
/// shield; generated in one style, forward mirrored from back) tinted `color` in `r`.
/// Returns false if the glyph is missing.
pub fn glyph(scene: &mut Canvas, name: &str, color: u32, r: Rect) -> bool {
    static CACHE: SpinLock<Vec<(String, Option<Arc<Canvas>>)>> = SpinLock::new(Vec::new());
    let cached = CACHE.lock().iter().find(|(n, _)| n == name).map(|(_, c)| c.clone());
    let img = cached.unwrap_or_else(|| {
        let c = fs::system_file(&format!("usr/share/nova/glyphs/{name}.png")).and_then(|d| Canvas::from_png(d).ok()).map(Arc::new);
        CACHE.lock().push((name.into(), c.clone()));
        c
    });
    match img {
        Some(m) => {
            scene.draw_mask(&m, r, color);
            true
        }
        None => false,
    }
}
