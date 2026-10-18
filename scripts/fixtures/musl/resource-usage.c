#define _GNU_SOURCE
#include <errno.h>
#include <stdint.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

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
	return 0;
}
