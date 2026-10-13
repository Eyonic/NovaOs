# Setup

## Arch Linux / CachyOS (verified)

```sh
sudo pacman -S --needed rustup qemu-system-x86 qemu-ui-gtk edk2-ovmf mtools util-linux limine
rustup default stable        # any toolchain; kernel/rust-toolchain.toml auto-installs 1.99.0
scripts/check-prereqs.sh
```

Do not run `pacman -Sy <pkg>` (partial upgrade). Either use the command above or do a full `-Syu` first.

The `limine` package only places files under `/usr/share/limine`. Nova copies
`BOOTX64.EFI` from there into its own image. **Never** run `limine bios-install`
or similar against a real disk for this project. On CachyOS, Limine is often the
host's own bootloader, and the demo must not touch it.

`qemu-ui-gtk` is only needed for the windowed `scripts/run.sh`. The automated test is headless.

## Other distributions (not verified)

You need the same pieces. Point the scripts at them if the paths differ:

| Need | Typical package | Override variable |
|---|---|---|
| QEMU x86-64 | `qemu-system-x86` | - |
| OVMF firmware (4M split CODE/VARS) | `ovmf` / `edk2-ovmf` | `OVMF_CODE`, `OVMF_VARS_TEMPLATE` |
| Limine UEFI binary | distro `limine` package, or the official binary release from limine-bootloader.org | `LIMINE_EFI` |
| mtools, sfdisk | `mtools`, `util-linux`/`fdisk` | - |

Example (Debian/Ubuntu paths):
`OVMF_CODE=/usr/share/OVMF/OVMF_CODE_4M.fd OVMF_VARS_TEMPLATE=/usr/share/OVMF/OVMF_VARS_4M.fd scripts/test.sh`

Limine base revision 3 is requested. Any Limine release from 8.x onward should work, but only 12.9.0 has been tested.

## KVM

If `/dev/kvm` is readable and writable by your user, QEMU uses hardware acceleration. Otherwise
it falls back to TCG emulation (slower, still boots in a few seconds). Force TCG with `NOVA_NO_KVM=1`.
