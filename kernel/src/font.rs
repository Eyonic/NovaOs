//! Anti-aliased UI text: Noto Sans (Regular and Bold, SIL Open Font License) rasterised
//! with fontdue. The desktop (Home, window titles, Nova Browser's toolbar) uses it;
//! terminals keep the 8x16 bitmap font for their character grid.
//!
//! The fonts are system files (`usr/share/fonts/noto/NotoSans-{Regular,Bold}.ttf`,
//! shipped by scripts/build.sh). Without them every call falls back to the bitmap
//! font, so the desktop still works. Rasterised glyphs are cached per size and weight.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use fontdue::{Font, FontSettings};

use crate::fs;
use crate::gfx::{blend, Canvas};
use crate::sync::SpinLock;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Weight {
    Regular,
    Bold,
    /// Monospace (terminals, code): Adwaita Mono, else DejaVu Sans Mono.
    Mono,
    MonoBold,
}

struct Fonts {
    regular: Option<Font>,
    bold: Option<Font>,
    mono: Option<Font>,
    mono_bold: Option<Font>,
    /// (weight, size in px, char) -> (advance, x offset, y offset from baseline, w, h, coverage)
    glyphs: BTreeMap<(Weight, u16, char), Glyph>,
}

struct Glyph {
    advance: f32,
    xmin: i32,
    ymin: i32,
    width: usize,
    height: usize,
    coverage: Vec<u8>,
}

static FONTS: SpinLock<Option<Fonts>> = SpinLock::new(None);

fn load(path: &str) -> Option<Font> {
    let data = fs::system_file(path)?;
    Font::from_bytes(data, FontSettings::default()).ok()
}

fn with_fonts<R>(f: impl FnOnce(&mut Fonts) -> R) -> R {
    let mut guard = FONTS.lock();
    let fonts = guard.get_or_insert_with(|| Fonts {
        regular: load("usr/share/fonts/noto/NotoSans-Regular.ttf"),
        bold: load("usr/share/fonts/noto/NotoSans-Bold.ttf"),
        mono: load("usr/share/fonts/mono/Mono-Regular.ttf"),
        mono_bold: load("usr/share/fonts/mono/Mono-Bold.ttf"),
        glyphs: BTreeMap::new(),
    });
    f(fonts)
}

/// Cell size (width, height) of the monospace font at `size`, if it is installed.
pub fn mono_cell(size: u16) -> Option<(i32, i32)> {
    with_fonts(|f| {
        f.mono.as_ref()?;
        let w = f.glyph(Weight::Mono, size, 'M').map(|g| g.advance)?;
        Some((rnd(w).max(1), line_height(size)))
    })
}

/// True when the UI font is installed (otherwise text uses the bitmap font).
pub fn available() -> bool {
    with_fonts(|f| f.regular.is_some())
}

impl Fonts {
    fn font(&self, weight: Weight) -> Option<&Font> {
        match weight {
            Weight::Bold => self.bold.as_ref().or(self.regular.as_ref()),
            Weight::Regular => self.regular.as_ref(),
            Weight::Mono => self.mono.as_ref(),
            Weight::MonoBold => self.mono_bold.as_ref().or(self.mono.as_ref()),
        }
    }

    fn glyph(&mut self, weight: Weight, size: u16, c: char) -> Option<&Glyph> {
        let key = (weight, size, c);
        if !self.glyphs.contains_key(&key) {
            let font = self.font(weight)?;
            let (m, coverage) = font.rasterize(c, size as f32);
            if self.glyphs.len() > 8000 {
                self.glyphs.clear();
            }
            self.glyphs.insert(key, Glyph { advance: m.advance_width, xmin: m.xmin, ymin: m.ymin, width: m.width, height: m.height, coverage });
        }
        self.glyphs.get(&key)
    }

