/* SPDX-License-Identifier: MIT OR Apache-2.0 */
/*
 * The bridge between lx_emul (C, built against the real Linux headers) and Nova's Rust
 * core. Functions named nova_* are implemented in Rust (kernel/src/linuxkpi.rs); functions
 * named nova_lx_* are implemented here in C and called from Rust.
 *
 * Rule of thumb: Linux data structures (struct pci_dev, net_device, sk_buff, ...) are only
 * ever touched from C, so their layout stays defined in one place, the Linux headers.
 * Rust sees them as opaque pointers.
 */
#ifndef NOVA_LX_EMUL_H
#define NOVA_LX_EMUL_H

#include <linux/types.h>

struct device;

/* Slab caches: lx_emul keeps only what allocation needs (base.c, support.c). */
struct kmem_cache {
	const char *name;
	unsigned int size, align;
	void (*ctor)(void *);
};

/* Run RCU callbacks queued by call_rcu() (support.c). */
void lx_emul_rcu_run(void);

/* ---- provided by Nova (Rust) ---- */

/* Memory. nova_alloc returns direct-map memory (DMA-capable, physically contiguous). */
void *nova_alloc(size_t size, size_t align, bool zero, bool dma32);
void nova_free(const void *ptr);
size_t nova_alloc_size(const void *ptr);
u64 nova_hhdm_offset(void);
u64 nova_kernel_phys_base(void);
void *nova_map_mmio(u64 phys, u64 len, bool write_combine);
/* vmap window: `count` pages at the physical addresses `phys`, with PWT/PCD bits. */
void *nova_vmap(const u64 *phys, size_t count, u64 pte_flags);
void nova_vunmap(const void *addr);
void nova_mem_info(u64 *total, u64 *free);
bool nova_phys_is_ram(u64 phys);

/* PCI configuration space (legacy mechanism, 256 bytes per function). */
u32 nova_pci_read(u8 bus, u8 devfn, u16 offset, u8 width);
void nova_pci_write(u8 bus, u8 devfn, u16 offset, u8 width, u32 value);

/* Time and scheduling. */
u64 nova_nanos(void);
/* Seconds since 1970 (UTC). */
u64 nova_unix_time(void);
/* Bytes XSAVE writes for the enabled state components; 0 without XSAVE (use FXSAVE). */
u64 nova_fpu_state_size(void);
void nova_ndelay(u64 ns);
void nova_sleep_ticks(u64 ticks);
void nova_yield(void);
/* Sleep until nova_wake(this task) or the next interrupt. */
void nova_park(void);
void nova_wake(u64 task_id);
u64 nova_task_id(void);
/* A new kernel task running entry(arg); returns its id. It runs once others yield. */
u64 nova_task_spawn(const char *name, void (*entry)(void *arg), void *arg);

/* Log one line (without the trailing newline) to the serial console and the log. */
void nova_log(int level, const char *msg, size_t len);
u64 nova_random_u64(void);

/* Interrupts: unmask (enable) or mask a PCI line at Nova's interrupt controller. */
void nova_irq_line(unsigned int line, bool enable);

/* Firmware: the bytes of /lib/firmware/<name> in a nova_alloc() buffer (freed with
 * nova_free), or NULL. */
void *nova_firmware_read(const char *name, size_t name_len, size_t *len);

/* The kernel command line (not NUL-terminated). */
const char *nova_cmdline(size_t *len);
/* Whether `nova.disable_driver=` names this driver. */
bool nova_driver_disabled(const char *name, size_t len);
/* A line on the boot console (screen, serial and dmesg), printf-style. */
void nova_console_line(const char *msg, size_t len);
__printf(1, 2) void nova_console_notice(const char *fmt, ...);

/* Network: a frame the Linux driver received, for Nova's network stack. */
void nova_netdev_rx(void *dev, const u8 *frame, size_t len);

/* ---- provided by lx_emul (C), called from Rust ---- */

/* Set up the memory layout variables; first call into lx_emul. */
void nova_lx_init(void);
/* Register a PCI function that Nova's own drivers did not claim. */
void nova_lx_pci_add(u8 bus, u8 devfn, u16 vendor, u16 device, u16 subvendor, u16 subdevice,
		     u32 class_rev, u8 irq);
/* Run Linux initcalls: drivers register and probe the added devices. */
void nova_lx_start(void);
/* Apply "driver.parameter=value" words of the kernel command line (support.c). */
void nova_lx_params(void);
/* The Linux driver bound to device (bus, devfn), or NULL. */
const char *nova_lx_pci_driver(u8 bus, u8 devfn);
/* Nova's scheduler switched to a task: point Linux's `current` at its task_struct
 * (from nova_lx_task_new; NULL for tasks without one). */
void nova_lx_switch(void *task);
/* A task_struct for new Nova task `id`, and the end of its task (task.c). */
void *nova_lx_task_new(u64 id);
void nova_lx_task_exit(void *task);
/* Advance jiffies; called from Nova's timer interrupt. */
void nova_lx_tick(void);
/* Run the Linux handlers on a PCI line (from Nova's IRQ dispatcher, interrupts off). */
bool nova_lx_irq(unsigned int line);
/* One iteration of deferred work: threaded handlers, NAPI, timers and work items. */
void nova_lx_poll(void);

