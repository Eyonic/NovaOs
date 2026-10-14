//! Terminal input from the PS/2 keyboard and the serial port.
//!
//! IRQ handlers only push raw bytes into lock-free single-producer/single-consumer
//! rings (producer: the IRQ handler; consumer: the main loop). Decoding into
//! [`Key`]s happens in the main loop, in [`read_key`].

use core::arch::asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

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
}

const RING_SIZE: usize = 256; // power of two

pub struct Ring {
    buf: UnsafeCell<[u8; RING_SIZE]>,
    /// Next slot to write; only the producer stores it.
    head: AtomicUsize,
    /// Next slot to read; only the consumer stores it.
    tail: AtomicUsize,
}

// SAFETY: single producer and single consumer; each slot is written before the
// Release store of `head` publishes it and read before the Release store of
// `tail` frees it.
unsafe impl Sync for Ring {}

impl Ring {
    pub const fn new() -> Self {
        Self { buf: UnsafeCell::new([0; RING_SIZE]), head: AtomicUsize::new(0), tail: AtomicUsize::new(0) }
    }

    /// Producer side. Drops the byte if the ring is full.
    pub fn push(&self, byte: u8) {
        let head = self.head.load(Ordering::Relaxed);
        if head.wrapping_sub(self.tail.load(Ordering::Acquire)) == RING_SIZE {
            return;
        }
        // SAFETY: the slot is free (checked above) and only the producer writes.
        unsafe { (*self.buf.get())[head % RING_SIZE] = byte };
        self.head.store(head.wrapping_add(1), Ordering::Release);
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

/// Next decoded key from the PS/2 keyboard, if any.
pub fn poll_keyboard() -> Option<Key> {
    let mut decoder = KEYBOARD.try_lock()?;
    while let Some(code) = SCANCODES.pop() {
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

/// Sleep until the next interrupt, unless `still_idle` turns false. The check
/// runs with interrupts disabled, and `sti; hlt` executes atomically (STI delays
/// interrupt delivery by one instruction), so a wake-up cannot be lost between
/// the check and the `hlt`.
pub fn wait_for_interrupt(still_idle: impl FnOnce() -> bool) {
    // SAFETY: only toggles IF and halts until the next interrupt; interrupts are
    // enabled again on every path.
    unsafe {
        asm!("cli", options(nomem, nostack));
        if still_idle() {
            asm!("sti", "hlt", options(nomem, nostack));
        } else {
            asm!("sti", options(nomem, nostack));
        }
    }
}
