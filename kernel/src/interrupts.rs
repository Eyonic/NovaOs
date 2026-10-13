//! IDT, exception entry stubs, and the exception reporter.
//!
//! Every exception vector 0-31 gets a gate. Nova does not attempt recovery: the
//! handler prints a report over serial and halts the CPU.
//!
//! References: Intel SDM Vol. 3A, chapter "Interrupt and Exception Handling"
//! (exception/interrupt vector table, "Error Code", "64-Bit Mode IDT", "64-Bit
//! Mode Stack Frame", "Interrupt Stack Table"); AMD APM Vol. 2, chapter
//! "Exceptions and Interrupts" (#VC, #SX). Cross-checked against the vector and
//! error-code table of the rust-osdev `x86_64` crate (src/structures/idt.rs).
//!
//! # Entry path invariants
//!
//! On an exception in 64-bit mode the CPU aligns RSP down to 16 bytes (or switches
//! to the IST stack), then pushes SS, RSP, RFLAGS, CS, RIP, and, for some vectors
//! only, an error code. To give the Rust side one frame layout, each stub for a
//! vector *without* an error code pushes a dummy 0 first. Every stub then pushes its
//! vector number and jumps to `exception_common`, which saves all 15 GPRs and calls
//! `exception_dispatch(&ExceptionFrame)`.
//!
//! Stack alignment at the `call`: 16-aligned base + 6 qwords (CPU frame incl. error
//! code) + 1 (vector) + 15 (GPRs) = 22 qwords = 176 bytes, a multiple of 16, as the
//! SysV ABI requires. The target disables the red zone and SSE, so interrupting
//! Rust code cannot corrupt red-zone data and there is no FPU state to save.
//!
//! Gates are interrupt gates (IF cleared on entry) at DPL 0. Software `int n` on
//! an error-code vector would produce a frame without an error code; nothing in
//! Nova does that, and with no user mode nothing else can.

use core::arch::{asm, naked_asm};
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::bootcell::BootCell;
use crate::gdt;
use crate::println;

/// Register state at the time of the exception, as laid out on the stack by
/// `exception_common` (lowest address first).
#[repr(C)]
pub struct ExceptionFrame {
    rax: u64, rbx: u64, rcx: u64, rdx: u64, rsi: u64, rdi: u64, rbp: u64,
    r8: u64, r9: u64, r10: u64, r11: u64, r12: u64, r13: u64, r14: u64, r15: u64,
    vector: u64,
    /// CPU-pushed error code, or the stub's dummy 0 for vectors without one.
    error_code: u64,
    // Pushed by the CPU:
    rip: u64, cs: u64, rflags: u64, rsp: u64, ss: u64,
}

#[derive(Clone, Copy, PartialEq)]
enum ErrorCode { None, Selector, PageFault, DoubleFault, Other }

struct ExceptionInfo {
    mnemonic: &'static str,
    name: &'static str,
    class: &'static str,
    error_code: ErrorCode,
}

/// Saves the GPRs, calls the Rust dispatcher, and never returns.
#[unsafe(naked)]
unsafe extern "C" fn exception_common() {
    naked_asm!(
        "push r15", "push r14", "push r13", "push r12", "push r11", "push r10", "push r9", "push r8",
        "push rbp", "push rdi", "push rsi", "push rdx", "push rcx", "push rbx", "push rax",
        "cld",            // SysV ABI requires DF=0 at calls
        "mov rdi, rsp",   // &ExceptionFrame
        "call {dispatch}",
        "2:", "cli", "hlt", "jmp 2b", // unreachable: dispatch returns `!`
        dispatch = sym exception_dispatch,
    )
}

/// Generates one entry stub per vector plus the matching info table, so the
/// "does this vector push an error code" fact has a single source of truth.
macro_rules! exceptions {
    ($($vector:literal $stub:ident $kind:ident $mnemonic:literal $name:literal $class:literal;)*) => {
        $( exceptions!(@stub $stub $vector $kind); )*
        const STUBS: [unsafe extern "C" fn(); 32] = [$($stub),*];
        const INFO: [ExceptionInfo; 32] = [$(ExceptionInfo {
            mnemonic: $mnemonic, name: $name, class: $class, error_code: exceptions!(@kind $kind),
        }),*];
    };
    (@stub $stub:ident $vector:literal none) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $stub() {
            naked_asm!("push 0", "push {v}", "jmp {common}", v = const $vector, common = sym exception_common)
        }
    };
    (@stub $stub:ident $vector:literal $kind:ident) => {
        #[unsafe(naked)]
        unsafe extern "C" fn $stub() {
            naked_asm!("push {v}", "jmp {common}", v = const $vector, common = sym exception_common)
        }
    };
    (@kind none) => { ErrorCode::None };
    (@kind selector) => { ErrorCode::Selector };
    (@kind page) => { ErrorCode::PageFault };
    (@kind double) => { ErrorCode::DoubleFault };
    (@kind other) => { ErrorCode::Other };
}

