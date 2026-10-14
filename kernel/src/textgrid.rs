//! A terminal's character grid: ANSI escape handling, UTF-8 decoding, scrolling.
//!
//! Used by the framebuffer console and by terminal windows on the desktop. The
//! grid understands the ANSI subset Nova emits, so one byte stream looks right on
//! the screen, in a desktop terminal and on a serial terminal:
//! `\n`, `\r`, `\t`, backspace, `ESC[...m` (16 colors, bold), `ESC[2J`, `ESC[H`,
//! `ESC[K`, `ESC[2K`. Text is UTF-8; each character is shown with the closest
//! glyph of the code page 437 font.

use alloc::vec;
use alloc::vec::Vec;

pub const FONT_WIDTH: usize = 8;
pub const FONT_HEIGHT: usize = 16;
/// Spleen 8x16 (BSD-2-Clause, see font/LICENSE.spleen): 256 glyphs in code page 437
/// order, 16 bytes per glyph, one byte per row, most significant bit leftmost.
pub static FONT: &[u8; 256 * FONT_HEIGHT] = include_bytes!("font/spleen-8x16.bin");

/// Unicode character shown by each CP437 glyph of the font, indexed by glyph number.
#[rustfmt::skip]
const CP437: [char; 256] = ['\u{0000}', '\u{263a}', '\u{263b}', '\u{2665}', '\u{2666}', '\u{2663}', '\u{2660}', '\u{2022}', '\u{25d8}', '\u{25cb}', '\u{25d9}', '\u{2642}', '\u{2640}', '\u{266a}', '\u{266b}', '\u{263c}', '\u{25ba}', '\u{25c4}', '\u{2195}', '\u{203c}', '\u{00b6}', '\u{00a7}', '\u{25ac}', '\u{21a8}', '\u{2191}', '\u{2193}', '\u{2192}', '\u{2190}', '\u{221f}', '\u{2194}', '\u{25b2}', '\u{25bc}', '\u{0020}', '\u{0021}', '\u{0022}', '\u{0023}', '\u{0024}', '\u{0025}', '\u{0026}', '\u{0027}', '\u{0028}', '\u{0029}', '\u{002a}', '\u{002b}', '\u{002c}', '\u{002d}', '\u{002e}', '\u{002f}', '\u{0030}', '\u{0031}', '\u{0032}', '\u{0033}', '\u{0034}', '\u{0035}', '\u{0036}', '\u{0037}', '\u{0038}', '\u{0039}', '\u{003a}', '\u{003b}', '\u{003c}', '\u{003d}', '\u{003e}', '\u{003f}', '\u{0040}', '\u{0041}', '\u{0042}', '\u{0043}', '\u{0044}', '\u{0045}', '\u{0046}', '\u{0047}', '\u{0048}', '\u{0049}', '\u{004a}', '\u{004b}', '\u{004c}', '\u{004d}', '\u{004e}', '\u{004f}', '\u{0050}', '\u{0051}', '\u{0052}', '\u{0053}', '\u{0054}', '\u{0055}', '\u{0056}', '\u{0057}', '\u{0058}', '\u{0059}', '\u{005a}', '\u{005b}', '\u{005c}', '\u{005d}', '\u{005e}', '\u{005f}', '\u{0060}', '\u{0061}', '\u{0062}', '\u{0063}', '\u{0064}', '\u{0065}', '\u{0066}', '\u{0067}', '\u{0068}', '\u{0069}', '\u{006a}', '\u{006b}', '\u{006c}', '\u{006d}', '\u{006e}', '\u{006f}', '\u{0070}', '\u{0071}', '\u{0072}', '\u{0073}', '\u{0074}', '\u{0075}', '\u{0076}', '\u{0077}', '\u{0078}', '\u{0079}', '\u{007a}', '\u{007b}', '\u{007c}', '\u{007d}', '\u{007e}', '\u{2302}', '\u{00c7}', '\u{00fc}', '\u{00e9}', '\u{00e2}', '\u{00e4}', '\u{00e0}', '\u{00e5}', '\u{00e7}', '\u{00ea}', '\u{00eb}', '\u{00e8}', '\u{00ef}', '\u{00ee}', '\u{00ec}', '\u{00c4}', '\u{00c5}', '\u{00c9}', '\u{00e6}', '\u{00c6}', '\u{00f4}', '\u{00f6}', '\u{00f2}', '\u{00fb}', '\u{00f9}', '\u{00ff}', '\u{00d6}', '\u{00dc}', '\u{00a2}', '\u{00a3}', '\u{00a5}', '\u{20a7}', '\u{0192}', '\u{00e1}', '\u{00ed}', '\u{00f3}', '\u{00fa}', '\u{00f1}', '\u{00d1}', '\u{00aa}', '\u{00ba}', '\u{00bf}', '\u{2310}', '\u{00ac}', '\u{00bd}', '\u{00bc}', '\u{00a1}', '\u{00ab}', '\u{00bb}', '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}', '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255d}', '\u{255c}', '\u{255b}', '\u{2510}', '\u{2514}', '\u{2534}', '\u{252c}', '\u{251c}', '\u{2500}', '\u{253c}', '\u{255e}', '\u{255f}', '\u{255a}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256c}', '\u{2567}', '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256b}', '\u{256a}', '\u{2518}', '\u{250c}', '\u{2588}', '\u{2584}', '\u{258c}', '\u{2590}', '\u{2580}', '\u{03b1}', '\u{00df}', '\u{0393}', '\u{03c0}', '\u{03a3}', '\u{03c3}', '\u{00b5}', '\u{03c4}', '\u{03a6}', '\u{0398}', '\u{03a9}', '\u{03b4}', '\u{221e}', '\u{03c6}', '\u{03b5}', '\u{2229}', '\u{2261}', '\u{00b1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}', '\u{00f7}', '\u{2248}', '\u{00b0}', '\u{2219}', '\u{00b7}', '\u{221a}', '\u{207f}', '\u{00b2}', '\u{25a0}', '\u{00a0}'];
/// VGA-style 16-color palette as 0xRRGGBB, indexed by ANSI color number (8-15 = bright).
pub const PALETTE: [u32; 16] = [
    // Nova's terminal theme: deep ink background, soft pastels tuned to the desktop.
    0x14151F, 0xF0717E, 0x8FD694, 0xF2C46D, 0x7AA8FF, 0xB89CFF, 0x5FD3D0, 0xD5D7E5,
    0x6A6E85, 0xFF8B95, 0xA9F0AD, 0xFFD991, 0x9CC0FF, 0xCDB8FF, 0x86E8E4, 0xFFFFFF,
];
pub const DEFAULT_FG: u8 = 7;
pub const DEFAULT_BG: u8 = 0;

