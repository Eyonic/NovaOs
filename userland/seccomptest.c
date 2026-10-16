/* seccomptest: seccomp as sandboxes use it (prctl(PR_SET_NO_NEW_PRIVS), seccomp(2),
 * classic BPF filters with ERRNO, TRAP, KILL actions, TSYNC, inheritance over fork and
 * execve, strict mode). Every filter goes into a forked child, so each check starts
 * clean. Prints one line per check and a summary; scripts/test.sh runs it in the
 * "seccomp" case. */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <signal.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>

static int passed, failed;

static void check(int ok, const char *what)
{
	printf("  %s %s\n", ok ? "ok  " : "FAIL", what);
	fflush(stdout);
	if (ok)
		passed++;
	else
		failed++;
}

#define NR_LOAD BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr))
#define ALLOW BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW)
/* if (nr == n) return ret; (the accumulator keeps nr) */
#define ON(n, ret) BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, (n), 0, 1), BPF_STMT(BPF_RET | BPF_K, (ret))

static long sc(long nr, long a, long b, long c, long d, long e)
{
	return syscall(nr, a, b, c, d, e, 0);
}

static int install(struct sock_filter *f, unsigned short len, unsigned flags)
{
	struct sock_fprog prog = { .len = len, .filter = f };
	return sc(SYS_seccomp, SECCOMP_SET_MODE_FILTER, flags, (long)&prog, 0, 0);
}

/* ERRNO(77) for getppid. */
static int block_getppid(unsigned flags)
{
	struct sock_filter f[] = { NR_LOAD, ON(SYS_getppid, SECCOMP_RET_ERRNO | 77), ALLOW };
	return install(f, sizeof(f) / sizeof(f[0]), flags);
}

static int getppid_blocked(void)
{
	errno = 0;
	return syscall(SYS_getppid) == -1 && errno == 77;
}

/* Fork; the child runs fn and exits with its result. Returns the wait status. */
static int in_child(int (*fn)(void))
{
	fflush(stdout);
	pid_t pid = fork();
	if (pid == 0)
		_exit(fn());
	int status = 0;
	waitpid(pid, &status, 0);
	return status;
}

static int child_ok(int (*fn)(void))
{
	int st = in_child(fn);
	if (WIFEXITED(st) && WEXITSTATUS(st) == 0)
		return 1;
	printf("  (child: exit status %d, signal %d)\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1, WIFSIGNALED(st) ? WTERMSIG(st) : 0);
	return 0;
}

static int status_has(const char *line)
{
	char buf[4096];
	FILE *f = fopen("/proc/self/status", "r");
	if (!f)
		return 0;
	size_t n = fread(buf, 1, sizeof(buf) - 1, f);
	fclose(f);
	buf[n] = 0;
	return strstr(buf, line) != NULL;
}

/* ---- the checks; each returns 0 on success */

static int t_errno(void)
{
	if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || block_getppid(0))
		return 1;
	/* An argument filter: fcntl(fd, F_GETFD) fails with EPERM, other fcntls work. */
	struct sock_filter f[] = {
		NR_LOAD,
		BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_fcntl, 0, 3),
		BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, args[1])),
		BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, F_GETFD, 0, 1),
		BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
		ALLOW,
	};
	if (install(f, sizeof(f) / sizeof(f[0]), 0))
		return 2;
	if (!getppid_blocked() || getpid() <= 0)
		return 3;
	errno = 0;
	if (fcntl(1, F_GETFD) != -1 || errno != EPERM || fcntl(1, F_GETFL) == -1)
		return 4;
	if (!status_has("NoNewPrivs:\t1") || !status_has("Seccomp:\t2") || !status_has("Seccomp_filters:\t2"))
		return 5;
	if (prctl(PR_GET_SECCOMP) != 2)
		return 6;
	return 0;
}

static volatile int trap_ok;

static void on_sigsys(int sig, siginfo_t *info, void *ucv)
{
	ucontext_t *uc = ucv;
	trap_ok = info->si_code == 1 /* SYS_SECCOMP */ && info->si_syscall == SYS_getuid && info->si_arch == AUDIT_ARCH_X86_64 &&
		  info->si_errno == 7 && info->si_call_addr == (void *)uc->uc_mcontext.gregs[REG_RIP] &&
		  uc->uc_mcontext.gregs[REG_RAX] == SYS_getuid && uc->uc_mcontext.gregs[REG_RDI] == 11;
	/* Emulate the call, as Chromium's sandbox does: the result goes into rax. */
	uc->uc_mcontext.gregs[REG_RAX] = 4242;
}

static int t_trap(void)
{
	struct sigaction sa = { 0 };
	sa.sa_sigaction = on_sigsys;
	sa.sa_flags = SA_SIGINFO | SA_NODEFER;
	sigaction(SIGSYS, &sa, NULL);
	struct sock_filter f[] = { NR_LOAD, ON(SYS_getuid, SECCOMP_RET_TRAP | 7), ALLOW };
	if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || install(f, 4, 0))
		return 1;
	long r = sc(SYS_getuid, 11, 22, 33, 44, 55);
	return r == 4242 && trap_ok ? 0 : 2;
}

