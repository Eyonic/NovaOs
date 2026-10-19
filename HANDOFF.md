# Handoff: where Nova stands and how to pick it up

Written 2026-10-19 so work can restart later, by a person or a new Claude session.
Read this first, then `README.md`, `docs/linux-drivers.md`, `docs/smp.md` and
`docs/real-hardware.md`.

## The goal

Nova is a Rust x86-64 kernel with a Linux system-call layer (32- and 64-bit programs)
and unmodified Linux drivers on a glue layer (`kernel/linuxkpi/`, lx_emul style). The
end goal is Steam games on the owner's PC:

- Ryzen 7 5800X, 62 GiB of memory
- NVMe drives (SN850X and Samsung 980) and an AMD xHCI USB controller
- Realtek RTL8168 and RTL8125 network chips
- Radeon RX 6800 (Navi 21)

Standing decisions:

- The core stays Rust. Nova is not to become a Linux distro or run on the Linux kernel.
- GPL Linux code stays out of the repo. It is fetched and built only with
  `FEATURES=linux-drivers`.
- Commit and push after each big step, once both test suites pass.
- The owner games on the build host. When they say so, stop every VM, build and agent
  until they release the machine.
- Never break the host: inspect it read-only, and put no Valve or host binaries in the repo.

## State on 2026-10-19

The branch is `linux-driver-layer` at `be695cd`. It is pushed to
`git@github.com:Eyonic/NovaOs.git` and the working tree is clean.

**Tests:** both suites pass 39 of 39 (`scripts/test.sh` and
`FEATURES=linux-drivers scripts/test.sh`).

### Working in QEMU

- SMP: user programs run on every CPU under one big kernel lock (BKL), 4 CPUs by default
  in tests. `nova.cpus=N` limits the CPU count.
- Keyed I/O waits: poll, epoll, wait4, pause and sigsuspend sleep until the object they
  watch changes. Before this they woke at every interrupt.
- vDSO for 64- and 32-bit programs (`clock_gettime` takes about 10 ns), plus
  `__kernel_vsyscall`.
- Steam reaches its sign-in window: web helper at about 62 s, sign-in at about 64–71 s on
  4 CPUs. While idle, Steam holds the BKL about 4% of the time.
- Chromium (multi-process), Xwayland and Wayland, PulseAudio (in the kernel), DRM user
  path (vkms/vgem), media apps, timerfd and inotify.

### Built but never run on real hardware

- `r8169` (RTL8168/RTL8125)
- `amdgpu`: links with 0 unresolved symbols and its firmware is in the image, but QEMU
  cannot emulate the RX 6800
- the native HDA sound driver (tested only in QEMU)
- USB HID on a real xHCI controller

**This is the open item.** The USB-stick test has never been run. VFIO passthrough on the
host is not possible: the GPU shares IOMMU group 15 with other devices.

## Next step: the real-hardware test

1. **The image.** `build/nova-usb.img` (about 790 MB) was built from `be695cd`. It has a
   boot menu, `nova.mode=shell` and `nova.remote=192.168.1.97:5599`, which streams the
   console over TCP to the build host. To rebuild it:

   ```sh
   scripts/fetch-linux.sh      # once
   NOVA_MEDIA=1 NOVA_BOOT_MENU=1 FEATURES=linux-drivers IMAGE=build/nova-usb.img \
     NOVA_CMDLINE="nova.mode=shell nova.remote=192.168.1.97:5599" scripts/build.sh
   ```

   With `NOVA_BOOT_MENU=1`, the build adds `amdgpu.dc=0` and the other menu entries on
   its own.

2. **Start the listener** on the build host before booting. It is not running after a
   reboot:

   ```sh
   mkdir -p build/logs/realhw
   socat -u TCP-LISTEN:5599,bind=192.168.1.97,reuseaddr,fork \
     SYSTEM:'cat >> build/logs/realhw/console-$(date +%Y%m%d-%H%M%S).log' &
   ```

   The host firewall (nft) was never checked, so it may block port 5599. If no log
   arrives, take photos of the screen instead.

