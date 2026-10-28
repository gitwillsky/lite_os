/* FUSE 的真实 guest 验证：本进程挂载 `fuse.probe`，fork 出一个最小内存文件系统守护进程，再作为客户端
 * 经普通 syscall 访问挂载点。
 *
 * 覆盖：INIT 前 /proc/mounts 不阻塞、GETATTR/LOOKUP、创建与超过 max_write 的分块写、重新打开以多页预读读回、mmap、
 * O_CREAT 的原子 CREATE 与守护进程不支持时的 MKNOD+OPEN 回退、KEEP_CACHE 下 AUTO_INVAL_DATA 发现远端改写、close 的 FLUSH 与其错误返回、
 * truncate、READDIRPLUS、rename、mkdir/rmdir/unlink、statfs、xattr 权限委托/长度/错误/ENOSYS 缓存、
 * 守护进程 errno 透传、signal → INTERRUPT → EINTR、
 * FORGET、FUSE_DEV_IOC_CLONE，以及守护进程消失后的 ENOTCONN 与卸载；第二个挂载协商 WRITEBACK_CACHE，验证
 * 缓冲写、close 写回与 mmap 脏页在最后句柄释放前写回；直接 I/O 覆盖 per-open fh、短读、
 * 部分写入、零大小文件读取、混合缓存/COW 映射与 MAP_FIXED 失败保留旧映射。
 *
 * 每一步失败都以步骤编号作为退出码；全部通过才打印唯一的成功 marker。 */
#include <errno.h>
#include <fcntl.h>
#include <linux/fuse.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/wait.h>
#include <sys/xattr.h>
#include <dirent.h>
#include <unistd.h>

#define MOUNTPOINT "/tmp/fuse-probe"
#define MAX_WRITE 131072
#define FILE_BYTES 300000
#define NODES 16
#define FUSE_MAGIC 0x65735546
#define DIRECT_HANDLE (UINT64_C(1) << 63)

static void fail(int step)
{
	dprintf(2, "LITEOS_FUSE_FAILED step=%d errno=%d\n", step, errno);
	_exit(step);
}

#define CHECK(step, condition) do { if (!(condition)) fail(step); } while (0)

/* ---------- 守护进程：一个扁平的内存树 ---------- */

struct node {
	int used;
	uint64_t parent;
	char name[64];
	uint32_t mode;
	uint64_t lookups;
	size_t size;
	uint64_t mtime;
	unsigned char *data;
	int has_xattr;
	size_t xattr_size;
	unsigned char xattr[128];
};

static struct node nodes[NODES + 1];
static unsigned char request[1 << 20];
static unsigned char reply[1 << 20];
static int device;
static int report;
static uint64_t slow_unique;
/* 第二个挂载的守护进程协商 WRITEBACK_CACHE。 */
static int writeback_mode;
/* 控制阶段分别协商/不协商 ABORT_ERROR，核对实际 errno 与后台预算。 */
static int control_mode;

static uint64_t node_of(uint64_t parent, const char *name)
{
	for (uint64_t id = 2; id <= NODES; id++)
		if (nodes[id].used && nodes[id].parent == parent && strcmp(nodes[id].name, name) == 0)
			return id;
	return 0;
}

static void attributes(uint64_t id, struct fuse_attr *attr)
{
	memset(attr, 0, sizeof *attr);
	attr->ino = id;
	attr->mode = nodes[id].mode;
	attr->nlink = S_ISDIR(nodes[id].mode) ? 2 : 1;
	attr->size = strcmp(nodes[id].name, "stream") == 0 ? 0 : nodes[id].size;
	attr->mtime = nodes[id].mtime;
	attr->blocks = (nodes[id].size + 511) / 512;
	attr->blksize = 4096;
}

static void entry(uint64_t id, struct fuse_entry_out *out)
{
	memset(out, 0, sizeof *out);
	out->nodeid = id;
	attributes(id, &out->attr);
	nodes[id].lookups++;
}

/* CREATE 的三个 direct 文件始终直接打开；同一 direct inode 的后续 OPEN 由 NONBLOCK 区分两种 fh。 */
static struct fuse_open_out opened(uint64_t id, int direct)
{
	struct fuse_open_out out = { .fh = id, .open_flags = FOPEN_KEEP_CACHE };
	if (direct) {
		out.fh |= DIRECT_HANDLE;
		out.open_flags |= FOPEN_DIRECT_IO;
	}
	return out;
}

static void send_reply(uint64_t unique, int error, const void *payload, size_t length)
{
	struct fuse_out_header header = {
		.len = sizeof header + length,
		.error = error,
		.unique = unique,
	};
	memcpy(reply, &header, sizeof header);
	if (length)
		memcpy(reply + sizeof header, payload, length);
	if (write(device, reply, header.len) != (ssize_t)header.len)
		_exit(90);
}

static uint64_t add_node(uint64_t parent, const char *name, uint32_t mode)
{
	for (uint64_t id = 2; id <= NODES; id++) {
		if (nodes[id].used)
			continue;
		memset(&nodes[id], 0, sizeof nodes[id]);
		nodes[id].used = 1;
		nodes[id].parent = parent;
		nodes[id].mode = mode;
		snprintf(nodes[id].name, sizeof nodes[id].name, "%s", name);
		if (S_ISREG(mode))
			nodes[id].data = calloc(1, FILE_BYTES);
			if (strcmp(name, "stream") == 0) {
				memcpy(nodes[id].data, "stream", 6);
				nodes[id].size = 6;
			}
		return id;
	}
	return 0;
}

