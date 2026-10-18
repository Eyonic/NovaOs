//! 32-bit (i386) Linux programs: the `int 0x80` system call interface.
//!
//! A 32-bit program runs in compatibility mode (process.rs, gdt::USER_CODE32) and enters
//! the kernel with `int 0x80`: the call number in eax, arguments in ebx, ecx, edx, esi,
//! edi, ebp, the result back in eax, every other register preserved. Calls that the Linux
//! kernel serves with the same function for both ABIs go straight to Nova's 64-bit
//! implementation (compat32_table.rs, generated from Linux's syscall tables); the others
//! convert their structures here (32-bit pointers, `long`s, time_t, stat64, iovec, ...),
//! as Linux's `compat_sys_*` functions do. glibc's i386 port uses the int 0x80 path when
//! the kernel offers no vDSO: Nova has one for 64-bit programs only (src/vdso.rs).

use alloc::string::String;
use alloc::vec::Vec;

use super::*;
use crate::process::{self, SyscallFrame};

#[path = "compat32_table.rs"]
mod table;

/// Registers as the `int 0x80` stub saves them (lowest address first), then the frame the
/// CPU pushed.
#[repr(C)]
pub struct Frame32 {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

/// The `int 0x80` gate's handler (interrupts.rs installs it with DPL 3).
#[unsafe(naked)]
pub unsafe extern "C" fn int80_entry() {
    core::arch::naked_asm!(
        "push rax", "push rbx", "push rcx", "push rdx", "push rsi", "push rdi", "push rbp",
        "push r8", "push r9", "push r10", "push r11", "push r12", "push r13", "push r14", "push r15",
        "cld",
        "mov rdi, rsp",
        "call {dispatch}",
        "cli",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10", "pop r9", "pop r8",
        "pop rbp", "pop rdi", "pop rsi", "pop rdx", "pop rcx", "pop rbx", "pop rax",
        "iretq",
        dispatch = sym int80_dispatch,
    )
}

/// A 32-bit frame in the layout the rest of the kernel uses (clone, execve, fork): the
/// i386 registers in the same-named slots, ecx in the r10 slot (process.rs's 32-bit entry
/// paths load it from there).
pub fn as_syscall_frame(f: &Frame32) -> SyscallFrame {
    SyscallFrame { r15: 0, r14: 0, r13: 0, r12: 0, rbp: f.rbp, rbx: f.rbx, r9: 0, r8: 0, r10: f.rcx, rdx: f.rdx, rsi: f.rsi, rdi: f.rdi, rax: f.rax, rip: f.rip, rflags: f.rflags, rsp: f.rsp }
}

extern "C" fn int80_dispatch(f: &mut Frame32) {
    // SAFETY: re-enable interrupts: system calls may block and let other tasks run.
    unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
    crate::smp::lock();
    process::check_exiting();
    let r = syscall32(f);
    f.rax = r as i32 as u32 as u64;
    process::check_exiting();
    deliver_signals(f);
    process::leave_kernel();
}

fn a(f: &Frame32) -> [u64; 6] {
    [f.rbx & 0xFFFF_FFFF, f.rcx & 0xFFFF_FFFF, f.rdx & 0xFFFF_FFFF, f.rsi & 0xFFFF_FFFF, f.rdi & 0xFFFF_FFFF, f.rbp & 0xFFFF_FFFF]
}

fn sx(v: u64) -> i64 {
    v as u32 as i32 as i64
}

/// The i386 system call `f.rax`.
pub fn syscall32(f: &mut Frame32) -> i64 {
    let nr = f.rax & 0xFFFF_FFFF;
    let [a0, a1, a2, a3, a4, a5] = a(f);
    let trace = proc().trace.load(core::sync::atomic::Ordering::Relaxed);
    if thread().seccomp.mode != seccomp::MODE_DISABLED
        && let Some(r) = seccomp::check(f.rip, nr, [a0, a1, a2, a3, a4, a5], true)
    {
        thread().recent.push(0x1000 | nr, [a0, a1, a2], r);
        return r;
    }
    thread().in_syscall = 0x1000 | nr; // i386 numbers shown as 0x1000 + nr
    thread().syscall_arg = a0;
    thread().syscall_arg2 = a1;
    thread().phase = "convert";
    let r = res(convert(f, nr, a0, a1, a2, a3, a4, a5));
    thread().ns_override = None;
    thread().recent.push(0x1000 | nr, [a0, a1, a2], r);
    thread().phase = "returned";
    thread().in_syscall = u64::MAX;
    if trace {
        crate::log::info(&alloc::format!("[{}] i386 {}({a0:#x}, {a1:#x}, {a2:#x}, {a3:#x}, {a4:#x}) = {r}", thread().tid, name32(nr)));
    }
    r
}

pub fn label(nr: u64) -> String {
    name32(nr)
}

fn name32(nr: u64) -> String {
    table::DIRECT.iter().find(|e| e.0 as u64 == nr).map_or_else(|| alloc::format!("syscall32_{nr}"), |e| e.2.into())
}

/// Call the 64-bit implementation `nr64` with these arguments.
fn native(f: &Frame32, nr64: u64, args: [u64; 6]) -> i64 {
    let mut sf = as_syscall_frame(f);
    dispatch(&mut sf, nr64, args[0], args[1], args[2], args[3], args[4], args[5])
}

fn rd32(addr: u64) -> Result<u32, i64> {
    read_u32(addr)
}

fn wr32(addr: u64, v: u32) -> Result<(), i64> {
    write_user(addr, &v.to_le_bytes())
}

/// Size of each 32-bit thread's scratch mapping.
const SCRATCH_SIZE: u64 = super::COMPAT_SCRATCH_SIZE;

/// A user buffer in Nova's own (64-bit) layout for a call whose result is converted: the
/// 64-bit implementation writes here. It is a private mapping per thread (below 4 GiB is
/// not needed: only the kernel touches it), never the program's stack, which may be small.
/// Every call uses the end of it, so one call's buffers must come from one scratch().
fn scratch(_f: &Frame32, len: u64) -> Result<u64, i64> {
    if len > SCRATCH_SIZE {
        return Err(EINVAL);
    }
    let t = thread();
    if t.compat_scratch == 0 {
        t.compat_scratch = mem().reserve_kernel_only(SCRATCH_SIZE).ok_or(super::ENOMEM_ERR)?;
    }
    let addr = (t.compat_scratch + SCRATCH_SIZE - len) & !15;
    if !mem().ensure(addr, len, true) {
        return Err(EFAULT);
    }
    Ok(addr)
}

/// x86-64 `struct stat` (144 bytes) to i386 `struct stat64` (96 bytes).
fn stat64(s: &[u8]) -> Vec<u8> {
    let q = |o: usize| u64::from_le_bytes(s[o..o + 8].try_into().unwrap());
    let d = |o: usize| u32::from_le_bytes(s[o..o + 4].try_into().unwrap());
    let mut b = Vec::with_capacity(96);
    b.extend_from_slice(&q(0).to_le_bytes()); // st_dev
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&(q(8) as u32).to_le_bytes()); // __st_ino
    b.extend_from_slice(&d(24).to_le_bytes()); // st_mode
    b.extend_from_slice(&(q(16) as u32).to_le_bytes()); // st_nlink
    b.extend_from_slice(&d(28).to_le_bytes()); // st_uid
    b.extend_from_slice(&d(32).to_le_bytes()); // st_gid
    b.extend_from_slice(&q(40).to_le_bytes()); // st_rdev
    b.extend_from_slice(&[0; 4]);
    b.extend_from_slice(&q(48).to_le_bytes()); // st_size
    b.extend_from_slice(&(q(56) as u32).to_le_bytes()); // st_blksize
    b.extend_from_slice(&q(64).to_le_bytes()); // st_blocks
    for o in [72, 80, 88, 96, 104, 112] {
        b.extend_from_slice(&(q(o) as u32).to_le_bytes()); // atime, mtime, ctime (+ ns)
    }
    b.extend_from_slice(&q(8).to_le_bytes()); // st_ino
    b
}

