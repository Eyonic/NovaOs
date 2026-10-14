//! Cooperative kernel tasks.
//!
//! Every task has its own stack and runs until it calls [`yield_now`] or
//! [`wait_event`]. There is no preemption: a task is never interrupted by another
//! task, only by IRQ handlers (which never touch scheduler state). That makes all
//! kernel data structures single-threaded from the tasks' point of view, as long as
//! no task yields while holding a lock.
//!
//! [`wait_event`] parks the current task until the next interrupt of any kind (timer,
//! keyboard, mouse, serial). The timer fires at `pit::HZ`, so a task that polls a
//! device in a `wait_event` loop checks it at least that often. When every task is
//! parked, the CPU halts until an interrupt arrives.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::arch::naked_asm;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

const STACK_SIZE: usize = 512 * 1024;

/// Incremented by every IRQ (see interrupts.rs); parked tasks wait for it to change.
pub static IRQ_COUNT: AtomicU64 = AtomicU64::new(0);

/// Timestamp-counter cycles spent halted with every task parked.
static IDLE_CYCLES: AtomicU64 = AtomicU64::new(0);
/// (TSC, idle cycles) at the previous `cpu_usage` call.
static LAST_SAMPLE: crate::sync::SpinLock<(u64, u64)> = crate::sync::SpinLock::new((0, 0));

fn rdtsc() -> u64 {
    let (lo, hi): (u32, u32);
    // SAFETY: RDTSC only reads the timestamp counter.
    unsafe { core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags)) };
    (hi as u64) << 32 | lo as u64
}

/// CPU load in percent since the previous call.
pub fn cpu_usage() -> u64 {
    let now = rdtsc();
    let idle = IDLE_CYCLES.load(Ordering::Relaxed);
    let Some(mut last) = LAST_SAMPLE.try_lock() else { return 0 };
    let (t0, i0) = *last;
    *last = (now, idle);
    let total = now.wrapping_sub(t0);
    if t0 == 0 || total == 0 {
        return 0;
    }
    100 - (idle.wrapping_sub(i0) * 100 / total).min(100)
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Ready,
    /// Parked until IRQ_COUNT differs from the stored value.
    Waiting(u64),
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
}

struct Scheduler {
    tasks: Vec<Task>,
    current: usize,
    next_id: usize,
}

struct SchedulerCell(UnsafeCell<Option<Scheduler>>);
// SAFETY: only tasks touch the scheduler, and tasks never run concurrently (one CPU,
// no preemption, IRQ handlers do not use it).
unsafe impl Sync for SchedulerCell {}
static SCHEDULER: SchedulerCell = SchedulerCell(UnsafeCell::new(None));

fn scheduler() -> &'static mut Scheduler {
    // SAFETY: see SchedulerCell. Callers never hold the reference across a switch.
    unsafe { (*SCHEDULER.0.get()).as_mut().expect("task::init not called") }
}

/// Turn the running boot code into task 0 ("kernel").
pub fn init() {
    // SAFETY: called once, before any other task exists.
    unsafe {
        *SCHEDULER.0.get() = Some(Scheduler {
            tasks: alloc::vec![Task { id: 0, name: "kernel".into(), state: State::Ready, rsp: 0, stack: None, entry: None, tty: None, user: None }],
            current: 0,
            next_id: 1,
        });
    }
}

/// Start a new task. It first runs when the current task yields.
pub fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> usize {
    spawn_with_tty(name, None, f)
}

/// Start a new task attached to a terminal.
pub fn spawn_with_tty(name: &str, tty: Option<crate::tty::TtyRef>, f: impl FnOnce() + Send + 'static) -> usize {
    let s = scheduler();
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
    let id = s.next_id;
    s.next_id += 1;
    s.tasks.push(Task { id, name: name.into(), state: State::Ready, rsp, stack: Some(stack), entry: Some(Box::new(f)), tty, user: None });
    id
}

extern "C" fn task_trampoline() -> ! {
    let entry = {
        let s = scheduler();
        s.tasks[s.current].entry.take().expect("task started twice")
    };
    entry();
    let s = scheduler();
    s.tasks[s.current].state = State::Finished;
    loop {
        yield_now(); // never scheduled again; reaped by another task
    }
}

