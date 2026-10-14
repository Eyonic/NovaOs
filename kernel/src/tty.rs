//! Terminals: a text grid plus an input queue, attached to tasks.
//!
//! A task attached to a terminal prints into the terminal's grid and reads keys
//! from its queue (`input::read_key`). On the desktop every terminal window owns
//! one; the window draws the grid and pushes the keys typed into it. A terminal
//! can also mirror COM1: its output is copied to the serial port and serial input
//! is accepted as keystrokes. That keeps the system usable (and testable) over a
//! serial line while the desktop is running.
//!
//! Fault and panic reports bypass terminals ([`emergency`]): they always go to
//! COM1 and the screen console.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::sync::Arc;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::input::Key;
use crate::serial::Serial;
use crate::sync::SpinLock;
use crate::textgrid::TextGrid;

pub struct Tty {
    pub grid: TextGrid,
    input: VecDeque<Key>,
    pub mirror_serial: bool,
    /// Set when the window is closed; reads then return Ctrl-D (end of input).
    pub hung_up: bool,
    pub title: String,
    /// When set, output is collected here instead of the grid (see `capture`).
    capture: Option<String>,
}

pub type TtyRef = Arc<SpinLock<Tty>>;

static EMERGENCY: AtomicBool = AtomicBool::new(false);

pub fn new(cols: usize, rows: usize, title: &str, mirror_serial: bool) -> TtyRef {
    Arc::new(SpinLock::new(Tty { grid: TextGrid::new(cols, rows), input: VecDeque::new(), mirror_serial, hung_up: false, title: title.into(), capture: None }))
}

/// From now on, all output goes to COM1 and the screen console (fault reporting).
pub fn emergency() {
    EMERGENCY.store(true, Ordering::SeqCst);
}

pub fn in_emergency() -> bool {
    EMERGENCY.load(Ordering::SeqCst)
}

/// Print to the current task's terminal, if it has one. Returns false if the
/// caller should print to the console instead.
pub fn print_to_current(args: fmt::Arguments) -> bool {
    if in_emergency() {
        return false;
    }
    let Some(tty) = crate::task::current_tty() else { return false };
    // try_lock: printing must never block or yield (it can run inside other locks).
    let Some(mut t) = tty.try_lock() else { return true };
    if let Some(c) = &mut t.capture {
        let _ = c.write_fmt(args);
        return true;
    }
    let _ = t.grid.write_fmt(args);
    if t.mirror_serial {
        let _ = Serial.write_fmt(args);
    }
    true
}

/// Write raw bytes (possibly partial UTF-8) to the current task's terminal, or to
/// the console if it has none. Used for user programs' stdout/stderr.
pub fn write_bytes(data: &[u8]) {
    match crate::task::current_tty() {
        Some(tty) if !in_emergency() => {
            let mut t = tty.lock();
            if let Some(c) = &mut t.capture {
                c.push_str(&String::from_utf8_lossy(data));
                return;
            }
            for &b in data {
                t.grid.write_byte(b);
            }
            if t.mirror_serial {
                drop(t);
                crate::serial::write_bytes(data);
            }
        }
        _ => crate::console::print(format_args!("{}", String::from_utf8_lossy(data))),
    }
}

/// Run `f` with the current task's output collected into a string.
pub fn capture(f: impl FnOnce()) -> String {
    let collector = new(80, 1, "capture", false);
    collector.lock().capture = Some(String::new());
    let previous = crate::task::current_tty();
    crate::task::set_tty(Some(collector.clone()));
    f();
    crate::task::set_tty(previous);
    let mut c = collector.lock();
    c.capture.take().unwrap_or_default()
}

impl Tty {
    pub fn push_key(&mut self, key: Key) {
        if self.input.len() < 4096 {
            self.input.push_back(key);
        }
    }
}

/// True if a key is waiting in `tty`'s queue.
pub fn has_input(tty: &TtyRef) -> bool {
    tty.try_lock().is_some_and(|t| !t.input.is_empty())
}

/// Next key for `tty`, waiting (and letting other tasks run) until one arrives.
pub fn read_key(tty: &TtyRef) -> Key {
    loop {
        {
            let mut t = tty.lock();
            if let Some(k) = t.input.pop_front() {
                return k;
            }
            if t.hung_up {
                return Key::Char(0x04);
            }
            if t.mirror_serial {
                drop(t);
                if let Some(k) = crate::input::poll_serial() {
                    return k;
                }
            }
        }
        crate::task::wait_event();
    }
}
