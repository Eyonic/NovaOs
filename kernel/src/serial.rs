//! Minimal 16550 UART driver for COM1 (I/O port 0x3F8).
//!
//! Output is polled. Input arrives through IRQ 4 once [`enable_rx_interrupt`]
//! has run; the IRQ handler drains the receive buffer into [`crate::input`].
//! QEMU routes COM1 to `-serial`.

use core::fmt;

use crate::port::{inb, outb};

const COM1: u16 = 0x3F8;

/// Configure COM1 for 115200 baud, 8N1, FIFO enabled, UART interrupts off.
pub fn init() {
    // SAFETY: standard 16550 init sequence on COM1 only.
    unsafe {
        outb(COM1 + 1, 0x00); // disable UART interrupts
        outb(COM1 + 3, 0x80); // DLAB on
        outb(COM1, 0x01); // divisor low  (115200 baud)
        outb(COM1 + 1, 0x00); // divisor high
        outb(COM1 + 3, 0x03); // DLAB off, 8 bits, no parity, 1 stop bit
        outb(COM1 + 2, 0xC7); // enable + clear FIFOs, 14-byte threshold
        outb(COM1 + 4, 0x03); // DTR + RTS
    }
}

/// Raise IRQ 4 when a byte has been received. Requires the PIC to be set up.
pub fn enable_rx_interrupt() {
    // SAFETY: COM1 registers only. OUT2 (MCR bit 3) gates the UART's IRQ line on PCs.
    unsafe {
        while inb(COM1 + 5) & 0x01 != 0 {
            inb(COM1); // discard anything received before we were listening
        }
        outb(COM1 + 4, 0x0B); // DTR + RTS + OUT2
        outb(COM1 + 1, 0x01); // interrupt on received data available
    }
}

/// One received byte, if the UART has any.
pub fn read_byte() -> Option<u8> {
    // SAFETY: polling the line-status register and reading RBR of COM1.
    unsafe { (inb(COM1 + 5) & 0x01 != 0).then(|| inb(COM1)) }
}

fn write_byte(byte: u8) {
    // SAFETY: polling the line-status register and writing THR of COM1.
    unsafe {
        while inb(COM1 + 5) & 0x20 == 0 {} // wait for transmit holding register empty
        outb(COM1, byte);
    }
    crate::remote::push(&[byte]);
}

/// Write raw bytes (LF becomes CR LF, as for text).
pub fn write_bytes(data: &[u8]) {
    for &b in data {
        if b == b'\n' {
            write_byte(b'\r');
        }
        write_byte(b);
    }
}

/// Zero-sized handle; all state lives in the UART itself, so this is lock-free.
pub struct Serial;

impl fmt::Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                write_byte(b'\r');
            }
            write_byte(b);
        }
        Ok(())
    }
}
