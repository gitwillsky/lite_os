/* inotify 的真实 guest 验证：tmpfs 与 ext4 上的目录/文件事件、cookie 配对、合并、阻塞与 poll/epoll、
 * IN_ONESHOT、rm_watch 与卸载。
 *
 * 每一步失败都以步骤编号作为退出码；全部通过才打印唯一的成功 marker。 */
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static void fail(int step)
{
	dprintf(2, "LITEOS_INOTIFY_FAILED step=%d errno=%d\n", step, errno);
	_exit(step);
}

#define CHECK(step, condition) do { if (!(condition)) fail(step); } while (0)

struct seen {
	int wd;
	unsigned mask, cookie;
	char name[64];
};

/* 非阻塞读空队列里的全部事件。 */
static int drain(int fd, struct seen *out, int max)
{
	char buffer[4096];
	int count = 0;
	for (;;) {
		ssize_t got = read(fd, buffer, sizeof buffer);
		if (got < 0) {
			if (errno == EAGAIN) return count;
			return -1;
		}
		for (char *at = buffer; at < buffer + got && count < max;) {
			struct inotify_event *event = (struct inotify_event *)at;
			out[count].wd = event->wd;
			out[count].mask = event->mask;
			out[count].cookie = event->cookie;
			snprintf(out[count].name, sizeof out[count].name, "%s", event->len ? event->name : "");
			count++;
			at += sizeof *event + event->len;
		}
	}
}

static int has(const struct seen *events, int count, unsigned mask, const char *name)
{
	for (int index = 0; index < count; index++)
		if ((events[index].mask & mask) == mask && !strcmp(events[index].name, name)) return 1;
	return 0;
}

static int touch_file(const char *path, const char *data)
{
	int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
	if (fd < 0) return -1;
	if (data && write(fd, data, strlen(data)) < 0) return -1;
	return close(fd);
}

#define ALL (IN_ACCESS | IN_MODIFY | IN_ATTRIB | IN_CLOSE_WRITE | IN_CLOSE_NOWRITE | IN_OPEN \
	| IN_MOVED_FROM | IN_MOVED_TO | IN_CREATE | IN_DELETE | IN_DELETE_SELF | IN_MOVE_SELF)

static void directory_events(const char *base, int step)
{
	char directory[96], file[128], moved[128];
	snprintf(directory, sizeof directory, "%s/watched", base);
	snprintf(file, sizeof file, "%s/a", directory);
	snprintf(moved, sizeof moved, "%s/b", directory);
	rmdir(directory);
	CHECK(step + 1, mkdir(directory, 0755) == 0);
	int fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
	CHECK(step + 2, fd >= 0);
	struct seen events[64];
	CHECK(step + 3, read(fd, events, sizeof events) == -1 && errno == EAGAIN);
	int wd = inotify_add_watch(fd, directory, ALL);
	CHECK(step + 4, wd > 0);
	CHECK(step + 5, inotify_add_watch(fd, directory, ALL) == wd); /* 重复添加返回同一个 wd */

	/* create + write + close：CREATE、OPEN、MODIFY、CLOSE_WRITE 都带文件名。 */
	CHECK(step + 6, touch_file(file, "hello") == 0);
	int count = drain(fd, events, 64);
	CHECK(step + 7, has(events, count, IN_CREATE, "a") && has(events, count, IN_OPEN, "a")
	      && has(events, count, IN_MODIFY, "a") && has(events, count, IN_CLOSE_WRITE, "a"));
	for (int index = 0; index < count; index++) CHECK(step + 8, events[index].wd == wd);

	/* 读：ACCESS 与 CLOSE_NOWRITE。 */
	char buffer[16];
	int reader = open(file, O_RDONLY);
	CHECK(step + 9, reader >= 0 && read(reader, buffer, sizeof buffer) == 5 && close(reader) == 0);
	count = drain(fd, events, 64);
	CHECK(step + 10, has(events, count, IN_OPEN, "a") && has(events, count, IN_ACCESS, "a")
	      && has(events, count, IN_CLOSE_NOWRITE, "a"));

	/* 属性变化。 */
	CHECK(step + 11, chmod(file, 0600) == 0);
	count = drain(fd, events, 64);
	CHECK(step + 12, count == 1 && has(events, count, IN_ATTRIB, "a"));

	/* rename：MOVED_FROM 与 MOVED_TO 的 cookie 相同且非零。 */
	CHECK(step + 13, rename(file, moved) == 0);
	count = drain(fd, events, 64);
	CHECK(step + 14, has(events, count, IN_MOVED_FROM, "a") && has(events, count, IN_MOVED_TO, "b"));
	unsigned from = 0, to = 1;
	for (int index = 0; index < count; index++) {
		if (events[index].mask & IN_MOVED_FROM) from = events[index].cookie;
		if (events[index].mask & IN_MOVED_TO) to = events[index].cookie;
	}
	CHECK(step + 15, from != 0 && from == to);

	/* 目录项：mkdir 带 ISDIR；unlink 与 rmdir 产生 DELETE。 */
	char sub[128];
	snprintf(sub, sizeof sub, "%s/sub", directory);
	CHECK(step + 16, mkdir(sub, 0755) == 0);
	count = drain(fd, events, 64);
	CHECK(step + 17, has(events, count, IN_CREATE | IN_ISDIR, "sub"));
	CHECK(step + 18, rmdir(sub) == 0 && unlink(moved) == 0);
	count = drain(fd, events, 64);
	CHECK(step + 19, has(events, count, IN_DELETE | IN_ISDIR, "sub") && has(events, count, IN_DELETE, "b"));

	/* 相邻的相同事件合并：两次写入只留一个 MODIFY。 */
	CHECK(step + 20, touch_file(file, NULL) == 0);
	drain(fd, events, 64);
	int writer = open(file, O_WRONLY);
	CHECK(step + 21, writer >= 0);
	drain(fd, events, 64);
	CHECK(step + 22, write(writer, "x", 1) == 1 && write(writer, "x", 1) == 1);
	count = drain(fd, events, 64);
	CHECK(step + 23, count == 1 && has(events, count, IN_MODIFY, "a"));
	CHECK(step + 24, close(writer) == 0 && unlink(file) == 0);

	/* rm_watch：队列里出现该 wd 的 IGNORED；再删或删陌生 wd 是 EINVAL。 */
	drain(fd, events, 64);
	CHECK(step + 25, inotify_rm_watch(fd, wd) == 0);
	count = drain(fd, events, 64);
	CHECK(step + 26, count == 1 && events[0].wd == wd && (events[0].mask & IN_IGNORED));
	CHECK(step + 27, inotify_rm_watch(fd, wd) == -1 && errno == EINVAL);
	CHECK(step + 28, close(fd) == 0 && rmdir(directory) == 0);
}

