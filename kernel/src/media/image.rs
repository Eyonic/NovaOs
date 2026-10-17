//! Still-image decoding for the Image Viewer, Files' thumbnails and cover art: PNG
//! (gfx.rs), JPEG with its EXIF orientation applied (zune-jpeg, pure Rust), and GIF
//! including animations (a decoder of our own: LZW, local palettes, interlacing,
//! disposal). Other formats (WebP, BMP, TIFF, ...) come through the media engine's
//! `decode` command when it is installed.

use alloc::vec;
use alloc::vec::Vec;

use crate::gfx::{Canvas, Rect};

/// A decoded image: one or more frames, each shown for `delay_ms` (0 = still).
pub struct Image {
    pub frames: Vec<(Canvas, u32)>,
}

impl Image {
    pub fn still(c: Canvas) -> Self {
        Image { frames: vec![(c, 0)] }
    }
    pub fn first(&self) -> &Canvas {
        &self.frames[0].0
    }
    pub fn animated(&self) -> bool {
        self.frames.len() > 1
    }
}

/// The format of `data` by its signature: "png", "jpeg", "gif", "webp", or None.
pub fn format_of(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG") {
        Some("png")
    } else if data.starts_with(&[0xFF, 0xD8]) {
        Some("jpeg")
    } else if data.starts_with(b"GIF8") {
        Some("gif")
    } else if data.len() > 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("webp")
    } else {
        None
    }
}

/// True for file names the viewer opens (by extension).
pub fn is_image_name(name: &str) -> bool {
    matches!(name.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref(), Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "tif" | "tiff"))
}

/// Decode what this module can decode by itself.
pub fn decode(data: &[u8]) -> Result<Image, &'static str> {
    match format_of(data) {
        Some("png") => Canvas::from_png(data).map(Image::still),
        Some("jpeg") => decode_jpeg(data).map(Image::still),
        Some("gif") => decode_gif(data),
        Some("webp") => Err("WebP needs the media engine"),
        _ => Err("not an image format Nova knows"),
    }
}

// ----- JPEG -------------------------------------------------------------------------

fn decode_jpeg(data: &[u8]) -> Result<Canvas, &'static str> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;

    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB).set_strict_mode(false).set_max_width(8192).set_max_height(8192);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(data), opts);
    let pixels = dec.decode().map_err(|_| "the JPEG file could not be decoded")?;
    let info = dec.info().ok_or("the JPEG file has no header")?;
    let (w, h) = (info.width as usize, info.height as usize);
    if w == 0 || h == 0 || pixels.len() < w * h * 3 {
        return Err("the JPEG file is damaged");
    }
    let mut c = Canvas::new(w, h);
    for (p, px) in c.pixels.iter_mut().zip(pixels.chunks_exact(3)) {
        *p = (px[0] as u32) << 16 | (px[1] as u32) << 8 | px[2] as u32;
    }
    let orientation = dec.exif().and_then(|e| exif_orientation(e)).unwrap_or(1);
    Ok(orient(c, orientation))
}

