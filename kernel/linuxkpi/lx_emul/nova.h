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

/* ---- provided by Nova (Rust) ---- */

/* Memory. nova_alloc returns direct-map memory (DMA-capable, physically contiguous). */
void *nova_alloc(size_t size, size_t align, bool zero, bool dma32);
void nova_free(const void *ptr);
size_t nova_alloc_size(const void *ptr);
u64 nova_hhdm_offset(void);
u64 nova_kernel_phys_base(void);
void *nova_map_mmio(u64 phys, u64 len, bool write_combine);

/* PCI configuration space (legacy mechanism, 256 bytes per function). */
u32 nova_pci_read(u8 bus, u8 devfn, u16 offset, u8 width);
void nova_pci_write(u8 bus, u8 devfn, u16 offset, u8 width, u32 value);

/* Time and scheduling. */
u64 nova_nanos(void);
void nova_ndelay(u64 ns);
void nova_sleep_ticks(u64 ticks);
void nova_yield(void);
/* Sleep until nova_wake(this task) or the next interrupt. */
void nova_park(void);
void nova_wake(u64 task_id);
u64 nova_task_id(void);

/* Log one line (without the trailing newline) to the serial console and the log. */
void nova_log(int level, const char *msg, size_t len);
u64 nova_random_u64(void);

/* Interrupts: unmask (enable) or mask a PCI line at Nova's interrupt controller. */
void nova_irq_line(unsigned int line, bool enable);

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
/* The Linux driver bound to device (bus, devfn), or NULL. */
const char *nova_lx_pci_driver(u8 bus, u8 devfn);
/* Nova's scheduler switched to task `task_id`: point Linux's `current` at it. */
void nova_lx_switch(u64 task_id);
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

/* Interrupt bookkeeping shared between the lx_emul files. */
void lx_emul_irq_poll(void);
void lx_emul_napi_poll(void);
void lx_emul_timers_run(void);
void lx_emul_work_run(void);

#endif
