//! seccomp: strict mode and classic-BPF system call filters (prctl(PR_SET_SECCOMP) and
//! seccomp(2)), as Chromium's sandbox and other hardened programs use them.
//!
//! Filters stack per thread (newest first, each pointing at the one below it), are
//! inherited by fork and clone and kept across execve. Every system call of a thread
//! that has one runs all of them on a `seccomp_data` (call number, audit architecture,
//! instruction pointer, arguments); the most severe result wins. Threads without filters
//! pay one test of `State::mode` in `linux::syscall` / `compat32::syscall32`.
//!
//! The programs are checked when loaded as Linux checks them (only the instructions
//! seccomp allows, forward jumps that stay inside, scratch memory written before it is
//! read, a RET at the end), so the interpreter below needs no run-time bounds checks
//! beyond what Rust does anyway.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::{read_u32, read_u64, read_user, thread, EACCES, EINVAL, ENOSYS};
use crate::process::{self, UserState};
use crate::signal;

const ENOMEM: i64 = 12;
const ESRCH: i64 = 3;
const EOPNOTSUPP: i64 = 95;

pub const MODE_DISABLED: u8 = 0;
pub const MODE_STRICT: u8 = 1;
pub const MODE_FILTER: u8 = 2;

const SET_MODE_STRICT: u64 = 0;
const SET_MODE_FILTER: u64 = 1;
const GET_ACTION_AVAIL: u64 = 2;

const FLAG_TSYNC: u64 = 1;
const FLAG_LOG: u64 = 2;
const FLAG_SPEC_ALLOW: u64 = 4;
const FLAG_TSYNC_ESRCH: u64 = 16;

pub const RET_KILL_PROCESS: u32 = 0x8000_0000;
pub const RET_KILL_THREAD: u32 = 0;
pub const RET_TRAP: u32 = 0x0003_0000;
pub const RET_ERRNO: u32 = 0x0005_0000;
pub const RET_USER_NOTIF: u32 = 0x7fc0_0000;
pub const RET_TRACE: u32 = 0x7ff0_0000;
pub const RET_LOG: u32 = 0x7ffc_0000;
pub const RET_ALLOW: u32 = 0x7fff_0000;
const ACTION_FULL: u32 = 0xffff_0000;
const DATA: u32 = 0xffff;

pub const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
pub const AUDIT_ARCH_I386: u32 = 0x4000_0003;

/// SIGSYS's si_code for a seccomp trap.
pub const SYS_SECCOMP: u32 = 1;

const MAX_INSNS: usize = 4096;
/// Linux's limit on all filters of a thread together (each costs 4 more instructions).
const MAX_INSNS_PER_PATH: usize = 32768;
/// sizeof(struct seccomp_data): nr, arch, instruction_pointer, args[6].
const DATA_SIZE: u32 = 64;
const MEM_WORDS: u32 = 16;

/// One classic BPF instruction (struct sock_filter).
#[derive(Clone, Copy)]
struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

pub struct Filter {
    prog: Vec<Insn>,
    /// The filter installed before this one.
    prev: Option<Arc<Filter>>,
}

impl Filter {
    fn chain(self: &Arc<Self>) -> impl Iterator<Item = &Filter> {
        core::iter::successors(Some(&**self), |f| f.prev.as_deref())
    }
}

/// A thread's seccomp state; cloned into children and new threads.
#[derive(Clone, Default)]
pub struct State {
    pub mode: u8,
    pub filter: Option<Arc<Filter>>,
}

impl State {
    pub fn filters(&self) -> usize {
        self.filter.as_ref().map_or(0, |f| f.chain().count())
    }
}

/// What a trapped call puts in SIGSYS's siginfo.
#[derive(Clone, Copy)]
pub struct Sigsys {
    pub call_addr: u64,
    pub syscall: u32,
    pub arch: u32,
    pub errno: u32,
}

// Opcodes seccomp accepts (linux/filter.h names).
const LD_W_ABS: u16 = 0x20;
const LD_W_LEN: u16 = 0x80;
const LDX_W_LEN: u16 = 0x81;
const LD_IMM: u16 = 0x00;
const LDX_IMM: u16 = 0x01;
const LD_MEM: u16 = 0x60;
const LDX_MEM: u16 = 0x61;
const ST: u16 = 0x02;
const STX: u16 = 0x03;
const RET_K: u16 = 0x06;
const RET_A: u16 = 0x16;
const TAX: u16 = 0x07;
const TXA: u16 = 0x87;
const JA: u16 = 0x05;
const ALU_NEG: u16 = 0x84;

