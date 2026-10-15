//! System V IPC: semaphore sets and shared-memory segments (semget/semop/semctl,
//! shmget/shmat/shmdt/shmctl). Steam's runtime uses SysV semaphores for its thread and
//! process synchronisation, and X11 clients use SysV shared memory for MIT-SHM images.
//!
//! Message queues are not provided. Permissions are recorded (IPC_STAT) but not checked,
//! like everything else in Nova.

use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;

use crate::ipc::{self, SharedRef};
use crate::sync::SpinLock;

use super::{proc, read_user, write_user, EAGAIN, EEXIST, EINTR, EINVAL, ENOENT};

const IPC_CREAT: u64 = 0o1000;
const IPC_EXCL: u64 = 0o2000;
const IPC_NOWAIT: i16 = 0o4000;
const IPC_RMID: u64 = 0;
const IPC_SET: u64 = 1;
const IPC_STAT: u64 = 2;
const IPC_INFO: u64 = 3;
const GETPID: u64 = 11;
const GETVAL: u64 = 12;
const GETALL: u64 = 13;
const GETNCNT: u64 = 14;
const GETZCNT: u64 = 15;
const SETVAL: u64 = 16;
const SETALL: u64 = 17;
const E2BIG: i64 = 7;
const ERANGE: i64 = 34;
const EIDRM: i64 = 43;

struct SemSet {
    key: i32,
    values: Vec<u16>,
    pids: Vec<u32>,
    mode: u32,
    otime: u64,
    ctime: u64,
}

struct Segment {
    key: i32,
    mem: SharedRef,
    size: u64,
    mode: u32,
    attached: u32,
    removed: bool,
    cpid: u32,
    lpid: u32,
}

struct Tables {
    sems: BTreeMap<i32, SemSet>,
    shms: BTreeMap<i32, Segment>,
    next: i32,
}

static TABLES: SpinLock<Tables> = SpinLock::new(Tables { sems: BTreeMap::new(), shms: BTreeMap::new(), next: 1 });

/// Where each process attached segments: (pid, address) -> (shmid, length).
static ATTACHED: SpinLock<BTreeMap<(u64, u64), (i32, u64)>> = SpinLock::new(BTreeMap::new());

fn now() -> u64 {
    crate::rtc::unix_time()
}

// ---------------------------------------------------------------- semaphores

pub fn semget(key: i32, nsems: u64, flags: u64) -> Result<i64, i64> {
    let mut t = TABLES.lock();
    if key != 0
        && let Some((&id, s)) = t.sems.iter().find(|(_, s)| s.key == key)
    {
        if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
            return Err(EEXIST);
        }
        if nsems as usize > s.values.len() {
            return Err(EINVAL);
        }
        return Ok(id as i64);
    }
    if key != 0 && flags & IPC_CREAT == 0 {
        return Err(ENOENT);
    }
    if nsems == 0 || nsems > 32000 {
        return Err(EINVAL);
    }
    let id = t.next;
    t.next += 1;
    t.sems.insert(id, SemSet { key, values: vec![0; nsems as usize], pids: vec![0; nsems as usize], mode: flags as u32 & 0o777, otime: 0, ctime: now() });
    Ok(id as i64)
}

/// semop / semtimedop: apply every operation atomically, waiting while one cannot
/// proceed (unless IPC_NOWAIT). `deadline` in ticks.
pub fn semop(id: i32, sops: u64, nsops: u64, deadline: Option<u64>) -> Result<i64, i64> {
    if nsops == 0 || nsops > 500 {
        return Err(if nsops == 0 { EINVAL } else { E2BIG });
    }
    let raw = read_user(sops, nsops as usize * 6)?;
    let ops: Vec<(usize, i16, i16)> = raw.chunks_exact(6).map(|b| (u16::from_le_bytes([b[0], b[1]]) as usize, i16::from_le_bytes([b[2], b[3]]), i16::from_le_bytes([b[4], b[5]]))).collect();
    loop {
        {
            let mut t = TABLES.lock();
            let s = t.sems.get_mut(&id).ok_or(EIDRM)?;
            if ops.iter().any(|&(n, _, _)| n >= s.values.len()) {
                return Err(EINVAL);
            }
            // Would every operation go through?
            let mut values = s.values.clone();
            let mut blocked_nowait = false;
            let mut ok = true;
            for &(n, op, flg) in &ops {
                let v = values[n] as i32;
                let next = v + op as i32;
                if (op == 0 && v != 0) || next < 0 {
                    ok = false;
                    blocked_nowait |= flg & IPC_NOWAIT != 0;
                    break;
                }
                if next > 32767 {
                    return Err(ERANGE);
                }
                values[n] = next as u16;
            }
            if ok {
                let pid = proc().pid as u32;
                for &(n, _, _) in &ops {
                    s.pids[n] = pid;
                }
                s.values = values;
                s.otime = now();
                drop(t);
                ipc::notify();
                return Ok(0);
            }
            if blocked_nowait {
                return Err(EAGAIN);
            }
        }
        if deadline.is_some_and(|d| crate::pit::ticks() >= d) {
            return Err(EAGAIN);
        }
        ipc::wait(deadline);
        crate::process::check_exiting();
        if crate::signal::has_deliverable(super::thread()) {
            return Err(EINTR);
        }
    }
}

