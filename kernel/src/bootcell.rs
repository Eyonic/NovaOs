//! Interior-mutable statics for CPU-owned tables (GDT, TSS, IDT, IST stack).

use core::cell::UnsafeCell;

/// A static that is written during a CPU's early init, before the CPU is told about it
/// (lgdt/ltr/lidt), with interrupts disabled; per-CPU tables (gdt.rs) are arrays in which
/// each CPU only writes its own element, also later (TSS.RSP0, TLS descriptors). The CPU
/// itself may write some of it (the TSS busy bit, the IST stack), which is why it must
/// live in writable memory.
pub struct BootCell<T>(UnsafeCell<T>);

// SAFETY: shared tables (the IDT) are written once by the boot CPU before the other
// CPUs start; per-CPU elements are written by their own CPU only, as documented above,
// so no data races are possible.
unsafe impl<T> Sync for BootCell<T> {}

impl<T> BootCell<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    pub const fn get(&self) -> *mut T {
        self.0.get()
    }
}