const CLASS_ALU: u16 = 0x04;
const CLASS_JMP: u16 = 0x05;
const SRC_X: u16 = 0x08;
const OP_DIV: u16 = 0x30;
const OP_LSH: u16 = 0x60;
const OP_RSH: u16 = 0x70;
const OP_MOD: u16 = 0x90;

/// Load-time checks (Linux's bpf_check_classic, seccomp_check_filter and
/// check_load_and_stores).
fn validate(prog: &[Insn]) -> Result<(), i64> {
    let len = prog.len();
    if len == 0 || len > MAX_INSNS {
        return Err(EINVAL);
    }
    let target_ok = |pc: usize, off: u64| (pc as u64 + 1 + off) < len as u64;
    for (pc, i) in prog.iter().enumerate() {
        let ok = match i.code {
            LD_W_ABS => i.k < DATA_SIZE && i.k % 4 == 0,
            LD_W_LEN | LDX_W_LEN | LD_IMM | LDX_IMM | RET_K | RET_A | TAX | TXA | ALU_NEG => true,
            LD_MEM | LDX_MEM | ST | STX => i.k < MEM_WORDS,
            JA => target_ok(pc, i.k as u64),
            c if c & 7 == CLASS_ALU => {
                let op = c & 0xf0;
                // ADD SUB MUL DIV OR AND LSH RSH (0x00-0x70), MOD, XOR.
                (op <= 0x70 || op == OP_MOD || op == 0xa0)
                    && c & !(0xf0 | SRC_X | 7) == 0
                    && (c & SRC_X != 0 || !(matches!(op, OP_DIV | OP_MOD) && i.k == 0) && !(matches!(op, OP_LSH | OP_RSH) && i.k >= 32))
            }
            c if c & 7 == CLASS_JMP => {
                // JEQ JGT JGE JSET, against K or X.
                matches!(c & !SRC_X, 0x15 | 0x25 | 0x35 | 0x45) && target_ok(pc, i.jt as u64) && target_ok(pc, i.jf as u64)
            }
            _ => false,
        };
        if !ok {
            return Err(EINVAL);
        }
    }
    if !matches!(prog[len - 1].code, RET_K | RET_A) {
        return Err(EINVAL);
    }
    // Scratch memory must be written on every path before it is read.
    let mut masks = alloc::vec![u16::MAX; len];
    let mut valid: u16 = 0;
    for (pc, i) in prog.iter().enumerate() {
        valid &= masks[pc];
        match i.code {
            ST | STX => valid |= 1 << i.k,
            LD_MEM | LDX_MEM if valid & (1 << i.k) == 0 => return Err(EINVAL),
            JA => {
                masks[pc + 1 + i.k as usize] &= valid;
                valid = u16::MAX;
            }
            c if c & 7 == CLASS_JMP => {
                masks[pc + 1 + i.jt as usize] &= valid;
                masks[pc + 1 + i.jf as usize] &= valid;
                valid = u16::MAX;
            }
            RET_K | RET_A => valid = u16::MAX,
            _ => {}
        }
    }
    Ok(())
}

/// Run one validated program on `data` (struct seccomp_data as 32-bit words).
fn run(prog: &[Insn], data: &[u32; 16]) -> u32 {
    let (mut a, mut x) = (0u32, 0u32);
    let mut m = [0u32; MEM_WORDS as usize];
    let mut pc = 0;
    loop {
        let i = prog[pc];
        pc += 1;
        match i.code {
            LD_W_ABS => a = data[(i.k / 4) as usize],
            LD_W_LEN => a = DATA_SIZE,
            LDX_W_LEN => x = DATA_SIZE,
            LD_IMM => a = i.k,
            LDX_IMM => x = i.k,
            LD_MEM => a = m[i.k as usize],
            LDX_MEM => x = m[i.k as usize],
            ST => m[i.k as usize] = a,
            STX => m[i.k as usize] = x,
            RET_K => return i.k,
            RET_A => return a,
            TAX => x = a,
            TXA => a = x,
            ALU_NEG => a = a.wrapping_neg(),
            JA => pc += i.k as usize,
            c if c & 7 == CLASS_ALU => {
                let v = if c & SRC_X != 0 { x } else { i.k };
                a = match c & 0xf0 {
                    0x00 => a.wrapping_add(v),
                    0x10 => a.wrapping_sub(v),
                    0x20 => a.wrapping_mul(v),
                    OP_DIV | OP_MOD if v == 0 => return 0, // as Linux: the program returns 0
                    OP_DIV => a / v,
                    OP_MOD => a % v,
                    0x40 => a | v,
                    0x50 => a & v,
                    OP_LSH => a.wrapping_shl(v),
                    OP_RSH => a.wrapping_shr(v),
                    _ => a ^ v,
                };
            }
            c => {
                let v = if c & SRC_X != 0 { x } else { i.k };
                let taken = match c & 0xf0 {
                    0x10 => a == v,
                    0x20 => a > v,
                    0x30 => a >= v,
                    _ => a & v != 0,
                };
                pc += if taken { i.jt } else { i.jf } as usize;
            }
        }
    }
}

