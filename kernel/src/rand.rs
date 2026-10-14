//! Random numbers for TCP sequence numbers, ports and TLS.
//!
//! Uses the CPU's RDRAND instruction (a hardware CSPRNG) when present. Without it,
//! falls back to SHA-256 in counter mode over a seed gathered from timestamp-counter
//! jitter. That fallback is not a vetted entropy source; `hardware()` reports which
//! one is in use so the boot log can say so.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use crate::sync::SpinLock;

static HAS_RDRAND: AtomicBool = AtomicBool::new(false);
static COUNTER: AtomicU64 = AtomicU64::new(0);
static SEED: SpinLock<[u8; 32]> = SpinLock::new([0; 32]);

fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: RDTSC only reads the timestamp counter.
    unsafe { asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

fn rdrand() -> Option<u64> {
    for _ in 0..10 {
        let value: u64;
        let ok: u8;
        // SAFETY: only executed when CPUID reported RDRAND support.
        unsafe { asm!("rdrand {}", "setc {}", out(reg) value, out(reg_byte) ok, options(nomem, nostack)) };
        if ok != 0 {
            return Some(value);
        }
    }
    None
}

pub fn init() {
    let ecx: u32;
    // SAFETY: CPUID leaf 1; RBX is preserved by hand because LLVM reserves it.
    unsafe {
        asm!("mov {tmp}, rbx", "cpuid", "mov rbx, {tmp}", tmp = out(reg) _,
             inout("eax") 1 => _, out("ecx") ecx, out("edx") _, options(nostack, preserves_flags));
    }
    HAS_RDRAND.store(ecx & (1 << 30) != 0 && rdrand().is_some(), Ordering::Relaxed);
    // Seed the fallback from TSC jitter around port I/O and from RDRAND if available.
    let mut hasher = Sha256::new();
    for _ in 0..4096 {
        let t = rdtsc();
        // SAFETY: reading the POST port is harmless and its latency varies.
        unsafe { crate::port::io_wait() };
        hasher.update((rdtsc().wrapping_sub(t)).to_le_bytes());
        hasher.update(t.to_le_bytes());
    }
    if let Some(r) = hardware().then(rdrand).flatten() {
        hasher.update(r.to_le_bytes());
    }
    *SEED.try_lock().expect("rand init re-entered") = hasher.finalize().into();
}

/// True if random numbers come from RDRAND.
pub fn hardware() -> bool {
    HAS_RDRAND.load(Ordering::Relaxed)
}

pub fn fill(buf: &mut [u8]) {
    for chunk in buf.chunks_mut(8) {
        let value = if hardware() { rdrand() } else { None }.unwrap_or_else(fallback);
        chunk.copy_from_slice(&value.to_le_bytes()[..chunk.len()]);
    }
}

fn fallback() -> u64 {
    let seed = *SEED.try_lock().expect("rand re-entered");
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let digest = Sha256::new().chain_update(seed).chain_update(n.to_le_bytes()).chain_update(rdtsc().to_le_bytes()).finalize();
    u64::from_le_bytes(digest[..8].try_into().unwrap())
}

fn getrandom_backend(buf: &mut [u8]) -> Result<(), getrandom::Error> {
    fill(buf);
    Ok(())
}
getrandom::register_custom_getrandom!(getrandom_backend);

pub fn u64() -> u64 {
    let mut b = [0; 8];
    fill(&mut b);
    u64::from_le_bytes(b)
}