// vector, stub, error code kind, mnemonic, name, class (SDM Vol. 3A exception table; AMD APM Vol. 2)
exceptions! {
    0  stub_00 none     "#DE" "Divide Error"                 "fault";
    1  stub_01 none     "#DB" "Debug"                        "fault/trap";
    2  stub_02 none     "NMI" "Non-Maskable Interrupt"       "interrupt";
    3  stub_03 none     "#BP" "Breakpoint"                   "trap";
    4  stub_04 none     "#OF" "Overflow"                     "trap";
    5  stub_05 none     "#BR" "BOUND Range Exceeded"         "fault";
    6  stub_06 none     "#UD" "Invalid Opcode"               "fault";
    7  stub_07 none     "#NM" "Device Not Available"         "fault";
    8  stub_08 double   "#DF" "Double Fault"                 "abort";
    9  stub_09 none     "-"   "Coprocessor Segment Overrun"  "fault";
    10 stub_10 selector "#TS" "Invalid TSS"                  "fault";
    11 stub_11 selector "#NP" "Segment Not Present"          "fault";
    12 stub_12 selector "#SS" "Stack-Segment Fault"          "fault";
    13 stub_13 selector "#GP" "General Protection"           "fault";
    14 stub_14 page     "#PF" "Page Fault"                   "fault";
    15 stub_15 none     "-"   "Reserved"                     "-";
    16 stub_16 none     "#MF" "x87 Floating-Point Error"     "fault";
    17 stub_17 other    "#AC" "Alignment Check"              "fault";
    18 stub_18 none     "#MC" "Machine Check"                "abort";
    19 stub_19 none     "#XM" "SIMD Floating-Point"          "fault";
    20 stub_20 none     "#VE" "Virtualization Exception"     "fault";
    21 stub_21 other    "#CP" "Control Protection"           "fault";
    22 stub_22 none     "-"   "Reserved"                     "-";
    23 stub_23 none     "-"   "Reserved"                     "-";
    24 stub_24 none     "-"   "Reserved"                     "-";
    25 stub_25 none     "-"   "Reserved"                     "-";
    26 stub_26 none     "-"   "Reserved"                     "-";
    27 stub_27 none     "-"   "Reserved"                     "-";
    28 stub_28 none     "#HV" "Hypervisor Injection"         "-";
    29 stub_29 other    "#VC" "VMM Communication"            "fault";
    30 stub_30 other    "#SX" "Security Exception"           "-";
    31 stub_31 none     "-"   "Reserved"                     "-";
}

/// 64-bit IDT gate descriptor (SDM Vol. 3A "64-Bit Mode IDT" gate format).
#[repr(C)]
#[derive(Clone, Copy)]
struct Gate {
    offset_low: u16,
    selector: u16,
    ist: u8,        // bits 0-2: IST index, 0 = use current stack
    attributes: u8, // P | DPL | 0 | type
    offset_mid: u16,
    offset_high: u32,
    _reserved: u32,
}

impl Gate {
    const MISSING: Self = Self { offset_low: 0, selector: 0, ist: 0, attributes: 0, offset_mid: 0, offset_high: 0, _reserved: 0 };

    fn new(handler: u64, ist: u8) -> Self {
        Self {
            offset_low: handler as u16,
            selector: gdt::KERNEL_CODE,
            ist,
            attributes: 0x8E, // present, DPL 0, type 0xE = 64-bit interrupt gate (clears IF)
            offset_mid: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            _reserved: 0,
        }
    }
}

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Full 256-entry table: vectors >= 32 stay not-present, so a stray interrupt
/// raises #NP (error code names the IDT vector) instead of reading garbage.
static IDT: BootCell<[Gate; 256]> = BootCell::new([Gate::MISSING; 256]);

/// Nesting depth of exception handling. 0 = not handling an exception.
static DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Self-test hook: when set, the reporter deliberately faults once (see selftest.rs).
pub static FAULT_INSIDE_HANDLER: AtomicBool = AtomicBool::new(false);

