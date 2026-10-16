# Linux drivers in Nova

Nova's core stays Rust. To support real hardware without rewriting every driver, Nova can
compile **unmodified Linux driver code** into its kernel and run it on top of a thin
emulation of the Linux kernel's internal API. This is the approach of Genode's `dde_linux`
and, in spirit, FreeBSD's LinuxKPI: real Linux subsystems and drivers, Nova underneath.

```
  Nova (Rust): memory, tasks, interrupts, PCI enumeration, TCP/IP, desktop, Linux programs
        |  nova_* calls (kernel/src/linuxkpi.rs)
  lx_emul (C, kernel/linuxkpi/lx_emul/): kmalloc, printk, timers, workqueues, IRQs, PCI core,
        |  net_device / sk_buff / NAPI, DMA  -- written for Nova, MIT OR Apache-2.0
  Linux 7.2.9 source (build/linux/, GPL-2.0): drivers + library code, compiled by Kbuild
```

Status: **milestones 1-5 done.** Linux's own network drivers run in Nova and carry its
traffic: `8139cp` (RealTek RTL8139C+, legacy INTx routed by ACPI through the I/O APIC,
`scripts/test.sh --case linuxnet`) and `e1000e` (Intel 82574, MSI-X through the local APIC,
`--case linuxmsi`). Linux's whole USB stack runs too: the xHCI host driver, the USB core
with hubs, HID and the input core, bridged to Nova's keyboard and mouse
(`--case usbinput`, on a machine with no PS/2 controller).

## Building

```sh
scripts/fetch-linux.sh                       # Linux 7.2.9 source into build/linux (~150 MB, sha256-checked)
FEATURES=linux-drivers scripts/build.sh      # configure, Kbuild the objects, link them into Nova
FEATURES=linux-drivers scripts/test.sh --case linuxnet
NOVA_NIC=rtl8139 scripts/run.sh              # boot with a card the Linux driver handles
NOVA_NIC=e1000e NOVA_CMDLINE=nova.disable_driver=e1000 FEATURES=linux-drivers scripts/build.sh
                                             # (e1000e: switch off Nova's own e1000 driver first)
```

`scripts/build-linux.sh` (called by `build.sh`) does the Linux side:

1. `make tinyconfig`, merge `kernel/linuxkpi/nova.config`, `make olddefconfig`, and check that
   Kconfig kept every setting of the fragment;
2. `make prepare`;
3. Kbuild compiles the objects in `kernel/linuxkpi/linux-objects.txt` and lx_emul
   (copied to `build/linux/nova/`), so lx_emul gets exactly the
   flags and headers the Linux objects get;
4. the object list goes to `build/linux/nova-objects.list`, which `kernel/build.rs` passes to
   the linker as plain objects.

Without the feature nothing of this is built and the kernel is unchanged.

## Licensing

The Linux source is GPL-2.0 and is never stored in this repository: `fetch-linux.sh` downloads
it from kernel.org at build time. lx_emul, `nova.config` and the Rust side are Nova's own code
(MIT OR Apache-2.0). **A kernel built with `FEATURES=linux-drivers` contains GPL-2.0 code, so
that binary as a whole is distributed under GPL-2.0** (with its corresponding source: this
repository plus the pinned Linux release). Kernels built without the feature are unaffected.

## How it fits together

* **Configuration** (`nova.config`): 64-bit, one CPU, no modules, `HZ=1000` (Nova's timer
  rate, so `jiffies` means the same on both sides), `printk` on, and everything off that would
  need Linux runtime machinery: jump labels, CPU mitigation thunks, ftrace, stack protector,
  paravirt, IBT. `RANDOMIZE_MEMORY` turns `PAGE_OFFSET` into the variable `page_offset_base`,
  which lx_emul sets to Limine's direct-map base, so Linux's `__pa()`/`__va()`/`virt_to_page()`
  agree with Nova's `memory.rs`. Linux 7.x no longer offers `PREEMPT_NONE` on x86; the
  preempt count starts at "no reschedule pending" and `preempt_schedule_thunk` is a `ret`.