/// The current task's terminal. None before `init` (early boot) or without one.
pub fn current_tty() -> Option<crate::tty::TtyRef> {
    // SAFETY: see SchedulerCell.
    let s = unsafe { (*SCHEDULER.0.get()).as_ref()? };
    s.tasks.get(s.current)?.tty.clone()
}

/// Attach (or detach) the current task's terminal.
pub fn set_tty(tty: Option<crate::tty::TtyRef>) {
    let s = scheduler();
    let current = s.current;
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
    let current = s.current;
    s.tasks[current].state = State::Waiting(IRQ_COUNT.load(Ordering::SeqCst));
    schedule();
}

/// Make a parked task runnable now (e.g. a futex waiter), instead of at the next interrupt.
pub fn wake(id: usize) {
    let s = scheduler();
    if let Some(t) = s.tasks.iter_mut().find(|t| t.id == id)
        && matches!(t.state, State::Waiting(_))
    {
        t.state = State::Ready;
    }
}

/// Sleep for at least `ticks` timer ticks.
pub fn sleep_ticks(ticks: u64) {
    let end = crate::pit::ticks() + ticks;
    while crate::pit::ticks() < end {
        wait_event();
    }
}

fn runnable(state: State) -> bool {
    match state {
        State::Ready => true,
        State::Waiting(seen) => IRQ_COUNT.load(Ordering::SeqCst) != seen,
        State::Finished => false,
    }
}

fn schedule() {
    loop {
        let s = scheduler();
        // Free the stacks of finished tasks other than the current one.
        let current_id = s.tasks[s.current].id;
        s.tasks.retain(|t| t.state != State::Finished || t.id == current_id);
        s.current = s.tasks.iter().position(|t| t.id == current_id).unwrap();

        let n = s.tasks.len();
        let next = (1..=n).map(|i| (s.current + i) % n).find(|&i| runnable(s.tasks[i].state));
        match next {
            Some(next) => {
                s.tasks[next].state = State::Ready;
                if next != s.current {
                    let old = s.current;
                    s.current = next;
                    crate::process::switch_out(s.tasks[old].user.as_deref_mut());
                    crate::process::switch_in(s.tasks[next].user.as_deref());
                    let old_rsp = &raw mut s.tasks[old].rsp;
                    let new_rsp = s.tasks[next].rsp;
                    // SAFETY: new_rsp was saved by `switch` or built by `spawn`; old_rsp
                    // points into the task list, which is not modified while switched out
                    // (Vec reallocation moves Task values, but `rsp` is written before
                    // any other task runs).
                    unsafe { switch(old_rsp, new_rsp) };
                }
                return;
            }
            None => {
                // Everyone is parked: halt until an interrupt changes IRQ_COUNT.
                let seen = IRQ_COUNT.load(Ordering::SeqCst);
                let t0 = rdtsc();
                crate::input::wait_for_interrupt(|| IRQ_COUNT.load(Ordering::SeqCst) == seen);
                IDLE_CYCLES.fetch_add(rdtsc().wrapping_sub(t0), Ordering::Relaxed);
            }
        }
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

/// The current task's user-program state.
pub fn current_user() -> Option<&'static mut crate::process::UserState> {
    let s = scheduler();
    let current = s.current;
    s.tasks[current].user.as_deref_mut()
}

/// Attach or detach user-program state to the current task (and load it).
pub fn set_user(user: Option<Box<crate::process::UserState>>) {
    let s = scheduler();
    let current = s.current;
    let old = core::mem::replace(&mut s.tasks[current].user, user);
    // Load the new address space before the old one (and its page tables) is freed.
    crate::process::switch_in(s.tasks[current].user.as_deref());
    drop(old);
}

pub fn current_id() -> usize {
    let s = scheduler();
    s.tasks[s.current].id
}

/// (id, name, state) of every task, for the `ps` command.
pub fn list() -> Vec<(usize, String, &'static str)> {
    let s = scheduler();
    s.tasks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let state = if i == s.current {
                "running"
            } else {
                match t.state {
                    State::Ready => "ready",
                    State::Waiting(_) => "waiting",
                    State::Finished => "finished",
                }
            };
            (t.id, t.name.clone(), state)
        })
        .collect()
}