    fn ascent(&self, weight: Weight, size: u16) -> i32 {
        self.font(weight).and_then(|f| f.horizontal_line_metrics(size as f32)).map_or(size as i32, |m| rnd(m.ascent))
    }
}

/// Width of `text` in pixels.
pub fn width(text: &str, size: u16, weight: Weight) -> i32 {
    with_fonts(|f| {
        if f.regular.is_none() {
            return text.chars().count() as i32 * 8 * scale_for(size);
        }
        rnd(text.chars().map(|c| f.glyph(weight, size, c).map_or(0.0, |g| g.advance)).sum::<f32>())
    })
}

/// Height of a line of text at `size` (ascent + descent + gap).
pub fn line_height(size: u16) -> i32 {
    (size as i32 * 4 + 2) / 3
}

/// Round to the nearest integer (no `f32::round` without std).
fn rnd(v: f32) -> i32 {
    if v >= 0.0 { (v + 0.5) as i32 } else { (v - 0.5) as i32 }
}

fn scale_for(size: u16) -> i32 {
    ((size as i32 + 8) / 16).max(1)
}

/// Draw `text` with its top-left corner at (x, y); returns the x where it ends.
pub fn draw(c: &mut Canvas, x: i32, y: i32, text: &str, size: u16, weight: Weight, color: u32) -> i32 {
    with_fonts(|f| {
        if f.regular.is_none() {
            // Bitmap fallback, scaled to roughly the requested size.
            let s = scale_for(size);
            return c.text_scaled(x, y + (size as i32 - 16 * s) / 2, text, color, s);
        }
        let baseline = y + f.ascent(weight, size);
        let mut pen = x as f32;
        for ch in text.chars() {
            let Some(g) = f.glyph(weight, size, ch) else { continue };
            let gx = rnd(pen) + g.xmin;
            let gy = baseline - g.ymin - g.height as i32;
            for row in 0..g.height {
                let py = gy + row as i32;
                if py < 0 || py >= c.height as i32 {
                    continue;
                }
                for col in 0..g.width {
                    let a = g.coverage[row * g.width + col] as u32;
                    let px = gx + col as i32;
                    if a == 0 || px < 0 || px >= c.width as i32 {
                        continue;
                    }
                    let i = py as usize * c.width + px as usize;
                    c.pixels[i] = if a >= 250 { color } else { blend(c.pixels[i], color, a) };
                }
            }
            pen += g.advance;
        }
        rnd(pen)
    })
}

/// Like [`draw`], cut with "..." to fit `max_w` pixels.
pub fn draw_clipped(c: &mut Canvas, x: i32, y: i32, text: &str, size: u16, weight: Weight, color: u32, max_w: i32) -> i32 {
    if width(text, size, weight) <= max_w {
        return draw(c, x, y, text, size, weight, color);
    }
    let dots = width("...", size, weight);
    let mut cut = alloc::string::String::new();
    for ch in text.chars() {
        cut.push(ch);
        if width(&cut, size, weight) + dots > max_w {
            cut.pop();
            break;
        }
    }
    cut.push_str("...");
    draw(c, x, y, &cut, size, weight, color)
}

/// Word-wrap `text` into lines no wider than `max_w`, at most `max_lines` (the last one
/// ends in "..." if text remains).
pub fn wrap(text: &str, size: u16, weight: Weight, max_w: i32, max_lines: usize) -> Vec<alloc::string::String> {
    let mut lines: Vec<alloc::string::String> = Vec::new();
    let mut cur = alloc::string::String::new();
    for word in text.split_whitespace() {
        let candidate = if cur.is_empty() { word.into() } else { alloc::format!("{cur} {word}") };
        if !cur.is_empty() && width(&candidate, size, weight) > max_w {
            lines.push(core::mem::take(&mut cur));
            if lines.len() == max_lines {
                if let Some(l) = lines.last_mut() {
                    l.push_str("...");
                }
                return lines;
            }
            cur = word.into();
        } else {
            cur = candidate;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}
