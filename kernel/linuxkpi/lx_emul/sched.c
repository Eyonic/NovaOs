// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul scheduling: timers, workqueues and interrupt handlers.
 *
 * Interrupt handlers run in real interrupt context: request_irq() unmasks the device's
 * PCI line at Nova's PIC, and Nova's IRQ dispatcher calls nova_lx_irq() for it, with
 * interrupts disabled as on Linux. Everything deferred runs in one Nova kernel task
 * ("linux", kernel/src/linuxkpi.rs), which every interrupt wakes: threaded handlers,
 * NAPI, expired timers and queued work items. Work items may sleep (msleep, mutexes):
 * that parks the "linux" task, nothing else.
 *
 * The task also polls each handler once per wakeup, with interrupts off. A handler checks
 * its device's status and returns IRQ_NONE when idle (as shared handlers must), so this
 * only matters for lines Nova could not wire (APIC and MSI arrive in milestone 2).
 *
 * The lists here are also touched from interrupt handlers (a handler may schedule NAPI,
 * queue work or arm a timer), so task-side list updates run with interrupts disabled.
 */
#include <linux/timer.h>
#include <linux/workqueue.h>
#include <linux/interrupt.h>
#include <linux/hardirq.h>
#include <linux/jiffies.h>
#include <linux/slab.h>
#include <linux/list.h>
#include <linux/hrtimer.h>
#include <linux/irq_work.h>
#include <linux/llist.h>
#include "nova.h"

void nova_park(void);
void nova_wake(u64 task_id);

/* ---------------------------------------------------------------- timers */

static HLIST_HEAD(lx_timers);

void timer_init_key(struct timer_list *timer, void (*func)(struct timer_list *), unsigned int flags,
		    const char *name, struct lock_class_key *key)
{
	timer->entry.pprev = NULL;
	timer->function = func;
	timer->flags = flags;
}

static int lx_timer_detach(struct timer_list *timer)
{
	if (!timer_pending(timer))
		return 0;
	hlist_del_init(&timer->entry);
	return 1;
}

int mod_timer(struct timer_list *timer, unsigned long expires)
{
	unsigned long flags;
	int was_pending;

	local_irq_save(flags);
	was_pending = lx_timer_detach(timer);
	timer->expires = expires;
	hlist_add_head(&timer->entry, &lx_timers);
	local_irq_restore(flags);
	return was_pending;
}

int timer_reduce(struct timer_list *timer, unsigned long expires)
{
	if (timer_pending(timer) && time_before_eq(timer->expires, expires))
		return 1;
	return mod_timer(timer, expires);
}

void add_timer(struct timer_list *timer)
{
	mod_timer(timer, timer->expires);
}

void add_timer_on(struct timer_list *timer, int cpu)
{
	add_timer(timer);
}

int timer_delete(struct timer_list *timer)
{
	unsigned long flags;
	int ret;

	local_irq_save(flags);
	ret = lx_timer_detach(timer);
	local_irq_restore(flags);
	return ret;
}

/* A timer callback only runs inside lx_emul_timers_run, which never yields, so once a
 * timer is detached its callback cannot be running elsewhere. */
int timer_delete_sync(struct timer_list *timer)
{
	return timer_delete(timer);
}

int timer_shutdown(struct timer_list *timer)
{
	int ret = timer_delete(timer);

	timer->function = NULL;
	return ret;
}

int timer_shutdown_sync(struct timer_list *timer)
{
	return timer_shutdown(timer);
}

void lx_emul_timers_run(void)
{
	struct timer_list *timer, *expired;
	unsigned long flags;

	do {
		/* Take one expired timer off the list, then run it with interrupts on (the
		 * callback may add or remove timers, so the scan restarts afterwards). */
		expired = NULL;
		local_irq_save(flags);
		hlist_for_each_entry(timer, &lx_timers, entry) {
			if (!time_before(jiffies, timer->expires)) {
				hlist_del_init(&timer->entry);
				expired = timer;
				break;
			}
		}
		local_irq_restore(flags);
		if (expired && expired->function)
			expired->function(expired);
	} while (expired);
}

/* ---------------------------------------------------------------- workqueues */

struct workqueue_struct {
	char name[32];
	unsigned int flags;
};

