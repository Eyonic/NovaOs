# Architecture (milestone 0.2)

## Boot chain

```
QEMU q35 ── OVMF (UEFI firmware, pflash)
              └─ boots \EFI\BOOT\BOOTX64.EFI from the GPT ESP in build/nova.img  (= Limine)
                   └─ reads \boot\limine\limine.conf
                        └─ loads \boot\nova-kernel (ELF64), sets up long mode + paging,
                           fills Limine request structs, jumps to kmain()
```

## Disk image (`scripts/build.sh`)

64 MiB raw file → GPT (sfdisk) → one EFI System Partition at 1 MiB, FAT32 (mformat).
mtools writes the filesystem directly inside the file at offset 1 MiB, so there's no
loop device, no root, and no mounting.

```
ESP:/EFI/BOOT/BOOTX64.EFI      Limine UEFI executable (copied from the host package)
ESP:/boot/limine/limine.conf   boot/limine.conf
ESP:/boot/nova-kernel          kernel ELF
```

OVMF's variable store is copied to `build/OVMF_VARS.4m.fd`, so the system copy stays read-only.

## Kernel

* Target `x86_64-unknown-none` (stable Rust, soft-float, **no red zone**), `-C code-model=kernel`,
  static relocation model. Linked at `0xffffffff80000000` by `kernel/linker.ld`.
* `src/limine.rs` implements only what's needed: the base-revision tag (revision 3), request
  start/end markers, and the bootloader-info and executable-cmdline requests. They live in
  `.limine_requests`, where Limine finds and patches them. The `limine` crate was not used
  because current releases require nightly Rust.
* `src/serial.rs` is a polled 16550 UART driver on COM1 with `print!`/`println!` macros. It is
  lock-free, so reporting from an exception handler can't deadlock on it.
* `src/main.rs` (`kmain`) runs serial init, then banner, then the base-revision check, then
  `gdt::init`, then `interrupts::init`, then the status line, then the optional self-test, then
  `cli; hlt` forever.

Boot order matters. Limine enters with IF=0, its own GDT (CS=`0x28`) in *bootloader-reclaimable*
memory, and an undefined IDT (Limine protocol, "x86-64 entry machine state"). Nova replaces the GDT
before the IDT, because the IDT gates reference Nova's code selector.

## Exception handling (0.2)

### GDT and TSS (`src/gdt.rs`)

| Selector | Descriptor |
|---|---|
| `0x00` | null |
| `0x08` | 64-bit kernel code (`0x00AF9A000000FFFF`) |
| `0x10` | kernel data (`0x00CF92000000FFFF`) |
| `0x18` | 64-bit TSS (16-byte system descriptor, type 0x9) |

The TSS's only purpose is **IST1**: a dedicated 16 KiB stack for the #DF gate. If an exception
happens because RSP is bad, the CPU can't push the exception frame, which escalates to #DF. #DF
then switches to IST1 and can still report. Without that, the CPU would triple-fault and reset.

After `lgdt`, CS is reloaded with a far return (`push 0x08; push rip; retfq`). DS/ES/SS get
`0x10`, FS/GS get null, and `ltr 0x18` loads the TSS.

### IDT and entry stubs (`src/interrupts.rs`)

All 32 exception vectors get a 64-bit **interrupt gate** (attribute `0x8E`: present, DPL 0,
type 0xE, IF cleared on entry) pointing at a per-vector naked-function stub. #DF uses IST1. Vectors
32-255 are not present, so a stray interrupt raises #NP, which is reported.

The CPU pushes an error code only for some vectors. Nova uses one table (the `exceptions!` macro) as
the single source of truth for which ones:

| Error code pushed | Vectors |
|---|---|
| yes | 8 #DF (always 0), 10 #TS, 11 #NP, 12 #SS, 13 #GP (selector format), 14 #PF (page-fault format), 17 #AC, 21 #CP, 29 #VC, 30 #SX |
| no  | all others: the stub pushes a dummy 0 so the frame layout is identical |

Stack at the Rust call (lowest address first), the `ExceptionFrame` struct:

```
rax rbx rcx rdx rsi rdi rbp r8..r15   <- pushed by exception_common
vector                                <- pushed by the stub
error code (real or dummy 0)          <- CPU or stub
rip cs rflags rsp ss                  <- CPU
```

Invariants of the entry path:

* The CPU aligns RSP to 16 before pushing (or uses the 16-aligned IST top). 6 + 1 + 15 qwords =
  176 bytes, so RSP is 16-aligned at the `call` as the SysV ABI requires. `cld` sets DF=0.
* No red zone and no SSE in the target, so interrupting Rust code destroys no live state.
* The handler never returns (`-> !`). The instruction after the `call` is a `cli; hlt` loop,
  just in case.
* Software `int n` on an error-code vector would produce a frame without an error code. Nova
  never does this, and there is no user mode that could.

