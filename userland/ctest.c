/* Exercises the C runtime (glibc, static) on Nova's Linux system call layer.
 * Prints one line per check and a summary; exit status = number of failures. */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <setjmp.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <time.h>
#include <unistd.h>

static int passed, failed;
#define CHECK(name, cond) do { if (cond) { passed++; printf("  ok   %s\n", name); } \
    else { failed++; printf("  FAIL %s (errno %d)\n", name, errno); } } while (0)

static int cmp(const void *a, const void *b) { return *(const int *)a - *(const int *)b; }
static jmp_buf jb;
static void jump(void) { longjmp(jb, 42); }

int main(void) {
    printf("ctest: C runtime checks\n");

    /* Memory: small, many, large (mmap), realloc, calloc. */
    char *small = malloc(32);
    strcpy(small, "nova");
    CHECK("malloc + strcpy", small && strcmp(small, "nova") == 0);
    void *blocks[2000];
    for (int i = 0; i < 2000; i++) blocks[i] = malloc(16 + i * 7);
    int all = 1;
    for (int i = 0; i < 2000; i++) { if (!blocks[i]) all = 0; memset(blocks[i], i & 0xff, 16 + i * 7); }
    for (int i = 0; i < 2000; i += 2) free(blocks[i]);
    for (int i = 1; i < 2000; i += 2) free(blocks[i]);
    CHECK("2000 allocations", all);
    size_t big = 64u << 20;
    unsigned char *huge = malloc(big);
    if (huge) { huge[0] = 1; huge[big / 2] = 2; huge[big - 1] = 3; }
    CHECK("64 MiB malloc (mmap, demand paging)", huge && huge[0] + huge[big / 2] + huge[big - 1] == 6);
    free(huge);
    int *grown = NULL;
    for (int n = 1; n <= 100000; n *= 10) { grown = realloc(grown, n * sizeof(int)); grown[n - 1] = n; }
    CHECK("realloc growth", grown && grown[99999] == 100000);
    free(grown);
    int *zeros = calloc(1000, sizeof(int));
    int z = 1;
    for (int i = 0; i < 1000; i++) if (zeros[i]) z = 0;
    CHECK("calloc zeroed", z);
    free(zeros);

    /* Strings, formatting, numbers. */
    char buf[128];
    snprintf(buf, sizeof buf, "%d %s %.3f %x %5.1e", 42, "abc", 3.14159, 255, 12345.678);
    CHECK("snprintf with floats", strcmp(buf, "42 abc 3.142 ff 1.2e+04") == 0);
    CHECK("strtod", fabs(strtod("2.5e3", NULL) - 2500.0) < 1e-9);
    CHECK("libm sqrt/sin/pow", fabs(sqrt(2.0) - 1.41421356) < 1e-6 && fabs(sin(0.0)) < 1e-12 && pow(2, 10) == 1024);
    int arr[] = {5, 3, 9, 1, 7};
    qsort(arr, 5, sizeof(int), cmp);
    CHECK("qsort", arr[0] == 1 && arr[4] == 9);
    CHECK("strstr / strncmp", strstr("hello nova world", "nova") != NULL && strncmp("abcdef", "abcxyz", 3) == 0);

    /* Control flow. */
    int r = setjmp(jb);
    if (r == 0) jump();
    CHECK("setjmp/longjmp", r == 42);

    /* Time. */
    time_t t = time(NULL);
    CHECK("time() is a plausible date", t > 1700000000);
    struct timespec a, b, d = {0, 50 * 1000 * 1000};
    clock_gettime(CLOCK_MONOTONIC, &a);
    nanosleep(&d, NULL);
    clock_gettime(CLOCK_MONOTONIC, &b);
    long ms = (b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000;
    CHECK("nanosleep 50 ms", ms >= 40 && ms < 1000);

    /* Files on NovaFS. */
    FILE *f = fopen("/tmp/ctest.txt", "w");
    CHECK("fopen for writing", f != NULL);
    if (f) { fprintf(f, "line one\nline two %d\n", 2); fclose(f); }
    f = fopen("/tmp/ctest.txt", "r");
    char line[64] = {0};
    if (f) { fgets(line, sizeof line, f); fgets(line, sizeof line, f); fclose(f); }
    CHECK("read back what was written", strcmp(line, "line two 2\n") == 0);
    struct stat st;
    CHECK("stat size", stat("/tmp/ctest.txt", &st) == 0 && st.st_size == 20 && S_ISREG(st.st_mode));
    CHECK("rename", rename("/tmp/ctest.txt", "/tmp/ctest2.txt") == 0 && access("/tmp/ctest2.txt", F_OK) == 0);
    DIR *dir = opendir("/tmp");
    int found = 0;
    struct dirent *e;
    while (dir && (e = readdir(dir)))
        if (strcmp(e->d_name, "ctest2.txt") == 0) found = 1;
    if (dir) closedir(dir);
    CHECK("opendir/readdir", found);
    CHECK("unlink", unlink("/tmp/ctest2.txt") == 0 && access("/tmp/ctest2.txt", F_OK) != 0);
    errno = 0;
    CHECK("missing file gives ENOENT", fopen("/no/such/file", "r") == NULL && errno == ENOENT);
    int fd = open("/dev/urandom", O_RDONLY);
    unsigned char rnd[16] = {0};
    int nz = 0;
    if (fd >= 0) { read(fd, rnd, sizeof rnd); close(fd); }
    for (int i = 0; i < 16; i++) nz |= rnd[i];
    CHECK("/dev/urandom", fd >= 0 && nz);

    /* Process information. */
    struct utsname u;
    CHECK("uname", uname(&u) == 0 && strcmp(u.machine, "x86_64") == 0);
    char cwd[256];
    CHECK("getcwd", getcwd(cwd, sizeof cwd) && cwd[0] == '/');
    CHECK("getpid", getpid() > 0);

    /* The vDSO: clock_gettime and friends without a system call, on the kernel's clock. */
    CHECK("vDSO mapped (AT_SYSINFO_EHDR)", getauxval(AT_SYSINFO_EHDR) != 0);
    struct timespec k1, v, k2;
    syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &k1);
    clock_gettime(CLOCK_MONOTONIC, &v);
    syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &k2);
    long long nk1 = k1.tv_sec * 1000000000LL + k1.tv_nsec, nv = v.tv_sec * 1000000000LL + v.tv_nsec,
              nk2 = k2.tv_sec * 1000000000LL + k2.tv_nsec;
    CHECK("clock_gettime(MONOTONIC) between two system calls", nk1 <= nv && nv <= nk2);
    struct timespec rk, rv;
    syscall(SYS_clock_gettime, CLOCK_REALTIME, &rk);
    clock_gettime(CLOCK_REALTIME, &rv);
    long long dr = (rv.tv_sec - rk.tv_sec) * 1000000000LL + (rv.tv_nsec - rk.tv_nsec);
    CHECK("clock_gettime(REALTIME) matches the system call", dr >= 0 && dr < 1000000);
    struct timespec a0, a1, ts;
    clock_gettime(CLOCK_MONOTONIC, &a0);
    for (int i = 0; i < 1000000; i++) clock_gettime(CLOCK_MONOTONIC, &ts);
    clock_gettime(CLOCK_MONOTONIC, &a1);
    long long per = ((a1.tv_sec - a0.tv_sec) * 1000000000LL + (a1.tv_nsec - a0.tv_nsec)) / 1000000;
    printf("  clock_gettime: %lld ns per call\n", per);
    CHECK("clock_gettime under 500 ns (no system call)", per < 500);

    printf("ctest: %d passed, %d failed\n", passed, failed);
    return failed;
}