static struct workqueue_struct lx_system_wq = { .name = "events" };
struct workqueue_struct *system_wq = &lx_system_wq;
struct workqueue_struct *system_percpu_wq = &lx_system_wq;
struct workqueue_struct *system_highpri_wq = &lx_system_wq;
struct workqueue_struct *system_long_wq = &lx_system_wq;
struct workqueue_struct *system_unbound_wq = &lx_system_wq;
struct workqueue_struct *system_dfl_wq = &lx_system_wq;
struct workqueue_struct *system_freezable_wq = &lx_system_wq;
struct workqueue_struct *system_power_efficient_wq = &lx_system_wq;
struct workqueue_struct *system_freezable_power_efficient_wq = &lx_system_wq;
struct workqueue_struct *system_bh_wq = &lx_system_wq;
struct workqueue_struct *system_bh_highpri_wq = &lx_system_wq;
struct workqueue_struct *system_dfl_long_wq = &lx_system_wq;

/*
 * Every queue feeds one list, served by a small pool of worker tasks (started by Nova
 * before the initcalls, linuxkpi.rs). A work item that sleeps (waiting for a USB
 * transfer, say) blocks only its worker; the others keep the list moving, as Linux's
 * worker pools do. Timers, NAPI and threaded handlers stay in the "linux" task.
 */
#define LX_WORKERS 4
static LIST_HEAD(lx_work_list);
static struct work_struct *lx_running[LX_WORKERS];
static u64 lx_worker_task[LX_WORKERS];
static unsigned int lx_workers;

static void lx_wake_workers(void)
{
	unsigned int i;

	for (i = 0; i < lx_workers; i++)
		if (!lx_running[i])
			nova_wake(lx_worker_task[i]);
}

struct workqueue_struct *alloc_workqueue_noprof(const char *fmt, unsigned int flags, int max_active, ...)
{
	struct workqueue_struct *wq = kzalloc(sizeof(*wq), GFP_KERNEL);
	va_list args;

	if (!wq)
		return NULL;
	va_start(args, max_active);
	vsnprintf(wq->name, sizeof(wq->name), fmt, args);
	va_end(args);
	wq->flags = flags;
	return wq;
}

static void lx_work_enqueue(struct work_struct *work)
{
	unsigned long flags;

	local_irq_save(flags);
	list_add_tail(&work->entry, &lx_work_list);
	local_irq_restore(flags);
	lx_wake_workers();
}

/* Work items turned off with disable_work() (Linux counts this in work->data; a short
 * list does here: drivers disable one or two items while they are down). */
static struct work_struct *lx_disabled_work[32];

static bool lx_work_disabled(struct work_struct *work)
{
	for (unsigned int i = 0; i < ARRAY_SIZE(lx_disabled_work); i++)
		if (lx_disabled_work[i] == work)
			return true;
	return false;
}

bool queue_work_on(int cpu, struct workqueue_struct *wq, struct work_struct *work)
{
	if (lx_work_disabled(work))
		return false;
	if (test_and_set_bit(WORK_STRUCT_PENDING_BIT, work_data_bits(work)))
		return false;
	lx_work_enqueue(work);
	return true;
}

void delayed_work_timer_fn(struct timer_list *t)
{
	struct delayed_work *dwork = container_of(t, struct delayed_work, timer);

	lx_work_enqueue(&dwork->work);
}

bool queue_delayed_work_on(int cpu, struct workqueue_struct *wq, struct delayed_work *dwork,
			   unsigned long delay)
{
	if (lx_work_disabled(&dwork->work))
		return false;
	if (test_and_set_bit(WORK_STRUCT_PENDING_BIT, work_data_bits(&dwork->work)))
		return false;
	dwork->wq = wq;
	dwork->cpu = cpu;
	if (!delay)
		lx_work_enqueue(&dwork->work);
	else
		mod_timer(&dwork->timer, jiffies + delay);
	return true;
}

/* Remove a work item from wherever it waits (timer or list); true if it was pending. */
static bool lx_work_unqueue(struct work_struct *work, struct timer_list *timer)
{
	unsigned long flags;

	local_irq_save(flags);
	if (!test_bit(WORK_STRUCT_PENDING_BIT, work_data_bits(work))) {
		local_irq_restore(flags);
		return false;
	}
	if (!(timer && lx_timer_detach(timer)))
		list_del_init(&work->entry);
	clear_bit(WORK_STRUCT_PENDING_BIT, work_data_bits(work));
	local_irq_restore(flags);
	return true;
}

