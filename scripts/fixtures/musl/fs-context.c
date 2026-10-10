#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

struct fs_probe { atomic_int phase; };

/* musl 1.2.6 的公开 clone wrapper 明确拒绝 Thread flags；这里使用它的同一 raw ABI
 * assembler entry（pthread_create 也使用），避免把 libc guard 误当作 kernel 验证。 */
extern int __clone(int (*)(void *), void *, int, void *, int *, void *, int *);

static int cwd_is(const char *expected)
{
	char cwd[128];
	return getcwd(cwd, sizeof cwd) && strcmp(cwd, expected) == 0;
}

static void wait_phase(struct fs_probe *probe, int phase)
{
	struct timespec pause = { .tv_nsec = 1000000 };
	for (int attempt = 0; atomic_load(&probe->phase) != phase; attempt++) {
		if (attempt == 1000) _exit(91);
		nanosleep(&pause, 0);
	}
}

static void *detach_fs(void *argument)
{
	struct fs_probe *probe = argument;
	if (chdir("/fs-shared") != 0 || umask(077) != 022) return 0;
	atomic_store(&probe->phase, 1);
	wait_phase(probe, 2);
	if (unshare(CLONE_FS) != 0 || chdir("/fs-private") != 0 || umask(033) != 077) return 0;
	pid_t child = fork();
	if (child == 0) _exit(cwd_is("/fs-private") && umask(022) == 033 ? 0 : 1);
	int status;
	if (child < 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 0;
	atomic_store(&probe->phase, 3);
	wait_phase(probe, 4);
	return cwd_is("/fs-private") && umask(022) == 033 ? argument : 0;
}

static int clone_with_private_fs(void *unused)
{
	(void)unused;
	/* 只有 raw clone 产生此实体，不加入 libc pthread 列表；callback 仅使用标准 fs syscall。 */
	if (chdir("/fs-private") != 0 || umask(011) != 022) _exit(92);
	return 0;
}

int verify_fs_context(void)
{
	pid_t child = fork();
	if (child == 0) {
		if (mkdir("/fs-shared", 0755) != 0 || mkdir("/fs-parent", 0755) != 0 || mkdir("/fs-private", 0755) != 0 || chdir("/") != 0) _exit(1);
		umask(022);
		struct fs_probe probe = {0};
		pthread_t worker;
		if (pthread_create(&worker, 0, detach_fs, &probe) != 0) _exit(2);
		wait_phase(&probe, 1);
		if (!cwd_is("/fs-shared") || umask(077) != 077) _exit(3);
		atomic_store(&probe.phase, 2);
		wait_phase(&probe, 3);
		if (!cwd_is("/fs-shared") || umask(077) != 077 || chdir("/fs-parent") != 0) _exit(4);
		atomic_store(&probe.phase, 4);
		void *joined;
		if (pthread_join(worker, &joined) != 0 || joined != &probe) _exit(5);
		/* 独占 context 的重复脱离幂等，标准 unknown flag 不改变 cwd/mask。 */
		if (unshare(CLONE_FS) != 0 || unshare(0) != 0) _exit(6);
		errno = 0;
		if (unshare(0x80000000U) != -1 || errno != EINVAL || !cwd_is("/fs-parent") || umask(022) != 077) _exit(7);
		const size_t stack_size = 64 * 1024;
		char *stack = mmap(0, stack_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		if (stack == MAP_FAILED) _exit(8);
		atomic_int clear_tid = -1;
		int flags = CLONE_VM | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD | CLONE_SYSVSEM | CLONE_SETTLS | CLONE_CHILD_CLEARTID;
		/* 显式共享 caller TLS 仅供这个不使用 pthread API 的 raw callback；parent 等待完成。 */
        if (__clone(clone_with_private_fs, stack + stack_size, flags, 0, 0, __builtin_thread_pointer(), (int *)&clear_tid) < 0) _exit(9);
		while (atomic_load(&clear_tid) != 0) syscall(SYS_sched_yield);
		if (!cwd_is("/fs-parent") || umask(022) != 022 || munmap(stack, stack_size) != 0 || chdir("/") != 0) _exit(10);
		if (rmdir("/fs-shared") != 0 || rmdir("/fs-parent") != 0 || rmdir("/fs-private") != 0) _exit(11);
		_exit(0);
	}
	int status;
	if (child <= 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) {
		printf("fs context child status=%d\n", child > 0 ? status : -1); return -1;
	}
	puts("LiteOS musl fs context sharing and unshare ok");
	return 0;
}
