# Project Nova

A minimal, real, bootable x86-64 operating-system kernel written in Rust (`no_std`, `no_main`),
booted via **UEFI → Limine → Nova** inside a **QEMU/OVMF virtual machine**. This is a feasibility
demo: it proves the edit → build → boot → verify loop, not an OS feature set.

**Current milestone: 0.2, CPU exception handling.** Nova loads its own GDT/TSS/IDT and
reports every CPU exception (vectors 0-31) over serial, then halts. It does not attempt recovery.

Everything runs in a VM. Nothing here writes to your real disks, ESP, or host bootloader;
the "disk" is the file `build/nova.img`.

## What it does

On boot, the kernel prints to the emulated serial port (COM1):

```
PROJECT NOVA KERNEL 0.2
[nova] arch: x86_64 (long mode, higher-half kernel)
[nova] boot protocol: Limine base revision 3
[nova] bootloader: Limine 12.9.0
[nova] gdt: loaded (code 0x08, data 0x10, tss 0x18, #DF on IST1)
[nova] idt: loaded, 32 exception vectors installed
[nova] cmdline: ""
[nova] status: kernel main path reached, init OK
[nova] entering idle loop
```

and then halts the CPU in a `cli; hlt` loop. A CPU exception produces a report like:

```
!!! CPU EXCEPTION !!!
exception: #PF Page Fault (vector 14, fault)
error code: 0x0000000000000002 (not-present, write, supervisor)
fault address (CR2): 0x00000deadbeef000
RIP: 0xffffffff8000101f  CS: 0x0008  RFLAGS: 0x0000000000010086
RSP: 0xffff80000bfa4f18  SS: 0x0010
RAX=... (all general-purpose registers)
[nova] system halted: unrecoverable exception
```

Panics print `!!! KERNEL PANIC !!!` with location and message, then halt.

## Repository layout

| Path | Purpose |
|---|---|
| `kernel/` | Rust kernel crate: `src/main.rs` (entry, panic handler), `src/gdt.rs` (GDT + TSS/IST), `src/interrupts.rs` (IDT, entry stubs, exception reports), `src/selftest.rs` (deliberate faults for tests), `src/serial.rs` (UART), `src/limine.rs` (boot-protocol structs), `src/bootcell.rs`, `linker.ld`, pinned `rust-toolchain.toml` |
| `boot/limine.conf` | Limine bootloader menu/config |
| `scripts/` | `check-prereqs.sh`, `build.sh`, `mkimage.sh`, `run.sh`, `test.sh`, `clean.sh` (`common.sh` holds shared paths) |
| `docs/` | [`setup.md`](docs/setup.md) (installing prerequisites), [`architecture.md`](docs/architecture.md) (boot chain, exception handling, tests) |
| `build/` | Generated: disk image, OVMF vars copy, logs (git-ignored) |

## Commands

Run from the repository root (scripts are bash; they work from any shell):

| Task | Command |
|---|---|
| Check prerequisites | `scripts/check-prereqs.sh` |
| Build kernel + bootable image | `scripts/build.sh` |
| Boot with a VM window (serial in terminal) | `scripts/run.sh` |
| Boot headless (serial in terminal) | `scripts/run.sh --headless` (quit: `Ctrl-A` then `X`) |
| Automated boot (regression) test | `scripts/test.sh` |
| **All tests** (boot, panic, 7 exception cases) | `scripts/test.sh --all` |
| One test case / list cases | `scripts/test.sh --case pf` / `scripts/test.sh --list` |
| Test with a different expected banner | `scripts/test.sh --expect "SOME TEXT"` |
| Test the panic handler | `scripts/test.sh --panic` |
| Boot an image with a self-test by hand | `NOVA_CMDLINE="nova.test=gp" scripts/build.sh && scripts/run.sh --headless` |
| Clean generated artifacts | `scripts/clean.sh` |

`scripts/test.sh` builds the kernel once, then for each case packs an image whose `limine.conf`
passes `nova.test=<case>` on the kernel command line. It boots QEMU headless with serial captured to
`build/logs/test-<timestamp>/<case>.serial.log` and waits (default 30 s, `TIMEOUT=` to change) for
the case's exact expected lines. It then checks that the VM stays up and silent for 2 s
(controlled halt, no reset), that no forbidden output appeared, and, for fault cases, that the
reported RIP equals the faulting instruction's address announced by the kernel. Exit code 0 = all
passed, 1 = something failed. `build/logs/test-latest` links the last run. Test cases are described
in [`docs/architecture.md`](docs/architecture.md#tests).

Every case boots the **same kernel binary**. Only the command line differs.

Environment overrides: `LIMINE_EFI`, `OVMF_CODE`, `OVMF_VARS_TEMPLATE` (file locations),
`NOVA_NO_KVM=1` (force software emulation), `PROFILE=dev` (debug build), `TIMEOUT`,
`NOVA_CMDLINE` (kernel command line baked into `build/nova.img` by `build.sh`).

## Try the edit loop

1. Edit `BANNER` in `kernel/src/main.rs`.
2. Run `scripts/test.sh --expect "<your new banner>"`. It passes only if the VM prints it.
3. To make it the default expectation, update `EXPECTED_BANNER` in `scripts/test.sh`.

## Tested versions

Verified on 2026-10-13 on CachyOS (Linux 7.2.3, x86_64, KVM, and with `NOVA_NO_KVM=1`):

| Tool | Version | Source |
|---|---|---|
| Rust | 1.99.0 (pinned in `kernel/rust-toolchain.toml`) | rustup |
| Kernel target | `x86_64-unknown-none` | rustup |
| QEMU | 11.1.2 (`qemu-system-x86`, `qemu-ui-gtk`) | pacman |
| OVMF | edk2-ovmf 202608 | pacman |
| Limine | 12.9.0 (`BOOTX64.EFI`, sha256 `f24efeec…892d629f`) | pacman |
| mtools / sfdisk | system packages | pacman |

Kernel crate dependencies: none.