/// Read a NULL-terminated array of 32-bit string pointers.
fn strings32(mut p: u64) -> Result<Vec<String>, i64> {
    let mut v = Vec::new();
    while p != 0 && v.len() < 4096 {
        let s = rd32(p)?;
        if s == 0 {
            break;
        }
        v.push(read_cstr(s as u64)?);
        p += 4;
    }
    Ok(v)
}

/// Copy a 32-bit iovec array into a 64-bit one in scratch space.
fn iovec64(f: &Frame32, iov: u64, count: u64) -> Result<u64, i64> {
    if count > 1024 {
        return Err(EINVAL);
    }
    let raw = read_user(iov, count as usize * 8)?;
    let out = scratch(f, count * 16)?;
    let mut b = Vec::with_capacity(count as usize * 16);
    for c in raw.chunks_exact(8) {
        b.extend_from_slice(&(u32::from_le_bytes(c[0..4].try_into().unwrap()) as u64).to_le_bytes());
        b.extend_from_slice(&(u32::from_le_bytes(c[4..8].try_into().unwrap()) as u64).to_le_bytes());
    }
    write_user(out, &b)?;
    Ok(out)
}

/// 32-bit timespec/timeval (two 32-bit fields) to 64-bit in scratch space; 0 stays 0.
fn time64_in(f: &Frame32, t: u64, slot: u64) -> Result<u64, i64> {
    if t == 0 {
        return Ok(0);
    }
    let (s, ns) = (rd32(t)? as i32 as i64, rd32(t + 4)? as i32 as i64);
    let out = scratch(f, 64)? + slot * 16;
    write_user(out, &[s.to_le_bytes(), ns.to_le_bytes()].concat())?;
    Ok(out)
}

/// A 64-bit timespec/timeval in scratch space back to 32 bits at `t`.
fn time32_out(from: u64, t: u64) -> Result<(), i64> {
    if t != 0 {
        let s = read_u64(from)? as u32;
        let ns = read_u64(from + 8)? as u32;
        write_user(t, &[s.to_le_bytes(), ns.to_le_bytes()].concat())?;
    }
    Ok(())
}

/// i386 user_desc (set_thread_area) to a GDT descriptor.
fn descriptor(base: u32, limit: u32, flags: u32) -> u64 {
    let (seg_32bit, contents, read_exec_only, limit_in_pages, not_present, useable) =
        (flags & 1, (flags >> 1) & 3, (flags >> 3) & 1, (flags >> 4) & 1, (flags >> 5) & 1, (flags >> 6) & 1);
    if base == 0 && limit == 0 && (flags & 0x29) == 0x28 {
        return 0; // the "empty" descriptor: read_exec_only + seg_not_present
    }
    let access: u64 = 0x10 | 0x60 | if not_present == 0 { 0x80 } else { 0 } | ((contents as u64) << 2) | if read_exec_only == 0 { 2 } else { 0 };
    let flags4: u64 = (seg_32bit as u64) << 2 | (limit_in_pages as u64) << 3 | useable as u64;
    (limit as u64 & 0xFFFF) | ((base as u64 & 0xFF_FFFF) << 16) | (access << 40) | (((limit as u64 >> 16) & 0xF) << 48) | (flags4 << 52) | (((base as u64) >> 24) << 56)
}

/// set_thread_area(struct user_desc *): install a TLS descriptor (entry -1 = pick one).
fn set_thread_area(u_info: u64) -> Result<i64, i64> {
    let b = read_user(u_info, 16)?;
    let w = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
    let (mut entry, base, limit, flags) = (w(0), w(1), w(2), w(3));
    let t = thread();
    if entry == u32::MAX {
        entry = (0..gdt::TLS_ENTRIES).find(|&i| t.tls[i] == 0).map(|i| (i + gdt::TLS_FIRST) as u32).ok_or(ESRCH)?;
        wr32(u_info, entry)?;
    }
    let index = (entry as usize).checked_sub(gdt::TLS_FIRST).filter(|&i| i < gdt::TLS_ENTRIES).ok_or(EINVAL)?;
    t.tls[index] = descriptor(base, limit, flags);
    gdt::set_tls(&t.tls);
    Ok(0)
}

