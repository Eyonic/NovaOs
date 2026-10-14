//! PS/2 keyboard on the 8042 controller (IRQ 1), US layout.
//!
//! The controller translates the keyboard's output to scan code set 1, which is
//! what Nova decodes: one byte per make code, the same byte | 0x80 on release, and
//! an 0xE0 prefix for extended keys such as the arrows.
//!
//! References: OSDev wiki "8042 PS/2 Controller" and "PS/2 Keyboard" (scan code set 1).

use crate::input::Key;
use crate::port::{inb, outb};

const DATA: u16 = 0x60;
const STATUS_CMD: u16 = 0x64;
const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;
/// Polling budget for controller handshakes, so a missing controller cannot hang boot.
const SPINS: u32 = 100_000;

fn wait_writable() -> bool {
    // SAFETY (all port access in this module): 8042 controller ports only.
    (0..SPINS).any(|_| unsafe { inb(STATUS_CMD) } & STATUS_INPUT_FULL == 0)
}

fn read_data() -> Option<u8> {
    (0..SPINS).any(|_| unsafe { inb(STATUS_CMD) } & STATUS_OUTPUT_FULL != 0).then(|| unsafe { inb(DATA) })
}

fn command(cmd: u8) -> bool {
    wait_writable() && {
        unsafe { outb(STATUS_CMD, cmd) };
        true
    }
}

fn write_data(byte: u8) -> bool {
    wait_writable() && {
        unsafe { outb(DATA, byte) };
        true
    }
}

/// Enable the first PS/2 port with translation and its IRQ, and enable scanning.
/// Must run with interrupts disabled. Returns false if the controller did not respond.
pub fn init() -> bool {
    if !command(0xAD) || !command(0xA7) {
        return false; // disable both ports while reconfiguring
    }
    while unsafe { inb(STATUS_CMD) } & STATUS_OUTPUT_FULL != 0 {
        unsafe { inb(DATA) }; // flush stale bytes
    }
    if !command(0x20) {
        return false;
    }
    let Some(config) = read_data() else { return false };
    // bit 0: port 1 IRQ on; bit 1: port 2 IRQ off; bit 4: port 1 clock on (0); bit 6: translation on.
    let config = (config | 0x01 | 0x40) & !(0x02 | 0x10);
    if !command(0x60) || !write_data(config) || !command(0xAE) {
        return false;
    }
    // Enable scanning (0xF4); the keyboard answers with ACK 0xFA, which we consume
    // here so it does not appear as a key press.
    write_data(0xF4) && read_data() == Some(0xFA)
}

/// Read the pending scan code byte. Called from the IRQ 1 handler.
pub fn read_scancode() -> u8 {
    unsafe { inb(DATA) }
}

const fn table(s: &[u8]) -> [u8; 0x3A] {
    assert!(s.len() == 0x3A, "scan code table must cover make codes 0x00-0x39");
    let mut t = [0; 0x3A];
    let mut i = 0;
    while i < t.len() {
        t[i] = s[i];
        i += 1;
    }
    t
}

// Scan code set 1 make codes 0x00-0x39. 0 = no character (modifier or unused).
const NORMAL: [u8; 0x3A] = table(b"\0\x1b1234567890-=\x08\tqwertyuiop[]\n\0asdfghjkl;'`\0\\zxcvbnm,./\0*\0 ");
const SHIFTED: [u8; 0x3A] = table(b"\0\x1b!@#$%^&*()_+\x08\tQWERTYUIOP{}\n\0ASDFGHJKL:\"~\0|ZXCVBNM<>?\0*\0 ");

/// Scan code set 1 decoder state. Lives in the main loop, not the IRQ handler.
pub struct Decoder {
    extended: bool,
    shift: [bool; 2],
    ctrl: bool,
    caps_lock: bool,
}

impl Decoder {
    pub const fn new() -> Self {
        Self { extended: false, shift: [false; 2], ctrl: false, caps_lock: false }
    }

    pub fn feed(&mut self, code: u8) -> Option<Key> {
        if code == 0xE0 {
            self.extended = true;
            return None;
        }
        let extended = core::mem::take(&mut self.extended);
        let released = code & 0x80 != 0;
        let make = code & 0x7F;

        match (extended, make) {
            (false, 0x2A) => self.shift[0] = !released,
            (false, 0x36) => self.shift[1] = !released,
            (_, 0x1D) => self.ctrl = !released, // left (plain) or right (E0) ctrl
            _ if released => {}
            (false, 0x3A) => self.caps_lock = !self.caps_lock,
            (true, 0x48) => return Some(Key::Up),
            (true, 0x50) => return Some(Key::Down),
            (true, 0x4B) => return Some(Key::Left),
            (true, 0x4D) => return Some(Key::Right),
            (true, 0x1C) => return Some(Key::Char(b'\n')), // keypad Enter
            (true, 0x35) => return Some(Key::Char(b'/')),  // keypad /
            (false, m) if (m as usize) < NORMAL.len() => {
                let shift = self.shift[0] || self.shift[1];
                let mut ch = if shift { SHIFTED[m as usize] } else { NORMAL[m as usize] };
                if self.caps_lock && ch.is_ascii_alphabetic() {
                    ch ^= 0x20; // caps lock inverts the case shift chose
                }
                if self.ctrl && ch.is_ascii_alphabetic() {
                    ch &= 0x1F; // Ctrl-A = 0x01 ... Ctrl-Z = 0x1A
                }
                return (ch != 0).then_some(Key::Char(ch));
            }
            _ => {}
        }
        None
    }
}
