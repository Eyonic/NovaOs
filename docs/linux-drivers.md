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
(`--case usbinput`, on a machine with no PS/2 controller). The GPU stack (DRM core, TTM,
`amdgpu` with its display core) is built in and linked (milestone 7a); it registers its
PCI driver at every boot but has only been run where no AMD GPU is present (QEMU), so
its probe is untested. Linux programs reach the DRM drivers through `/dev/dri` (milestone
8): libdrm's tools and Mesa (GBM, EGL, llvmpipe through `kms_swrast`) run on Linux's
software drivers vkms and vgem (`--case drm`), through the same code path amdgpu's nodes
take.

## Building

```sh
scripts/fetch-linux.sh                       # Linux 7.2.9 source into build/linux (~150 MB, sha256-checked)
FEATURES=linux-drivers scripts/build.sh      # configure, Kbuild the objects, link them into Nova
FEATURES=linux-drivers scripts/test.sh --case linuxnet
NOVA_NIC=rtl8139 scripts/run.sh              # boot with a card the Linux driver handles
NOVA_NIC=e1000e NOVA_CMDLINE=nova.disable_driver=e1000 FEATURES=linux-drivers scripts/build.sh
                                             # (e1000e: switch off Nova's own e1000 driver first)
NOVA_CMDLINE="amdgpu.dc=0" FEATURES=linux-drivers scripts/build.sh
                                             # Linux module parameters, as on Linux's command line
NOVA_CMDLINE="nova.disable_driver=amdgpu" FEATURES=linux-drivers scripts/build.sh
                                             # keep a Linux driver from probing
FEATURES=linux-drivers scripts/test.sh --case drm
                                             # DRM from Linux programs on vkms and vgem
NOVA_DRM_TOOLS=1 NOVA_CMDLINE="nova.mode=shell nova.vkms=1 nova.vgem=1" \
    FEATURES=linux-drivers scripts/build.sh  # with the host's modetest, vbltest, eglinfo,
                                             # Mesa (GBM/EGL/GLES, radeonsi, RADV) and,
                                             # with NOVA_KMSCUBE=<path>, a kmscube build
```

`scripts/build-linux.sh` (called by `build.sh`) does the Linux side:

1. `make tinyconfig`, merge `kernel/linuxkpi/nova.config`, `make olddefconfig`, and check that
   Kconfig kept every setting of the fragment;
2. `make prepare`;
3. Kbuild compiles the objects in `kernel/linuxkpi/linux-objects.txt` and lx_emul
   (copied to `build/linux/nova/`), so lx_emul gets exactly the
   flags and headers the Linux objects get. An entry ending in `/` is a whole directory
   (`drivers/gpu/`): Kbuild builds its `built-in.a`, a thin archive of everything the
   configuration enables there, and its members are linked;
4. the object list goes to `build/linux/nova-objects.list`, which `kernel/build.rs` passes to
   the linker as plain objects.

