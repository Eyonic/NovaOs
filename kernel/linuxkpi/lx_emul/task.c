// SPDX-License-Identifier: MIT OR Apache-2.0
/*
 * lx_emul tasks: `current`, sleeping and waking, wait queues, completions, semaphores,
 * reader/writer semaphores.
 *
 * Every Nova kernel task that runs Linux code has a struct task_struct here; Nova's
 * scheduler points Linux's `current_task` at it on every switch (nova_lx_switch, from
 * task.rs). Sleeping follows Linux's protocol: set current->__state, re-check the
 * condition, schedule(). schedule() parks the Nova task until wake_up_process() sets the
 * state back to TASK_RUNNING (and wakes the Nova task) or, for timeouts, until the
 * deadline passes. Wakers may run in interrupt handlers, so the lists here are only
 * touched with interrupts off.
 *
 * Semaphores and rwsems keep no waiter lists (their real ones are private to Linux's
 * locking code): a waiter re-checks at the next interrupt (at most one timer tick).
 */
#include <linux/sched.h>
#include <linux/sched/task.h>
#include <linux/wait.h>
#include <linux/swait.h>
#include <linux/completion.h>
#include <linux/semaphore.h>
#include <linux/rwsem.h>
#include <linux/jiffies.h>
#include <linux/string.h>
#include "nova.h"

void nova_park(void);
void nova_wake(u64 task_id);

/* ---------------------------------------------------------------- current */

#define LX_TASK_SLOTS 64
static struct task_struct lx_tasks[LX_TASK_SLOTS];
static u64 lx_task_owner[LX_TASK_SLOTS]; /* Nova task id + 1, 0 = free */

DEFINE_PER_CPU_CACHE_HOT(struct task_struct *, current_task) = &lx_tasks[0];
/* Linux's linker script provides the const alias; Nova's build is ours. */
asm(".globl const_current_task\n.set const_current_task, current_task");

static struct task_struct *lx_task_for(u64 id)
{
	unsigned int slot = id % LX_TASK_SLOTS;
	struct task_struct *t = &lx_tasks[slot];

	if (lx_task_owner[slot] != id + 1) {
		/* A new task, or one whose slot a newer task (id + 64k) has taken over. */
		memset(t, 0, sizeof(*t));
		t->__state = TASK_RUNNING;
		t->pid = t->tgid = (pid_t)id;
		snprintf(t->comm, sizeof(t->comm), "nova-%llu", id);
		lx_task_owner[slot] = id + 1;
	}
	return t;
}

/* Called by Nova's scheduler (task.rs) whenever task `id` starts running. */
void nova_lx_switch(u64 id)
{
	this_cpu_write(current_task, lx_task_for(id));
}

static u64 lx_task_id(struct task_struct *p)
{
	return (u64)p->pid;
}

/* ---------------------------------------------------------------- sleeping and waking */

int wake_up_process(struct task_struct *p)
{
	unsigned int old = READ_ONCE(p->__state);

	if (old == TASK_RUNNING)
		return 0;
	WRITE_ONCE(p->__state, TASK_RUNNING);
	nova_wake(lx_task_id(p));
	return 1;
}

int wake_up_state(struct task_struct *p, unsigned int state)
{
	return (READ_ONCE(p->__state) & state) ? wake_up_process(p) : 0;
}

int default_wake_function(struct wait_queue_entry *curr, unsigned mode, int wake_flags, void *key)
{
	return wake_up_state(curr->private, mode);
}

void schedule(void)
{
	while (READ_ONCE(current->__state) != TASK_RUNNING)
		nova_park();
}

void io_schedule(void)
{
	schedule();
}

long schedule_timeout(long timeout)
{
	unsigned long deadline;

	if (timeout == MAX_SCHEDULE_TIMEOUT) {
		schedule();
		return timeout;
	}
	if (timeout < 0)
		timeout = 0;
	deadline = jiffies + timeout;
	while (READ_ONCE(current->__state) != TASK_RUNNING && time_before(jiffies, deadline))
		nova_park();
	__set_current_state(TASK_RUNNING);
	timeout = (long)(deadline - jiffies);
	return timeout < 0 ? 0 : timeout;
}

