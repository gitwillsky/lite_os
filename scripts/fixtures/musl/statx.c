#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>

/* statx 与 stat 逐字段一致，并覆盖 flags、mask、设备号、挂载 identity 与 ext4 创建时间。 */
static int same_as_stat(const struct statx *x, const struct stat *s)
{
	if ((x->stx_mask & STATX_BASIC_STATS) != STATX_BASIC_STATS) return -1;
	if (x->stx_mode != s->st_mode || x->stx_nlink != s->st_nlink) return -2;
	if (x->stx_uid != s->st_uid || x->stx_gid != s->st_gid) return -3;
	if (x->stx_ino != s->st_ino || (uint64_t)x->stx_size != (uint64_t)s->st_size) return -4;
	if (x->stx_blocks != (uint64_t)s->st_blocks || x->stx_blksize != (uint32_t)s->st_blksize) return -5;
	if (x->stx_mtime.tv_sec != s->st_mtim.tv_sec || x->stx_atime.tv_sec != s->st_atim.tv_sec
	    || x->stx_ctime.tv_sec != s->st_ctim.tv_sec) return -6;
	if (x->stx_mtime.tv_nsec != (uint32_t)s->st_mtim.tv_nsec) return -7;
	if (makedev(x->stx_dev_major, x->stx_dev_minor) != s->st_dev) return -8;
	return 0;
}

int verify_statx(void)
{
	struct statx x;
	struct stat s;
	memset(&x, 0xaa, sizeof x);
	if (statx(AT_FDCWD, "/", 0, STATX_BASIC_STATS | STATX_BTIME, &x) != 0) return 1;
	if (stat("/", &s) != 0 || same_as_stat(&x, &s) != 0) return 2;
	if (!S_ISDIR(x.stx_mode)) return 3;
	if (x.stx_attributes_mask != 0 || x.stx_attributes != 0) return 4;

	/* 写一个文件：statx 与 stat 的大小、时间一致；ext4 上还有创建时间。 */
	const char *path = "/tmp/statx-probe";
	unlink(path);
	int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0640);
	if (fd < 0) return 5;
	if (write(fd, "hello statx", 11) != 11) return 6;
	if (statx(AT_FDCWD, path, 0, STATX_ALL, &x) != 0 || fstat(fd, &s) != 0) return 7;
	if (same_as_stat(&x, &s) != 0 || x.stx_size != 11 || (x.stx_mode & 0777) != 0640) return 8;
	if (x.stx_mask & STATX_BTIME) {
		time_t now = time(NULL);
		if (x.stx_btime.tv_sec > now + 2 || x.stx_btime.tv_sec < now - 600) return 9;
	}
	/* AT_EMPTY_PATH 直接操作 fd，与路径查询得到同一个对象。 */
	struct statx by_fd;
	if (statx(fd, "", AT_EMPTY_PATH, STATX_BASIC_STATS, &by_fd) != 0) return 10;
	if (by_fd.stx_ino != x.stx_ino || by_fd.stx_mnt_id != x.stx_mnt_id) return 11;
	if (!(x.stx_mask & STATX_MNT_ID) || x.stx_mnt_id == 0) return 12;
	errno = 0;
	if (statx(fd, "", 0, STATX_BASIC_STATS, &by_fd) == 0 || errno != ENOENT) return 13;

	/* 符号链接：NOFOLLOW 看到链接本身，否则看到目标。 */
	const char *link = "/tmp/statx-link";
	unlink(link);
	if (symlink(path, link) != 0) return 14;
	if (statx(AT_FDCWD, link, AT_SYMLINK_NOFOLLOW, STATX_BASIC_STATS, &x) != 0) return 15;
	if (!S_ISLNK(x.stx_mode)) return 16;
	if (statx(AT_FDCWD, link, 0, STATX_BASIC_STATS, &x) != 0 || !S_ISREG(x.stx_mode)) return 17;

	/* 设备文件的 rdev。 */
	if (statx(AT_FDCWD, "/dev/null", 0, STATX_BASIC_STATS, &x) != 0) return 18;
	if (!S_ISCHR(x.stx_mode) || x.stx_rdev_major != 1 || x.stx_rdev_minor != 3) return 19;

	/* 参数错误。 */
	errno = 0;
	if (statx(AT_FDCWD, "/", 0x80000, STATX_BASIC_STATS, &x) == 0 || errno != EINVAL) return 20;
	errno = 0;
	if (statx(AT_FDCWD, "/", 0, 0x80000000u, &x) == 0 || errno != EINVAL) return 21;
	errno = 0;
	if (statx(AT_FDCWD, "/", AT_STATX_FORCE_SYNC | AT_STATX_DONT_SYNC, STATX_BASIC_STATS, &x) == 0
	    || errno != EINVAL) return 22;
	if (statx(AT_FDCWD, "/", AT_STATX_FORCE_SYNC, STATX_BASIC_STATS, &x) != 0) return 23;
	if (statx(AT_FDCWD, "/", AT_STATX_DONT_SYNC | AT_NO_AUTOMOUNT, STATX_BASIC_STATS, &x) != 0) return 24;
	errno = 0;
	if (statx(AT_FDCWD, "/tmp/statx-missing", 0, STATX_BASIC_STATS, &x) == 0 || errno != ENOENT) return 25;
	errno = 0;
	if (statx(-1 + 1000, "", AT_EMPTY_PATH, STATX_BASIC_STATS, &x) == 0 || errno != EBADF) return 26;
	errno = 0;
	if (statx(AT_FDCWD, "/", 0, STATX_BASIC_STATS, (struct statx *)1) == 0 || errno != EFAULT) return 27;

	close(fd);
	unlink(link);
	unlink(path);
	return 0;
}
