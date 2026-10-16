//! Terminal input from the PS/2 keyboard and the serial port.
//!
//! IRQ handlers only push raw bytes into lock-free rings (producer: the IRQ handler;
//! consumer: the main loop). Decoding into
//! [`Key`]s happens in the main loop, in [`read_key`].

use alloc::collections::VecDeque;
use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::keyboard;
use crate::sync::SpinLock;

/// A decoded key press. Enter is `Char(b'\n')`, Backspace `Char(0x08)`, Tab
/// `Char(b'\t')`, and Ctrl-letter the matching control character (Ctrl-C = 0x03).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Key {
    Char(u8),
    Up,
    Down,
    Left,
    Right,
    /// The Windows (Super) key: always brings back Home.
    Super,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    /// F1: opens or tucks away the assistant's chat panel.
    F1,
}

const RING_SIZE: usize = 4096; // power of two (serial input can arrive a long line at a time)

pub struct Ring {
    buf: UnsafeCell<[u8; RING_SIZE]>,
    /// Next slot to write; only the producer stores it.
    head: AtomicUsize,
    /// Next slot to read; only the consumer stores it.
    tail: AtomicUsize,
    /// Held while a byte is pushed: producers on different CPUs (COM1's interrupt on the
    /// boot CPU, the remote console's or USB keyboard's task anywhere) take turns. Always
    /// taken with interrupts off, so a producer never waits for itself.
    producing: AtomicBool,
}

// SAFETY: producers are serialised by `producing`, and there is one consumer at a time
// (it runs under the big kernel lock); each slot is written before the Release store of
// `head` publishes it and read before the Release store of `tail` frees it.
unsafe impl Sync for Ring {}

impl Ring {
    pub const fn new() -> Self {
        Self { buf: UnsafeCell::new([0; RING_SIZE]), head: AtomicUsize::new(0), tail: AtomicUsize::new(0), producing: AtomicBool::new(false) }
    }

    /// Producer side (any context). Drops the byte if the ring is full.
    pub fn push(&self, byte: u8) {
        let flags: u64;
        // SAFETY: save RFLAGS and disable interrupts; restored below.
        unsafe { asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem)) };
        while self.producing.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        let head = self.head.load(Ordering::Relaxed);
        if head.wrapping_sub(self.tail.load(Ordering::Acquire)) != RING_SIZE {
            // SAFETY: the slot is free (checked above) and only this producer writes.
            unsafe { (*self.buf.get())[head % RING_SIZE] = byte };
            self.head.store(head.wrapping_add(1), Ordering::Release);
        }
        self.producing.store(false, Ordering::Release);
        if flags & (1 << 9) != 0 {
            // SAFETY: interrupts were enabled on entry.
            unsafe { asm!("sti", options(nomem, nostack)) };
        }
    }

    /// Consumer side.
    pub fn pop(&self) -> Option<u8> {
        let tail = self.tail.load(Ordering::Relaxed);
        if tail == self.head.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: the slot was published by the producer's Release store of `head`.
        let byte = unsafe { (*self.buf.get())[tail % RING_SIZE] };
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        Some(byte)
    }

}

/// Scan code bytes from IRQ 1.
pub static SCANCODES: Ring = Ring::new();
/// Received bytes from IRQ 4 (COM1).
pub static SERIAL_RX: Ring = Ring::new();

/// Decoder for terminal input on the serial line: CR or LF = Enter, DEL or BS =
/// Backspace, and the VT100 arrow sequences `ESC [ A/B/C/D`.
#[derive(Clone, Copy)]
enum SerialState {
    Normal,
    Escape,
    Csi,
    /// The previous byte was CR; a following LF belongs to the same Enter.
    AfterCr,
}

/// Decoder state. Each decoder has one consumer at a time (the text-mode reader,
/// or the desktop for the keyboard and the serial-mirroring terminal for COM1), and
/// tasks never run concurrently, so plain statics behind a SpinLock suffice.
static KEYBOARD: SpinLock<keyboard::Decoder> = SpinLock::new(keyboard::Decoder::new());
static SERIAL: SpinLock<SerialState> = SpinLock::new(SerialState::Normal);