static bool lx_work_running(struct work_struct *work)
{
	unsigned int i;

	for (i = 0; i < lx_workers; i++)
		if (lx_running[i] == work && lx_worker_task[i] != nova_task_id())
			return true;
	return false;
}

/* Wait until no other worker runs `work` (a work item waiting for itself returns). */
static bool lx_work_wait(struct work_struct *work)
{
	bool waited = false;

	while (lx_work_running(work)) {
		waited = true;
		nova_park();
	}
	return waited;
}

bool mod_delayed_work_on(int cpu, struct workqueue_struct *wq, struct delayed_work *dwork,
			 unsigned long delay)
{
	bool was_pending = lx_work_unqueue(&dwork->work, &dwork->timer);

	queue_delayed_work_on(cpu, wq, dwork, delay);
	return was_pending;
}

bool cancel_work(struct work_struct *work)
{
	return lx_work_unqueue(work, NULL);
}

bool cancel_work_sync(struct work_struct *work)
{
	bool was_pending = lx_work_unqueue(work, NULL);

	lx_work_wait(work);
	return was_pending;
}

bool cancel_delayed_work(struct delayed_work *dwork)
{
	return lx_work_unqueue(&dwork->work, &dwork->timer);
}

bool cancel_delayed_work_sync(struct delayed_work *dwork)
{
	bool was_pending = lx_work_unqueue(&dwork->work, &dwork->timer);

	lx_work_wait(&dwork->work);
	return was_pending;
}

bool flush_work(struct work_struct *work)
{
	if (lx_work_unqueue(work, NULL)) {
		work->func(work);
		return true;
	}
	return lx_work_wait(work);
}

bool flush_delayed_work(struct delayed_work *dwork)
{
	if (lx_work_unqueue(&dwork->work, &dwork->timer)) {
		dwork->work.func(&dwork->work);
		return true;
	}
	return lx_work_wait(&dwork->work);
}

/* One worker task (index `n`); never returns. Called from Nova (linuxkpi.rs). */
void nova_lx_worker(unsigned int n)
{
	lx_worker_task[n] = nova_task_id();
	if (n + 1 > lx_workers)
		lx_workers = n + 1;
	for (;;) {
		struct work_struct *work = NULL;
		unsigned long flags;

		local_irq_save(flags);
		if (!list_empty(&lx_work_list)) {
			work = list_first_entry(&lx_work_list, struct work_struct, entry);
			list_del_init(&work->entry);
			lx_running[n] = work;
			/* Cleared before running, so the item can queue itself again. */
			clear_bit(WORK_STRUCT_PENDING_BIT, work_data_bits(work));
		}
		local_irq_restore(flags);
		if (!work) {
			nova_park();
			continue;
		}
		work->func(work);
		lx_running[n] = NULL;
	}
}

static bool lx_work_idle(void)
{
	unsigned int i;

	if (!list_empty(&lx_work_list))
		return false;
	for (i = 0; i < lx_workers; i++)
		if (lx_running[i] && lx_worker_task[i] != nova_task_id())
			return false;
	return true;
}

void __flush_workqueue(struct workqueue_struct *wq)
{
	while (!lx_work_idle())
		nova_park();
}

void destroy_workqueue(struct workqueue_struct *wq)
{
	__flush_workqueue(wq);
	if (wq != &lx_system_wq)
		kfree(wq);
}

/* ---------------------------------------------------------------- interrupts */

struct lx_irq {
	unsigned int irq;
	irq_handler_t handler, thread_fn;
	void *dev_id;
	const char *name;
	bool used;
	bool thread_pending;
};

static struct lx_irq lx_irqs[32];
static unsigned int lx_irq_disabled_count[768];
/* Disable depth per IRQ number: GSIs/PIC lines, MSI numbers (0x1000 + vector) and virtual
 * numbers of IRQ domains (0x2000 + n, irq.c) apart. */
#define lx_irq_disabled lx_irq_disabled_count
#define LX_IRQ_SLOT(irq) ((irq) >= 0x2000 ? 512 + ((irq) & 255) : (irq) >= 0x1000 ? 256 + ((irq) & 255) : (irq) & 255)

/* Provided by Nova (linuxkpi.rs): unmask/mask a PCI line at the PIC. */
void nova_irq_line(unsigned int line, bool enable);

