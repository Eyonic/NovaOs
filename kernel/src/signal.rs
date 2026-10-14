//! POSIX signals for Linux programs: dispositions, masks, sending, delivery and return.
//!
//! The ABI is Linux's x86-64 one, so glibc's `sigaction()`, `raise()`, handlers and
//! `sigreturn` work unchanged: `rt_sigaction` takes the kernel `struct sigaction`
//! (handler, flags, restorer, mask), and a handler runs on an `rt_sigframe` (return address
//! = the restorer, `ucontext` with the interrupted registers in `sigcontext`, `siginfo`,
//! and the FPU/SIMD state in an XSAVE area marked with Linux's FP_XSTATE magic numbers).
//!
//! Signals are delivered when a thread returns from a system call (process.rs calls
//! [`deliver`]); a program that never makes one sees its signals at its next call.
//! Dispositions are per process, masks and thread-directed signals per thread.

use crate::process::{self, SyscallFrame, UserState};

pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGILL: u32 = 4;
pub const SIGABRT: u32 = 6;
pub const SIGBUS: u32 = 7;
pub const SIGFPE: u32 = 8;
pub const SIGKILL: u32 = 9;
pub const SIGSEGV: u32 = 11;
pub const SIGPIPE: u32 = 13;
pub const SIGTERM: u32 = 15;
pub const SIGCHLD: u32 = 17;
pub const SIGCONT: u32 = 18;
pub const SIGSTOP: u32 = 19;
pub const SIGTSTP: u32 = 20;
pub const SIGTTIN: u32 = 21;
pub const SIGTTOU: u32 = 22;
pub const SIGURG: u32 = 23;
pub const SIGWINCH: u32 = 28;
pub const NSIG: u32 = 64;

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;
const SA_SIGINFO: u64 = 0x4;
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;
const SS_ONSTACK: u32 = 1;
const SS_DISABLE: u32 = 2;
const SI_USER: i32 = 0;
const SI_TKILL: i32 = -6;

/// One entry of the kernel's `struct sigaction` (x86-64: 32 bytes).
#[derive(Clone, Copy, Default)]
pub struct Action {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

/// A process's signal state shared by its threads.
pub struct Signals {
    pub actions: [Action; NSIG as usize + 1],
    /// Signals sent to the process as a whole (bit n-1 = signal n).
    pub pending: u64,
    /// Who sent each pending signal (pid), for siginfo.
    pub sender: [u32; NSIG as usize + 1],
}

impl Signals {
    pub const fn new() -> Self {
        Self { actions: [Action { handler: 0, flags: 0, restorer: 0, mask: 0 }; NSIG as usize + 1], pending: 0, sender: [0; NSIG as usize + 1] }
    }