static void file_events(const char *base, int step)
{
	char file[128];
	snprintf(file, sizeof file, "%s/watched-file", base);
	CHECK(step + 1, touch_file(file, "data") == 0);
	int fd = inotify_init1(IN_NONBLOCK);
	CHECK(step + 2, fd >= 0);
	/* 参数校验。 */
	CHECK(step + 3, inotify_add_watch(fd, file, 0) == -1 && errno == EINVAL);
	CHECK(step + 4, inotify_add_watch(fd, "/nonexistent-path", IN_MODIFY) == -1 && errno == ENOENT);
	CHECK(step + 5, inotify_add_watch(fd, file, IN_MODIFY | IN_ONLYDIR) == -1 && errno == ENOTDIR);
	CHECK(step + 6, inotify_add_watch(0, file, IN_MODIFY) == -1 && errno == EINVAL); /* fd 不是 inotify */
	int wd = inotify_add_watch(fd, file, IN_MODIFY | IN_ATTRIB | IN_DELETE_SELF);
	CHECK(step + 7, wd > 0);
	/* IN_MASK_ADD 合并掩码，仍是同一个 wd。 */
	CHECK(step + 8, inotify_add_watch(fd, file, IN_OPEN | IN_MASK_ADD) == wd);

	struct seen events[16];
	int writer = open(file, O_WRONLY);
	CHECK(step + 9, writer >= 0 && write(writer, "z", 1) == 1 && close(writer) == 0);
	int count = drain(fd, events, 16);
	/* 文件自身的 watch：没有名字；CLOSE_WRITE 不在掩码里。 */
	CHECK(step + 10, has(events, count, IN_OPEN, "") && has(events, count, IN_MODIFY, "")
	      && !has(events, count, IN_CLOSE_WRITE, ""));

	/* 删除最后一个 link：DELETE_SELF，随后该 wd 被内核撤销并投递 IGNORED。 */
	CHECK(step + 11, unlink(file) == 0);
	count = drain(fd, events, 16);
	CHECK(step + 12, has(events, count, IN_DELETE_SELF, "") && has(events, count, IN_IGNORED, ""));
	CHECK(step + 13, inotify_rm_watch(fd, wd) == -1 && errno == EINVAL);

	/* IN_ONESHOT：第一个事件之后 watch 自动撤销。 */
	CHECK(step + 14, touch_file(file, "x") == 0);
	wd = inotify_add_watch(fd, file, IN_MODIFY | IN_ONESHOT);
	CHECK(step + 15, wd > 0 && touch_file(file, "yy") == 0);
	count = drain(fd, events, 16);
	CHECK(step + 16, has(events, count, IN_MODIFY, "") && has(events, count, IN_IGNORED, ""));
	CHECK(step + 17, unlink(file) == 0 && close(fd) == 0);
}

