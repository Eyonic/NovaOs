//! Processor identification (CPUID).

use alloc::string::String;
use core::arch::asm;

fn cpuid(leaf: u32) -> [u32; 4] {
    let (a, b, c, d): (u32, u32, u32, u32);
    // SAFETY: CPUID is available on every x86-64 CPU; RBX is reserved by LLVM, so it
    // is saved and restored by hand.
    unsafe {
        asm!("mov {tmp:r}, rbx", "cpuid", "xchg {tmp:r}, rbx", tmp = out(reg) b,
             inout("eax") leaf => a, inout("ecx") 0 => c, out("edx") d, options(nostack, preserves_flags));
    }
    [a, b, c, d]
}

/// The processor's brand string, e.g. "AMD Ryzen 7 5800X 8-Core Processor".
pub fn brand() -> String {
    if cpuid(0x8000_0000)[0] < 0x8000_0004 {
        let v = cpuid(0);
        let bytes: alloc::vec::Vec<u8> = [v[1], v[3], v[2]].iter().flat_map(|r| r.to_le_bytes()).collect();
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    let mut bytes = alloc::vec::Vec::new();
    for leaf in 0x8000_0002..=0x8000_0004 {
        for r in cpuid(leaf) {
            bytes.extend_from_slice(&r.to_le_bytes());
        }
    }
    String::from_utf8_lossy(&bytes).trim_matches(|c: char| c == '\0' || c == ' ').into()
}
