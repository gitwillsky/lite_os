/* 内存型文件（memfd 与 tmpfs）的真实 guest 验证：共享映射一致性、截断、seal、配额与 unlink 后的空间归还。
 *
 * 每一步失败都以步骤编号作为退出码，便于从串口输出定位；全部通过才打印唯一的成功 marker。 */
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096

static void fail(int step)
{
	dprintf(2, "LITEOS_MEMFILE_FAILED step=%d errno=%d\n", step, errno);
	_exit(step);
}

#define CHECK(step, condition) do { if (!(condition)) fail(step); } while (0)

/* 子进程触碰 `address`：返回 1 表示按预期收到 SIGBUS，0 表示正常访问成功，-1 其他。 */
static int touch_in_child(volatile char *address, int write_access)
{
	int status;
	pid_t child = fork();
	if (child == 0) {
		if (write_access) *address = 1;
		else (void)*address;
		_exit(0);
	}
	if (child < 0 || waitpid(child, &status, 0) != child) return -1;
	if (WIFSIGNALED(status) && WTERMSIG(status) == SIGBUS) return 1;
	return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : -1;
}

static long free_blocks(const char *path)
{
	struct statfs fs;
	return statfs(path, &fs) == 0 ? (long)fs.f_bfree : -1;
}

static void memfd_semantics(void)
{
	char buffer[16];
	int fd = memfd_create("probe", MFD_ALLOW_SEALING);
	CHECK(1, fd >= 0);
	CHECK(2, ftruncate(fd, 2 * PAGE) == 0);
	char *map = mmap(0, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	CHECK(3, map != MAP_FAILED);

	/* 映射写入对 read 立即可见，write 对映射立即可见。 */
	memcpy(map + PAGE + 10, "mapped", 6);
	CHECK(4, pread(fd, buffer, 6, PAGE + 10) == 6 && memcmp(buffer, "mapped", 6) == 0);
	CHECK(5, pwrite(fd, "written", 7, 100) == 7 && memcmp(map + 100, "written", 7) == 0);

	/* fork 后子进程经同一共享映射写入，父进程看得到。 */
	pid_t child = fork();
	if (child == 0) {
		memcpy(map + 200, "child", 5);
		_exit(0);
	}
	int status;
	CHECK(6, child > 0 && waitpid(child, &status, 0) == child && WIFEXITED(status));
	CHECK(7, memcmp(map + 200, "child", 5) == 0);

	/* 缩小：被截掉的页上的映射访问得到 SIGBUS，保留页不受影响。 */
	CHECK(8, ftruncate(fd, PAGE) == 0);
	CHECK(9, touch_in_child(map + PAGE, 0) == 1);
	CHECK(10, touch_in_child(map, 0) == 0);

	/* 再增长：被截掉的内容读为零，不会复活。 */
	CHECK(11, ftruncate(fd, 2 * PAGE) == 0);
	CHECK(12, pread(fd, buffer, 6, PAGE + 10) == 6);
	for (int i = 0; i < 6; i++) CHECK(13, buffer[i] == 0);
	CHECK(14, map[PAGE + 10] == 0);

	/* seal：GROW|SHRINK 之后 ftruncate 被拒绝，seal 掩码可读回，SEAL_SEAL 之后不能再加。 */
	CHECK(15, fcntl(fd, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK) == 0);
	CHECK(16, (fcntl(fd, F_GET_SEALS) & (F_SEAL_GROW | F_SEAL_SHRINK)) == (F_SEAL_GROW | F_SEAL_SHRINK));
	CHECK(17, ftruncate(fd, 4 * PAGE) == -1 && errno == EPERM);
	CHECK(18, ftruncate(fd, PAGE) == -1 && errno == EPERM);
	CHECK(19, fcntl(fd, F_ADD_SEALS, F_SEAL_SEAL) == 0);
	CHECK(20, fcntl(fd, F_ADD_SEALS, F_SEAL_WRITE) == -1);
	CHECK(21, munmap(map, 2 * PAGE) == 0 && close(fd) == 0);

	/* 未声明 MFD_ALLOW_SEALING 的 memfd 初始即带 SEAL_SEAL。 */
	fd = memfd_create("sealed", 0);
	CHECK(22, fd >= 0 && fcntl(fd, F_ADD_SEALS, F_SEAL_GROW) == -1 && errno == EPERM);
	CHECK(23, close(fd) == 0);
}

static void tmpfs_semantics(void)
{
	char buffer[8];
	struct stat before;
	struct stat after;
	long baseline = free_blocks("/tmp");
	CHECK(30, baseline > 0);

	int fd = open("/tmp/memfile-probe", O_RDWR | O_CREAT | O_EXCL, 0600);
	CHECK(31, fd >= 0 && ftruncate(fd, 3 * PAGE) == 0);
	CHECK(32, fstat(fd, &before) == 0 && before.st_size == 3 * PAGE && before.st_blocks == 0);

	/* 洞读为零且不占空间；写入的页才计入 st_blocks 与 statfs。 */
	char *shared = mmap(0, 3 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	CHECK(33, shared != MAP_FAILED && shared[PAGE] == 0);
	memcpy(shared + 2 * PAGE, "tmpfs-page", 10);
	CHECK(34, msync(shared, 3 * PAGE, MS_SYNC) == 0);
	CHECK(35, fstat(fd, &after) == 0 && after.st_blocks >= 8);
	CHECK(36, free_blocks("/tmp") < baseline);

	/* MAP_PRIVATE 写入走 COW，不改文件。 */
	char *private_map = mmap(0, PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 2 * PAGE);
	CHECK(37, private_map != MAP_FAILED && memcmp(private_map, "tmpfs-page", 10) == 0);
	memcpy(private_map, "PRIVATE!!!", 10);
	CHECK(38, pread(fd, buffer, 6, 2 * PAGE) == 6 && memcmp(buffer, "tmpfs-", 6) == 0);

	/* unlink 之后映射与 fd 仍可访问；空间在最后一个引用消失后立即归还。 */
	CHECK(39, unlink("/tmp/memfile-probe") == 0);
	CHECK(40, memcmp(shared + 2 * PAGE, "tmpfs-page", 10) == 0);
	CHECK(41, free_blocks("/tmp") < baseline);
	CHECK(42, munmap(shared, 3 * PAGE) == 0 && munmap(private_map, PAGE) == 0);
	CHECK(43, free_blocks("/tmp") < baseline);
	CHECK(44, close(fd) == 0);
	CHECK(45, free_blocks("/tmp") == baseline);
}

static void quota_semantics(void)
{
	/* size=16k 的 tmpfs：写入超出配额得到 ENOSPC，而映射触碰超出配额的页得到 SIGBUS。 */
	CHECK(50, mkdir("/mnt", 0755) == 0 || errno == EEXIST);
	CHECK(51, mkdir("/mnt/quota", 0755) == 0 || errno == EEXIST);
	CHECK(52, mount("tmpfs", "/mnt/quota", "tmpfs", 0, "size=16k") == 0);
	int fd = open("/mnt/quota/file", O_RDWR | O_CREAT, 0600);
	CHECK(53, fd >= 0 && ftruncate(fd, 8 * PAGE) == 0);
	char *map = mmap(0, 8 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	CHECK(54, map != MAP_FAILED);
	for (int page = 0; page < 4; page++) map[page * PAGE] = 1;
	CHECK(55, touch_in_child(map + 4 * PAGE, 1) == 1);
	CHECK(56, pwrite(fd, "x", 1, 6 * PAGE) == -1 && errno == ENOSPC);
	CHECK(57, munmap(map, 8 * PAGE) == 0 && close(fd) == 0);
	CHECK(58, unlink("/mnt/quota/file") == 0);
	CHECK(59, umount("/mnt/quota") == 0);
}

int main(void)
{
	memfd_semantics();
	tmpfs_semantics();
	quota_semantics();
	puts("LITEOS_MEMFILE_42");
	return 0;
}
