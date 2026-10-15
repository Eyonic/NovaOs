//! Off-screen pixel buffers and 2D drawing: everything the desktop and apps draw with.
//!
//! Pixels are 0x00RRGGBB. All drawing clips to the canvas.

use alloc::vec;
use alloc::vec::Vec;

use crate::textgrid::{self, FONT, FONT_HEIGHT, FONT_WIDTH};

#[derive(Clone)]
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }

    pub fn intersect(&self, o: &Rect) -> Option<Rect> {
        let x0 = self.x.max(o.x);
        let y0 = self.y.max(o.y);
        let x1 = (self.x + self.w).min(o.x + o.w);
        let y1 = (self.y + self.h).min(o.y + o.h);
        (x1 > x0 && y1 > y0).then(|| Rect::new(x0, y0, x1 - x0, y1 - y0))
    }

    pub fn union(&self, o: &Rect) -> Rect {
        let x0 = self.x.min(o.x);
        let y0 = self.y.min(o.y);
        let x1 = (self.x + self.w).max(o.x + o.w);
        let y1 = (self.y + self.h).max(o.y + o.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// Blend `fg` over `bg` with alpha 0-255.
pub fn blend(bg: u32, fg: u32, alpha: u32) -> u32 {
    let mix = |shift: u32| {
        let b = (bg >> shift) & 0xFF;
        let f = (fg >> shift) & 0xFF;
        ((f * alpha + b * (255 - alpha)) / 255) << shift
    };
    mix(16) | mix(8) | mix(0)
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Self {
        Self { width, height, pixels: vec![0; width * height] }
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        if (width, height) != (self.width, self.height) {
            *self = Canvas::new(width, height);
        }
    }

    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width as i32, self.height as i32)
    }

    pub fn clear(&mut self, color: u32) {
        self.pixels.fill(color);
    }

    #[inline]
    pub fn set(&mut self, x: i32, y: i32, color: u32) {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
            self.pixels[y as usize * self.width + x as usize] = color;
        }
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32) -> u32 {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height { self.pixels[y as usize * self.width + x as usize] } else { 0 }
    }

    pub fn fill_rect(&mut self, r: Rect, color: u32) {
        let Some(r) = r.intersect(&self.bounds()) else { return };
        for y in r.y..r.y + r.h {
            let row = y as usize * self.width;
            self.pixels[row + r.x as usize..row + (r.x + r.w) as usize].fill(color);
        }
    }

    /// Fill with `color` at `alpha` (0-255) over what is there.
    pub fn blend_rect(&mut self, r: Rect, color: u32, alpha: u32) {
        let Some(r) = r.intersect(&self.bounds()) else { return };
        for y in r.y..r.y + r.h {
            for x in r.x..r.x + r.w {
                let i = y as usize * self.width + x as usize;
                self.pixels[i] = blend(self.pixels[i], color, alpha);
            }
        }
    }

    pub fn stroke_rect(&mut self, r: Rect, color: u32) {
        self.fill_rect(Rect::new(r.x, r.y, r.w, 1), color);
        self.fill_rect(Rect::new(r.x, r.y + r.h - 1, r.w, 1), color);
        self.fill_rect(Rect::new(r.x, r.y, 1, r.h), color);
        self.fill_rect(Rect::new(r.x + r.w - 1, r.y, 1, r.h), color);
    }

    /// Blend one pixel: `color` at `alpha` over what is there.
    #[inline]
    pub fn blend_px(&mut self, x: i32, y: i32, color: u32, alpha: u32) {
        if x >= 0 && y >= 0 && (x as usize) < self.width && (y as usize) < self.height {
            let i = y as usize * self.width + x as usize;
            self.pixels[i] = if alpha >= 255 { color } else { blend(self.pixels[i], color, alpha) };
        }
    }

    /// Rectangle with all four corners rounded by `radius` (anti-aliased).
    pub fn fill_round_rect(&mut self, r: Rect, radius: i32, color: u32) {
        self.fill_round_rect_corners(r, radius, color, ALL_CORNERS, 255);
    }

    /// Rectangle with the `corners` in `which` rounded (see [`TOP_CORNERS`]), filled with
    /// `color` at `alpha`. The corners are anti-aliased, so edges look smooth at any size.
    pub fn fill_round_rect_corners(&mut self, r: Rect, radius: i32, color: u32, which: u8, alpha: u32) {
        let radius = radius.min(r.w / 2).min(r.h / 2).max(0);
        if radius == 0 || which == 0 {
            if alpha >= 255 { self.fill_rect(r, color) } else { self.blend_rect(r, color, alpha) }
            return;
        }
        let fill = |c: &mut Canvas, rr: Rect| if alpha >= 255 { c.fill_rect(rr, color) } else { c.blend_rect(rr, color, alpha) };
        // The middle band, then the top and bottom bands minus whatever corners are round.
        fill(self, Rect::new(r.x, r.y + radius, r.w, r.h - 2 * radius));
        for (band_y, left, right) in [(r.y, which & TOP_LEFT != 0, which & TOP_RIGHT != 0), (r.y + r.h - radius, which & BOTTOM_LEFT != 0, which & BOTTOM_RIGHT != 0)] {
            let x0 = if left { r.x + radius } else { r.x };
            let x1 = if right { r.x + r.w - radius } else { r.x + r.w };
            fill(self, Rect::new(x0, band_y, x1 - x0, radius));
        }
        for dy in 0..radius {
            for dx in 0..radius {
                let a = corner_coverage(dx, dy, radius) * alpha / 255;
                if a == 0 {
                    continue;
                }
                if which & TOP_LEFT != 0 {
                    self.blend_px(r.x + dx, r.y + dy, color, a);
                }
                if which & TOP_RIGHT != 0 {
                    self.blend_px(r.x + r.w - 1 - dx, r.y + dy, color, a);
                }
                if which & BOTTOM_LEFT != 0 {
                    self.blend_px(r.x + dx, r.y + r.h - 1 - dy, color, a);
                }
                if which & BOTTOM_RIGHT != 0 {
                    self.blend_px(r.x + r.w - 1 - dx, r.y + r.h - 1 - dy, color, a);
                }
            }
        }
    }

    /// Rounded rectangle filled with `color` at `alpha` over what is there.
    pub fn blend_round_rect(&mut self, r: Rect, radius: i32, color: u32, alpha: u32) {
        self.fill_round_rect_corners(r, radius, color, ALL_CORNERS, alpha);
    }

    /// Outline of a rounded rectangle, `width` px wide, anti-aliased (focus rings, borders).
    pub fn stroke_round_rect_w(&mut self, r: Rect, radius: i32, width: i32, color: u32) {
        let radius = radius.min(r.w / 2).min(r.h / 2).max(0);
        let w = width.max(1);
        let ir = (radius - w).max(0); // the inner rectangle's corner radius
        // Straight edges.
        self.fill_rect(Rect::new(r.x + radius, r.y, r.w - 2 * radius, w), color);
        self.fill_rect(Rect::new(r.x + radius, r.y + r.h - w, r.w - 2 * radius, w), color);
        self.fill_rect(Rect::new(r.x, r.y + radius, w, r.h - 2 * radius), color);
        self.fill_rect(Rect::new(r.x + r.w - w, r.y + radius, w, r.h - 2 * radius), color);
        // Corners: outer coverage minus inner coverage.
        for dy in 0..radius {
            for dx in 0..radius {
                let outer = corner_coverage(dx, dy, radius);
                let inner = if dx >= w && dy >= w { corner_coverage(dx - w, dy - w, ir) } else { 0 };
                let a = outer.saturating_sub(inner);
                if a == 0 {
                    continue;
                }
                self.blend_px(r.x + dx, r.y + dy, color, a);
                self.blend_px(r.x + r.w - 1 - dx, r.y + dy, color, a);
                self.blend_px(r.x + dx, r.y + r.h - 1 - dy, color, a);
                self.blend_px(r.x + r.w - 1 - dx, r.y + r.h - 1 - dy, color, a);
            }
        }
    }

    /// Outline of a rounded rectangle, 2 px wide.
    pub fn stroke_round_rect(&mut self, r: Rect, radius: i32, color: u32) {
        self.stroke_round_rect_w(r, radius, 2, color);
    }

    pub fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: u32) {
        let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
        let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
        let (mut x, mut y, mut err) = (x0, y0, dx + dy);
        loop {
            self.set(x, y, color);
            if x == x1 && y == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Disc of radius `r` around the pixel (cx, cy), anti-aliased.
    pub fn fill_circle(&mut self, cx: i32, cy: i32, r: i32, color: u32) {
        if r <= 0 {
            return self.set(cx, cy, color);
        }
        for dy in -r - 1..=r + 1 {
            // Solid span, then the edge pixels on each side by coverage.
            let solid = (r - 1) * (r - 1) >= dy * dy;
            let inner = isqrt(((r - 1) * (r - 1) - dy * dy).max(0));
            let outer = isqrt(((r + 1) * (r + 1) - dy * dy).max(0)) + 1;
            if solid {
                self.fill_rect(Rect::new(cx - inner, cy + dy, 2 * inner + 1, 1), color);
            }
            for dx in if solid { inner + 1 } else { 0 }..=outer {
                let a = disc_coverage(dx, dy, r);
                if a > 0 {
                    self.blend_px(cx + dx, cy + dy, color, a);
                    if dx > 0 {
                        self.blend_px(cx - dx, cy + dy, color, a);
                    }
                }
            }
        }
    }

    pub fn stroke_circle(&mut self, cx: i32, cy: i32, r: i32, color: u32) {
        let (mut x, mut y, mut err) = (r, 0, 1 - r);
        while x >= y {
            for (px, py) in [(x, y), (y, x), (-y, x), (-x, y), (-x, -y), (-y, -x), (y, -x), (x, -y)] {
                self.set(cx + px, cy + py, color);
            }
            y += 1;
            if err < 0 {
                err += 2 * y + 1;
            } else {
                x -= 1;
                err += 2 * (y - x) + 1;
            }
        }
    }

    /// Draw one glyph at `scale`; `bg` None = transparent background.
    pub fn glyph(&mut self, x: i32, y: i32, glyph: u8, fg: u32, bg: Option<u32>, scale: i32) {
        let bits = &FONT[glyph as usize * FONT_HEIGHT..][..FONT_HEIGHT];
        for (dy, &row) in bits.iter().enumerate() {
            for dx in 0..FONT_WIDTH {
                let on = row & (0x80 >> dx) != 0;
                let color = if on { Some(fg) } else { bg };
                if let Some(c) = color {
                    let (px, py) = (x + dx as i32 * scale, y + dy as i32 * scale);
                    if scale == 1 {
                        self.set(px, py, c);
                    } else {
                        self.fill_rect(Rect::new(px, py, scale, scale), c);
                    }
                }
            }
        }
    }

    /// Draw text (UTF-8, shown with the closest CP437 glyphs). Returns the end x.
    pub fn text(&mut self, x: i32, y: i32, s: &str, color: u32) -> i32 {
        self.text_scaled(x, y, s, color, 1)
    }

    pub fn text_scaled(&mut self, x: i32, y: i32, s: &str, color: u32, scale: i32) -> i32 {
        let mut cx = x;
        for c in s.chars() {
            if c == '\n' {
                continue;
            }
            self.glyph(cx, y, textgrid::glyph(c), color, None, scale);
            cx += FONT_WIDTH as i32 * scale;
        }
        cx
    }

    /// Text clipped to `max_w` pixels, with "..." if cut.
    pub fn text_clipped(&mut self, x: i32, y: i32, s: &str, color: u32, max_w: i32) {
        let max_chars = (max_w / FONT_WIDTH as i32).max(0) as usize;
        let count = s.chars().count();
        if count <= max_chars {
            self.text(x, y, s, color);
        } else if max_chars > 3 {
            let cut: alloc::string::String = s.chars().take(max_chars - 3).chain("...".chars()).collect();
            self.text(x, y, &cut, color);
        }
    }

    /// Copy `src` (or its `src_rect` part) to (dx, dy).
    pub fn blit(&mut self, src: &Canvas, src_rect: Rect, dx: i32, dy: i32) {
        let Some(sr) = src_rect.intersect(&src.bounds()) else { return };
        let dest = Rect::new(dx + (sr.x - src_rect.x), dy + (sr.y - src_rect.y), sr.w, sr.h);
        let Some(d) = dest.intersect(&self.bounds()) else { return };
        let (ox, oy) = (sr.x + (d.x - dest.x), sr.y + (d.y - dest.y));
        for row in 0..d.h {
            let s0 = (oy + row) as usize * src.width + ox as usize;
            let d0 = (d.y + row) as usize * self.width + d.x as usize;
            self.pixels[d0..d0 + d.w as usize].copy_from_slice(&src.pixels[s0..s0 + d.w as usize]);
        }
    }

    /// Draw `src` scaled into `dst`: a box filter (up to 4x4 samples per pixel) when
    /// shrinking, nearest pixel when enlarging. `radius` > 0 rounds the corners
    /// (anti-aliased); `alpha` (0..=255) fades it in.
    pub fn blit_scaled(&mut self, src: &Canvas, dst: Rect, radius: i32, alpha: u32) {
        self.blit_scaled_corners(src, dst, radius, ALL_CORNERS, alpha);
    }

    /// [`blit_scaled`] with only the corners in `which` rounded (see [`TOP_CORNERS`]).
    pub fn blit_scaled_corners(&mut self, src: &Canvas, dst: Rect, radius: i32, which: u8, alpha: u32) {
        if dst.w <= 0 || dst.h <= 0 || src.width == 0 || src.height == 0 || alpha == 0 {
            return;
        }
        let Some(vis) = dst.intersect(&self.bounds()) else { return };
        let radius = radius.min(dst.w / 2).min(dst.h / 2).max(0);
        let cols = Samples::table(src.width, dst.w, vis.x - dst.x, vis.w);
        let rows = Samples::table(src.height, dst.h, vis.y - dst.y, vis.h);
        for (row, y) in rows.iter().zip(vis.y..) {
            let ry = y - dst.y;
            for (col, x) in cols.iter().zip(vis.x..) {
                let a = alpha * corner_alpha(ry, x - dst.x, dst, radius, which) / 255;
                if a == 0 {
                    continue;
                }
                let p = if row.n == 1 && col.n == 1 {
                    src.pixels[row.at[0] * src.width + col.at[0]]
                } else {
                    let (mut r, mut g, mut b) = (0u32, 0u32, 0u32);
                    for sy in &row.at[..row.n] {
                        for sx in &col.at[..col.n] {
                            let p = src.pixels[sy * src.width + sx];
                            r += (p >> 16) & 0xFF;
                            g += (p >> 8) & 0xFF;
                            b += p & 0xFF;
                        }
                    }
                    let n = (row.n * col.n) as u32;
                    ((r / n) << 16) | ((g / n) << 8) | (b / n)
                };
                let i = y as usize * self.width + x as usize;
                self.pixels[i] = if a >= 255 { p } else { blend(self.pixels[i], p, a) };
            }
        }
    }

    /// Draw a mask image (white = opaque, black = transparent, e.g. a toolbar glyph)
    /// scaled into `dst`, tinted `color`. Shrinking box-filters up to 4x4 samples per pixel,
    /// so glyph edges stay smooth at 16-24 px.
    pub fn draw_mask(&mut self, mask: &Canvas, dst: Rect, color: u32) {
        let Some(vis) = dst.intersect(&self.bounds()) else { return };
        if mask.width == 0 || mask.height == 0 || dst.w <= 0 || dst.h <= 0 {
            return;
        }
        let cols = Samples::table(mask.width, dst.w, vis.x - dst.x, vis.w);
        let rows = Samples::table(mask.height, dst.h, vis.y - dst.y, vis.h);
        for (row, y) in rows.iter().zip(vis.y..) {
            for (col, x) in cols.iter().zip(vis.x..) {
                let mut a = 0;
                for sy in &row.at[..row.n] {
                    for sx in &col.at[..col.n] {
                        a += (mask.pixels[sy * mask.width + sx] >> 8) & 0xFF;
                    }
                }
                let a = a / (row.n * col.n) as u32;
                if a > 4 {
                    self.blend_px(x, y, color, a);
                }
            }
        }
    }

    /// Vertical gradient from `top` to `bottom`.
    pub fn gradient(&mut self, r: Rect, top: u32, bottom: u32) {
        for dy in 0..r.h {
            let t = if r.h > 1 { (dy * 255 / (r.h - 1)) as u32 } else { 0 };
            self.fill_rect(Rect::new(r.x, r.y + dy, r.w, 1), blend(top, bottom, t));
        }
    }

    /// Soft drop shadow around `r` (drawn before the window itself): `size` px wide.
    pub fn shadow(&mut self, r: Rect, size: i32) {
        self.shadow_round(r, 0, size, 90);
    }

    /// A shadow for a rounded shape: ambient all around plus a key light from above
    /// (the shadow sits a little lower than the shape), fading quadratically from
    /// `strength` (0..=255) at the edge to nothing `size` px out, like the two-layer
    /// shadows of desktop toolkits.
    pub fn shadow_round(&mut self, r: Rect, radius: i32, size: i32, strength: u32) {
        if size <= 0 {
            return;
        }
        let drop = (size / 3).max(1);
        for i in 1..=size {
            let t = (size - i + 1) as u32;
            let alpha = strength * t * t / (size as u32 * size as u32);
            if alpha == 0 {
                continue;
            }
            self.ring(Rect::new(r.x - i, r.y - i + drop, r.w + 2 * i, r.h + 2 * i), radius + i, alpha);
        }
    }

    /// One pixel wide black ring along the edge of a rounded rectangle, at `alpha`.
    fn ring(&mut self, r: Rect, radius: i32, alpha: u32) {
        let radius = radius.min(r.w / 2).min(r.h / 2).max(0);
        let inset = |dy: i32| -> i32 {
            let d = if dy < radius { radius - dy } else if dy >= r.h - radius { dy - (r.h - radius) + 1 } else { 0 };
            if d > 0 { radius - isqrt((radius * radius - d * d).max(0)) } else { 0 }
        };
        for dy in 0..r.h {
            let y = r.y + dy;
            let a = inset(dy);
            if dy == 0 || dy == r.h - 1 {
                self.blend_rect(Rect::new(r.x + a, y, r.w - 2 * a, 1), 0, alpha);
                continue;
            }
            // The edge pixel, plus the step out to the next row's inset in the corners.
            let outer = if dy < r.h / 2 { inset(dy - 1) } else { inset(dy + 1) };
            let run = (outer - a).max(1);
            self.blend_rect(Rect::new(r.x + a, y, run, 1), 0, alpha);
            self.blend_rect(Rect::new(r.x + r.w - a - run, y, run, 1), 0, alpha);
        }
    }

    /// Copy `src` to (dx, dy) with the corners in `which` rounded by `radius`: what was
    /// underneath shows through outside the arcs (window contents in a rounded frame).
    pub fn blit_corners(&mut self, src: &Canvas, src_rect: Rect, dx: i32, dy: i32, radius: i32, which: u8) {
        let Some(sr) = src_rect.intersect(&src.bounds()) else { return };
        let dst = Rect::new(dx, dy, sr.w, sr.h);
        let radius = radius.min(dst.w / 2).min(dst.h / 2).max(0);
        // Keep the corner squares, copy everything, then re-blend the corners by coverage.
        let mut kept: Vec<(i32, i32, u32)> = Vec::new();
        if radius > 0 {
            for cy in 0..radius {
                for cx in 0..radius {
                    for (x, y) in [(dst.x + cx, dst.y + cy), (dst.x + dst.w - 1 - cx, dst.y + cy), (dst.x + cx, dst.y + dst.h - 1 - cy), (dst.x + dst.w - 1 - cx, dst.y + dst.h - 1 - cy)] {
                        kept.push((x, y, self.get(x, y)));
                    }
                }
            }
        }
        self.blit(src, sr, dx, dy);
        for (x, y, under) in kept {
            let a = corner_alpha(y - dst.y, x - dst.x, dst, radius, which);
            if a < 255 {
                let over = self.get(x, y);
                self.set(x, y, blend(under, over, a));
            }
        }
    }

    /// Encode as a PNG (stored/uncompressed deflate): small code, valid file.
    pub fn to_png(&self) -> Vec<u8> {
        fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(&(data.len() as u32).to_be_bytes());
            let start = out.len();
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            let crc = crate::storage::crc32(&out[start..]);
            out.extend_from_slice(&crc.to_be_bytes());
        }
        let mut raw = Vec::with_capacity((self.width * 3 + 1) * self.height);
        for y in 0..self.height {
            raw.push(0); // filter: none
            for &p in &self.pixels[y * self.width..(y + 1) * self.width] {
                raw.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
            }
        }
        // zlib stream with stored deflate blocks of at most 65535 bytes.
        let mut z = vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
        for (i, block) in blocks.iter().enumerate() {
            z.push(if i + 1 == blocks.len() { 1 } else { 0 });
            let len = block.len() as u16;
            z.extend_from_slice(&len.to_le_bytes());
            z.extend_from_slice(&(!len).to_le_bytes());
            z.extend_from_slice(block);
        }
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &raw {
            a = (a + byte as u32) % 65521;
            b = (b + a) % 65521;
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());

        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&(self.width as u32).to_be_bytes());
        ihdr.extend_from_slice(&(self.height as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit RGB
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &z);
        chunk(&mut out, b"IEND", &[]);
        out
    }
}

