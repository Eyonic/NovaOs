/* hello32-dyn: a 32-bit program linked against the 32-bit glibc (ld-linux.so.2). */
#include <math.h>
#include <stdio.h>

int main(void)
{
	volatile double x = 2.0;
	printf("Hello from a dynamically linked 32-bit program! sqrt(2) = %.6f\n", sqrt(x));
	return 0;
}
