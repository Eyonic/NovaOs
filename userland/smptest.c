// Multiprocessor checks for Nova: the CPU count every source reports, busy threads that
// really run in parallel (wall time, and the CPUs getcpu reports), and stress for the
// paths SMP makes racy: contended futexes, and mmap/munmap from many threads while others
// touch the same pages (a missing TLB shootdown shows up as a stale read).
//   smptest [expected-cpus]
#define _GNU_SOURCE
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/sysinfo.h>
#include <time.h>
#include <unistd.h>

static int passed, failed;
static void check(const char *name, int ok) {
    (ok ? passed++ : failed++);
    printf("  %s %s\n", ok ? "ok  " : "FAIL", name);
    fflush(stdout);
}

static double now(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

static int cpuinfo_count(void) {
    FILE *f = fopen("/proc/cpuinfo", "r");
    char line[256];
    int n = 0;
    if (!f) return -1;
    while (fgets(line, sizeof line, f))
        if (!strncmp(line, "processor", 9)) n++;
    fclose(f);
    return n;
}

// ---------------------------------------------------------------- parallel busy threads

static volatile unsigned long sink;
static unsigned long spin_work(unsigned long iterations) {
    unsigned long x = 1;
    for (unsigned long i = 0; i < iterations; i++) x = x * 6364136223846793005UL + 1442695040888963407UL;
    return x;
}

struct busy { unsigned long iterations; double start, end; unsigned long cpus; };

static void *busy_thread(void *arg) {
    struct busy *b = arg;
    b->start = now();
    // Work in slices, noting the CPU after each (a 64-bit set of CPU ids seen).
    for (int slice = 0; slice < 50; slice++) {
        sink += spin_work(b->iterations / 50);
        unsigned cpu = (unsigned)sched_getcpu();
        if (cpu < 64) b->cpus |= 1UL << cpu;
    }
    b->end = now();
    return NULL;
}

static void parallel_check(int n) {
    // Calibrate: work that takes about 300 ms alone.
    unsigned long it = 1000000;
    double t;
    for (;;) {
        double t0 = now();
        sink += spin_work(it);
        t = now() - t0;
        if (t > 0.05) break;
        it *= 4;
    }
    it = (unsigned long)(it * (0.3 / t));
    struct busy alone = { it };
    busy_thread(&alone);
    double single = alone.end - alone.start;

    pthread_t th[64];
    struct busy b[64];
    double t0 = now();
    for (int i = 0; i < n; i++) {
        b[i] = (struct busy){ it };
        pthread_create(&th[i], NULL, busy_thread, &b[i]);
    }
    for (int i = 0; i < n; i++) pthread_join(th[i], NULL);
    double wall = now() - t0;
    unsigned long cpus = 0;
    double last_start = 0, first_end = 1e30;
    for (int i = 0; i < n; i++) {
        cpus |= b[i].cpus;
        if (b[i].start > last_start) last_start = b[i].start;
        if (b[i].end < first_end) first_end = b[i].end;
    }
    double speedup = single * n / wall;
    printf("smptest: %d busy threads: %.0f ms alone, %.0f ms together, speedup %.2f, on %d CPUs, all running together for %.0f ms\n",
           n, single * 1000, wall * 1000, speedup, __builtin_popcountl(cpus), (first_end - last_start) * 1000);
    if (n > 1) {
        // Each thread alone needs `single`; in parallel they overlap almost completely.
        check("busy threads run in parallel (speedup above half the CPU count)", speedup > n * 0.5);
        check("busy threads ran on every CPU", __builtin_popcountl(cpus) == n);
        check("busy threads overlap in time", first_end - last_start > single * 0.5);
    }
}

// ---------------------------------------------------------------- futex stress

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static long counter, turn;
#define FUTEX_THREADS 8
#define FUTEX_ROUNDS 20000

static void *mutex_thread(void *arg) {
    (void)arg;
    for (int i = 0; i < FUTEX_ROUNDS; i++) {
        pthread_mutex_lock(&lock);
        long c = counter;
        if (i % 64 == 0) sched_yield(); // hold it a while: others block in futex_wait
        counter = c + 1;
        pthread_mutex_unlock(&lock);
    }
    return NULL;
}

// Ping-pong: thread k waits for turn % n == k, then passes it on.
static void *pingpong_thread(void *arg) {
    long k = (long)arg;
    for (int i = 0; i < 2000; i++) {
        pthread_mutex_lock(&lock);
        while (turn % 4 != k) pthread_cond_wait(&cond, &lock);
        turn++;
        pthread_cond_broadcast(&cond);
        pthread_mutex_unlock(&lock);
    }
    return NULL;
}

static void futex_check(void) {
    pthread_t th[FUTEX_THREADS];
    for (int i = 0; i < FUTEX_THREADS; i++) pthread_create(&th[i], NULL, mutex_thread, NULL);
    for (int i = 0; i < FUTEX_THREADS; i++) pthread_join(th[i], NULL);
    check("8 threads x 20000 contended mutex increments", counter == (long)FUTEX_THREADS * FUTEX_ROUNDS);
    double t0 = now();
    for (long i = 0; i < 4; i++) pthread_create(&th[i], NULL, pingpong_thread, (void *)i);
    for (int i = 0; i < 4; i++) pthread_join(th[i], NULL);
    printf("smptest: condition variable ping-pong, 8000 hand-overs in %.0f ms\n", (now() - t0) * 1000);
    check("4 threads pass a turn 8000 times (condition variable)", turn == 8000);
}

// ---------------------------------------------------------------- mmap/munmap stress

#define MAP_THREADS 6
#define MAP_ROUNDS 300
static atomic_int map_errors;

static void *map_thread(void *arg) {
    long id = (long)arg;
    for (int r = 0; r < MAP_ROUNDS; r++) {
        size_t len = (size_t)(1 + (r * 7 + id) % 16) * 4096;
        unsigned char *p = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (p == MAP_FAILED) { atomic_fetch_add(&map_errors, 1); continue; }
        for (size_t o = 0; o < len; o += 512) p[o] = (unsigned char)(id + r + o);
        for (size_t o = 0; o < len; o += 512)
            if (p[o] != (unsigned char)(id + r + o)) atomic_fetch_add(&map_errors, 1);
        if (r % 3 == 0) mprotect(p, len, PROT_READ);
        munmap(p, len);
    }
    return NULL;
}

// One page replaced again and again (MAP_FIXED) while a reader on another CPU keeps
// reading it: after the replacement, the reader must see the new page, never the old
// frame through a stale TLB entry. The old frame is given straight to a scratch page with
// other contents (Nova's page pool reuses the last freed page first), so a stale read
// shows.
static unsigned *volatile shared_page;
static atomic_int phase, reader_done, stale_reads;
static atomic_uint expect;

static void *reader_thread(void *arg) {
    (void)arg;
    unsigned seen = 0;
    while (!atomic_load(&reader_done)) {
        int ph = atomic_load(&phase);
        unsigned v = shared_page[0]; // warms this CPU's TLB with the current page
        if (ph % 2 == 0 && v != atomic_load(&expect) && atomic_load(&phase) == ph) atomic_fetch_add(&stale_reads, 1);
        seen += v;
    }
    sink += seen;
    return NULL;
}

static void remap_check(void) {
    unsigned *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    p[0] = 0x1000;
    atomic_store(&expect, 0x1000);
    shared_page = p;
    pthread_t reader;
    pthread_create(&reader, NULL, reader_thread, NULL);
    for (unsigned i = 1; i <= 2000; i++) {
        atomic_fetch_add(&phase, 1); // odd: changing
        unsigned *q = mmap(p, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
        if (q != p) { atomic_fetch_add(&map_errors, 1); break; }
        unsigned *scratch = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        scratch[0] = 0xdead0000 + i;
        q[0] = 0x1000 + i;
        atomic_store(&expect, 0x1000 + i);
        atomic_fetch_add(&phase, 1); // even: stable, the reader must see 0x1000 + i
        for (volatile int d = 0; d < 2000; d++) {}
        munmap(scratch, 4096);
    }
    atomic_store(&reader_done, 1);
    pthread_join(reader, NULL);
    printf("smptest: 2000 page replacements under a reader, %d stale reads\n", atomic_load(&stale_reads));
    check("a page replaced by mmap(MAP_FIXED) is never read stale on another CPU", atomic_load(&stale_reads) == 0);
}

static void map_check(void) {
    pthread_t th[MAP_THREADS];
    for (long i = 0; i < MAP_THREADS; i++) pthread_create(&th[i], NULL, map_thread, (void *)i);
    for (int i = 0; i < MAP_THREADS; i++) pthread_join(th[i], NULL);
    check("6 threads x 300 mmap/write/verify/mprotect/munmap", atomic_load(&map_errors) == 0);
    remap_check();
}

int main(int argc, char **argv) {
    int onln = (int)sysconf(_SC_NPROCESSORS_ONLN), conf = (int)sysconf(_SC_NPROCESSORS_CONF);
    cpu_set_t set;
    int affinity = sched_getaffinity(0, sizeof set, &set) == 0 ? CPU_COUNT(&set) : -1;
    int info = cpuinfo_count();
    printf("smptest: %d CPUs (sysconf online %d, configured %d, get_nprocs %d, affinity %d, /proc/cpuinfo %d)\n",
           onln, onln, conf, get_nprocs(), affinity, info);
    check("every source reports the same CPU count", onln == conf && onln == get_nprocs() && onln == affinity && onln == info);
    if (argc > 1) check("the expected CPU count", onln == atoi(argv[1]));
    parallel_check(onln > 64 ? 64 : onln);
    futex_check();
    map_check();
    printf("smptest: %d passed, %d failed\n", passed, failed);
    return failed != 0;
}
