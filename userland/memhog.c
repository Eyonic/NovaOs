/* Allocates and touches memory until the system runs out: Nova must stop this program,
 * not panic. Prints progress every 256 MiB. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(void) {
    size_t chunk = 64 << 20, total = 0;
    for (;;) {
        char *p = malloc(chunk);
        if (!p) {
            printf("memhog: malloc failed after %zu MiB\n", total >> 20);
            return 1;
        }
        memset(p, 0xA5, chunk);
        total += chunk;
        if (total % (256 << 20) == 0) {
            printf("memhog: %zu MiB\n", total >> 20);
            fflush(stdout);
        }
    }
}
