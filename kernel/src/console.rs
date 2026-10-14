//! Text console on the Limine framebuffer, plus the `print!`/`println!` macros.
//!
//! Output goes to COM1 and to the screen console, unless the printing task is
//! attached to a terminal (see tty.rs), in which case it goes there instead.
//! The screen console is a [`TextGrid`] drawn with the 8x16 font; after each print
//! only cells that differ from what is on screen are redrawn, so a scroll costs one
//! repaint per print call rather than one per line.
//!
//! When the desktop takes over the framebuffer, the console stops drawing
//! ([`suspend`]); it keeps its text for when it is shown again.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::limine;
use crate::serial::Serial;
use crate::sync::SpinLock;
use crate::textgrid::{Cell, TextGrid, FONT, FONT_HEIGHT, FONT_WIDTH, PALETTE};

/// The framebuffer, as Limine described it. Shared with the desktop compositor.
#[derive(Clone, Copy)]
pub struct Screen {
    pub addr: usize,
    pub pitch: usize,
    pub width: usize,
    pub height: usize,
    shifts: [u8; 3],
}

impl Screen {
    /// Convert 0xRRGGBB to the framebuffer's pixel format.
    pub fn pixel(&self, rgb: u32) -> u32 {
        let [r, g, b] = [(rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF];
        (r << self.shifts[0]) | (g << self.shifts[1]) | (b << self.shifts[2])
    }

    /// True if pixels are 0x00RRGGBB in memory, so buffers can be copied verbatim.
    pub fn is_xrgb(&self) -> bool {
        self.shifts == [16, 8, 0]
    }
}

struct Console {
    screen: Option<Screen>,
    grid: TextGrid,
    /// What is on screen: (cell, has cursor). None = must redraw.
    shown: Vec<Option<(Cell, bool)>>,
    suspended: bool,
}

static CONSOLE: SpinLock<Option<Console>> = SpinLock::new(None);
static SCREEN: SpinLock<Option<Screen>> = SpinLock::new(None);

pub fn screen() -> Option<Screen> {
    *SCREEN.try_lock()?
}

/// Screen facts for the `info` command: (width, height, cols, rows) in pixels and cells.
pub fn screen_info() -> Option<(usize, usize, usize, usize)> {
    let c = CONSOLE.try_lock()?;
    let c = c.as_ref()?;
    let s = c.screen?;
    Some((s.width, s.height, c.grid.cols, c.grid.rows))
}

/// Attach the console to Limine's first framebuffer. Returns the grid size in cells.
/// Requires the heap.
pub fn init() -> Result<(usize, usize), &'static str> {
    let fb = limine::FRAMEBUFFER.first().ok_or("no framebuffer from bootloader")?;
    // Only the common case is supported: 32-bit pixels with 8-bit RGB channels.
    if fb.memory_model != limine::FRAMEBUFFER_RGB
        || fb.bpp != 32
        || [fb.red_mask_size, fb.green_mask_size, fb.blue_mask_size] != [8, 8, 8]
    {
        return Err("unsupported pixel format (need 32 bpp RGB)");
    }
    let screen = Screen {
        addr: fb.address as usize,
        pitch: fb.pitch as usize,
        width: fb.width as usize,
        height: fb.height as usize,
        shifts: [fb.red_mask_shift, fb.green_mask_shift, fb.blue_mask_shift],
    };
    let (cols, rows) = (screen.width / FONT_WIDTH, screen.height / FONT_HEIGHT);
    let mut console = Console { screen: Some(screen), grid: TextGrid::new(cols, rows), shown: vec![None; cols * rows], suspended: false };
    console.clear_pixels();
    console.flush();
    *CONSOLE.try_lock().ok_or("console busy")? = Some(console);
    *SCREEN.try_lock().ok_or("console busy")? = Some(screen);
    Ok((cols, rows))
}

/// Stop drawing (the desktop owns the screen now).
pub fn suspend() {
    if let Some(Some(c)) = CONSOLE.try_lock().as_deref_mut() {
        c.suspended = true;
    }
}

/// Draw again, repainting everything.
pub fn resume() {
    if let Some(Some(c)) = CONSOLE.try_lock().as_deref_mut() {
        c.suspended = false;
        c.clear_pixels();
        c.flush();
    }
}

impl Console {
    fn put_pixel(s: &Screen, x: usize, y: usize, color: u32) {
        // SAFETY: callers keep x < width and y < height; Limine maps the whole
        // framebuffer (pitch * height bytes) at `addr`.
        unsafe { ((s.addr + y * s.pitch + x * 4) as *mut u32).write_volatile(color) }
    }

    fn clear_pixels(&mut self) {
        let Some(s) = self.screen else { return };
        let bg = s.pixel(PALETTE[0]);
        for y in 0..s.height {
            for x in 0..s.width {
                Self::put_pixel(&s, x, y, bg);
            }
        }
        self.shown.fill(None);
    }

    fn draw_cell(s: &Screen, col: usize, row: usize, cell: Cell, cursor: bool) {
        let (fg, bg) = (s.pixel(PALETTE[cell.fg as usize & 15]), s.pixel(PALETTE[cell.bg as usize & 15]));
        let glyph = &FONT[cell.ch as usize * FONT_HEIGHT..][..FONT_HEIGHT];
        for (dy, &bits) in glyph.iter().enumerate() {
            // Cursor: a solid bar over the bottom two rows of the cell.
            let bits = if cursor && dy >= FONT_HEIGHT - 2 { 0xFF } else { bits };
            for dx in 0..FONT_WIDTH {
                let on = bits & (0x80 >> dx) != 0;
                Self::put_pixel(s, col * FONT_WIDTH + dx, row * FONT_HEIGHT + dy, if on { fg } else { bg });
            }
        }
    }

    /// Redraw every cell that differs from the screen.
    fn flush(&mut self) {
        let Some(s) = self.screen else { return };
        if self.suspended {
            return;
        }
        let cursor = self.grid.cursor_index();
        let cols = self.grid.cols;
        for (i, &cell) in self.grid.cells.iter().enumerate() {
            let want = Some((cell, i == cursor));
            if self.shown[i] != want {
                Self::draw_cell(&s, i % cols, i / cols, cell, i == cursor);
                self.shown[i] = want;
            }
        }
        self.grid.dirty = false;
    }
}

/// Write to the screen console (not serial). Skipped if the console is busy, which
/// only happens when a fault interrupted the console itself.
pub fn write_screen(args: fmt::Arguments) {
    if let Some(mut guard) = CONSOLE.try_lock()
        && let Some(console) = guard.as_mut()
    {
        let _ = console.grid.write_fmt(args);
        console.flush();
    }
}

/// Backend of `print!`.
pub fn print(args: fmt::Arguments) {
    if crate::tty::print_to_current(args) {
        return;
    }
    let _ = Serial.write_fmt(args);
    write_screen(args);
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::console::print(format_args!($($arg)*)) };
}