/// Populate and load the IDT. Requires `gdt::init` first (gates use its CS).
pub fn init() {
    let idt = IDT.get();
    for (vector, stub) in STUBS.iter().enumerate() {
        let ist = if vector == 8 { gdt::DOUBLE_FAULT_IST } else { 0 };
        // SAFETY: single-threaded init before `lidt`; index < 32 < 256.
        unsafe { (*idt)[vector] = Gate::new(*stub as usize as u64, ist) };
    }
    let pointer = DescriptorTablePointer { limit: (size_of::<[Gate; 256]>() - 1) as u16, base: idt as u64 };
    // SAFETY: the IDT is 'static, fully initialised, and every present gate points
    // at a stub defined above.
    unsafe { asm!("lidt [{}]", in(reg) &pointer, options(readonly, nostack, preserves_flags)) };
}

pub const fn installed_vectors() -> usize {
    STUBS.len()
}

fn read_cr2() -> u64 {
    let value: u64;
    // SAFETY: reading CR2 has no side effects at CPL 0.
    unsafe { asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags)) };
    value
}

/// Called from `exception_common` with a pointer to the saved state. Never returns.
extern "C" fn exception_dispatch(frame: &ExceptionFrame) -> ! {
    let cr2 = read_cr2(); // first: a nested #PF would overwrite it
    let info = &INFO[frame.vector as usize & 31];

    match DEPTH.fetch_add(1, Ordering::SeqCst) {
        0 => report(frame, info, cr2),
        // The reporter itself faulted. Print one short line and stop.
        1 => {
            println!();
            println!("!!! NESTED EXCEPTION: {} {} (vector {}) while reporting an exception", info.mnemonic, info.name, frame.vector);
            println!("[nova] system halted: unrecoverable exception");
        }
        // Even the one-line report faulted; printing is not safe any more.
        _ => {}
    }
    crate::halt_forever()
}

fn report(f: &ExceptionFrame, info: &ExceptionInfo, cr2: u64) {
    println!();
    println!("!!! CPU EXCEPTION !!!");
    println!("exception: {} {} (vector {}, {})", info.mnemonic, info.name, f.vector, info.class);

    if FAULT_INSIDE_HANDLER.swap(false, Ordering::SeqCst) {
        // SAFETY: deliberate #UD for the nested-exception self-test.
        unsafe { asm!("ud2", options(nomem, nostack)) };
    }

    let e = f.error_code;
    match info.error_code {
        ErrorCode::None => println!("error code: none (not pushed for this vector)"),
        ErrorCode::Other => println!("error code: {e:#018x}"),
        ErrorCode::DoubleFault => println!("error code: {e:#018x} (always zero for #DF)"),
        ErrorCode::Selector => {
            // SDM Vol. 3A "Error Code": bit 0 EXT, bits 1-2 table (IDT if bit 1 set, else GDT/LDT by bit 2), bits 3-15 index.
            let table = if e & 0b10 != 0 { "IDT" } else if e & 0b100 != 0 { "LDT" } else { "GDT" };
            let ext = if e & 1 != 0 { ", external event" } else { "" };
            if e == 0 {
                println!("error code: {e:#018x} (not selector-related)");
            } else {
                println!("error code: {e:#018x} (selector index {}, {table}{ext})", (e >> 3) & 0x1FFF);
            }
        }
        ErrorCode::PageFault => {
            // SDM Vol. 3A, chapter "Paging", page-fault error code.
            println!(
                "error code: {e:#018x} ({}, {}, {}{}{}{})",
                if e & 1 != 0 { "protection violation" } else { "not-present" },
                if e & 2 != 0 { "write" } else { "read" },
                if e & 4 != 0 { "user" } else { "supervisor" },
                if e & 8 != 0 { ", reserved bit set" } else { "" },
                if e & 16 != 0 { ", instruction fetch" } else { "" },
                if e & 32 != 0 { ", protection key" } else { "" },
            );
            println!("fault address (CR2): {cr2:#018x}");
        }
    }

    println!("RIP: {:#018x}  CS: {:#06x}  RFLAGS: {:#018x}", f.rip, f.cs, f.rflags);
    println!("RSP: {:#018x}  SS: {:#06x}", f.rsp, f.ss);
    if info.error_code == ErrorCode::DoubleFault {
        println!("note: saved RIP is architecturally undefined for #DF");
    }
    println!("RAX={:#018x} RBX={:#018x} RCX={:#018x} RDX={:#018x}", f.rax, f.rbx, f.rcx, f.rdx);
    println!("RSI={:#018x} RDI={:#018x} RBP={:#018x} R8 ={:#018x}", f.rsi, f.rdi, f.rbp, f.r8);
    println!("R9 ={:#018x} R10={:#018x} R11={:#018x} R12={:#018x}", f.r9, f.r10, f.r11, f.r12);
    println!("R13={:#018x} R14={:#018x} R15={:#018x}", f.r13, f.r14, f.r15);
    println!("[nova] system halted: unrecoverable exception");
}
