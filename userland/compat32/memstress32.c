/* memstress32: a 32-bit program hammering the memory system calls glibc's allocator uses
 * (brk, mmap, munmap, mremap, madvise) from several threads, checking that every byte it
 * wrote comes back unchanged. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>

#define THREADS 4
#define SLOTS 256
#define ROUNDS 20000

static int bad;

static uint32_t rnd(uint32_t *s)
{
	*s ^= *s << 13;
	*s ^= *s >> 17;
	*s ^= *s << 5;
	return *s;
}

static void fill(unsigned char *p, size_t n, unsigned char tag)
{
	for (size_t i = 0; i < n; i += 61)
		p[i] = (unsigned char)(tag + i);
	if (n)
		p[n - 1] = tag ^ 0x5A;
}

/* Check a block filled with fill(p, full, tag), of which the first n bytes still exist. */
static int check_part(unsigned char *p, size_t n, size_t full, unsigned char tag)
{
	for (size_t i = 0; i < n; i += 61)
		if (i != full - 1 && p[i] != (unsigned char)(tag + i))
			return 0;
	return n < full || n == 0 || p[n - 1] == (tag ^ 0x5A);
}

static int check(unsigned char *p, size_t n, unsigned char tag)
{
	return check_part(p, n, n, tag);
}

static void *worker(void *arg)
{
	uint32_t seed = 0x9E3779B9u * (uint32_t)(uintptr_t)arg + 1;
	unsigned char *ptr[SLOTS] = {0};
	size_t len[SLOTS] = {0};
	unsigned char tag[SLOTS] = {0};
	for (int r = 0; r < ROUNDS; r++) {
		int i = rnd(&seed) % SLOTS;
		if (ptr[i] && !check(ptr[i], len[i], tag[i])) {
			__sync_fetch_and_add(&bad, 1);
			printf("  corrupted block %p (%zu bytes) in thread %d, round %d\n", (void *)ptr[i], len[i], (int)(uintptr_t)arg, r);
		}
		uint32_t what = rnd(&seed) % 10;
		/* Mostly small blocks (brk heap), some large ones (mmap, > 128 KiB). */
		size_t n = (rnd(&seed) % 16 == 0) ? 128 * 1024 + rnd(&seed) % (2 << 20) : rnd(&seed) % 4096;
		if (what < 4 || !ptr[i]) {
			free(ptr[i]);
			ptr[i] = malloc(n);
			len[i] = n;
		} else if (what < 8) {
			ptr[i] = realloc(ptr[i], n);
			if (!check_part(ptr[i], n < len[i] ? n : len[i], len[i], tag[i])) {
				__sync_fetch_and_add(&bad, 1);
				printf("  realloc lost data (%zu bytes kept)\n", len[i]);
			}
			len[i] = n;
		} else {
			free(ptr[i]);
			ptr[i] = NULL;
			len[i] = 0;
			continue;
		}
		tag[i] = (unsigned char)rnd(&seed);
		fill(ptr[i], len[i], tag[i]);
	}
	for (int i = 0; i < SLOTS; i++)
		free(ptr[i]);
	return NULL;
}

int main(int argc, char **argv)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	if (argc > 1 && argv[1][0] == 'm') {
		/* "main": the same work on the main thread (glibc's brk heap, no thread arenas). */
		worker((void *)1);
		printf("memstress32: main thread: %s\n", bad ? "CORRUPTION" : "all data intact");
		return bad != 0;
	}
	int threads = argc > 1 ? atoi(argv[1]) : THREADS;
	if (threads < 1 || threads > 16)
		threads = THREADS;
	printf("memstress32: %d threads x %d rounds of malloc/realloc/free\n", threads, ROUNDS);
	pthread_t t[16];
	for (int i = 0; i < threads; i++)
		pthread_create(&t[i], NULL, worker, (void *)(uintptr_t)(i + 1));
	for (int i = 0; i < threads; i++)
		pthread_join(t[i], NULL);
	/* mmap / madvise / munmap directly. */
	for (int r = 0; r < 200; r++) {
		size_t n = (1 + r % 37) * 4096;
		unsigned char *p = mmap(NULL, n, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		memset(p, 0xAB, n);
		madvise(p + 4096, n > 8192 ? n - 8192 : 0, MADV_DONTNEED);
		if (p[0] != 0xAB || p[n - 1] != 0xAB || (n > 8192 && p[4096] != 0)) {
			bad++;
			printf("  madvise dropped the wrong pages (size %zu)\n", n);
		}
		munmap(p, n);
	}
	printf("memstress32: %s\n", bad ? "CORRUPTION" : "all data intact");
	return bad != 0;
}