static bool lx_irq_line_used(unsigned int irq)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(lx_irqs); i++)
		if (lx_irqs[i].used && lx_irqs[i].irq == irq)
			return true;
	return false;
}

int request_threaded_irq(unsigned int irq, irq_handler_t handler, irq_handler_t thread_fn,
			 unsigned long flags, const char *name, void *dev)
{
	unsigned long irqflags;
	unsigned int i;

	if (!handler && !thread_fn)
		return -EINVAL;
	for (i = 0; i < ARRAY_SIZE(lx_irqs); i++) {
		if (lx_irqs[i].used)
			continue;
		local_irq_save(irqflags);
		lx_irqs[i] = (struct lx_irq){ .irq = irq, .handler = handler, .thread_fn = thread_fn,
					      .dev_id = dev, .name = name, .used = true };
		local_irq_restore(irqflags);
		if (!lx_irq_disabled[LX_IRQ_SLOT(irq)])
			nova_irq_line(irq, true);
		return 0;
	}
	return -EBUSY;
}

const void *free_irq(unsigned int irq, void *dev_id)
{
	unsigned long flags;
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(lx_irqs); i++) {
		if (lx_irqs[i].used && lx_irqs[i].irq == irq && lx_irqs[i].dev_id == dev_id) {
			local_irq_save(flags);
			lx_irqs[i].used = false;
			local_irq_restore(flags);
			if (!lx_irq_line_used(irq))
				nova_irq_line(irq, false);
			return lx_irqs[i].name;
		}
	}
	return NULL;
}

/* As on Linux, disabling an interrupt masks the line at the interrupt controller. */
void disable_irq_nosync(unsigned int irq)
{
	if (lx_irq_disabled[LX_IRQ_SLOT(irq)]++ == 0)
		nova_irq_line(irq, false);
}

void disable_irq(unsigned int irq)
{
	disable_irq_nosync(irq);
}

void enable_irq(unsigned int irq)
{
	if (lx_irq_disabled[LX_IRQ_SLOT(irq)] && --lx_irq_disabled[LX_IRQ_SLOT(irq)] == 0 && lx_irq_line_used(irq))
		nova_irq_line(irq, true);
}

/* A handler runs either from the interrupt or from the poll below, both with interrupts
 * off, so once this returns no handler can still be running. */
void synchronize_irq(unsigned int irq)
{
}

/* Run the handlers on `irq`; interrupts are disabled. True if one claimed it. */
static bool lx_irq_run(unsigned int irq, bool any_line)
{
	bool claimed = false;
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(lx_irqs); i++) {
		struct lx_irq *a = &lx_irqs[i];
		irqreturn_t ret = IRQ_WAKE_THREAD;

		if (!a->used || (!any_line && a->irq != irq) || lx_irq_disabled[LX_IRQ_SLOT(a->irq)])
			continue;
		if (a->handler) {
			__preempt_count_add(HARDIRQ_OFFSET);
			ret = a->handler(a->irq, a->dev_id);
			__preempt_count_sub(HARDIRQ_OFFSET);
		}
		if (ret == IRQ_WAKE_THREAD && a->thread_fn)
			a->thread_pending = true;
		if (ret != IRQ_NONE)
			claimed = true;
	}
	return claimed;
}

/* Called by Nova's IRQ dispatcher for a PCI line, with interrupts disabled. */
bool nova_lx_irq(unsigned int line)
{
	return lx_irq_run(line, false);
}

/* A demultiplexed interrupt (irq.c), from inside another handler. */
void lx_emul_irq_handle(unsigned int irq)
{
	lx_irq_run(irq, false);
}

void lx_emul_irq_poll(void)
{
	unsigned long flags;
	unsigned int i;

	local_irq_save(flags);
	lx_irq_run(0, true);
	local_irq_restore(flags);

	/* Threaded handlers run in task context, like Linux's irq threads. */
	for (i = 0; i < ARRAY_SIZE(lx_irqs); i++) {
		struct lx_irq *a = &lx_irqs[i];

		if (a->used && a->thread_pending) {
			a->thread_pending = false;
			a->thread_fn(a->irq, a->dev_id);
		}
	}
}

/* ---------------------------------------------------------------- the poll loop */

