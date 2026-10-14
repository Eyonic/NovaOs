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
#include "nova.h"

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

/* Every queue feeds one list: there is one CPU and one worker. */
static LIST_HEAD(lx_work_list);

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
}

bool queue_work_on(int cpu, struct workqueue_struct *wq, struct work_struct *work)
{
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

/* Work items run only in lx_emul_work_run, between calls of which nothing is running,
 * so cancelling never has to wait for a running instance (unless a work item cancels
 * itself, which then simply returns). */
bool cancel_work_sync(struct work_struct *work)
{
	return lx_work_unqueue(work, NULL);
}

bool cancel_delayed_work(struct delayed_work *dwork)
{
	return lx_work_unqueue(&dwork->work, &dwork->timer);
}

bool cancel_delayed_work_sync(struct delayed_work *dwork)
{
	return lx_work_unqueue(&dwork->work, &dwork->timer);
}

static void lx_work_execute(struct work_struct *work)
{
	clear_bit(WORK_STRUCT_PENDING_BIT, work_data_bits(work));
	work->func(work);
}

bool flush_work(struct work_struct *work)
{
	if (!lx_work_unqueue(work, NULL))
		return false;
	work->func(work);
	return true;
}

bool flush_delayed_work(struct delayed_work *dwork)
{
	if (!lx_work_unqueue(&dwork->work, &dwork->timer))
		return false;
	dwork->work.func(&dwork->work);
	return true;
}

void lx_emul_work_run(void)
{
	for (;;) {
		struct work_struct *work = NULL;
		unsigned long flags;

		local_irq_save(flags);
		if (!list_empty(&lx_work_list)) {
			work = list_first_entry(&lx_work_list, struct work_struct, entry);
			list_del_init(&work->entry);
		}
		local_irq_restore(flags);
		if (!work)
			return;
		lx_work_execute(work);
	}
}

void __flush_workqueue(struct workqueue_struct *wq)
{
	lx_emul_work_run();
}

void destroy_workqueue(struct workqueue_struct *wq)
{
	lx_emul_work_run();
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
static unsigned int lx_irq_disabled_count[512];
/* Disable depth per IRQ number: GSIs/PIC lines and MSI numbers (0x1000 + vector) apart. */
#define lx_irq_disabled lx_irq_disabled_count
#define LX_IRQ_SLOT(irq) ((irq) >= 0x1000 ? 256 + ((irq) & 255) : (irq) & 255)

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
	lx_emul_work_run();
}