#[allow(clippy::too_many_arguments)]
fn convert(f: &mut Frame32, nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> Result<i64, i64> {
    if let Some(&(_, nr64, _)) = table::DIRECT.iter().find(|e| e.0 as u64 == nr) {
        return Ok(native(f, nr64 as u64, [a0, a1, a2, a3, a4, a5]));
    }
    Ok(match nr {
        5 => native(f, 2, [a0, a1, a2, 0, 0, 0]),                 // open
        295 => native(f, 257, [sx(a0) as u64, a1, a2, a3, 0, 0]), // openat (AT_FDCWD is negative)
        7 => native(f, 61, [sx(a0) as u64, a1, a2, 0, 0, 0]),     // waitpid = wait4(pid, status, options, NULL)
        114 => {
            // wait4 with a 32-bit rusage (72 bytes)
            let r = native(f, 61, [sx(a0) as u64, a1, a2, 0, 0, 0]);
            if r >= 0 && a3 != 0 {
                write_user(a3, &[0u8; 72])?;
            }
            r
        }
        284 => native(f, 247, [a0, sx(a1) as u64, a2, a3, 0, 0]), // waitid (siginfo is the same size)
        11 => {
            // execve with 32-bit argv/envp arrays
            let path = resolve(&read_cstr(a0)?);
            let (args, env) = (strings32(a1)?, strings32(a2)?);
            let mut sf = as_syscall_frame(f);
            process::exec(&mut sf, path, args, env)?;
            0
        }
        19 => native(f, 8, [a0, sx(a1) as u64, a2, 0, 0, 0]), // lseek
        140 => {
            // _llseek(fd, offset_high, offset_low, loff_t *result, whence)
            let off = (a1 << 32) | a2;
            let r = native(f, 8, [a0, off, a4, 0, 0, 0]);
            if r < 0 {
                return Err(-r);
            }
            write_u64(a3, r as u64)?;
            0
        }
        192 => native(f, 9, [a0, a1, a2, a3, sx(a4) as u64, a5 << 12]), // mmap2: offset in pages
        195 | 196 | 197 | 300 => {
            // stat64, lstat64, fstat64, fstatat64
            let buf = scratch(f, 144)?;
            let (r, out) = match nr {
                195 => (native(f, 4, [a0, buf, 0, 0, 0, 0]), a1),
                196 => (native(f, 6, [a0, buf, 0, 0, 0, 0]), a1),
                197 => (native(f, 5, [a0, buf, 0, 0, 0, 0]), a1),
                _ => (native(f, 262, [sx(a0) as u64, a1, buf, a3, 0, 0]), a2),
            };
            if r < 0 {
                return Err(-r);
            }
            write_user(out, &stat64(&read_user(buf, 144)?))?;
            0
        }
        145 | 146 => {
            // readv, writev with 32-bit iovecs
            let iov = iovec64(f, a1, a2)?;
            native(f, if nr == 145 { 19 } else { 20 }, [a0, iov, a2, 0, 0, 0])
        }
        180 | 181 => native(f, if nr == 180 { 17 } else { 18 }, [a0, a1, a2, (a4 << 32) | a3, 0, 0]), // pread64, pwrite64
        193 => native(f, 76, [a0, (a2 << 32) | a1, 0, 0, 0, 0]), // truncate64
        194 => native(f, 77, [a0, (a2 << 32) | a1, 0, 0, 0, 0]), // ftruncate64
        54 => native(f, 16, [a0, a1, a2, 0, 0, 0]),                // ioctl (terminal ioctls have the same layout)
        55 | 221 => native(f, 72, [a0, a1, a2, 0, 0, 0]),          // fcntl, fcntl64
        174 => {
            // rt_sigaction with the 32-bit struct sigaction (handler, flags, restorer, mask)
            let sig = a0 as u32;
            if sig == 0 || sig > crate::signal::NSIG {
                return Err(EINVAL);
            }
            let old = proc().signals.lock().actions[sig as usize];
            if a2 != 0 {
                let b = [old.handler as u32, old.flags as u32, old.restorer as u32, old.mask as u32, (old.mask >> 32) as u32];
                write_user(a2, &b.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>())?;
            }
            if a1 != 0 {
                if sig == crate::signal::SIGKILL || sig == crate::signal::SIGSTOP {
                    return Err(EINVAL);
                }
                let b = read_user(a1, 20)?;
                let w = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
                let action = crate::signal::Action { handler: w(0), flags: w(1), restorer: w(2), mask: w(3) | w(4) << 32 };
                proc().signals.lock().actions[sig as usize] = action;
            }
            0
        }
        175 => native(f, 14, [a0, a1, a2, a3, 0, 0]), // rt_sigprocmask (sigset_t has the same layout)
        176 => native(f, 127, [a0, a1, 0, 0, 0, 0]),  // rt_sigpending
        179 => native(f, 130, [a0, a1, 0, 0, 0, 0]),  // rt_sigsuspend
        186 => {
            // sigaltstack with a 32-bit stack_t (ss_sp, ss_flags, ss_size)
            let t = thread();
            if a1 != 0 {
                write_user(a1, &[(t.altstack.0 as u32).to_le_bytes(), t.altstack.2.to_le_bytes(), (t.altstack.1 as u32).to_le_bytes()].concat())?;
            }
            if a0 != 0 {
                let b = read_user(a0, 12)?;
                let w = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
                t.altstack = if w(1) & 2 != 0 { (0, 0, 2) } else { (w(0) as u64, w(2) as u64, w(1) & !1) };
            }
            0
        }
        173 | 119 => sigreturn(f, nr == 173), // rt_sigreturn, sigreturn
        120 => {
            // clone(flags, stack, parent_tid, tls (struct user_desc *), child_tid)
            let (flags, stack, ptid, tls, ctid) = (a0, a1, a2, a3, a4);
            if flags & CLONE_VM != 0 && flags & CLONE_THREAD != 0 {
                let mut tls_entries = thread().tls;
                if flags & CLONE_SETTLS != 0 {
                    let b = read_user(tls, 16)?;
                    let w = |i: usize| u32::from_le_bytes(b[i * 4..i * 4 + 4].try_into().unwrap());
                    let index = (w(0) as usize).checked_sub(gdt::TLS_FIRST).filter(|&i| i < gdt::TLS_ENTRIES).ok_or(EINVAL)?;
                    tls_entries[index] = descriptor(w(1), w(2), w(3));
                }
                let sf = as_syscall_frame(f);
                let tid = process::pid_view(proc(), process::spawn_thread32(&sf, stack, tls_entries, if flags & CLONE_CHILD_CLEARTID != 0 { ctid } else { 0 }));
                if flags & CLONE_PARENT_SETTID != 0 {
                    wr32(ptid, tid as u32)?;
                }
                if flags & CLONE_CHILD_SETTID != 0 {
                    wr32(ctid, tid as u32)?;
                }
                tid as i64
            } else {
                let sf = as_syscall_frame(f);
                let pid = process::fork(&sf, flags & CLONE_VFORK != 0, stack, None, flags)?;
                if flags & super::CLONE_NEWNS != 0 {
                    process::unshare_ns(pid);
                }
                if flags & super::CLONE_NEWUSER != 0 {
                    process::unshare_user(pid);
                }
                let pid = process::pid_view(proc(), pid);
                if flags & CLONE_PARENT_SETTID != 0 {
                    wr32(ptid, pid as u32)?;
                }
                pid as i64
            }
        }
        435 => {
            // clone3(struct clone_args *, size): 64-bit fields, tls is a struct user_desc *
            // as for clone. Same as clone, with the stack given as base + size.
            let b = read_user(a0, a1.clamp(64, 88) as usize)?;
            let q = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
            let (flags, ctid, ptid, exit_signal, stack, size, tls) = (q(0), q(2), q(3), q(4), q(5), q(6), q(7));
            if flags & !0xFFFF_FFFF != 0 || exit_signal > 64 {
                return Err(EINVAL);
            }
            return convert(f, 120, flags | exit_signal, if stack != 0 { stack + size } else { 0 }, ptid, tls, ctid, 0);
        }
        2 | 190 => {
            // fork, vfork
            let sf = as_syscall_frame(f);
            process::pid_view(proc(), process::fork(&sf, nr == 190, 0, None, 0)?) as i64
        }
        243 => set_thread_area(a0)?,
        126 => {
            // sigprocmask(how, set, old) with 32-bit (old-style) signal sets
            let buf = scratch(f, 16)?;
            let set = if a1 != 0 { write_user(buf, &(rd32(a1)? as u64).to_le_bytes())?; buf } else { 0 };
            let r = native(f, 14, [a0, set, if a2 != 0 { buf + 8 } else { 0 }, 8, 0, 0]);
            if r < 0 {
                return Err(-r);
            }
            if a2 != 0 {
                wr32(a2, read_u64(buf + 8)? as u32)?;
            }
            0
        }
        117 => {
            // ipc(call, first, second, third, ptr, fifth): the SysV IPC multiplexer
            let (call, first, second, third, ptr, fifth) = (a0 & 0xFFFF, a1, a2, a3, a4, a5);
            match call {
                1 => super::sysv::semop(first as i32, ptr, second, None)?,
                2 => super::sysv::semget(first as i32, second, third)?,
                3 => super::sysv::semctl(first as i32, second, third & !0x100, if ptr == 0 { 0 } else { rd32(ptr)? as u64 }, true)?,
                4 => {
                    let d = if fifth == 0 { None } else { Some(crate::pit::ticks() + (rd32(fifth)? as u64 * 1000 + rd32(fifth + 4)? as u64 / 1_000_000).max(1)) };
                    super::sysv::semop(first as i32, ptr, second, d)?
                }
                21 => {
                    let va = super::sysv::shmat(first as i32, ptr, second)?;
                    wr32(third, va as u32)?;
                    0
                }
                22 => super::sysv::shmdt(ptr)?,
                23 => super::sysv::shmget(first as i32, second, third)?,
                24 => super::sysv::shmctl(first as i32, second & !0x100, ptr, true)?,
                _ => return Err(ENOSYS),
            }
        }
        268 | 269 | 99 | 100 => {
            // statfs64(path, size, buf), fstatfs64(fd, size, buf), statfs, fstatfs: from the
            // x86-64 struct statfs (all 8-byte fields)
            let buf = scratch(f, 128)?;
            let r = native(f, if nr == 268 || nr == 99 { 137 } else { 138 }, [a0, buf, 0, 0, 0, 0]);
            if r < 0 {
                return Err(-r);
            }
            let raw = read_user(buf, 120)?;
            let q = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
            let mut out = Vec::new();
            let wide = nr == 268 || nr == 269;
            out.extend_from_slice(&(q(0) as u32).to_le_bytes());
            out.extend_from_slice(&(q(1) as u32).to_le_bytes());
            for i in 2..7 {
                if wide { out.extend_from_slice(&q(i).to_le_bytes()) } else { out.extend_from_slice(&(q(i).min(u32::MAX as u64) as u32).to_le_bytes()) }
            }
            out.extend_from_slice(&q(7).to_le_bytes()); // fsid
            for i in 8..11 {
                out.extend_from_slice(&(q(i) as u32).to_le_bytes());
            }
            out.resize(if wide { 84 } else { 64 }, 0);
            write_user(if wide { a2 } else { a1 }, &out)?;
            0
        }
        142 | 308 => {
            // _newselect(n, r, w, e, timeval32*), pselect6(n, r, w, e, timespec32*, sigmask)
            let d = if a4 == 0 {
                None
            } else {
                let (sec, frac) = (rd32(a4)? as u64, rd32(a4 + 4)? as u64);
                let us = sec * 1_000_000 + if nr == 142 { frac } else { frac / 1000 };
                Some(crate::pit::ticks() + us.div_ceil(1_000_000 / crate::pit::HZ))
            };
            super::select_fds(a0 as usize, a1, a2, a3, d)?
        }
        320 | 271 | 30 => {
            // utimensat(dirfd, path, timespec32[2], flags), utimes(path, timeval32[2]),
            // utime(path, utimbuf32 { actime, modtime })
            let times = if nr == 320 { a2 } else { a1 };
            let t64 = if times == 0 {
                0
            } else {
                let buf = scratch(f, 32)?;
                let (a_s, a_f, m_s, m_f) = if nr == 30 {
                    (rd32(times)? as i32 as i64, 0, rd32(times + 4)? as i32 as i64, 0)
                } else {
                    (rd32(times)? as i32 as i64, rd32(times + 4)? as i32 as i64, rd32(times + 8)? as i32 as i64, rd32(times + 12)? as i32 as i64)
                };
                // utimes gives microseconds; the x86-64 utimensat call takes nanoseconds.
                let scale = if nr == 271 { 1000 } else { 1 };
                write_user(buf, &[a_s, a_f * scale, m_s, m_f * scale].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>())?;
                buf
            };
            if nr == 320 { native(f, 280, [sx(a0) as u64, a1, t64, a3, 0, 0]) } else { native(f, 280, [AT_FDCWD as u64, a0, t64, 0, 0, 0]) }
        }
        93 => native(f, 77, [a0, a1 as u32 as u64, 0, 0, 0, 0]), // ftruncate (32-bit length)
        324 => native(f, 285, [a0, a1, a2 | a3 << 32, a4 | a5 << 32, 0, 0]), // fallocate (64-bit offset and length in register pairs)
        312 => {
            // get_robust_list(pid, head**, len*): the caller's own (Steam checks it)
            if a0 != 0 && a0 != thread().tid {
                return Err(EPERM);
            }
            wr32(a1, thread().robust_list as u32)?;
            wr32(a2, 12)?;
            0
        }
        77 => {
            // getrusage(who, struct rusage32*): Nova does not account usage
            write_user(a1, &[0u8; 72])?;
            0
        }
        116 => {
            // sysinfo(struct sysinfo32*): from the x86-64 one, in 4 KiB units
            let buf = scratch(f, 112)?;
            let r = native(f, 99, [buf, 0, 0, 0, 0, 0]);
            if r < 0 {
                return Err(-r);
            }
            let raw = read_user(buf, 112)?;
            let q = |i: usize| u64::from_le_bytes(raw[i * 8..i * 8 + 8].try_into().unwrap());
            let unit = (u32::from_le_bytes(raw[104..108].try_into().unwrap()).max(1)) as u64;
            let pages = |v: u64| ((v * unit) / 4096).min(u32::MAX as u64) as u32;
            let mut out = Vec::new();
            for v in [q(0) as u32, q(1) as u32, q(2) as u32, q(3) as u32, pages(q(4)), pages(q(5)), pages(q(6)), pages(q(7)), pages(q(8)), pages(q(9))] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&(u16::from_le_bytes(raw[80..82].try_into().unwrap())).to_le_bytes());
            out.extend_from_slice(&[0u8; 2]);
            out.extend_from_slice(&0u32.to_le_bytes()); // totalhigh
            out.extend_from_slice(&0u32.to_le_bytes()); // freehigh
            out.extend_from_slice(&4096u32.to_le_bytes()); // mem_unit
            out.resize(64, 0);
            write_user(a0, &out)?;
            0
        }
        92 => native(f, 76, [a0, a1 as u32 as u64, 0, 0, 0, 0]), // truncate
        241 | 242 => native(f, if nr == 241 { 203 } else { 204 }, [a0, a1, a2, 0, 0, 0]), // sched_setaffinity, sched_getaffinity
        371 => native(f, 45, [a0, a1, a2, a3, a4, a5]), // recvfrom
        370 => {
            // sendmsg(fd, msghdr32*, flags)
            let (m, _, _) = msghdr64(f, a1, false)?;
            native(f, 46, [a0, m, a2, 0, 0, 0])
        }
        372 => {
            // recvmsg(fd, msghdr32*, flags)
            let (m, _, _) = msghdr64(f, a1, true)?;
            let r = native(f, 47, [a0, m, a2, 0, 0, 0]);
            if r >= 0 {
                msghdr32_out(a1, m)?;
            }
            r
        }
        365 => native(f, 55, [a0, a1, a2, a3, a4, 0]), // getsockopt
        366 => native(f, 54, [a0, a1, a2, a3, a4, 0]), // setsockopt
        394 => super::sysv::semctl(a0 as i32, a1, a2 & !0x100, a3, true)?,
        396 => super::sysv::shmctl(a0 as i32, a1 & !0x100, a2, true)?,
        397 => super::sysv::shmat(a0 as i32, a1, a2)? as i64,
        311 => {
            // set_robust_list(head, len): remembered for get_robust_list (robust futexes are
            // not cleaned up at exit yet)
            thread().robust_list = a0;
            0
        }
        244 => {
            // get_thread_area
            let entry = rd32(a0)? as usize;
            let index = entry.checked_sub(gdt::TLS_FIRST).filter(|&i| i < gdt::TLS_ENTRIES).ok_or(EINVAL)?;
            let d = thread().tls[index];
            let base = ((d >> 16) & 0xFF_FFFF) as u32 | (((d >> 56) as u32) << 24);
            let limit = (d & 0xFFFF) as u32 | (((d >> 48) & 0xF) as u32) << 16;
            write_user(a0 + 4, &[base.to_le_bytes(), limit.to_le_bytes(), 0x51u32.to_le_bytes()].concat())?;
            0
        }
        240 => {
            // futex with a 32-bit timespec timeout (FUTEX_WAIT and friends)
            let op = a1 & 0x7F;
            let timeout = if matches!(op, 0 | 9 | 11) { time64_in(f, a3, 0)? } else { a3 };
            native(f, 202, [a0, a1, a2, timeout, a4, a5])
        }
        265 | 266 => {
            // clock_gettime, clock_getres (32-bit timespec)
            let buf = scratch(f, 16)?;
            let r = native(f, if nr == 265 { 228 } else { 229 }, [a0, buf, 0, 0, 0, 0]);
            if r < 0 {
                return Err(-r);
            }
            time32_out(buf, a1)?;
            0
        }
        78 => {
            // gettimeofday (32-bit timeval)
            let buf = scratch(f, 16)?;
            let r = native(f, 96, [buf, 0, 0, 0, 0, 0]);
            if r < 0 {
                return Err(-r);
            }
            time32_out(buf, a0)?;
            0
        }
        13 => {
            // time(t)
            let t = (rtc::unix_time() as u32) as i64;
            if a0 != 0 {
                wr32(a0, t as u32)?;
            }
            t
        }
        162 => native(f, 35, [time64_in(f, a0, 0)?, 0, 0, 0, 0, 0]), // nanosleep
        267 => native(f, 230, [a0, a1, time64_in(f, a2, 0)?, 0, 0, 0]), // clock_nanosleep
        76 | 191 => {
            // getrlimit, ugetrlimit (32-bit rlimit, saturating)
            let buf = scratch(f, 16)?;
            let r = native(f, 302, [0, a0, 0, buf, 0, 0]);
            if r < 0 {
                return Err(-r);
            }
            let cur = read_u64(buf)?.min(u32::MAX as u64) as u32;
            let max = read_u64(buf + 8)?.min(u32::MAX as u64) as u32;
            write_user(a1, &[cur.to_le_bytes(), max.to_le_bytes()].concat())?;
            0
        }
        102 => {
            // socketcall(call, unsigned long *args): the old multiplexer
            let n = [0, 3, 3, 3, 2, 3, 3, 3, 4, 4, 4, 6, 6, 2, 5, 5, 3, 3, 4, 5, 4][(a0 as usize).min(20)];
            let raw = read_user(a1, n * 4)?;
            let w = |i: usize| raw.get(i * 4..i * 4 + 4).map_or(0, |c| u32::from_le_bytes(c.try_into().unwrap()) as u64);
            let nr64 = match a0 {
                1 => 41,  // socket
                2 => 49,  // bind
                3 => 42,  // connect
                4 => 50,  // listen
                5 => 43,  // accept
                6 => 51,  // getsockname
                7 => 52,  // getpeername
                8 => 53,  // socketpair
                9 => 44,  // send = sendto(..., NULL, 0)
                10 => 45, // recv = recvfrom(..., NULL, NULL)
                11 => 44, // sendto
                12 => 45, // recvfrom
                13 => 48, // shutdown
                14 => 54, // setsockopt
                15 => 55, // getsockopt
                18 => 288, // accept4
                16 | 17 => {
                    // sendmsg, recvmsg
                    let (m, _, _) = msghdr64(f, w(1), a0 == 17)?;
                    let r = native(f, if a0 == 16 { 46 } else { 47 }, [w(0), m, w(2), 0, 0, 0]);
                    if a0 == 17 && r >= 0 {
                        msghdr32_out(w(1), m)?;
                    }
                    return Ok(r);
                }
                _ => return Err(EINVAL),
            };
            native(f, nr64, [w(0), w(1), w(2), w(3), w(4), w(5)])
        }
        _ => {
            crate::log::warn(&alloc::format!("{}: unimplemented 32-bit Linux syscall {nr} ({})", proc().name, name32(nr)));
            return Err(ENOSYS);
        }
    })
}

