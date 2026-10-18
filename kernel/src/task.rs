//! Cooperative kernel tasks and the scheduler.
//!
//! Every task has its own stack and runs until it calls [`yield_now`] or
//! [`wait_event`]. Kernel code is never preempted: a task is never interrupted by another
//! task, only by IRQ handlers (which never touch scheduler state), and kernel code runs
//! under the big kernel lock on every CPU (smp.rs). That makes all kernel data structures
//! single-threaded from the tasks' point of view, as long as no task yields while holding
//! a lock. Only user code is preempted, by the timer.
//!
//! Tasks are not bound to a CPU: a CPU that schedules picks the next runnable task (round
//! robin) that no other CPU is running. A CPU with nothing to run switches to its own idle
//! task, which halts with the big kernel lock released until it is kicked (a task woke up
//! or was started, smp::kick_idle) or, on the boot CPU, until any interrupt arrives.
//!
//! [`wait_event`] parks the current task until the next interrupt of any kind on the boot
//! CPU (timer, keyboard, mouse, serial). The timer fires at `pit::HZ`, so a task that polls
//! a device in a `wait_event` loop checks it at least that often.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::naked_asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::smp::{self, MAX_CPUS};

const STACK_SIZE: usize = 512 * 1024;
/// Idle tasks only halt, but interrupt handlers run on their stack.
const IDLE_STACK_SIZE: usize = 64 * 1024;

/// Incremented by every IRQ (see interrupts.rs); parked tasks wait for it to change.
pub static IRQ_COUNT: AtomicU64 = AtomicU64::new(0);
/// Incremented by device events only (input, NICs, Linux drivers' wake-ups), not by the
/// timer: tasks parked in [`wait_io`] wait for it to change.
pub static DEV_IRQ: AtomicU64 = AtomicU64::new(0);
/// DEV_IRQ when the scheduler last looked: whether [`tick_kick`] has new work to offer.
static DEV_SEEN: AtomicU64 = AtomicU64::new(0);