/// Corner flags for [`Canvas::fill_round_rect_corners`] and [`Canvas::blit_scaled_corners`].
pub const TOP_LEFT: u8 = 1;
pub const TOP_RIGHT: u8 = 2;
pub const BOTTOM_LEFT: u8 = 4;
pub const BOTTOM_RIGHT: u8 = 8;
pub const TOP_CORNERS: u8 = TOP_LEFT | TOP_RIGHT;
pub const BOTTOM_CORNERS: u8 = BOTTOM_LEFT | BOTTOM_RIGHT;
pub const ALL_CORNERS: u8 = TOP_CORNERS | BOTTOM_CORNERS;

/// Coverage (0..=255) of the pixel `dx`, `dy` from the corner of a rounded rectangle
/// with corner `radius`: 255 inside the arc, 0 outside, in between on the edge.
pub fn corner_coverage(dx: i32, dy: i32, radius: i32) -> u32 {
    if radius <= 0 || dx >= radius || dy >= radius {
        return 255;
    }
    // Distance from the pixel centre to the arc centre, in half pixels.
    let (hx, hy) = (2 * (radius - dx) - 1, 2 * (radius - dy) - 1);
    let d16 = isqrt(64 * (hx * hx + hy * hy)); // 16 * distance in pixels
    (16 * radius + 8 - d16).clamp(0, 16) as u32 * 255 / 16
}