/// Lower is more severe: KILL_PROCESS (negative as s32) first, ALLOW last.
fn severity(ret: u32) -> i32 {
    (ret & ACTION_FULL) as i32
}

/// Called on syscall entry for a thread with seccomp on. None: run the call; Some(r):
/// skip it and return r (for a trap, the call number, as Linux rolls rax back).
pub fn check(ip: u64, nr: u64, args: [u64; 6], compat: bool) -> Option<i64> {
    let u = thread();
    if u.seccomp.mode == MODE_STRICT {
        let allowed: &[u64] = if compat { &[3, 4, 1, 119] } else { &[0, 1, 60, 15] };
        if !allowed.contains(&nr) {
            process::terminate(&u.process, signal::SIGKILL);
            process::check_exiting();
        }
        return None;
    }
    let arch = if compat { AUDIT_ARCH_I386 } else { AUDIT_ARCH_X86_64 };
    let mut data = [0u32; 16];
    data[0] = nr as u32;
    data[1] = arch;
    data[2] = ip as u32;
    data[3] = (ip >> 32) as u32;
    for (n, a) in args.iter().enumerate() {
        data[4 + 2 * n] = *a as u32;
        data[5 + 2 * n] = (*a >> 32) as u32;
    }
    let mut ret = RET_ALLOW;
    for f in u.seccomp.filter.as_ref()?.chain() {
        let r = run(&f.prog, &data);
        if severity(r) < severity(ret) {
            ret = r;
        }
    }
    let data_bits = ret & DATA;
    match ret & ACTION_FULL {
        RET_ALLOW | RET_LOG => None,
        RET_ERRNO => Some(-(data_bits.min(4095) as i64)),
        // No tracer and no listener: the call fails as an unknown one would.
        RET_TRACE | RET_USER_NOTIF => Some(-ENOSYS),
        RET_TRAP => {
            // Forced like Linux's force_sig_seccomp: unblocked, and an ignored SIGSYS
            // reverts to its default action (which kills).
            u.sigsys = Some(Sigsys { call_addr: ip, syscall: nr as u32, arch, errno: data_bits });
            u.sigmask &= !signal::bit(signal::SIGSYS);
            {
                let mut s = u.process.signals.lock();
                if s.actions[signal::SIGSYS as usize].handler == 1 {
                    s.actions[signal::SIGSYS as usize] = signal::Action::default();
                }
            }
            u.pending |= signal::bit(signal::SIGSYS);
            Some(nr as i64)
        }
        RET_KILL_THREAD if u.process.threads.load(core::sync::atomic::Ordering::SeqCst) > 1 => process::exit_thread(128 + signal::SIGSYS as i32),
        // KILL_PROCESS, the last thread's KILL_THREAD, and unknown actions.
        _ => {
            process::terminate(&u.process, signal::SIGSYS);
            process::check_exiting();
            Some(-ENOSYS)
        }
    }
}

/// seccomp(2) (x86-64 317, i386 354).
pub fn seccomp(op: u64, flags: u64, uargs: u64) -> Result<i64, i64> {
    match op {
        SET_MODE_STRICT if flags == 0 && uargs == 0 => set_strict(),
        SET_MODE_FILTER => set_filter(flags, uargs),
        GET_ACTION_AVAIL if flags == 0 => match read_u32(uargs)? {
            RET_KILL_PROCESS | RET_KILL_THREAD | RET_TRAP | RET_ERRNO | RET_TRACE | RET_LOG | RET_ALLOW => Ok(0),
            _ => Err(EOPNOTSUPP),
        },
        _ => Err(EINVAL),
    }
}

