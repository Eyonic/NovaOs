# Architecture (milestone 0.4)

Nova 0.4 is a small x86-64 operating system in Rust: a single-address-space kernel with
cooperative kernel tasks, Rust drivers for the machine it runs on, TCP/IP and TLS, a
filesystem and installer, a desktop with script apps, Claude Code built in, and a Linux
system call layer that runs unmodified static C/C++ programs in user mode.

| Layer | Modules (kernel/src/) |
|---|---|
| CPU, interrupts | `gdt`, `interrupts`, `pic`, `pit`, `rtc`, `cpu`, `selftest` |
| Memory, tasks | `memory` (heap, MMIO, page tables), `task` (cooperative scheduler), `sync` |
| Drivers | `drivers/` (registry, `e1000`, `ahci`), `pci`, `keyboard`, `mouse`, `serial`, `console` |
| Network | `net` (smoltcp: DHCP, DNS, TCP), `tls` (rustls), `http` |
| Storage | `storage` (disks, GPT), `fs` (NovaFS), `install` |
| Text | `textgrid`, `tty`, `lineedit`, `shell` |
| Desktop | `wm` (window manager, compositor), `gfx`, `apps` (Rhai apps, `apps/*.rhai`) |
| Claude | `claude` (Claude Code for Nova), `claude_prompt.md`, `setup` (first boot) |
| Compatibility | `process` (user mode, ELF, threads), `linux` (Linux syscalls), `ipc` (pipes, sockets, epoll, shared memory) |
| Browser | `browser` (Chromium over DevTools), `shields` (blocking, HTTPS upgrade) |

## Boot chain

```
QEMU q35 ── OVMF (UEFI firmware, pflash)
              └─ boots \EFI\BOOT\BOOTX64.EFI from the GPT ESP in build/nova.img  (= Limine)
                   └─ reads \boot\limine\limine.conf
                        └─ loads \boot\nova-kernel (ELF64) and the program modules
                           (\nova\bin\*), sets up long mode + paging, fills Limine
                           request structs, jumps to kmain()
```

## Disk image (`scripts/build.sh`)

64 MiB raw file → GPT (sfdisk) → one EFI System Partition at 1 MiB, FAT32 (mformat).
mtools writes the filesystem directly inside the file at offset 1 MiB, so there's no
loop device, no root, and no mounting.

```
ESP:/EFI/BOOT/BOOTX64.EFI      Limine UEFI executable (copied from the host package)
ESP:/boot/limine/limine.conf   boot/limine.conf, plus one module_path/module_string per program
ESP:/boot/nova-kernel          kernel ELF (debug info stripped; the full ELF stays in kernel/target)
ESP:/nova/bin/*                userland/*.c and *.cpp, compiled with gcc/g++ -static
```

OVMF's variable store is copied to `build/OVMF_VARS.4m.fd`, so the system copy stays read-only.

## Kernel

* Target `x86_64-unknown-none` (stable Rust, soft-float, **no red zone**), `-C code-model=kernel`,
  static relocation model. Linked at `0xffffffff80000000` by `kernel/linker.ld`.
* `src/limine.rs` implements only what's needed: the base-revision tag (revision 3), request
  start/end markers, and the bootloader-info, executable-cmdline, framebuffer and memory-map
  requests (one generic `Request<R>` struct). They live in
  `.limine_requests`, where Limine finds and patches them. The `limine` crate was not used
  because current releases require nightly Rust.
