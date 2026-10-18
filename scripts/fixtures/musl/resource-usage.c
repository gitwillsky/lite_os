#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <string.h>
#include <sys/mman.h>
#include <stdint.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <time.h>
#include <sys/syscall.h>
#include <unistd.h>

#define PAGE 4096u

static uint64_t cpu_us(void)
{
	struct timespec now;
	clock_gettime(CLOCK_THREAD_CPUTIME_ID, &now);
	return (uint64_t)now.tv_sec * 1000000u + (uint64_t)now.tv_nsec / 1000u;
}

static void burn(uint64_t microseconds)
{
	uint64_t end = cpu_us() + microseconds;
	while (cpu_us() < end) {}
}

static uint64_t used_us(const struct rusage *usage)
{
	return (uint64_t)usage->ru_utime.tv_sec * 1000000u + (uint64_t)usage->ru_utime.tv_usec
	       + (uint64_t)usage->ru_stime.tv_sec * 1000000u + (uint64_t)usage->ru_stime.tv_usec;
}

static volatile sig_atomic_t chld_seen;
static volatile long chld_utime, chld_stime;

static void on_sigchld(int signal, siginfo_t *info, void *context)
{
	(void)signal;
	(void)context;
	chld_utime = (long)info->si_utime;
	chld_stime = (long)info->si_stime;
	chld_seen = 1;
}