long io_schedule_timeout(long timeout)
{
	return schedule_timeout(timeout);
}

long schedule_timeout_idle(long timeout)
{
	__set_current_state(TASK_IDLE);
	return schedule_timeout(timeout);
}

void yield(void)
{
	nova_yield();
}

/* ---------------------------------------------------------------- wait queues */

void __init_waitqueue_head(struct wait_queue_head *wq_head, const char *name, struct lock_class_key *key)
{
	spin_lock_init(&wq_head->lock);
	INIT_LIST_HEAD(&wq_head->head);
}

void init_wait_entry(struct wait_queue_entry *wq_entry, int flags)
{
	wq_entry->flags = flags;
	wq_entry->private = current;
	wq_entry->func = autoremove_wake_function;
	INIT_LIST_HEAD(&wq_entry->entry);
}

void add_wait_queue(struct wait_queue_head *wq_head, struct wait_queue_entry *wq_entry)
{
	unsigned long flags;

	wq_entry->flags &= ~WQ_FLAG_EXCLUSIVE;
	local_irq_save(flags);
	list_add(&wq_entry->entry, &wq_head->head);
	local_irq_restore(flags);
}

void add_wait_queue_exclusive(struct wait_queue_head *wq_head, struct wait_queue_entry *wq_entry)
{
	unsigned long flags;

	wq_entry->flags |= WQ_FLAG_EXCLUSIVE;
	local_irq_save(flags);
	list_add_tail(&wq_entry->entry, &wq_head->head);
	local_irq_restore(flags);
}

void remove_wait_queue(struct wait_queue_head *wq_head, struct wait_queue_entry *wq_entry)
{
	unsigned long flags;

	local_irq_save(flags);
	list_del_init(&wq_entry->entry);
	local_irq_restore(flags);
}

long prepare_to_wait_event(struct wait_queue_head *wq_head, struct wait_queue_entry *wq_entry, int state)
{
	unsigned long flags;

	local_irq_save(flags);
	if (list_empty(&wq_entry->entry)) {
		if (wq_entry->flags & WQ_FLAG_EXCLUSIVE)
			list_add_tail(&wq_entry->entry, &wq_head->head);
		else
			list_add(&wq_entry->entry, &wq_head->head);
	}
	set_current_state(state);
	local_irq_restore(flags);
	return 0; /* no signals in Nova's kernel tasks */
}

void prepare_to_wait(struct wait_queue_head *wq_head, struct wait_queue_entry *wq_entry, int state)
{
	wq_entry->flags &= ~WQ_FLAG_EXCLUSIVE;
	prepare_to_wait_event(wq_head, wq_entry, state);
}

bool prepare_to_wait_exclusive(struct wait_queue_head *wq_head, struct wait_queue_entry *wq_entry, int state)
{
	wq_entry->flags |= WQ_FLAG_EXCLUSIVE;
	prepare_to_wait_event(wq_head, wq_entry, state);
	return true;
}

void finish_wait(struct wait_queue_head *wq_head, struct wait_queue_entry *wq_entry)
{
	unsigned long flags;

	__set_current_state(TASK_RUNNING);
	local_irq_save(flags);
	list_del_init(&wq_entry->entry);
	local_irq_restore(flags);
}

int autoremove_wake_function(struct wait_queue_entry *wq_entry, unsigned mode, int sync, void *key)
{
	int ret = default_wake_function(wq_entry, mode, sync, key);

	if (ret)
		list_del_init_careful(&wq_entry->entry);
	return ret;
}

int woken_wake_function(struct wait_queue_entry *wq_entry, unsigned mode, int sync, void *key)
{
	smp_mb();
	wq_entry->flags |= WQ_FLAG_WOKEN;
	return default_wake_function(wq_entry, mode, sync, key);
}

