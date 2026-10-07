/* SMP 下的文件系统并发压测：tmpfs 与 ext4 上的 create/rename/unlink/link/mkdir/rmdir 竞争与 FIFO 多写者。
 *
 * 目标是抓死锁、丢更新与计数漂移：每一组竞争结束后都要求最终状态满足不变量。
 * 每一步失败都以步骤编号作为退出码；全部通过才打印唯一的成功 marker。 */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <time.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define THREADS 6
#define ROUNDS 150

static void fail(int step)
{
	dprintf(2, "LITEOS_FSSTRESS_FAILED step=%d errno=%d\n", step, errno);
	_exit(step);
}

#define CHECK(step, condition) do { if (!(condition)) fail(step); } while (0)

static char root[64];
static atomic_int started;

static void barrier(void)
{
	atomic_fetch_add(&started, 1);
	while (atomic_load(&started) < THREADS) sched_yield();
}

static int count_entries(const char *path)
{
	DIR *dir = opendir(path);
	if (!dir) return -1;
	int count = 0;
	struct dirent *entry;
	while ((entry = readdir(dir)))
		if (strcmp(entry->d_name, ".") && strcmp(entry->d_name, "..")) count++;
	closedir(dir);
	return count;
}

static void stage(const char *name)
{
	printf("fsstress %s %s\n", root, name);
	fflush(stdout);
}

static void run_threads(void *(*body)(void *))
{
	pthread_t threads[THREADS];
	atomic_store(&started, 0);
	for (long index = 0; index < THREADS; index++)
		CHECK(1, pthread_create(&threads[index], 0, body, (void *)index) == 0);
	for (int index = 0; index < THREADS; index++) {
		void *result;
		CHECK(2, pthread_join(threads[index], &result) == 0);
		if (result) fail((int)(long)result);
	}
}

/* A. 每个线程在共享目录里 create → write → rename → unlink；结束时目录必须为空，期间的目录遍历不崩溃。 */
static void *churn(void *argument)
{
	long id = (long)argument;
	char from[128], to[128], buffer[16];
	barrier();
	for (int round = 0; round < ROUNDS; round++) {
		snprintf(from, sizeof from, "%s/a/f-%ld-%d", root, id, round);
		snprintf(to, sizeof to, "%s/a/g-%ld-%d", root, id, round);
		int fd = open(from, O_RDWR | O_CREAT | O_EXCL, 0600);
		if (fd < 0) return (void *)10;
		if (write(fd, "payload", 7) != 7) return (void *)11;
		if (rename(from, to) != 0) return (void *)12;
		if (pread(fd, buffer, 7, 0) != 7 || memcmp(buffer, "payload", 7)) return (void *)13;
		if (close(fd) != 0) return (void *)14;
		if (round % 16 == 0 && count_entries(root) < 0) return (void *)15;
		if (unlink(to) != 0) return (void *)16;
	}
	return 0;
}

/* B. 一个文件在两个目录间被多个线程来回 rename：任何时刻恰好存在一份，最终也恰好一份。 */
static void *pingpong(void *argument)
{
	(void)argument;
	char x[128], y[128];
	snprintf(x, sizeof x, "%s/p/x/file", root);
	snprintf(y, sizeof y, "%s/p/y/file", root);
	barrier();
	for (int round = 0; round < ROUNDS * 2; round++) {
		/* 同一时刻只有一个 rename 能成功，输家得到 ENOENT；任何别的错误都是 bug。 */
		if (rename(x, y) != 0 && errno != ENOENT) return (void *)20;
		if (rename(y, x) != 0 && errno != ENOENT) return (void *)21;
	}
	return 0;
}

/* C. 同名 mkdir/rmdir 竞争：只允许 EEXIST/ENOENT，不允许别的错误。 */
static void *dir_race(void *argument)
{
	(void)argument;
	char path[128];
	snprintf(path, sizeof path, "%s/d/shared", root);
	barrier();
	for (int round = 0; round < ROUNDS * 2; round++) {
		if (mkdir(path, 0700) != 0 && errno != EEXIST) return (void *)30;
		if (rmdir(path) != 0 && errno != ENOENT) return (void *)31;
	}
	return 0;
}