/// A device produced something (interrupt context too): tasks waiting for I/O re-check.
pub fn device_event() {
    DEV_IRQ.fetch_add(1, Ordering::SeqCst);
    IRQ_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// Timestamp-counter cycles spent halted in idle tasks, summed over the CPUs.
static IDLE_CYCLES: AtomicU64 = AtomicU64::new(0);
/// (TSC, idle cycles) at the previous `cpu_usage` call.
static LAST_SAMPLE: crate::sync::SpinLock<(u64, u64)> = crate::sync::SpinLock::new((0, 0));

/// Tasks waiting or ready but not running anywhere, as of the last scheduling decision:
/// whether an idle CPU might find something to do.
static QUEUED: AtomicUsize = AtomicUsize::new(0);
/// An idle CPU was kicked by the timer to look for work and has not looked yet.
static TICK_KICKED: AtomicBool = AtomicBool::new(false);

fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: RDTSC only reads the timestamp counter.
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

/// CPU load in percent since the previous call (all CPUs together).
pub fn cpu_usage() -> u64 {
    let now = rdtsc();
    let idle = IDLE_CYCLES.load(Ordering::Relaxed);
    let Some(mut last) = LAST_SAMPLE.try_lock() else { return 0 };
    let (t0, i0) = *last;
    *last = (now, idle);
    let total = now.wrapping_sub(t0).saturating_mul(smp::count() as u64);
    if t0 == 0 || total == 0 {
        return 0;
    }
    100 - (idle.wrapping_sub(i0) * 100 / total).min(100)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Ready,
    /// Parked until IRQ_COUNT differs from the stored value.
    Waiting(u64),
    /// Parked until [`wake`] or the deadline (a tick count; u64::MAX: none). Nothing polls.
    Blocked(u64),
    /// Parked until [`wake`], a device event (DEV_IRQ differs from the first value) or the
    /// deadline (the second): an I/O wait, which timer ticks alone do not end.
    Io(u64, u64),
    Finished,
}

struct Task {
    id: usize,
    name: String,
    state: State,
    /// Saved stack pointer while not running.
    rsp: u64,
    /// None for the boot task, which runs on Limine's stack.
    stack: Option<Box<[u8]>>,
    entry: Option<Box<dyn FnOnce() + Send>>,
    /// Terminal for this task's print!/read_key, if any.
    tty: Option<crate::tty::TtyRef>,
    /// CPU state of the user program this task runs, if any (see process.rs).
    user: Option<Box<crate::process::UserState>>,
    /// Times switched in (debugging: `ps`).
    runs: u64,
    /// The CPU running this task (it is that CPU's current task).
    cpu: Option<usize>,
    /// Set for CPU n's idle task: it runs only there, and only when nothing else can.
    idle_of: Option<usize>,
    /// CPUs the task may run on (bit n = CPU n; sched_setaffinity).
    affinity: u64,
    /// [`wake`] came while the task was not parked: its next [`block`] returns at once.
    wake_pending: bool,
    /// Its Linux task_struct (Linux's `current` while it runs; linuxkpi.rs), or 0.
    lx: usize,
    /// Earliest timerfd expiration (ticks) among those its current I/O wait polled.
    io_timer: u64,
    /// Keys of the objects its current I/O wait polled (ipc::watch).
    watch: Vec<usize>,
}

impl Task {
    fn new(id: usize, name: &str, rsp: u64, stack: Option<Box<[u8]>>, entry: Option<Box<dyn FnOnce() + Send>>, tty: Option<crate::tty::TtyRef>) -> Self {
        Self { id, name: name.into(), state: State::Ready, rsp, stack, entry, tty, user: None, runs: 0, cpu: None, idle_of: None, affinity: u64::MAX, wake_pending: false, lx: 0, io_timer: u64::MAX, watch: Vec::new() }
    }
}

struct Scheduler {
    tasks: Vec<Task>,
    /// Index of each CPU's current task (in `tasks`).
    current: [usize; MAX_CPUS],
    /// Id of each CPU's idle task (0 = none yet).
    idle: [usize; MAX_CPUS],
    next_id: usize,
}

struct SchedulerCell(UnsafeCell<Option<Scheduler>>);
// SAFETY: only kernel code under the big kernel lock touches the scheduler (one CPU at a
// time), with interrupts disabled where it changes structure, and IRQ handlers do not use
// it.
unsafe impl Sync for SchedulerCell {}
static SCHEDULER: SchedulerCell = SchedulerCell(UnsafeCell::new(None));

fn scheduler() -> &'static mut Scheduler {
    // SAFETY: see SchedulerCell. Callers never hold the reference across a switch.
    unsafe { (*SCHEDULER.0.get()).as_mut().expect("task::init not called") }
}

/// Index of the task running on this CPU.
fn cur(s: &Scheduler) -> usize {
    s.current[smp::id()]
}

fn irq_save() -> u64 {
    let flags: u64;
    // SAFETY: save RFLAGS, disable interrupts.
    unsafe { core::arch::asm!("pushfq", "pop {}", "cli", out(reg) flags, options(nomem)) };
    flags
}

fn irq_restore(flags: u64) {
    if flags & (1 << 9) != 0 {
        // SAFETY: interrupts were enabled before irq_save.
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    }
}

/// Turn the running boot code into task 0 ("kernel"), and give the boot CPU its idle task.
pub fn init() {
    let mut boot = Task::new(0, "kernel", 0, None, None, None);
    boot.cpu = Some(0);
    // SAFETY: called once, before any other task exists.
    unsafe {
        *SCHEDULER.0.get() = Some(Scheduler { tasks: alloc::vec![boot], current: [0; MAX_CPUS], idle: [0; MAX_CPUS], next_id: 1 });
    }
    let id = spawn_with_tty("idle0", None, || idle_loop(0, true));
    let s = scheduler();
    let t = s.tasks.iter_mut().find(|t| t.id == id).unwrap();
    t.idle_of = Some(0);
    t.affinity = 1;
    s.idle[0] = id;
}

