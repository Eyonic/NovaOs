//! CMOS real-time clock: wall-clock time for TLS certificate checks and the desktop clock.
//!
//! Read once at boot (waiting out an update cycle), then advanced with the PIT.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::pit;
use crate::port::{inb, outb};

static BOOT_UNIX: AtomicU64 = AtomicU64::new(0);
static BOOT_TICKS: AtomicU64 = AtomicU64::new(0);

fn cmos(reg: u8) -> u8 {
    // SAFETY: CMOS index/data ports; bit 7 of the index keeps NMI enabled state as 0 (enabled).
    unsafe {
        outb(0x70, reg);
        inb(0x71)
    }
}

fn updating() -> bool {
    cmos(0x0A) & 0x80 != 0
}

fn read_raw() -> [u8; 6] {
    while updating() {}
    [cmos(0x00), cmos(0x02), cmos(0x04), cmos(0x07), cmos(0x08), cmos(0x09)]
}

/// Days from 1970-01-01 to the given civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub fn init() {
    // Read until two consecutive reads agree, so we never see a half-updated value.
    let mut raw = read_raw();
    loop {
        let again = read_raw();
        if again == raw {
            break;
        }
        raw = again;
    }
    let status_b = cmos(0x0B);
    let bcd = status_b & 0x04 == 0;
    let conv = |v: u8| if bcd { (v & 0x0F) + (v >> 4) * 10 } else { v };
    let [s, mi, h, d, mo, y] = raw;
    let (sec, min, day, month, year) = (conv(s), conv(mi), conv(d), conv(mo), conv(y));
    // 12-hour mode: bit 7 of the hour is PM.
    let hour = if status_b & 0x02 == 0 && h & 0x80 != 0 { (conv(h & 0x7F) % 12) + 12 } else { conv(h & 0x7F) };
    let days = days_from_civil(2000 + year as i64, month as i64, day as i64);
    let unix = days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64;
    BOOT_UNIX.store(unix.max(0) as u64, Ordering::Relaxed);
    BOOT_TICKS.store(pit::ticks(), Ordering::Relaxed);
}

/// Nanoseconds since 1970-01-01 UTC, advancing with the TSC-based monotonic clock.
pub fn unix_nanos() -> u64 {
    let boot_ns = BOOT_TICKS.load(Ordering::Relaxed) * (1_000_000_000 / pit::HZ);
    BOOT_UNIX.load(Ordering::Relaxed) * 1_000_000_000 + pit::nanos().saturating_sub(boot_ns)
}

/// Seconds since 1970-01-01 UTC (the RTC is assumed to hold UTC, as QEMU's does by default).
pub fn unix_time() -> u64 {
    BOOT_UNIX.load(Ordering::Relaxed) + (pit::ticks() - BOOT_TICKS.load(Ordering::Relaxed)) / pit::HZ
}

/// (year, month, day, hour, minute, second) in UTC.
pub fn now() -> (i64, u32, u32, u32, u32, u32) {
    let t = unix_time() as i64;
    let (days, secs) = (t.div_euclid(86400), t.rem_euclid(86400));
    // Inverse of days_from_civil.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d, (secs / 3600) as u32, (secs / 60 % 60) as u32, (secs % 60) as u32)
}