static size_t directory(uint64_t id, uint64_t offset, uint32_t size, int plus, unsigned char *out)
{
	size_t used = 0;
	uint64_t index = 0;
	for (uint64_t child = 2; child <= NODES; child++) {
		if (!nodes[child].used || nodes[child].parent != id)
			continue;
		index++;
		if (index <= offset)
			continue;
		size_t name = strlen(nodes[child].name);
		size_t record = plus ? FUSE_DIRENTPLUS_SIZE(&(struct fuse_direntplus){ .dirent.namelen = name })
				     : FUSE_DIRENT_SIZE(&(struct fuse_dirent){ .namelen = name });
		if (used + record > size)
			break;
		memset(out + used, 0, record);
		struct fuse_dirent *dirent;
		if (plus) {
			struct fuse_direntplus *item = (void *)(out + used);
			entry(child, &item->entry_out);
			dirent = &item->dirent;
		} else {
			dirent = (void *)(out + used);
		}
		dirent->ino = child;
		dirent->off = index;
		dirent->namelen = name;
		dirent->type = nodes[child].mode >> 12;
		memcpy(dirent->name, nodes[child].name, name);
		used += record;
	}
	return used;
}

static void serve(void)
{
	/* 一个多线程守护进程的第二个端点：CLONE 成功，再次绑定同一端点是 EINVAL。 */
	int clone = open("/dev/fuse", O_RDWR);
	uint32_t source = device;
	if (clone < 0 || ioctl(clone, FUSE_DEV_IOC_CLONE, &source) != 0)
		_exit(91);
	if (ioctl(clone, FUSE_DEV_IOC_CLONE, &source) != -1 || errno != EINVAL)
		_exit(92);
	close(clone);

	nodes[1] = (struct node){ .used = 1, .mode = S_IFDIR | 0755 };
	for (;;) {
		ssize_t length = read(device, request, sizeof request);
		if (length < 0 && (errno == ENODEV || errno == ECONNABORTED)) {
			if (control_mode && errno != (control_mode == 2 ? ECONNABORTED : ENODEV))
				_exit(95);
			_exit(0);
		}
		if (length < (ssize_t)sizeof(struct fuse_in_header))
			_exit(93);
		struct fuse_in_header *in = (void *)request;
		void *body = request + sizeof *in;
		uint64_t id = in->nodeid;
		switch (in->opcode) {
		case FUSE_INIT: {
			struct fuse_init_in *init = body;
			struct fuse_init_out out = {
				.major = FUSE_KERNEL_VERSION,
				.minor = FUSE_KERNEL_MINOR_VERSION,
				.max_readahead = init->max_readahead,
				.flags = FUSE_ASYNC_READ | FUSE_BIG_WRITES | FUSE_MAX_PAGES |
					 FUSE_DO_READDIRPLUS | FUSE_PARALLEL_DIROPS | FUSE_AUTO_INVAL_DATA |
					 FUSE_INIT_EXT | (writeback_mode ? FUSE_WRITEBACK_CACHE : 0) |
					 (control_mode == 2 ? FUSE_ABORT_ERROR : 0),
				.max_background = control_mode ? 7 : 0,
				.congestion_threshold = control_mode ? 5 : 0,
				.max_write = MAX_WRITE,
				.max_pages = MAX_WRITE / 4096,
				.time_gran = 1,
			};
			send_reply(in->unique, 0, &out, sizeof out);
			break;
		}
		case FUSE_LOOKUP: {
			const char *name = body;
			if (strcmp(name, "timeout") == 0) {
				send_reply(in->unique, -ETIMEDOUT, NULL, 0);
				break;
			}
			if (strcmp(name, "verify") == 0) {
				/* 报告 wb 的服务器端内容：mmap 写在最后一个句柄释放前已写回。 */
				uint64_t wb = node_of(1, "wb");
				if (wb && memcmp(nodes[wb].data, "MMAP", 4) == 0)
					write(report, "V", 1);
				send_reply(in->unique, -ENOENT, NULL, 0);
				break;
			}
			if (strcmp(name, "mutate") == 0) {
				/* 模拟另一个客户端改写 data：内容与 mtime 都变，大小不变。 */
				uint64_t data = node_of(1, "data");
				memcpy(nodes[data].data, "XXXX", 4);
				nodes[data].mtime++;
				send_reply(in->unique, -ENOENT, NULL, 0);
				break;
			}
			if (strcmp(name, "slow") == 0) {
				write(report, "S", 1);
				slow_unique = in->unique; /* 等 INTERRUPT 再回复 */
				break;
			}
			uint64_t child = node_of(id, name);
			if (!child) {
				send_reply(in->unique, -ENOENT, NULL, 0);
				break;
			}
			struct fuse_entry_out out;
			entry(child, &out);
			send_reply(in->unique, 0, &out, sizeof out);
			break;
		}
		case FUSE_INTERRUPT: {
			struct fuse_interrupt_in *interrupt = body;
			if (interrupt->unique == slow_unique) {
				/* 先报告再回复：客户端收到 EINTR 后立即检查报告。 */
				write(report, "I", 1);
				send_reply(slow_unique, -EINTR, NULL, 0);
				slow_unique = 0;
			}
			break;
		}
		case FUSE_FORGET:
		case FUSE_BATCH_FORGET:
			write(report, "F", 1);
			break;
		case FUSE_GETATTR: {
			struct fuse_attr_out out = { 0 };
			attributes(id, &out.attr);
			send_reply(in->unique, 0, &out, sizeof out);
			break;
		}
		case FUSE_SETATTR: {
			struct fuse_setattr_in *set = body;
			if ((set->valid & FATTR_SIZE) && set->size <= FILE_BYTES) {
				if (set->size < nodes[id].size)
					memset(nodes[id].data + set->size, 0, nodes[id].size - set->size);
				nodes[id].size = set->size;
			}
			if (set->valid & FATTR_MODE)
				nodes[id].mode = (nodes[id].mode & S_IFMT) | (set->mode & 07777);
			struct fuse_attr_out out = { 0 };
			attributes(id, &out.attr);
			send_reply(in->unique, 0, &out, sizeof out);
			break;
		}
		case FUSE_MKNOD:
		case FUSE_MKDIR: {
			uint32_t mode;
			const char *name;
			if (in->opcode == FUSE_MKNOD) {
				mode = ((struct fuse_mknod_in *)body)->mode;
				name = (const char *)body + sizeof(struct fuse_mknod_in);
			} else {
				mode = S_IFDIR | ((struct fuse_mkdir_in *)body)->mode;
				name = (const char *)body + sizeof(struct fuse_mkdir_in);
			}
			uint64_t child = node_of(id, name) ? 0 : add_node(id, name, mode);
			if (!child) {
				send_reply(in->unique, -EEXIST, NULL, 0);
				break;
			}
			struct fuse_entry_out out;
			entry(child, &out);
			send_reply(in->unique, 0, &out, sizeof out);
			break;
		}
		case FUSE_CREATE: {
			/* flushfail 走旧守护进程路径：ENOSYS 让内核记下 no_create，改用 MKNOD + OPEN。 */
			struct fuse_create_in *create = body;
			const char *name = (const char *)body + sizeof *create;
			if (writeback_mode && (create->flags & O_ACCMODE) == O_RDWR)
				write(report, "W", 1); /* 只写打开被升级为读写 */
			if (strcmp(name, "flushfail") == 0) {
				send_reply(in->unique, -ENOSYS, NULL, 0);
				break;
			}
			uint64_t child = node_of(id, name) ? 0 : add_node(id, name, create->mode);
			if (!child) {
				send_reply(in->unique, -EEXIST, NULL, 0);
				break;
			}
			struct {
				struct fuse_entry_out entry;
				struct fuse_open_out open;
			} out = { .open = { .fh = child } };
			if (strcmp(name, "direct") == 0 || strcmp(name, "stream") == 0 || strcmp(name, "partial") == 0)
				out.open = opened(child, 1);
			entry(child, &out.entry);
			write(report, "C", 1);
			send_reply(in->unique, 0, &out, sizeof out);
			break;
		}
		case FUSE_UNLINK:
		case FUSE_RMDIR: {
			uint64_t child = node_of(id, body);
			if (!child) {
				send_reply(in->unique, -ENOENT, NULL, 0);
				break;
			}
			int occupied = 0;
			for (uint64_t other = 2; other <= NODES; other++)
				occupied |= nodes[other].used && nodes[other].parent == child;
			if (in->opcode == FUSE_RMDIR && occupied) {
				send_reply(in->unique, -ENOTEMPTY, NULL, 0);
				break;
			}
			nodes[child].used = 0;
			send_reply(in->unique, 0, NULL, 0);
			break;
		}
		case FUSE_RENAME: {
			struct fuse_rename_in *rename = body;
			const char *old = (const char *)body + sizeof *rename;
			const char *new = old + strlen(old) + 1;
			uint64_t child = node_of(id, old);
			if (!child) {
				send_reply(in->unique, -ENOENT, NULL, 0);
				break;
			}
			nodes[child].parent = rename->newdir;
			snprintf(nodes[child].name, sizeof nodes[child].name, "%s", new);
			send_reply(in->unique, 0, NULL, 0);
			break;
		}
		case FUSE_OPEN:
		case FUSE_OPENDIR: {
			/* data 以 KEEP_CACHE 打开：打开不作废缓存，远端改动只能靠 AUTO_INVAL_DATA 发现。 */
			struct fuse_open_out out = {
				.fh = id,
				.open_flags = strcmp(nodes[id].name, "data") == 0 ? FOPEN_KEEP_CACHE : 0,
			};
			if (strcmp(nodes[id].name, "direct") == 0)
				out = opened(id, ((struct fuse_open_in *)body)->flags & O_NONBLOCK);
			send_reply(in->unique, 0, &out, sizeof out);
			break;
		}
		case FUSE_READ: {
			struct fuse_read_in *read_in = body;
			size_t count = 0;
			if (read_in->size > 4096)
				write(report, "R", 1); /* 页缓存预读把多页合成一个 READ */
			if (read_in->offset < nodes[id].size) {
				count = nodes[id].size - read_in->offset;
				if (count > read_in->size)
					count = read_in->size;
			}
			/* 直接 fh 的短读不是 EOF；选错 inode 全局 fh 会返回完整长度而暴露问题。 */
			if ((read_in->fh & DIRECT_HANDLE) && strcmp(nodes[id].name, "direct") == 0 && count > 3)
				count = 3;
			send_reply(in->unique, 0, nodes[id].data + read_in->offset, count);
			break;
		}
		case FUSE_WRITE: {
			struct fuse_write_in *write_in = body;
			if (write_in->fh & DIRECT_HANDLE) {
				if (write_in->write_flags & FUSE_WRITE_CACHE ||
				    (strcmp(nodes[id].name, "partial") == 0 && write_in->offset >= MAX_WRITE)) {
					send_reply(in->unique, -EIO, NULL, 0);
					break;
				}
				if (write_in->size == 4)
					write_in->size = 2;
			}
			if (write_in->size > MAX_WRITE || write_in->offset + write_in->size > FILE_BYTES)
				_exit(94);
			memcpy(nodes[id].data + write_in->offset, (char *)body + sizeof *write_in, write_in->size);
			write(report, (write_in->write_flags & FUSE_WRITE_CACHE) ? "K" : "w", 1);
			if (write_in->offset + write_in->size > nodes[id].size)
				nodes[id].size = write_in->offset + write_in->size;
			nodes[id].mtime++;
			struct fuse_write_out out = { .size = write_in->size };
			send_reply(in->unique, 0, &out, sizeof out);
			break;
		}
		case FUSE_READDIR:
		case FUSE_READDIRPLUS: {
			struct fuse_read_in *read_in = body;
			static unsigned char listing[65536];
			size_t used = directory(id, read_in->offset, read_in->size,
						in->opcode == FUSE_READDIRPLUS, listing);
			send_reply(in->unique, 0, listing, used);
			break;
		}
		case FUSE_SETXATTR: {
			struct fuse_setxattr_in *set = body;
			const char *name = (char *)body + FUSE_COMPAT_SETXATTR_IN_SIZE;
			const unsigned char *value = (const void *)(name + strlen(name) + 1);
			if (strcmp(name, "user.DOSATTRIB") != 0)
				send_reply(in->unique, -EOPNOTSUPP, NULL, 0);
			else if (set->flags & XATTR_CREATE && nodes[id].has_xattr)
				send_reply(in->unique, -EEXIST, NULL, 0);
			else if (set->flags & XATTR_REPLACE && !nodes[id].has_xattr)
				send_reply(in->unique, -ENODATA, NULL, 0);
			else if (set->size > sizeof nodes[id].xattr)
				send_reply(in->unique, -E2BIG, NULL, 0);
			else {
				memcpy(nodes[id].xattr, value, set->size);
				nodes[id].xattr_size = set->size;
				nodes[id].has_xattr = 1;
				send_reply(in->unique, 0, NULL, 0);
			}
			break;
		}
		case FUSE_GETXATTR:
		case FUSE_LISTXATTR: {
			struct fuse_getxattr_in *get = body;
			const char *name = (char *)body + sizeof *get;
			if (in->opcode == FUSE_GETXATTR && strcmp(name, "user.unsupported") == 0) {
				write(report, "X", 1);
				send_reply(in->unique, -ENOSYS, NULL, 0);
				break;
			}
			if (in->opcode == FUSE_GETXATTR &&
			    (strcmp(name, "user.DOSATTRIB") != 0 || !nodes[id].has_xattr)) {
				send_reply(in->unique, -ENODATA, NULL, 0);
				break;
			}
			const void *value = nodes[id].xattr;
			size_t size = nodes[id].xattr_size;
			if (in->opcode == FUSE_LISTXATTR) {
				value = "user.DOSATTRIB";
				size = nodes[id].has_xattr ? sizeof "user.DOSATTRIB" : 0;
				if (strcmp(nodes[id].name, "bad-list") == 0) {
					value = "unterminated";
					size = strlen(value);
				}
			}
			if (!get->size) {
				struct fuse_getxattr_out out = { .size = size };
				send_reply(in->unique, 0, &out, sizeof out);
			} else if (get->size < size)
				send_reply(in->unique, -ERANGE, NULL, 0);
			else
				send_reply(in->unique, 0, value, size);
			break;
		}
		case FUSE_REMOVEXATTR:
			if (strcmp(body, "user.DOSATTRIB") != 0 || !nodes[id].has_xattr)
				send_reply(in->unique, -ENODATA, NULL, 0);
			else {
				nodes[id].has_xattr = 0;
				send_reply(in->unique, 0, NULL, 0);
			}
			break;
		case FUSE_STATFS: {
			struct fuse_statfs_out out = { .st = { .blocks = 1000, .bfree = 600, .bavail = 500,
							       .files = NODES, .ffree = 3, .bsize = 4096,
							       .namelen = 255, .frsize = 4096 } };
			send_reply(in->unique, 0, &out, sizeof out);
			break;
		}
		case FUSE_FLUSH:
			write(report, "L", 1);
			send_reply(in->unique, strcmp(nodes[id].name, "flushfail") == 0 ? -EIO : 0, NULL, 0);
			break;
		case FUSE_RELEASE:
		case FUSE_RELEASEDIR:
		case FUSE_FSYNC:
			send_reply(in->unique, 0, NULL, 0);
			break;
		default:
			send_reply(in->unique, -ENOSYS, NULL, 0);
			break;
		}
	}
}

