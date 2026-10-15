/* unixtest: named AF_UNIX sockets, as X11 and Wayland use them: bind to a path or an
 * abstract name, listen, connect from another process, accept, pass a descriptor. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static int passed, failed;

static void check(int ok, const char *what)
{
	printf("  %s %s\n", ok ? "ok  " : "FAIL", what);
	ok ? passed++ : failed++;
}

static socklen_t addr(struct sockaddr_un *sa, const char *path, int abstract)
{
	memset(sa, 0, sizeof *sa);
	sa->sun_family = AF_UNIX;
	if (abstract) {
		memcpy(sa->sun_path + 1, path, strlen(path));
		return offsetof(struct sockaddr_un, sun_path) + 1 + strlen(path);
	}
	strcpy(sa->sun_path, path);
	return sizeof *sa;
}

static int client(const char *path, int abstract)
{
	struct sockaddr_un sa;
	socklen_t len = addr(&sa, path, abstract);
	int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
	if (connect(fd, (struct sockaddr *)&sa, len) != 0) {
		close(fd);
		return -errno;
	}
	return fd;
}

static int server(const char *path, int abstract)
{
	struct sockaddr_un sa;
	socklen_t len = addr(&sa, path, abstract);
	int fd = socket(AF_UNIX, SOCK_STREAM, 0);
	if (bind(fd, (struct sockaddr *)&sa, len) != 0 || listen(fd, 8) != 0) {
		close(fd);
		return -errno;
	}
	return fd;
}

int main(void)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	printf("unixtest: named Unix sockets\n");
	mkdir("/run/sock", 0755);
	const char *path = "/run/sock/s0";
	int srv = server(path, 0);
	struct stat st;
	check(srv >= 0 && stat(path, &st) == 0, "bind a path, listen: the socket file exists");
	check(server(path, 0) == -EADDRINUSE, "binding the same path again: EADDRINUSE");
	check(client("/run/sock/none", 0) == -ENOENT, "connect to a missing path: ENOENT");

	struct pollfd p = {srv, POLLIN, 0};
	check(poll(&p, 1, 0) == 0, "no pending connection: the listener is not readable");

	pid_t pid = fork();
	if (pid == 0) {
		int c = client(path, 0);
		if (c < 0)
			_exit(1);
		write(c, "hello server", 12);
		char buf[32] = {0};
		read(c, buf, sizeof buf - 1);
		/* The server sent a descriptor of a file; read it through that. */
		_exit(strcmp(buf, "hello client") == 0 ? 0 : 2);
	}
	check(poll(&p, 1, 5000) == 1 && (p.revents & POLLIN), "a connection makes the listener readable");
	int conn = accept4(srv, NULL, NULL, SOCK_CLOEXEC);
	char buf[32] = {0};
	check(conn >= 0 && read(conn, buf, sizeof buf - 1) == 12 && strcmp(buf, "hello server") == 0, "accept, then read what the client wrote");
	write(conn, "hello client", 12);
	int status;
	check(waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0, "the client read the reply");
	close(conn);

	/* Descriptor passing over an accepted connection. */
	int c = client(path, 0);
	conn = accept(srv, NULL, NULL);
	int pfd[2];
	pipe(pfd);
	char cbuf[CMSG_SPACE(sizeof(int))] = {0};
	struct iovec iov = {"x", 1};
	struct msghdr m = {.msg_iov = &iov, .msg_iovlen = 1, .msg_control = cbuf, .msg_controllen = sizeof cbuf};
	struct cmsghdr *cm = CMSG_FIRSTHDR(&m);
	cm->cmsg_level = SOL_SOCKET;
	cm->cmsg_type = SCM_RIGHTS;
	cm->cmsg_len = CMSG_LEN(sizeof(int));
	memcpy(CMSG_DATA(cm), &pfd[1], sizeof(int));
	sendmsg(conn, &m, 0);
	char rbuf[CMSG_SPACE(sizeof(int))];
	char x;
	struct iovec riov = {&x, 1};
	struct msghdr rm = {.msg_iov = &riov, .msg_iovlen = 1, .msg_control = rbuf, .msg_controllen = sizeof rbuf};
	int got = -1;
	if (recvmsg(c, &rm, 0) == 1 && CMSG_FIRSTHDR(&rm))
		memcpy(&got, CMSG_DATA(CMSG_FIRSTHDR(&rm)), sizeof(int));
	write(got, "via fd", 6);
	char pbuf[8] = {0};
	check(got >= 0 && read(pfd[0], pbuf, 6) == 6 && strcmp(pbuf, "via fd") == 0, "SCM_RIGHTS over a named socket");
	close(c);
	close(conn);

	int ab = server("nova-test-abstract", 1);
	c = client("nova-test-abstract", 1);
	conn = accept(ab, NULL, NULL);
	write(c, "abs", 3);
	memset(buf, 0, sizeof buf);
	check(ab >= 0 && c >= 0 && read(conn, buf, 3) == 3 && strcmp(buf, "abs") == 0, "abstract socket names");
	struct sockaddr_un name;
	socklen_t nlen = sizeof name;
	check(getsockname(srv, (struct sockaddr *)&name, &nlen) == 0 && strcmp(name.sun_path, path) == 0, "getsockname of the listener");

	int nb = socket(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0);
	struct sockaddr_un sa;
	bind(nb, (struct sockaddr *)&sa, addr(&sa, "/run/sock/nb", 0));
	listen(nb, 1);
	check(accept(nb, NULL, NULL) == -1 && errno == EAGAIN, "non-blocking accept with nothing pending: EAGAIN");

	close(srv);
	check(client(path, 0) == -ECONNREFUSED, "after the listener closes: ECONNREFUSED");
	unlink(path);
	check(server(path, 0) >= 0, "the path can be bound again after unlink");
	printf("unixtest: %d passed, %d failed\n", passed, failed);
	return failed != 0;
}
