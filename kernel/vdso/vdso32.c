// Nova's vDSO for 32-bit programs: the same clock as vdso.c, with i386 arithmetic (no
// 128-bit products, no libgcc: split multiplications and the CPU's 64/32 division).

typedef unsigned long long u64;
typedef unsigned int u32;
typedef long long i64;
typedef int i32;

struct vvar {
    u64 tsc_base, base_ns, mult, real_offset, ready;
};
extern const volatile struct vvar nova_vvar __attribute__((visibility("hidden")));

struct timespec64 { i64 tv_sec; i64 tv_nsec; };
struct timespec32 { i32 tv_sec; i32 tv_nsec; };
struct timeval32 { i32 tv_sec; i32 tv_usec; };

static inline u64 rdtsc(void) {
    u64 v;
    __asm__ volatile("rdtsc" : "=A"(v));
    return v;
}

static inline u64 mul32x32(u32 a, u32 b) {
    return (u64)a * b;
}

// (d * mult) >> 32 without a 128-bit product.
static inline u64 scale(u64 d, u64 m) {
    u32 dl = (u32)d, dh = (u32)(d >> 32), ml = (u32)m, mh = (u32)(m >> 32);
    return ((u64)dh * m) + mul32x32(dl, mh) + (mul32x32(dl, ml) >> 32);
}

static inline u64 mono(void) {
    return nova_vvar.base_ns + scale(rdtsc() - nova_vvar.tsc_base, nova_vvar.mult);
}

// n / d and n % d for a 32-bit divisor: two 64/32 divisions, each quotient fitting 32 bits.
static inline u64 divmod(u64 n, u32 d, u32 *rem) {
    u32 hi = (u32)(n >> 32), lo = (u32)n, qh, ql, r;
    qh = hi / d;
    r = hi % d;
    __asm__("divl %4" : "=a"(ql), "=d"(r) : "a"(lo), "d"(r), "rm"(d));
    *rem = r;
    return (u64)qh << 32 | ql;
}

static inline long sys2(long nr, long a, long b) {
    long r;
    __asm__ volatile("int $0x80" : "=a"(r) : "a"(nr), "b"(a), "c"(b) : "memory");
    return r;
}

static inline long sys3(long nr, long a, long b, long c) {
    long r;
    __asm__ volatile("int $0x80" : "=a"(r) : "a"(nr), "b"(a), "c"(b), "d"(c) : "memory");
    return r;
}

// Nanoseconds for `clock`, or 0 if the vDSO does not serve it.
static inline int now(int clock, u64 *ns) {
    if (!nova_vvar.ready)
        return 0;
    switch (clock) {
    case 0: case 5: case 8: case 11:
        *ns = mono() + nova_vvar.real_offset;
        return 1;
    case 1: case 4: case 6: case 7: case 9:
        *ns = mono();
        return 1;
    }
    return 0;
}

int __vdso_clock_gettime64(int clock, struct timespec64 *ts) {
    u64 ns;
    u32 rem;
    if (!now(clock, &ns))
        return sys2(403, clock, (long)ts);
    ts->tv_sec = divmod(ns, 1000000000, &rem);
    ts->tv_nsec = rem;
    return 0;
}

int __vdso_clock_gettime(int clock, struct timespec32 *ts) {
    u64 ns;
    u32 rem;
    if (!now(clock, &ns))
        return sys2(265, clock, (long)ts);
    ts->tv_sec = (i32)divmod(ns, 1000000000, &rem);
    ts->tv_nsec = rem;
    return 0;
}

int __vdso_gettimeofday(struct timeval32 *tv, void *tz) {
    u64 ns;
    u32 rem;
    if (tz || !now(0, &ns))
        return sys2(78, (long)tv, (long)tz);
    if (tv) {
        tv->tv_sec = (i32)divmod(ns, 1000000000, &rem);
        tv->tv_usec = rem / 1000;
    }
    return 0;
}

i32 __vdso_time(i32 *t) {
    u64 ns;
    u32 rem;
    if (!now(0, &ns))
        return sys2(13, (long)t, 0);
    i32 s = (i32)divmod(ns, 1000000000, &rem);
    if (t)
        *t = s;
    return s;
}

long __vdso_getcpu(unsigned *cpu, unsigned *node, void *cache) {
    return sys3(318, (long)cpu, (long)node, (long)cache);
}

// The system call entry glibc's i386 port calls through (AT_SYSINFO, %gs:0x10) once a
// vDSO is present: the plain int 0x80.
__asm__(".globl __kernel_vsyscall\n"
        ".type __kernel_vsyscall, @function\n"
        "__kernel_vsyscall:\n"
        "    int $0x80\n"
        "    ret\n"
        ".size __kernel_vsyscall, . - __kernel_vsyscall\n");