/// The EXIF Orientation tag (1..=8) from a TIFF-structured EXIF block.
fn exif_orientation(exif: &[u8]) -> Option<u16> {
    let tiff = if exif.starts_with(b"Exif\0\0") { &exif[6..] } else { exif };
    let le = match tiff.get(0..2)? {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b = tiff.get(o..o + 2)?;
        Some(if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b = tiff.get(o..o + 4)?;
        Some(if le { u32::from_le_bytes([b[0], b[1], b[2], b[3]]) } else { u32::from_be_bytes([b[0], b[1], b[2], b[3]]) })
    };
    let ifd = u32_at(4)? as usize;
    let n = u16_at(ifd)? as usize;
    for i in 0..n.min(256) {
        let e = ifd + 2 + i * 12;
        if u16_at(e)? == 0x0112 {
            let v = u16_at(e + 8)?;
            return (1..=8).contains(&v).then_some(v);
        }
    }
    None
}

/// Apply an EXIF orientation (rotations and flips) so the picture is upright.
pub fn orient(c: Canvas, orientation: u16) -> Canvas {
    match orientation {
        2 => flip_h(&c),
        3 => rotate(&c, 2),
        4 => flip_v(&c),
        5 => flip_h(&rotate(&c, 1)),
        6 => rotate(&c, 1),
        7 => flip_h(&rotate(&c, 3)),
        8 => rotate(&c, 3),
        _ => c,
    }
}

/// Rotate by `quarter_turns` x 90 degrees clockwise.
pub fn rotate(c: &Canvas, quarter_turns: u32) -> Canvas {
    let (w, h) = (c.width, c.height);
    match quarter_turns % 4 {
        0 => c.clone(),
        1 => {
            let mut o = Canvas::new(h, w);
            for y in 0..h {
                for x in 0..w {
                    o.pixels[x * h + (h - 1 - y)] = c.pixels[y * w + x];
                }
            }
            o
        }
        2 => {
            let mut o = Canvas::new(w, h);
            for (d, s) in o.pixels.iter_mut().zip(c.pixels.iter().rev()) {
                *d = *s;
            }
            o
        }
        _ => {
            let mut o = Canvas::new(h, w);
            for y in 0..h {
                for x in 0..w {
                    o.pixels[(w - 1 - x) * h + y] = c.pixels[y * w + x];
                }
            }
            o
        }
    }
}

fn flip_h(c: &Canvas) -> Canvas {
    let mut o = c.clone();
    for row in o.pixels.chunks_exact_mut(c.width.max(1)) {
        row.reverse();
    }
    o
}

fn flip_v(c: &Canvas) -> Canvas {
    let mut o = Canvas::new(c.width, c.height);
    for y in 0..c.height {
        let src = &c.pixels[y * c.width..(y + 1) * c.width];
        let dy = c.height - 1 - y;
        o.pixels[dy * c.width..(dy + 1) * c.width].copy_from_slice(src);
    }
    o
}

// ----- GIF --------------------------------------------------------------------------

struct GifReader<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> GifReader<'a> {
    fn u8(&mut self) -> Result<u8, &'static str> {
        let v = *self.d.get(self.pos).ok_or("truncated GIF")?;
        self.pos += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, &'static str> {
        Ok(self.u8()? as u16 | (self.u8()? as u16) << 8)
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], &'static str> {
        let v = self.d.get(self.pos..self.pos + n).ok_or("truncated GIF")?;
        self.pos += n;
        Ok(v)
    }
    /// The concatenated data sub-blocks that follow.
    fn sub_blocks(&mut self) -> Result<Vec<u8>, &'static str> {
        let mut out = Vec::new();
        loop {
            let n = self.u8()? as usize;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(self.bytes(n)?);
        }
    }
    fn skip_sub_blocks(&mut self) -> Result<(), &'static str> {
        loop {
            let n = self.u8()? as usize;
            if n == 0 {
                return Ok(());
            }
            self.bytes(n)?;
        }
    }
}

fn palette(r: &mut GifReader, flags: u8, has: u8) -> Result<Vec<u32>, &'static str> {
    if flags & has == 0 {
        return Ok(Vec::new());
    }
    let n = 2usize << (flags & 7);
    Ok(r.bytes(n * 3)?.chunks_exact(3).map(|c| (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32).collect())
}

/// LZW decode `data` with the given minimum code size into `pixels` indices.
fn lzw(data: &[u8], min_code: u8, pixels: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels);
    let min_code = min_code.clamp(2, 11) as usize;
    let clear = 1usize << min_code;
    let end = clear + 1;
    // Dictionary as (prefix code, suffix byte); entries below `clear` are literals.
    let mut prefix: Vec<u16> = vec![0; 4096];
    let mut suffix: Vec<u8> = vec![0; 4096];
    let mut lengths: Vec<u16> = vec![0; 4096];
    for i in 0..clear {
        suffix[i] = i as u8;
        lengths[i] = 1;
    }
    let mut code_size = min_code + 1;
    let mut next = end + 1;
    let mut prev: Option<usize> = None;
    let (mut bits, mut nbits, mut i) = (0u32, 0u32, 0usize);
    let mut stack: Vec<u8> = Vec::with_capacity(4096);
    while out.len() < pixels {
        while nbits < code_size as u32 {
            if i >= data.len() {
                return out;
            }
            bits |= (data[i] as u32) << nbits;
            nbits += 8;
            i += 1;
        }
        let code = (bits & ((1 << code_size) - 1)) as usize;
        bits >>= code_size;
        nbits -= code_size as u32;
        if code == clear {
            code_size = min_code + 1;
            next = end + 1;
            prev = None;
            continue;
        }
        if code == end {
            break;
        }
        // The string for `code`, or prev + first(prev) for the not-yet-defined code.
        let (mut c, first_of_prev) = match prev {
            None => {
                if code >= next {
                    break;
                }
                (code, None)
            }
            Some(p) => {
                if code > next {
                    break;
                }
                if code == next {
                    (p, Some(p))
                } else {
                    (code, None)
                }
            }
        };
        stack.clear();
        // Walk the chain back to its first byte.
        while c >= clear {
            stack.push(suffix[c]);
            c = prefix[c] as usize;
        }
        let first = c as u8;
        if first_of_prev.is_some() {
            out.push(first);
        }
        out.push(first);
        for &b in stack.iter().rev() {
            out.push(b);
        }
        if let Some(p) = prev
            && next < 4096
        {
            prefix[next] = p as u16;
            suffix[next] = first;
            lengths[next] = lengths[p] + 1;
            next += 1;
            if next == (1 << code_size) && code_size < 12 {
                code_size += 1;
            }
        }
        prev = Some(code);
    }
    out.truncate(pixels);
    out
}

