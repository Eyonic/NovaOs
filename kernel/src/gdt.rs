//! Global Descriptor Table and Task State Segment.
//!
//! Limine's GDT lives in bootloader-reclaimable memory, so Nova installs its own.
//! In long mode segmentation is mostly vestigial: the GDT provides the kernel
//! CS/SS selectors and the TSS descriptor. The TSS matters for its Interrupt Stack
//! Table (IST): the double-fault gate switches to a dedicated, known-good stack,
//! so a fault caused by a bad kernel stack can still be reported.
//!
//! References: Intel SDM Vol. 3A, chapters "Protected-Mode Memory Management"
//! (segment descriptors), "Task Management" (64-bit TSS layout), "Interrupt and
//! Exception Handling" (Interrupt Stack Table); AMD APM Vol. 2 (long-mode TSS).
//!
//! Every CPU has its own GDT (the 32-bit TLS entries belong to the thread running there)
//! and its own TSS (kernel stack, #DF stack). CPU n's TSS descriptor sits at entry
//! 16 + 2n, so `str` (the TSS selector) tells code which CPU it runs on (smp::id).

use core::arch::asm;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::bootcell::BootCell;
use crate::smp::MAX_CPUS;

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
/// User segments in Linux's layout, which SYSRET requires: STAR[63:48] = the 32-bit code
/// segment, + 8 = data (SS), + 16 = 64-bit code.
pub const USER_CODE32: u16 = 0x18 | 3;
pub const USER_DATA: u16 = 0x20 | 3;
pub const USER_CODE: u16 = 0x28 | 3;
/// Selector of CPU 0's TSS; CPU n's is this + 16 * n.
pub const TSS_SELECTOR_BASE: u16 = (TSS_FIRST * 8) as u16;
const TSS_FIRST: usize = 16;
/// Thread-local storage descriptors for 32-bit programs (set_thread_area), entries 12-14
/// as on Linux: glibc's i386 TLS lives in %gs = 0x63.
pub const TLS_FIRST: usize = 12;
pub const TLS_ENTRIES: usize = 3;

/// IST slot used by the #DF gate (1-based, as encoded in an IDT gate; 0 = no IST).
pub const DOUBLE_FAULT_IST: u8 = 1;
const IST_STACK_SIZE: usize = 16 * 1024;

/// 64-bit TSS (SDM Vol. 3A "Task Management"). 104 bytes, no I/O permission bitmap.
#[repr(C, packed)]
struct Tss {
    _reserved0: u32,
    rsp: [u64; 3],
    _reserved1: u64,
    ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    iomap_base: u16,
}
const _: () = assert!(size_of::<Tss>() == 104);

#[repr(C, align(16))]
struct Stack([u8; IST_STACK_SIZE]);

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

// Flat 64-bit descriptors: base 0, limit 0xFFFFF, G=1.
//   code: access 0x9A (present, DPL0, code, readable), flags L=1 (64-bit)
//   data: access 0x92 (present, DPL0, data, writable)
const CODE_DESCRIPTOR: u64 = 0x00AF_9A00_0000_FFFF;
const DATA_DESCRIPTOR: u64 = 0x00CF_9200_0000_FFFF;
// Same with DPL 3 (access 0xF2 / 0xFA); the 32-bit code segment has D=1, L=0
// (compatibility mode).
const USER_DATA_DESCRIPTOR: u64 = 0x00CF_F200_0000_FFFF;
const USER_CODE_DESCRIPTOR: u64 = 0x00AF_FA00_0000_FFFF;
const USER_CODE32_DESCRIPTOR: u64 = 0x00CF_FA00_0000_FFFF;
/// Access byte for an available 64-bit TSS: present, DPL0, type 0b1001.
const TSS_ACCESS: u64 = 0x89;

/// null, kernel code, kernel data, user code 32, user data, user code 64, unused, TLS 12-14,
/// unused, then one TSS slot pair (16-byte system descriptor) per CPU.
const GDT_ENTRIES: usize = TSS_FIRST + 2 * MAX_CPUS;
const _: () = assert!(TLS_FIRST + TLS_ENTRIES <= TSS_FIRST);
static GDT: BootCell<[[u64; GDT_ENTRIES]; MAX_CPUS]> = BootCell::new([[0; GDT_ENTRIES]; MAX_CPUS]);
const EMPTY_TSS: Tss = Tss { _reserved0: 0, rsp: [0; 3], _reserved1: 0, ist: [0; 7], _reserved2: 0, _reserved3: 0, iomap_base: 0 };
static TSS: BootCell<[Tss; MAX_CPUS]> = BootCell::new([EMPTY_TSS; MAX_CPUS]);
static DOUBLE_FAULT_STACK: BootCell<[Stack; MAX_CPUS]> = BootCell::new([const { Stack([0; IST_STACK_SIZE]) }; MAX_CPUS]);
static LOADED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// The boot CPU's GDT and TSS (see [`init_cpu`]).
pub fn init() {
    init_cpu(0);
}