/* getrusage 报告 SELF/THREAD 的 CPU 时间；CHILDREN 只在 wait 回收 child 之后增长。 */
int verify_resource_usage(void)
{
	struct rusage usage;
	burn(30000);
	if (getrusage(RUSAGE_SELF, &usage) != 0 || used_us(&usage) < 25000) return 1;
	if (getrusage(RUSAGE_THREAD, &usage) != 0 || used_us(&usage) < 25000) return 2;
	/* 同一进程里先前的用例可能已回收过 child，所以只比较差值。 */
	if (getrusage(RUSAGE_CHILDREN, &usage) != 0) return 3;
	uint64_t children_before = used_us(&usage);

	pid_t child = fork();
	if (child == 0) {
		burn(40000);
		_exit(0);
	}
	if (child < 0) return 4;
	/* child 已退出但尚未回收时不能计入。 */
	siginfo_t info;
	if (waitid(P_PID, (id_t)child, &info, WEXITED | WNOWAIT) != 0) return 5;
	if (getrusage(RUSAGE_CHILDREN, &usage) != 0 || used_us(&usage) != children_before) return 6;
	int status;
	if (waitpid(child, &status, 0) != child || !WIFEXITED(status)) return 7;
	if (getrusage(RUSAGE_CHILDREN, &usage) != 0 || used_us(&usage) < children_before + 35000) return 8;

	errno = 0;
	if (getrusage(2, &usage) == 0 || errno != EINVAL) return 9;
	errno = 0;
	if (getrusage(RUSAGE_SELF, (struct rusage *)1) == 0 || errno != EFAULT) return 10;

	/* system 时间：大量 write(/dev/null) 把时间花在内核里，tick 采样必须把一部分记成 system。 */
	int null_fd = open("/dev/null", O_WRONLY);
	if (null_fd < 0) return 11;
	char block[4096];
	memset(block, 'x', sizeof block);
	uint64_t end = cpu_us() + 150000;
	while (cpu_us() < end) write(null_fd, block, sizeof block);
	close(null_fd);
	if (getrusage(RUSAGE_SELF, &usage) != 0 || usage.ru_stime.tv_sec * 1000000 + usage.ru_stime.tv_usec == 0) return 12;

	/* 缺页：首次访问 64 个匿名页至少增加 64 次 minor fault。 */
	struct rusage before, after;
	getrusage(RUSAGE_SELF, &before);
	size_t bytes = 64 * PAGE;
	unsigned char *pages = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (pages == MAP_FAILED) return 13;
	for (size_t i = 0; i < bytes; i += PAGE) pages[i] = 1;
	getrusage(RUSAGE_SELF, &after);
	if (after.ru_minflt < before.ru_minflt + 64) return 14;
	munmap(pages, bytes);

	/* maxrss：驻留 8 MiB 后读取，释放后峰值仍保留。 */
	size_t big = 8u << 20;
	unsigned char *resident = mmap(NULL, big, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (resident == MAP_FAILED) return 15;
	memset(resident, 1, big);
	getrusage(RUSAGE_SELF, &usage);
	if (usage.ru_maxrss < 8192) return 16;
	munmap(resident, big);
	getrusage(RUSAGE_SELF, &after);
	if (after.ru_maxrss < usage.ru_maxrss) return 17;

	/* 自愿切换：每次 nanosleep 都是一次阻塞。 */
	struct rusage thread_before;
	getrusage(RUSAGE_THREAD, &thread_before);
	getrusage(RUSAGE_SELF, &before);
	struct timespec nap = {0, 5 * 1000 * 1000};
	for (int i = 0; i < 4; i++) nanosleep(&nap, NULL);
	getrusage(RUSAGE_SELF, &after);
	if (after.ru_nvcsw < before.ru_nvcsw + 4) return 18;
	/* RUSAGE_THREAD 只统计 calling Thread，同样至少推进这几次阻塞。 */
	struct rusage thread;
	if (getrusage(RUSAGE_THREAD, &thread) != 0 || thread.ru_nvcsw < thread_before.ru_nvcsw + 4) return 19;

	/* 非自愿切换：两个忙循环固定在同一 CPU 上，只能靠抢占轮流运行。 */
	cpu_set_t original, only_cpu0;
	if (sched_getaffinity(0, sizeof original, &original) != 0) return 20;
	CPU_ZERO(&only_cpu0);
	CPU_SET(0, &only_cpu0);
	if (sched_setaffinity(0, sizeof only_cpu0, &only_cpu0) != 0) return 21;
	getrusage(RUSAGE_SELF, &before);
	pid_t rival = fork();
	if (rival == 0) {
		burn(80000);
		_exit(0);
	}
	if (rival < 0) return 22;
	burn(80000);
	waitpid(rival, NULL, 0);
	getrusage(RUSAGE_SELF, &after);
	sched_setaffinity(0, sizeof original, &original);
	if (after.ru_nivcsw <= before.ru_nivcsw) return 23;

	/* wait4/waitid 的 rusage 与 SIGCHLD 的 si_utime/si_stime 报告 child 的 CPU 时间。 */
	struct sigaction action;
	memset(&action, 0, sizeof action);
	action.sa_sigaction = on_sigchld;
	action.sa_flags = SA_SIGINFO | SA_RESTART;
	sigemptyset(&action.sa_mask);
	if (sigaction(SIGCHLD, &action, NULL) != 0) return 24;
	/* 同一进程里先前的用例可能屏蔽过 SIGCHLD。 */
	sigset_t chld_only;
	sigemptyset(&chld_only);
	sigaddset(&chld_only, SIGCHLD);
	sigprocmask(SIG_UNBLOCK, &chld_only, NULL);
	pid_t worker = fork();
	if (worker == 0) {
		burn(60000);
		_exit(0);
	}
	if (worker < 0) return 25;
	struct rusage waited;
	int worker_status;
	if (wait4(worker, &worker_status, 0, &waited) != worker) return 26;
	if (used_us(&waited) < 55000) return 27;
	for (int i = 0; i < 100 && !chld_seen; i++) usleep(1000);
	if (!chld_seen) return 28;
	if (chld_utime + chld_stime < 5) return 32;
	signal(SIGCHLD, SIG_DFL);
	pid_t second = fork();
	if (second == 0) {
		burn(30000);
		_exit(0);
	}
	if (second < 0) return 29;
	siginfo_t reaped;
	struct rusage via_waitid;
	/* glibc/musl 的 waitid 包装不暴露 rusage，直接发 syscall。 */
	if (syscall(SYS_waitid, P_PID, (id_t)second, &reaped, WEXITED, &via_waitid) != 0) return 30;
	if (used_us(&via_waitid) < 25000) return 31;
	return 0;
}