/// CPU `cpu`'s idle task, before the CPU starts: it starts on this task's stack, already
/// running it ([`run_idle`]). Returns the stack top.
pub fn add_idle(cpu: usize) -> u64 {
    let s = scheduler();
    let mut stack = alloc::vec![0u8; IDLE_STACK_SIZE].into_boxed_slice();
    let top = (stack.as_mut_ptr() as u64 + IDLE_STACK_SIZE as u64) & !15;
    let id = s.next_id;
    s.next_id += 1;
    let mut t = Task::new(id, &alloc::format!("idle{cpu}"), 0, Some(stack), None, None);
    t.idle_of = Some(cpu);
    t.affinity = 1 << cpu;
    t.cpu = Some(cpu);
    s.tasks.push(t);
    s.current[cpu] = s.tasks.len() - 1;
    s.idle[cpu] = id;
    top
}

/// An application processor, fully initialised, becomes its idle task. It does not hold
/// the big kernel lock yet.
pub fn run_idle(cpu: usize) -> ! {
    idle_loop(cpu, false)
}

/// Start a new task. It first runs when a CPU schedules (an idle one is woken for it).
pub fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> usize {
    spawn_with_tty(name, None, f)
}

/// Start a new task attached to a terminal.
pub fn spawn_with_tty(name: &str, tty: Option<crate::tty::TtyRef>, f: impl FnOnce() + Send + 'static) -> usize {
    let mut stack = alloc::vec![0u8; STACK_SIZE].into_boxed_slice();
    let top = (stack.as_mut_ptr() as u64 + STACK_SIZE as u64) & !15;
    // Initial frame consumed by `switch`: RFLAGS (IF set), six callee-saved
    // registers, then the return address (`task_trampoline`), then a dummy return
    // address for the trampoline itself, so it starts with RSP = 8 mod 16 like any
    // called function.
    let frame = [0x202u64, 0, 0, 0, 0, 0, 0, task_trampoline as *const () as usize as u64, 0];
    let rsp = top - (frame.len() * 8) as u64;
    // SAFETY: the frame fits at the top of the freshly allocated stack.
    unsafe { core::ptr::copy_nonoverlapping(frame.as_ptr(), rsp as *mut u64, frame.len()) };
    let flags = irq_save();
    let s = scheduler();
    let id = s.next_id;
    s.next_id += 1;
    irq_restore(flags);
    #[cfg_attr(not(feature = "linux-drivers"), allow(unused_mut))]
    let mut t = Task::new(id, name, rsp, Some(stack), Some(Box::new(f)), tty);
    #[cfg(feature = "linux-drivers")]
    {
        t.lx = crate::linuxkpi::task_new(id);
    }
    // `s` again: the allocation above may have yielded.
    let flags = irq_save();
    scheduler().tasks.push(t);
    irq_restore(flags);
    smp::kick_idle();
    id
}

extern "C" fn task_trampoline() -> ! {
    let entry = {
        let s = scheduler();
        let i = cur(s);
        s.tasks[i].entry.take().expect("task started twice")
    };
    entry();
    #[cfg(feature = "linux-drivers")]
    {
        let s = scheduler();
        let i = cur(s);
        crate::linuxkpi::task_exit(core::mem::take(&mut s.tasks[i].lx));
    }
    let s = scheduler();
    let i = cur(s);
    s.tasks[i].state = State::Finished;
    loop {
        yield_now(); // never scheduled again; reaped by another task
    }
}

/// The current task's terminal. None before `init` (early boot) or without one.
pub fn current_tty() -> Option<crate::tty::TtyRef> {
    // SAFETY: see SchedulerCell.
    let s = unsafe { (*SCHEDULER.0.get()).as_ref()? };
    s.tasks.get(s.current[smp::id()])?.tty.clone()
}

/// Attach (or detach) the current task's terminal.
pub fn set_tty(tty: Option<crate::tty::TtyRef>) {
    let s = scheduler();
    let current = cur(s);
    s.tasks[current].tty = tty;
}

/// True if a task with this id is still alive.
pub fn alive(id: usize) -> bool {
    scheduler().tasks.iter().any(|t| t.id == id && t.state != State::Finished)
}

/// Let other ready tasks run; returns when this task is scheduled again.
pub fn yield_now() {
    schedule();
}

