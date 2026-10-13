# Architecture (milestone 0.1)

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

* Target `x86_64-unknown-none` (stable Rust, soft-float, no red zone), `-C code-model=kernel`,
  static relocation model. Linked at `0xffffffff80000000` by `kernel/linker.ld`.
* `src/limine.rs` implements only what's needed: the base-revision tag (revision 3), request
  start/end markers, and the bootloader-info request. They live in `.limine_requests`,
  where Limine finds and patches them. The `limine` crate was not used because current
  releases require nightly Rust.
* `src/serial.rs` is a polled 16550 UART driver on COM1 with `print!`/`println!` macros.
* `src/main.rs`: `kmain` initialises serial, prints the banner and status, verifies the base
  revision (panics otherwise), then halts in `cli; hlt`. The panic handler prints
  `!!! KERNEL PANIC !!!` plus location and message, then halts.

### `unsafe` inventory

| Where | Why |
|---|---|
| `serial.rs` `outb`/`inb` + callers | x86 port I/O via `asm!`, restricted to COM1 |
| `limine.rs` `unsafe impl Sync` | statics mutated only by the bootloader before entry |
| `limine.rs` volatile reads, `CStr::from_ptr` | reading bootloader-provided data |
| `main.rs` `halt_forever` | `cli; hlt` |

## Testing (`scripts/test.sh`)

The kernel never exits, so the test is an external watchdog: start QEMU headless with
`-serial file:...`, poll the log for exact lines, and decide pass, fail, timeout, panic, or
QEMU-exited. QEMU is then stopped with SIGTERM (SIGKILL after 5 s), and the script verifies the
process is gone. Compilation success alone never counts as a pass.

## Not included (by design, for this milestone)

No interrupts/IDT, GDT of its own, memory management, framebuffer output, filesystems,
networking, SMP, or user space. The QEMU window shows only firmware and bootloader output.
All kernel output goes to serial.
