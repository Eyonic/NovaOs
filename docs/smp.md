# Multiprocessor support (SMP)

Nova uses every CPU of the machine for user code: the threads of a program (Chromium's,
Steam's, a game's) run in parallel on all cores. The kernel itself stays single-threaded:
one big kernel lock (BKL) makes sure only one CPU at a time executes kernel code, so every
kernel structure keeps the one-CPU assumptions it was written with. This is how Linux 2.0
and the early BSDs went multiprocessor; finer locking can come path by path later (see
"What's next").

`nova.cpus=N` on the kernel command line uses at most N CPUs; `nova.cpus=1` is exactly the
old single-CPU behaviour (the other CPUs are halted for good) and is the safety switch for
a first boot on new hardware. The USB boot menu (`NOVA_BOOT_MENU=1`) has a "one CPU" entry.

## Bring-up (`smp.rs`, `limine.rs`)

Limine's MP request starts every application processor (AP) before the kernel runs and
parks it. After the boot CPU has its local APIC (`apic.rs`) and a calibrated TSC,
`smp::start_aps` gives each AP an id (1, 2, ... in Limine's order), an idle task and its
stack (`task::add_idle`), and writes its `goto_address`. The AP (`ap_entry`, `ap_main`):

1. switches to its stack and copies the boot CPU's CR0, CR4, EFER and PAT (paging with NX
   and global pages, write protection, the SSE/XSAVE enables, memory types), loads the
   kernel's page tables;
2. loads its own GDT and TSS (`gdt::init_cpu`), the shared IDT, sets XCR0 and its SYSCALL
   MSRs (`process::init_cpu`);