/* ---------- 客户端 ---------- */

static void on_alarm(int signal)
{
	(void)signal;
}

static unsigned char expected(size_t index)
{
	return (unsigned char)(index * 7 % 251);
}

static int mounts_contains(const char *needle)
{
	static char text[16384];
	int fd = open("/proc/mounts", O_RDONLY);
	if (fd < 0)
		return 0;
	ssize_t length = read(fd, text, sizeof text - 1);
	close(fd);
	if (length < 0)
		return 0;
	text[length] = 0;
	return strstr(text, needle) != NULL;
}

/* 挂载 `point` 并 fork 守护进程；返回守护进程 pid，`events` 是其报告管道的读端。 */
static pid_t start(const char *point, int *events)
{
	char options[128];
	int reports[2];
	device = open("/dev/fuse", O_RDWR);
	CHECK(200, device >= 0);
	CHECK(201, mkdir(point, 0755) == 0 || errno == EEXIST);
	snprintf(options, sizeof options, "fd=%d,rootmode=40000,user_id=0,group_id=0,allow_other", device);
	CHECK(202, mount("probe", point, "fuse.probe", 0, options) == 0);
	CHECK(203, pipe2(reports, O_CLOEXEC | O_NONBLOCK) == 0);
	pid_t daemon = fork();
	CHECK(204, daemon >= 0);
	if (daemon == 0) {
		close(reports[0]);
		report = reports[1];
		serve();
	}
	close(reports[1]);
	CHECK(205, close(device) == 0);
	*events = reports[0];
	return daemon;
}

