#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <signal.h>
#include <sched.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/swap.h>
#include <sys/sysinfo.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int proc_value(const char *path, const char *name, unsigned long long *value)
{
	char bytes[8192];
	int fd = open(path, O_RDONLY | O_CLOEXEC);
	if (fd < 0) return -1;
	ssize_t count = read(fd, bytes, sizeof bytes - 1);
	close(fd);
	if (count <= 0) return -1;
	bytes[count] = 0;
	for (char *line = bytes; line && *line; ) {
		if (strncmp(line, name, strlen(name)) == 0
		    && sscanf(line + strlen(name), "%llu", value) == 1) return 0;
		line = strchr(line, '\n');
		if (line) line++;
	}
	return -1;
}

static int memory_value(const char *name, unsigned long long *value)
{
	return proc_value("/proc/meminfo", name, value);
}

static int child_memory_value(pid_t pid, const char *name, unsigned long long *value)
{
	char path[64];
	snprintf(path, sizeof path, "/proc/%ld/status", (long)pid);
	return proc_value(path, name, value);
}

/* One pressure consumer exercises the same area/slot/reclaim path for partition and file storage. */
static int swap_pressure(unsigned long long swap_kb, pid_t target, unsigned long long target_rss)
{
	volatile unsigned char *bytes = MAP_FAILED;
	size_t length = 0, touched = 0;
	unsigned long long total_kb, free_kb, swap_free_kb;
	struct timespec pause = { .tv_nsec = 20000000 };
	if (memory_value("MemTotal:", &total_kb) != 0) goto failed;
	size_t capacity = total_kb / 4;
	size_t low = capacity / 64 > 256 ? capacity / 64 : 256;
	length = capacity * 4096;
	bytes = mmap(0, length, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (bytes == MAP_FAILED) goto failed;
	int used = 0;
	while (touched < capacity && !used) {
		size_t until = touched + 64 < capacity ? touched + 64 : capacity;
		for (; touched < until; touched++) {
			bytes[touched * 4096] = (unsigned char)(touched % 251 + 1);
			bytes[touched * 4096 + 4095] = (unsigned char)(touched % 241 + 3);
		}
		if (memory_value("MemFree:", &free_kb) != 0) goto failed;
		if (free_kb / 4 >= low) continue;
		for (int attempt = 0; attempt < 100; attempt++) {
			if (nanosleep(&pause, 0) != 0
			    || memory_value("SwapFree:", &swap_free_kb) != 0
			    || memory_value("MemFree:", &free_kb) != 0) goto failed;
			unsigned long long rss = 0;
			if (target && child_memory_value(target, "VmRSS:", &rss) != 0) goto failed;
			/* A sleeping child owns only the warm 8MiB seed and the small probe image.
			 * Require 2MiB of its own RSS to disappear; unrelated swap activity cannot pass. */
			if (swap_free_kb < swap_kb && (!target || rss + 2048 <= target_rss)) { used = 1; break; }
			if (free_kb / 4 >= low) break;
		}
	}
	if (!used) goto failed;
	for (size_t page = 0; page < touched; page++) {
		if (bytes[page * 4096] != (unsigned char)(page % 251 + 1)
		    || bytes[page * 4096 + 4095] != (unsigned char)(page % 241 + 3)) goto failed;
	}
	/* 腾出容量再 drain；这不是借 swapoff 触发 OOM kill 的测试。 */
	if (munmap((void *)bytes, length) != 0) goto failed;
	bytes = MAP_FAILED;
	return 0;
failed:
	if (bytes != MAP_FAILED) munmap((void *)bytes, length);
	return -1;
}

static volatile sig_atomic_t swapoff_signals;
static void swapoff_signal(int signal) { if (signal == SIGUSR1) swapoff_signals++; }

/* Real signal generation races a drain with hundreds of outstanding page reads.
 * The child seed remains alive; EINTR must retain the area lease and permit retry. */
static int interrupt_swapoff(const cpu_set_t *available)
{
	struct sigaction action = { .sa_handler = swapoff_signal }, previous;
	pid_t parent = getpid(), sender = -1;
	int status, result = -1, saved = 0;
	sigemptyset(&action.sa_mask);
	swapoff_signals = 0;
	if (sigaction(SIGUSR1, &action, &previous) != 0) return -1;
	sender = fork();
	if (sender == 0) {
		cpu_set_t selected;
		CPU_ZERO(&selected);
		for (int cpu = CPU_SETSIZE - 1; cpu >= 0; cpu--) if (CPU_ISSET(cpu, available)) { CPU_SET(cpu, &selected); break; }
		if (sched_setaffinity(0, sizeof selected, &selected) != 0) _exit(2);
		struct timespec pause = { .tv_nsec = 200000 };
		for (;;) { if (kill(parent, SIGUSR1) != 0 || nanosleep(&pause, 0) != 0) _exit(3); }
	}
	if (sender > 0) { errno = 0; result = swapoff("/dev/vdb"); saved = errno; }
	if (sender > 0) {
		kill(sender, SIGKILL);
		while (waitpid(sender, &status, 0) == -1 && errno == EINTR) {}
	}
	int received = swapoff_signals;
	if (sigaction(SIGUSR1, &previous, 0) != 0) return -1;
	if (sender < 0 || result != -1 || saved != EINTR || !received) {
		printf("swapoff interruption result=%d errno=%d signals=%d\n", result, saved, received);
		return -1;
	}
	errno = 0;
	if (swapon("/dev/vdb", 0) != -1 || errno != EBUSY || swapoff("/dev/vdb") != 0) return -1;
	if (swapon("/dev/vdb", SWAP_FLAG_PREFER | 7) != 0) return -1;
	printf("LiteOS musl swapoff EINTR retry ok\n");
	return 0;
}

/* Exercise shared COW, shared anonymous and both MemoryFile consumers under real pressure. */
static int shared_swap_pressure(unsigned long long swap_kb, int kind)
{
	const size_t length = 8 * 1024 * 1024, pages = length / 4096;
	volatile unsigned char *seed = MAP_FAILED;
	int fd = -1, mounted = 0, ready[2] = {-1, -1}, resume[2] = {-1, -1};
	pid_t child = -1;
	int status, stage = 1, affinity_saved = 0;
	cpu_set_t affinity, selected;
	unsigned long long before, after, cached;
	if (sched_getaffinity(0, sizeof affinity, &affinity) != 0) goto failed;
	affinity_saved = 1;
	CPU_ZERO(&selected);
	for (int cpu = 0; cpu < CPU_SETSIZE; cpu++) if (CPU_ISSET(cpu, &affinity)) { CPU_SET(cpu, &selected); break; }
	if (sched_setaffinity(0, sizeof selected, &selected) != 0) goto failed;
	if (kind == 2) fd = memfd_create("swap-shmem", MFD_CLOEXEC);
	if (kind == 3) {
		if (mkdir("/swap-shmem", 0700) != 0 || mount("tmpfs", "/swap-shmem", "tmpfs", 0, "size=8m,nr_inodes=8") != 0) goto failed;
		mounted = 1;
		fd = open("/swap-shmem/content", O_RDWR | O_CREAT | O_EXCL | O_CLOEXEC, 0600);
	}
	if ((kind == 2 || kind == 3) && (fd < 0 || ftruncate(fd, length) != 0)) goto failed;
	int flags = (kind == 0 || kind == 4) ? MAP_PRIVATE | MAP_ANONYMOUS : kind == 1 ? MAP_SHARED | MAP_ANONYMOUS : MAP_SHARED;
	seed = mmap(0, length, PROT_READ | PROT_WRITE, flags, fd, 0);
	if (seed == MAP_FAILED) goto failed;
	for (size_t page = 0; page < pages; page++) {
		seed[page * 4096] = (unsigned char)(page % 251 + 1);
		seed[page * 4096 + 4095] = (unsigned char)(page % 241 + 3);
	}
	stage = 2;
	if (pipe(ready) != 0 || pipe(resume) != 0) goto failed;
	child = fork();
	if (child == 0) {
		char token;
		close(ready[0]); close(resume[1]);
		CPU_ZERO(&selected);
		for (int cpu = CPU_SETSIZE - 1; cpu >= 0; cpu--) if (CPU_ISSET(cpu, &affinity)) { CPU_SET(cpu, &selected); break; }
		if (sched_setaffinity(0, sizeof selected, &selected) != 0) _exit(1);
		for (size_t page = 0; page < pages; page++) {
			if (seed[page * 4096] != (unsigned char)(page % 251 + 1)) _exit(2);
		}
		if (write(ready[1], "r", 1) != 1 || read(resume[0], &token, 1) != 1) _exit(3);
		for (size_t page = 0; page < pages; page++) {
			unsigned char first = page == 0 && kind != 0 && kind != 4 ? 0xee : (unsigned char)(page % 251 + 1);
			unsigned char last = page == pages - 1 && (kind == 2 || kind == 3) ? 0 : (unsigned char)(page % 241 + 3);
			if (seed[page * 4096] != first || seed[page * 4096 + 4095] != last) _exit(4);
		}
		_exit(0);
	}
	char token;
	if (child < 0 || read(ready[0], &token, 1) != 1 || child_memory_value(child, "VmRSS:", &before) != 0 || before < length / 1024) goto failed;
	stage = 3;
	if (swap_pressure(swap_kb, child, before) != 0) goto failed;
	stage = 30;
	if (kind == 4 && interrupt_swapoff(&affinity) != 0) goto failed;
	stage = 31;
	/* mprotect prepares current physical identities; a stale alias stays lazy. */
	if (mprotect((void *)seed, length, PROT_NONE) != 0 || mprotect((void *)seed, length, PROT_READ | PROT_WRITE) != 0) goto failed;
	stage = 32;
	for (size_t page = 0; page < pages; page++) {
		if (seed[page * 4096] != (unsigned char)(page % 251 + 1) || seed[page * 4096 + 4095] != (unsigned char)(page % 241 + 3)) goto failed;
	}
	stage = 33;
	if (child_memory_value(child, "VmRSS:", &after) != 0 || memory_value("SwapCached:", &cached) != 0) goto failed;
	/* RSS includes code/stack as well. Only the 8MiB seed was restored by the parent;
	 * at least seed_kB - child_total_RSS seed pages remain stale in the child and
	 * retain a cache slot. Non-seed swapped pages need not be resident/cache pages. */
	unsigned long long missing_seed = after < length / 1024 ? length / 1024 - after : 0;
	if (kind != 4 && (after + 2048 > before || !missing_seed || cached < missing_seed)) {
		printf("shared swap counters before=%llu after=%llu cached=%llu kB\n", before, after, cached);
		goto failed;
	}
	stage = 4;
	seed[0] = 0xee;
	if (kind == 2 || kind == 3) {
		struct stat st;
		unsigned char byte;
		if (fstat(fd, &st) != 0 || st.st_blocks != (blkcnt_t)(length / 512) || pread(fd, &byte, 1, 0) != 1 || byte != 0xee) goto failed;
		if (ftruncate(fd, length - 7) != 0 || ftruncate(fd, length) != 0) goto failed;
	}
	if (write(resume[1], "g", 1) != 1 || waitpid(child, &status, 0) != child) goto failed;
	child = -1;
	if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) goto failed;
	if (munmap((void *)seed, length) != 0) goto failed;
	seed = MAP_FAILED;
	if (fd >= 0 && close(fd) != 0) goto failed;
	fd = -1;
	if (mounted && (unlink("/swap-shmem/content") != 0 || umount("/swap-shmem") != 0 || rmdir("/swap-shmem") != 0)) goto failed;
	mounted = 0;
	for (int i = 0; i < 2; i++) { close(ready[i]); close(resume[i]); }
	if (sched_setaffinity(0, sizeof affinity, &affinity) != 0) goto failed;
	affinity_saved = 0;
	printf("LiteOS musl shared swap kind=%d ok\n", kind);
	return 0;
failed:
	printf("LiteOS musl shared swap failed kind=%d stage=%d errno=%d\n", kind, stage, errno);
	if (child > 0) { kill(child, SIGKILL); waitpid(child, &status, 0); }
	if (seed != MAP_FAILED) munmap((void *)seed, length);
	if (fd >= 0) close(fd);
	for (int i = 0; i < 2; i++) { if (ready[i] >= 0) close(ready[i]); if (resume[i] >= 0) close(resume[i]); }
	if (mounted) { unlink("/swap-shmem/content"); umount("/swap-shmem"); rmdir("/swap-shmem"); }
	if (affinity_saved) sched_setaffinity(0, sizeof affinity, &affinity);
	return -1;
}