/// semctl(id, semnum, cmd, arg): `arg` is the union semun (a value or a pointer).
pub fn semctl(id: i32, num: u64, cmd: u64, arg: u64, compat: bool) -> Result<i64, i64> {
    let mut t = TABLES.lock();
    if cmd == IPC_INFO {
        return Ok(t.sems.keys().max().copied().unwrap_or(0) as i64);
    }
    let s = t.sems.get_mut(&id).ok_or(EINVAL)?;
    let n = num as usize;
    Ok(match cmd {
        IPC_RMID => {
            t.sems.remove(&id);
            drop(t);
            ipc::notify();
            0
        }
        IPC_SET => 0,
        IPC_STAT => {
            // struct semid64_ds: ipc_perm, otime, ctime, nsems
            let mut b = vec![0u8; if compat { 64 } else { 104 }];
            let perm = perm_bytes(s.key, s.mode);
            b[..perm.len()].copy_from_slice(&perm);
            let (o, c, k) = if compat { (36, 44, 52) } else { (48, 64, 80) };
            b[o..o + 4].copy_from_slice(&(s.otime as u32).to_le_bytes());
            b[c..c + 4].copy_from_slice(&(s.ctime as u32).to_le_bytes());
            b[k..k + 4].copy_from_slice(&(s.values.len() as u32).to_le_bytes());
            drop(t);
            write_user(arg, &b)?;
            0
        }
        GETVAL => *s.values.get(n).ok_or(EINVAL)? as i64,
        GETPID => *s.pids.get(n).ok_or(EINVAL)? as i64,
        GETNCNT | GETZCNT => 0,
        GETALL => {
            let b: Vec<u8> = s.values.iter().flat_map(|v| v.to_le_bytes()).collect();
            drop(t);
            write_user(arg, &b)?;
            0
        }
        SETVAL => {
            let v = arg as u32 as i32;
            if !(0..=32767).contains(&v) {
                return Err(ERANGE);
            }
            *s.values.get_mut(n).ok_or(EINVAL)? = v as u16;
            s.ctime = now();
            drop(t);
            ipc::notify();
            0
        }
        SETALL => {
            let len = s.values.len();
            drop(t);
            let raw = read_user(arg, len * 2)?;
            let mut t = TABLES.lock();
            let s = t.sems.get_mut(&id).ok_or(EIDRM)?;
            for (v, b) in s.values.iter_mut().zip(raw.chunks_exact(2)) {
                *v = u16::from_le_bytes([b[0], b[1]]);
            }
            drop(t);
            ipc::notify();
            0
        }
        _ => return Err(EINVAL),
    })
}

/// struct ipc64_perm: key, uid, gid, cuid, cgid, mode (the 64-bit layout's first 24 bytes).
fn perm_bytes(key: i32, mode: u32) -> Vec<u8> {
    let uid = crate::process::NOVA_UID;
    let mut b = Vec::new();
    for v in [key as u32, uid, uid, uid, uid, mode] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b
}

// ---------------------------------------------------------------- shared memory

pub fn shmget(key: i32, size: u64, flags: u64) -> Result<i64, i64> {
    let mut t = TABLES.lock();
    if key != 0
        && let Some((&id, s)) = t.shms.iter().find(|(_, s)| s.key == key && !s.removed)
    {
        if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
            return Err(EEXIST);
        }
        if size > s.size {
            return Err(EINVAL);
        }
        return Ok(id as i64);
    }
    if key != 0 && flags & IPC_CREAT == 0 {
        return Err(ENOENT);
    }
    if size == 0 || size > 1 << 34 {
        return Err(EINVAL);
    }
    let id = t.next;
    t.next += 1;
    let mem = ipc::memfd();
    mem.lock().truncate(size);
    t.shms.insert(id, Segment { key, mem, size, mode: flags as u32 & 0o777, attached: 0, removed: false, cpid: proc().pid as u32, lpid: 0 });
    Ok(id as i64)
}