static int drain(int events, char wanted)
{
	char event;
	int seen = 0;
	while (read(events, &event, 1) == 1)
		seen += event == wanted;
	return seen;
}

/* WRITEBACK_CACHE：写只进页缓存，close 才交给守护进程；mmap 脏页在最后一个句柄释放前写回。 */
static void writeback_phase(void)
{
	static char data[1000];
	struct stat info;
	int events;
	writeback_mode = 1;
	pid_t daemon = start("/tmp/fuse-wb", &events);
	int fd = open("/tmp/fuse-wb/wb", O_CREAT | O_WRONLY, 0644);
	CHECK(210, fd >= 0);
	for (int i = 0; i < 10; i++) {
		memset(data + i * 100, 'a' + i, 100);
		CHECK(211, write(fd, data + i * 100, 100) == 100);
	}
	CHECK(212, fstat(fd, &info) == 0 && info.st_size == 1000);
	/* 读事件前先做一次需要往返的请求，确保守护进程已处理之前的全部消息。 */
	CHECK(213, stat("/tmp/fuse-wb/none", &info) == -1 && errno == ENOENT);
	char event;
	int upgraded = 0, early = 0;
	while (read(events, &event, 1) == 1) {
		upgraded |= event == 'W';
		early |= event == 'w' || event == 'K';
	}
	CHECK(214, upgraded && !early);
	CHECK(215, close(fd) == 0);
	CHECK(216, stat("/tmp/fuse-wb/none", &info) == -1 && errno == ENOENT);
	CHECK(217, drain(events, 'K') > 0);
	static char back[1000];
	fd = open("/tmp/fuse-wb/wb", O_RDONLY);
	CHECK(218, fd >= 0 && read(fd, back, sizeof back) == 1000 && memcmp(back, data, 1000) == 0);
	CHECK(219, close(fd) == 0);

	fd = open("/tmp/fuse-wb/wb", O_RDWR);
	CHECK(220, fd >= 0);
	char *mapped = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	CHECK(221, mapped != MAP_FAILED);
	memcpy(mapped, "MMAP", 4);
	CHECK(222, munmap(mapped, 4096) == 0 && close(fd) == 0);
	CHECK(223, stat("/tmp/fuse-wb/verify", &info) == -1 && errno == ENOENT);
	CHECK(224, drain(events, 'V') == 1);
	CHECK(225, kill(daemon, SIGKILL) == 0 && waitpid(daemon, NULL, 0) == daemon);
	CHECK(226, umount("/tmp/fuse-wb") == 0);
}