/// The Unicode character a cell's CP437 byte stands for (for TrueType rendering).
pub fn char_of(b: u8) -> char {
    CP437[b as usize]
}

/// The font glyph for a character: exact CP437 match, else a look-alike, else '?'.
pub fn glyph(c: char) -> u8 {
    if (' '..='~').contains(&c) {
        return c as u8;
    }
    if let Some(i) = CP437.iter().skip(1).position(|&g| g == c) {
        return i as u8 + 1;
    }
    let similar = match c {
        '\u{2018}' | '\u{2019}' | '\u{201b}' | '\u{2032}' => '\'',
        '\u{201c}' | '\u{201d}' | '\u{2033}' => '"',
        '\u{2013}' | '\u{2014}' | '\u{2212}' | '\u{2010}' | '\u{2011}' => '-',
        '\u{2026}' => '.',
        '\u{2715}' | '\u{2717}' | '\u{2718}' | '\u{00d7}' => 'x',
        '\u{273b}' | '\u{2731}' | '\u{2733}' | '\u{2736}' => '*',
        '\u{23fa}' | '\u{25cf}' | '\u{26ab}' | '\u{2b24}' => '\u{2022}',
        '\u{23bf}' | '\u{2570}' => '\u{2514}',
        '\u{256d}' => '\u{250c}',
        '\u{256e}' => '\u{2510}',
        '\u{256f}' => '\u{2518}',
        '\u{2713}' | '\u{2714}' => '\u{221a}',
        '\u{276f}' | '\u{203a}' | '\u{25b8}' => '>',
        '\u{2039}' => '<',
        '\u{00a0}' | '\u{2009}' | '\u{200a}' | '\u{202f}' => ' ',
        '\t' => ' ',
        _ => '?',
    };
    if similar.is_ascii() { similar as u8 } else { CP437.iter().position(|&g| g == similar).unwrap_or(b'?' as usize) as u8 }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cell {
    pub ch: u8,
    pub fg: u8,
    pub bg: u8,
}

pub const BLANK: Cell = Cell { ch: b' ', fg: DEFAULT_FG, bg: DEFAULT_BG };

#[derive(Clone, Copy)]
enum Parser {
    Normal,
    Escape,
    /// Inside `ESC [`: up to 4 numeric parameters.
    Csi { params: [u16; 4], count: usize },
}

pub struct TextGrid {
    pub cols: usize,
    pub rows: usize,
    pub col: usize,
    pub row: usize,
    fg: u8,
    bg: u8,
    bold: bool,
    parser: Parser,
    /// Partial UTF-8 sequence: (code point so far, continuation bytes still expected).
    utf8: (u32, u8),
    pub cells: Vec<Cell>,
    /// Lines that scrolled off the top (oldest first), at most SCROLLBACK.
    pub history: alloc::collections::VecDeque<Vec<Cell>>,
    /// How many lines the view is scrolled back (0 = live).
    pub view_offset: usize,
    /// Set by every change; renderers clear it.
    pub dirty: bool,
}

/// Lines of scrollback kept per terminal.
pub const SCROLLBACK: usize = 2000;

impl TextGrid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Self { cols, rows, col: 0, row: 0, fg: DEFAULT_FG, bg: DEFAULT_BG, bold: false, parser: Parser::Normal, utf8: (0, 0), cells: vec![BLANK; cols * rows], history: alloc::collections::VecDeque::new(), view_offset: 0, dirty: true }
    }

    /// Change the size, keeping the bottom-most text.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        let mut cells = vec![BLANK; cols * rows];
        let skip = (self.row + 1).saturating_sub(rows);
        for r in 0..rows.min(self.rows - skip) {
            for c in 0..cols.min(self.cols) {
                cells[r * cols + c] = self.cells[(r + skip) * self.cols + c];
            }
        }
        self.row = (self.row - skip).min(rows - 1);
        self.col = self.col.min(cols);
        self.cols = cols;
        self.rows = rows;
        self.cells = cells;
        self.dirty = true;
    }

    /// Scroll the view back (positive) or forward (negative) through the scrollback.
    pub fn scroll_view(&mut self, lines: i32) {
        let max = self.history.len() as i32;
        let new = (self.view_offset as i32 + lines).clamp(0, max) as usize;
        if new != self.view_offset {
            self.view_offset = new;
            self.dirty = true;
        }
    }

    /// The cells of screen row `r` as currently viewed (scrollback or live).
    pub fn view_row(&self, r: usize) -> &[Cell] {
        let back = self.view_offset.min(self.history.len());
        if r < back {
            let line = &self.history[self.history.len() - back + r];
            &line[..line.len().min(self.cols)]
        } else {
            let lr = r - back;
            &self.cells[lr * self.cols..(lr + 1) * self.cols]
        }
    }

    /// Index of the cursor cell.
    pub fn cursor_index(&self) -> usize {
        self.row * self.cols + self.col.min(self.cols - 1)
    }

    fn blank(&self) -> Cell {
        Cell { bg: self.bg, ..BLANK }
    }

    fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
            return;
        }
        let (cols, len) = (self.cols, self.rows * self.cols);
        if self.history.len() >= SCROLLBACK {
            self.history.pop_front();
        }
        self.history.push_back(self.cells[..cols].to_vec());
        self.cells.copy_within(cols..len, 0);
        let blank = self.blank();
        self.cells[len - cols..len].fill(blank);
    }

    fn put(&mut self, ch: u8) {
        if self.col >= self.cols {
            self.newline(); // deferred wrap: only when a character needs the next line
        }
        let fg = if self.bold && self.fg < 8 { self.fg + 8 } else { self.fg };
        let i = self.row * self.cols + self.col;
        self.cells[i] = Cell { ch, fg, bg: self.bg };
        self.col += 1;
    }

    pub fn clear(&mut self) {
        let blank = self.blank();
        self.cells.fill(blank);
        self.row = 0;
        self.col = 0;
        self.dirty = true;
    }

    fn csi(&mut self, final_byte: u8, params: &[u16]) {
        match final_byte {
            b'm' => {
                for &p in if params.is_empty() { &[0][..] } else { params } {
                    match p {
                        0 => (self.fg, self.bg, self.bold) = (DEFAULT_FG, DEFAULT_BG, false),
                        1 => self.bold = true,
                        2 => self.fg = 8, // dim: shown as dark grey
                        22 => self.bold = false,
                        30..=37 => self.fg = (p - 30) as u8,
                        39 => self.fg = DEFAULT_FG,
                        40..=47 => self.bg = (p - 40) as u8,
                        49 => self.bg = DEFAULT_BG,
                        90..=97 => self.fg = (p - 90 + 8) as u8,
                        100..=107 => self.bg = (p - 100 + 8) as u8,
                        _ => {}
                    }
                }
            }
            b'J' if params == [2] => self.clear(),
            b'H' if params.is_empty() => (self.row, self.col) = (0, 0),
            b'K' => {
                let blank = self.blank();
                let line = self.row * self.cols;
                let start = if params == [2] { line } else { line + self.col.min(self.cols) };
                self.cells[start..line + self.cols].fill(blank);
            }
            _ => {}
        }
    }

    fn control_or_char(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\r' => self.col = 0,
            '\u{8}' => {
                // Backspace at the start of a wrapped line goes to the end of the previous one.
                if self.col == 0 && self.row > 0 {
                    self.row -= 1;
                    self.col = self.cols - 1;
                } else {
                    self.col = self.col.min(self.cols).saturating_sub(1);
                }
            }
            '\t' => {
                for _ in 0..8 - self.col % 8 {
                    self.put(b' ');
                }
            }
            c if c.is_control() => {}
            c => self.put(glyph(c)),
        }
    }

    pub fn write_byte(&mut self, byte: u8) {
        if self.view_offset != 0 {
            self.view_offset = 0;
        }
        self.dirty = true;
        // UTF-8 continuation of a multi-byte character.
        if self.utf8.1 > 0 {
            if byte & 0xC0 == 0x80 {
                self.utf8 = ((self.utf8.0 << 6) | (byte & 0x3F) as u32, self.utf8.1 - 1);
                if self.utf8.1 == 0 {
                    self.control_or_char(char::from_u32(self.utf8.0).unwrap_or('?'));
                }
                return;
            }
            self.utf8 = (0, 0);
            self.control_or_char('?');
        }
        self.parser = match (self.parser, byte) {
            (Parser::Normal, 0x1B) => Parser::Escape,
            (Parser::Normal, 0xC0..=0xDF) => {
                self.utf8 = ((byte & 0x1F) as u32, 1);
                Parser::Normal
            }
            (Parser::Normal, 0xE0..=0xEF) => {
                self.utf8 = ((byte & 0x0F) as u32, 2);
                Parser::Normal
            }
            (Parser::Normal, 0xF0..=0xF7) => {
                self.utf8 = ((byte & 0x07) as u32, 3);
                Parser::Normal
            }
            (Parser::Normal, b) => {
                self.control_or_char(if b < 0x80 { b as char } else { '?' });
                Parser::Normal
            }
            (Parser::Escape, b'[') => Parser::Csi { params: [0; 4], count: 0 },
            (Parser::Escape, _) => Parser::Normal,
            (Parser::Csi { mut params, mut count }, b'0'..=b'9') => {
                count = count.max(1);
                if count <= params.len() {
                    let p = &mut params[count - 1];
                    *p = p.saturating_mul(10).saturating_add((byte - b'0') as u16);
                }
                Parser::Csi { params, count }
            }
            (Parser::Csi { params, count }, b';') => Parser::Csi { params, count: (count.max(1) + 1).min(params.len() + 1) },
            (Parser::Csi { params, count }, 0x40..=0x7E) => {
                self.csi(byte, &params[..count.min(params.len())]);
                Parser::Normal
            }
            (Parser::Csi { .. }, _) => Parser::Normal,
        };
    }

    pub fn write_str(&mut self, s: &str) {
        s.bytes().for_each(|b| self.write_byte(b));
    }
}

impl core::fmt::Write for TextGrid {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        TextGrid::write_str(self, s);
        Ok(())
    }
}