### Reporting and halting

`exception_dispatch` reads CR2 first (a nested #PF would overwrite it), then increments a nesting
counter:

| Depth | Behavior |
|---|---|
| 0 | full report: name, vector, class (fault/trap/abort), decoded error code, CR2 for #PF, RIP/CS/RFLAGS/RSP/SS, all GPRs, then `[nova] system halted: unrecoverable exception` |
| 1 | the reporter itself faulted: one line `!!! NESTED EXCEPTION: ...` and the halted line |
| ≥2 | halt without printing |

Then `cli; hlt` forever. The panic handler has the same kind of guard against recursive panics.
**No exception is recovered from in 0.2.** #BP is a trap and could be resumed, but it is also
reported and halted.

Reading the report: for faults, RIP points **at** the faulting instruction and RFLAGS.RF (bit 16)
is set. For traps (#BP), RIP points **after** it. For #DF, RIP is architecturally undefined
(printed with a note).

### References

* Intel 64 and IA-32 SDM Vol. 3A: "Interrupt and Exception Handling" (vector table, error code,
  64-bit IDT gates, 64-bit stack frame, IST), "Task Management" (64-bit TSS), "Paging" (#PF error
  code), "Protected-Mode Memory Management" (descriptors).
* AMD64 APM Vol. 2: "Exceptions and Interrupts" (#VC, #SX).
* Limine protocol, `PROTOCOL.md`: x86-64 entry machine state, base revisions, executable cmdline.
* Cross-checked against the rust-osdev `x86_64` crate's `src/structures/idt.rs` (vector and
  error-code table, gate layout, #DF RIP note).

### `unsafe` inventory

| Where | Why |
|---|---|
| `serial.rs` `outb`/`inb` + callers | x86 port I/O via `asm!`, restricted to COM1 |
| `limine.rs` `unsafe impl Sync`, volatile reads, `CStr::from_ptr` | bootloader-written statics and strings |
| `bootcell.rs` `unsafe impl Sync` | CPU tables written only during single-CPU init |
| `gdt.rs` TSS/GDT writes, `lgdt`, far return, segment loads, `ltr` | installing the GDT/TSS |
| `interrupts.rs` naked stubs + `exception_common` | interrupt entry (about 25 instructions total) |
| `interrupts.rs` IDT writes, `lidt`, `mov cr2` | installing the IDT, reading the fault address |
| `selftest.rs` `global_asm!` routines | deliberate faults, only reachable with `nova.test=` |
| `main.rs` `halt_forever` | `cli; hlt` |

## Tests

`scripts/test.sh` is an external watchdog: the kernel never exits, so the script boots QEMU
headless with `-no-reboot` and `-serial file:...`, polls the log, and stops QEMU itself. A triple
fault therefore shows up as "QEMU exited". Compilation success alone never counts as a pass.

All cases run the same kernel binary. `scripts/mkimage.sh` adds `cmdline: nova.test=<case>` to
`limine.conf`, and `src/selftest.rs` dispatches on it.

| Case | Trigger | Verified |
|---|---|---|
| `boot` | none (regression) | banner, GDT/IDT lines, status, idle line; no exception or panic |
| `panic` | `panic!()` | panic header, location, message, halted line |
| `de` | `div ecx` with ECX=0 | #DE, vector 0, "error code: none", RIP == faulting instruction |
| `ud` | `ud2` | #UD, vector 6, no error code, RIP match |
| `gp` | `mov ds, ax` with selector 0x28 (past the GDT limit) | #GP, error code `0x28` decoded as index 5/GDT, RIP match |
| `pf` | write to unmapped `0xdeadbeef000` | #PF, error code `0x2` (not-present, write, supervisor), CR2, RIP match |
| `bp` | `int3` | #BP as a **trap**: RIP == the instruction *after* `int3` |
| `df` | `mov rsp, 0xdeadbeef000; push` | #PF can't be delivered → #DF on IST1, error code 0, RSP shows the bad stack, no #PF report |
| `nested` | #BP whose reporter executes `ud2` | nested-exception line, first report cut short, controlled halt |

Each pass also requires that QEMU is still running and printing nothing 2 s after the last expected
line (controlled halt, no reset or loop). The RIP check compares against the address of a global
label on the faulting instruction, which the kernel prints before triggering. A frame-layout bug
shifts every field and fails the check.

The tests were validated by mutation: declaring #PF as error-code-less makes `pf` fail (the report
shows RIP=`0x2`, CS=the real RIP), and removing IST1 from the #DF gate makes `df` fail (triple
fault, QEMU exits).

## Not included (by design)

No hardware interrupts (PIC/APIC, timer), exception recovery, memory management, framebuffer
output, filesystems, networking, SMP, or user space. The QEMU window shows only firmware and
bootloader output. All kernel output goes to serial.
