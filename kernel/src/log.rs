//! Kernel log messages that should not land in a terminal: they go to COM1, and to an
//! in-memory ring that the `dmesg` command shows (real PCs have no serial port).

use alloc::collections::VecDeque;
use alloc::string::String;
use core::fmt::Write;

use crate::sync::SpinLock;

/// The last kernel log lines (oldest first).
static RING: SpinLock<VecDeque<String>> = SpinLock::new(VecDeque::new());
const RING_LINES: usize = 2000;

fn remember(line: String) {
    // Never wait here: a message may come from a fault or an interrupt handler that
    // interrupted another logger.
    if let Some(mut ring) = RING.try_lock() {
        if ring.len() >= RING_LINES {
            ring.pop_front();
        }
        ring.push_back(line);
    }
}

/// The kernel console's current unfinished line (`print!` without a newline yet).
static PARTIAL: SpinLock<String> = SpinLock::new(String::new());

/// Console output is kept until the shell or desktop starts (after that the console is
/// theirs; kernel messages still arrive through warn and info).
static BOOTING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);

pub fn boot_done() {
    BOOTING.store(false, core::sync::atomic::Ordering::Relaxed);
}

/// Keep kernel console output (boot messages, the device list) for `dmesg` too.
pub fn capture(args: core::fmt::Arguments) {
    if !BOOTING.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let Some(mut partial) = PARTIAL.try_lock() else { return };
    let _ = partial.write_fmt(args);
    while let Some(end) = partial.find('\n') {
        let line: String = partial[..end].trim_end_matches('\r').into();
        partial.replace_range(..=end, "");
        // Escape sequences (colours) are for the screen only.
        remember(strip_escapes(&line));
    }
}

fn strip_escapes(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

pub fn warn(msg: &str) {
    let _ = writeln!(crate::serial::Serial, "[nova] warning: {msg}\r");
    remember(alloc::format!("warning: {msg}"));
}

pub fn info(msg: &str) {
    let _ = writeln!(crate::serial::Serial, "[nova] {msg}\r");
    remember(String::from(msg));
}

/// Logged lines containing `filter` (all with an empty filter), oldest first.
pub fn lines(filter: &str) -> alloc::vec::Vec<String> {
    RING.lock().iter().filter(|l| l.contains(filter)).cloned().collect()
}