static int store_gets_sigbus(volatile unsigned char *address)
{
	pid_t child = fork();
	if (child == 0) { *address = 0x57; _exit(1); }
	int status;
	return child > 0 && waitpid(child, &status, 0) == child && WIFSIGNALED(status) && WTERMSIG(status) == SIGBUS;
}

static int verify_swap_file(void)
{
	const char *path = "/swap-file", *alias = "/swap-alias", *other = "/swap-other";
	int stage = 1, active = 0, fd = -1, ro = -1, pinned = 0;
	int ready[2] = {-1, -1}, resume[2] = {-1, -1};
	pid_t warm_child = -1;
	cpu_set_t affinity, cpu;
	unsigned char header[4096] = {0};
	volatile unsigned char *shared = MAP_FAILED, *late = MAP_FAILED, *private = MAP_FAILED, *readonly = MAP_FAILED, *fixed = MAP_FAILED;
	uint32_t version = 1, last = 8191;
	memcpy(header + 1024, &version, 4); memcpy(header + 1028, &last, 4);
	memcpy(header + 4086, "SWAPSPACE2", 10);
	fd = open(path, O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600);
	if (fd < 0 || fallocate(fd, 0, 0, 32 * 1024 * 1024) != 0) goto failed;
	shared = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
	if (shared == MAP_FAILED) goto failed;
	shared[64] = 0x46; /* Publish the resident/writer before the failing activation. */
	/* A bad header must unfreeze the inode and permit write-fault/fork recovery. */
	errno = 0;
	if (swapon(path, 0) != -1 || errno != EINVAL) goto failed;
	pid_t child = fork();
	if (child == 0) { shared[64] = 0x48; _exit(0); }
	int status;
	if (child <= 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0 || shared[64] != 0x48) goto failed;
	stage = 2;
	if (pwrite(fd, header, 4096, 0) != 4096) goto failed;
	shared[64] = 0x69;
	late = mmap(0, 8192, PROT_NONE, MAP_SHARED, fd, 0);
	if (late == MAP_FAILED || link(path, alias) != 0) goto failed;
	/* Warm the old child leaf before activation. With SMP, child uses CPU1 and activation CPU0. */
	if (sched_getaffinity(0, sizeof affinity, &affinity) != 0 || pipe(ready) != 0 || pipe(resume) != 0) goto failed;
	CPU_ZERO(&cpu); CPU_SET(0, &cpu);
	if (sched_setaffinity(0, sizeof cpu, &cpu) != 0) goto failed;
	pinned = 1;
	warm_child = fork();
	if (warm_child == 0) {
		close(ready[0]); close(resume[1]);
		CPU_ZERO(&cpu); CPU_SET(CPU_ISSET(1, &affinity) ? 1 : 0, &cpu);
		if (sched_setaffinity(0, sizeof cpu, &cpu) != 0) _exit(3);
		shared[65] = 0x34;
		char token = 1;
		if (write(ready[1], &token, 1) != 1 || read(resume[0], &token, 1) != 1) _exit(4);
		shared[65] = 0x35; /* The already-existing W leaf must have been revoked remotely. */
		_exit(5);
	}
	if (warm_child < 0) goto failed;
	close(ready[1]); ready[1] = -1;
	close(resume[0]); resume[0] = -1;
	char token;
	if (read(ready[0], &token, 1) != 1) goto failed;
	close(ready[0]); ready[0] = -1;
	if (swapon(path, SWAP_FLAG_PREFER | 9) != 0) goto failed;
	active = 1;
	if (write(resume[1], &token, 1) != 1) goto failed;
	close(resume[1]); resume[1] = -1;
	if (waitpid(warm_child, &status, 0) != warm_child || !WIFSIGNALED(status) || WTERMSIG(status) != SIGBUS) goto failed;
	warm_child = -1;
	if (sched_setaffinity(0, sizeof affinity, &affinity) != 0) goto failed;
	pinned = 0;
	stage = 3;
	struct sysinfo info;
	if (sysinfo(&info) != 0 || info.totalswap != 32 * 1024 * 1024 - 4096) goto failed;
	int tablefd = open("/proc/swaps", O_RDONLY | O_CLOEXEC);
	char table[512]; ssize_t count = tablefd >= 0 ? read(tablefd, table, sizeof table - 1) : -1;
	if (tablefd >= 0) close(tablefd);
	if (count <= 0) goto failed;
	table[count] = 0;
	if (!strstr(table, "/swap-file\tfile\t32764\t0\t9")) goto failed;
	errno = 0; if (swapon(alias, 0) != -1 || errno != EBUSY) goto failed;
	errno = 0; if (pwrite(fd, header, 1, 0) != -1 || errno != ETXTBSY) goto failed;
	errno = 0; if (ftruncate(fd, 0) != -1 || errno != ETXTBSY) goto failed;
	errno = 0; if (fallocate(fd, 0, 0, 4096) != -1 || errno != ETXTBSY) goto failed;
	errno = 0; if (unlink(alias) != -1 || errno != EPERM) goto failed;
	errno = 0; if (rename(path, other) != -1 || errno != EPERM) goto failed;
	int otherfd = open(other, O_CREAT | O_EXCL | O_WRONLY, 0600);
	if (otherfd < 0 || close(otherfd) != 0) goto failed;
	errno = 0; if (rename(other, path) != -1 || errno != EPERM) goto failed;
	stage = 4;
	errno = 0;
	if (mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0) != MAP_FAILED || errno != ETXTBSY) goto failed;
	fixed = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (fixed == MAP_FAILED) goto failed;
	fixed[0] = 0x83;
	errno = 0;
	if (mmap((void *)fixed, 8192, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_FIXED, fd, 0) != MAP_FAILED || errno != ETXTBSY || fixed[0] != 0x83) goto failed;
	if (!store_gets_sigbus(shared + 64) || mprotect((void *)late, 8192, PROT_READ | PROT_WRITE) != 0 || !store_gets_sigbus(late + 64)) goto failed;
	private = mmap(0, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
	if (private == MAP_FAILED) goto failed;
	private[64] = 0x7b;
	if (shared[64] != 0x69) goto failed;
	ro = open(path, O_RDONLY | O_CLOEXEC);
	if (ro < 0) goto failed;
	readonly = mmap(0, 8192, PROT_READ, MAP_SHARED, ro, 0);
	if (readonly == MAP_FAILED) goto failed;
	errno = 0; if (mprotect((void *)readonly, 8192, PROT_READ | PROT_WRITE) != -1 || errno != EACCES) goto failed;
	if (close(fd) != 0 || close(ro) != 0) goto failed;
	fd = ro = -1;
	stage = 5;
	/* OFDs are now closed: only the swap source owns mount write admission. */
	errno = 0;
	if (mount(0, "/", 0, MS_REMOUNT | MS_RDONLY, 0) != -1 || errno != EBUSY) goto failed;
	if (swap_pressure(32764, 0, 0) != 0) goto failed;
	stage = 6;
	if (swapoff(alias) != 0) goto failed;
	active = 0;
	child = fork();
	if (child == 0) { shared[64] = 0x5c; _exit(0); }
	if (child <= 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0 || shared[64] != 0x5c) goto failed;
	munmap((void *)shared, 8192); shared = MAP_FAILED;
	munmap((void *)late, 8192); late = MAP_FAILED;
	munmap((void *)private, 8192); private = MAP_FAILED;
	munmap((void *)readonly, 8192); readonly = MAP_FAILED;
	munmap((void *)fixed, 8192); fixed = MAP_FAILED;
	stage = 7;
	if (mount(0, "/", 0, MS_REMOUNT | MS_RDONLY, 0) != 0) goto failed;
	errno = 0; if (swapon(path, 0) != -1 || errno != EROFS) goto failed;
	errno = 0; if (swapoff(path) != -1 || errno != EROFS) goto failed;
	if (mount(0, "/", 0, MS_REMOUNT, 0) != 0) goto failed;
	if (unlink(alias) != 0 || unlink(path) != 0 || unlink(other) != 0) goto failed;
	puts("LiteOS musl swapfile ok");
	return 0;
failed:
	printf("LiteOS musl swap failed swapfile stage=%d errno=%d\n", stage, errno);
	if (fd >= 0) close(fd);
	if (ro >= 0) close(ro);
	for (int index = 0; index < 2; index++) {
		if (ready[index] >= 0) close(ready[index]);
		if (resume[index] >= 0) close(resume[index]);
	}
	if (warm_child > 0) waitpid(warm_child, &status, 0);
	if (pinned) sched_setaffinity(0, sizeof affinity, &affinity);
	if (shared != MAP_FAILED) munmap((void *)shared, 8192);
	if (late != MAP_FAILED) munmap((void *)late, 8192);
	if (private != MAP_FAILED) munmap((void *)private, 8192);
	if (readonly != MAP_FAILED) munmap((void *)readonly, 8192);
	if (fixed != MAP_FAILED) munmap((void *)fixed, 8192);
	if (active) swapoff(path);
	return -1;
}

