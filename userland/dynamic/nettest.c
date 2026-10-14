/* Networking through Nova's Linux socket layer: DNS (glibc getaddrinfo over UDP)
 * and an HTTP request over TCP. Usage: nettest [host[:port]] */
#include <netdb.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#include <arpa/inet.h>

int main(int argc, char **argv) {
    char host[256] = "example.com", port[16] = "80";
    if (argc > 1) {
        snprintf(host, sizeof host, "%s", argv[1]);
        char *colon = strchr(host, ':');
        if (colon) { *colon = 0; snprintf(port, sizeof port, "%s", colon + 1); }
    }
    struct addrinfo hints = {0}, *res = NULL;
    hints.ai_family = AF_INET;
    hints.ai_socktype = SOCK_STREAM;
    int rc = getaddrinfo(host, port, &hints, &res);
    if (rc != 0) { printf("nettest: getaddrinfo(%s): %s\n", host, gai_strerror(rc)); return 1; }
    char ip[64];
    inet_ntop(AF_INET, &((struct sockaddr_in *)res->ai_addr)->sin_addr, ip, sizeof ip);
    printf("nettest: %s resolves to %s\n", host, ip);
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    if (connect(fd, res->ai_addr, res->ai_addrlen) != 0) { perror("nettest: connect"); return 1; }
    printf("nettest: connected\n");
    char req[256];
    snprintf(req, sizeof req, "GET / HTTP/1.1\r\nHost: %s\r\nConnection: close\r\n\r\n", host);
    write(fd, req, strlen(req));
    /* MSG_PEEK must leave the data queued (Chromium peeks to check idle connections). */
    char peeked[8] = {0};
    ssize_t p = recv(fd, peeked, 5, MSG_PEEK);
    char buf[4096];
    long total = 0;
    ssize_t n;
    char first[128] = {0};
    while ((n = read(fd, buf, sizeof buf)) > 0) {
        if (total == 0) {
            printf("nettest: MSG_PEEK %s\n", p == 5 && n >= 5 && memcmp(peeked, buf, 5) == 0 ? "kept the data" : "LOST DATA");
            memcpy(first, buf, n < 127 ? n : 127); char *e = strchr(first, '\r'); if (e) *e = 0;
        }
        total += n;
    }
    close(fd);
    freeaddrinfo(res);
    printf("nettest: %s, %ld bytes\n", first, total);
    return 0;
}