fn decode_serial(state: &mut SerialState, byte: u8) -> Option<Key> {
    let (next, key) = match (*state, byte) {
        (SerialState::Escape, b'[' | b'O') => (SerialState::Csi, None),
        (SerialState::Escape, _) => (SerialState::Normal, None),
        (SerialState::Csi, b'A') => (SerialState::Normal, Some(Key::Up)),
        (SerialState::Csi, b'B') => (SerialState::Normal, Some(Key::Down)),
        (SerialState::Csi, b'C') => (SerialState::Normal, Some(Key::Right)),
        (SerialState::Csi, b'D') => (SerialState::Normal, Some(Key::Left)),
        (SerialState::Csi, 0x40..=0x7E) => (SerialState::Normal, None), // other sequence: ignore
        (SerialState::Csi, _) => (SerialState::Csi, None),
        (SerialState::AfterCr, b'\n') => (SerialState::Normal, None),
        (_, 0x1B) => (SerialState::Escape, None),
        (_, b'\r') => (SerialState::AfterCr, Some(Key::Char(b'\n'))),
        (_, 0x7F) => (SerialState::Normal, Some(Key::Char(0x08))),
        (_, b) => (SerialState::Normal, Some(Key::Char(b))),
    };
    *state = next;
    key
}

/// Raw key events (Linux key code, pressed) decoded alongside the keys, for windows
/// that take raw input (wm.rs); the oldest are dropped if nobody takes them.
static RAW_KEYS: SpinLock<(bool, VecDeque<(u16, bool)>)> = SpinLock::new((false, VecDeque::new()));

/// Set-1 scancode (with or without the E0 prefix) to a Linux key code. Without E0 the
/// make codes 1..=0x58 are the Linux codes themselves.
fn linux_keycode(code: u8, e0: bool) -> Option<u16> {
    let make = code & 0x7F;
    if !e0 {
        return (1..=0x58).contains(&make).then_some(make as u16);
    }
    Some(match make {
        0x1C => 96,  // KEY_KPENTER
        0x1D => 97,  // KEY_RIGHTCTRL
        0x35 => 98,  // KEY_KPSLASH
        0x38 => 100, // KEY_RIGHTALT
        0x47 => 102, // KEY_HOME
        0x48 => 103, // KEY_UP
        0x49 => 104, // KEY_PAGEUP
        0x4B => 105, // KEY_LEFT
        0x4D => 106, // KEY_RIGHT
        0x4F => 107, // KEY_END
        0x50 => 108, // KEY_DOWN
        0x51 => 109, // KEY_PAGEDOWN
        0x52 => 110, // KEY_INSERT
        0x53 => 111, // KEY_DELETE
        0x5B => 125, // KEY_LEFTMETA
        0x5C => 126, // KEY_RIGHTMETA
        0x5D => 127, // KEY_COMPOSE
        _ => return None,
    })
}

fn record_raw(code: u8) {
    let mut r = RAW_KEYS.lock();
    if code == 0xE0 {
        r.0 = true;
        return;
    }
    let e0 = core::mem::take(&mut r.0);
    if let Some(k) = linux_keycode(code, e0) {
        if r.1.len() >= 128 {
            r.1.pop_front();
        }
        r.1.push_back((k, code & 0x80 == 0));
    }
}

/// Raw key events since the last call.
pub fn take_raw_keys() -> VecDeque<(u16, bool)> {
    core::mem::take(&mut RAW_KEYS.lock().1)
}

/// Next decoded key from the PS/2 keyboard, if any.
pub fn poll_keyboard() -> Option<Key> {
    let mut decoder = KEYBOARD.try_lock()?;
    while let Some(code) = SCANCODES.pop() {
        record_raw(code);
        if let Some(key) = decoder.feed(code) {
            return Some(key);
        }
    }
    None
}

/// Next decoded key from COM1, if any.
pub fn poll_serial() -> Option<Key> {
    let mut state = SERIAL.try_lock()?;
    while let Some(byte) = SERIAL_RX.pop() {
        if let Some(key) = decode_serial(&mut state, byte) {
            return Some(key);
        }
    }
    None
}

/// Block until a key arrives: from the current task's terminal if it has one,
/// otherwise from the keyboard or the serial line directly (text mode).
pub fn read_key() -> Key {
    if let Some(tty) = crate::task::current_tty() {
        return crate::tty::read_key(&tty);
    }
    loop {
        if let Some(key) = poll_keyboard().or_else(poll_serial) {
            return key;
        }
        crate::task::wait_event();
    }
}