3. enables its local APIC in xAPIC mode (`apic::init_ap`) and starts its LAPIC timer
   (periodic, at the PIT's 1 kHz, calibrated once against the TSC);
4. reports ready and becomes its idle task.

CPUs Nova does not use (beyond `nova.cpus`, or all of them without an I/O APIC) are sent
to a `cli; hlt` loop instead of being left spinning in the bootloader.

## Per-CPU state

| What | Where |
|---|---|
| CPU id | `smp::id()`: the TSS selector (`str`). CPU n's TSS descriptor is GDT entry 16 + 2n, so no GS base or MSR is needed |
| GDT (with the 32-bit TLS entries of the thread running there) and TSS (RSP0, the #DF IST stack) | `gdt.rs`, one of each per CPU |
| SYSCALL entry | one 32-byte stub per CPU (`process.rs`); LSTAR points at the CPU's own stub, which saves the user RSP into and loads the kernel stack from its own slot, RIP-relative. No `swapgs`: Nova never touches the GS base |
| current task | `Scheduler::current[cpu]` (`task.rs`) |
| loaded address space | `smp::LOADED_CR3[cpu]`, kept by `smp::load_cr3` (for shootdowns) |
| timer | the boot CPU keeps the PIT; APs have their LAPIC timer (`smp::TIMER_VECTOR`) |
| FPU/SSE/AVX state | saved and restored per thread on every switch as before; a thread that moves to another CPU takes it along |

## The big kernel lock

`smp::lock()` / `smp::unlock()`; the lock is owned by a CPU, not a task (a task switch
inside the kernel keeps it, and a thread that blocked on one CPU may return to user mode
from another).

* Taken on every entry from user mode: system calls (SYSCALL and `int 0x80`), exceptions
  (page faults), the preemption tick, a kick. Kernel tasks (the desktop, drivers, Linux's
  kthreads) hold it while they run. An idle CPU takes it to look for work.
* Released on every way back to user mode (`process::leave_kernel`: SYSRET, the IRETQs of
  exceptions and interrupts, the first entry of a thread, execve) and when a CPU goes idle.
* Waiting for it, a CPU keeps interrupts enabled where it can and serves TLB shootdowns in
  any case, so the holder can never wait for the waiter.
* Handed over in the scheduler (`smp::relax`): a CPU about to pick the next task first lets
  in the CPUs waiting for the lock (usually threads entering a system call). Without this, a
  CPU running parked tasks one after another, each re-checking its condition, kept the lock
  for good: with Chromium on the desktop under 4 CPUs the lock was held 97% of the time,
  the other CPUs waited for it 67% of theirs, and Chromium's main thread starved in its
  system calls (the `browser` case failed under load).

Without the lock run only: user mode, the idle halt, interrupt top halves that just record
data (PIT ticks, the PS/2 and COM1 rings, which take a small producer lock now), and IPIs.

### Device interrupts

All device interrupts (I/O APIC lines, MSI) still go to the boot CPU. Handlers that run
driver code (ACPI's SCI, Linux drivers' handlers) need the BKL (`smp::device_irq`):

* if the boot CPU holds or can take it, the handler runs right there;
* otherwise it is *deferred*: the vector is marked, a level-triggered line is masked at the
  I/O APIC, the interrupt is acknowledged, and the holder gets an `IPI_IRQ`. The holder runs
  the handler nested in its kernel code, exactly as if the interrupt had arrived on it, so
  driver code that disables interrupts to exclude its handler (lx_emul's lists, uACPI's
  spinlocks, Linux's `spin_lock_irqsave`) keeps working. Whoever releases the lock runs
  what is still deferred first.

This also keeps busy-waits in kernel code that poll for an interrupt-set flag working on
any CPU, which would deadlock if handlers waited for the lock.

Linux's `current` (the driver layer is built without `CONFIG_SMP`) is a single variable; it
is pointed at the CPU's current task on every switch and every time the lock is taken.

## Scheduler (`task.rs`)

One task list, shared under the BKL. Each task records the CPU running it; a CPU that
schedules picks, round robin, the next runnable task that no other CPU runs (and whose
affinity allows this CPU). A CPU with nothing to run switches to its idle task, which:

* halts with the lock released until it is *kicked* (`IPI_KICK`), or, on the boot CPU,
  until any interrupt (parked tasks wait for "the next interrupt", as before);
* is kicked when a task becomes ready (`task::wake`, `spawn`), when a CPU leaves a task
  that is still ready (preempted) or sees more ready tasks than it can take, and by the boot
  CPU's timer, at the preemption rate, when the boot CPU is busy and tasks are waiting (one
  idle CPU at a time). Parked tasks that an interrupt made runnable do not count as work for
  another CPU: they usually just re-check and park again, which the lock serialises anyway.

User threads are preempted every 4 ms by their CPU's timer (PIT on the boot CPU, LAPIC on
the others). `sched_setaffinity`/`sched_getaffinity` set and report a per-task CPU mask.

## TLB shootdown

Every page-table change that removes or restricts a mapping is followed by a shootdown
before the frames can be reused (`smp::flush_user`): munmap, mprotect to read-only,
MAP_FIXED over existing pages, mremap, copy-on-write of a shared system-file page, madvise
discard, address-space teardown. `process::Memory` collects the stale pages of an
operation (`stale`) and flushes once at its end; unmapped frames are freed only after
that. The targets are the other CPUs whose `LOADED_CR3` is that address space; each
invalidates the pages (or reloads CR3 for more than 32) in its `IPI_TLB` handler, or while
waiting for the BKL, and acknowledges; the initiator (the BKL holder, so one request at a
time) waits. Granting access (read-only to writable, new page-table levels) needs no
shootdown: a CPU with the old entry takes a spurious page fault, which `page_fault`
recognises and retries. A dropped address space is evicted from any CPU still holding it.

Kernel mappings are shared by all address spaces: `vunmap` (the Linux driver layer's
`vmap` window) shoots down on every CPU (`smp::flush_kernel`), toggling CR4.PGE for large
ranges since kernel entries may be global. MMIO mappings are never removed.

## Other places that knew there was one CPU

* Process exit, `exit_group`, SIGKILL and execve kick the CPUs running the program's other
  threads, which enter the kernel and leave at once instead of at their next tick.
* `FUTEX_WAKE_OP` updates the user's word with an atomic read-modify-write.
* `getcpu` reports the real CPU; `/proc/cpuinfo`, `/proc/stat`,
  `/sys/devices/system/cpu/{online,possible,present}` and the `cpuN` directories describe
  every online CPU, so glibc's `get_nprocs`, `sysconf(_SC_NPROCESSORS_ONLN/CONF)` and
  `nproc` agree (Chromium, Mesa and games size their thread pools from these).
* `task::cpu_usage` averages over all CPUs; `ps` shows which CPU runs a task.
* Panics and fatal kernel exceptions stop the other CPUs first (`IPI_STOP`).
* `madvise(MADV_FREE)` keeps the pages (it used to drop them at once like
  `MADV_DONTNEED`): Linux keeps a page the program writes again, and a thread on another CPU
  may write it while the call runs.

Signals are still delivered when the target thread next returns from a system call, as
before; a thread spinning in user code on another CPU sees a fatal signal (via the exit
kick) at once, a handled one at its next system call.

## Measurements

Steam from `steam` to its sign-in window (docs/steam.md, QEMU/KVM, 10 GiB), runs of the
two configurations interleaved so that both see the same network:

| | update phase | web helper started | sign-in window |
|---|---|---|---|
| `nova.cpus=1` | 60-61 s | +70 s | 74 s |
| 4 CPUs | 52-53 s | +60-61 s | 64-65 s |

The update phase (the bootstrapper fetching and unpacking the client) leaves the CPUs
85-90% idle: it waits on downloads and sleeps. When the client starts, the BKL is held
55-90% of the time (mmap, clone, getdents, access, readlink of many processes at once),
which caps the gain there.

`cpus` in the Nova shell shows, since its previous call, each CPU's idle and lock-wait
time, how long the BKL was held, and what for (the holder's system call at timer ticks).

## Testing

`scripts/common.sh` boots QEMU with `-smp ${NOVA_SMP:-4}`. `NOVA_TEST_CMDLINE="nova.cpus=1"
scripts/test.sh --all` runs every case on one CPU. The `smp` case runs
`userland/smptest.c`: the CPU count from every source, busy threads that must overlap and
run on every CPU (speedup close to the CPU count), contended mutexes and condition
variables, mmap/munmap/mprotect from many threads, and a page replaced again and again with
`MAP_FIXED` while another CPU reads it, whose old frame is reused at once: a missing
shootdown shows up as stale reads (hundreds of thousands of them when the shootdown is
switched off).

## What's next

* Take the BKL off self-contained hot paths: futex wait/wake, `sched_yield`,
  `clock_gettime`/`gettimeofday` (better: a vDSO), `getpid`-style calls, page faults on
  private anonymous memory.
* Parked tasks poll: a task in `wait_event` becomes runnable at the next interrupt and
  checks its condition. Futex waits and sleeps no longer do (`task::block`: parked until
  `task::wake` or a deadline; a wake that comes early is remembered), but poll, epoll and
  wait4 still do, which is most of the BKL time of an idle Steam (about 12%). Tried and
  measured: `ipc::wait` parked with `task::block` (re-check every 10 ms) gains nothing as
  long as the network task wakes every I/O waiter after each stack poll (every tick); with
  that wake limited to polls where smoltcp moved a packet, idle Steam's lock time fell to
  about 1%, but the Steam client then stalled before starting its web helper on 4 CPUs
  (some socket readiness is not announced by packet activity). That needs finding first.

  Second attempt (2026-10-17, not merged), measured on fresh images with the same script
  (baseline: web helper +62 s, sign-in +66 s, idle Steam 14% lock time):
  - the network task notifies only when a fingerprint of every socket's readiness
    (state, can/may recv/send, queue lengths) changes, whoever polled the stack;
  - `ipc::wait` parks in a new `Io` state until `notify`, a device interrupt (a separate
    counter that the timer does not bump), the caller's deadline, the expiry of a timerfd
    the caller itself polled, or a 10 ms re-check;
  - every `next_gen()` marks a change, and the system-call exit wakes waiters once.

  Steam then reached the sign-in window at +127 s, or later. Logging showed that almost
  no readiness was found only by the 10 ms re-check, so wake-ups were not missed. The cost
  is the opposite: `notify` wakes every I/O waiter (100+ Chromium threads), and doing so
  after every state-changing system call wakes them far more often than once per tick.
  The way forward is per-object wait queues (a waiter registers on the channels, sockets
  and timerfds it polls, and a change wakes only those), not a global wake list.
* Device interrupts on other CPUs (MSI affinity), asynchronous signal delivery to a thread
  running on another CPU, x2APIC for machines with APIC ids above 254, PCID.