static void waiting(const char *base, int step)
{
	char directory[96], file[128];
	snprintf(directory, sizeof directory, "%s/wait", base);
	snprintf(file, sizeof file, "%s/f", directory);
	rmdir(directory);
	CHECK(step + 1, mkdir(directory, 0755) == 0);
	int fd = inotify_init1(0);
	CHECK(step + 2, fd >= 0 && inotify_add_watch(fd, directory, IN_CREATE) > 0);
	struct pollfd poller = { .fd = fd, .events = POLLIN };
	CHECK(step + 3, poll(&poller, 1, 0) == 0);
	int epoll = epoll_create1(0);
	struct epoll_event interest = { .events = EPOLLIN, .data.fd = fd };
	struct epoll_event ready;
	CHECK(step + 4, epoll >= 0 && epoll_ctl(epoll, EPOLL_CTL_ADD, fd, &interest) == 0);
	CHECK(step + 5, epoll_wait(epoll, &ready, 1, 0) == 0);

	/* 阻塞 read 由另一个进程稍后的 create 唤醒；epoll_wait 同样。 */
	pid_t child = fork();
	if (child == 0) {
		const struct timespec delay = { 0, 250 * 1000 * 1000 };
		nanosleep(&delay, 0);
		_exit(touch_file(file, NULL) == 0 ? 0 : 1);
	}
	struct timespec start, now;
	clock_gettime(CLOCK_MONOTONIC, &start);
	CHECK(step + 6, epoll_wait(epoll, &ready, 1, 5000) == 1 && ready.data.fd == fd);
	clock_gettime(CLOCK_MONOTONIC, &now);
	CHECK(step + 7, (now.tv_sec - start.tv_sec) * 1000 + (now.tv_nsec - start.tv_nsec) / 1000000 >= 150);
	int pending = 0;
	CHECK(step + 8, ioctl(fd, FIONREAD, &pending) == 0 && pending == 32); /* 16 头 + 对齐到 16 的 "f\0" */
	char small[8];
	CHECK(step + 9, read(fd, small, sizeof small) == -1 && errno == EINVAL); /* 缓冲放不下一个事件 */
	char buffer[64];
	ssize_t got = read(fd, buffer, sizeof buffer); /* 已有事件：不阻塞 */
	CHECK(step + 10, got == 32 && ((struct inotify_event *)buffer)->mask == IN_CREATE);
	int status;
	CHECK(step + 11, waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));

	/* 真正的阻塞 read。 */
	child = fork();
	if (child == 0) {
		const struct timespec delay = { 0, 250 * 1000 * 1000 };
		nanosleep(&delay, 0);
		_exit(unlink(file) == 0 && touch_file(file, NULL) == 0 ? 0 : 1);
	}
	clock_gettime(CLOCK_MONOTONIC, &start);
	got = read(fd, buffer, sizeof buffer);
	clock_gettime(CLOCK_MONOTONIC, &now);
	CHECK(step + 12, got > 0);
	CHECK(step + 13, (now.tv_sec - start.tv_sec) * 1000 + (now.tv_nsec - start.tv_nsec) / 1000000 >= 150);
	CHECK(step + 14, waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
	CHECK(step + 15, close(epoll) == 0 && close(fd) == 0 && unlink(file) == 0 && rmdir(directory) == 0);
}

static void unmount_events(void)
{
	CHECK(300, mkdir("/mnt", 0755) == 0 || errno == EEXIST);
	CHECK(301, mkdir("/mnt/inw", 0755) == 0 || errno == EEXIST);
	CHECK(302, mount("tmpfs", "/mnt/inw", "tmpfs", 0, "") == 0);
	CHECK(303, mkdir("/mnt/inw/d", 0755) == 0);
	int fd = inotify_init1(IN_NONBLOCK);
	CHECK(304, fd >= 0 && inotify_add_watch(fd, "/mnt/inw/d", IN_CREATE) > 0);
	/* 被 watch 的目录引用着挂载？watch 不持有引用：卸载成功，并收到 UNMOUNT 与 IGNORED。 */
	CHECK(305, umount("/mnt/inw") == 0);
	struct seen events[8];
	int count = drain(fd, events, 8);
	CHECK(306, count == 2 && (events[0].mask & IN_UNMOUNT) && (events[1].mask & IN_IGNORED));
	CHECK(307, close(fd) == 0);
}

int main(void)
{
	directory_events("/tmp", 0);
	file_events("/tmp", 40);
	waiting("/tmp", 80);
	directory_events("/var/tmp", 100);
	file_events("/var/tmp", 140);
	waiting("/var/tmp", 180);
	unmount_events();
	puts("LITEOS_INOTIFY_42");
	return 0;
}