3. **Write and boot the stick** as `docs/real-hardware.md` describes: Secure Boot off, then
   pick the stick in the boot menu. The menu entries are:
   - "safe graphics" (the default)
   - full `amdgpu`
   - no GPU driver
   - one CPU

4. **Read the results.** Check `build/logs/realhw/*.log` (or the photos), then fix
   `r8169`, `amdgpu` and HDA in this order:
   - Network first: it is what makes remote logs and further debugging easy.
   - `amdgpu` with `dc=0` next, then the full display path. The desktop cannot take back
     scan-out from amdgpu yet.

## After that

- Steam past sign-in: the library, then a game. This needs a GPU path for Chromium and
  games, so it depends on amdgpu plus Mesa (radeonsi/RADV) in user space.
- Further optimisation, with diminishing returns:
  - Take the BKL off hot paths. Lock-time profile while idle: read 21%, epoll_wait 17%,
    poll 15%, kernel tasks 7%.
  - Per-CPU scheduling.

## How to measure Steam start time

The Steam tools were saved outside the repo in `~/Documents/novaos/devtools/`:

- `serial_probe.py` and `run-steam.sh`: drive the Nova shell over the QEMU serial port.
- `steam-t2.sh <tag>`: times one start and prints `RESULT webhelper=N login=M` and the
  BKL share. It times inside the guest with a shell loop that starts no processes.
- `steam-diag.sh`: dumps Steam's logs from inside the VM.

They expect a Steam image at `build/steam.img`. That image no longer exists; rebuild it
with:

```sh
NOVA_STEAM=1 FEATURES=linux-drivers IMAGE=build/steam.img NOVA_CMDLINE="nova.mode=shell" scripts/build.sh
IMG=steam.img ~/Documents/novaos/devtools/steam-t2.sh run1
```

One trap: Steam rotates `logs/webhelper.txt` when the web helper restarts, so a grep run
late can wrongly report that a milestone never happened.

## Secrets and accounts

None of these values belong in the repo or in chat:

| What | Where |
|---|---|
| DeepSeek API key | `~/.config/nova/deepseek-api-key` |
| Image service (ComfyUI) URL | `~/.config/nova/comfyui-url` |
| GitHub deploy key | `~/.ssh/nova_github_ed25519` |

Use the image service for app and poster art. So far DeepSeek has been used only for
side questions; its last answer, on firmware, was not useful.

To push:

```sh
GIT_SSH_COMMAND="ssh -i $HOME/.ssh/nova_github_ed25519 -o IdentitiesOnly=yes" \
  git push -q git@github.com:Eyonic/NovaOs.git linux-driver-layer
```

To commit, set `GIT_AUTHOR_NAME=guus`, `GIT_AUTHOR_EMAIL=fableneverwinternight@gmail.com`
and the matching `GIT_COMMITTER_*` variables.

## Where things are

| Path | What |
|---|---|
| `kernel/src/` | the kernel: `task.rs` (scheduler, SMP), `ipc.rs` (keyed waits), `linux.rs` and `linux/` (system calls, 32-bit compat), `process.rs`, `vdso.rs`, `net.rs`, `pulse.rs`, `wayland.rs` |
| `kernel/linuxkpi/` | lx_emul glue for the Linux drivers; `amdgpu.config` |
| `kernel/vdso/` | vDSO sources, built by `kernel/build.rs` |
| `userland/` | test programs (`ctest`, `proctest`, …) and Nova's apps |
| `scripts/` | `build.sh`, `run.sh`, `test.sh`, `fetch-*.sh`, `clean.sh` |
| `docs/` | design notes and measurements: `smp.md` has the I/O-wait and vDSO numbers |
| `build/` | build output, not in git: `nova-usb.img`, `linux/` (the Linux tree), `logs/` |