/// Coverage of the pixel at offset (dx, dy) from the centre of a disc of radius `r`.
fn disc_coverage(dx: i32, dy: i32, r: i32) -> u32 {
    let d16 = isqrt(256 * (dx * dx + dy * dy));
    (16 * r + 8 - d16).clamp(0, 16) as u32 * 255 / 16
}

/// Corner alpha for the pixel (`rx`, `ry`) inside `dst` with the given corners rounded.
fn corner_alpha(ry: i32, rx: i32, dst: Rect, radius: i32, which: u8) -> u32 {
    if radius <= 0 {
        return 255;
    }
    let (left, right) = (rx < radius, rx >= dst.w - radius);
    let (top, bottom) = (ry < radius, ry >= dst.h - radius);
    let corner = match (left, right, top, bottom) {
        (true, _, true, _) => TOP_LEFT,
        (_, true, true, _) => TOP_RIGHT,
        (true, _, _, true) => BOTTOM_LEFT,
        (_, true, _, true) => BOTTOM_RIGHT,
        _ => return 255,
    };
    if which & corner == 0 {
        return 255;
    }
    let dx = if left { rx } else { dst.w - 1 - rx };
    let dy = if top { ry } else { dst.h - 1 - ry };
    corner_coverage(dx, dy, radius)
}