/* 同一挂载启用 WRITEBACK_CACHE：DIRECT_IO 必须仍立即使用本次打开的 fh，不缓冲、不钳制 EOF。 */
static void direct_phase(void)
{
	static char data[MAX_WRITE + 4096];
	struct stat info;
	int events;
	writeback_mode = 1;
	pid_t daemon = start("/tmp/fuse-dio", &events);
	int direct = open("/tmp/fuse-dio/direct", O_CREAT | O_RDWR, 0644);
	memset(data, 'a', 8192);
	CHECK(140, direct >= 0 && write(direct, data, 8192) == 8192);
	CHECK(141, close(direct) == 0);
	int cached = open("/tmp/fuse-dio/direct", O_RDWR);
	CHECK(142, cached >= 0 && pread(cached, data, 8192, 0) == 8192 && data[8191] == 'a');
	direct = open("/tmp/fuse-dio/direct", O_RDWR | O_NONBLOCK);
	CHECK(143, direct >= 0 && pread(direct, data, 16, 0) == 3);
	CHECK(144, fstat(direct, &info) == 0 && info.st_size == 8192);
	char *shared = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, cached, 0);
	char *cow = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE, cached, 0);
	char *clean = mmap(NULL, 8192, PROT_READ, MAP_PRIVATE, direct, 0);
	CHECK(145, shared != MAP_FAILED && cow != MAP_FAILED && clean != MAP_FAILED);
	CHECK(146, clean[0] == 'a');
	cow[0] = 'P';
	shared[0] = 'M';
	/* 撤销 cache aliases 并写回 mmap 脏页后，只提交服务端确认的两个字节；COW 私有内容不丢失。 */
	CHECK(147, pwrite(direct, "ABCD", 4, 0) == 2);
	CHECK(148, shared[0] == 'A' && shared[1] == 'B' && clean[0] == 'A' && cow[0] == 'P');
	CHECK(149, pread(cached, data, 4, 0) == 4 && memcmp(data, "ABaa", 4) == 0);
	CHECK(150, munmap(shared, 8192) == 0 && munmap(cow, 8192) == 0 && munmap(clean, 8192) == 0);
	char *old = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	CHECK(151, old != MAP_FAILED);
	old[0] = 'q';
	CHECK(152, mmap(old, 4096, PROT_READ, MAP_SHARED | MAP_FIXED, direct, 0) == MAP_FAILED && errno == ENODEV);
	CHECK(153, old[0] == 'q' && munmap(old, 4096) == 0);
	CHECK(154, close(direct) == 0 && close(cached) == 0);

	direct = open("/tmp/fuse-dio/stream", O_CREAT | O_RDWR, 0644);
	CHECK(155, direct >= 0 && fstat(direct, &info) == 0 && info.st_size == 0);
	CHECK(156, pread(direct, data, 16, 0) == 6 && memcmp(data, "stream", 6) == 0);
	CHECK(157, close(direct) == 0);
	direct = open("/tmp/fuse-dio/partial", O_CREAT | O_RDWR, 0644);
	memset(data, 'p', sizeof data);
	CHECK(158, direct >= 0 && write(direct, data, sizeof data) == MAX_WRITE);
	CHECK(159, fstat(direct, &info) == 0 && info.st_size == MAX_WRITE && close(direct) == 0);
	CHECK(160, kill(daemon, SIGKILL) == 0 && waitpid(daemon, NULL, 0) == daemon);
	CHECK(161, umount("/tmp/fuse-dio") == 0 && close(events) == 0);
}

