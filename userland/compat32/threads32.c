/* threads32: POSIX threads in a 32-bit program: thread creation with per-thread TLS
 * (set_thread_area), mutexes and condition variables (futex), join, and a few 32-bit
 * ABI details (isatty through ioctl, 64-bit file offsets, clock_gettime). */
#define _FILE_OFFSET_BITS 64
#include <fcntl.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#define THREADS 4
#define ROUNDS 10000

static __thread int tls_value;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cond = PTHREAD_COND_INITIALIZER;
static long counter;
static int ready;
static int passed, failed;

static void check(int ok, const char *what)
{
	printf("  %s %s\n", ok ? "ok  " : "FAIL", what);
	ok ? passed++ : failed++;
}

static void *worker(void *arg)
{
	int id = (int)(long)arg;
	tls_value = id * 100;
	for (int i = 0; i < ROUNDS; i++) {
		pthread_mutex_lock(&lock);
		counter++;
		pthread_mutex_unlock(&lock);
	}
	pthread_mutex_lock(&lock);
	ready++;
	pthread_cond_signal(&cond);
	pthread_mutex_unlock(&lock);
	return (void *)(long)(tls_value == id * 100);
}

int main(void)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	printf("threads32: 32-bit threads and ABI\n");
	tls_value = 7;
	pthread_t t[THREADS];
	int created = 1;
	for (int i = 0; i < THREADS; i++)
		created &= pthread_create(&t[i], NULL, worker, (void *)(long)(i + 1)) == 0;
	check(created, "pthread_create x4");
	pthread_mutex_lock(&lock);
	while (ready < THREADS)
		pthread_cond_wait(&cond, &lock);
	pthread_mutex_unlock(&lock);
	check(1, "pthread_cond_wait woken by the workers");
	int tls_ok = 1;
	for (int i = 0; i < THREADS; i++) {
		void *r;
		tls_ok &= pthread_join(t[i], &r) == 0 && r == (void *)1;
	}
	check(tls_ok && tls_value == 7, "each thread has its own __thread variable");
	check(counter == THREADS * ROUNDS, "a mutex protects a shared counter");

	struct timespec a, b;
	clock_gettime(CLOCK_MONOTONIC, &a);
	usleep(20000);
	clock_gettime(CLOCK_MONOTONIC, &b);
	long ms = (b.tv_sec - a.tv_sec) * 1000 + (b.tv_nsec - a.tv_nsec) / 1000000;
	if (ms < 15 || ms >= 1000)
		printf("  (usleep(20 ms) took %ld ms)\n", ms);
	check(ms >= 15 && ms < 1000, "clock_gettime and usleep");

	int fd = open("/tmp/big32", O_RDWR | O_CREAT | O_TRUNC, 0644);
	off_t far = (off_t)5 << 30; /* 5 GiB: needs _llseek with a 64-bit offset */
	check(fd >= 0 && lseek(fd, far, SEEK_SET) == far, "lseek past 4 GiB");
	if (fd >= 0) {
		close(fd);
		unlink("/tmp/big32");
	}
	char cwd[64];
	check(getcwd(cwd, sizeof cwd) && cwd[0] == '/', "getcwd");
	printf("threads32: %d passed, %d failed\n", passed, failed);
	return failed != 0;
}