/// Source sample positions for one destination pixel when scaling (at most 4 per axis).
struct Samples {
    at: [usize; 4],
    n: usize,
}

impl Samples {
    /// The table for destination pixels `from..from + count` of a `dst_len` wide (or tall)
    /// target drawn from `src_len` source pixels.
    fn table(src_len: usize, dst_len: i32, from: i32, count: i32) -> Vec<Samples> {
        let (s, d) = (src_len as i64, dst_len as i64);
        (from..from + count)
            .map(|p| {
                let p = p as i64;
                let s0 = (p * s / d).min(s - 1);
                let s1 = ((p + 1) * s / d).clamp(s0 + 1, s);
                let span = s1 - s0;
                let n = span.min(4) as usize;
                let mut at = [0usize; 4];
                for (i, slot) in at.iter_mut().take(n).enumerate() {
                    // Spread the samples evenly over the footprint.
                    *slot = (s0 + (2 * i as i64 + 1) * span / (2 * n as i64)).min(s - 1) as usize;
                }
                Samples { at, n }
            })
            .collect()
    }
}

pub fn isqrt(n: i32) -> i32 {
    if n <= 0 {
        return 0;
    }
    let mut x = n;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + n / x) / 2;
    }
    x
}

/// Base64 (standard alphabet, padded), for sending PNGs to the API.
pub fn base64(data: &[u8]) -> alloc::string::String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = alloc::string::String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

