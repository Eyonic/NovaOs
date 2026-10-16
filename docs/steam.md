# Steam on Nova

Valve's Linux Steam client runs on Nova up to its sign-in window
([screenshot](screenshots/steam-login.png)). Nothing of Steam is in this repository: the
image build copies the host's 32- and 64-bit Mesa/GL stacks when asked, and Steam
downloads itself at first start.

## Running it

```sh
NOVA_STEAM=1 IMAGE=build/linux-test.img NOVA_CMDLINE="nova.mode=shell" scripts/build.sh
IMAGE=build/linux-test.img NOVA_RAM=10G scripts/run.sh --live   # KVM, 10 GiB of memory
```

In the Nova shell:

```sh
sh
mkdir -p /home/nova && Xwayland :0 -shm -geometry 1024x768 > /run/xw.log 2>&1 &
export HOME=/home/nova DISPLAY=:0 PULSE_SERVER=unix:/run/user/1000/pulse/native GALLIUM_DRIVER=softpipe
cd /home/nova
steam -no-cef-sandbox -cef-disable-gpu -cef-disable-hang-timeouts > /run/steam.log 2>&1 < /dev/null &
```

The first start downloads and installs the client (a few minutes); the sign-in window
appears about a minute after the client starts. `wayland save /run/shot.ppm` in the Nova
shell saves what Xwayland shows.

Why the flags: `-no-cef-sandbox` because Nova has no seccomp-BPF yet; `-cef-disable-gpu`
because there is no GPU driver for Chromium in QEMU; `-cef-disable-hang-timeouts` because
Chromium's GPU watchdog is too impatient for a software renderer in a VM.

## What it needed from Nova

Each of these was found by running Steam and is a general Linux-compatibility fix:

| Steam needed | Nova provides |
|---|---|
| 32-bit client with a 4 GiB address space | i386 compatibility mode; mmap that fails with ENOMEM instead of overlapping mappings; per-thread kernel buffers kept above 4 GiB |
| Steam runtime containers | mount namespaces, bind mounts, `pivot_root`, user namespaces (bubblewrap) |
| Audio (libpulse retries for ever without a server) | a PulseAudio native-protocol server in the kernel (`pulse.rs`); with an HD Audio device it plays streams (see [audio.md](audio.md)) |
| Chromium's zygote (it asks the browser for each child's pid) | `SO_PASSCRED` / `SCM_CREDENTIALS` |
| Chromium's sandbox IPC (localtime, fonts) | Linux `poll` semantics for half-shut Unix sockets (`shutdown(SHUT_RD/SHUT_WR)`) |
| The UI's websocket to the client | loopback TCP (127.0.0.1) |
| The client checking who is at the other end of that websocket (`lsof -i TCP@addr`, then the parent chain in `/proc/<pid>/stat`) | `/bin/lsof` (userland/lsof.c, reading `/proc/nova/tcp`), every pid in `/proc`, real `stat`/`status`/`cmdline`/`comm` per pid |

## Debugging aids

- `ps` shows each thread's current system call, its program, Chromium's `--type=` and the
  thread name (`prctl(PR_SET_NAME)`).
- `recent <pid>|<name>`: each thread's last system calls.
- `peer <pid> <fd>`: which processes hold the other end of a socket or pipe.
- `trace <pid> on|off`, or `nova.trace=NAME` on the kernel command line, logs system calls
  to serial.
- `userland/cdp.c` (`/bin/cdp`): a minimal Chrome DevTools Protocol client; start Steam
  with `-cef-enable-debugging` and query `http://127.0.0.1:8080/json`.

## Next

Signing in and the library window, sound output, a GPU path for Chromium and games
(Vulkan on real hardware with the amdgpu driver), and seccomp so Steam's sandbox can stay
on.