* **Real Linux code where it can be reused**: `lib/vsprintf.c` (all of printk's formats),
  string, bitmap, CRC32, hexdump, kstrtox, iomap (`ioread*`/`iowrite*`, `pci_iomap`),
  `net/ethernet/eth.c` (`eth_type_trans`, `alloc_etherdev`, `ether_setup`), the x86
  checksum and I/O-memory helpers, `drivers/net/mii.c`.
* **lx_emul** implements what cannot be reused because it *is* the kernel: `base.c` (kmalloc
  family on Nova's heap with a pointer-to-layout table, printk output, delays, mutexes, RCU,
  bottom halves, randomness), `sched.c` (timers, workqueues, interrupt handlers), `pci.c`
  (`struct pci_dev` with BARs sized as Linux does, driver matching with ID tables and class
  masks, config space, `ioremap`, DMA), `net.c` (`net_device`, `sk_buff`, NAPI, and the bridge
  to Nova's network stack). Every function is defined against the real Linux prototype, so an
  API change in a new Linux release is a compile error, not a silent mismatch.
* **Linking** (`kernel/linker.ld`): Linux's `.init.*`/`.exit.*` sections stay resident; the
  initcall sections are kept in level order between `__nova_initcall_start/_end`, which
  `nova_lx_start()` runs (built-in `module_init` becomes a level-6 initcall). x86
  alternatives are left unapplied (the default instructions are always correct).
* **Probing** (`drivers/mod.rs`): Nova's Rust drivers get the first pick; every PCI function
  still missing a driver is offered to Linux (`nova_lx_pci_add`) before the initcalls run, so
  `pci_register_driver()` probes at once, as on Linux at boot. Records show `linux/<driver>`.
* **Interrupts** (Nova core, also without the feature): ACPI comes from uACPI
  (`kernel/third_party/uacpi`, MIT, vendored; Nova's kernel API for it is `src/acpi.rs`).
  When the MADT describes an I/O APIC, `src/apic.rs` enables the local APIC and I/O APIC and
  masks the 8259s; ISA interrupts keep their vectors (32 + IRQ) with ACPI's interrupt source
  overrides applied. PCI INTx pins are resolved through the firmware's `_PRT` (AML evaluated
  by uACPI, interrupt link devices included) to a GSI, which is what Linux drivers see as
  `pdev->irq`, as on Linux/x86. Vectors 48-239 are handed out on demand to GSIs and to
  MSI/MSI-X messages (IRQ numbers 0x1000 + vector). lx_emul implements
  `pci_alloc_irq_vectors()`, `pci_irq_vector()`, `pci_enable_msi()` and friends, preferring
  MSI-X, then MSI, then INTx, like Linux.
* **Execution**: interrupt handlers run in real interrupt context, with interrupts disabled
  and `HARDIRQ_OFFSET` in the preempt count (so `in_interrupt()` is right). Deferred work runs
  in one Nova kernel task, "linux", which every interrupt wakes: threaded handlers, NAPI,
  timers and work items (which may sleep). Linux's `spin_lock_irqsave()` really disables
  interrupts, so driver locking keeps its meaning, and lx_emul's own lists (NAPI, timers,
  work) are updated with interrupts off. The task also polls each handler once per wakeup
  with interrupts off, as a fallback; a line that keeps firing with no handler claiming it is
  masked, as Linux's "nobody cared" does. Nothing preempts Linux code, which keeps RCU
  trivially correct on one CPU.
* **Pages**: `alloc_pages()` returns real `struct page`s. Pages come from Nova's heap
  (naturally aligned), and their entries in Linux's `vmemmap` array (at `vmemmap_base`, PML4
  slot 468) are backed with memory on demand by `memory::populate`. `put_page()` frees through
  Linux's own refcounting. Compound pages (folios > 1 page) are not supported yet.
* **Tasks and sleeping** (`task.c`): every Nova task running Linux code has a
  `task_struct`, and Nova's scheduler points Linux's `current` at it on every switch.
  `schedule()`, `wake_up_process()`, wait queues, completions, semaphores and rwsems follow
  Linux's protocol on top of Nova's park/wake. Work items run on a pool of four worker
  tasks, so one that sleeps (a USB control transfer waiting for its completion, delivered
  by another work item) does not block the rest, as with Linux's worker pools.
* **Driver core** (`device.c`): buses, drivers, match-then-probe on `device_add()` and
  `driver_register()`, reference-counted devices with release functions, real devres lists
  and groups. sysfs is enabled in the configuration (the USB hub driver treats a missing
  sysfs node as an error) and implemented as placeholders that accept everything.
* **Input bridge** (`input.c`, `linuxkpi.rs`): an input handler attached to every Linux input
  device turns keys into PS/2 set 1 scan codes and pointer motion (relative or absolute) into
  Nova mouse reports.
* **Network bridge**: a Linux `net_device` becomes a `LinuxNic` (the `Nic` trait of Nova's
  network stack). Received frames go from `netif_receive_skb()`/`napi_gro_receive()` straight
  to Nova; Nova transmits through the driver's `ndo_start_xmit`. Like Linux's queueing layer,
  nothing is handed to a driver without carrier or with its queue stopped (`Nic::can_transmit`):
  Nova's stack keeps the packet instead. Linux's protocol stack is not used.
