/* The smallest Nova program: a statically linked C program using glibc. */
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    printf("Hello from C on Nova!\n");
    for (int i = 0; i < argc; i++)
        printf("  argv[%d] = %s\n", i, argv[i]);
    printf("  HOME=%s TERM=%s\n", getenv("HOME"), getenv("TERM"));
    return 0;
}