impl Canvas {
    /// Decode a PNG (8-bit grey, grey+alpha, RGB, RGBA or palette; non-interlaced).
    pub fn from_png(data: &[u8]) -> Result<Canvas, &'static str> {
        if data.len() < 8 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
            return Err("not a PNG file");
        }
        let (mut w, mut h, mut depth, mut color, mut interlace) = (0usize, 0usize, 0u8, 0u8, 0u8);
        let mut idat = Vec::new();
        let mut palette: Vec<u32> = Vec::new();
        let mut p = 8;
        while p + 12 <= data.len() {
            let len = u32::from_be_bytes(data[p..p + 4].try_into().unwrap()) as usize;
            let kind = &data[p + 4..p + 8];
            let body = data.get(p + 8..p + 8 + len).ok_or("truncated PNG")?;
            match kind {
                b"IHDR" => {
                    w = u32::from_be_bytes(body[0..4].try_into().unwrap()) as usize;
                    h = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
                    depth = body[8];
                    color = body[9];
                    interlace = body[12];
                }
                b"PLTE" => palette = body.chunks_exact(3).map(|c| (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32).collect(),
                b"IDAT" => idat.extend_from_slice(body),
                b"IEND" => break,
                _ => {}
            }
            p += 12 + len;
        }
        if depth != 8 || interlace != 0 || w == 0 || h == 0 || w > 8192 || h > 16384 {
            return Err("unsupported PNG (need 8-bit, non-interlaced)");
        }
        let bpp = match color {
            0 => 1,
            2 => 3,
            3 => 1,
            4 => 2,
            6 => 4,
            _ => return Err("unsupported PNG colour type"),
        };
        let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&idat).map_err(|_| "corrupt PNG data")?;
        let stride = w * bpp;
        if raw.len() < h * (stride + 1) {
            return Err("truncated PNG data");
        }
        let mut out = Canvas::new(w, h);
        let mut prev = vec![0u8; stride];
        let mut cur = vec![0u8; stride];
        for y in 0..h {
            let line = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
            let filter = line[0];
            for x in 0..stride {
                let a = if x >= bpp { cur[x - bpp] as i32 } else { 0 };
                let b = prev[x] as i32;
                let c = if x >= bpp { prev[x - bpp] as i32 } else { 0 };
                let pred = match filter {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => (a + b) / 2,
                    4 => {
                        let pp = a + b - c;
                        let (pa, pb, pc) = ((pp - a).abs(), (pp - b).abs(), (pp - c).abs());
                        if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
                    }
                    _ => return Err("bad PNG filter"),
                };
                cur[x] = line[1 + x].wrapping_add(pred as u8);
            }
            for x in 0..w {
                let px = &cur[x * bpp..x * bpp + bpp];
                out.pixels[y * w + x] = match color {
                    0 | 4 => (px[0] as u32) * 0x010101,
                    3 => *palette.get(px[0] as usize).unwrap_or(&0),
                    _ => (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32,
                };
            }
            core::mem::swap(&mut prev, &mut cur);
        }
        Ok(out)
    }
}