// ---------------------------------------------------------------- sendmsg / recvmsg

/// struct msghdr (i386: seven 32-bit fields) as the x86-64 one, laid out in one scratch
/// block: msghdr (56 bytes), iovecs, then the control buffer. Control messages (cmsghdr:
/// 32-bit length on i386, 64-bit on x86-64) are converted for sendmsg. Returns (scratch
/// address of the msghdr, control buffer address, its capacity).
fn msghdr64(f: &Frame32, m: u64, for_recv: bool) -> Result<(u64, u64, u64), i64> {
    let h = read_user(m, 28)?;
    let w = |i: usize| u32::from_le_bytes(h[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
    let (name, namelen, iov, iovlen, control, controllen) = (w(0), w(1), w(2), w(3), w(4), w(5));
    if iovlen > 1024 || controllen > 65536 {
        return Err(EINVAL);
    }
    // Control messages: 32-bit headers (len, level, type) and 4-byte alignment on i386;
    // 64-bit length and 8-byte alignment on x86-64.
    let mut ctl = Vec::new();
    if !for_recv && control != 0 && controllen >= 12 {
        let raw = read_user(control, controllen as usize)?;
        let mut at = 0;
        while at + 12 <= raw.len() {
            let len = u32::from_le_bytes(raw[at..at + 4].try_into().unwrap()) as usize;
            if len < 12 || at + len > raw.len() {
                break;
            }
            let data = &raw[at + 12..at + len];
            ctl.extend_from_slice(&((16 + data.len()) as u64).to_le_bytes());
            ctl.extend_from_slice(&raw[at + 4..at + 12]); // level, type
            ctl.extend_from_slice(data);
            ctl.resize(ctl.len().next_multiple_of(8), 0);
            at += len.next_multiple_of(4);
        }
    }
    let ctl_cap = if for_recv { controllen * 2 + 64 } else { ctl.len() as u64 };
    let total = 56 + iovlen * 16 + ctl_cap;
    let base = scratch(f, total)?;
    let (iov64, ctl64) = (base + 56, base + 56 + iovlen * 16);
    let raw = read_user(iov, iovlen as usize * 8)?;
    let mut b = Vec::with_capacity(iovlen as usize * 16);
    for c in raw.chunks_exact(8) {
        b.extend_from_slice(&(u32::from_le_bytes(c[0..4].try_into().unwrap()) as u64).to_le_bytes());
        b.extend_from_slice(&(u32::from_le_bytes(c[4..8].try_into().unwrap()) as u64).to_le_bytes());
    }
    write_user(iov64, &b)?;
    if !ctl.is_empty() {
        write_user(ctl64, &ctl)?;
    }
    let has_ctl = if for_recv { control != 0 } else { !ctl.is_empty() };
    let mut hdr = Vec::with_capacity(56);
    for v in [name, namelen, iov64, iovlen, if has_ctl { ctl64 } else { 0 }, if has_ctl { ctl_cap } else { 0 }, 0] {
        hdr.extend_from_slice(&v.to_le_bytes());
    }
    // namelen and flags are 32-bit fields followed by padding in the x86-64 layout.
    write_user(base, &hdr)?;
    Ok((base, ctl64, ctl_cap))
}

/// After recvmsg: copy namelen, flags and the (converted) control messages back.
fn msghdr32_out(m: u64, base: u64) -> Result<(), i64> {
    let h = read_user(base, 56)?;
    let q = |i: usize| u64::from_le_bytes(h[i * 8..i * 8 + 8].try_into().unwrap());
    let (namelen, ctl64, ctllen, flags) = (q(1) as u32, q(4), q(5), q(6) as u32);
    wr32(m + 4, namelen)?;
    let control = rd32(m + 16)? as u64;
    let cap = rd32(m + 20)? as usize;
    let mut out = Vec::new();
    let mut truncated = false;
    if ctl64 != 0 && ctllen > 0 {
        let raw = read_user(ctl64, ctllen as usize)?;
        let mut at = 0;
        while at + 16 <= raw.len() {
            let len = u64::from_le_bytes(raw[at..at + 8].try_into().unwrap()) as usize;
            if len < 16 || at + len > raw.len() {
                break;
            }
            let data = &raw[at + 16..at + len];
            let mut c = Vec::new();
            c.extend_from_slice(&((12 + data.len()) as u32).to_le_bytes());
            c.extend_from_slice(&raw[at + 8..at + 16]);
            c.extend_from_slice(data);
            c.resize(c.len().next_multiple_of(4), 0);
            if out.len() + c.len() > cap {
                truncated = true;
                break;
            }
            out.extend_from_slice(&c);
            at += len.next_multiple_of(8);
        }
    }
    if control != 0 && !out.is_empty() {
        write_user(control, &out)?;
    }
    wr32(m + 20, out.len() as u32)?;
    wr32(m + 24, flags | if truncated { 8 } else { 0 })?; // MSG_CTRUNC
    Ok(())
}

// ---------------------------------------------------------------- signals

use crate::signal::{self, Action};

// i386 frames (Linux's sigframe_ia32 / rt_sigframe_ia32 layouts).
const SC_SIZE: u64 = 88;
const RT_INFO: u64 = 16;
const RT_UC: u64 = RT_INFO + 128;
const RT_MCONTEXT: u64 = RT_UC + 20;
const RT_SIGMASK: u64 = RT_MCONTEXT + SC_SIZE;
const RT_RETCODE: u64 = RT_SIGMASK + 8;
const RT_SIZE: u64 = RT_RETCODE + 8;
const OLD_SC: u64 = 8;
const OLD_EXTRAMASK: u64 = OLD_SC + SC_SIZE;
const OLD_RETCODE: u64 = OLD_EXTRAMASK + 4;
const OLD_SIZE: u64 = OLD_RETCODE + 8;

/// sigcontext_32: gs, fs, es, ds, edi, esi, ebp, esp, ebx, edx, ecx, eax, trapno, err,
/// eip, cs, eflags, esp_at_signal, ss, fpstate, oldmask, cr2.
fn sigcontext(f: &Frame32, mask: u64) -> [u32; 22] {
    // The live selectors: user_fs/user_gs are only saved when the thread is switched out.
    let (fs, gs) = process::current_fs_gs(thread());
    [
        gs as u32, fs as u32, gdt::USER_DATA as u32, gdt::USER_DATA as u32,
        f.rdi as u32, f.rsi as u32, f.rbp as u32, f.rsp as u32, f.rbx as u32, f.rdx as u32, f.rcx as u32, f.rax as u32,
        0, 0, f.rip as u32, gdt::USER_CODE32 as u32, f.rflags as u32, f.rsp as u32, gdt::USER_DATA as u32, 0, mask as u32, 0,
    ]
}

fn put_words(at: u64, words: &[u32]) -> Result<(), i64> {
    write_user(at, &words.iter().flat_map(|w| w.to_le_bytes()).collect::<Vec<u8>>())
}

/// Deliver pending signals to a 32-bit thread before it returns from int 0x80.
pub fn deliver_signals(f: &mut Frame32) {
    while let Some((sig, sender, to_thread)) = signal::take(thread()) {
        let action = proc().signals.lock().actions[sig as usize];
        match action.handler {
            1 => continue, // SIG_IGN
            0 => match signal::default_action(sig) {
                signal::Default::Ignore => continue,
                signal::Default::Terminate => {
                    process::terminate(proc(), sig);
                    process::check_exiting();
                    return;
                }
            },
            _ => {
                if push_frame(f, sig, sender, to_thread, &action).is_err() {
                    process::terminate(proc(), signal::SIGSEGV);
                    process::check_exiting();
                }
                return;
            }
        }
    }
}

fn push_frame(f: &mut Frame32, sig: u32, sender: u32, to_thread: bool, action: &Action) -> Result<(), i64> {
    const SA_SIGINFO: u64 = 4;
    const SA_ONSTACK: u64 = 0x0800_0000;
    const SA_RESTORER: u64 = 0x0400_0000;
    const SA_NODEFER: u64 = 0x4000_0000;
    const SA_RESETHAND: u64 = 0x8000_0000;
    let t = thread();
    let old_mask = t.saved_mask.take().unwrap_or(t.sigmask);
    let rt = action.flags & SA_SIGINFO != 0;
    let size = if rt { RT_SIZE } else { OLD_SIZE };
    let use_alt = action.flags & SA_ONSTACK != 0 && t.altstack.2 & 2 == 0 && t.altstack.1 != 0 && !(f.rsp >= t.altstack.0 && f.rsp < t.altstack.0 + t.altstack.1);
    let top = if use_alt { t.altstack.0 + t.altstack.1 } else { f.rsp & 0xFFFF_FFFF };
    // At the handler's first instruction, esp + 4 is 16-byte aligned (as after a call).
    let frame = ((top - size) & !15) - 4;
    if !mem().ensure(frame, size, true) {
        return Err(EFAULT);
    }
    let sc = sigcontext(f, old_mask);
    let retcode_at = frame + if rt { RT_RETCODE } else { OLD_RETCODE };
    let ret = if action.flags & SA_RESTORER != 0 { action.restorer as u32 } else { retcode_at as u32 };
    // "mov eax, N; int 0x80": rt_sigreturn (173) or sigreturn (119).
    let nr: u32 = if rt { 173 } else { 119 };
    let retcode = [0xB8, nr as u8, (nr >> 8) as u8, 0, 0, 0xCD, 0x80, 0x90];
    write_user(frame, &alloc::vec![0u8; size as usize])?;
    if rt {
        put_words(frame, &[ret, sig, (frame + RT_INFO) as u32, (frame + RT_UC) as u32])?;
        match signal::sigsys_info(t, sig, to_thread) {
            // _sigsys: si_errno = filter data, SYS_SECCOMP, call address, number, arch.
            Some(i) => put_words(frame + RT_INFO, &[sig, i.errno, seccomp::SYS_SECCOMP, i.call_addr as u32, i.syscall, i.arch])?,
            None => put_words(frame + RT_INFO, &[sig, 0, if to_thread { -6i32 as u32 } else { 0 }, process::pid_view(proc(), sender as u64) as u32, 0])?,
        }
        let stack_flags = if use_alt { t.altstack.2 } else { 2 };
        put_words(frame + RT_UC, &[0, 0, t.altstack.0 as u32, stack_flags, t.altstack.1 as u32])?;
        put_words(frame + RT_MCONTEXT, &sc)?;
        put_words(frame + RT_SIGMASK, &[old_mask as u32, (old_mask >> 32) as u32])?;
    } else {
        put_words(frame, &[ret, sig])?;
        put_words(frame + OLD_SC, &sc)?;
        put_words(frame + OLD_EXTRAMASK, &[(old_mask >> 32) as u32])?;
    }
    write_user(retcode_at, &retcode)?;
    process::push_signal_fpu(t);
    t.sigmask |= action.mask | if action.flags & SA_NODEFER == 0 { signal::bit(sig) } else { 0 };
    t.sigmask &= !(signal::bit(signal::SIGKILL) | signal::bit(signal::SIGSTOP));
    if action.flags & SA_RESETHAND != 0 {
        proc().signals.lock().actions[sig as usize] = Action::default();
    }
    f.rsp = frame;
    f.rip = action.handler & 0xFFFF_FFFF;
    // Linux also passes sig, &info, &uc in eax, edx, ecx (regparm(3) handlers).
    f.rax = sig as u64;
    f.rdx = if rt { frame + RT_INFO } else { 0 };
    f.rcx = if rt { frame + RT_UC } else { 0 };
    f.rflags &= !(1 << 10);
    Ok(())
}

/// rt_sigreturn (173) and sigreturn (119) for 32-bit threads.
fn sigreturn(f: &mut Frame32, rt: bool) -> i64 {
    // The handler's `ret` popped pretcode; old-style restorers also popped sig.
    let frame = if rt { f.rsp - 4 } else { f.rsp - 8 };
    let sc_at = frame + if rt { RT_MCONTEXT } else { OLD_SC };
    let Ok(raw) = read_user(sc_at, SC_SIZE as usize) else {
        process::terminate(proc(), signal::SIGSEGV);
        process::check_exiting();
        return 0;
    };
    let w = |i: usize| u32::from_le_bytes(raw[i * 4..i * 4 + 4].try_into().unwrap()) as u64;
    let mask = if rt {
        read_u64(frame + RT_SIGMASK).unwrap_or(0)
    } else {
        w(20) | (read_u32(frame + OLD_EXTRAMASK).unwrap_or(0) as u64) << 32
    };
    let t = thread();
    t.sigmask = mask & !(signal::bit(signal::SIGKILL) | signal::bit(signal::SIGSTOP));
    t.user_gs = w(0) as u16;
    t.user_fs = w(1) as u16;
    (f.rdi, f.rsi, f.rbp, f.rbx, f.rdx, f.rcx, f.rax) = (w(4), w(5), w(6), w(8), w(9), w(10), w(11));
    f.rsp = w(7);
    f.rip = w(14);
    f.rflags = (w(16) & 0x0CD5) | 0x202;
    process::pop_signal_fpu(t);
    // The restored selectors must be loadable user ones; anything else becomes null.
    let ok = |s: u16| s == 0 || s == gdt::USER_DATA || (s & 3 == 3 && (s >> 3) as usize >= gdt::TLS_FIRST && ((s >> 3) as usize) < gdt::TLS_FIRST + gdt::TLS_ENTRIES);
    if !ok(t.user_gs) {
        t.user_gs = 0;
    }
    if !ok(t.user_fs) {
        t.user_fs = 0;
    }
    process::reload_fs_gs(t.user_fs, t.user_gs);
    f.rax as i64
}
