//! Kernel log messages that should not land in a terminal: they go to COM1 only.

use core::fmt::Write;

pub fn warn(msg: &str) {
    let _ = writeln!(crate::serial::Serial, "[nova] warning: {msg}\r");
}

pub fn info(msg: &str) {
    let _ = writeln!(crate::serial::Serial, "[nova] {msg}\r");
}
