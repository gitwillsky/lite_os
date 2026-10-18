#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#define PAGE 4096u

static unsigned char pattern(size_t offset)
{
	return (unsigned char)(offset * 131u + 7u);
}

static void fill(unsigned char *base, size_t bytes)
{
	for (size_t i = 0; i < bytes; i++) base[i] = pattern(i);
}

static int check(const unsigned char *base, size_t bytes)
{
	for (size_t i = 0; i < bytes; i++)
		if (base[i] != pattern(i)) return -1;
	return 0;
}

static int all_zero(const unsigned char *base, size_t bytes)
{
	for (size_t i = 0; i < bytes; i++)
		if (base[i] != 0) return -1;
	return 0;
}

static void *map(size_t pages)
{
	return mmap(NULL, pages * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
}

/* mremap：缩小、原地增长、搬迁（MAYMOVE/FIXED/DONTUNMAP）、共享映射别名与各类映射的数据保持。 */
int verify_mremap(void)
{
	/* 缩小：保留前缀内容，释放的尾部可被 MAP_FIXED_NOREPLACE 重新占用。 */
	unsigned char *a = map(4);
	if (a == MAP_FAILED) return 1;
	fill(a, 4 * PAGE);
	if (mremap(a, 4 * PAGE, 2 * PAGE, 0) != (void *)a) return 2;
	if (check(a, 2 * PAGE) != 0) return 3;
	void *tail = mmap(a + 2 * PAGE, 2 * PAGE, PROT_READ | PROT_WRITE,
	                  MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
	if (tail != (void *)(a + 2 * PAGE)) return 4;
	munmap(a, 4 * PAGE);

	/* 原地增长：后面是空洞时地址不变，旧内容保留，新页为零且可写。 */
	unsigned char *b = map(8);
	if (b == MAP_FAILED) return 5;
	munmap(b + 2 * PAGE, 6 * PAGE);
	fill(b, 2 * PAGE);
	if (mremap(b, 2 * PAGE, 8 * PAGE, 0) != (void *)b) return 6;
	if (check(b, 2 * PAGE) != 0 || all_zero(b + 2 * PAGE, 6 * PAGE) != 0) return 7;
	b[8 * PAGE - 1] = 1;
	munmap(b, 8 * PAGE);

	/* 被挡住：[page0][洞][page2]；不允许搬迁返回 ENOMEM，允许则搬迁且内容保持。 */
	unsigned char *c = map(3);
	if (c == MAP_FAILED) return 8;
	munmap(c + PAGE, PAGE);
	fill(c, PAGE);
	errno = 0;
	if (mremap(c, PAGE, 3 * PAGE, 0) != MAP_FAILED || errno != ENOMEM) return 9;
	if (check(c, PAGE) != 0) return 10;
	unsigned char *moved = mremap(c, PAGE, 3 * PAGE, MREMAP_MAYMOVE);
	if (moved == MAP_FAILED || moved == c) return 11;
	if (check(moved, PAGE) != 0 || all_zero(moved + PAGE, 2 * PAGE) != 0) return 12;
	/* 旧区间已释放，可被重新占用。 */
	void *reuse = mmap(c, PAGE, PROT_READ | PROT_WRITE,
	                   MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
	if (reuse != (void *)c) return 13;
	munmap(c, PAGE);
	munmap(c + 2 * PAGE, PAGE);
	munmap(moved, 3 * PAGE);

	/* 搬迁转移已物化页的 owner：稀疏映射里写过的页保持，没碰过的页仍为零。 */
	unsigned char *d = map(64);
	if (d == MAP_FAILED) return 14;
	d[0] = 11;
	d[17 * PAGE + 5] = 22;
	d[63 * PAGE + PAGE - 1] = 33;
	unsigned char *blocker = mmap(d + 64 * PAGE, PAGE, PROT_READ,
	                              MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0);
	if (blocker != (void *)(d + 64 * PAGE)) return 15;
	unsigned char *e = mremap(d, 64 * PAGE, 128 * PAGE, MREMAP_MAYMOVE);
	if (e == MAP_FAILED || e == d) return 16;
	if (e[0] != 11 || e[17 * PAGE + 5] != 22 || e[63 * PAGE + PAGE - 1] != 33) return 17;
	if (e[1] != 0 || e[40 * PAGE] != 0 || e[100 * PAGE] != 0) return 18;
	e[127 * PAGE] = 1;
	munmap(e, 128 * PAGE);
	munmap(blocker, PAGE);

	/* MREMAP_FIXED：搬到指定地址，替换其中既有映射；与源相交是 EINVAL。 */
	unsigned char *src = map(2);
	unsigned char *dst = map(4);
	if (src == MAP_FAILED || dst == MAP_FAILED) return 19;
	fill(src, 2 * PAGE);
	memset(dst, 0x5a, 4 * PAGE);
	errno = 0;
	if (mremap(src, 2 * PAGE, 2 * PAGE, MREMAP_MAYMOVE | MREMAP_FIXED, src + PAGE) != MAP_FAILED
	    || errno != EINVAL) return 20;
	errno = 0;
	if (mremap(src, 2 * PAGE, 2 * PAGE, MREMAP_FIXED, dst) != MAP_FAILED || errno != EINVAL) return 21;
	if (mremap(src, 2 * PAGE, 2 * PAGE, MREMAP_MAYMOVE | MREMAP_FIXED, dst) != (void *)dst) return 22;
	if (check(dst, 2 * PAGE) != 0 || dst[2 * PAGE] != 0x5a) return 23;
	/* 源被释放，可以重新占用。 */
	if (mmap(src, 2 * PAGE, PROT_READ | PROT_WRITE,
	         MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0) != (void *)src) return 24;
	munmap(src, 2 * PAGE);
	munmap(dst, 4 * PAGE);

	/* MREMAP_DONTUNMAP：内容搬走，旧范围保留为全零的新匿名映射。 */
	unsigned char *keep = map(3);
	if (keep == MAP_FAILED) return 25;
	fill(keep, 3 * PAGE);
	unsigned char *gone = mremap(keep, 3 * PAGE, 3 * PAGE, MREMAP_MAYMOVE | MREMAP_DONTUNMAP);
	if (gone == MAP_FAILED || gone == keep) return 26;
	if (check(gone, 3 * PAGE) != 0 || all_zero(keep, 3 * PAGE) != 0) return 27;
	keep[0] = 9;
	errno = 0;
	if (mremap(keep, 3 * PAGE, 3 * PAGE, MREMAP_DONTUNMAP) != MAP_FAILED || errno != EINVAL) return 28;
	errno = 0;
	if (mremap(keep, 3 * PAGE, 4 * PAGE, MREMAP_MAYMOVE | MREMAP_DONTUNMAP) != MAP_FAILED || errno != EINVAL) return 29;
	munmap(keep, 3 * PAGE);
	munmap(gone, 3 * PAGE);

	/* 参数错误与源范围错误。 */
	unsigned char *f = map(2);
	if (f == MAP_FAILED) return 30;
	errno = 0;
	if (mremap(f + 1, PAGE, PAGE, 0) != MAP_FAILED || errno != EINVAL) return 31;
	errno = 0;
	if (mremap(f, PAGE, 0, 0) != MAP_FAILED || errno != EINVAL) return 32;
	errno = 0;
	if (mremap(f, 4 * PAGE, 4 * PAGE, 0) != MAP_FAILED || errno != EFAULT) return 33;
	errno = 0;
	if (mremap(f, 0, PAGE, MREMAP_MAYMOVE) != MAP_FAILED || errno != EINVAL) return 34; /* 私有映射不能别名 */
	munmap(f, 2 * PAGE);

	/* 共享匿名：搬迁转移 backing 而不是复制，fork 出来的子进程仍与新地址共享同一批页。 */
	unsigned char *shared = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	if (shared == MAP_FAILED) return 35;
	shared[0] = 1;
	/* 紧挨着放一页，迫使增长走搬迁路径。 */
	if (mmap(shared + 2 * PAGE, PAGE, PROT_READ,
	         MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0) != (void *)(shared + 2 * PAGE)) return 59;
	int ready[2], done[2];
	if (pipe(ready) != 0 || pipe(done) != 0) return 36;
	pid_t child = fork();
	if (child == 0) {
		char token;
		read(ready[0], &token, 1);
		shared[0] = 42; /* 子进程通过旧地址写 */
		shared[PAGE] = 43;
		write(done[1], "x", 1);
		_exit(0);
	}
	if (child < 0) return 37;
	unsigned char *shared_moved = mremap(shared, 2 * PAGE, 4 * PAGE, MREMAP_MAYMOVE);
	if (shared_moved == MAP_FAILED || shared_moved == shared) return 38;
	if (shared_moved[0] != 1) return 39;
	write(ready[1], "x", 1);
	char token;
	read(done[0], &token, 1);
	if (shared_moved[0] != 42 || shared_moved[PAGE] != 43) return 40;
	waitpid(child, NULL, 0);
	/* 增长出来的页没有 backing：访问得到 SIGBUS，与 Linux 一致。 */
	child = fork();
	if (child == 0) {
		*(volatile unsigned char *)(shared_moved + 2 * PAGE) = 1;
		_exit(0);
	}
	int status;
	if (waitpid(child, &status, 0) != child || !WIFSIGNALED(status) || WTERMSIG(status) != SIGBUS) return 41;
	/* old_size == 0 为共享映射建立别名，两个地址看到同一批页。 */
	unsigned char *alias = mremap(shared_moved, 0, 2 * PAGE, MREMAP_MAYMOVE);
	if (alias == MAP_FAILED || alias == shared_moved) return 42;
	alias[0] = 77;
	if (shared_moved[0] != 77) return 43;
	errno = 0;
	if (mremap(shared_moved, 0, 2 * PAGE, 0) != MAP_FAILED || errno != ENOMEM) return 44;
	munmap(alias, 2 * PAGE);
	munmap(shared_moved, 4 * PAGE);
	munmap(shared + 2 * PAGE, PAGE);

	/* 共享文件映射：搬迁并增长后，写入对文件可见，增长出的页映射文件的后续内容。 */
	char path[] = "/tmp/mremap-XXXXXX";
	int fd = mkstemp(path);
	if (fd < 0) return 45;
	unlink(path);
	unsigned char block[PAGE];
	for (int i = 0; i < 4; i++) {
		memset(block, 'a' + i, PAGE);
		if (write(fd, block, PAGE) != PAGE) return 46;
	}
	unsigned char *file = mmap(NULL, 2 * PAGE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (file == MAP_FAILED) return 47;
	file[0] = 'Z';
	unsigned char *file_moved = mremap(file, 2 * PAGE, 4 * PAGE, MREMAP_MAYMOVE | MREMAP_FIXED, file + 16 * PAGE);
	if (file_moved != file + 16 * PAGE) return 48;
	if (file_moved[0] != 'Z' || file_moved[PAGE] != 'b' || file_moved[3 * PAGE] != 'd') return 49;
	file_moved[2 * PAGE] = 'Y';
	if (pread(fd, block, 1, 0) != 1 || block[0] != 'Z') return 50;
	if (pread(fd, block, 1, 2 * PAGE) != 1 || block[0] != 'Y') return 51;
	munmap(file_moved, 4 * PAGE);

	/* 私有文件映射：写过的页（COW）与未写的页在搬迁后各自保持内容。 */
	unsigned char *priv = mmap(NULL, 3 * PAGE, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
	if (priv == MAP_FAILED) return 52;
	priv[0] = 'Q';
	unsigned char *priv_moved =
		mremap(priv, 3 * PAGE, 3 * PAGE, MREMAP_MAYMOVE | MREMAP_FIXED, priv + 32 * PAGE);
	if (priv_moved != priv + 32 * PAGE) return 53;
	if (priv_moved[0] != 'Q' || priv_moved[PAGE] != 'b' || priv_moved[2 * PAGE] != 'Y') return 54;
	if (pread(fd, block, 1, 0) != 1 || block[0] != 'Z') return 55; /* 私有写不污染文件 */
	munmap(priv_moved, 3 * PAGE);
	close(fd);

	/* musl realloc 对大块优先用 mremap，失败时回退复制；两条路径都必须保持内容。 */
	size_t size = 1u << 20;
	unsigned char *big = malloc(size);
	if (!big) return 56;
	fill(big, size);
	big = realloc(big, 4 * size);
	if (!big || check(big, size) != 0) return 57;
	big = realloc(big, size / 2);
	if (!big || check(big, size / 2) != 0) return 58;
	free(big);
	return 0;
}