long wait_woken(struct wait_queue_entry *wq_entry, unsigned mode, long timeout)
{
	set_current_state(mode);
	if (!(wq_entry->flags & WQ_FLAG_WOKEN))
		timeout = schedule_timeout(timeout);
	__set_current_state(TASK_RUNNING);
	wq_entry->flags &= ~WQ_FLAG_WOKEN;
	return timeout;
}

static int lx_wake_up_common(struct wait_queue_head *wq_head, unsigned int mode, int nr_exclusive,
			     int wake_flags, void *key)
{
	struct wait_queue_entry *curr, *next;
	unsigned long flags;
	int woken = 0;

	local_irq_save(flags);
	list_for_each_entry_safe(curr, next, &wq_head->head, entry) {
		unsigned int entry_flags = curr->flags;
		int ret = curr->func(curr, mode, wake_flags, key);

		if (ret < 0)
			break;
		if (ret)
			woken++;
		if (ret && (entry_flags & WQ_FLAG_EXCLUSIVE) && !--nr_exclusive)
			break;
	}
	local_irq_restore(flags);
	return woken;
}

int __wake_up(struct wait_queue_head *wq_head, unsigned int mode, int nr_exclusive, void *key)
{
	return lx_wake_up_common(wq_head, mode, nr_exclusive, 0, key);
}

void __wake_up_locked(struct wait_queue_head *wq_head, unsigned int mode, int nr)
{
	lx_wake_up_common(wq_head, mode, nr, 0, NULL);
}

void __wake_up_locked_key(struct wait_queue_head *wq_head, unsigned int mode, void *key)
{
	lx_wake_up_common(wq_head, mode, 1, 0, key);
}

void __wake_up_sync_key(struct wait_queue_head *wq_head, unsigned int mode, void *key)
{
	lx_wake_up_common(wq_head, mode, 1, 0, key);
}

/* ---------------------------------------------------------------- completions */

struct lx_swaiter {
	struct task_struct *task;
	struct list_head node;
};

void __init_swait_queue_head(struct swait_queue_head *q, const char *name, struct lock_class_key *key)
{
	raw_spin_lock_init(&q->lock);
	INIT_LIST_HEAD(&q->task_list);
}

static void lx_complete(struct completion *x, bool all)
{
	struct lx_swaiter *w, *next;
	unsigned long flags;

	local_irq_save(flags);
	if (all)
		x->done = UINT_MAX;
	else if (x->done != UINT_MAX)
		x->done++;
	list_for_each_entry_safe(w, next, &x->wait.task_list, node) {
		list_del_init(&w->node);
		wake_up_process(w->task);
		if (!all)
			break;
	}
	local_irq_restore(flags);
}

void complete(struct completion *x)
{
	lx_complete(x, false);
}

void complete_on_current_cpu(struct completion *x)
{
	lx_complete(x, false);
}

void complete_all(struct completion *x)
{
	lx_complete(x, true);
}

/* Wait for x->done; returns the remaining timeout (at least 1) or 0 on timeout. */
static long lx_wait_for_completion(struct completion *x, long timeout, int state)
{
	struct lx_swaiter w = { .task = current };
	unsigned long flags;

	INIT_LIST_HEAD(&w.node);
	local_irq_save(flags);
	while (!x->done) {
		if (list_empty(&w.node))
			list_add_tail(&w.node, &x->wait.task_list);
		set_current_state(state);
		local_irq_restore(flags);
		timeout = schedule_timeout(timeout);
		local_irq_save(flags);
		if (!timeout && !x->done) {
			list_del_init(&w.node);
			local_irq_restore(flags);
			return 0;
		}
	}
	list_del_init(&w.node);
	__set_current_state(TASK_RUNNING);
	if (x->done != UINT_MAX)
		x->done--;
	local_irq_restore(flags);
	return timeout ?: 1;
}

void wait_for_completion(struct completion *x)
{
	lx_wait_for_completion(x, MAX_SCHEDULE_TIMEOUT, TASK_UNINTERRUPTIBLE);
}

void wait_for_completion_io(struct completion *x)
{
	wait_for_completion(x);
}

int wait_for_completion_interruptible(struct completion *x)
{
	wait_for_completion(x);
	return 0;
}