static long control_waiting(const char *path)
{
	char number[32];
	int fd = open(path, O_RDONLY);
	CHECK(170, fd >= 0);
	ssize_t length = read(fd, number, sizeof number - 1);
	CHECK(171, length > 0 && close(fd) == 0);
	number[length] = 0;
	return strtol(number, NULL, 10);
}

/* 对真实控制 fd 调参、积压/放行 RELEASE，再 abort 在途 LOOKUP；旧 fd 不延长挂载可见性。 */
static void control_phase(int mode)
{
	char waiting[256], maximum[256], threshold[256], abort_path[256], directory[128];
	char number[16];
	static char data[8192];
	struct stat info;
	struct statfs filesystem;
	int events;
	writeback_mode = 0;
	control_mode = mode;
	pid_t daemon = start("/tmp/fuse-control", &events);
	CHECK(172, stat("/tmp/fuse-control", &info) == 0);
	snprintf(directory, sizeof directory, "/sys/fs/fuse/connections/%llu", (unsigned long long)info.st_dev);
	snprintf(waiting, sizeof waiting, "%s/waiting", directory);
	snprintf(maximum, sizeof maximum, "%s/max_background", directory);
	snprintf(threshold, sizeof threshold, "%s/congestion_threshold", directory);
	snprintf(abort_path, sizeof abort_path, "%s/abort", directory);
	CHECK(173, statfs(directory, &filesystem) == 0 && filesystem.f_type == 0x65735543);
	CHECK(174, stat(directory, &info) == 0 && (info.st_mode & 0777) == 0500);
	int limit = open(maximum, O_RDWR | O_TRUNC);
	CHECK(175, limit >= 0 && read(limit, number, sizeof number) == 2 && memcmp(number, "7\n", 2) == 0);
	CHECK(176, lseek(limit, 0, SEEK_SET) == 0 && write(limit, "0", 1) == 1 && write(limit, "0", 1) == 1);
	int fd = open("/tmp/fuse-control/file", O_CREAT | O_RDWR, 0644);
	CHECK(177, fd >= 0 && control_waiting(waiting) == 0);
	CHECK(178, close(fd) == 0 && control_waiting(waiting) == 1);
	CHECK(179, write(limit, "2", 1) == 1 && stat("/tmp/fuse-control/missing", &info) == -1 && errno == ENOENT);
	CHECK(180, control_waiting(waiting) == 0);
	CHECK(181, pwrite(limit, "3", 1, 1) == -1 && errno == EINVAL);
	CHECK(182, pwrite(limit, "65536", 5, 0) == -1 && errno == EINVAL);
	CHECK(183, pwrite(limit, "0x3\n", 4, 0) == 4);
	int congestion = open(threshold, O_WRONLY | O_TRUNC);
	CHECK(184, congestion >= 0 && write(congestion, "0", 1) == 1 && close(congestion) == 0);
	fd = open("/tmp/fuse-control/data", O_CREAT | O_RDWR, 0644);
	CHECK(185, fd >= 0 && write(fd, data, sizeof data) == sizeof data);
	(void)drain(events, 'R');
	CHECK(186, pread(fd, data, sizeof data, 0) == sizeof data && drain(events, 'R') == 0 && close(fd) == 0);
	fd = open(waiting, O_RDONLY);
	CHECK(187, fd >= 0 && pread(fd, number, 1, 0) == -1 && errno == ESPIPE);
	CHECK(188, lseek(fd, 0, SEEK_SET) == -1 && errno == ESPIPE && close(fd) == 0);
	CHECK(199, stat("/tmp/fuse-control/missing", &info) == -1 && errno == ENOENT && control_waiting(waiting) == 0);
	pid_t caller = fork();
	CHECK(189, caller >= 0);
	if (caller == 0) {
		int result = stat("/tmp/fuse-control/slow", &info);
		CHECK(190, result == -1 && errno == ECONNABORTED);
		_exit(0);
	}
	int sent = 0;
	for (int attempt = 0; attempt < 2000 && !(sent = drain(events, 'S')); attempt++)
		usleep(1000);
	CHECK(191, sent && control_waiting(waiting) > 0);
	int abort_fd = open(abort_path, O_WRONLY);
	CHECK(192, abort_fd >= 0 && write(abort_fd, "1", 1) == 1);
	int status;
	CHECK(193, waitpid(caller, &status, 0) == caller && WIFEXITED(status) && WEXITSTATUS(status) == 0);
	CHECK(194, waitpid(daemon, &status, 0) == daemon && WIFEXITED(status) && WEXITSTATUS(status) == 0);
	fd = open(waiting, O_RDONLY);
	CHECK(195, fd >= 0 && control_waiting(waiting) == 0 && umount("/tmp/fuse-control") == 0);
	CHECK(196, read(fd, number, sizeof number) == 0 && pwrite(limit, "4", 1, 0) == 1 && write(abort_fd, "1", 1) == 1);
	CHECK(197, stat(directory, &info) == -1 && errno == ENOENT);
	CHECK(198, close(fd) == 0 && close(limit) == 0 && close(abort_fd) == 0 && close(events) == 0);
}