With the feature, `build.sh` also copies firmware from the host's linux-firmware package
into the image (never into the repository): Realtek's `rtl_nic` files and amdgpu's files for
Navi 21 and 22 (`sienna_cichlid_*`, `navy_flounder_*`; `NOVA_AMDGPU_FIRMWARE` picks other
chips), decompressed.

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
  `nova.disable_driver=<name>` keeps a Linux driver from probing too. Before a display
  driver probes, a line on the boot screen names it and that switch (a GPU driver takes the
  screen over). `driver.parameter=value` words on the kernel command line set built-in
  drivers' module parameters before the initcalls, as on Linux (`support.c`, from the
  `__param` section).
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
  Linux's own refcounting. Compound pages (folios > 1 page) are not supported: `__GFP_COMP`
  requests of order > 0 fail, and TTM's page pool falls back to smaller orders. `vmap()`
  maps scattered pages into a window at Linux's `VMALLOC_START` (PML4 slot 402,
  `memory::vmap`, first-fit with guard pages, unmapped again by `vunmap()`). Without
  `CONFIG_X86_PAT` Linux code asks for at most UC- instead of write-combining, and the
  direct map stays write-back (`set_pages_array_wc()` changes nothing; amdgpu turns its
  write-combined system-memory buffers off without PAT). shmem files (TTM's swap storage)
  keep their pages in RAM; shrinkers are registered but never called.
* **Files** (`fs.c`): DRM and dma-buf hand out file descriptors (PRIME buffers, sync files,
  leases). They are real `struct file`s with Linux's reference counting and release order
  (`f_op->release`, the dentry's `d_release`, the inode), on anonymous inodes and pseudo
  file systems (`kern_mount`, `init_pseudo`). Linux programs use them as their own
  descriptors (`user.c`, `kernel/src/linux/lxfile.rs`; see milestone 8 below).
* **Kernel FPU sections** (`fpu.c`): Nova's kernel never uses FPU/SSE/AVX registers (soft-float
  Rust, `-mgeneral-regs-only` C), so their live contents belong to a user program.
  `kernel_fpu_begin()` saves them (XSAVE of every enabled component, else FXSAVE) and loads
  clean x87/MXCSR state; `kernel_fpu_end()` restores them. Sections nest (an interrupt
  handler inside a task's section) with one save area per level. amdgpu's display core
  computes clocks and bandwidth this way. One section runs at every boot as a self-check.
* **Tasks and sleeping** (`task.c`): every Nova task gets a `task_struct` when it is
  created (freed after it ends, past an RCU grace period), and Nova's scheduler points
  Linux's `current` at it on every switch. All share one `signal_struct` (no resource
  limits, no pids) and lead their own thread group.
  `schedule()`, `wake_up_process()`, wait queues, completions, semaphores and rwsems follow
  Linux's protocol on top of Nova's park/wake. Work items run on a pool of four worker
  tasks, so one that sleeps (a USB control transfer waiting for its completion, delivered
  by another work item) does not block the rest, as with Linux's worker pools. kthreads
  and kthread workers are Nova tasks (`nova_task_spawn`) with their own `task_struct`,
  started stopped as on Linux. hrtimers and irq_work run from the "linux" task, so their
  resolution is the 1 ms tick. Wound/wait mutexes (`dma_resv`, modeset locks) implement
  both wait-die and wound-wait backoff with `-EDEADLK`.
* **IRQ domains** (`irq.c`): virtual IRQ numbers (0x2000 + n) for sources a driver
  demultiplexes itself (amdgpu forwards its audio co-processor's interrupts this way).
* **Driver core** (`device.c`): buses, drivers, match-then-probe on `device_add()` and
  `driver_register()`, reference-counted devices with release functions, real devres lists
  and groups. sysfs is enabled in the configuration (the USB hub driver treats a missing
  sysfs node as an error) and implemented as placeholders that accept everything.
  Free-standing kobjects (amdgpu's IP discovery tree) are reference counted and released
  through their `kobj_type`; classes, hwmon devices and software nodes exist as far as
  drivers keep and release them.
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
  `dma_map_sgtable()` and `dma_map_resource()` map physically too; a device is
  "addressing limited" (TTM then allocates below 4 GiB) exactly when it gets that pool.

## Adding a driver

1. Enable it in `kernel/linuxkpi/nova.config` and add its object(s) to `linux-objects.txt`
   (or its directory, for a subsystem as large as DRM).
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
| 7 | GPU | DRM core, TTM, `amdgpu` (incl. display core), linked (7a) | `struct page` backing and page allocation, kernel FPU sections, firmware, KMS to Nova's compositor |
| 8 | Userspace for games | `vgem`, `vkms` (testing) | DRM nodes, ioctls, poll, dma-buf/sync-file descriptors and mmap of GPU buffers for Linux programs (done in QEMU); first run on the RX 6800 |
| 9 | Steam | Mesa RADV, Steam runtime, Proton (userspace) | a Wayland compositor or Xwayland path, audio (HDA via ALSA), namespaces for the Steam runtime |

### Milestone 7 status: amdgpu links (7a)

nova.config enables amdgpu with its display core (SI/CIK support, userptr, fbdev emulation
and the KFD compute driver off). `linux-objects.txt` takes whole directories: everything
the configuration enables in `drivers/gpu/` (DRM core, TTM, the display helpers, the GPU
scheduler and buddy allocator, amdgpu, the display core, 989 objects), `drivers/dma-buf/`,
`drivers/i2c/` and `drivers/video/` (HDMI infoframes, the backlight class). Library code
came along as is (`lib/rbtree.c`, `sort.c`, `list_sort.c`, `math/gcd.c`, `kfifo.c`,
`llist.c`, `mm/mempool.c`, the x86 `memmove`/`copy_page`/`clear_page`); the
other ~210 missing symbols are lx_emul: files (`fs.c`), folios, vmap, shmem, page attributes,
LRU lists and shrinkers (`mm.c`), hrtimers, irq_work and delayed-work controls
(`sched.c`), kthreads and kthread workers (`task.c`), IRQ domains (`irq.c`), wound/wait
and RT mutexes (`base.c`), kobjects, classes, hwmon and software nodes (`device.c`), PCIe
link queries, the expansion ROM, saved states and resizable-BAR queries (`pci.c`), kernel
FPU sections (`fpu.c`), and stubs for CEC, perf, power supplies and the VGA arbiter
(`gpu.c`). The kernel links with no unresolved symbol.

In QEMU (no AMD GPU), amdgpu registers its PCI driver and binds nothing; both test suites
pass, `--case linuxnet` also checks that `amdgpu.dc=0` on the command line reaches the
driver's parameter. **Everything past registration is untested**: the first probe will
happen on the reference machine. What to expect there, most likely problems first:

- **The screen.** Nova draws on the firmware framebuffer, which lives in VRAM. When amdgpu
  initializes the display core it reprograms the display pipes, and nothing hands the
  new scan-out back to Nova's compositor yet (KMS to the compositor is the next step), so
  the screen may go dark during the probe. A line on the boot screen announces the probe;
  `nova.disable_driver=amdgpu` boots without it, and `amdgpu.dc=0` brings up the GPU
  without touching the display (the firmware framebuffer should stay). With the screen
  dark, the log is only on COM1 (`dmesg` keeps it, but cannot be shown).
- **Firmware** is read from the image's system pak while drivers probe (the system files
  are registered in the file system later); a missing file fails the probe with -ENOENT
  and a "firmware: ... not found" line.
- **VBIOS**: there is no ACPI VFCT table for Linux code, so amdgpu reads the VBIOS from
  the ROM BAR (`pci_map_rom`, if the firmware assigned it an address) or through its own
  ROM registers.
- **Memory**: the visible VRAM BAR is mapped uncached in 4 KiB pages (`ioremap_wc` is
  uncached without PAT, and MMIO mappings are never removed); with Resizable BAR enabled
  that is 16 GiB (32 MiB of page tables, a slow start) and slow CPU access to VRAM.
  `set_pages_array_wc/uc()` leave the direct map write-back.
- **Topology**: only functions Nova hands over are visible to Linux code, not the bridges
  above the GPU (Navi 21 sits behind its own PCIe switch), so link-speed queries see the
  GPU's own link only, and AtomicOps routing is reported unsupported.
- **Execution**: kthreads, hrtimers (1 ms resolution), irq_work and wound/wait mutexes are
  new and only exercised by the boot path so far; a kernel FPU section that sleeps would
  corrupt user FPU state (Linux forbids it too).

### Milestone 8 status: programs use the GPU

The files Linux drivers create are ordinary descriptors of Nova's Linux programs, and
everything libdrm, Mesa and the Vulkan drivers do with them reaches the drivers:

- **Device nodes.** `register_chrdev()`/`cdev_add()` keep their file operations and every
  device added with a device number is listed (`user.c`). DRM's minors appear as
  `/dev/dri/card*` and `/dev/dri/renderD*` (character devices, `st_rdev` 226:N), and in
  `/sys` as `drmGetDevices2()` reads them: `/sys/dev/char/226:N` and `/sys/class/drm/*`
  (links to the minor under its parent), the minor's `uevent` (`DEVNAME=dri/...`) and
  `dev`, the parent's `subsystem` link, `uevent` and `drm/` directory, and for a PCI
  parent its `vendor`, `device`, `subsystem_vendor`, `subsystem_device`, `revision`,
  `class`, `config` and `boot_vga` from the real function. amdgpu's card and render nodes
  come out of this code path like vkms's and vgem's: on the RX 6800 radeonsi and RADV see
  `/dev/dri/renderD128` with vendor 0x1002 and Navi 21's device id.
- **Open files.** A descriptor (`Kind::Linux`) holds a `struct lx_ufile`, which owns one
  reference to the driver's `struct file`: dup, fork and SCM_RIGHTS share one open file,
  and the driver's release runs with the last close. Open runs `f_op->open` on the
  node's inode (as `chrdev_open`), ioctl `unlocked_ioctl`, and for 32-bit programs
  `compat_ioctl` with `TS_COMPAT` set (`CONFIG_IA32_EMULATION`: DRM's `drm_ioc32.c`
  converts `VERSION`, `WAIT_VBLANK` and the other ioctls whose layouts differ). read
  (DRM events), write, llseek (a dma-buf's size at `SEEK_END`), `F_SETFL`/`FIONBIO`
  (`O_NONBLOCK` in `f_flags`) and fstat work. poll, select and epoll call `f_op->poll`;
  the file stays on the wait queues the driver names with `poll_wait()` until it closes,
  and their wakeups, from interrupts too, wake Nova's waiters and count for epoll's
  edge-triggered mode.
- **Descriptors made by Linux code.** `get_unused_fd_flags()` reserves a slot in the
  calling program's table, `fd_install()` fills it (dma-buf and sync-file exports, with
  `O_CLOEXEC` honoured); `fget()`/`fdget()` find the struct file behind a descriptor a
  program passes in (PRIME import, syncobj import, `DMA_BUF_IOCTL_*`).
- **Mappings.** mmap makes a `vm_area_struct` for the Nova mapping (the process's
  `Memory` keeps it with the range) and runs `f_op->mmap`. A fault in it (the program's,
  or Linux code's touching user memory, which Nova's kernel fault handler now serves)
  calls `vm_ops->fault` without the address-space lock; `vmf_insert_pfn(_prot)`,
  `vmf_insert_mixed`, `vm_insert_page`, `remap_pfn_range` (also `io_remap_pfn_range`)
  and handlers that return a page map frames with the caching bits the driver asks for.
  `unmap_mapping_range()` zaps a file range in every address space that maps it (TTM
  moving or evicting a buffer, shmem purges); the next access faults again. munmap, exit
  and exec retire the vma, and `vm_ops->close` runs once no fault is in flight, on a task
  of its own ("linux-files": a close can drop the last reference to a DRM file and wait
  for a vblank that the "linux" task delivers). fork gives the child its own vma
  (`vm_ops->open`); its pages fault in again there. munmap of a mapping's front or back
  trims the vma.
- **User memory.** `copy_from_user()`, `copy_to_user()`, `get_user()` and `put_user()`
  check the range against the program's mappings (kernel pointers still pass) and return
  `-EFAULT` instead of trusting it; pages not present yet are filled on first touch.

**Testing without a GPU.** Linux's vkms (a virtual display with vblank, atomic KMS) and
vgem (GEM buffers, PRIME, fences) are built in; they sit on faux devices, which are only
created with `nova.vkms=1` / `nova.vgem=1` on the command line, so real machines never
see a fake GPU (`--case linuxnet` checks that none appears). `--case drm` runs
`userland/drmtest.c` (KMS enumeration, dumb buffers drawn through mmap, an atomic modeset
and ten page flips paced at 16.7 ms by vblank with events read after poll, legacy flips,
vblank waits with events, PRIME both ways between vkms and vgem, dma-buf mmap and sync,
a vgem fence seen through a sync file and dma-buf poll and signalled by another
process, a dma-buf passed over SCM_RIGHTS, a mapping inherited by fork) and
`userland/compat32/drmtest32.c` (the converted ioctls, a GEM offset above 4 GiB mapped
through mmap2) from a 32-bit program.

With the host's tools in the image (`NOVA_DRM_TOOLS=1`), unmodified programs run on vkms:
`modetest -M vkms` lists connectors, modes, planes and properties and sets modes;
`vbltest -M vkms` counts 60 Hz vblanks (while a mode is set; with every CRTC off it fails,
as on Linux); `eglinfo` brings up Mesa's GBM platform on the vkms node through
`kms_swrast` with llvmpipe (OpenGL 4.6, GLES 3.2); kmscube renders through GBM and EGL at
60 frames per second with page flips on vkms. What does not work there is not Nova's:
`kmscube -A` needs `EGL_ANDROID_native_fence_sync`, which llvmpipe lacks; offscreen
rendering on vgem's render node fails because dumb buffers are not allowed on render
nodes; `vulkaninfo` finds no GPU (there is none, and lavapipe is not in the image).

**What remains for radeonsi and RADV on the RX 6800**, beyond amdgpu's own first probe
(milestone 7), and what QEMU cannot show:

- *VRAM through the BAR.* CPU mappings of VRAM buffers are TTM faults that insert BAR
  page frames with `vmf_insert_pfn_prot()`; Nova maps them with the driver's PWT/PCD/PAT
  bits, which without `CONFIG_X86_PAT` means uncached (UC-) rather than write-combined:
  correct but slow for uploads. The visible-VRAM window and resizable BAR set its size.
- *Fences and waits.* `AMDGPU_WAIT_CS`, `DRM_IOCTL_SYNCOBJ_WAIT` and timeline syncobjs
  are DRM core and amdgpu code driven by the GPU's interrupts and the GPU scheduler's
  kthreads, so they should work as fence waits do on vgem, but only real interrupts will
  show it. Waits are not interruptible by signals (lx_emul's tasks never have one
  pending), so a program killed during a GPU hang stays until the wait times out.
  `DRM_IOCTL_SYNCOBJ_EVENTFD` fails (no eventfd behind Linux code; Mesa falls back).
- *userptr/HMM* (`AMDGPU_GEM_USERPTR`, used for `VK_EXT_external_memory_host` and some
  OpenCL paths) stays off: it needs MMU notifiers on Nova's address spaces. RADV and
  radeonsi check for it and do without.
- *Process identity.* Each thread is its own Linux thread group: per-process accounting
  in amdgpu (fdinfo, the scheduler entity's last user) sees threads, not processes, and
  DRM files record no pid.
- *Huge mappings.* Only 4 KiB pages are inserted (`CONFIG_ARCH_SUPPORTS_PMD_PFNMAP` is
  off; `get_unmapped_area` alignment is Nova's own), so large buffers fault in 4 KiB
  steps (TTM prefaults 16 at a time).
- *Mesa's view.* The image needs Mesa's radeonsi and RADV with the Vulkan loader
  (`NOVA_DRM_TOOLS=1` copies the host's) and the firmware that `build.sh` already ships.

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
