#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static int proc_value(const char *path, const char *name, unsigned long long *value)
{
	char text[8192];
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0) return -1;
	ssize_t count = read(fd, text, sizeof text - 1);
	close(fd);
	if (count <= 0) return -1;
	text[count] = 0;
	for (char *line = text; line && *line; ) {
		if (strncmp(line, name, strlen(name)) == 0
		    && sscanf(line + strlen(name), "%llu", value) == 1) return 0;
		line = strchr(line, '\n');
		if (line) line++;
	}
	return -1;
}

/* 真实 cache + 不可回收匿名页施压，验证后台工作在 allocator stall 前释放页且不丢数据。 */
int verify_memory_pressure(void)
{
	const size_t page_size = 4096, cache_pages = 2048;
	const char *path = "/memory-pressure-cache";
	unsigned long long total_kb, free_kb, scanned_before, stolen_before, stalls_before;
	int result = 1, fd = -1;
	volatile unsigned char *anonymous = MAP_FAILED;
	size_t touched = 0, length = 0;
	unsigned char data[4096];
	if ((mkdir("/proc", 0755) != 0 && errno != EEXIST)
	    || mount("proc", "/proc", "proc", 0, 0) != 0) goto done;
	if (proc_value("/proc/meminfo", "MemTotal:", &total_kb) != 0
	    || proc_value("/proc/meminfo", "MemFree:", &free_kb) != 0) goto done;
	size_t capacity = total_kb / 4;
	size_t low = capacity / 64 > 256 ? capacity / 64 : 256;
	length = capacity * page_size;
	fd = open(path, O_CREAT | O_TRUNC | O_RDWR | O_CLOEXEC, 0600);
	if (fd < 0) goto done;
	for (size_t page = 0; page < cache_pages; page++) {
		memset(data, (unsigned char)(page % 251 + 1), sizeof data);
		if (pwrite(fd, data, sizeof data, page * page_size) != sizeof data) goto done;
	}
	if (fsync(fd) != 0) goto done;
	anonymous = mmap(0, length, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (anonymous == MAP_FAILED) goto done;
	if (proc_value("/proc/vmstat", "pgscan_kswapd ", &scanned_before) != 0
	    || proc_value("/proc/vmstat", "pgsteal_kswapd ", &stolen_before) != 0
	    || proc_value("/proc/vmstat", "allocstall ", &stalls_before) != 0) goto done;
	result = 2;
	for (;;) {
		unsigned long long scanned;
		if (proc_value("/proc/vmstat", "pgscan_kswapd ", &scanned) != 0
		    || proc_value("/proc/meminfo", "MemFree:", &free_kb) != 0) goto done;
		if (scanned > scanned_before || free_kb / 4 < low) break;
		size_t pages = free_kb / 4 - (low - 16);
		if (pages > 64) pages = 64;
		if (touched + pages > capacity) goto done;
		for (size_t page = 0; page < pages; page++, touched++)
			anonymous[touched * page_size] = (unsigned char)(touched % 251 + 1);
	}
	result = 3;
	for (int attempt = 0; attempt < 100; attempt++) {
		unsigned long long scanned, stolen, stalls;
		if (proc_value("/proc/vmstat", "pgscan_kswapd ", &scanned) != 0
		    || proc_value("/proc/vmstat", "pgsteal_kswapd ", &stolen) != 0
		    || proc_value("/proc/vmstat", "allocstall ", &stalls) != 0) goto done;
		if (stalls != stalls_before) {
			dprintf(2, "memory pressure allocstall before=%llu after=%llu scanned=%llu/%llu stolen=%llu/%llu free=%llu touched=%zu\n", stalls_before, stalls, scanned_before, scanned, stolen_before, stolen, free_kb, touched);
			goto done;
		}
		if (scanned > scanned_before && stolen > stolen_before) {
			result = 4;
			for (size_t page = 0; page < touched; page++)
				if (anonymous[page * page_size] != (unsigned char)(page % 251 + 1)) goto done;
			// 先释放压力源，再用完整文件读回验证回收/写回的内容。
			munmap((void *)anonymous, length);
			anonymous = MAP_FAILED;
			for (size_t page = 0; page < cache_pages; page++) {
				if (pread(fd, data, sizeof data, page * page_size) != sizeof data) goto done;
				for (size_t byte = 0; byte < sizeof data; byte++)
					if (data[byte] != (unsigned char)(page % 251 + 1)) goto done;
			}
			result = 0;
			goto done;
		}
		if (attempt == 99) {
			if (proc_value("/proc/meminfo", "MemFree:", &free_kb) != 0) goto done;
			dprintf(2, "memory pressure timeout scanned=%llu/%llu stolen=%llu/%llu free=%llu touched=%zu total=%llu low=%zu\n", scanned_before, scanned, stolen_before, stolen, free_kb, touched, total_kb, low);
		}
		const struct timespec delay = { .tv_nsec = 20 * 1000 * 1000 };
		if (nanosleep(&delay, 0) != 0) goto done;
	}
done:
	if (anonymous != MAP_FAILED) munmap((void *)anonymous, length);
	if (fd >= 0) close(fd);
	unlink(path);
	if (result) dprintf(2, "memory pressure step %d failed\n", result);
	return result;
}
