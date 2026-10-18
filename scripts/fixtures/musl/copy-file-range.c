#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

/* copy_file_range：显式/共享 offset 的四种组合、partial 与 EOF、错误码和同文件重叠。 */
static int fill_file(const char *path, size_t bytes)
{
	int fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0644);
	if (fd < 0) return -1;
	char block[512];
	for (size_t done = 0; done < bytes; done += sizeof block) {
		for (size_t i = 0; i < sizeof block; i++) block[i] = (char)((done + i) * 7 + 3);
		if (write(fd, block, sizeof block) != (ssize_t)sizeof block) return -1;
	}
	return fd;
}

static int content_matches(int fd, off_t at, off_t source, size_t bytes)
{
	char got[512];
	for (size_t done = 0; done < bytes; done += sizeof got) {
		if (pread(fd, got, sizeof got, at + (off_t)done) != (ssize_t)sizeof got) return -1;
		for (size_t i = 0; i < sizeof got; i++)
			if (got[i] != (char)(((size_t)source + done + i) * 7 + 3)) return -1;
	}
	return 0;
}

int verify_copy_file_range(void)
{
	const char *in_path = "/tmp/cfr-in", *out_path = "/tmp/cfr-out";
	int in = fill_file(in_path, 8192);
	int out = open(out_path, O_CREAT | O_RDWR | O_TRUNC, 0644);
	if (in < 0 || out < 0) return 1;

	/* 共享 offset：输入从当前位置读，输出从当前位置写，两个 OFD offset 都前进。 */
	if (lseek(in, 1024, SEEK_SET) != 1024) return 2;
	if (copy_file_range(in, NULL, out, NULL, 2048, 0) != 2048) return 3;
	if (lseek(in, 0, SEEK_CUR) != 3072 || lseek(out, 0, SEEK_CUR) != 2048) return 4;
	if (content_matches(out, 0, 1024, 2048) != 0) return 5;

	/* 显式输入 offset：输入 OFD offset 不变，指针被更新。 */
	loff_t in_off = 4096;
	if (lseek(in, 100, SEEK_SET) != 100) return 6;
	if (copy_file_range(in, &in_off, out, NULL, 1024, 0) != 1024) return 7;
	if (in_off != 5120 || lseek(in, 0, SEEK_CUR) != 100) return 8;
	if (lseek(out, 0, SEEK_CUR) != 3072 || content_matches(out, 2048, 4096, 1024) != 0) return 9;

	/* 显式输出 offset：输出 OFD offset 不变。 */
	loff_t out_off = 8192;
	if (copy_file_range(in, NULL, out, &out_off, 512, 0) != 512) return 10;
	if (out_off != 8704 || lseek(out, 0, SEEK_CUR) != 3072) return 11;
	if (content_matches(out, 8192, 100, 512) != 0) return 12;

	/* 两侧都显式；越过输入 EOF 时 partial，随后在 EOF 返回 0。 */
	in_off = 8000;
	out_off = 0;
	if (copy_file_range(in, &in_off, out, &out_off, 4096, 0) != 192) return 13;
	if (in_off != 8192 || out_off != 192) return 14;
	if (copy_file_range(in, &in_off, out, &out_off, 4096, 0) != 0) return 15;
	if (in_off != 8192 || out_off != 192) return 16;

	/* 同一文件：不重叠可以，重叠是 EINVAL。 */
	in_off = 0;
	out_off = 4096;
	if (copy_file_range(in, &in_off, in, &out_off, 1024, 0) != 1024) return 17;
	in_off = 0;
	out_off = 512;
	errno = 0;
	if (copy_file_range(in, &in_off, in, &out_off, 1024, 0) != -1 || errno != EINVAL) return 18;

	/* 错误码。 */
	errno = 0;
	if (copy_file_range(in, NULL, out, NULL, 16, 1) != -1 || errno != EINVAL) return 19;
	errno = 0;
	if (copy_file_range(-1, NULL, out, NULL, 16, 0) != -1 || errno != EBADF) return 20;
	loff_t negative = -1;
	errno = 0;
	if (copy_file_range(in, &negative, out, NULL, 16, 0) != -1 || errno != EINVAL) return 21;
	errno = 0;
	if (copy_file_range(in, (loff_t *)1, out, NULL, 16, 0) != -1 || errno != EFAULT) return 22;
	int read_only = open(out_path, O_RDONLY);
	errno = 0;
	if (copy_file_range(in, NULL, read_only, NULL, 16, 0) != -1 || errno != EBADF) return 23;
	int appender = open(out_path, O_WRONLY | O_APPEND);
	errno = 0;
	if (copy_file_range(in, NULL, appender, NULL, 16, 0) != -1 || errno != EBADF) return 24;
	int directory = open("/tmp", O_RDONLY);
	errno = 0;
	if (copy_file_range(directory, NULL, out, NULL, 16, 0) != -1 || errno != EISDIR) return 25;
	int pipe_fds[2];
	if (pipe(pipe_fds) != 0) return 26;
	errno = 0;
	if (copy_file_range(in, NULL, pipe_fds[1], NULL, 16, 0) != -1 || errno != EINVAL) return 27;
	loff_t big = INT64_MAX - 8;
	errno = 0;
	if (copy_file_range(in, &big, out, NULL, 4096, 0) != -1 || errno != EOVERFLOW) return 28;
	if (copy_file_range(in, NULL, out, NULL, 0, 0) != 0) return 29;

	close(in); close(out); close(read_only); close(appender); close(directory);
	close(pipe_fds[0]); close(pipe_fds[1]);
	unlink(in_path);
	unlink(out_path);
	return 0;
}
