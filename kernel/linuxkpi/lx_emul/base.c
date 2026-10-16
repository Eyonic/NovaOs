// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul base: the lowest layer of the Linux kernel API, implemented on Nova's Rust core.
 * Memory allocation, printk, delays, mutexes, RCU, bottom halves, preemption, random
 * numbers. Every function here is defined against its real Linux prototype (the headers
 * are Linux's own), so a signature change in a new Linux release is a compile error.
 *
 * Nova runs these on one CPU with cooperative kernel tasks, and Linux code only runs in
 * the lx_emul task or in a Nova task calling into a driver. Nothing preempts Linux code,
 * so spinlocks need no atomic loop, RCU readers can never be interrupted by a grace period,
 * and "interrupt context" (polled handlers) is ordinary task context.
 */
#include <linux/slab.h>
#include <linux/string.h>
#include <linux/printk.h>
#include <linux/delay.h>
#include <linux/mutex.h>
#include <linux/rcupdate.h>
#include <linux/srcu.h>
#include <linux/bottom_half.h>
#include <linux/random.h>
#include <linux/uaccess.h>
#include <linux/device.h>
#include <linux/moduleparam.h>
#include <linux/sched.h>
#include <linux/jiffies.h>
#include <linux/fwnode.h>
#include <linux/property.h>
#include <linux/mmdebug.h>
#include <linux/tracepoint-defs.h>
#include <linux/trace_events.h>
#include <linux/rtnetlink.h>
#include <linux/netdevice.h>
#include <linux/timekeeping.h>
#include <linux/math64.h>
#include <asm/processor.h>
#include <asm/cpu.h>
#include <asm/cpuid/api.h>
#include "nova.h"

static void lx_identify_cpu(void);

/* ---------------------------------------------------------------- layout */

/* With CONFIG_RANDOMIZE_MEMORY these are variables; Linux's __pa()/__va() use them.
 * The direct map is Limine's HHDM, and the kernel image's physical base is where Limine
 * loaded Nova (the image is linked at __START_KERNEL_map, 0xffffffff80000000). */
unsigned long page_offset_base;
unsigned long vmalloc_base;
unsigned long vmemmap_base;
unsigned long phys_base;

void nova_lx_init(void)
{
	page_offset_base = nova_hhdm_offset();
	phys_base = nova_kernel_phys_base();
	/* Linux's default vmemmap base (PML4 slot 468). Entries are backed with memory on
	 * demand, when pages are allocated (mm.c); for other memory virt_to_page() and
	 * page_to_phys() are pure arithmetic, which is all streaming DMA mapping needs. */
	vmemmap_base = 0xffffea0000000000UL; /* memory.rs VMEMMAP_BASE */
	lx_identify_cpu();
}

/* Nova has no vmalloc area: every kernel allocation is in the direct map. */
bool is_vmalloc_addr(const void *x)
{
	return false;
}

/* ---------------------------------------------------------------- memory */

/* kmalloc guarantees natural alignment for power-of-two sizes (up to a page) and
 * ARCH_KMALLOC_MINALIGN otherwise; drivers rely on it for DMA descriptors. */
static void *lx_kmalloc(size_t size, gfp_t flags)
{
	size_t align = ARCH_KMALLOC_MINALIGN;

	if (!size)
		return ZERO_SIZE_PTR;
	if (is_power_of_2(size))
		align = size < PAGE_SIZE ? size : PAGE_SIZE;
	if (align < 16)
		align = 16;
	return nova_alloc(size, align, flags & __GFP_ZERO, flags & (GFP_DMA32 | GFP_DMA));
}

/* The objects only use the kmalloc caches as tokens for their size class. */
kmem_buckets kmalloc_caches[NR_KMALLOC_TYPES];

void *__kmalloc_noprof(DECL_TOKEN_PARAMS(size, token), gfp_t flags)
{
	return lx_kmalloc(size, flags);
}

void *__kmalloc_node_noprof(DECL_KMALLOC_PARAMS(size, b, token), gfp_t flags, int node)
{
	return lx_kmalloc(size, flags);
}

void *__kmalloc_cache_noprof(struct kmem_cache *s, gfp_t flags, size_t size)
{
	return lx_kmalloc(size, flags);
}

void *__kmalloc_cache_node_noprof(struct kmem_cache *s, gfp_t gfpflags, int node, size_t size)
{
	return lx_kmalloc(size, gfpflags);
}

void *__kmalloc_large_noprof(size_t size, gfp_t flags)
{
	return nova_alloc(size, PAGE_SIZE, flags & __GFP_ZERO, flags & (GFP_DMA32 | GFP_DMA));
}

void *__kmalloc_large_node_noprof(size_t size, gfp_t flags, int node)
{
	return __kmalloc_large_noprof(size, flags);
}

void *__kmalloc_node_track_caller_noprof(DECL_KMALLOC_PARAMS(size, b, token), gfp_t flags, int node,
					 unsigned long caller)
{
	return lx_kmalloc(size, flags);
}

void *__kvmalloc_node_noprof(DECL_KMALLOC_PARAMS(size, b, token), unsigned long align, gfp_t flags, int node)
{
	if (align < 16)
		align = 16;
	return nova_alloc(size, align, flags & __GFP_ZERO, false);
}

void kfree(const void *objp)
{
	if (ZERO_OR_NULL_PTR(objp))
		return;
	nova_free(objp);
}

void kvfree(const void *addr)
{
	kfree(addr);
}

void kfree_sensitive(const void *objp)
{
	if (ZERO_OR_NULL_PTR(objp))
		return;
	memzero_explicit((void *)objp, nova_alloc_size(objp));
	kfree(objp);
}

size_t ksize(const void *objp)
{
	return ZERO_OR_NULL_PTR(objp) ? 0 : nova_alloc_size(objp);
}

char *kstrdup(const char *s, gfp_t gfp)
{
	return s ? kmemdup_noprof(s, strlen(s) + 1, gfp) : NULL;
}

const char *kstrdup_const(const char *s, gfp_t gfp)
{
	return kstrdup(s, gfp);
}

void kfree_const(const void *x)
{
	kfree(x);
}

char *kstrndup(const char *s, size_t max, gfp_t gfp)
{
	size_t len;
	char *buf;

	if (!s)
		return NULL;
	len = strnlen(s, max);
	buf = lx_kmalloc(len + 1, gfp);
	if (buf) {
		memcpy(buf, s, len);
		buf[len] = '\0';
	}
	return buf;
}

void *kmemdup_noprof(const void *src, size_t len, gfp_t gfp)
{
	void *p = lx_kmalloc(len, gfp);

	if (p)
		memcpy(p, src, len);
	return p;
}

/* All Linux code in Nova runs in the kernel: "user" pointers are kernel pointers. */
unsigned long _copy_from_user(void *to, const void __user *from, unsigned long n)
{
	memcpy(to, (const void __force *)from, n);
	return 0;
}

unsigned long _copy_to_user(void __user *to, const void *from, unsigned long n)
{
	memcpy((void __force *)to, from, n);
	return 0;
}

void *memdup_user_nul(const void __user *src, size_t len)
{
	char *p = lx_kmalloc(len + 1, GFP_KERNEL);

	if (!p)
		return ERR_PTR(-ENOMEM);
	memcpy(p, (const void __force *)src, len);
	p[len] = '\0';
	return p;
}

/* ---------------------------------------------------------------- printk */

static void lx_log(const char *text)
{
	int level = LOGLEVEL_DEFAULT;
	const char *start = text;
	size_t len;

	/* KERN_<LEVEL> is "\001" followed by a digit. */
	while (printk_get_level(start)) {
		char c = printk_get_level(start);

		if (c >= '0' && c <= '7')
			level = c - '0';
		start = printk_skip_level(start);
	}
	len = strlen(start);
	while (len && start[len - 1] == '\n')
		len--;
	if (level <= LOGLEVEL_INFO || level == LOGLEVEL_DEFAULT)
		nova_log(level, start, len);
}

int vprintk(const char *fmt, va_list args)
{
	char buf[512];
	int n = vscnprintf(buf, sizeof(buf), fmt, args);

	lx_log(buf);
	return n;
}

int _printk(const char *fmt, ...)
{
	va_list args;
	int n;

	va_start(args, fmt);
	n = vprintk(fmt, args);
	va_end(args);
	return n;
}

static void lx_dev_printk(const char *level, const struct device *dev, const char *fmt, va_list ap)
{
	va_list args;
	struct va_format vaf = { .fmt = fmt, .va = &args };

	va_copy(args, ap);
	if (dev && dev->driver)
		_printk("%s%s %s: %pV", level, dev->driver->name, dev_name(dev), &vaf);
	else if (dev)
		_printk("%s%s: %pV", level, dev_name(dev), &vaf);
	else
		_printk("%s(NULL device): %pV", level, &vaf);
	va_end(args);
}

#define LX_DEV_PRINTK(fn, level)					\
	void fn(const struct device *dev, const char *fmt, ...)		\
	{								\
		va_list args;						\
		va_start(args, fmt);					\
		lx_dev_printk(level, dev, fmt, args);			\
		va_end(args);						\
	}

LX_DEV_PRINTK(_dev_emerg, KERN_EMERG)
LX_DEV_PRINTK(_dev_alert, KERN_ALERT)
LX_DEV_PRINTK(_dev_crit, KERN_CRIT)
LX_DEV_PRINTK(_dev_err, KERN_ERR)
LX_DEV_PRINTK(_dev_warn, KERN_WARNING)
LX_DEV_PRINTK(_dev_notice, KERN_NOTICE)
LX_DEV_PRINTK(_dev_info, KERN_INFO)

/* ---------------------------------------------------------------- time */

/* jiffies and jiffies_64 are the same 64-bit counter (jiffies is an alias, as in Linux's
 * vmlinux.lds), advanced by Nova's timer tick (pit.rs calls nova_lx_tick). */
u64 jiffies_64 __cacheline_aligned_in_smp;
asm(".globl jiffies\n.set jiffies, jiffies_64");

void nova_lx_tick(void)
{
	jiffies_64++;
}

/* udelay(n) passes n * 0x10c7 (2^32 / 10^6, rounded up), ndelay(n) n * 5 (2^32 / 10^9). */
unsigned long __msecs_to_jiffies(const unsigned int m)
{
	if ((int)m < 0)
		return MAX_JIFFY_OFFSET;
	return _msecs_to_jiffies(m);
}

unsigned long __usecs_to_jiffies(const unsigned int u)
{
	if (u > jiffies_to_usecs(MAX_JIFFY_OFFSET))
		return MAX_JIFFY_OFFSET;
	return _usecs_to_jiffies(u);
}

void __const_udelay(unsigned long xloops)
{
	nova_ndelay(((u64)xloops * 1000000000ULL) >> 32);
}

void __udelay(unsigned long usecs)
{
	nova_ndelay((u64)usecs * 1000);
}

void __ndelay(unsigned long nsecs)
{
	nova_ndelay(nsecs);
}

void __delay(unsigned long loops)
{
	nova_ndelay(loops);
}

void msleep(unsigned int msecs)
{
	nova_sleep_ticks(msecs_to_jiffies(msecs) + 1);
}

unsigned long msleep_interruptible(unsigned int msecs)
{
	msleep(msecs);
	return 0;
}

long schedule_timeout_uninterruptible(long timeout)
{
	nova_sleep_ticks(timeout);
	return 0;
}

long schedule_timeout_interruptible(long timeout)
{
	return schedule_timeout_uninterruptible(timeout);
}

void usleep_range_state(unsigned long min, unsigned long max, unsigned int state)
{
	/* Below one timer tick, wait actively; a sleep would round up to 10 ms. */
	if (max < 1000000 / HZ)
		nova_ndelay((u64)min * 1000);
	else
		nova_sleep_ticks(usecs_to_jiffies(min) + 1);
}

unsigned long round_jiffies(unsigned long j)
{
	return roundup(j, HZ);
}

unsigned long round_jiffies_relative(unsigned long j)
{
	return round_jiffies(j + jiffies) - jiffies;
}

struct timespec64 ns_to_timespec64(s64 nsec)
{
	struct timespec64 ts = { 0, 0 };
	s32 rem;

	if (!nsec)
		return ts;
	ts.tv_sec = div_s64_rem(nsec, NSEC_PER_SEC, &rem);
	if (rem < 0) {
		ts.tv_sec--;
		rem += NSEC_PER_SEC;
	}
	ts.tv_nsec = rem;
	return ts;
}

/* Clocks: Nova has one, monotonic since boot (TSC based). Real time arrives with an RTC
 * offset later; PTP hardware timestamping is not supported. */
ktime_t ktime_get(void)
{
	return nova_nanos();
}

ktime_t ktime_get_with_offset(enum tk_offsets offs)
{
	return nova_nanos();
}

void ktime_get_snapshot_id(clockid_t clock_id, struct system_time_snapshot *systime_snapshot)
{
	memset(systime_snapshot, 0, sizeof(*systime_snapshot));
	systime_snapshot->systime = systime_snapshot->monoraw = nova_nanos();
}

int get_device_system_crosststamp(int (*get_time_fn)(ktime_t *device_time, struct system_counterval_t *sys_counterval, void *ctx),
				  void *ctx, struct system_time_snapshot *history, struct system_device_crosststamp *xtstamp)
{
	return -EOPNOTSUPP;
}

void dump_stack(void)
{
	_printk(KERN_WARNING "(stack trace unavailable in Nova)\n");
}

/* CPU identification for drivers that check it (the boot CPU, from CPUID). */
struct cpuinfo_x86 boot_cpu_data;

static void lx_identify_cpu(void)
{
	unsigned int eax, ebx, ecx, edx;

	cpuid(0, &eax, &ebx, &ecx, &edx);
	boot_cpu_data.cpuid_level = eax;
	memcpy(&boot_cpu_data.x86_vendor_id[0], &ebx, 4);
	memcpy(&boot_cpu_data.x86_vendor_id[4], &edx, 4);
	memcpy(&boot_cpu_data.x86_vendor_id[8], &ecx, 4);
	if (!memcmp(boot_cpu_data.x86_vendor_id, "AuthenticAMD", 12))
		boot_cpu_data.x86_vendor = X86_VENDOR_AMD;
	else if (!memcmp(boot_cpu_data.x86_vendor_id, "GenuineIntel", 12))
		boot_cpu_data.x86_vendor = X86_VENDOR_INTEL;
	else
		boot_cpu_data.x86_vendor = X86_VENDOR_UNKNOWN;
	cpuid(1, &eax, &ebx, &ecx, &edx);
	boot_cpu_data.x86 = x86_family(eax);
	boot_cpu_data.x86_model = x86_model(eax);
	boot_cpu_data.x86_stepping = x86_stepping(eax);
	boot_cpu_data.x86_capability[CPUID_1_EDX] = edx;
	boot_cpu_data.x86_capability[CPUID_1_ECX] = ecx;
	boot_cpu_data.x86_clflush_size = 64;
	boot_cpu_data.x86_cache_alignment = 64;
}

/* ---------------------------------------------------------------- locking */

/* A Nova task id + 1 as the owner (0 = unlocked). Contended locks yield to the holder. */
void mutex_init_generic(struct mutex *lock)
{
	atomic_long_set(&lock->owner, 0);
}

void mutex_lock(struct mutex *lock)
{
	long self = nova_task_id() + 1;

	while (atomic_long_cmpxchg(&lock->owner, 0, self) != 0)
		nova_yield();
}

int mutex_lock_interruptible(struct mutex *lock)
{
	mutex_lock(lock);
	return 0;
}

int mutex_lock_killable(struct mutex *lock)
{
	mutex_lock(lock);
	return 0;
}

int mutex_trylock(struct mutex *lock)
{
	return atomic_long_cmpxchg(&lock->owner, 0, nova_task_id() + 1) == 0;
}

void mutex_unlock(struct mutex *lock)
{
	atomic_long_set(&lock->owner, 0);
}

bool mutex_is_locked(struct mutex *lock)
{
	return atomic_long_read(&lock->owner) != 0;
}

static DEFINE_MUTEX(lx_rtnl_mutex);

void rtnl_lock(void)
{
	mutex_lock(&lx_rtnl_mutex);
}

void rtnl_unlock(void)
{
	mutex_unlock(&lx_rtnl_mutex);
}

int rtnl_trylock(void)
{
	return mutex_trylock(&lx_rtnl_mutex);
}

int rtnl_is_locked(void)
{
	return mutex_is_locked(&lx_rtnl_mutex);
}

/* No reader can be inside an RCU read-side section while another task runs (nothing
 * preempts Linux code and readers do not sleep), so a grace period is always over. */
void synchronize_rcu(void)
{
}

void synchronize_net(void)
{
}

/* Sleepable RCU: the same argument as for RCU holds (readers are never interrupted by a
 * grace period on one CPU), so read sections need no tracking. */
int init_srcu_struct(struct srcu_struct *ssp)
{
	return 0;
}

void __srcu_read_unlock(struct srcu_struct *ssp, int idx)
{
}

void synchronize_srcu(struct srcu_struct *ssp)
{
}

void cleanup_srcu_struct(struct srcu_struct *ssp)
{
}

enum system_states system_state = SYSTEM_RUNNING;

/* Bottom halves: softirqs are never raised (NAPI is polled from the lx_emul task), so
 * enabling them only undoes the count that local_bh_disable() added. */
void __local_bh_enable_ip(unsigned long ip, unsigned int cnt)
{
	__preempt_count_sub(cnt);
}

/* Preemption: start at PREEMPT_ENABLED ("no reschedule needed"), so preempt_enable()
 * never takes the slow path into preempt_schedule_thunk (lx_emul/thunks.S). */
int __preempt_count = PREEMPT_ENABLED;

/* ---------------------------------------------------------------- random */

void get_random_bytes(void *buf, size_t len)
{
	u8 *p = buf;

	while (len) {
		u64 r = nova_random_u64();
		size_t n = len < sizeof(r) ? len : sizeof(r);

		memcpy(p, &r, n);
		p += n;
		len -= n;
	}
}

u32 __get_random_u32_below(u32 ceil)
{
	return ceil ? (u32)(((u64)(u32)nova_random_u64() * ceil) >> 32) : 0;
}

u32 get_random_u32(void)
{
	return (u32)nova_random_u64();
}

u64 get_random_u64(void)
{
	return nova_random_u64();
}

int execute_with_initialized_rng(struct notifier_block *nb)
{
	/* Nova's RNG (RDRAND or TSC jitter) is ready before any Linux code runs. */
	nb->notifier_call(nb, 0, NULL);
	return 0;
}

/* ---------------------------------------------------------------- odds and ends */

/* Module parameters: drivers are built in and keep their defaults. */
const struct kernel_param_ops param_ops_bool;
const struct kernel_param_ops param_ops_int;
const struct kernel_param_ops param_ops_uint;
const struct kernel_param_ops param_ops_charp;
const struct kernel_param_ops param_array_ops;

/* vsprintf's %pG/%pGp/%pGv decoders: no flag names (printed as raw numbers). */
const struct trace_print_flags gfpflag_names[] = { { 0, NULL } };
const struct trace_print_flags pageflag_names[] = { { 0, NULL } };
const struct trace_print_flags vmaflag_names[] = { { 0, NULL } };

/* Firmware nodes (device tree / ACPI): Nova has none yet. */
unsigned int fwnode_count_parents(const struct fwnode_handle *fwn)
{
	return 0;
}

struct fwnode_handle *fwnode_get_nth_parent(struct fwnode_handle *fwn, unsigned int depth)
{
	return NULL;
}

const char *fwnode_get_name(const struct fwnode_handle *fwnode)
{
	return NULL;
}

const char *fwnode_get_name_prefix(const struct fwnode_handle *fwnode)
{
	return "";
}

int get_cmdline(struct task_struct *task, char *buffer, int buflen)
{
	return 0;
}

char *file_path(struct file *filp, char *buf, int buflen)
{
	return ERR_PTR(-ENOENT);
}

/* Log a probe error (except -EPROBE_DEFER, which is not one) and return it. */
int dev_err_probe(const struct device *dev, int err, const char *fmt, ...)
{
	struct va_format vaf;
	va_list args;

	if (err != -EPROBE_DEFER) {
		va_start(args, fmt);
		vaf.fmt = fmt;
		vaf.va = &args;
		dev_err(dev, "error %pe: %pV", ERR_PTR(err), &vaf);
		va_end(args);
	}
	return err;
}