void nova_lx_poll(void)
{
	lx_emul_irq_poll();
	lx_emul_napi_poll();
	lx_emul_timers_run();
	lx_emul_hrtimers_run();
	lx_emul_irq_work_run();
	lx_emul_rcu_run();
}

/* disable_work(): cancel the item and refuse to queue it until enable_work(). */
bool disable_work(struct work_struct *work)
{
	bool was_pending = cancel_work(work);

	if (!lx_work_disabled(work))
		for (unsigned int i = 0; i < ARRAY_SIZE(lx_disabled_work); i++)
			if (!lx_disabled_work[i]) {
				lx_disabled_work[i] = work;
				break;
			}
	return was_pending;
}

bool disable_work_sync(struct work_struct *work)
{
	bool was_pending = disable_work(work);

	lx_work_wait(work);
	return was_pending;
}

bool enable_work(struct work_struct *work)
{
	for (unsigned int i = 0; i < ARRAY_SIZE(lx_disabled_work); i++)
		if (lx_disabled_work[i] == work)
			lx_disabled_work[i] = NULL;
	return true;
}

bool disable_delayed_work_sync(struct delayed_work *dwork)
{
	bool was_pending = lx_work_unqueue(&dwork->work, &dwork->timer);

	disable_work(&dwork->work);
	lx_work_wait(&dwork->work);
	return was_pending;
}

bool enable_delayed_work(struct delayed_work *dwork)
{
	return enable_work(&dwork->work);
}

bool queue_work_node(int node, struct workqueue_struct *wq, struct work_struct *work)
{
	return queue_work_on(WORK_CPU_UNBOUND, wq, work);
}

/* All queues share one list: draining one waits until the whole list is idle. */
void drain_workqueue(struct workqueue_struct *wq)
{
	__flush_workqueue(wq);
}

/* The work item the calling worker runs, if the caller is a worker. */
struct work_struct *current_work(void)
{
	unsigned int i;

	for (i = 0; i < lx_workers; i++)
		if (lx_worker_task[i] == nova_task_id())
			return lx_running[i];
	return NULL;
}

/* ---------------------------------------------------------------- high-resolution timers */

/*
 * hrtimers run from the "linux" task like timer_list timers, so their resolution is
 * Nova's timer tick (1 ms); callbacks run with interrupts enabled, outside hard
 * interrupt context. DRM uses them for its software vblank timer. Active timers are
 * kept in a short array (Linux sorts them in a tree, but only a handful exist here).
 */
static struct hrtimer_clock_base lx_hrtimer_base;
static struct hrtimer *lx_hrtimers[32];

static int lx_hrtimer_slot(const struct hrtimer *timer)
{
	unsigned int i;

	for (i = 0; i < ARRAY_SIZE(lx_hrtimers); i++)
		if (lx_hrtimers[i] == timer)
			return i;
	return -1;
}

void hrtimer_setup(struct hrtimer *timer, enum hrtimer_restart (*function)(struct hrtimer *),
		   clockid_t clock_id, enum hrtimer_mode mode)
{
	memset(timer, 0, sizeof(*timer));
	timer->base = &lx_hrtimer_base;
	ACCESS_PRIVATE(timer, function) = function;
}

ktime_t hrtimer_cb_get_time(const struct hrtimer *timer)
{
	return ktime_get();
}

/* Add, saturating at KTIME_MAX instead of wrapping. */
ktime_t ktime_add_safe(const ktime_t lhs, const ktime_t rhs)
{
	ktime_t res = ktime_add_unsafe(lhs, rhs);

	if (res < 0 || res < lhs || res < rhs)
		res = ktime_set(KTIME_SEC_MAX, 0);
	return res;
}

/* Take a timer off the active array; interrupts are disabled. */
static bool lx_hrtimer_dequeue(struct hrtimer *timer)
{
	int slot = lx_hrtimer_slot(timer);

	if (slot < 0)
		return false;
	lx_hrtimers[slot] = NULL;
	WRITE_ONCE(timer->is_queued, false);
	return true;
}

static void lx_hrtimer_enqueue(struct hrtimer *timer)
{
	int slot = lx_hrtimer_slot(timer);

	if (slot < 0)
		slot = lx_hrtimer_slot(NULL);
	if (WARN_ON(slot < 0))
		return;
	lx_hrtimers[slot] = timer;
	WRITE_ONCE(timer->is_queued, true);
}