int main(void)
{
	static unsigned char buffer[FILE_BYTES];
	char options[128];
	struct stat info;
	int reports[2];

	device = open("/dev/fuse", O_RDWR);
	CHECK(1, device >= 0);
	CHECK(2, read(device, buffer, 8192) == -1 && errno == EPERM);
	CHECK(3, mkdir(MOUNTPOINT, 0755) == 0 || errno == EEXIST);
	snprintf(options, sizeof options, "fd=%d,rootmode=40000,user_id=0,group_id=0,allow_other", device);
	CHECK(4, mount("probe", MOUNTPOINT, "fuse.probe", MS_NOSUID | MS_NODEV, options) == 0);
	/* 守护进程尚未开始服务：mount table 不得等待 INIT 或 statfs。 */
	CHECK(5, mounts_contains("probe " MOUNTPOINT " fuse.probe rw,nosuid,nodev"));
	CHECK(6, mounts_contains("user_id=0,group_id=0,allow_other"));
	CHECK(7, pipe2(reports, O_CLOEXEC | O_NONBLOCK) == 0);

	pid_t daemon = fork();
	CHECK(8, daemon >= 0);
	if (daemon == 0) {
		close(reports[0]);
		report = reports[1];
		serve();
	}
	close(reports[1]);
	CHECK(9, close(device) == 0);

	CHECK(10, stat(MOUNTPOINT, &info) == 0 && S_ISDIR(info.st_mode) && (info.st_mode & 07777) == 0755);

	/* 创建并分块写：300000 字节超过 max_write，必须拆成多个 WRITE。 */
	for (size_t i = 0; i < FILE_BYTES; i++)
		buffer[i] = expected(i);
	int fd = open(MOUNTPOINT "/data", O_CREAT | O_EXCL | O_WRONLY, 0644);
	CHECK(20, fd >= 0);
	CHECK(21, write(fd, buffer, FILE_BYTES) == FILE_BYTES);
	CHECK(22, close(fd) == 0);
	CHECK(23, stat(MOUNTPOINT "/data", &info) == 0 && S_ISREG(info.st_mode) && info.st_size == FILE_BYTES);
	/* 属性保留空值/CREATE/REPLACE，并通过 size=0 查询；不在本地制造第二份 xattr 内容。 */
	CHECK(110, getxattr(MOUNTPOINT "/data", "user.DOSATTRIB", NULL, 0) == -1 && errno == ENODATA);
	CHECK(111, setxattr(MOUNTPOINT "/data", "user.DOSATTRIB", "0x20", 4, XATTR_CREATE) == 0);
	CHECK(112, setxattr(MOUNTPOINT "/data", "user.DOSATTRIB", "", 0, XATTR_CREATE) == -1 && errno == EEXIST);
	CHECK(113, getxattr(MOUNTPOINT "/data", "user.DOSATTRIB", NULL, 0) == 4);
	CHECK(114, getxattr(MOUNTPOINT "/data", "user.DOSATTRIB", buffer, 2) == -1 && errno == ERANGE);
	CHECK(115, getxattr(MOUNTPOINT "/data", "user.DOSATTRIB", buffer, 4) == 4 && memcmp(buffer, "0x20", 4) == 0);
	CHECK(116, listxattr(MOUNTPOINT "/data", NULL, 0) == sizeof "user.DOSATTRIB");
	CHECK(117, listxattr(MOUNTPOINT "/data", (char *)buffer, 64) == sizeof "user.DOSATTRIB" &&
		  memcmp(buffer, "user.DOSATTRIB", sizeof "user.DOSATTRIB") == 0);
	/* 未设置 default_permissions：服务端允许普通用户写 root:0644 文件属性，本地 DAC 不能抢先拒绝。 */
	pid_t caller = fork();
	CHECK(118, caller >= 0);
	if (caller == 0) {
		CHECK(119, setgid(1000) == 0 && setuid(1000) == 0);
		CHECK(120, setxattr(MOUNTPOINT "/data", "user.DOSATTRIB", "", 0, XATTR_REPLACE) == 0);
		_exit(0);
	}
	int caller_status;
	CHECK(121, waitpid(caller, &caller_status, 0) == caller && WIFEXITED(caller_status) && WEXITSTATUS(caller_status) == 0);
	CHECK(122, getxattr(MOUNTPOINT "/data", "user.DOSATTRIB", buffer, 4) == 0);
	CHECK(123, removexattr(MOUNTPOINT "/data", "user.DOSATTRIB") == 0);
	CHECK(124, removexattr(MOUNTPOINT "/data", "user.DOSATTRIB") == -1 && errno == ENODATA);
	CHECK(125, listxattr(MOUNTPOINT "/data", NULL, 0) == 0);
	int malformed = open(MOUNTPOINT "/bad-list", O_CREAT | O_RDWR, 0644);
	CHECK(126, malformed >= 0 && listxattr(MOUNTPOINT "/bad-list", (char *)buffer, 64) == -1 && errno == EIO);
	CHECK(127, close(malformed) == 0 && unlink(MOUNTPOINT "/bad-list") == 0);
	/* ENOSYS 对该 opcode 缓存：第二次不再向守护进程发请求，其他 xattr opcode 不受影响。 */
	CHECK(128, getxattr(MOUNTPOINT "/data", "user.unsupported", NULL, 0) == -1 && errno == EOPNOTSUPP);
	CHECK(129, getxattr(MOUNTPOINT "/data", "user.unsupported", NULL, 0) == -1 && errno == EOPNOTSUPP);
	CHECK(130, setxattr(MOUNTPOINT "/data", "user.DOSATTRIB", "", 0, 0) == 0);

	/* 重新打开读回（无 KEEP_CACHE：打开作废页缓存，内容来自守护进程）。 */
	memset(buffer, 0, sizeof buffer);
	fd = open(MOUNTPOINT "/data", O_RDONLY);
	CHECK(30, fd >= 0);
	size_t done = 0;
	while (done < FILE_BYTES) {
		ssize_t got = read(fd, buffer + done, FILE_BYTES - done);
		CHECK(31, got > 0);
		done += got;
	}
	CHECK(32, read(fd, buffer, 1) == 0);
	for (size_t i = 0; i < FILE_BYTES; i++)
		CHECK(33, buffer[i] == expected(i));

	/* mmap 经同一页缓存。 */
	unsigned char *mapped = mmap(NULL, FILE_BYTES, PROT_READ, MAP_SHARED, fd, 0);
	CHECK(34, mapped != MAP_FAILED);
	CHECK(35, mapped[0] == expected(0) && mapped[4096 * 20 + 3] == expected(4096 * 20 + 3) &&
		  mapped[FILE_BYTES - 1] == expected(FILE_BYTES - 1));
	CHECK(36, munmap(mapped, FILE_BYTES) == 0 && close(fd) == 0);

	/* AUTO_INVAL_DATA：KEEP_CACHE 打开后内容已在缓存；守护进程改写并更新 mtime，同一 fd 再读必须看到新内容。 */
	fd = open(MOUNTPOINT "/data", O_RDONLY);
	CHECK(37, fd >= 0 && pread(fd, buffer, 4, 0) == 4 && buffer[1] == expected(1));
	CHECK(38, stat(MOUNTPOINT "/mutate", &info) == -1 && errno == ENOENT);
	CHECK(39, pread(fd, buffer, 4, 0) == 4 && memcmp(buffer, "XXXX", 4) == 0 && close(fd) == 0);

	/* close(2) 发 FLUSH，守护进程的错误由 close 返回，fd 仍被关闭。 */
	fd = open(MOUNTPOINT "/flushfail", O_CREAT | O_WRONLY, 0644);
	CHECK(42, fd >= 0);
	CHECK(43, close(fd) == -1 && errno == EIO);
	CHECK(44, fcntl(fd, F_GETFD) == -1 && errno == EBADF);
	CHECK(45, unlink(MOUNTPOINT "/flushfail") == 0);

	/* truncate → SETATTR(size)。 */
	CHECK(40, truncate(MOUNTPOINT "/data", 1000) == 0);
	CHECK(41, stat(MOUNTPOINT "/data", &info) == 0 && info.st_size == 1000);

	/* 目录：mkdir、READDIRPLUS 列表、rename、rmdir。 */
	CHECK(50, mkdir(MOUNTPOINT "/sub", 0700) == 0);
	CHECK(51, stat(MOUNTPOINT "/sub", &info) == 0 && S_ISDIR(info.st_mode) && (info.st_mode & 0777) == 0700);
	DIR *listing = opendir(MOUNTPOINT);
	CHECK(52, listing != NULL);
	int seen = 0;
	for (struct dirent *item; (item = readdir(listing)) != NULL;) {
		if (strcmp(item->d_name, "data") == 0 && item->d_type == DT_REG)
			seen |= 1;
		if (strcmp(item->d_name, "sub") == 0 && item->d_type == DT_DIR)
			seen |= 2;
	}
	CHECK(53, seen == 3 && closedir(listing) == 0);
	CHECK(54, rename(MOUNTPOINT "/data", MOUNTPOINT "/sub/moved") == 0);
	CHECK(55, stat(MOUNTPOINT "/data", &info) == -1 && errno == ENOENT);
	CHECK(56, stat(MOUNTPOINT "/sub/moved", &info) == 0 && info.st_size == 1000);
	CHECK(57, rmdir(MOUNTPOINT "/sub") == -1 && errno == ENOTEMPTY);
	CHECK(58, unlink(MOUNTPOINT "/sub/moved") == 0);
	CHECK(59, rmdir(MOUNTPOINT "/sub") == 0);
	CHECK(60, stat(MOUNTPOINT "/sub", &info) == -1 && errno == ENOENT);

	/* statfs 来自守护进程。 */
	struct statfs filesystem;
	CHECK(70, statfs(MOUNTPOINT, &filesystem) == 0);
	CHECK(71, filesystem.f_type == FUSE_MAGIC && filesystem.f_blocks == 1000 &&
		  filesystem.f_bavail == 500 && filesystem.f_bsize == 4096);

	/* 守护进程的 errno 原样到达调用者。 */
	CHECK(80, stat(MOUNTPOINT "/timeout", &info) == -1 && errno == ETIMEDOUT);

	/* signal 打断已发出的请求：内核补发 INTERRUPT，守护进程以 EINTR 回复。 */
	struct sigaction action = { .sa_handler = on_alarm };
	CHECK(90, sigaction(SIGALRM, &action, NULL) == 0);
	alarm(1);
	CHECK(91, stat(MOUNTPOINT "/slow", &info) == -1 && errno == EINTR);
	char event = 0;
	int interrupted = 0, forgotten = 0, readahead = 0, flushed = 0, created = 0, no_getxattr = 0;
	while (read(reports[0], &event, 1) == 1) {
		interrupted |= event == 'I';
		forgotten |= event == 'F';
		readahead |= event == 'R';
		flushed |= event == 'L';
		created |= event == 'C';
		no_getxattr += event == 'X';
	}
	CHECK(96, created);
	CHECK(131, no_getxattr == 1);
	CHECK(95, flushed);
	CHECK(92, interrupted);
	CHECK(94, readahead);
	/* 删除后的 inode 引用全部释放，守护进程收到 FORGET。 */
	CHECK(93, forgotten);

	/* 守护进程消失：连接中止，访问得到 ENOTCONN，卸载仍成功。 */
	CHECK(100, kill(daemon, SIGKILL) == 0);
	int status;
	CHECK(101, waitpid(daemon, &status, 0) == daemon && WIFSIGNALED(status));
	CHECK(102, stat(MOUNTPOINT "/anything", &info) == -1 && errno == ENOTCONN);
	CHECK(103, umount(MOUNTPOINT) == 0);
	CHECK(104, !mounts_contains("fuse.probe"));
	writeback_phase();
	direct_phase();
	control_phase(1);
	control_phase(2);
	puts("LITEOS_FUSE_42");
	return 0;
}
