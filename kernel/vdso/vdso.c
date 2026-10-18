// Nova's vDSO (64-bit programs): clock_gettime, gettimeofday and time without a system
// call. The kernel maps a read-only data page (`nova_vvar`, src/vdso.rs) just below this
// object; the clock is the kernel's own (pit::nanos): the TSC scaled by a multiplier
// calibrated at boot, plus an offset for the wall clock. Anything else is a system call.

typedef unsigned long u64;
typedef unsigned int u32;
typedef long i64;

struct vvar {
    u64 tsc_base;    // TSC value at base_ns
    u64 base_ns;     // monotonic nanoseconds at tsc_base
    u64 mult;        // nanoseconds per TSC tick, times 2^32
    u64 real_offset; // CLOCK_REALTIME minus CLOCK_MONOTONIC (wrapping)
    u64 ready;       // 0 until the kernel has filled the page
};
extern const volatile struct vvar nova_vvar __attribute__((visibility("hidden")));

struct timespec { i64 tv_sec; i64 tv_nsec; };
struct timeval { i64 tv_sec; i64 tv_usec; };

static inline u64 rdtsc(void) {
    u32 lo, hi;
    __asm__ volatile("rdtsc" : "=a"(lo), "=d"(hi));
    return (u64)hi << 32 | lo;
}

static inline u64 mono(void) {
    u64 d = rdtsc() - nova_vvar.tsc_base;
    return nova_vvar.base_ns + (u64)(((unsigned __int128)d * nova_vvar.mult) >> 32);
}

static inline long sys2(long nr, long a, long b) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b) : "rcx", "r11", "memory");
    return r;
}

static inline long sys3(long nr, long a, long b, long c) {
    long r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return r;
}

int __vdso_clock_gettime(int clock, struct timespec *ts) {
    u64 ns;
    if (!nova_vvar.ready)
        return sys2(228, clock, (long)ts);
    switch (clock) {
    case 0: case 5: case 8: case 11: // REALTIME, REALTIME_COARSE, REALTIME_ALARM, TAI
        ns = mono() + nova_vvar.real_offset;
        break;
    case 1: case 4: case 6: case 7: case 9: // MONOTONIC (RAW, COARSE), BOOTTIME (ALARM)
        ns = mono();
        break;
    default:
        return sys2(228, clock, (long)ts);
    }
    ts->tv_sec = ns / 1000000000;
    ts->tv_nsec = ns % 1000000000;
    return 0;
}

int __vdso_gettimeofday(struct timeval *tv, void *tz) {
    if (!nova_vvar.ready || tz)
        return sys2(96, (long)tv, (long)tz);
    if (tv) {
        u64 us = (mono() + nova_vvar.real_offset) / 1000;
        tv->tv_sec = us / 1000000;
        tv->tv_usec = us % 1000000;
    }
    return 0;
}

i64 __vdso_time(i64 *t) {
    if (!nova_vvar.ready)
        return sys2(201, (long)t, 0);
    i64 s = (mono() + nova_vvar.real_offset) / 1000000000;
    if (t)
        *t = s;
    return s;
}

long __vdso_getcpu(unsigned *cpu, unsigned *node, void *cache) {
    return sys3(309, (long)cpu, (long)node, (long)cache);
}

int clock_gettime(int, struct timespec *) __attribute__((weak, alias("__vdso_clock_gettime")));
int gettimeofday(struct timeval *, void *) __attribute__((weak, alias("__vdso_gettimeofday")));
i64 time(i64 *) __attribute__((weak, alias("__vdso_time")));
long getcpu(unsigned *, unsigned *, void *) __attribute__((weak, alias("__vdso_getcpu")));