/// Park until the next interrupt, letting other tasks run meanwhile.
pub fn wait_event() {
    let s = scheduler();
    let current = cur(s);
    s.tasks[current].state = State::Waiting(IRQ_COUNT.load(Ordering::SeqCst));
    schedule();
}

/// Park until [`wake`] or `deadline` (a tick count), whichever comes first; unlike
/// [`wait_event`], interrupts do not end the wait, so the task costs nothing meanwhile.
/// Callers re-check their condition after every return (wakes may be early).
pub fn block(deadline: Option<u64>) {
    let flags = irq_save();
    let s = scheduler();
    let current = cur(s);
    let t = &mut s.tasks[current];
    if core::mem::take(&mut t.wake_pending) {
        irq_restore(flags);
        return;
    }
    let d = deadline.unwrap_or(u64::MAX);
    t.state = State::Blocked(d);
    NEXT_DEADLINE.fetch_min(d, Ordering::Relaxed);
    schedule();
    irq_restore(flags);
}

/// A timerfd the current task just polled expires at tick `at`: its next I/O wait ends
/// by then.
pub fn note_timer(at: u64) {
    let s = scheduler();
    let i = cur(s);
    s.tasks[i].io_timer = s.tasks[i].io_timer.min(at);
}

/// The current task's next I/O wait is for the object with this key.
pub fn watch(key: usize) {
    let s = scheduler();
    let i = cur(s);
    let w = &mut s.tasks[i].watch;
    // Re-checks poll the same objects again; a scan that never waits stops growing here.
    if w.len() < 4096 && !w.contains(&key) {
        w.push(key);
    }
}

/// The keys noted by [`watch`] since the last call.
pub fn take_watch() -> Vec<usize> {
    let s = scheduler();
    let i = cur(s);
    core::mem::take(&mut s.tasks[i].watch)
}

/// The earliest tick noted by [`note_timer`] since the last call (u64::MAX: none).
pub fn take_timer() -> u64 {
    let s = scheduler();
    let i = cur(s);
    core::mem::replace(&mut s.tasks[i].io_timer, u64::MAX)
}

/// Park for I/O ([`crate::ipc::wait`]) until [`wake`], a device event or `deadline` (a
/// tick count). A wake that came since the task last parked returns at once.
pub fn wait_io(deadline: u64) {
    let flags = irq_save();
    let s = scheduler();
    let current = cur(s);
    let t = &mut s.tasks[current];
    if core::mem::take(&mut t.wake_pending) {
        irq_restore(flags);
        return;
    }
    t.state = State::Io(DEV_IRQ.load(Ordering::SeqCst), deadline);
    NEXT_DEADLINE.fetch_min(deadline, Ordering::Relaxed);
    schedule();
    irq_restore(flags);
}

/// The earliest deadline of a blocked task (an upper bound: it is only lowered here and
/// recomputed when scheduling), for the boot CPU's tick to know when to look again.
static NEXT_DEADLINE: AtomicU64 = AtomicU64::new(u64::MAX);

/// Make a parked task runnable now (e.g. a futex waiter), instead of at the next interrupt.
pub fn wake(id: usize) {
    let flags = irq_save();
    let s = scheduler();
    let mut kick = false;
    if let Some(t) = s.tasks.iter_mut().find(|t| t.id == id) {
        if matches!(t.state, State::Waiting(_) | State::Blocked(_) | State::Io(..)) {
            t.state = State::Ready;
            kick = t.cpu.is_none();
        } else if t.state == State::Ready {
            t.wake_pending = true;
        }
    }
    irq_restore(flags);
    if kick {
        smp::kick_idle();
    }
}

/// Sleep for at least `ticks` timer ticks.
pub fn sleep_ticks(ticks: u64) {
    let end = crate::pit::ticks() + ticks;
    while crate::pit::ticks() < end {
        wait_event();
    }
}