/// Build and load CPU `cpu`'s GDT and TSS, then reload every segment register.
///
/// Runs once per CPU, on that CPU, with interrupts disabled (Limine starts the BSP and the
/// APs with IF=0). Each CPU only ever writes its own tables.
pub fn init_cpu(cpu: usize) {
    // `ltr` on an already-busy TSS would #GP; make a second call a clear panic instead.
    assert!(!LOADED[cpu].swap(true, Ordering::SeqCst), "gdt::init_cpu called twice");

    // SAFETY: in bounds; the stack is only used by this CPU's #DF gate.
    let ist_top = unsafe { (&raw mut (*DOUBLE_FAULT_STACK.get())[cpu]) as u64 } + IST_STACK_SIZE as u64; // 16-byte aligned
    // SAFETY: in bounds; only this CPU touches its TSS.
    let tss = unsafe { &raw mut (*TSS.get())[cpu] };
    // SAFETY: init runs once before the TSS is loaded; nothing else references it yet.
    unsafe {
        tss.write(Tss {
            _reserved0: 0,
            rsp: [0; 3], // RSP0: set on every switch to a user thread (set_kernel_stack)
            _reserved1: 0,
            ist: [ist_top, 0, 0, 0, 0, 0, 0], // IST1 = #DF stack
            _reserved2: 0,
            _reserved3: 0,
            iomap_base: size_of::<Tss>() as u16, // beyond the limit = no I/O bitmap
        });
    }

    let base = tss as u64;
    let limit = (size_of::<Tss>() - 1) as u64;
    let tss_low = (limit & 0xFFFF)
        | ((base & 0xFF_FFFF) << 16)
        | (TSS_ACCESS << 40)
        | (((limit >> 16) & 0xF) << 48)
        | (((base >> 24) & 0xFF) << 56);
    let tss_high = base >> 32;

    // SAFETY: in bounds; this CPU's GDT is not loaded yet.
    let gdt = unsafe { &raw mut (*GDT.get())[cpu] };
    // SAFETY: as above.
    unsafe {
        let mut table = [0u64; GDT_ENTRIES];
        table[..6].copy_from_slice(&[0, CODE_DESCRIPTOR, DATA_DESCRIPTOR, USER_CODE32_DESCRIPTOR, USER_DATA_DESCRIPTOR, USER_CODE_DESCRIPTOR]);
        table[TSS_FIRST + 2 * cpu] = tss_low;
        table[TSS_FIRST + 2 * cpu + 1] = tss_high;
        gdt.write(table);
    }
    let tss_selector = TSS_SELECTOR_BASE + 16 * cpu as u16;

    let pointer = DescriptorTablePointer { limit: (size_of::<[u64; GDT_ENTRIES]>() - 1) as u16, base: gdt as u64 };

    // SAFETY: the GDT is fully built and 'static. After `lgdt`, CS still caches
    // Limine's descriptor (selector 0x28), so it is reloaded with a far return
    // (push new CS, push target RIP, retfq). Then the data segments get our data
    // selector, FS/GS get null (unused), and the TSS selector is loaded. Interrupts
    // are disabled, so nothing can observe the intermediate state.
    unsafe {
        asm!("lgdt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags));
        asm!(
            "push {code}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            "mov fs, {null:x}",
            "mov gs, {null:x}",
            "ltr {tss:x}",
            code = in(reg) KERNEL_CODE as u64,
            data = in(reg) KERNEL_DATA as u64,
            null = in(reg) 0u64,
            tss = in(reg) tss_selector as u64,
            tmp = out(reg) _, // not lateout: must not share a register with the inputs
            options(preserves_flags),
        );
    }
}

/// Stack this CPU switches to when an interrupt or exception arrives in user mode
/// (TSS.RSP0). Set to the running task's kernel stack on every task switch.
pub fn set_kernel_stack(top: u64) {
    let cpu = crate::smp::id();
    // SAFETY: RSP0 is read by the CPU only on a privilege change, which cannot
    // happen while this (kernel) code runs; the TSS stays loaded. Only this CPU
    // writes its own TSS.
    unsafe { core::ptr::write_unaligned(core::ptr::addr_of_mut!((*TSS.get())[cpu].rsp[0]), top) };
}

/// Install the running thread's TLS descriptors in this CPU's GDT (entries 12-14; 0 = not
/// present).
pub fn set_tls(entries: &[u64; TLS_ENTRIES]) {
    let cpu = crate::smp::id();
    // SAFETY: in bounds; only this CPU writes its own GDT.
    let gdt = unsafe { &raw mut (*GDT.get())[cpu] };
    for (i, d) in entries.iter().enumerate() {
        // SAFETY: TLS slots are only used by user mode; the CPU reads a descriptor when a
        // selector is loaded, which happens after this (switch_in reloads %fs/%gs).
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!((*gdt)[TLS_FIRST + i]), *d) };
    }
}