fn decode_gif(data: &[u8]) -> Result<Image, &'static str> {
    let mut r = GifReader { d: data, pos: 6 };
    let (sw, sh) = (r.u16()? as usize, r.u16()? as usize);
    let flags = r.u8()?;
    let bg_index = r.u8()?;
    r.u8()?; // aspect
    let global = palette(&mut r, flags, 0x80)?;
    if sw == 0 || sh == 0 || sw * sh > 64 << 20 {
        return Err("bad GIF size");
    }
    let bg = if flags & 0x80 != 0 { global.get(bg_index as usize).copied().unwrap_or(0) } else { 0 };
    let _ = bg;
    let mut frames: Vec<(Canvas, u32)> = Vec::new();
    // The compositing canvas and an alpha plane (transparent pixels show the viewer's
    // background, which the viewer draws as a checkerboard-free dark surface).
    let mut canvas = Canvas::new(sw, sh);
    let mut shown = vec![false; sw * sh];
    let (mut delay, mut transparent, mut disposal): (u32, Option<u8>, u8) = (0, None, 0);
    let mut saved: Option<(Vec<u32>, Vec<bool>)> = None;
    let mut last_rect = Rect::new(0, 0, 0, 0);
    loop {
        let block = match r.u8() {
            Ok(b) => b,
            Err(_) => break,
        };
        match block {
            0x3B => break,
            0x21 => {
                let label = r.u8()?;
                if label == 0xF9 {
                    let d = r.sub_blocks()?;
                    if d.len() >= 4 {
                        disposal = (d[0] >> 2) & 7;
                        delay = (d[1] as u32 | (d[2] as u32) << 8) * 10;
                        transparent = (d[0] & 1 != 0).then_some(d[3]);
                    }
                } else {
                    r.skip_sub_blocks()?;
                }
            }
            0x2C => {
                let (x, y, w, h) = (r.u16()? as usize, r.u16()? as usize, r.u16()? as usize, r.u16()? as usize);
                let iflags = r.u8()?;
                let local = palette(&mut r, iflags, 0x80)?;
                let pal = if local.is_empty() { &global } else { &local };
                let min_code = r.u8()?;
                let lz = r.sub_blocks()?;
                let idx = lzw(&lz, min_code, w * h);
                // Dispose of the previous frame first.
                if frames.len() < 1000 && !frames.is_empty() {
                    match last_disposal(&frames) {
                        2 => {
                            for yy in last_rect.y..last_rect.y + last_rect.h {
                                for xx in last_rect.x..last_rect.x + last_rect.w {
                                    if (0..sw as i32).contains(&xx) && (0..sh as i32).contains(&yy) {
                                        let i = yy as usize * sw + xx as usize;
                                        canvas.pixels[i] = 0;
                                        shown[i] = false;
                                    }
                                }
                            }
                        }
                        3 => {
                            if let Some((p, s)) = saved.take() {
                                canvas.pixels = p;
                                shown = s;
                            }
                        }
                        _ => {}
                    }
                }
                if disposal == 3 {
                    saved = Some((canvas.pixels.clone(), shown.clone()));
                }
                let interlaced = iflags & 0x40 != 0;
                let rows: Vec<usize> = if interlaced {
                    let mut v = Vec::with_capacity(h);
                    for (start, step) in [(0, 8), (4, 8), (2, 4), (1, 2)] {
                        v.extend((start..h).step_by(step));
                    }
                    v
                } else {
                    (0..h).collect()
                };
                for (src_row, &dst_row) in rows.iter().enumerate() {
                    let yy = y + dst_row;
                    if yy >= sh {
                        continue;
                    }
                    for xx in 0..w {
                        let Some(&ci) = idx.get(src_row * w + xx) else { continue };
                        if Some(ci) == transparent || x + xx >= sw {
                            continue;
                        }
                        let i = yy * sw + x + xx;
                        canvas.pixels[i] = pal.get(ci as usize).copied().unwrap_or(0);
                        shown[i] = true;
                    }
                }
                last_rect = Rect::new(x as i32, y as i32, w as i32, h as i32);
                let shown_delay = if delay == 0 && frames.is_empty() { 0 } else { delay.max(20) };
                frames.push((canvas.clone(), shown_delay | (disposal as u32) << 28));
                transparent = None;
                if frames.len() >= 400 {
                    break;
                }
            }
            _ => break,
        }
    }
    if frames.is_empty() {
        return Err("the GIF has no frames");
    }
    // Strip the disposal bits kept in the delay for the loop above.
    for f in frames.iter_mut() {
        f.1 &= 0x0FFF_FFFF;
    }
    if frames.len() == 1 {
        frames[0].1 = 0;
    }
    Ok(Image { frames })
}

fn last_disposal(frames: &[(Canvas, u32)]) -> u8 {
    frames.last().map_or(0, |f| (f.1 >> 28) as u8)
}
