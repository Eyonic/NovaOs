/* proctest: processes and signals as Linux programs use them (fork, exec, wait, kill,
 * sigaction, posix_spawn, pipes across fork, close-on-exec). Prints one line per check
 * and a summary; scripts/test.sh runs it in the "procs" case. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

static int passed, failed;

static void check(int ok, const char *what)
{
	printf("  %s %s\n", ok ? "ok  " : "FAIL", what);
	if (ok)
		passed++;
	else
		failed++;
}

static volatile sig_atomic_t usr1, usr2, chld, info_ok;

static void on_usr1(int sig)
{
	usr1 = sig;
}

static void on_usr2(int sig, siginfo_t *info, void *uc)
{
	usr2 = sig;
	info_ok = info && info->si_signo == SIGUSR2 && uc != NULL;
}

static void on_chld(int sig)
{
	chld++;
}

int main(int argc, char **argv)
{
	struct sigaction sa;
	sigset_t set, pending;
	int status, fds[2];
	pid_t pid;

	/* Re-executed by the close-on-exec check: report whether fd 3 survived exec. */
	if (argc > 1 && strcmp(argv[1], "fdcheck") == 0)
		return fcntl(atoi(argv[2]), F_GETFD) == -1 && errno == EBADF ? 0 : 1;

	printf("proctest: processes and signals\n");

	/* Signal handlers. */
	memset(&sa, 0, sizeof(sa));
	sa.sa_handler = on_usr1;
	sigaction(SIGUSR1, &sa, NULL);
	raise(SIGUSR1);
	check(usr1 == SIGUSR1, "raise() runs a sigaction handler");

	memset(&sa, 0, sizeof(sa));
	sa.sa_sigaction = on_usr2;
	sa.sa_flags = SA_SIGINFO;
	sigaction(SIGUSR2, &sa, NULL);
	kill(getpid(), SIGUSR2);
	check(usr2 == SIGUSR2 && info_ok, "kill(self) runs an SA_SIGINFO handler with siginfo");

	/* Blocking: pending while blocked, delivered on unblock. */
	usr1 = 0;
	sigemptyset(&set);
	sigaddset(&set, SIGUSR1);
	sigprocmask(SIG_BLOCK, &set, NULL);
	raise(SIGUSR1);
	sigpending(&pending);
	check(usr1 == 0 && sigismember(&pending, SIGUSR1), "a blocked signal stays pending");
	sigprocmask(SIG_UNBLOCK, &set, NULL);
	check(usr1 == SIGUSR1, "unblocking delivers it");

	/* fork + exit status. */
	pid = fork();
	if (pid == 0)
		_exit(42);
	check(pid > 0 && waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 42,
	      "fork(), child exits 42, waitpid() sees 42");

	/* The child has a copy of the memory. */
	{
		static int shared_value = 1;
		pid = fork();
		if (pid == 0) {
			shared_value = 2;
			_exit(shared_value);
		}
		waitpid(pid, &status, 0);
		check(shared_value == 1 && WEXITSTATUS(status) == 2, "the child's writes stay in the child");
	}

	/* A pipe across fork. */
	if (pipe(fds) == 0) {
		char buf[32] = { 0 };
		pid = fork();
		if (pid == 0) {
			close(fds[0]);
			write(fds[1], "through the pipe", 16);
			_exit(0);
		}
		close(fds[1]);
		read(fds[0], buf, sizeof(buf) - 1);
		close(fds[0]);
		waitpid(pid, &status, 0);
		check(strcmp(buf, "through the pipe") == 0, "a pipe carries data from child to parent");
	}

	/* fork + execve. */
	pid = fork();
	if (pid == 0) {
		char *args[] = { "hello", "from", "exec", NULL };
		execve("/bin/hello", args, environ);
		_exit(127);
	}
	check(pid > 0 && waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0,
	      "fork() + execve(/bin/hello)");

	/* posix_spawn (clone with CLONE_VM | CLONE_VFORK inside glibc). */
	{
		char *args[] = { "hello", "spawned", NULL };
		int rc = posix_spawn(&pid, "/bin/hello", NULL, NULL, args, environ);
		check(rc == 0 && waitpid(pid, &status, 0) == pid && WEXITSTATUS(status) == 0, "posix_spawn(/bin/hello)");
	}

	/* execve of a missing program fails in the child, not the parent. */
	pid = fork();
	if (pid == 0) {
		char *args[] = { "nope", NULL };
		execve("/bin/does-not-exist", args, environ);
		_exit(errno == ENOENT ? 7 : 8);
	}
	waitpid(pid, &status, 0);
	check(WEXITSTATUS(status) == 7, "execve of a missing file returns ENOENT");

	/* Close-on-exec. */
	{
		int fd = open("/bin/hello", O_RDONLY | O_CLOEXEC);
		char fdnum[16];
		snprintf(fdnum, sizeof(fdnum), "%d", fd);
		pid = fork();
		if (pid == 0) {
			char *args[] = { "proctest", "fdcheck", fdnum, NULL };
			execve("/bin/proctest", args, environ);
			_exit(127);
		}
		waitpid(pid, &status, 0);
		check(fd >= 0 && WIFEXITED(status) && WEXITSTATUS(status) == 0, "O_CLOEXEC descriptors are closed by execve");
		close(fd);
	}

	/* SIGCHLD, killing a child, WNOHANG. */
	memset(&sa, 0, sizeof(sa));
	sa.sa_handler = on_chld;
	sigaction(SIGCHLD, &sa, NULL);
	pid = fork();
	if (pid == 0) {
		for (;;)
			pause();
	}
	check(waitpid(pid, &status, WNOHANG) == 0, "WNOHANG returns 0 while the child runs");
	kill(pid, SIGTERM);
	check(waitpid(pid, &status, 0) == pid && WIFSIGNALED(status) && WTERMSIG(status) == SIGTERM,
	      "kill(child, SIGTERM): WIFSIGNALED, WTERMSIG == SIGTERM");
	check(chld > 0, "the parent got SIGCHLD");
	check(waitpid(-1, &status, 0) == -1 && errno == ECHILD, "no children left: ECHILD");

	printf("proctest: %d passed, %d failed\n", passed, failed);
	return failed != 0;
}
