//! 8253/8254 programmable interval timer, channel 0 on IRQ 0: the system tick.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::port::outb;

/// Tick rate: 1 ms, the resolution of sleeps, timeouts and timers.
pub const HZ: u64 = 1000;
/// User programs are preempted every this many ticks (4 ms time slices).
pub const PREEMPT_TICKS: u64 = 4;
const BASE_FREQUENCY: u64 = 1_193_182;

static TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let divisor = (BASE_FREQUENCY / HZ) as u16;
    // SAFETY: PIT command and channel 0 data ports.
    unsafe {
        outb(0x43, 0x36); // channel 0, lobyte/hibyte, mode 3 (square wave), binary
        outb(0x40, divisor as u8);
        outb(0x40, (divisor >> 8) as u8);
    }
}

/// Called from the IRQ 0 handler.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "linux-drivers")]
    crate::linuxkpi::tick();
}

/// Ticks since the timer was started.
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// Timestamp-counter frequency (Hz), measured against the PIT at boot.
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// TSC value at calibration, paired with the tick count then.
static TSC_BASE: AtomicU64 = AtomicU64::new(0);
static TICK_BASE: AtomicU64 = AtomicU64::new(0);

fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: RDTSC only reads the timestamp counter.
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

/// Measure the TSC frequency over 100 ms. Interrupts must be enabled.
pub fn calibrate() -> u64 {
    let start_tick = ticks();
    while ticks() == start_tick {
        core::hint::spin_loop();
    }
    let (t0, k0) = (rdtsc(), ticks());
    while ticks() < k0 + HZ / 10 {
        core::hint::spin_loop();
    }
    let (t1, k1) = (rdtsc(), ticks());
    let hz = (t1 - t0) * HZ / (k1 - k0);
    TSC_BASE.store(t0, Ordering::Relaxed);
    TICK_BASE.store(k0, Ordering::Relaxed);
    TSC_HZ.store(hz, Ordering::Relaxed);
    TSC_MULT.store(((1_000_000_000u128 << 32) / hz as u128) as u64, Ordering::Relaxed);
    hz
}

/// Nanoseconds per TSC tick, times 2^32 (0 before calibration).
static TSC_MULT: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds since the timer started, with TSC resolution (tick resolution
/// before calibration). The vDSO computes the same, from [`clock_params`].
pub fn nanos() -> u64 {
    let mult = TSC_MULT.load(Ordering::Relaxed);
    if mult == 0 {
        return ticks() * (1_000_000_000 / HZ);
    }
    let base_ns = TICK_BASE.load(Ordering::Relaxed) * (1_000_000_000 / HZ);
    let elapsed = rdtsc().wrapping_sub(TSC_BASE.load(Ordering::Relaxed));
    base_ns + ((elapsed as u128 * mult as u128) >> 32) as u64
}

/// (TSC at the base, nanoseconds at the base, nanoseconds per tick times 2^32): the clock
/// [`nanos`] reads, for the vDSO. None before calibration.
pub fn clock_params() -> Option<(u64, u64, u64)> {
    let mult = TSC_MULT.load(Ordering::Relaxed);
    (mult != 0).then(|| (TSC_BASE.load(Ordering::Relaxed), TICK_BASE.load(Ordering::Relaxed) * (1_000_000_000 / HZ), mult))
}