/// The boot CPU's timer tick (interrupt context): parked tasks may be runnable now; if this
/// CPU is busy, wake an idle one to run them (one at a time: it wakes others if it finds
/// more work than it can run).
pub fn tick_kick() {
    let work = QUEUED.load(Ordering::Relaxed) > 0
        || crate::pit::ticks() >= NEXT_DEADLINE.load(Ordering::Relaxed)
        || DEV_IRQ.load(Ordering::Relaxed) != DEV_SEEN.load(Ordering::Relaxed);
    // At the preemption rate: the busy CPU itself looks that often.
    if smp::count() > 1 && work && crate::pit::ticks() % crate::pit::PREEMPT_TICKS == 0 && !smp::is_idle(smp::id()) && !TICK_KICKED.load(Ordering::Relaxed) && smp::kick_idle() {
        TICK_KICKED.store(true, Ordering::Relaxed);
    }
}

fn runnable(state: State) -> bool {
    match state {
        State::Ready => true,
        State::Waiting(seen) => IRQ_COUNT.load(Ordering::SeqCst) != seen,
        State::Blocked(deadline) => crate::pit::ticks() >= deadline,
        State::Io(seen, deadline) => DEV_IRQ.load(Ordering::SeqCst) != seen || crate::pit::ticks() >= deadline,
        State::Finished => false,
    }
}

impl Scheduler {
    /// Free the stacks of finished tasks that no CPU runs any more, keeping each CPU's
    /// current index right.
    fn reap(&mut self) {
        if !self.tasks.iter().any(|t| t.state == State::Finished && t.cpu.is_none()) {
            return;
        }
        let online = smp::online_mask();
        let mut ids = [usize::MAX; MAX_CPUS];
        for (c, id) in ids.iter_mut().enumerate() {
            if online & (1 << c) != 0 {
                *id = self.tasks[self.current[c]].id;
            }
        }
        self.tasks.retain(|t| t.state != State::Finished || t.cpu.is_some());
        for (c, &id) in ids.iter().enumerate() {
            if id != usize::MAX {
                self.current[c] = self.tasks.iter().position(|t| t.id == id).unwrap();
            }
        }
    }

    /// May CPU `cpu`, whose current task is at `current`, run task `i` now?
    fn eligible(&self, i: usize, cpu: usize, current: usize) -> bool {
        let t = &self.tasks[i];
        t.idle_of.is_none() && (t.cpu.is_none() || i == current) && t.affinity & (1 << cpu) != 0 && runnable(t.state)
    }
}

fn schedule() {
    // Other CPUs waiting to enter the kernel (system calls) go first: this CPU may be
    // about to run task after task, all under the lock.
    smp::relax();
    let flags = irq_save();
    TICK_KICKED.store(false, Ordering::Relaxed);
    let me = smp::id();
    let s = scheduler();
    s.reap();
    let current = s.current[me];
    let n = s.tasks.len();
    // Round robin from the task after the current one; the current task itself last.
    let mut next = None;
    let mut more = false;
    let mut queued = 0;
    let mut deadline = u64::MAX;
    DEV_SEEN.store(DEV_IRQ.load(Ordering::Relaxed), Ordering::Relaxed);
    for k in 1..=n {
        let i = (current + k) % n;
        let t = &s.tasks[i];
        match t.state {
            State::Blocked(d) | State::Io(_, d) => deadline = deadline.min(d),
            State::Finished => {}
            _ if t.cpu.is_none() && t.idle_of.is_none() => queued += 1,
            _ => {}
        }
        if s.eligible(i, me, current) {
            if next.is_none() {
                next = Some(i);
            } else if i != current && s.tasks[i].state == State::Ready {
                // Only woken or preempted tasks are worth a CPU: a parked task that is
                // runnable because an interrupt came usually just checks and parks again,
                // which other CPUs cannot do in parallel anyway (the kernel lock).
                more = true;
            }
        }
    }
    QUEUED.store(queued, Ordering::Relaxed);
    NEXT_DEADLINE.store(deadline, Ordering::Relaxed);
    let next = match next {
        Some(i) => {
            s.tasks[i].state = State::Ready;
            i
        }
        // Nothing to run: this CPU's idle task (if it is not the one asking).
        None if s.tasks[current].idle_of == Some(me) => current,
        None => s.tasks.iter().position(|t| t.id == s.idle[me]).expect("CPU without an idle task"),
    };
    if next != current {
        // The task this CPU leaves may be picked up elsewhere if it can run; and work that
        // this CPU cannot take now goes to an idle CPU.
        if more || (s.tasks[current].idle_of.is_none() && s.tasks[current].state == State::Ready) {
            smp::kick_idle();
        }
        switch_to(s, current, next, me);
    } else if more {
        smp::kick_idle();
    }
    irq_restore(flags);
}

