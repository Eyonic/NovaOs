// lsof: the part of lsof(8) that Linux programs use to find who owns a local TCP
// connection (Steam checks that its UI's websocket comes from its own web helper):
//   lsof [-P] [-F fields] -i [TCP]@host:port
// Lists the loopback TCP sockets whose local or remote address matches, from Nova's
// /proc/nova/tcp ("pid ppid uid fd local remote state" per line). With -F the output
// uses lsof's field format (p pid, R parent, u uid per process; f fd, n name per file);
// without it, a short table. Exits with 1 when nothing matches, as lsof does.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

int main(int argc, char **argv) {
    const char *spec = NULL;
    const char *fields = NULL;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "-i") && i + 1 < argc) spec = argv[++i];
        else if (!strncmp(argv[i], "-i", 2) && argv[i][2]) spec = argv[i] + 2;
        else if (!strcmp(argv[i], "-F") && i + 1 < argc) fields = argv[++i];
        else if (!strncmp(argv[i], "-F", 2)) fields = argv[i][2] ? argv[i] + 2 : "";
    }
    // "TCP@127.0.0.1:32771", "@localhost:80", "TCP:32771" (port only).
    char host[64] = "", port[16] = "";
    if (spec) {
        const char *at = strchr(spec, '@');
        const char *addr = at ? at + 1 : (strchr(spec, ':') ? strchr(spec, ':') : "");
        if (*addr == ':') {
            snprintf(port, sizeof port, "%s", addr + 1);
        } else {
            const char *colon = strrchr(addr, ':');
            if (colon) {
                snprintf(host, sizeof host, "%.*s", (int)(colon - addr), addr);
                snprintf(port, sizeof port, "%s", colon + 1);
            } else {
                snprintf(host, sizeof host, "%s", addr);
            }
        }
        if (!strcmp(host, "localhost")) strcpy(host, "127.0.0.1");
    }
    FILE *f = fopen("/proc/nova/tcp", "r");
    if (!f) return 1;
    char line[256];
    int matched = 0;
    long last_pid = -1;
    if (!fields) printf("COMMAND   PID USER   FD   TYPE NODE NAME\n");
    while (fgets(line, sizeof line, f)) {
        long pid, ppid, uid, fd;
        char local[64], remote[64], state[32];
        if (sscanf(line, "%ld %ld %ld %ld %63s %63s %31s", &pid, &ppid, &uid, &fd, local, remote, state) != 7) continue;
        int hit = !spec;
        for (int side = 0; side < 2 && !hit; side++) {
            const char *a = side ? remote : local;
            const char *colon = strrchr(a, ':');
            if (!colon) continue;
            int host_ok = !*host || (!strncmp(a, host, (size_t)(colon - a)) && strlen(host) == (size_t)(colon - a)) || !strncmp(a, "0.0.0.0", 7);
            int port_ok = !*port || !strcmp(colon + 1, port);
            hit = host_ok && port_ok;
        }
        if (!hit) continue;
        matched++;
        char name[160];
        if (!strcmp(state, "LISTEN")) snprintf(name, sizeof name, "%s", local);
        else snprintf(name, sizeof name, "%s->%s", local, remote);
        if (fields) {
            if (pid != last_pid) {
                printf("p%ld\n", pid);
                if (strchr(fields, 'R')) printf("R%ld\n", ppid);
                if (strchr(fields, 'u')) printf("u%ld\n", uid);
                last_pid = pid;
            }
            printf("f%ld\n", fd);
            if (strchr(fields, 'n')) printf("n%s\n", name);
        } else {
            printf("?       %5ld %-5ld %3ldu  IPv4 TCP  %s (%s)\n", pid, uid, fd, name, state);
        }
    }
    fclose(f);
    return matched ? 0 : 1;
}
