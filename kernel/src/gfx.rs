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

    /// Rectangle with corners rounded by `radius`.
    pub fn fill_round_rect(&mut self, r: Rect, radius: i32, color: u32) {
        let radius = radius.min(r.w / 2).min(r.h / 2).max(0);
        for dy in 0..r.h {
            let inset = if dy < radius {
                let y = radius - dy;
                radius - isqrt(radius * radius - y * y)
            } else if dy >= r.h - radius {
                let y = dy - (r.h - radius) + 1;
                radius - isqrt((radius * radius - y * y).max(0))
            } else {
                0
            };
            self.fill_rect(Rect::new(r.x + inset, r.y + dy, r.w - 2 * inset, 1), color);
        }
    }

    /// Outline of a rounded rectangle, 2 px wide.
    pub fn stroke_round_rect(&mut self, r: Rect, radius: i32, color: u32) {
        for (dx, dy) in [(0, 0), (1, 1)] {
            let o = Rect::new(r.x + dx, r.y + dy, r.w - 2 * dx, r.h - 2 * dy);
            let rad = (radius - dx).max(0);
            self.fill_rect(Rect::new(o.x + rad, o.y, o.w - 2 * rad, 1), color);
            self.fill_rect(Rect::new(o.x + rad, o.y + o.h - 1, o.w - 2 * rad, 1), color);
            self.fill_rect(Rect::new(o.x, o.y + rad, 1, o.h - 2 * rad), color);
            self.fill_rect(Rect::new(o.x + o.w - 1, o.y + rad, 1, o.h - 2 * rad), color);
            // Corner arcs.
            for i in 0..=rad {
                let j = isqrt(rad * rad - i * i);
                for (cx, cy, sx, sy) in [(o.x + rad, o.y + rad, -1, -1), (o.x + o.w - 1 - rad, o.y + rad, 1, -1), (o.x + rad, o.y + o.h - 1 - rad, -1, 1), (o.x + o.w - 1 - rad, o.y + o.h - 1 - rad, 1, 1)] {
                    self.set(cx + sx * i, cy + sy * j, color);
                    self.set(cx + sx * j, cy + sy * i, color);
                }
            }
        }
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

    pub fn fill_circle(&mut self, cx: i32, cy: i32, r: i32, color: u32) {
        for dy in -r..=r {
            let w = isqrt(r * r - dy * dy);
            self.fill_rect(Rect::new(cx - w, cy + dy, 2 * w + 1, 1), color);
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

    /// Draw `src` scaled into `dst`: averaged 2x2 samples when shrinking, nearest pixel
    /// when enlarging. `radius` > 0 rounds the corners; `alpha` (0..=255) fades it in.
    pub fn blit_scaled(&mut self, src: &Canvas, dst: Rect, radius: i32, alpha: u32) {
        if dst.w <= 0 || dst.h <= 0 || src.width == 0 || src.height == 0 {
            return;
        }
        let Some(vis) = dst.intersect(&self.bounds()) else { return };
        let (sw, sh) = (src.width as i64, src.height as i64);
        let shrink = src.width as i32 > dst.w;
        for y in vis.y..vis.y + vis.h {
            let ry = y - dst.y;
            // Horizontal inset of the rounded corner on this row.
            let inset = if radius > 0 {
                let dy = if ry < radius { radius - ry } else if ry >= dst.h - radius { ry - (dst.h - radius - 1) } else { 0 };
                if dy > 0 { radius - isqrt(radius * radius - dy * dy) } else { 0 }
            } else {
                0
            };
            let sy0 = (ry as i64 * sh / dst.h as i64) as usize;
            let sy1 = (((ry as i64 * 2 + 1) * sh / (dst.h as i64 * 2)) as usize).min(src.height - 1);
            for x in vis.x.max(dst.x + inset)..(vis.x + vis.w).min(dst.x + dst.w - inset) {
                let rx = (x - dst.x) as i64;
                let sx0 = (rx * sw / dst.w as i64) as usize;
                let p = if shrink {
                    let sx1 = (((rx * 2 + 1) * sw / (dst.w as i64 * 2)) as usize).min(src.width - 1);
                    let px = [src.pixels[sy0 * src.width + sx0], src.pixels[sy0 * src.width + sx1], src.pixels[sy1 * src.width + sx0], src.pixels[sy1 * src.width + sx1]];
                    let ch = |s: u32| px.iter().map(|p| (p >> s) & 0xFF).sum::<u32>() / 4;
                    (ch(16) << 16) | (ch(8) << 8) | ch(0)
                } else {
                    src.pixels[sy0 * src.width + sx0]
                };
                let i = y as usize * self.width + x as usize;
                self.pixels[i] = if alpha >= 255 { p } else { blend(self.pixels[i], p, alpha) };
            }
        }
    }

    /// Draw a mask image (white = opaque, black = transparent, e.g. a toolbar glyph)
    /// scaled into `dst`, tinted `color`. Shrinking averages 2x2 samples for smooth edges.
    pub fn draw_mask(&mut self, mask: &Canvas, dst: Rect, color: u32) {
        let Some(vis) = dst.intersect(&self.bounds()) else { return };
        if mask.width == 0 || mask.height == 0 {
            return;
        }
        let (mw, mh) = (mask.width as i64, mask.height as i64);
        for y in vis.y..vis.y + vis.h {
            let ry = (y - dst.y) as i64;
            let sy0 = (ry * mh / dst.h as i64) as usize;
            let sy1 = (((ry * 2 + 1) * mh / (dst.h as i64 * 2)) as usize).min(mask.height - 1);
            for x in vis.x..vis.x + vis.w {
                let rx = (x - dst.x) as i64;
                let sx0 = (rx * mw / dst.w as i64) as usize;
                let sx1 = (((rx * 2 + 1) * mw / (dst.w as i64 * 2)) as usize).min(mask.width - 1);
                let a = [(sy0, sx0), (sy0, sx1), (sy1, sx0), (sy1, sx1)].iter().map(|&(yy, xx)| (mask.pixels[yy * mask.width + xx] >> 8) & 0xFF).sum::<u32>() / 4;
                if a > 8 {
                    let i = y as usize * self.width + x as usize;
                    self.pixels[i] = blend(self.pixels[i], color, a);
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

    /// Soft drop shadow around `r` (drawn before the window itself).
    pub fn shadow(&mut self, r: Rect, size: i32) {
        for i in 1..=size {
            let alpha = (60 * (size - i + 1) / size) as u32;
            self.blend_rect(Rect::new(r.x - i + 2, r.y + r.h + i - 1, r.w + 2 * i - 2, 1), 0, alpha);
            self.blend_rect(Rect::new(r.x + r.w + i - 1, r.y - i + 4, 1, r.h + 2 * i - 4), 0, alpha);
            self.blend_rect(Rect::new(r.x - i, r.y - i + 4, 1, r.h + 2 * i - 4), 0, alpha / 2);
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