/* Network devices registered by Linux drivers, in registration order. */
void *nova_lx_netdev_get(unsigned int index);
int nova_lx_netdev_open(void *dev);
void nova_lx_netdev_mac(void *dev, u8 mac[6]);
bool nova_lx_netdev_carrier(void *dev);
bool nova_lx_netdev_tx_ready(void *dev);
/* 0 = queued, 1 = queue full (try again later), negative = dropped. */
int nova_lx_netdev_xmit(void *dev, const u8 *frame, size_t len);

/* Linux files for Linux programs (user.c, called from kernel/src/linux/lxfile.rs). */
struct lx_ufile;
struct lx_vma;
struct page;
struct file;

/* What Nova needs to show a device node in /dev and /sys (lxfile.rs keeps the layout). */
struct nova_devnode {
	char path[64];		/* under /dev, from the class's devnode(): "dri/card0" */
	char name[32];		/* the device's name: "card0" */
	char parent[64];	/* the parent device's name: "0000:03:00.0", "vkms" */
	char bus[16];		/* the parent's bus: "pci", "faux", "platform", "" */
	char driver[32];	/* the parent's driver: "amdgpu", "" */
	u32 major, minor;
	u16 vendor, device, subvendor, subdevice;	/* PCI parents */
	u32 class_code;
	u8 revision;
};

/* Device node `index` (in the order they were added); false past the end. */
bool nova_lx_devnode(unsigned int index, struct nova_devnode *out);

/* Open device node `devt`: the open file, or NULL and *err. */
struct lx_ufile *nova_lx_ufile_open(u32 devt, u32 flags, int *err);
/* The last descriptor is gone (f_op->release when it was the last file reference). */
void nova_lx_ufile_close(struct lx_ufile *uf);
long nova_lx_ufile_ioctl(struct lx_ufile *uf, u32 cmd, u64 arg, bool compat);
long nova_lx_ufile_read(struct lx_ufile *uf, u64 buf, u64 len);
long nova_lx_ufile_write(struct lx_ufile *uf, u64 buf, u64 len);
s64 nova_lx_ufile_llseek(struct lx_ufile *uf, s64 offset, int whence);
/* EPOLL* bits ready now, and the number of wakeups so far in *events. */
u32 nova_lx_ufile_poll(struct lx_ufile *uf, u64 *events);
void nova_lx_ufile_setfl(struct lx_ufile *uf, u32 flags);
/* For fstat and /proc/self/fd: the inode's mode, device number and number, the size
 * (llseek to the end, as for a dma-buf), and a name ("card0", "dmabuf", "sync_file"). */
struct nova_ufile_info {
	u32 mode;
	u32 rdev_major, rdev_minor;
	u64 ino;
	char name[32];
	bool anon;
};

void nova_lx_ufile_info(struct lx_ufile *uf, struct nova_ufile_info *info);
struct file *nova_lx_ufile_get(struct lx_ufile *uf);
/* For the __get_user/__put_user thunks: may Linux code access this user range? */
bool nova_lx_user_ok(unsigned long addr, unsigned long len, bool write);

/* mmap() of an open file at [addr, addr + len) in Nova address space `space`: a new
 * vma, then the driver's mmap on it (0 or -errno; a failed one is freed with _free). */
struct lx_vma *nova_lx_vma_new(struct lx_ufile *uf, u64 space, u64 addr, u64 len, u32 prot, u32 flags, u64 pgoff,
			       int *err);
int nova_lx_vma_mmap(struct lx_vma *v);
void nova_lx_vma_free(struct lx_vma *v);
bool nova_lx_vma_writable(struct lx_vma *v);
/* A fault at addr: 0 when the page is mapped now, else VM_FAULT_* bits. */
u32 nova_lx_vma_fault(struct lx_vma *v, u64 addr, bool write);
void nova_lx_vma_trim(struct lx_vma *v, u64 start, u64 end);
void nova_lx_vma_detach(struct lx_vma *v);
void nova_lx_vma_close(struct lx_vma *v);
struct lx_vma *nova_lx_vma_dup(struct lx_vma *v, u64 space);
void nova_lx_put_page(struct page *page);

/* Interrupt bookkeeping shared between the lx_emul files. */
void lx_emul_irq_poll(void);
void lx_emul_irq_handle(unsigned int irq);
void lx_emul_napi_poll(void);
void lx_emul_timers_run(void);
void lx_emul_hrtimers_run(void);
void lx_emul_irq_work_run(void);
/* Devices with a device number (user.c): their nodes appear in Nova's /dev. */
void lx_devnode_add(struct device *dev);
void lx_devnode_del(struct device *dev);
/* Memory statistics from Nova (mm.c); called by nova_lx_init. */
void lx_emul_mm_init(void);
/* One work-queue worker task (index n < 4); never returns. */
void nova_lx_worker(unsigned int n);

#endif
