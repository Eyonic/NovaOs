//! Interior-mutable statics for CPU-owned tables (GDT, TSS, IDT, IST stack).

use core::cell::UnsafeCell;

/// A static that is written only by the boot CPU during early init, before the
/// CPU is told about it (lgdt/ltr/lidt), with interrupts disabled. After init it
/// is never written from Rust again. The CPU itself may write some of it (the TSS
/// busy bit, the IST stack), which is why it must live in writable memory.
pub struct BootCell<T>(UnsafeCell<T>);

// SAFETY: Nova runs on a single CPU and only mutates BootCells during init, as
// documented above, so no data races are possible.
unsafe impl<T> Sync for BootCell<T> {}

impl<T> BootCell<T> {
    pub const fn new(value: T) -> Self {
        Self(UnsafeCell::new(value))
    }

    pub const fn get(&self) -> *mut T {
        self.0.get()
    }
}
