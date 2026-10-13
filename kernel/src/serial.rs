//! Minimal 16550 UART driver for COM1 (I/O port 0x3F8).
//!
//! Output-only and polled; no interrupts. QEMU routes COM1 to `-serial`.

use core::arch::asm;
use core::fmt;

const COM1: u16 = 0x3F8;

/// Write one byte to an I/O port.
///
/// SAFETY: port I/O has hardware side effects; callers must only touch ports
/// belonging to a device they own. Here that is exclusively COM1.
unsafe fn outb(port: u16, value: u8) {
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) }
}

/// Read one byte from an I/O port. SAFETY: see [`outb`].
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags)) }
    value
}

/// Configure COM1 for 115200 baud, 8N1, FIFO enabled.
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

fn write_byte(byte: u8) {
    // SAFETY: polling the line-status register and writing THR of COM1.
    unsafe {
        while inb(COM1 + 5) & 0x20 == 0 {} // wait for transmit holding register empty
        outb(COM1, byte);
    }
}

/// Zero-sized handle; all state lives in the UART itself.
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

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::print!("{}\n", format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let _ = write!($crate::serial::Serial, $($arg)*);
    }};
}
