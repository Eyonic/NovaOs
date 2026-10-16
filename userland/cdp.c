// cdp: a minimal Chrome DevTools Protocol client (debugging Chromium-based programs such
// as Steam's web helper inside Nova).
//   cdp PORT /devtools/page/ID '{"id":1,"method":"Runtime.evaluate","params":{...}}' ...
// Opens a WebSocket to 127.0.0.1:PORT, sends each message, and prints every text frame
// that arrives until the connection has been quiet for three seconds.
#include <arpa/inet.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int send_all(int fd, const void *p, size_t n) {
    const char *c = p;
    while (n) {
        ssize_t w = write(fd, c, n);
        if (w <= 0) return -1;
        c += w;
        n -= (size_t)w;
    }
    return 0;
}

static int read_full(int fd, void *p, size_t n) {
    char *c = p;
    while (n) {
        ssize_t r = read(fd, c, n);
        if (r <= 0) return -1;
        c += r;
        n -= (size_t)r;
    }
    return 0;
}

// Send one masked text frame (clients must mask; a zero mask keeps it simple).
static int send_text(int fd, const char *msg) {
    size_t n = strlen(msg);
    unsigned char h[14];
    size_t hl = 0;
    h[hl++] = 0x81;
    if (n < 126) {
        h[hl++] = 0x80 | (unsigned char)n;
    } else if (n < 65536) {
        h[hl++] = 0x80 | 126;
        h[hl++] = (unsigned char)(n >> 8);
        h[hl++] = (unsigned char)n;
    } else {
        h[hl++] = 0x80 | 127;
        for (int i = 7; i >= 0; i--) h[hl++] = (unsigned char)((unsigned long long)n >> (i * 8));
    }
    memset(h + hl, 0, 4);
    hl += 4;
    return send_all(fd, h, hl) || send_all(fd, msg, n);
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: cdp PORT PATH [JSON...]\n");
        return 2;
    }
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in sa = {.sin_family = AF_INET, .sin_port = htons((unsigned short)atoi(argv[1]))};
    sa.sin_addr.s_addr = htonl(0x7f000001);
    if (connect(fd, (struct sockaddr *)&sa, sizeof sa) < 0) {
        perror("cdp: connect");
        return 1;
    }
    char req[1024];
    snprintf(req, sizeof req,
             "GET %s HTTP/1.1\r\nHost: 127.0.0.1:%s\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
             "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
             argv[2], argv[1]);
    send_all(fd, req, strlen(req));
    // Skip the HTTP response headers.
    char last4[4] = {0};
    for (;;) {
        char c;
        if (read(fd, &c, 1) != 1) {
            fprintf(stderr, "cdp: handshake failed\n");
            return 1;
        }
        memmove(last4, last4 + 1, 3);
        last4[3] = c;
        if (!memcmp(last4, "\r\n\r\n", 4)) break;
    }
    for (int i = 3; i < argc; i++) send_text(fd, argv[i]);
    for (;;) {
        struct pollfd p = {.fd = fd, .events = POLLIN};
        if (poll(&p, 1, 3000) <= 0) break;
        unsigned char h[2];
        if (read_full(fd, h, 2)) break;
        unsigned long long n = h[1] & 0x7f;
        if (n == 126) {
            unsigned char e[2];
            if (read_full(fd, e, 2)) break;
            n = (unsigned long long)e[0] << 8 | e[1];
        } else if (n == 127) {
            unsigned char e[8];
            if (read_full(fd, e, 8)) break;
            n = 0;
            for (int i = 0; i < 8; i++) n = n << 8 | e[i];
        }
        char *buf = malloc(n + 1);
        if (!buf || read_full(fd, buf, n)) break;
        buf[n] = 0;
        if ((h[0] & 0x0f) == 1) printf("%s\n", buf);
        free(buf);
        fflush(stdout);
    }
    return 0;
}