/* D. 多线程对同一 inode link/unlink：最终 link count 必须回到 1。 */
static void *link_race(void *argument)
{
	long id = (long)argument;
	char source[128], alias[128];
	snprintf(source, sizeof source, "%s/l/source", root);
	barrier();
	for (int round = 0; round < ROUNDS; round++) {
		snprintf(alias, sizeof alias, "%s/l/alias-%ld-%d", root, id, round);
		if (link(source, alias) != 0) return (void *)40;
		if (unlink(alias) != 0) return (void *)41;
	}
	return 0;
}

/* E. 目录里同时创建与读取：readdir 在并发增删时不重复返回同一名字（cookie 稳定）。 */
static void *scan_while_changing(void *argument)
{
	long id = (long)argument;
	char path[128];
	barrier();
	if (id == 0) {
		for (int round = 0; round < ROUNDS * 2; round++) {
			DIR *dir = opendir(root);
			if (!dir) return (void *)50;
			char seen[512][32];
			int count = 0;
			struct dirent *entry;
			while ((entry = readdir(dir)) && count < 512) {
				for (int index = 0; index < count; index++)
					if (!strcmp(seen[index], entry->d_name)) return (void *)51;
				snprintf(seen[count++], 32, "%s", entry->d_name);
			}
			closedir(dir);
		}
		return 0;
	}
	for (int round = 0; round < ROUNDS; round++) {
		snprintf(path, sizeof path, "%s/s-%ld-%d", root, id, round);
		int fd = open(path, O_CREAT | O_WRONLY, 0600);
		if (fd < 0) return (void *)52;
		close(fd);
		if (round % 2 && unlink(path) != 0) return (void *)53;
	}
	return 0;
}

/* 递归删除 `directory` 里的全部内容（不删 `directory` 自身）；任何一项删不掉都是失败，而不是重试。 */
static void remove_tree_files(const char *directory, int step)
{
	DIR *dir = opendir(directory);
	CHECK(step, dir != NULL);
	struct dirent *entry;
	while ((entry = readdir(dir))) {
		if (!strcmp(entry->d_name, ".") || !strcmp(entry->d_name, "..")) continue;
		char path[256];
		snprintf(path, sizeof path, "%s/%s", directory, entry->d_name);
		struct stat info;
		CHECK(step + 1, lstat(path, &info) == 0);
		if (S_ISDIR(info.st_mode)) {
			remove_tree_files(path, step);
			CHECK(step + 2, rmdir(path) == 0);
		} else {
			CHECK(step + 3, unlink(path) == 0);
		}
	}
	closedir(dir);
}

/* F. FIFO：多个写者各写固定长度、可识别的原子消息，单个读者必须收到全部且不撕裂。 */
static void fifo_many_writers(const char *base)
{
	char path[128];
	snprintf(path, sizeof path, "%s/fifo", base);
	unlink(path);
	CHECK(70, mknod(path, S_IFIFO | 0600, 0) == 0);
	enum { WRITERS = 4, MESSAGES = 200, SIZE = 32 };
	pid_t children[WRITERS];
	for (int writer = 0; writer < WRITERS; writer++) {
		children[writer] = fork();
		if (children[writer] == 0) {
			int fd = open(path, O_WRONLY);
			if (fd < 0) _exit(1);
			char message[SIZE];
			for (int index = 0; index < MESSAGES; index++) {
				memset(message, 'A' + writer, SIZE);
				if (write(fd, message, SIZE) != SIZE) _exit(2);
			}
			_exit(close(fd) == 0 ? 0 : 3);
		}
		CHECK(71, children[writer] > 0);
	}
	stage("fifo-reader-open");
	int reader = open(path, O_RDONLY);
	stage("fifo-reader-opened");
	CHECK(72, reader >= 0);
	int counts[WRITERS] = { 0 };
	char message[SIZE];
	ssize_t got;
	int total = 0;
	while ((got = read(reader, message, SIZE)) > 0) {
		/* PIPE_BUF 以内的写是原子的：每次读到的 32 字节必须来自同一个写者。 */
		CHECK(73, got == SIZE);
		for (int index = 1; index < SIZE; index++) CHECK(74, message[index] == message[0]);
		CHECK(75, message[0] >= 'A' && message[0] < 'A' + WRITERS);
		counts[message[0] - 'A']++;
		total++;
	}
	CHECK(76, got == 0 && total == WRITERS * MESSAGES);
	for (int writer = 0; writer < WRITERS; writer++) {
		int status;
		CHECK(77, counts[writer] == MESSAGES);
		CHECK(78, waitpid(children[writer], &status, 0) == children[writer] && WIFEXITED(status) && !WEXITSTATUS(status));
	}
	CHECK(79, close(reader) == 0 && unlink(path) == 0);
}

