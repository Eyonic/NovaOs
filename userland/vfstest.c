/* vfstest: the file system as Linux programs see it: symbolic and hard links, directories,
 * rename, permissions, and tmpfs (/run) with many files and long names. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

static int passed, failed;

static void check(int ok, const char *what)
{
	printf("  %s %s\n", ok ? "ok  " : "FAIL", what);
	ok ? passed++ : failed++;
}

static int write_file(const char *path, const char *text)
{
	int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
	if (fd < 0)
		return -1;
	int r = write(fd, text, strlen(text)) == (ssize_t)strlen(text) ? 0 : -1;
	close(fd);
	return r;
}

static int read_file(const char *path, char *buf, size_t size)
{
	int fd = open(path, O_RDONLY);
	if (fd < 0)
		return -1;
	ssize_t n = read(fd, buf, size - 1);
	close(fd);
	buf[n < 0 ? 0 : n] = 0;
	return n;
}

static int count_entries(const char *dir)
{
	DIR *d = opendir(dir);
	if (!d)
		return -1;
	int n = 0;
	struct dirent *e;
	while ((e = readdir(d)))
		if (strcmp(e->d_name, ".") && strcmp(e->d_name, ".."))
			n++;
	closedir(d);
	return n;
}

static void suite(const char *base)
{
	char p[PATH_MAX], q[PATH_MAX], buf[256];
	struct stat st;
	printf(" in %s:\n", base);

	snprintf(p, sizeof p, "%s/d", base);
	check(mkdir(p, 0755) == 0 && stat(p, &st) == 0 && S_ISDIR(st.st_mode), "mkdir makes an (empty) directory");
	check(mkdir(p, 0755) == -1 && errno == EEXIST, "mkdir of an existing name: EEXIST");
	snprintf(p, sizeof p, "%s/d/file", base);
	check(write_file(p, "hello vfs") == 0, "create a file in it");

	snprintf(q, sizeof q, "%s/link", base);
	check(symlink("d/file", q) == 0, "symlink (relative target)");
	check(read_file(q, buf, sizeof buf) == 9 && strcmp(buf, "hello vfs") == 0, "open follows the link");
	ssize_t n = readlink(q, buf, sizeof buf);
	check(n == 6 && memcmp(buf, "d/file", 6) == 0, "readlink returns the target");
	check(lstat(q, &st) == 0 && S_ISLNK(st.st_mode) && stat(q, &st) == 0 && S_ISREG(st.st_mode) && st.st_size == 9,
	      "lstat sees the link, stat the file");

	snprintf(q, sizeof q, "%s/dirlink", base);
	snprintf(p, sizeof p, "%s/d", base);
	check(symlink(p, q) == 0, "symlink to a directory (absolute target)");
	snprintf(q, sizeof q, "%s/dirlink/file", base);
	check(read_file(q, buf, sizeof buf) == 9, "a link in the middle of a path is followed");
	snprintf(q, sizeof q, "%s/dirlink/../d/file", base);
	char *real = realpath(q, NULL);
	snprintf(p, sizeof p, "%s/d/file", base);
	check(real && strcmp(real, p) == 0, "realpath resolves links and ..");
	free(real);

	snprintf(p, sizeof p, "%s/loop1", base);
	snprintf(q, sizeof q, "%s/loop2", base);
	symlink(q, p);
	symlink(p, q);
	check(open(p, O_RDONLY) == -1 && errno == ELOOP, "a link loop gives ELOOP");
	check(unlink(p) == 0 && unlink(q) == 0 && lstat(p, &st) == -1 && errno == ENOENT, "unlink removes links, not targets");

	snprintf(p, sizeof p, "%s/d/file", base);
	snprintf(q, sizeof q, "%s/d/renamed", base);
	check(rename(p, q) == 0 && stat(p, &st) == -1 && read_file(q, buf, sizeof buf) == 9, "rename a file");
	snprintf(p, sizeof p, "%s/d", base);
	snprintf(q, sizeof q, "%s/e", base);
	check(rename(p, q) == 0 && count_entries(q) == 1, "rename a directory with its contents");
	snprintf(p, sizeof p, "%s/e/renamed", base);
	check(chmod(p, 0751) == 0 && stat(p, &st) == 0 && (st.st_mode & 07777) == 0751, "chmod sets the permission bits");
	snprintf(q, sizeof q, "%s/e", base);
	check(rmdir(q) == -1 && errno == ENOTEMPTY, "rmdir of a non-empty directory: ENOTEMPTY");
	check(unlink(p) == 0 && rmdir(q) == 0 && stat(q, &st) == -1, "rmdir after emptying it");

	snprintf(p, sizeof p, "%s/append", base);
	write_file(p, "ab");
	int fd = open(p, O_WRONLY | O_APPEND);
	lseek(fd, 0, SEEK_SET);
	write(fd, "cd", 2);
	close(fd);
	check(read_file(p, buf, sizeof buf) == 4 && strcmp(buf, "abcd") == 0, "O_APPEND writes at the end");
	unlink(p);
}

int main(void)
{
	setvbuf(stdout, NULL, _IONBF, 0);
	printf("vfstest: file system\n");
	mkdir("/vfst", 0755);
	suite("/vfst");
	suite("/run");

	/* tmpfs only: inodes, many files, long names. */
	struct stat a, b;
	char buf[64];
	write_file("/run/orig", "shared");
	check(link("/run/orig", "/run/hard") == 0 && stat("/run/orig", &a) == 0 && stat("/run/hard", &b) == 0 && a.st_ino == b.st_ino &&
	          a.st_nlink == 2,
	      "tmpfs hard link: same inode, nlink 2");
	int fd = open("/run/hard", O_WRONLY | O_TRUNC);
	write(fd, "changed", 7);
	close(fd);
	check(read_file("/run/orig", buf, sizeof buf) == 7 && strcmp(buf, "changed") == 0, "writes through one name show through the other");
	fd = open("/run/orig", O_RDWR);
	unlink("/run/orig");
	unlink("/run/hard");
	check(fd >= 0 && pread(fd, buf, 7, 0) == 7 && stat("/run/orig", &a) == -1, "an unlinked file stays readable while open");
	char *map = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	ftruncate(fd, 4096);
	map[0] = 'C';
	check(pread(fd, buf, 1, 0) == 1 && buf[0] == 'C', "MAP_SHARED of a tmpfs file shares its pages");
	close(fd);

	// Sizes come from programs: one the kernel cannot hold is an error for the program,
	// never a failed kernel allocation.
	fd = open("/vfs-big", O_RDWR | O_CREAT | O_TRUNC, 0644);
	check(fd >= 0 && ftruncate(fd, 1LL << 36) == -1 && errno == ENOSPC, "ftruncate to 64 GiB is ENOSPC");
	check(pwrite(fd, "x", 1, 1LL << 36) == -1 && errno == ENOSPC, "a write at 64 GiB is ENOSPC");
	close(fd);
	unlink("/vfs-big");

	mkdir("/run/many", 0755);
	int made = 0;
	char name[300];
	for (int i = 0; i < 3000; i++) {
		snprintf(name, sizeof name, "/run/many/file-%04d", i);
		made += write_file(name, "x") == 0;
	}
	check(made == 3000 && count_entries("/run/many") == 3000, "3000 files in one directory");
	memset(name, 0, sizeof name);
	strcpy(name, "/run/");
	memset(name + 5, 'n', 200);
	check(write_file(name, "long") == 0 && stat(name, &a) == 0 && a.st_size == 4, "a 200-character file name");

	check(count_entries("/") > 0 && access("/run", F_OK) == 0, "/run (a mount point) is listed and reachable");
	printf("vfstest: %d passed, %d failed\n", passed, failed);
	return failed != 0;
}