    /// For execve: handled signals revert to their default, ignored ones stay ignored.
    pub fn reset_handlers(&mut self) {
        for a in self.actions.iter_mut() {
            if a.handler != SIG_IGN {
                *a = Action::default();
            }
        }
    }
}

pub fn bit(sig: u32) -> u64 {
    1u64 << (sig - 1)
}

/// What happens to a signal nobody handles.
pub(crate) enum Default {
    Terminate,
    Ignore,
}

pub(crate) fn default_action(sig: u32) -> Default {
    match sig {
        SIGCHLD | SIGURG | SIGWINCH | SIGCONT => Default::Ignore,
        // Job control: Nova has no stopped state yet, so stopping is ignored.
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => Default::Ignore,
        _ => Default::Terminate,
    }
}

/// Make signal `sig` pending for process `p` (from `sender`). Wakes its threads.
pub fn send_process(p: &process::Process, sig: u32, sender: u64) {
    if sig == 0 || sig > NSIG {
        return;
    }
    if sig == SIGKILL {
        process::terminate(p, SIGKILL);
        return;
    }
    {
        let mut s = p.signals.lock();
        // A signal that is ignored at the time it is sent is discarded.
        let a = s.actions[sig as usize];
        if a.handler == SIG_IGN || (a.handler == SIG_DFL && matches!(default_action(sig), Default::Ignore)) {
            return;
        }
        s.pending |= bit(sig);
        s.sender[sig as usize] = sender as u32;
    }
    crate::linux::futex_wake_all(p);
    crate::ipc::notify();
}

/// Make `sig` pending for the calling thread itself (raise / tgkill to self).
pub fn send_self(u: &mut UserState, sig: u32) {
    if sig == SIGKILL {
        process::terminate(&u.process, SIGKILL);
        return;
    }
    if sig != 0 && sig <= NSIG {
        u.pending |= bit(sig);
    }
}

/// A deliverable (pending and not blocked) signal for `u`, if any, without taking it.
pub fn has_deliverable(u: &UserState) -> bool {
    let process = u.process.signals.lock().pending;
    (process | u.pending) & !u.sigmask != 0
}

/// Take the lowest deliverable signal: (signal, sender pid, sent to the thread itself).
pub(crate) fn take(u: &mut UserState) -> Option<(u32, u32, bool)> {
    let own = u.pending & !u.sigmask;
    if own != 0 {
        let sig = own.trailing_zeros() + 1;
        u.pending &= !bit(sig);
        return Some((sig, u.process.pid as u32, true));
    }
    let mut s = u.process.signals.lock();
    let shared = s.pending & !u.sigmask;
    if shared == 0 {
        return None;
    }
    let sig = shared.trailing_zeros() + 1;
    s.pending &= !bit(sig);
    Some((sig, s.sender[sig as usize], false))
}

// rt_sigframe layout (x86-64): pretcode, ucontext, siginfo; the FPU state follows.
const UC: u64 = 8;
const UC_STACK: u64 = UC + 16;
const UC_MCONTEXT: u64 = UC + 40;
const UC_SIGMASK: u64 = UC_MCONTEXT + 256;
const INFO: u64 = UC_SIGMASK + 8;
const FRAME_SIZE: u64 = INFO + 128;
// sigcontext: r8..r15, rdi, rsi, rbp, rbx, rdx, rax, rcx, rsp, rip, eflags, then
// cs/gs/fs/ss, err, trapno, oldmask, cr2, fpstate, reserved.
const SC_RSP: u64 = 15 * 8;
const SC_RIP: u64 = 16 * 8;
const SC_EFLAGS: u64 = 17 * 8;
const SC_SEGS: u64 = 18 * 8;
const SC_OLDMASK: u64 = 21 * 8;
const SC_FPSTATE: u64 = 23 * 8;
const UC_FP_XSTATE: u64 = 1;
const FP_XSTATE_MAGIC1: u32 = 0x4650_5853;
const FP_XSTATE_MAGIC2: u32 = 0x4650_5845;

fn sigcontext_regs(f: &SyscallFrame) -> [u64; 18] {
    // The syscall clobbered rcx (return address) and r11 (flags); sigcontext holds what
    // the program will see after sigreturn: rcx = rip, r11 = rflags, as on Linux.
    [f.r8, f.r9, f.r10, f.rflags, f.r12, f.r13, f.r14, f.r15, f.rdi, f.rsi, f.rbp, f.rbx, f.rdx, f.rax, f.rip, f.rsp, f.rip, f.rflags]
}

/// Deliver pending signals before returning to user mode from a system call: default
/// actions run here, a handler gets a signal frame (and the frame `f` is redirected to
/// it). Called with the frame of the syscall that is returning.
pub fn deliver(f: &mut SyscallFrame) {
    let Some(u) = crate::task::current_user() else { return };
    while let Some((sig, sender, to_thread)) = take(u) {
        let action = u.process.signals.lock().actions[sig as usize];
        match action.handler {
            SIG_IGN => continue,
            SIG_DFL => match default_action(sig) {
                Default::Ignore => continue,
                Default::Terminate => {
                    process::terminate(&u.process, sig);
                    process::check_exiting();
                    return;
                }
            },
            _ => {
                if !push_frame(u, f, sig, sender, to_thread, &action) {
                    // The stack is unusable: like Linux, force SIGSEGV's default action.
                    process::terminate(&u.process, SIGSEGV);
                    process::check_exiting();
                }
                return; // one handler at a time; others are delivered after it
            }
        }
    }
}

fn push_frame(u: &mut UserState, f: &mut SyscallFrame, sig: u32, sender: u32, to_thread: bool, action: &Action) -> bool {
    let xsave_size = process::xsave_size();
    let fp_size = if xsave_size != 0 { xsave_size + 4 } else { 512 };
    // On the alternate stack if asked for and not already on it; skip the red zone.
    let on_alt = u.altstack.2 & SS_DISABLE == 0 && u.altstack.1 != 0 && !(f.rsp >= u.altstack.0 && f.rsp < u.altstack.0 + u.altstack.1);
    let mut sp = if action.flags & SA_ONSTACK != 0 && on_alt { u.altstack.0 + u.altstack.1 } else { f.rsp - 128 };
    sp = (sp - fp_size) & !63;
    let fpstate = sp;
    sp -= FRAME_SIZE;
    // At the handler's first instruction, rsp + 8 is 16-byte aligned (a call just happened).
    sp = ((sp + 8) & !15) - 8;
    let frame = sp;
    let total = fpstate + fp_size - frame;
    let old_mask = u.saved_mask.take().unwrap_or(u.sigmask);
    if !u.process.mem.lock().ensure(frame, total, true) {
        return false;
    }
    // SAFETY: [frame, frame + total) is mapped and writable in the current address space
    // (checked just above); this thread is in kernel mode on its own kernel stack.
    unsafe {
        core::ptr::write_bytes(frame as *mut u8, 0, (fpstate - frame) as usize);
        let w64 = |off: u64, v: u64| ((frame + off) as *mut u64).write_unaligned(v);
        let w32 = |off: u64, v: u32| ((frame + off) as *mut u32).write_unaligned(v);
        w64(0, action.restorer);
        w64(UC, if xsave_size != 0 { UC_FP_XSTATE } else { 0 });
        w64(UC_STACK, u.altstack.0);
        w32(UC_STACK + 8, if on_alt { u.altstack.2 } else { SS_DISABLE });
        w64(UC_STACK + 16, u.altstack.1);
        for (i, r) in sigcontext_regs(f).iter().enumerate() {
            w64(UC_MCONTEXT + i as u64 * 8, *r);
        }
        // cs, gs, fs, ss as user mode sees them.
        w64(UC_MCONTEXT + SC_SEGS, 0x23 << 48 | 0x2b);
        w64(UC_MCONTEXT + SC_OLDMASK, old_mask);
        w64(UC_MCONTEXT + SC_FPSTATE, fpstate);
        w64(UC_SIGMASK, old_mask);
        w32(INFO, sig);
        w32(INFO + 8, if to_thread { SI_TKILL as u32 } else { SI_USER as u32 });
        w32(INFO + 16, sender);
        // The live SIMD state belongs to this thread (the kernel does not use it).
        process::save_fpu(fpstate as *mut u8);
        if xsave_size != 0 {
            // _fpx_sw_bytes in the legacy area's software-reserved bytes, then MAGIC2.
            let sw = fpstate + 464;
            (sw as *mut u32).write_unaligned(FP_XSTATE_MAGIC1);
            ((sw + 4) as *mut u32).write_unaligned(fp_size as u32);
            ((sw + 8) as *mut u64).write_unaligned(process::xcr0());
            ((sw + 16) as *mut u32).write_unaligned(xsave_size as u32);
            ((fpstate + xsave_size) as *mut u32).write_unaligned(FP_XSTATE_MAGIC2);
        }
    }
    u.sigmask |= action.mask | if action.flags & SA_NODEFER == 0 { bit(sig) } else { 0 };
    u.sigmask &= !(bit(SIGKILL) | bit(SIGSTOP));
    if action.flags & SA_RESETHAND != 0 {
        u.process.signals.lock().actions[sig as usize] = Action::default();
    }
    f.rsp = frame;
    f.rip = action.handler;
    f.rdi = sig as u64;
    f.rsi = frame + INFO;
    f.rdx = frame + UC;
    f.rax = 0;
    f.rflags &= !(1 << 10); // DF clear on entry, as the ABI requires
    let _ = SA_SIGINFO | SA_RESTORER; // glibc always sets both; the frame works either way
    true
}

/// rt_sigreturn: restore what [`push_frame`] saved. Returns the restored rax.
pub fn sigreturn(f: &mut SyscallFrame) -> i64 {
    let Some(u) = crate::task::current_user() else { return 0 };
    let frame = f.rsp - 8; // the handler's `ret` popped pretcode
    if !u.process.mem.lock().ensure(frame, FRAME_SIZE, false) {
        process::terminate(&u.process, SIGSEGV);
        process::check_exiting();
        return 0;
    }
    // SAFETY: the frame range is mapped (checked); values are only loaded into registers.
    let (regs, mask, fpstate, uc_flags) = unsafe {
        let r64 = |off: u64| ((frame + off) as *const u64).read_unaligned();
        let mut regs = [0u64; 18];
        for (i, r) in regs.iter_mut().enumerate() {
            *r = r64(UC_MCONTEXT + i as u64 * 8);
        }
        (regs, r64(UC_SIGMASK), r64(UC_MCONTEXT + SC_FPSTATE), r64(UC))
    };
    let [r8, r9, r10, _r11, r12, r13, r14, r15, rdi, rsi, rbp, rbx, rdx, rax, _rcx, rsp, rip, eflags] = regs;
    *f = SyscallFrame { r15, r14, r13, r12, rbp, rbx, r9, r8, r10, rdx, rsi, rdi, rax, rip, rsp, rflags: (eflags & 0x0CD5) | 0x202 };
    u.sigmask = mask & !(bit(SIGKILL) | bit(SIGSTOP));
    if fpstate != 0 {
        let size = if uc_flags & UC_FP_XSTATE != 0 { process::xsave_size() } else { 512 };
        if u.process.mem.lock().ensure(fpstate, size.max(512), false) {
            // SAFETY: mapped (checked); the area was written by push_frame or the program.
            unsafe { process::restore_fpu(fpstate as *const u8, uc_flags & UC_FP_XSTATE != 0) };
        }
    }
    let _ = SS_ONSTACK;
    rax as i64
}
