# Project Nova

A minimal, real, bootable x86-64 operating-system kernel written in Rust (`no_std`, `no_main`),
booted via **UEFI → Limine → Nova** inside a **QEMU/OVMF virtual machine**. This is a feasibility
demo: it proves the edit → build → boot → verify loop, not an OS feature set.

Everything runs in a VM. Nothing here writes to your real disks, ESP, or host bootloader;
the "disk" is the file `build/nova.img`.

## What it does

On boot, the kernel prints to the emulated serial port (COM1):

```
PROJECT NOVA KERNEL 0.1 - IDE TEST PASSED
[nova] arch: x86_64 (long mode, higher-half kernel)
[nova] boot protocol: Limine base revision 3
[nova] bootloader: Limine 12.9.0
[nova] status: kernel main path reached, init OK
[nova] entering idle loop
```

and then halts the CPU in a `cli; hlt` loop. Panics are reported over serial.

## Repository layout

| Path | Purpose |
|---|---|
| `kernel/` | Rust kernel crate (`src/main.rs` entry + panic handler, `src/serial.rs` UART driver, `src/limine.rs` boot-protocol structs, `linker.ld`, pinned `rust-toolchain.toml`) |
| `boot/limine.conf` | Limine bootloader menu/config |
| `scripts/` | `check-prereqs.sh`, `build.sh`, `run.sh`, `test.sh`, `clean.sh` (`common.sh` holds shared paths) |
| `docs/` | [`setup.md`](docs/setup.md) (installing prerequisites), [`architecture.md`](docs/architecture.md) (how it boots) |
| `build/` | Generated: disk image, OVMF vars copy, logs (git-ignored) |

## Commands

Run from the repository root (scripts are bash; they work from any shell):

| Task | Command |
|---|---|
| Check prerequisites | `scripts/check-prereqs.sh` |
| Build kernel + bootable image | `scripts/build.sh` |
| Boot with a VM window (serial in terminal) | `scripts/run.sh` |
| Boot headless (serial in terminal) | `scripts/run.sh --headless` (quit: `Ctrl-A` then `X`) |
| Automated boot test | `scripts/test.sh` |
| Test with a different expected banner | `scripts/test.sh --expect "SOME TEXT"` |
| Test the panic handler | `scripts/test.sh --panic` |
| Clean generated artifacts | `scripts/clean.sh` |

`scripts/test.sh` builds the image, boots QEMU headless with serial captured to
`build/logs/test-<timestamp>-<mode>/serial.log`, waits (default 30 s, `TIMEOUT=` to change) for the
banner **and** status line as exact lines, then terminates QEMU. Exit code 0 = pass, 1 = fail
(build failure, timeout, kernel panic, or QEMU died). `build/logs/test-latest` links the last run.

Environment overrides: `LIMINE_EFI`, `OVMF_CODE`, `OVMF_VARS_TEMPLATE` (file locations),
`NOVA_NO_KVM=1` (force software emulation), `PROFILE=dev` (debug build), `TIMEOUT`.

## Try the edit loop

1. Edit `BANNER` in `kernel/src/main.rs`.
2. Run `scripts/test.sh --expect "<your new banner>"`. It passes only if the VM prints it.
3. To make it the default expectation, update `EXPECTED_BANNER` in `scripts/test.sh`.

## Tested versions

Verified on 2026-10-13 on CachyOS (Linux 7.2.3, x86_64, KVM):

| Tool | Version | Source |
|---|---|---|
| Rust | 1.99.0 (pinned in `kernel/rust-toolchain.toml`) | rustup |
| Kernel target | `x86_64-unknown-none` | rustup |
| QEMU | 11.1.2 (`qemu-system-x86`, `qemu-ui-gtk`) | pacman |
| OVMF | edk2-ovmf 202608 | pacman |
| Limine | 12.9.0 (`BOOTX64.EFI`, sha256 `f24efeec…892d629f`) | pacman |
| mtools / sfdisk | system packages | pacman |

Kernel crate dependencies: none.
