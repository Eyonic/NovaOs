/* hello32: a 32-bit (i386) program, statically linked. */
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv)
{
	printf("Hello from a 32-bit program on Nova! (pointer size %zu, argc %d)\n", sizeof(void *), argc);
	for (int i = 1; i < argc; i++)
		printf("  argv[%d] = %s\n", i, argv[i]);
	printf("  HOME=%s\n", getenv("HOME") ? getenv("HOME") : "(unset)");
	return 0;
}