pub fn shmat(id: i32, addr: u64, flags: u64) -> Result<u64, i64> {
    let (mem, size) = {
        let mut t = TABLES.lock();
        let s = t.shms.get_mut(&id).ok_or(EINVAL)?;
        if s.removed && s.attached == 0 {
            return Err(EIDRM);
        }
        s.attached += 1;
        s.lpid = proc().pid as u32;
        (s.mem.clone(), s.size)
    };
    let len = size.next_multiple_of(crate::process::PAGE);
    let readonly = flags & 0o10000 != 0; // SHM_RDONLY
    let va = {
        let mut m = super::mem();
        let va = if addr != 0 {
            let a = if flags & 0o20000 != 0 { addr & !(crate::process::PAGE - 1) } else { addr }; // SHM_RND
            m.unmap(a, len);
            m.areas.push(crate::process::Area { start: a, end: a + len, writable: true });
            a
        } else {
            m.reserve_at(0, len, true)
        };
        m.map_shared(va, mem, 0, len);
        if readonly {
            m.protect(va, len, false);
        }
        va
    };
    ATTACHED.lock().insert((proc().pid, va), (id, len));
    Ok(va)
}

pub fn shmdt(addr: u64) -> Result<i64, i64> {
    let (id, len) = ATTACHED.lock().remove(&(proc().pid, addr)).ok_or(EINVAL)?;
    super::mem().unmap(addr, len);
    detach(id);
    Ok(0)
}

fn detach(id: i32) {
    let mut t = TABLES.lock();
    if let Some(s) = t.shms.get_mut(&id) {
        s.attached = s.attached.saturating_sub(1);
        if s.removed && s.attached == 0 {
            t.shms.remove(&id);
        }
    }
}

/// A process ended (or exec'd): its attachments go away.
pub fn process_gone(pid: u64) {
    let ids: Vec<i32> = {
        let mut a = ATTACHED.lock();
        let keys: Vec<(u64, u64)> = a.keys().filter(|k| k.0 == pid).copied().collect();
        keys.iter().filter_map(|k| a.remove(k)).map(|(id, _)| id).collect()
    };
    for id in ids {
        detach(id);
    }
}

/// A forked child inherits the parent's attachments.
pub fn process_forked(parent: u64, child: u64) {
    let mut a = ATTACHED.lock();
    let inherited: Vec<((u64, u64), (i32, u64))> = a.iter().filter(|(k, _)| k.0 == parent).map(|(&(_, va), &v)| ((child, va), v)).collect();
    let mut t = TABLES.lock();
    for (k, v) in inherited {
        if let Some(s) = t.shms.get_mut(&v.0) {
            s.attached += 1;
        }
        a.insert(k, v);
    }
}

pub fn shmctl(id: i32, cmd: u64, buf: u64, compat: bool) -> Result<i64, i64> {
    let mut t = TABLES.lock();
    if cmd == IPC_INFO {
        return Ok(t.shms.keys().max().copied().unwrap_or(0) as i64);
    }
    let s = t.shms.get_mut(&id).ok_or(EINVAL)?;
    Ok(match cmd {
        IPC_RMID => {
            s.removed = true;
            if s.attached == 0 {
                t.shms.remove(&id);
            }
            0
        }
        IPC_SET => 0,
        IPC_STAT => {
            // struct shmid64_ds: ipc_perm (48), segsz, atime, dtime, ctime, cpid, lpid, nattch
            let mut b = vec![0u8; if compat { 84 } else { 112 }];
            let perm = perm_bytes(s.key, s.mode);
            b[..perm.len()].copy_from_slice(&perm);
            if compat {
                b[36..40].copy_from_slice(&(s.size as u32).to_le_bytes());
                b[64..68].copy_from_slice(&s.cpid.to_le_bytes());
                b[68..72].copy_from_slice(&s.lpid.to_le_bytes());
                b[72..76].copy_from_slice(&s.attached.to_le_bytes());
            } else {
                b[48..56].copy_from_slice(&s.size.to_le_bytes());
                b[80..84].copy_from_slice(&s.cpid.to_le_bytes());
                b[84..88].copy_from_slice(&s.lpid.to_le_bytes());
                b[88..96].copy_from_slice(&(s.attached as u64).to_le_bytes());
            }
            drop(t);
            write_user(buf, &b)?;
            0
        }
        _ => return Err(EINVAL),
    })
}
