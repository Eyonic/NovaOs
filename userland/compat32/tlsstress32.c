/* tlsstress32: 32-bit threads check, across preemption, that %gs still points at their
 * own thread-local storage and that their registers survive. */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>

#define THREADS 4

static __thread volatile uintptr_t mine;
static int bad[THREADS + 1];

static void *worker(void *arg)
{
	uintptr_t id = (uintptr_t)arg;
	pthread_t self = pthread_self();
	mine = id;
	volatile uint64_t acc = 0;
	for (uint32_t i = 0; i < 30000000u; i++) {
		acc += i ^ id;
		if ((i & 0xFFFF) == 0 && (mine != id || !pthread_equal(pthread_self(), self)))
			bad[id]++;
	}
	return (void *)(uintptr_t)(acc & 1);
}

int main(void)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	pthread_t t[THREADS];
	for (uintptr_t i = 0; i < THREADS; i++)
		pthread_create(&t[i], NULL, worker, (void *)(i + 1));
	for (int i = 0; i < THREADS; i++)
		pthread_join(t[i], NULL);
	int total = 0;
	for (int i = 1; i <= THREADS; i++)
		total += bad[i];
	printf("tlsstress32: %s (%d wrong-TLS observations)\n", total ? "TLS MIXED UP" : "TLS intact", total);
	return total != 0;
}