void hrtimer_start_range_ns(struct hrtimer *timer, ktime_t tim, u64 range_ns, const enum hrtimer_mode mode)
{
	unsigned long flags;

	if (mode & HRTIMER_MODE_REL)
		tim = ktime_add_safe(ktime_get(), tim);
	local_irq_save(flags);
	hrtimer_set_expires_range_ns(timer, tim, range_ns);
	lx_hrtimer_enqueue(timer);
	local_irq_restore(flags);
}

int hrtimer_try_to_cancel(struct hrtimer *timer)
{
	unsigned long flags;
	bool queued;

	if (hrtimer_callback_running(timer))
		return -1;
	local_irq_save(flags);
	queued = lx_hrtimer_dequeue(timer);
	local_irq_restore(flags);
	return queued;
}

int hrtimer_cancel(struct hrtimer *timer)
{
	int ret;

	while ((ret = hrtimer_try_to_cancel(timer)) < 0)
		nova_park();
	return ret;
}

bool hrtimer_active(const struct hrtimer *timer)
{
	return READ_ONCE(timer->is_queued) || timer->base->running == timer;
}

u64 hrtimer_forward(struct hrtimer *timer, ktime_t now, ktime_t interval)
{
	ktime_t delta = ktime_sub(now, hrtimer_get_expires(timer));
	u64 orun = 1;

	if (delta < 0)
		return 0;
	if (interval < NSEC_PER_MSEC / 10)
		interval = NSEC_PER_MSEC / 10;
	if (delta >= interval) {
		orun = ktime_divns(delta, interval);
		hrtimer_add_expires_ns(timer, orun * interval);
		if (hrtimer_get_expires(timer) > now)
			return orun;
		orun++;
	}
	hrtimer_add_expires(timer, interval);
	return orun;
}

void lx_emul_hrtimers_run(void)
{
	struct hrtimer *expired;
	unsigned long flags;
	unsigned int i;

	for (;;) {
		ktime_t now = ktime_get();

		expired = NULL;
		local_irq_save(flags);
		for (i = 0; i < ARRAY_SIZE(lx_hrtimers); i++) {
			struct hrtimer *t = lx_hrtimers[i];

			if (t && hrtimer_get_softexpires(t) <= now) {
				lx_hrtimer_dequeue(t);
				lx_hrtimer_base.running = t;
				expired = t;
				break;
			}
		}
		local_irq_restore(flags);
		if (!expired)
			break;
		if (ACCESS_PRIVATE(expired, function)(expired) == HRTIMER_RESTART) {
			local_irq_save(flags);
			lx_hrtimer_enqueue(expired);
			local_irq_restore(flags);
		}
		lx_hrtimer_base.running = NULL;
	}
}

/* ---------------------------------------------------------------- irq_work */

/* Queued irq_work runs from the "linux" task, which every interrupt wakes (Linux runs
 * it from a self-interrupt right after the queuing handler returns). */
static LLIST_HEAD(lx_irq_works);

bool irq_work_queue(struct irq_work *work)
{
	if (atomic_fetch_or(IRQ_WORK_CLAIMED, &work->node.a_flags) & IRQ_WORK_PENDING)
		return false;
	llist_add(&work->node.llist, &lx_irq_works);
	return true;
}

void lx_emul_irq_work_run(void)
{
	struct llist_node *list = llist_del_all(&lx_irq_works);
	struct irq_work *work, *next;

	llist_for_each_entry_safe(work, next, llist_reverse_order(list), node.llist) {
		unsigned long flags;

		atomic_andnot(IRQ_WORK_PENDING, &work->node.a_flags);
		local_irq_save(flags);
		work->func(work);
		local_irq_restore(flags);
		atomic_andnot(IRQ_WORK_BUSY, &work->node.a_flags);
	}
}

/* Tiny SRCU's grace-period machinery: grace periods are always over here (base.c), so
 * these never have anything to do. */
void srcu_drive_gp(struct work_struct *wp)
{
}

void srcu_tiny_irq_work(struct irq_work *irq_work)
{
}

/* ---------------------------------------------------------------- time conversion */

u64 nsecs_to_jiffies64(u64 n)
{
	return div_u64(n, NSEC_PER_SEC / HZ);
}

unsigned long nsecs_to_jiffies(u64 n)
{
	return (unsigned long)nsecs_to_jiffies64(n);
}