* `src/serial.rs` is a 16550 UART driver on COM1: polled output (lock-free, so reporting from an
  exception handler can't deadlock on it), interrupt-driven input.
* `src/console.rs` holds `print!`/`println!`: every print goes to serial and to the framebuffer
  console (see below).
* `src/main.rs` (`kmain`) runs serial init, heap, console, banner, the base-revision check,
  `gdt::init`, `interrupts::init`, `process::init` (SSE/AVX, SYSCALL), the status line, the
  optional self-test, PIC/PIT/RTC/keyboard/mouse/serial-input setup, `sti`, and then
  `setup::run` (hardware, first-boot setup, desktop), which never returns.

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
| `0x18` | user data, DPL 3 (selector `0x1B`) |
| `0x20` | 64-bit user code, DPL 3 (selector `0x23`) |
| `0x28` | 64-bit TSS (16-byte system descriptor, type 0x9) |

The user segments are in the order SYSRET requires (STAR[63:48] = 0x10: SS = 0x18, CS = 0x20).
TSS.RSP0 is set on every task switch to the running program's kernel stack.

Besides RSP0 for user programs, the TSS provides **IST1**: a dedicated 16 KiB stack for the #DF gate. If an exception
happens because RSP is bad, the CPU can't push the exception frame, which escalates to #DF. #DF
then switches to IST1 and can still report. Without that, the CPU would triple-fault and reset.

After `lgdt`, CS is reloaded with a far return (`push 0x08; push rip; retfq`). DS/ES/SS get
`0x10`, FS/GS get null, and `ltr 0x28` loads the TSS.

### IDT and entry stubs (`src/interrupts.rs`)

All 32 exception vectors get a 64-bit **interrupt gate** (attribute `0x8E`: present, DPL 0,
type 0xE, IF cleared on entry) pointing at a per-vector naked-function stub. #DF uses IST1. Vectors
32-47 are the IRQs (next section). Vectors 48-255 are not present, so a stray interrupt raises #NP,
which is reported.

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
**No kernel exception is recovered from.** #BP is a trap and could be resumed, but it is also
reported and halted. Exceptions raised in user mode are different: a page fault inside a
demand-filled area maps a zero page and resumes the program (`exception_common` returns with
`iretq` when the dispatcher returns); any other user fault ends only that program, with a
message like `ctest: segmentation fault at address 0x... (instruction 0x...)`.

Reading the report: for faults, RIP points **at** the faulting instruction and RFLAGS.RF (bit 16)
is set. For traps (#BP), RIP points **after** it. For #DF, RIP is architecturally undefined
(printed with a note).

## Hardware interrupts (0.3)

### Interrupt controller and timer (`src/pic.rs`, `src/pit.rs`)

Nova uses the legacy 8259 PIC pair, remapped so IRQ 0-15 arrive on vectors 32-47. Unmasked lines:
IRQ 0 (PIT), IRQ 1 (PS/2 keyboard), IRQ 2 (cascade), IRQ 4 (COM1), IRQ 12 (PS/2 mouse). Limine leaves the PICs and the I/O
APIC entries masked, but with base revision 3 the local APIC is in whatever state the firmware left,
and a masked LINT0 would swallow the PIC's output. `pic::init` therefore hardware-disables the local
APIC (clears `IA32_APIC_BASE` bit 11, and bit 10 for x2APIC). With the LAPIC off, the PIC's INTR
line goes straight to the CPU, as on a pre-APIC PC. That is fine for one CPU. SMP would need the
LAPIC and I/O APIC instead.

The PIT's channel 0 runs at 100 Hz (divisor 11931, mode 3). Its handler only increments a tick
counter, which `uptime` and `sleep` read.

Spurious IRQ 7/15 are detected by reading the PIC's in-service register. They get no EOI (IRQ 7) or
a master-only EOI (IRQ 15).

### IRQ entry path (`src/interrupts.rs`)

IRQ stubs push a dummy error code and the vector, exactly like the error-code-less exception stubs,
so the frame layout and the 16-byte alignment argument are the same. `irq_common` saves all 15 GPRs,
calls `irq_dispatch`, restores them, drops vector and dummy, and `iretq`s. The gates are interrupt
gates, so IF stays clear for the whole handler and IRQs never nest.

Handlers do the minimum. They never print and never take a lock:

| IRQ | Handler |
|---|---|
| 0 | `pit::tick()` |
| 1 | read one scan code byte from port 0x60 into the `SCANCODES` ring |
| 4 | drain the UART receive buffer into the `SERIAL_RX` ring |
| 12 | read one mouse byte into the `mouse::BYTES` ring |

Every IRQ also increments `task::IRQ_COUNT`, which wakes parked tasks. If the timer interrupts
a user program (CS RPL 3), the handler yields to other tasks after the EOI: that is how user
programs are preempted while kernel tasks stay cooperative.

Then they send an EOI.

### Input (`src/input.rs`, `src/keyboard.rs`)

The rings are lock-free single-producer (IRQ handler) / single-consumer (main loop) queues of 256
bytes. If a ring is full, new bytes are dropped. `input::read_key` decodes in the main loop, not in
the handler:

* Keyboard: scan code set 1 (the 8042 translates), US layout, Shift, Caps Lock, Ctrl-letter, and
  the `E0`-prefixed arrows and keypad Enter/slash. `keyboard::init` enables port 1, its IRQ, and
  translation in the controller config byte, then sends "enable scanning" (0xF4) and consumes the ACK.
  All controller handshakes have a timeout, so a machine without an 8042 still boots (serial only).
* Serial: CR, LF or CRLF are Enter, DEL/BS are Backspace, and `ESC [ A/B/C/D` (`ESC O x` too) are
  the arrows.

With nothing to read, `read_key` sleeps in `wait_for_interrupt`: `cli`, check the rings, then
`sti; hlt`. STI's one-instruction interrupt shadow makes `sti; hlt` atomic, so an IRQ that arrives
after the check still wakes the `hlt` instead of being lost.

## Memory and tasks (0.4)

**Heap and mappings (`memory.rs`).** Limine maps all usable RAM at the higher-half direct map
(HHDM). The heap (talc allocator) is the largest usable region, used through the HHDM, so it is
physically contiguous: `virt_to_phys` is a subtraction and heap buffers can be handed to DMA
devices (`DmaBuffer`). Device registers are not in the HHDM under base revision 3, so
`map_mmio` maps them uncached (PCD|PWT) into a window at `0xFFFF_E000_0000_0000`, creating page
tables from the heap. The window's PML4 entry is created at boot so process address spaces,
which copy the kernel's upper half, see later MMIO mappings too. QEMU gets 1 GiB of RAM.

**Tasks (`task.rs`).** Cooperative kernel tasks, each with a 512 KiB stack. `yield_now` and
`wait_event` (park until the next interrupt) are the only switch points, and IRQ handlers never
touch scheduler state, so kernel data needs no locking beyond detecting re-entry. `switch` saves
callee-saved registers and RFLAGS: a task switched out from inside an IRQ handler (a preempted
user program) resumes with IF clear, everything else with IF set. When all tasks are parked the
CPU halts (`sti; hlt`), and the time spent there gives the CPU load shown in the taskbar.
`SpinLock::lock` yields while the lock is held elsewhere; `try_lock` is used where blocking is
impossible (printing, fault reporting).

## Drivers (0.4)

`drivers::probe_all` scans every PCI function (legacy configuration mechanism) and binds the
built-in Rust drivers. The result per device (bound, failed, not needed, missing, port drafted)
is kept for `lspci`, System Monitor and Claude.

| Device | Driver | Notes |
|---|---|---|
| Intel 8254x/82574 Ethernet | `drivers/e1000.rs` | legacy descriptors, 64-entry rings, polled |
| AHCI SATA controller | `drivers/ahci.rs` | IDENTIFY, READ/WRITE DMA EXT, one command slot, polled |
| PS/2 keyboard and mouse | `keyboard.rs`, `mouse.rs` | scan code set 1; VMware vmmouse backdoor for absolute pointing |
| 16550 UART | `serial.rs` | polled output, IRQ input |
| Display | firmware framebuffer (GOP via Limine) | no mode setting |
| PIC, PIT, CMOS RTC | `pic.rs`, `pit.rs`, `rtc.rs` | |

QEMU runs `q35,vmport=on` with an `e1000` NIC; vmport gives the absolute pointer, so the cursor
follows the host mouse without grabbing it.

## Network (0.4)

`net.rs` wraps smoltcp (DHCPv4, DNS, TCP). One task polls the interface every timer tick;
blocking calls (`resolve`, `TcpStream::connect/read/write_all`) poll too and park between polls.
`tls.rs` uses rustls's no_std unbuffered client with the RustCrypto provider and the Mozilla root
store (webpki-roots); certificates are checked against the RTC clock. The RustCrypto crates are
built with their portable backends (`--cfg *_force_soft` in `kernel/.cargo/config.toml`) because
the kernel target has no SSE and their SIMD backends crash LLVM for it. Randomness comes from
RDRAND, with a SHA-256-of-TSC-jitter fallback (`rand.rs`). `http.rs` is a small HTTP/1.1 client
(Content-Length, chunked, close-delimited bodies, line streaming for server-sent events).

## Storage and installation (0.4)

Disks (AHCI, plus a 32 MiB RAM disk in live sessions) are registered in `storage.rs`, which reads
and writes GPT (with CRC-32). The boot disk is recognised by the GPT disk GUID Limine reports for
the kernel file.

NovaFS (`fs.rs`): block 0 superblock (`NOVAFS01`), blocks 1-32 a 1024-entry directory, then file
data as one contiguous extent per file (first fit). Paths are flat strings; directories are
prefixes. Each change writes the data, then the one directory block it touched.

Installing (`install.rs`) writes a new GPT to the target disk: an ESP copied sector by sector
from the boot disk's ESP, and a NovaFS data partition (type GUID
`4E6F7661-4653-4E44-8001-4E4F56414653`) that receives the live session's files. It refuses the
boot disk. At boot, a boot disk with a NovaFS partition means "installed"; otherwise Nova runs
live from RAM.

## Terminals, console and shell (0.3, extended in 0.4)

`textgrid.rs` is a terminal grid: UTF-8 decoding, CP437 glyph mapping (with look-alikes for
common symbols), the ANSI subset Nova emits (SGR colours and bold, `ESC[2J`, `ESC[H`, `ESC[K`),
deferred wrap and scrolling. The framebuffer console (`console.rs`) draws one grid with the
Spleen 8x16 font and redraws only changed cells. Desktop terminal windows draw their own grids.

A `tty` is a grid plus an input queue, attached to tasks: `print!` from a task with a tty goes to
it, `input::read_key` reads from it. A tty can mirror COM1 (output copied to serial, serial input
accepted as keys); the Claude Code window opened at desktop start does, which keeps the system
usable and testable over the serial line. `tty::capture` collects a task's output in a string
(used for Claude's `run_command`). Fault and panic reports bypass ttys and always reach COM1 and
the screen.

`lineedit.rs` is the line editor (Backspace, Ctrl-U/C/L/D, Tab completion callback, history);
the shell (`shell.rs`) and Claude Code use it. Shell commands: `help echo clear info mem uptime
sleep history ps lspci ifconfig host fetch disks ls cat write rm install run crash halt reboot
shutdown`, plus any program in `bin/`.

## Desktop (0.4)

`wm.rs` runs in its own task. It owns the screen once started (the console stops drawing), reads
keyboard and mouse, routes keys to the focused window and pointer events to the window under the
pointer, and recomposes the scene when something changed (at most every 2 ticks): Nova Home,
windows bottom to top with shadow,
title bar, minimise/close buttons and a resize grip. There is no taskbar or start menu. The
Windows (Super) key, scancode E0 5B/5C, minimizes every window to show Home. Open windows are
cards in Home's Continue row; right-clicking a card opens a menu with Open and Stop running.
Network, CPU, memory and the clock are in Home's header. The scene is copied to the
framebuffer; the pointer is drawn on top and moved without recomposing.

Nova Home (`home.rs`) is the launcher behind the windows. A hero banner shows the selected item
full width: its artwork, title, description, an Open button and, for apps and windows, a live
preview. Below it are rows of poster cards:

- Continue: the open windows, as live thumbnails drawn by `wm::draw_window_thumb`.
- Create with AI: the assistant, and idea cards that open it with a first request.
- Your apps
- Apps
- Web: sites opened in Nova Browser.
- System: restart and shut down.

Hovering a card selects it and enlarges it. After 0.6 s, an app card plays a live preview: a
background task runs the app headlessly (`apps::snapshot`) about once a second. Keys browse Home
when no window has the focus. Posters come from `usr/share/nova/home/*.png` (generated artwork
from `assets/home/`, copied in by `build.sh`). Items without artwork get a procedural poster.
Posters and the hero are rendered once per item and cached.

UI text is anti-aliased Noto Sans (Regular and Bold, `font.rs`, rasterised with fontdue and
cached per size and weight). Home, window titles, Nova Browser's toolbar and app labels
(`g.label`) use it, while terminals keep the 8x16 bitmap font for their character grid. Without the
font files everything falls back to the bitmap font. App icons and toolbar glyphs (`icons.rs`)
are generated images from `assets/` (see assets/README.md). Glyphs are white-on-black masks,
tinted when drawn (`Canvas::draw_mask`).

Window content is a `gfx::Canvas` handed over by the app (`wm::present`) or a terminal grid.
Apps receive `wm::Event`s (keys, mouse down/up/move, wheel, resize, close).

## Apps (0.4)

Apps are Rhai scripts (`apps.rs`), each in its own task and window. Header comments give title,
icon, colour and size; callbacks (`init`, `draw(g)`, `click`, `key`, `tick`, ...) run with `this`
bound to the app state. The graphics object offers rectangles, rounded rectangles, lines,
circles, text (1x/2x/scaled), buttons, progress bars and PNG images (`g.image`, decoded once
and cached until the filesystem changes); other builtins give time, random
numbers, files, HTTP, system information and launching. The Rhai engine yields to other tasks
every 20,000 operations and stops runaway scripts (50 million operations per callback). Errors
are shown in the window. The bundled Files, Text Editor, System Monitor and Settings apps are
Rhai apps in `kernel/src/apps/`, written to `system/apps/` at boot. `apps::check` runs an app
headless (init, simulated events, draw) and returns errors plus a PNG of the frame.

## Claude Code for Nova (0.4)

`claude.rs` talks to the Messages API over HTTPS: model `claude-opus-5-5` (changeable with
`/model`), streaming, adaptive thinking with summaries shown dimmed, `effort: high`, automatic
prompt caching, `eager_input_streaming` on client tools, and server-side refusal fallbacks
(`fallbacks: "default"`, beta `server-side-fallback-2026-07-01`) for models that support them.
Thinking blocks are passed back unchanged; after a mid-output fallback, the declined model's
thinking and tool_use blocks before the `fallback` block are dropped as the API requires.
Transient errors (429, 5xx, overloaded) are retried with backoff; refusals and API errors leave
the conversation in a valid state.

Sign-in: `scripts/run.sh` passes the host's Anthropic CLI login (`ant auth print-credentials
--access-token`, else `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY`) as the QEMU fw_cfg file
`opt/nova/claude-credential` ("oauth" or "api-key", the secret, its source). `fwcfg.rs` reads it
through the legacy I/O ports 0x510/0x511; `claude::sign_in` checks it with `GET /v1/models` and
then uses it from memory only (OAuth tokens as `Authorization: Bearer` with the `oauth-2025-04-20`
beta, added to any other betas). Without one, or if the API rejects it, `/login` asks for an API
key, which is stored in `system/claude.json`.

Image generation (`imagegen.rs`) uses a ComfyUI server's HTTP API. The address comes from the
fw_cfg file `opt/nova/comfyui-url` (set by `run.sh` from `NOVA_COMFYUI_URL` or
`~/.config/nova/comfyui-url`) or from `nova.comfyui=`. Nova posts a Z-Image Turbo workflow to
`/prompt`, polls `/history/<id>` for up to 5 minutes (the server may be warming up) and
downloads the PNG from `/view`. The assistant's `generate_image` tool saves the PNG and attaches
it to the tool result, so the model sees what it got. The system prompt has the assistant design
each app first: a small (~512 px) mockup of the window, then the app built to match it, with
generated assets for its pictures. `check_app` takes an optional `reference` image (the mockup)
and returns it next to the screenshot. Tools: read,
write, edit, list and delete files; run shell commands (commands that change things and file
deletions ask the user first, `/permissions` toggles that); `check_app` (screenshot attached as
an image); `launch_app`; `http_get`; `system_info`; `pci_config`; and the server-side web search
tool. The system prompt (`claude_prompt.md`) documents the app API and Nova's environment.

## First boot and setup (0.4)

`setup.rs` runs after the kernel core: detect hardware and bind drivers, mount storage, install
bundled apps and programs, bring up the network. On first boot (no `system/setup-done`) Claude
Code opens full screen for login; then, for PCI devices without a driver, Claude inspects them
(`pci_config`) and writes a Rust driver port to `drivers/` (Nova cannot compile Rust at runtime;
the port is compiled into the next kernel build) and the device is marked "port drafted". In a
live session with a free disk, setup offers installation and reboots into the installed system.
Then the desktop starts with Claude Code open. `nova.mode=shell` on the kernel command line runs
the text shell instead (tests); `nova.install=no` skips the install question;
`nova.claude_api=URL` points Claude at another API endpoint (the tests' mock server).

## Linux compatibility environment (0.4)

Nova runs x86-64 Linux programs unmodified: static or dynamically linked, C or C++, single or
multi-threaded, including the Chromium engine (`process.rs`, `linux.rs`, `ipc.rs`).

* **Address spaces.** Each program gets a PML4 whose upper half is the kernel's. The program
  is loaded at its ELF addresses (PIE at `0x555555554000`); for a `PT_INTERP` program the
  dynamic loader is loaded too (at `0x7F8000000000`) and started with `AT_BASE`/`AT_ENTRY`,
  and glibc's own loader maps the shared libraries through `openat` and `mmap`. Stack (8 MiB),
  heap (`brk`) and anonymous `mmap` regions are areas filled with zero pages on first touch.
  `mmap` honours address hints. `mprotect` enforces write permission (programs test it:
  Chromium checks that a read-only page makes `getrlimit` fail with `EFAULT`).
* **System files without copies.** Programs, libraries, fonts and the Chromium engine are
  packed into one Limine module (`scripts/mkpak.py`, 4 KiB-aligned files) and registered as
  read-only files over NovaFS. Mapping them shares the module's physical pages
  (copy-on-write), so a 205 MB binary is not copied.
* **Threads.** `clone`/`clone3` with `CLONE_VM | CLONE_THREAD` start a kernel task that
  enters user mode with the parent's registers on a new stack and TLS; `set_tid_address`,
  `CLONE_CHILD_CLEARTID` and futexes (wait, wake, requeue, wake-op, bitsets, timeouts) work.
  `exit_group` stops every thread at its next kernel entry; the address space is freed with
  the last thread. Timer interrupts preempt user threads.
* **Files and IPC.** NovaFS files, `/dev/null`, `/dev/urandom`, terminals; pipes, Unix socket
  pairs (stream, datagram, seqpacket) with `SCM_RIGHTS` descriptor passing, eventfd, epoll
  (level and edge triggered), `poll`; memfd and `/dev/shm` shared memory whose `MAP_SHARED`
  mappings alias the same physical pages; synthetic `/proc` (`self/exe`, `maps`, `status`,
  `stat`, `fd/N`, `meminfo`, `cpuinfo`, ...), `/sys/devices/system/cpu` and `/etc` files.
* **Network.** `AF_INET` TCP and UDP sockets over smoltcp: non-blocking `connect` with
  `EINPROGRESS`/`SO_ERROR`, `send*`/`recv*` including `sendmmsg` (glibc's resolver), poll/epoll
  readiness. glibc DNS works through a synthetic `/etc/resolv.conf`.
* **Time.** `CLOCK_MONOTONIC`/`REALTIME` use the TSC, calibrated against the PIT at boot
  (nanosecond resolution; a coarse clock made Chromium's schedulers busy-poll).
* **CPU state.** XSAVE/XRSTOR with x87, SSE, AVX and (if present) AVX-512 per thread; FS
  base per thread. Faults in user code end the program, not the system.
* **Diagnostics.** `strace <program>` logs every system call with arguments and results to
  the serial console; unimplemented calls return `-ENOSYS` and are logged once.

Bundled test programs: `ctest` (24 libc checks), `cxxtest` and `cxxtest-dyn` (17 C++ runtime
checks: exceptions, RTTI, static constructors/destructors, STL, iostreams, fstream),
`threads` (7 checks: mutexes, condition variables, TLS, async, exceptions in threads),
`hello-dyn`, `nettest` (DNS + HTTP over sockets), `guess` (interactive stdin).

Not yet: `fork`/`exec` (Chromium runs with `--single-process`), signals delivered to
handlers, `AF_INET6`, listening sockets, inotify.

## Nova Browser and Shields (0.4)

The browser is a Rust shell around the Chromium engine (`browser.rs`, `shields.rs`). The
engine is Chrome for Testing's `chrome-headless-shell` (downloaded by
`scripts/fetch-chromium.sh`, shipped under `/opt/chromium` with the shared libraries, NSS
modules and fonts it needs). Nova starts it with `--single-process --no-sandbox
--remote-debugging-pipe` and talks the DevTools protocol over two pipes on its fds 3 and 4
(JSON messages separated by NUL bytes):

* `Target.attachToTarget` (flattened session), `Page.enable`, `Emulation.setDeviceMetricsOverride`
  (viewport = window size) and `Page.startScreencast` (PNG frames, decoded with
  `miniz_oxide` and shown in the window).
* Navigation (`Page.navigate`, history, reload), page title (`Runtime.evaluate`), and input:
  mouse buttons, movement and wheel (`Input.dispatchMouseEvent`), keys (`Input.dispatchKeyEvent`).
* The address bar takes URLs, host names or searches (DuckDuckGo).

Shields sees every request through `Fetch.enable` / `Fetch.requestPaused` and answers
continue, fail (`BlockedByClient`) or continue with a rewritten URL:

* blocks third-party requests to a built-in list of advertising, analytics, tracking and
  fingerprinting hosts, and anything matched by filter lists in `system/shields/*.txt`
  (Adblock Plus syntax: `||host^`, wildcards, separators, anchors, `@@` exceptions,
  `$third-party`, `$domain=`; cosmetic rules are skipped; each rule's longest literal is
  checked first so large lists stay fast). `download https://easylist.to/easylist/easylist.txt
  system/shields/easylist.txt` adds EasyList;
* upgrades page loads from `http://` to `https://` (except local addresses) and strips
  tracking parameters (`utm_*`, `fbclid`, `gclid`, ...);
* can be turned down per site from the Shields panel (`system/shields/allowed-sites`).

The Shields button shows the count of blocked requests; the panel lists the blocked hosts.

### References

* Intel 64 and IA-32 SDM Vol. 3A: "Interrupt and Exception Handling" (vector table, error code,
  64-bit IDT gates, 64-bit stack frame, IST), "Task Management" (64-bit TSS), "Paging" (#PF error
  code), "Protected-Mode Memory Management" (descriptors).
* AMD64 APM Vol. 2: "Exceptions and Interrupts" (#VC, #SX).
* Limine protocol, `PROTOCOL.md`: x86-64 entry machine state, base revisions, executable cmdline,
  framebuffer and memory-map features.
* Intel 8259A datasheet (ICW1-4, OCW2/3); Intel SDM Vol. 3A, "Advanced Programmable Interrupt
  Controller" (disabling the local APIC); OSDev wiki "8042 PS/2 Controller", "PS/2 Keyboard"
  (scan code set 1), "Programmable Interval Timer".
* Cross-checked against the rust-osdev `x86_64` crate's `src/structures/idt.rs` (vector and
  error-code table, gate layout, #DF RIP note).

### `unsafe` inventory

| Where | Why |
|---|---|
| `port.rs` `outb`/`outw`/`inb` + callers | x86 port I/O via `asm!`: COM1, PICs, PIT, 8042, ACPI PM control |
| `pic.rs` `cpuid`, `rdmsr`/`wrmsr` | detecting and disabling the local APIC |
| `console.rs` volatile framebuffer writes | drawing; bounds come from Limine's framebuffer description |
| `sync.rs`, `input.rs` `unsafe impl Sync`, `UnsafeCell` access | spinlock and SPSC rings |
| `input.rs` `cli` / `sti; hlt` | idle wait without a lost-wakeup race |
| `interrupts.rs` IRQ stubs + `irq_common`, `sti` | IRQ entry/exit (`iretq`), enabling interrupts |
| `power.rs` `reboot` (`lidt` of an empty IDT + `int3`) | forced triple fault as a reset fallback |
| `memory.rs` page-table walks and writes, `invlpg`, `mov cr3` | MMIO mappings, process address spaces |
| `task.rs` naked `switch`, scheduler cell | context switches between task stacks |
| `drivers/e1000.rs`, `drivers/ahci.rs` volatile MMIO and descriptor access | device registers and DMA rings |
| `mouse.rs` VMware backdoor `in` | absolute pointer |
| `process.rs` CR0/CR4/XCR0/MSR writes, `xsave`/`xrstor`, naked `syscall_entry`, `enter_user`, `leave_user` | user mode |
| `linux.rs` copies to/from user memory | after `UserState::ensure` checked and mapped the range |
| `wm.rs`, `console.rs` framebuffer writes | drawing |
| `limine.rs` `unsafe impl Sync`, volatile reads, `CStr::from_ptr` | bootloader-written statics and strings |
| `bootcell.rs` `unsafe impl Sync` | CPU tables written only during single-CPU init |
| `gdt.rs` TSS/GDT writes, `lgdt`, far return, segment loads, `ltr` | installing the GDT/TSS |
| `interrupts.rs` naked stubs + `exception_common` | interrupt entry (about 25 instructions total) |
| `interrupts.rs` IDT writes, `lidt`, `mov cr2` | installing the IDT, reading the fault address |
| `selftest.rs` `global_asm!` routines | deliberate faults, only reachable with `nova.test=` or `crash` |
| `main.rs` `halt_forever` | `cli; hlt` |

## Tests

`scripts/test.sh` is an external watchdog: the kernel never exits, so the script boots QEMU
headless with `-no-reboot`, polls the serial log, and stops QEMU itself. COM1 and the QEMU monitor
are `pipe` chardevs (FIFOs `serial.in/.out`, `monitor.in/.out`). `cat` copies the serial output into
the log, and the script types into `serial.in`, or sends `sendkey` commands to the monitor for the
PS/2 keyboard. A triple
fault therefore shows up as "QEMU exited". Compilation success alone never counts as a pass.

All cases run the same kernel binary. `scripts/mkimage.sh` adds `cmdline: nova.test=<case>` to
`limine.conf`, and `src/selftest.rs` dispatches on it.

| Case | Trigger | Verified |
|---|---|---|
| `boot` | none (regression) | banner, status, interrupts line, `starting shell`, prompt; no exception or panic |
| `panic` | `panic!()` | panic header, location, message, halted line |
| `de` | `div ecx` with ECX=0 | #DE, vector 0, "error code: none", RIP == faulting instruction |
| `ud` | `ud2` | #UD, vector 6, no error code, RIP match |
| `gp` | `mov ds, ax` with selector 0x40 (past the GDT limit) | #GP, error code `0x40` decoded as index 8/GDT, RIP match |
| `pf` | write to unmapped `0xdeadbeef000` | #PF, error code `0x2` (not-present, write, supervisor), CR2, RIP match |
| `bp` | `int3` | #BP as a **trap**: RIP == the instruction *after* `int3` |
| `df` | `mov rsp, 0xdeadbeef000; push` | #PF can't be delivered → #DF on IST1, error code 0, RSP shows the bad stack, no #PF report |
| `nested` | #BP whose reporter executes `ud2` | nested-exception line, first report cut short, controlled halt |
| `shell` | typed over serial: `help`, `echo`, unknown command, `sleep 1`, Up-arrow history recall, DEL editing, `history` | each output; `sleep` returning proves the timer IRQ works |
| `keyboard` | typed as PS/2 key presses: Shift, punctuation, Backspace, Tab completion, Ctrl-C | the echoed text and `^C` |
| `crash` | `crash pf` typed at the shell, with interrupts on | full #PF report with RIP check, controlled halt |
| `native` | `hello`, `ctest`, `cxxtest`, `threads`, `hello-dyn`, `cxxtest-dyn`, `guess` | C and C++ runtime checks pass (static and dynamic), threads, stdin, exit status, no unimplemented syscall |
| `net` | `nettest 10.0.2.2:<mock port>` | glibc DNS and a TCP HTTP exchange through the Linux socket layer |
| `chromium` | `chrome-headless-shell --screenshot` of a `data:` URL | the Chromium engine starts, renders and writes the PNG (skipped if Chromium was not downloaded) |
| `browser` | desktop; "open the test page" typed to Claude Code (mock API) | Claude calls `open_url`, Nova Browser starts Chromium, Chromium fetches the page from the host, Shields blocks its Google Analytics script (skipped without Chromium) |
| `setup` | live image + empty 2 GiB disk + an Intel HD Audio device; API key typed over serial; `y` to install | drivers bound, login against the mock API, Claude ports the audio driver, installation, reboot (QEMU exits) |
| `hostlogin` | live image with an OAuth token passed in via fw_cfg, as `run.sh` does; `nova.install=no` | no key prompt; sign-in, driver porting and the desktop session use Bearer + the OAuth beta (the mock rejects anything else) |
| `installed` | boots the disk `setup` installed; "make me a clock app" over serial | installed-system boot, no login prompt, ported driver recognised, Claude writes, checks (PNG) and launches an app |

The Claude cases run against `scripts/mock_anthropic.py`, a scripted stand-in for the Messages
API that speaks the real protocol (chunked SSE, thinking blocks with signatures, streamed tool
input) and rejects malformed requests: missing headers or betas, non-adaptive thinking, tools
without `eager_input_streaming`, thinking blocks not echoed unchanged, tool results that do not
answer the previous tool calls. Any violation fails the test. The real API was exercised only
for TLS and authentication (`fetch https://api.anthropic.com/v1/models` returns 401 without a
key); a full session needs an API key.

Steps can be paced by the shell prompt (shell cases) or by `wait:TEXT` markers (setup,
installed, native). `scripts/qmp_input.py` sends absolute mouse clicks and screenshots over QMP for
manual desktop checks.

Shell cases send each input only after the shell has printed one more prompt, and they finish only
once the prompt after the last input has appeared. Each pass also requires that QEMU is still
running and printing nothing 2 s after the last expected line (controlled halt or idle shell, no
reset or loop). The RIP check compares against the address of a global
label on the faulting instruction, which the kernel prints before triggering. A frame-layout bug
shifts every field and fails the check.

The tests were validated by mutation: declaring #PF as error-code-less makes `pf` fail (the report
shows RIP=`0x2`, CS=the real RIP), and removing IST1 from the #DF gate makes `df` fail (triple
fault, QEMU exits). Masking the timer IRQ makes `shell` fail (`sleep 1` never returns), and ignoring
Shift makes `keyboard` fail.

## Not included (by design)

No SMP, APIC/I/O APIC, USB, audio, GPU acceleration, or Wi-Fi. Device drivers are polled. The
compatibility layer has no fork/exec, signal handlers, IPv6 or server sockets yet. Kernel code
(drivers, desktop, Rhai apps, the browser shell) shares one address space; Linux programs run in
user mode. Rhai apps are not sandboxed from the filesystem.