static int t_kill_process(void)
{
	struct sock_filter f[] = { NR_LOAD, ON(SYS_getppid, SECCOMP_RET_KILL_PROCESS), ALLOW };
	if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || install(f, 4, 0))
		return 1;
	syscall(SYS_getppid);
	return 2; /* not reached */
}

static int t_kill_thread(void)
{
	/* KILL_THREAD in the only thread ends the process with SIGSYS too; also checks that
	 * the most severe of two filters wins (ERRNO from the newer one loses to KILL). */
	struct sock_filter f[] = { NR_LOAD, ON(SYS_getppid, SECCOMP_RET_KILL_THREAD), ALLOW };
	if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || install(f, 4, 0) || block_getppid(0))
		return 1;
	syscall(SYS_getppid);
	return 2;
}

/* TSYNC: a worker thread created before the filter gets it. */
static int go_pipe[2], done_pipe[2];

static void *worker(void *arg)
{
	char c;
	for (;;) {
		if (read(go_pipe[0], &c, 1) != 1)
			return NULL;
		int r;
		if (c == 'a') /* before TSYNC: the main thread's non-synced filter is not ours */
			r = syscall(SYS_getpgrp) != -1;
		else if (c == 'b') /* after: the whole stack, synced */
			r = getppid_blocked() && syscall(SYS_getpgrp) == -1 && errno == 78;
		else
			return NULL;
		c = r ? 'y' : 'n';
		write(done_pipe[1], &c, 1);
	}
}

static int ask(char what)
{
	char c;
	write(go_pipe[1], &what, 1);
	return read(done_pipe[0], &c, 1) == 1 && c == 'y';
}

static int own_filter_ready[2];

static void *loner(void *arg)
{
	char c = 'r';
	struct sock_filter f[] = { NR_LOAD, ON(SYS_getsid, SECCOMP_RET_ERRNO | 1), ALLOW };
	install(f, 4, 0);
	write(own_filter_ready[1], &c, 1);
	pause();
	return NULL;
}

static int t_tsync(void)
{
	pthread_t t, t2;
	if (pipe(go_pipe) || pipe(done_pipe) || pipe(own_filter_ready) || prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0))
		return 1;
	pthread_create(&t, NULL, worker, NULL);
	struct sock_filter f[] = { NR_LOAD, ON(SYS_getpgrp, SECCOMP_RET_ERRNO | 78), ALLOW };
	if (install(f, 4, 0))
		return 2;
	if (!ask('a'))
		return 3;
	if (block_getppid(SECCOMP_FILTER_FLAG_TSYNC) != 0)
		return 4;
	if (!ask('b'))
		return 5;
	/* A thread with a filter of its own blocks TSYNC: the call returns its tid. */
	char c;
	pthread_create(&t2, NULL, loner, NULL);
	read(own_filter_ready[0], &c, 1);
	long r = block_getppid(SECCOMP_FILTER_FLAG_TSYNC);
	if (r <= 0 || r == getpid())
		return 6;
	return 0;
}

static char *self;

static int t_fork_exec(void)
{
	if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || block_getppid(0))
		return 1;
	int st = in_child(getppid_blocked);
	if (!WIFEXITED(st) || WEXITSTATUS(st) != 1)
		return 2; /* getppid_blocked returns 1 when blocked */
	execl(self, self, "execcheck", (char *)NULL);
	return 3;
}

static int exec_check(void)
{
	return getppid_blocked() && status_has("Seccomp:\t2") && status_has("NoNewPrivs:\t1") ? 0 : 1;
}

static int t_strict(void)
{
	static const char msg[] = "  ok   strict mode: write() still works\n";
	if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_STRICT))
		return 1;
	write(1, msg, sizeof(msg) - 1);
	syscall(SYS_exit, 0); /* exit_group is not allowed in strict mode */
	return 2;
}

static int t_strict_violation(void)
{
	if (sc(SYS_seccomp, SECCOMP_SET_MODE_STRICT, 0, 0, 0, 0))
		return 1;
	syscall(SYS_getpid);
	syscall(SYS_exit, 2);
	return 2;
}

