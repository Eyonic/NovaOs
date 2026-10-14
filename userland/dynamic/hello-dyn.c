/* Like hello.c, but dynamically linked: runs through ld-linux and libc.so.6. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <link.h>
#include <stdio.h>

static int count_libs(struct dl_phdr_info *info, size_t size, void *data) {
    (void)size;
    if (info->dlpi_name && info->dlpi_name[0]) printf("  loaded %s\n", info->dlpi_name);
    ++*(int *)data;
    return 0;
}

int main(void) {
    printf("Hello from a dynamically linked C program on Nova!\n");
    int n = 0;
    dl_iterate_phdr(count_libs, &n);
    printf("  %d objects in the process\n", n);
    return 0;
}