/// Switch CPU `me` from task `old` to task `new` (indices; BKL held, interrupts off).
/// Returns when `old` runs again, on whichever CPU picks it.
fn switch_to(s: &mut Scheduler, old: usize, new: usize, me: usize) {
    s.tasks[new].runs += 1;
    s.tasks[old].cpu = None;
    s.tasks[new].cpu = Some(me);
    s.current[me] = new;
    crate::process::switch_out(s.tasks[old].user.as_deref_mut());
    crate::process::switch_in(s.tasks[new].user.as_deref());
    #[cfg(feature = "linux-drivers")]
    crate::linuxkpi::switch_to(s.tasks[new].lx);
    let old_rsp = &raw mut s.tasks[old].rsp;
    let new_rsp = s.tasks[new].rsp;
    // SAFETY: new_rsp was saved by `switch` or built by `spawn`; old_rsp points into the
    // task list, which is not modified until `switch` has written it (no other CPU runs
    // kernel code before this one releases the big kernel lock, which only the next
    // task can do). Vec reallocation later moves Task values, `rsp` with them.
    unsafe { switch(old_rsp, new_rsp) };
}

/// The idle task of CPU `cpu`: run whatever is runnable; when nothing is, halt with the
/// big kernel lock released until kicked (or, on the boot CPU, any interrupt: parked tasks
/// wait for exactly that). `held`: this CPU holds the lock already.
fn idle_loop(cpu: usize, mut held: bool) -> ! {
    loop {
        if !held {
            smp::lock();
        }
        let seen = IRQ_COUNT.load(Ordering::SeqCst);
        schedule(); // runs other tasks; returns when this CPU has nothing to do
        TICK_KICKED.store(false, Ordering::Relaxed);
        let flags = irq_save();
        smp::set_idle(cpu, true);
        smp::unlock();
        held = false;
        let t0 = rdtsc();
        // Interrupts stay off between each check and `hlt` (`sti; hlt` is atomic), so a
        // wake-up between them is not lost.
        while !smp::take_kick(cpu) && (cpu != 0 || IRQ_COUNT.load(Ordering::SeqCst) == seen) {
            // SAFETY: halts until the next interrupt, which is handled at once.
            unsafe { core::arch::asm!("sti", "hlt", "cli", options(nomem, nostack)) };
        }
        let idle = rdtsc().wrapping_sub(t0);
        IDLE_CYCLES.fetch_add(idle, Ordering::Relaxed);
        smp::IDLE_CYCLES[cpu].fetch_add(idle, Ordering::Relaxed);
        smp::set_idle(cpu, false);
        irq_restore(flags);
    }
}

/// Save callee-saved registers and RSP to `*old_rsp`, load `new_rsp`, restore, return
/// into the other task.
#[unsafe(naked)]
unsafe extern "C" fn switch(old_rsp: *mut u64, new_rsp: u64) {
    naked_asm!(
        // RFLAGS travels with the task: one switched out from inside an IRQ handler
        // (preempted user program) has IF clear, ordinary tasks have it set.
        "pushfq",
        "push rbx", "push rbp", "push r12", "push r13", "push r14", "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop rbp", "pop rbx",
        "popfq",
        "ret",
    )
}

/// The CPUs task `tid` (a thread id; 0 = the calling thread) may run on.
pub fn affinity(tid: u64) -> Option<u64> {
    let s = scheduler();
    let i = find_thread(s, tid)?;
    Some(s.tasks[i].affinity & smp::online_mask())
}

