/* FIFO 与设备节点的真实 guest 验证：tmpfs 与 ext4 上的 mknod、open 汇合、EOF/EPIPE、nodev 与持久性。
 *
 * 每一步失败都以步骤编号作为退出码；全部通过才打印唯一的成功 marker。 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void fail(int step)
{
	dprintf(2, "LITEOS_SPECIAL_FAILED step=%d errno=%d\n", step, errno);
	_exit(step);
}

#define CHECK(step, condition) do { if (!(condition)) fail(step); } while (0)

static long elapsed_ms(const struct timespec *from)
{
	struct timespec now;
	clock_gettime(CLOCK_MONOTONIC, &now);
	return (now.tv_sec - from->tv_sec) * 1000 + (now.tv_nsec - from->tv_nsec) / 1000000;
}

/* 在 `path` 所在的文件系统上跑完整的 FIFO 语义；`base` 区分步骤编号。 */
static void fifo_semantics(const char *path, int base)
{
	char buffer[32];
	struct stat info;
	unlink(path);
	CHECK(base + 1, mknod(path, S_IFIFO | 0600, 0) == 0);
	CHECK(base + 2, stat(path, &info) == 0 && S_ISFIFO(info.st_mode) && (info.st_mode & 0777) == 0600);

	/* 没有 reader：非阻塞只写 open 得到 ENXIO；非阻塞只读 open 立即成功。 */
	CHECK(base + 3, open(path, O_WRONLY | O_NONBLOCK) == -1 && errno == ENXIO);
	int reader = open(path, O_RDONLY | O_NONBLOCK);
	CHECK(base + 4, reader >= 0);
	int writer = open(path, O_WRONLY | O_NONBLOCK);
	CHECK(base + 5, writer >= 0);
	CHECK(base + 6, write(writer, "hello", 5) == 5);
	CHECK(base + 7, read(reader, buffer, sizeof buffer) == 5 && memcmp(buffer, "hello", 5) == 0);
	CHECK(base + 8, read(reader, buffer, sizeof buffer) == -1 && errno == EAGAIN);
	CHECK(base + 9, close(writer) == 0);
	CHECK(base + 10, read(reader, buffer, sizeof buffer) == 0); /* 没有 writer：EOF */

	/* 没有 reader 的写：EPIPE（SIGPIPE 已忽略）。 */
	writer = open(path, O_WRONLY | O_NONBLOCK);
	CHECK(base + 11, writer >= 0);
	CHECK(base + 12, close(reader) == 0);
	CHECK(base + 13, write(writer, "x", 1) == -1 && errno == EPIPE);
	CHECK(base + 14, close(writer) == 0);

	/* 阻塞汇合：子进程延迟 200ms 才打开 writer，父进程的阻塞 reader open 必须等到它。 */
	pid_t child = fork();
	if (child == 0) {
		const struct timespec delay = { 0, 200 * 1000 * 1000 };
		nanosleep(&delay, 0);
		int fd = open(path, O_WRONLY);
		if (fd < 0 || write(fd, "from-child", 10) != 10) _exit(1);
		_exit(close(fd) == 0 ? 0 : 2);
	}
	struct timespec start;
	clock_gettime(CLOCK_MONOTONIC, &start);
	reader = open(path, O_RDONLY);
	CHECK(base + 15, reader >= 0 && elapsed_ms(&start) >= 150);
	ssize_t total = 0, got;
	while ((got = read(reader, buffer + total, sizeof buffer - total)) > 0) total += got;
	CHECK(base + 16, got == 0 && total == 10 && memcmp(buffer, "from-child", 10) == 0);
	int status;
	CHECK(base + 17, waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
	CHECK(base + 18, close(reader) == 0);

	/* 反方向：阻塞 writer open 等 reader，被 unlink 后已打开的两端仍然互通。 */
	child = fork();
	if (child == 0) {
		const struct timespec delay = { 0, 200 * 1000 * 1000 };
		nanosleep(&delay, 0);
		int fd = open(path, O_RDONLY);
		if (fd < 0) _exit(1);
		unlink(path);
		ssize_t count = read(fd, buffer, sizeof buffer);
		_exit(count == 4 && memcmp(buffer, "ping", 4) == 0 ? 0 : 2);
	}
	clock_gettime(CLOCK_MONOTONIC, &start);
	writer = open(path, O_WRONLY);
	CHECK(base + 19, writer >= 0 && elapsed_ms(&start) >= 150);
	CHECK(base + 20, write(writer, "ping", 4) == 4);
	CHECK(base + 21, waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
	CHECK(base + 22, close(writer) == 0);
	CHECK(base + 23, stat(path, &info) == -1 && errno == ENOENT);

	/* O_RDWR：同一个 fd 既是 reader 又是 writer，打开不阻塞，写入的数据可由自己读回。 */
	CHECK(base + 24, mknod(path, S_IFIFO | 0600, 0) == 0);
	int both = open(path, O_RDWR);
	CHECK(base + 25, both >= 0);
	CHECK(base + 26, write(both, "loop", 4) == 4 && read(both, buffer, sizeof buffer) == 4
	      && memcmp(buffer, "loop", 4) == 0);

	/* poll：尚无 writer 来过的 reader 不报 POLLHUP；writer 来过又离开之后才报；数据到来报 POLLIN。 */
	CHECK(base + 27, close(both) == 0);
	reader = open(path, O_RDONLY | O_NONBLOCK);
	struct pollfd poll_reader = { .fd = reader, .events = POLLIN };
	CHECK(base + 28, reader >= 0 && poll(&poll_reader, 1, 0) == 0);
	writer = open(path, O_WRONLY | O_NONBLOCK);
	CHECK(base + 29, writer >= 0 && poll(&poll_reader, 1, 0) == 0);
	CHECK(base + 30, write(writer, "z", 1) == 1 && poll(&poll_reader, 1, 0) == 1
	      && (poll_reader.revents & POLLIN));
	CHECK(base + 31, read(reader, buffer, 1) == 1 && close(writer) == 0);
	poll_reader.revents = 0;
	CHECK(base + 32, poll(&poll_reader, 1, 0) == 1 && (poll_reader.revents & POLLHUP));
	CHECK(base + 33, close(reader) == 0);

	/* SIGPIPE：默认处置下，向没有 reader 的 FIFO 写入终止写者。 */
	child = fork();
	if (child == 0) {
		int r = open(path, O_RDONLY | O_NONBLOCK);
		int w = open(path, O_WRONLY | O_NONBLOCK);
		if (r < 0 || w < 0 || close(r) != 0) _exit(1);
		signal(SIGPIPE, SIG_DFL);
		(void)!write(w, "x", 1);
		_exit(2);
	}
	CHECK(base + 34, waitpid(child, &status, 0) == child && WIFSIGNALED(status)
	      && WTERMSIG(status) == SIGPIPE);
	CHECK(base + 35, unlink(path) == 0);
}

static void device_nodes(const char *directory, int base)
{
	char path[64];
	char buffer[8];
	snprintf(path, sizeof path, "%s/null-node", directory);
	unlink(path);
	CHECK(base + 1, mknod(path, S_IFCHR | 0666, makedev(1, 3)) == 0);
	struct stat info;
	CHECK(base + 2, stat(path, &info) == 0 && S_ISCHR(info.st_mode)
	      && major(info.st_rdev) == 1 && minor(info.st_rdev) == 3);
	int fd = open(path, O_RDWR);
	CHECK(base + 3, fd >= 0 && write(fd, "discard", 7) == 7 && read(fd, buffer, sizeof buffer) == 0);
	CHECK(base + 4, close(fd) == 0 && unlink(path) == 0);

	/* 块设备节点：可打开读到 vda 的 ext4 超级块魔数。 */
	snprintf(path, sizeof path, "%s/disk-node", directory);
	unlink(path);
	CHECK(base + 5, mknod(path, S_IFBLK | 0600, makedev(254, 0)) == 0);
	fd = open(path, O_RDONLY);
	CHECK(base + 6, fd >= 0 && pread(fd, buffer, 2, 1080) == 2
	      && (unsigned char)buffer[0] == 0x53 && (unsigned char)buffer[1] == 0xef);
	CHECK(base + 7, close(fd) == 0 && unlink(path) == 0);
}

int main(void)
{
	signal(SIGPIPE, SIG_IGN);
	fifo_semantics("/tmp/fifo-probe", 0);
	fifo_semantics("/var/tmp/fifo-probe", 100);
	device_nodes("/tmp", 200);
	device_nodes("/var/tmp", 220);

	/* nodev 挂载上的设备节点不可打开；类型错误的 mknod 参数被拒绝。 */
	CHECK(300, mkdir("/mnt", 0755) == 0 || errno == EEXIST);
	CHECK(301, mkdir("/mnt/nodev", 0755) == 0 || errno == EEXIST);
	CHECK(302, mount("tmpfs", "/mnt/nodev", "tmpfs", MS_NODEV, "") == 0);
	CHECK(303, mknod("/mnt/nodev/null", S_IFCHR | 0666, makedev(1, 3)) == 0);
	CHECK(304, open("/mnt/nodev/null", O_RDWR) == -1 && errno == EACCES);
	CHECK(305, unlink("/mnt/nodev/null") == 0 && umount("/mnt/nodev") == 0);
	CHECK(306, mknod("/tmp/bad-node", S_IFDIR | 0755, 0) == -1 && errno == EPERM);
	puts("LITEOS_SPECIAL_42");
	return 0;
}