/// prctl(PR_SET_SECCOMP, mode, filter).
pub fn prctl_set(mode: u64, prog: u64) -> Result<i64, i64> {
    match mode {
        1 => set_strict(),
        2 => set_filter(0, prog),
        _ => Err(EINVAL),
    }
}

fn set_strict() -> Result<i64, i64> {
    let u = thread();
    if u.seccomp.mode != MODE_DISABLED && u.seccomp.mode != MODE_STRICT {
        return Err(EINVAL);
    }
    u.seccomp.mode = MODE_STRICT;
    Ok(0)
}

/// CAP_SYS_ADMIN in the caller's user namespace (Nova's capget rule).
fn sys_admin() -> bool {
    let c = thread().process.cred.lock();
    let full = c.euid == 0 || c.userns.is_some();
    c.caps.map_or(full, |caps| caps[0] & (1 << 21) != 0)
}

fn set_filter(flags: u64, uprog: u64) -> Result<i64, i64> {
    if flags & !(FLAG_TSYNC | FLAG_LOG | FLAG_SPEC_ALLOW | FLAG_TSYNC_ESRCH) != 0 {
        return Err(EINVAL); // NEW_LISTENER among them: no user-space notification yet
    }
    // struct sock_fprog: u16 len, then the pointer (at 8, or at 4 for i386).
    let compat = thread().compat;
    let len = read_user(uprog, 2).map(|b| u16::from_le_bytes([b[0], b[1]]))? as usize;
    let ptr = if compat { read_u32(uprog + 4)? as u64 } else { read_u64(uprog + 8)? };
    if len == 0 || len > MAX_INSNS {
        return Err(EINVAL);
    }
    let raw = read_user(ptr, len * 8)?;
    let prog: Vec<Insn> = raw
        .chunks_exact(8)
        .map(|c| Insn { code: u16::from_le_bytes([c[0], c[1]]), jt: c[2], jf: c[3], k: u32::from_le_bytes([c[4], c[5], c[6], c[7]]) })
        .collect();
    // Unprivileged filters need no_new_privs: a filter must not confuse a setuid program.
    if !thread().no_new_privs && !sys_admin() {
        return Err(EACCES);
    }
    validate(&prog)?;
    let u = thread();
    if u.seccomp.mode == MODE_STRICT {
        return Err(EINVAL);
    }
    let path: usize = u.seccomp.filter.as_ref().map_or(0, |f| f.chain().map(|f| f.prog.len() + 4).sum());
    if path + prog.len() + 4 > MAX_INSNS_PER_PATH {
        return Err(ENOMEM);
    }
    let (pid, tid) = (u.process.pid, u.tid);
    if flags & FLAG_TSYNC != 0 {
        // Every other thread must have no filter, or one this thread's stack extends.
        let mine = u.seccomp.filter.clone();
        let is_ancestor = |f: &Option<Arc<Filter>>| match (f, &mine) {
            (None, _) => true,
            (Some(f), Some(m)) => m.chain().any(|a| core::ptr::eq(a, &**f)),
            (Some(_), None) => false,
        };
        let mut blocker = 0;
        crate::task::each_thread(pid, |t: &mut UserState| {
            if blocker == 0 && t.tid != tid && (t.seccomp.mode == MODE_STRICT || !is_ancestor(&t.seccomp.filter)) {
                blocker = t.tid;
            }
        });
        if blocker != 0 {
            return if flags & FLAG_TSYNC_ESRCH != 0 { Err(ESRCH) } else { Ok(blocker as i64) };
        }
    }
    let u = thread();
    let filter = Arc::new(Filter { prog, prev: u.seccomp.filter.take() });
    u.seccomp = State { mode: MODE_FILTER, filter: Some(filter.clone()) };
    if flags & FLAG_TSYNC != 0 {
        let nnp = u.no_new_privs;
        crate::task::each_thread(pid, |t: &mut UserState| {
            t.seccomp = State { mode: MODE_FILTER, filter: Some(filter.clone()) };
            t.no_new_privs |= nnp;
        });
    }
    Ok(0)
}

/// The /proc/<pid>/status lines.
pub fn status_lines(nnp: bool, s: &State) -> alloc::string::String {
    alloc::format!("NoNewPrivs:\t{}\nSeccomp:\t{}\nSeccomp_filters:\t{}\n", nnp as u8, s.mode, s.filters())
}