/* 标准 musl syscall + 实际 VirtIO I/O：只在专用第二块 SWAPSPACE2 盘冷启动中运行。 */
int verify_swap(void)
{
	int stage = 1, active = 0;
	if ((mkdir("/dev", 0755) != 0 && errno != EEXIST)
	    || (mkdir("/proc", 0755) != 0 && errno != EEXIST)
	    || mount("proc", "/proc", "proc", 0, 0) != 0) goto failed;
	stage = 2;
	errno = 0;
	if (swapon("/dev/vda", 0) != -1 || errno != EBUSY) goto failed;
	if (swapon("/dev/vdb", SWAP_FLAG_PREFER | 7) != 0) goto failed;
	active = 1;
	stage = 3;
	errno = 0;
	if (swapon("/dev/vdb", 0) != -1 || errno != EBUSY) goto failed;
	errno = 0;
	if (open("/dev/vdb", O_WRONLY | O_CLOEXEC) != -1 || errno != EBUSY) goto failed;
	errno = 0;
	if (mount("/dev/vdb", "/dev", "ext4", 0, 0) != -1 || errno != EBUSY) goto failed;
	struct sysinfo info;
	unsigned long long swap_kb;
	if (sysinfo(&info) != 0 || info.totalswap != 64 * 1024 * 1024 - 4096
	    || info.freeswap != info.totalswap
	    || memory_value("SwapTotal:", &swap_kb) != 0
	    || swap_kb * 1024 != info.totalswap) goto failed;
	int fd = open("/proc/swaps", O_RDONLY | O_CLOEXEC);
	char table[512];
	ssize_t count = fd >= 0 ? read(fd, table, sizeof table - 1) : -1;
	if (fd >= 0) close(fd);
	if (count <= 0) goto failed;
	table[count] = 0;
	if (!strstr(table, "/dev/vdb\tpartition\t65532\t0\t7")) goto failed;
	stage = 4;
	pid_t child = fork();
	if (child == 0) {
		if (setuid(1000) != 0) _exit(1);
		errno = 0;
		if (swapon(0, 0) != -1 || errno != EPERM) _exit(2);
		errno = 0;
		_exit(swapoff(0) == -1 && errno == EPERM ? 0 : 3);
	}
	int status;
	if (child <= 0 || waitpid(child, &status, 0) != child
	    || !WIFEXITED(status) || WEXITSTATUS(status) != 0) goto failed;
	stage = 5;
	if (swap_pressure(swap_kb, 0, 0) != 0) goto failed;
	stage = 6;
	for (int kind = 0; kind < 5; kind++) if (shared_swap_pressure(swap_kb, kind) != 0) goto failed;
	stage = 7;
	if (swapoff("/dev/vdb") != 0) goto failed;
	active = 0;
	if (sysinfo(&info) != 0 || info.totalswap || info.freeswap) goto failed;
	fd = open("/dev/vdb", O_WRONLY | O_CLOEXEC);
	if (fd < 0 || close(fd) != 0) goto failed;
	/* 返回前已交还物理 claim，header 仍完整，重新启用同一设备应立即成功。 */
	if (swapon("/dev/vdb", 0) != 0) goto failed;
	active = 1;
	if (swapoff("/dev/vdb") != 0) goto failed;
	active = 0;
	stage = 8;
	if (verify_swap_file() != 0) goto failed;
	puts("LiteOS musl swap ok");
	return 0;
failed:
	printf("LiteOS musl swap failed stage=%d errno=%d\n", stage, errno);
	if (active) swapoff("/dev/vdb");
	return 40 + stage;
}