static int t_validation(void)
{
	/* Without no_new_privs (and without CAP_SYS_ADMIN) a filter is refused. */
	struct sock_filter ok[] = { ALLOW };
	errno = 0;
	if (install(ok, 1, 0) != -1 || errno != EACCES)
		return 1;
	if (prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) || prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) != 1)
		return 2;
	/* Chromium's probe: a NULL program is EFAULT once the flags are understood. */
	errno = 0;
	if (sc(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, 0, 0, 0) != -1 || errno != EFAULT)
		return 3;
	errno = 0;
	if (sc(SYS_seccomp, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC, 0, 0, 0) != -1 || errno != EFAULT)
		return 4;
	struct sock_filter no_ret[] = { NR_LOAD };
	struct sock_filter out_of_range[] = { BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 0, 5, 0), ALLOW };
	struct sock_filter misaligned[] = { BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 2), ALLOW };
	struct sock_filter past_end[] = { BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 64), ALLOW };
	struct sock_filter bad_op[] = { BPF_STMT(BPF_LD | BPF_B | BPF_ABS, 0), ALLOW };
	struct sock_filter uninit_mem[] = { BPF_STMT(BPF_LD | BPF_MEM, 3), ALLOW };
	struct sock_filter div0[] = { BPF_STMT(BPF_ALU | BPF_DIV | BPF_K, 0), ALLOW };
	struct {
		struct sock_filter *f;
		unsigned short n;
	} bad[] = { { no_ret, 1 }, { out_of_range, 2 }, { misaligned, 2 }, { past_end, 2 }, { bad_op, 2 }, { uninit_mem, 2 }, { div0, 2 }, { ok, 0 } };
	for (unsigned i = 0; i < sizeof(bad) / sizeof(bad[0]); i++) {
		errno = 0;
		if (install(bad[i].f, bad[i].n, 0) != -1 || errno != EINVAL)
			return 10 + i;
	}
	/* Linux refuses TSYNC with NEW_LISTENER (without TSYNC_ESRCH); Nova has no listeners. */
	errno = 0;
	if (install(ok, 1, SECCOMP_FILTER_FLAG_NEW_LISTENER | SECCOMP_FILTER_FLAG_TSYNC) != -1 || errno != EINVAL)
		return 5;
	unsigned act = SECCOMP_RET_TRAP;
	if (sc(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, (long)&act, 0, 0) != 0)
		return 6;
	act = SECCOMP_RET_KILL_PROCESS;
	if (sc(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, (long)&act, 0, 0) != 0)
		return 7;
	act = 0x12340000;
	errno = 0;
	if (sc(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, (long)&act, 0, 0) != -1 || errno != EOPNOTSUPP)
		return 8;
	/* Scratch memory, ALU and X: a program that computes nr * 2 + 1 == 2 * getppid + 1. */
	struct sock_filter calc[] = {
		NR_LOAD,
		BPF_STMT(BPF_ST, 0),
		BPF_STMT(BPF_LDX | BPF_MEM, 0),
		BPF_STMT(BPF_ALU | BPF_ADD | BPF_X, 0),
		BPF_STMT(BPF_ALU | BPF_OR | BPF_K, 1),
		BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, 2 * SYS_getppid + 1, 0, 1),
		BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | 77),
		/* The architecture: anything but x86-64 would be killed. */
		BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, arch)),
		BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0),
		BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS),
		ALLOW,
	};
	if (install(calc, sizeof(calc) / sizeof(calc[0]), SECCOMP_FILTER_FLAG_LOG | SECCOMP_FILTER_FLAG_SPEC_ALLOW))
		return 9;
	return getppid_blocked() && getpid() > 0 ? 0 : 30;
}

int main(int argc, char **argv)
{
	self = argv[0][0] == '/' ? argv[0] : "/bin/seccomptest";
	if (argc > 1 && strcmp(argv[1], "execcheck") == 0)
		return exec_check();

	printf("seccomptest: seccomp filters and strict mode\n");
	check(prctl(PR_GET_SECCOMP) == 0 && prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 0 && status_has("Seccomp:\t0"), "seccomp off at start, /proc/self/status says so");
	check(child_ok(t_validation), "no_new_privs required, Chromium's probes, invalid programs refused, GET_ACTION_AVAIL, ALU/memory/arch");
	check(child_ok(t_errno), "SECCOMP_RET_ERRNO by call number and by argument; filters stack; /proc/self/status");
	check(child_ok(t_trap), "SECCOMP_RET_TRAP: SIGSYS handler sees siginfo and ucontext, sets the result in rax");
	int st = in_child(t_kill_process);
	check(WIFSIGNALED(st) && WTERMSIG(st) == SIGSYS, "SECCOMP_RET_KILL_PROCESS: the child dies of SIGSYS");
	st = in_child(t_kill_thread);
	check(WIFSIGNALED(st) && WTERMSIG(st) == SIGSYS, "KILL_THREAD (single thread) beats ERRNO: SIGSYS");
	check(child_ok(t_tsync), "SECCOMP_FILTER_FLAG_TSYNC: other threads get the filter; a diverged thread blocks it");
	check(child_ok(t_fork_exec), "filters are inherited by fork and kept across execve");
	st = in_child(t_strict);
	check(WIFEXITED(st) && WEXITSTATUS(st) == 0, "strict mode: exit() allowed");
	st = in_child(t_strict_violation);
	check(WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL, "strict mode: any other call is SIGKILL");
	check(prctl(PR_GET_SECCOMP) == 0 && syscall(SYS_getppid) >= 0, "the parent is unaffected");
	printf("seccomptest: %d passed, %d failed\n", passed, failed);
	return failed != 0;
}