int wait_for_completion_killable(struct completion *x)
{
	wait_for_completion(x);
	return 0;
}

int wait_for_completion_state(struct completion *x, unsigned int state)
{
	lx_wait_for_completion(x, MAX_SCHEDULE_TIMEOUT, state);
	return 0;
}

unsigned long wait_for_completion_timeout(struct completion *x, unsigned long timeout)
{
	return lx_wait_for_completion(x, timeout, TASK_UNINTERRUPTIBLE);
}

unsigned long wait_for_completion_io_timeout(struct completion *x, unsigned long timeout)
{
	return wait_for_completion_timeout(x, timeout);
}

long wait_for_completion_interruptible_timeout(struct completion *x, unsigned long timeout)
{
	return lx_wait_for_completion(x, timeout, TASK_INTERRUPTIBLE);
}

long wait_for_completion_killable_timeout(struct completion *x, unsigned long timeout)
{
	return lx_wait_for_completion(x, timeout, TASK_KILLABLE);
}

bool try_wait_for_completion(struct completion *x)
{
	unsigned long flags;
	bool ret = false;

	local_irq_save(flags);
	if (x->done) {
		if (x->done != UINT_MAX)
			x->done--;
		ret = true;
	}
	local_irq_restore(flags);
	return ret;
}

bool completion_done(struct completion *x)
{
	return READ_ONCE(x->done) != 0;
}

/* ---------------------------------------------------------------- semaphores */

int down_trylock(struct semaphore *sem)
{
	unsigned long flags;
	int busy;

	local_irq_save(flags);
	busy = sem->count == 0;
	if (!busy)
		sem->count--;
	local_irq_restore(flags);
	return busy;
}

int down_timeout(struct semaphore *sem, long timeout)
{
	unsigned long deadline = jiffies + timeout;

	while (down_trylock(sem)) {
		if (timeout != MAX_SCHEDULE_TIMEOUT && !time_before(jiffies, deadline))
			return -ETIME;
		nova_park();
	}
	return 0;
}

void down(struct semaphore *sem)
{
	while (down_trylock(sem))
		nova_park();
}

int down_interruptible(struct semaphore *sem)
{
	down(sem);
	return 0;
}

int down_killable(struct semaphore *sem)
{
	down(sem);
	return 0;
}

void up(struct semaphore *sem)
{
	unsigned long flags;

	local_irq_save(flags);
	sem->count++;
	local_irq_restore(flags);
}

/* ---------------------------------------------------------------- reader/writer semaphores */

/* count: 0 = free, > 0 = that many readers, -1 = a writer. */
void __init_rwsem(struct rw_semaphore *sem, const char *name, struct lock_class_key *key)
{
	atomic_long_set(&sem->count, 0);
	atomic_long_set(&sem->owner, 0);
}

int down_read_trylock(struct rw_semaphore *sem)
{
	long c = atomic_long_read(&sem->count);

	while (c >= 0)
		if (atomic_long_try_cmpxchg(&sem->count, &c, c + 1))
			return 1;
	return 0;
}

int down_write_trylock(struct rw_semaphore *sem)
{
	long c = 0;

	return atomic_long_try_cmpxchg(&sem->count, &c, -1);
}

void down_read(struct rw_semaphore *sem)
{
	while (!down_read_trylock(sem))
		nova_park();
}

int down_read_killable(struct rw_semaphore *sem)
{
	down_read(sem);
	return 0;
}

int down_read_interruptible(struct rw_semaphore *sem)
{
	down_read(sem);
	return 0;
}

void down_write(struct rw_semaphore *sem)
{
	while (!down_write_trylock(sem))
		nova_park();
}

int down_write_killable(struct rw_semaphore *sem)
{
	down_write(sem);
	return 0;
}

void up_read(struct rw_semaphore *sem)
{
	atomic_long_dec(&sem->count);
}

void up_write(struct rw_semaphore *sem)
{
	atomic_long_set(&sem->count, 0);
}

void downgrade_write(struct rw_semaphore *sem)
{
	atomic_long_set(&sem->count, 1);
}