* **DMA**: no IOMMU, bus address = physical address. A 16 MiB pool below 4 GiB
  (`memory.rs`, feature only) serves devices with 32-bit masks (Linux's default).

## Adding a driver

1. Enable it in `kernel/linuxkpi/nova.config` and add its object(s) to `linux-objects.txt`.
2. `scripts/build-linux.sh`, then list what is still missing:
   `ld -r -o /tmp/all.o $(cat build/linux/nova-objects.list) && nm -u /tmp/all.o`
3. For each symbol, prefer compiling the real Linux file that defines it (check its own `nm -u`
   first); otherwise implement it in lx_emul against the header's prototype.

`8139cp` needed nine new functions once `8139too` had been brought up.

## Roadmap to the reference machine

Target: AMD Zen 3 (x86-64-v3), 16+ GiB, UEFI, NVMe boot drive, USB 3 / xHCI, a GPU with
Vulkan 1.3 (reference: Radeon RX 6700 XT; development machine: RX 6800, the same `amdgpu`
generation), and Steam.

| # | Milestone | Linux code reused | Nova work |
|---|---|---|---|
| 1 | Driver layer + netdev (done) | 8139too, 8139cp, e1000e, mii, eth, lib | lx_emul core, build, Rust bridge |
| 2 | Interrupts and SMP | uACPI (MIT) | real IRQ delivery to Linux handlers, ACPI, local APIC + I/O APIC, `_PRT` routing, MSI/MSI-X, page allocation (done); then multiple CPUs |
| 3 | Real hardware boot | uACPI | PCIe ECAM from MCFG and ACPI power off / reboot (done); devices behind bridges, local APIC timer |
| 4 | NVMe boot drive | — | done, natively: `drivers/nvme.rs` (see below) |
| 5 | USB input | `drivers/usb/core`, `xhci`, `usbhid`, HID core, input core (done) | task/wait model, driver core, input bridge (done) |
| 6 | The board's network | `r8169` (RTL8168/8125), phylib, Realtek PHY driver, ethtool link-mode tables | built in and linked; `request_firmware` reads `/lib/firmware` from Nova's disk (the image gets the host's `rtl_nic` files). QEMU has no RTL8168 model, so the first real test is on the reference machine |
| 7 | GPU | DRM core, TTM, `amdgpu` (incl. display core) | `struct page` backing and page allocation, kernel FPU sections, firmware, KMS to Nova's compositor |
| 8 | Userspace for games | — | fork/exec, signals, DRM ioctls and mmap of GPU buffers for Linux programs |
| 9 | Steam | Mesa RADV, Steam runtime, Proton (userspace) | a Wayland compositor or Xwayland path, audio (HDA via ALSA), namespaces for the Steam runtime |

### Milestone 7 status: amdgpu compiles

With `kernel/linuxkpi/amdgpu.config` (amdgpu plus its display core) added to
nova.config (SI/CIK support, userptr and the KFD compute driver off), Kbuild accepts the
configuration and compiles all 988 objects of `drivers/gpu/drm` (DRM core, TTM, the
display helpers, amdgpu, the display core), `drivers/dma-buf` and `drivers/i2c` without an
error. Linked with Nova's existing Linux objects, they need **246 symbols** that lx_emul
does not provide yet, in these groups:

- library code to compile as is: HDMI infoframes (`drivers/video/hdmi.c`), the GPU buddy
  allocator, `gcd`, interval trees;
- files: anonymous inodes, `fd_install`/`fget`/`fput`, `dentry_open` (DRM and dma-buf
  hand out file descriptors);
- pages and folios: shmem-backed TTM buffer objects, `copy_page`, folio locking and
  dirtying, page-attribute caching modes (`cachemode2protval`);
- timers and interrupts: hrtimers, IRQ domains (`generic_handle_domain_irq`), delayed
  work disable/enable;
- device core: classes with files, software nodes, `device_for_each_child`, backlight and
  CEC notifier stubs;
- DMA: `dma_map_sgtable`, `dma_map_resource`, addressing limits.

The display core also needs kernel FPU sections (`kernel_fpu_begin/end`, saving the
extended state around DC's floating-point code). After linking, bring-up has to happen on
the reference machine: QEMU has no AMD GPU model.

NVMe is the one family on the roadmap that Nova drives natively instead of through Linux:
it is a single standard interface for every NVMe SSD, while Linux's driver sits on blk-mq and
the generic block layer, which are tied to Linux's VFS and page cache (Genode, whose approach
this layer follows, uses its own NVMe driver too). `drivers/nvme.rs` is polled with one admin
and one I/O queue pair and PRP lists; Nova installs onto and boots from NVMe
(`--case nvme`, `--case nvmeboot`). Drives formatted with 4 KiB logical blocks are readable
and writable, but not installation targets (the installer lays GPT out in 512-byte sectors).

Known limitations: one CPU, INTx routing for devices on the root bus only (others need
MSI), no bounce
buffers (DMA beyond a device's mask fails cleanly), linear `sk_buff`s only, extended PCI
configuration space unavailable, no hot unplug.