/// sched_setaffinity: restrict thread `tid` (0 = the calling thread) to the CPUs in
/// `mask`. False if there is no such thread or no online CPU in the mask.
pub fn set_affinity(tid: u64, mask: u64) -> bool {
    let s = scheduler();
    let Some(i) = find_thread(s, tid) else { return false };
    if mask & smp::online_mask() == 0 {
        return false;
    }
    s.tasks[i].affinity = mask;
    // Move off a CPU it may no longer use.
    if i == cur(s) && mask & (1 << smp::id()) == 0 {
        yield_now();
    }
    true
}

fn find_thread(s: &Scheduler, tid: u64) -> Option<usize> {
    if tid == 0 {
        return Some(cur(s));
    }
    s.tasks.iter().position(|t| t.user.as_deref().is_some_and(|u| u.tid == tid))
}

/// Wake every parked thread of process `pid` (it is being torn down).
pub fn wake_process(pid: u64) {
    let ids: Vec<usize> = scheduler().tasks.iter().filter(|t| t.user.as_deref().is_some_and(|u| u.process.pid == pid)).map(|t| t.id).collect();
    for id in ids {
        wake(id);
    }
}

/// CPUs that run a thread of process `pid` now (to kick them into the kernel).
pub fn cpus_running(pid: u64) -> u64 {
    scheduler().tasks.iter().filter(|t| t.user.as_deref().is_some_and(|u| u.process.pid == pid)).filter_map(|t| t.cpu).fold(0, |m, c| m | 1 << c)
}

/// The current task's user-program state.
pub fn current_user() -> Option<&'static mut crate::process::UserState> {
    let s = scheduler();
    let current = cur(s);
    s.tasks[current].user.as_deref_mut()
}

/// Attach or detach user-program state to the current task (and load it).
pub fn set_user(user: Option<Box<crate::process::UserState>>) {
    let s = scheduler();
    let current = cur(s);
    let old = core::mem::replace(&mut s.tasks[current].user, user);
    // Load the new address space before the old one (and its page tables) is freed.
    crate::process::switch_in(s.tasks[current].user.as_deref());
    drop(old);
}

pub fn current_id() -> usize {
    let s = scheduler();
    s.tasks[cur(s)].id
}

/// `current_id`, or None before the scheduler exists.
pub fn current_id_checked() -> Option<usize> {
    // SAFETY: see SchedulerCell.
    let s = unsafe { (*SCHEDULER.0.get()).as_ref()? };
    s.tasks.get(s.current[smp::id()]).map(|t| t.id)
}

/// The Linux task_struct of the task running on this CPU, or None before the scheduler
/// exists (smp.rs: Linux's `current` follows the big kernel lock).
#[cfg(feature = "linux-drivers")]
pub fn current_lx_checked() -> Option<usize> {
    // SAFETY: see SchedulerCell.
    let s = unsafe { (*SCHEDULER.0.get()).as_ref()? };
    s.tasks.get(s.current[smp::id()]).map(|t| t.lx)
}

/// The Linux task_struct of task `id` (0 if none or the task is gone).
#[cfg(feature = "linux-drivers")]
pub fn lx_task(id: usize) -> usize {
    scheduler().tasks.iter().find(|t| t.id == id).map_or(0, |t| t.lx)
}

/// The last system calls of every thread of process `pid` (the `recent` command).
pub fn recent_calls(pid: u64) -> Vec<String> {
    let s = scheduler();
    let mut out = Vec::new();
    for t in &s.tasks {
        if let Some(u) = t.user.as_deref()
            && u.process.pid == pid
        {
            out.push(alloc::format!("thread {}{}:", u.tid, if u.comm.is_empty() { String::new() } else { alloc::format!(" \"{}\"", u.comm) }));
            out.extend(u.recent.lines());
        }
    }
    out
}

/// For a kernel panic: which program was running, the system call it was in, and its
/// last ones (an allocation failure is usually a size a program passed in).
pub fn panic_context() {
    let Some(u) = current_user() else { return };
    let comm = if u.comm.is_empty() { String::new() } else { alloc::format!(" \"{}\"", u.comm) };
    crate::println!("[nova] while running pid {} thread {}{comm}", u.process.pid, u.tid);
    if u.in_syscall != u64::MAX {
        crate::println!("[nova] in system call {}({:#x}, {:#x}, ...)", crate::linux::syscall_label(u.in_syscall), u.syscall_arg, u.syscall_arg2);
    }
    crate::println!("[nova] its last system calls:");
    for line in u.recent.lines() {
        crate::println!("{line}");
    }
}