/* G. 多个写者同时阻塞在 open(O_WRONLY)，一个 reader 到来必须把它们全部放行（open 汇合是广播）。 */
static void blocked_writers_released_together(const char *base)
{
	char path[128];
	snprintf(path, sizeof path, "%s/rendezvous", base);
	unlink(path);
	CHECK(80, mknod(path, S_IFIFO | 0600, 0) == 0);
	enum { WRITERS = 5 };
	pid_t children[WRITERS];
	for (int writer = 0; writer < WRITERS; writer++) {
		children[writer] = fork();
		if (children[writer] == 0) {
			int fd = open(path, O_WRONLY);
			_exit(fd >= 0 && close(fd) == 0 ? 0 : 1);
		}
		CHECK(81, children[writer] > 0);
	}
	/* 让全部子进程先阻塞进 open，再放行。 */
	const struct timespec delay = { 0, 300 * 1000 * 1000 };
	nanosleep(&delay, 0);
	int reader = open(path, O_RDONLY);
	CHECK(82, reader >= 0);
	for (int writer = 0; writer < WRITERS; writer++) {
		int status;
		CHECK(83, waitpid(children[writer], &status, 0) == children[writer]
		      && WIFEXITED(status) && WEXITSTATUS(status) == 0);
	}
	CHECK(84, close(reader) == 0 && unlink(path) == 0);
}

static void make(const char *suffix)
{
	char path[128];
	snprintf(path, sizeof path, "%s/%s", root, suffix);
	CHECK(3, mkdir(path, 0700) == 0);
}

static void one_filesystem(const char *base, int offset)
{
	snprintf(root, sizeof root, "%s/stress", base);
	rmdir(root);
	CHECK(offset + 1, mkdir(root, 0700) == 0);

	make("a");
	stage("churn");
	run_threads(churn);
	char path[128];
	snprintf(path, sizeof path, "%s/a", root);
	CHECK(offset + 2, count_entries(path) == 0);

	make("p");
	snprintf(path, sizeof path, "%s/p/x", root);
	CHECK(offset + 3, mkdir(path, 0700) == 0);
	snprintf(path, sizeof path, "%s/p/y", root);
	CHECK(offset + 4, mkdir(path, 0700) == 0);
	snprintf(path, sizeof path, "%s/p/x/file", root);
	int fd = open(path, O_CREAT | O_WRONLY, 0600);
	CHECK(offset + 5, fd >= 0 && close(fd) == 0);
	stage("pingpong");
	run_threads(pingpong);
	char other[128];
	snprintf(path, sizeof path, "%s/p/x", root);
	snprintf(other, sizeof other, "%s/p/y", root);
	CHECK(offset + 6, count_entries(path) + count_entries(other) == 1);

	make("d");
	stage("dir_race");
	run_threads(dir_race);
	snprintf(path, sizeof path, "%s/d/shared", root);
	rmdir(path);
	snprintf(path, sizeof path, "%s/d", root);
	CHECK(offset + 7, count_entries(path) == 0);

	make("l");
	snprintf(path, sizeof path, "%s/l/source", root);
	fd = open(path, O_CREAT | O_WRONLY, 0600);
	CHECK(offset + 8, fd >= 0 && close(fd) == 0);
	stage("link_race");
	run_threads(link_race);
	struct stat info;
	CHECK(offset + 9, stat(path, &info) == 0 && info.st_nlink == 1);

	stage("scan");
	run_threads(scan_while_changing);
	stage("scan-done");
	fifo_many_writers(root);
	stage("fifo-done");
	blocked_writers_released_together(root);
	stage("rendezvous-done");
	remove_tree_files(root, offset + 10);
	CHECK(offset + 20, count_entries(root) == 0);
	CHECK(offset + 21, rmdir(root) == 0);
}

int main(void)
{
	signal(SIGPIPE, SIG_IGN);
	one_filesystem("/tmp", 100);
	one_filesystem("/var/tmp", 200);
	puts("LITEOS_FSSTRESS_42");
	return 0;
}