/// Run `f` on every thread of process `pid`, the calling one included (seccomp TSYNC).
/// Under the big kernel lock: the others are parked in the kernel or running user code,
/// and see the change at their next system call.
pub fn each_thread(pid: u64, mut f: impl FnMut(&mut crate::process::UserState)) {
    for t in scheduler().tasks.iter_mut() {
        if let Some(u) = t.user.as_deref_mut()
            && u.process.pid == pid
        {
            f(u);
        }
    }
}

/// Run `f` on the user state of thread `tid` of process `pid` (tgkill). False if there
/// is no such thread. Not for the calling thread, whose state is `process::thread()`.
pub fn with_user_thread(pid: u64, tid: u64, f: impl FnOnce(&mut crate::process::UserState)) -> bool {
    let s = scheduler();
    match s.tasks.iter_mut().filter_map(|t| t.user.as_deref_mut()).find(|u| u.process.pid == pid && u.tid == tid) {
        Some(u) => {
            f(u);
            true
        }
        None => false,
    }
}

/// (id, name, state) of every task, for the `ps` command.
pub fn list() -> Vec<(usize, String, &'static str)> {
    let s = scheduler();
    s.tasks
        .iter()
        .filter(|t| t.idle_of.is_none())
        .map(|t| {
            let state = if t.cpu.is_some() {
                "running"
            } else {
                match t.state {
                    State::Ready => "ready",
                    State::Waiting(_) | State::Blocked(_) | State::Io(..) => "waiting",
                    State::Finished => "finished",
                }
            };
            let name = match t.user.as_deref() {
                Some(u) if u.in_syscall != u64::MAX => {
                    let n = u.in_syscall & 0xFFF;
                    // read, write, poll-family and socket calls: show what the descriptor is.
                    let fd_call = if u.in_syscall & 0x1000 != 0 { matches!(n, 3 | 4 | 146 | 145 | 371 | 372 | 370) } else { matches!(n, 0 | 1 | 19 | 20 | 45 | 46 | 47 | 44) };
                    let futex = if u.in_syscall & 0x1000 != 0 { matches!(n, 240 | 422) } else { n == 202 };
                    let what = if fd_call {
                        alloc::format!(" fd {} = {}", u.syscall_arg, crate::linux::describe_fd(&u.process, u.syscall_arg))
                    } else if u.wake_at != 0 && matches!(n, 35 | 230 | 267 | 162 | 407) {
                        alloc::format!(" wake at {} ms, now {} ms", u.wake_at / 1_000_000, crate::pit::nanos() / 1_000_000)
                    } else if futex {
                        alloc::format!(" futex {:#x} op {}", u.syscall_arg, u.syscall_arg2)
                    } else {
                        String::new()
                    };
                    alloc::format!("{} (in syscall {}{}{what}, {})", t.name, n, if u.in_syscall & 0x1000 != 0 { " i386" } else { "" }, u.phase)
                }
                _ => t.name.clone(),
            };
            // The program (and a Chromium child's --type) rather than the inherited task name.
            let prog = t.user.as_deref().map(|u| {
                let exe = u.process.exe.try_lock().map(|e| String::from(e.rsplit('/').next().unwrap_or(""))).unwrap_or_default();
                let kind = u.process.argv.try_lock().and_then(|a| a.iter().find(|a| a.starts_with("--type=")).cloned()).unwrap_or_default();
                let comm = if u.comm.is_empty() { String::new() } else { alloc::format!(" \"{}\"", u.comm) };
                alloc::format!(" <{exe}{}{kind}>{comm}", if kind.is_empty() { "" } else { " " })
            });
            let on = match t.cpu {
                Some(c) if smp::count() > 1 => alloc::format!(", cpu {c}"),
                _ => String::new(),
            };
            (t.id, alloc::format!("{name}{} [runs {}, {:?}{on}]", prog.unwrap_or_default(), t.runs, t.state), state)
        })
        .collect()
}
