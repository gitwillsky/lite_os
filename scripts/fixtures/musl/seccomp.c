#define _GNU_SOURCE
#include <errno.h>
#include <linux/audit.h>
#include <linux/filter.h>
#include <linux/futex.h>
#include <linux/seccomp.h>
#include <pthread.h>
#include <signal.h>
#include <sched.h>
#include <stddef.h>
#include <stdatomic.h>
#include <stdint.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <ucontext.h>
#include <unistd.h>

#if defined(__riscv)
/* 独立 consumer 的实际 types 验证 Linux LP64 alignment，不能只做 kernel codec 自洽 roundtrip。 */
_Static_assert(sizeof(siginfo_t) == 128, "Linux siginfo_t");
_Static_assert(_Alignof(mcontext_t) == 16, "Linux RISC-V mcontext alignment");
_Static_assert(sizeof(mcontext_t) == 784, "Linux RISC-V mcontext size");
_Static_assert(offsetof(ucontext_t, uc_mcontext) == 176, "Linux RISC-V ucontext padding");
_Static_assert(sizeof(ucontext_t) == 960, "Linux RISC-V ucontext size");
#endif

static int install_flags(int number, uint32_t action, unsigned flags)
{
	struct sock_filter code[] = {
		BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 0),
		BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, number, 0, 1),
		BPF_STMT(BPF_RET | BPF_K, action),
		BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
	};
	struct sock_fprog program = { .len = 4, .filter = code };
	return syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, flags, &program);
}

static int install(int number, uint32_t action)
{
    return install_flags(number, action, 0);
}

static int denied_parent(void)
{
	errno = 0;
	return syscall(SYS_getppid) == -1 && errno == EACCES;
}

int verify_seccomp_exec_image(void)
{
	return prctl(PR_GET_NO_NEW_PRIVS, 0UL, 0UL, 0UL, 0UL) == 1
		&& prctl(PR_GET_SECCOMP, 0UL, 0UL, 0UL, 0UL) == SECCOMP_MODE_FILTER
		&& denied_parent() ? 0 : 91;
}

static void *inherited_filter(void *unused)
{
	(void)unused;
	return (void *)(uintptr_t)verify_seccomp_exec_image();
}

static int inheritance(void)
{
	if (setgid(1000) || setuid(1000)) return 1;
	errno = 0;
	if (install(SYS_getppid, SECCOMP_RET_ERRNO | EPERM) != -1 || errno != EACCES) return 2;
	if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL) != 0) return 3;
	if (install(SYS_getppid, SECCOMP_RET_ERRNO | EPERM)
		|| install(SYS_getppid, SECCOMP_RET_ERRNO | EACCES) || !denied_parent()) return 4;
	struct sock_filter allow = BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW);
	struct sock_fprog program = { .len = 1, .filter = &allow };
	if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &program, 0UL, 0UL) != 0
		|| !denied_parent()) return 5;
	pthread_t thread;
	void *result = (void *)1;
	if (pthread_create(&thread, 0, inherited_filter, 0)
		|| pthread_join(thread, &result) || result != 0) return 6;
	pid_t child = fork();
	if (child == 0) {
		if (!denied_parent()) _exit(92);
		execl("/bin/init", "/bin/init", "seccomp-exec-image", (char *)0);
		_exit(93);
	}
	int status;
	if (child < 0 || waitpid(child, &status, 0) != child
		|| !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 7;
	return 0;
}

static volatile sig_atomic_t trap_ok;
static int trapped_number;
static unsigned long trapped_argument;

static void trap_handler(int signal, siginfo_t *info, void *context)
{
	ucontext_t *saved = context;
#if defined(__aarch64__)
	unsigned long *result = &saved->uc_mcontext.regs[0];
	unsigned long pc = saved->uc_mcontext.pc;
	unsigned int architecture = AUDIT_ARCH_AARCH64;
#elif defined(__riscv)
	unsigned long *result = &saved->uc_mcontext.__gregs[REG_A0];
	unsigned long pc = saved->uc_mcontext.__gregs[REG_PC];
	unsigned int architecture = AUDIT_ARCH_RISCV64;
#else
#error Unsupported seccomp runtime fixture architecture
#endif
	trap_ok = signal == SIGSYS && info->si_code == SYS_SECCOMP && info->si_errno == 42
		&& info->si_syscall == trapped_number && info->si_arch == architecture
		&& (unsigned long)info->si_call_addr == pc && pc && *result == trapped_argument;
	*result = 777;
}

static int traps(void)
{
	struct sigaction action = { .sa_sigaction = trap_handler, .sa_flags = SA_SIGINFO };
	if (sigaction(SIGSYS, &action, 0) || install(SYS_getpid, SECCOMP_RET_TRAP | 42)
		|| install(SYS_execve, SECCOMP_RET_TRAP | 42)) return 1;
	trapped_number = SYS_getpid;
	trapped_argument = 0x123456;
	if (syscall(SYS_getpid, trapped_argument) != 777 || !trap_ok) return 2;
	trapped_number = SYS_execve;
	trapped_argument = 0;
	trap_ok = 0;
	if (syscall(SYS_execve, 0, 0, 0) != 777 || !trap_ok) return 3;
	return 0;
}

static int strict_fatal(void)
{
	if (syscall(SYS_seccomp, SECCOMP_SET_MODE_STRICT, 0, 0)) return 1;
	syscall(SYS_getpid);
	return 99;
}

static _Atomic int clear_tid;
static _Atomic int thread_phase;

static int prepare_kernel_thread_exit(void)
{
	if (syscall(SYS_set_tid_address, &clear_tid) <= 0) {
		atomic_store_explicit(&thread_phase, 1, memory_order_release);
		atomic_store_explicit(&clear_tid, 0, memory_order_release);
		syscall(SYS_futex, &clear_tid, FUTEX_WAKE, 1, 0, 0, 0);
		return -1;
	}
	return 0;
}

static int wait_kernel_thread_exit(void)
{
	/* seccomp fatal 绕过 libc pthread_exit，musl 的 detach_state 不会更新；pthread_join
	 * 在 Linux 上也会永久等待。这里验证 kernel 的标准 clear-child-tid/futex completion。 */
	while (atomic_load_explicit(&clear_tid, memory_order_acquire)) {
		if (syscall(SYS_futex, &clear_tid, FUTEX_WAIT, 1, 0, 0, 0) == -1
			&& errno != EAGAIN && errno != EINTR) return -1;
	}
	return atomic_load_explicit(&thread_phase, memory_order_acquire) == 2 ? 0 : -1;
}

static void *strict_thread(void *unused)
{
	(void)unused;
	if (prepare_kernel_thread_exit()) return (void *)1;
	/* prctl STRICT 按 Linux 忽略 filter pointer 和后两个参数。 */
	if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_STRICT, 123UL, 456UL, 789UL) != 0) {
		atomic_store_explicit(&thread_phase, 1, memory_order_release);
		return (void *)1;
	}
	atomic_store_explicit(&thread_phase, 2, memory_order_release);
	syscall(SYS_getpid);
	return (void *)2;
}

static int strict_thread_survival(void)
{
	pthread_t thread;
	atomic_store(&clear_tid, 1);
	atomic_store(&thread_phase, 0);
	if (pthread_create(&thread, 0, strict_thread, 0) || wait_kernel_thread_exit()) return 1;
	return getpid() > 0 && prctl(PR_GET_SECCOMP, 0UL, 0UL, 0UL, 0UL) == 0 ? 0 : 2;
}

static void *kill_thread(void *argument)
{
	if (prepare_kernel_thread_exit()) return (void *)1;
	if (install(SYS_getpid, (uint32_t)(uintptr_t)argument)) {
		atomic_store_explicit(&thread_phase, 1, memory_order_release);
		return (void *)1;
	}
	atomic_store_explicit(&thread_phase, 2, memory_order_release);
	syscall(SYS_getpid);
	return (void *)2;
}

static int filter_thread_survival(void)
{
	pthread_t thread;
	atomic_store(&clear_tid, 1);
	atomic_store(&thread_phase, 0);
	if (pthread_create(&thread, 0, kill_thread, (void *)(uintptr_t)SECCOMP_RET_KILL_THREAD)
		|| wait_kernel_thread_exit()) return 1;
	return getpid() > 0 ? 0 : 2;
}

static int filter_group_fatal(void)
{
	pthread_t thread;
	atomic_store(&clear_tid, 1);
	atomic_store(&thread_phase, 0);
	if (pthread_create(&thread, 0, kill_thread, (void *)(uintptr_t)SECCOMP_RET_KILL_PROCESS)) return 1;
	wait_kernel_thread_exit();
	return 99;
}

static int private_syscall(void)
{
#if defined(__riscv)
	if (install(SYS_riscv_hwprobe, SECCOMP_RET_ERRNO | EACCES)) return 1;
	errno = 0;
	if (syscall(SYS_riscv_hwprobe, 0, 0, 0, 0, 0) != -1 || errno != EACCES) return 2;
#endif
	if (install(SYS_getppid, SECCOMP_RET_TRACE)) return 3;
	errno = 0;
	return syscall(SYS_getppid) == -1 && errno == ENOSYS ? 0 : 4;
}

static int skipped_exec_zero(void)
{
	if (install(SYS_execve, SECCOMP_RET_ERRNO)) return 1;
	/* 返回 0 只代表 filter 跳过 syscall；旧映像必须继续且 a0/x0 确实为 0。 */
	return syscall(SYS_execve, "/nonexistent-seccomp-image", 0, 0) == 0 ? 0 : 2;
}

static int immutable_copy(void)
{
	struct sock_filter code[] = {
		BPF_STMT(BPF_LD | BPF_W | BPF_ABS, 0),
		BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getppid, 0, 1),
		BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EACCES),
		BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
	};
	struct sock_fprog program = { .len = 4, .filter = code };
	if (syscall(SYS_seccomp, SECCOMP_SET_MODE_FILTER, 0, &program)) return 1;
	/* volatile 保证修改实际发生；数组仍存活时立即触发，不能借栈复用伪装 copy 证明。 */
	((volatile struct sock_filter *)code)[2].k = SECCOMP_RET_ALLOW;
	return denied_parent() ? 0 : 2;
}

static _Atomic int sync_ready;
static _Atomic int sync_go;
static int sync_tid;
static int sync_divergent;
static int sync_expect_nnp;

static void *sync_worker(void *unused)
{
    (void)unused;
    sync_tid = syscall(SYS_gettid);
    if (sync_divergent && install(SYS_getpid, SECCOMP_RET_ERRNO | EPERM)) {
        atomic_store_explicit(&sync_ready, -1, memory_order_release);
        return (void *)1;
    }
    atomic_store_explicit(&sync_ready, 1, memory_order_release);
    while (!atomic_load_explicit(&sync_go, memory_order_acquire)) sched_yield();
    if (sync_divergent) {
        errno = 0;
        return syscall(SYS_getpid) == -1 && errno == EPERM ? 0 : (void *)2;
    }
    if (!denied_parent() || prctl(PR_GET_NO_NEW_PRIVS, 0UL, 0UL, 0UL, 0UL) != sync_expect_nnp)
        return (void *)3;
    /* 创建者早于 TSYNC 已经存在；其随后 clone 的 child 必须从提交点继承策略。 */
    pthread_t child;
    void *result = (void *)1;
    if (pthread_create(&child, 0, inherited_filter, 0)
        || pthread_join(child, &result) || result != 0) return (void *)4;
    return 0;
}

static int synchronize_threads(void)
{
    pthread_t thread;
    atomic_store(&sync_ready, 0);
    atomic_store(&sync_go, 0);
    sync_divergent = 0;
    sync_expect_nnp = 1;
    if (pthread_create(&thread, 0, sync_worker, 0)) return 1;
    while (!atomic_load_explicit(&sync_ready, memory_order_acquire)) sched_yield();
    if (atomic_load(&sync_ready) != 1) return 2;
    if (prctl(PR_SET_NO_NEW_PRIVS, 1UL, 0UL, 0UL, 0UL)
        || install_flags(SYS_getppid, SECCOMP_RET_ERRNO | EACCES, SECCOMP_FILTER_FLAG_TSYNC)
        || !denied_parent()) return 3;
    atomic_store_explicit(&sync_go, 1, memory_order_release);
    void *result = (void *)1;
    return pthread_join(thread, &result) || result != 0 ? 4 : 0;
}

static int synchronize_divergent_failure(void)
{
    pthread_t thread;
    atomic_store(&sync_ready, 0);
    atomic_store(&sync_go, 0);
    sync_divergent = 1;
    if (pthread_create(&thread, 0, sync_worker, 0)) return 1;
    while (!atomic_load_explicit(&sync_ready, memory_order_acquire)) sched_yield();
    if (atomic_load(&sync_ready) != 1) return 2;
    if (install_flags(SYS_getppid, SECCOMP_RET_ERRNO | EACCES, SECCOMP_FILTER_FLAG_TSYNC) != sync_tid
        || prctl(PR_GET_SECCOMP, 0UL, 0UL, 0UL, 0UL) != 0 || getppid() <= 0) return 3;
    errno = 0;
    if (install_flags(SYS_getppid, SECCOMP_RET_ERRNO | EACCES,
            SECCOMP_FILTER_FLAG_TSYNC | SECCOMP_FILTER_FLAG_TSYNC_ESRCH) != -1
        || errno != ESRCH || prctl(PR_GET_SECCOMP, 0UL, 0UL, 0UL, 0UL) != 0) return 4;
    atomic_store_explicit(&sync_go, 1, memory_order_release);
    void *result = (void *)1;
    if (pthread_join(thread, &result) || result != 0) return 5;
    return install_flags(SYS_getppid, SECCOMP_RET_ERRNO | EACCES, SECCOMP_FILTER_FLAG_TSYNC)
        || !denied_parent() ? 6 : 0;
}

static int run(const char *name, int (*scenario)(void), int signal)
{
	write(1, name, strlen(name));
	write(1, "\n", 1);
	pid_t child = fork();
	if (child == 0) _exit(scenario());
	int status;
	if (child < 0 || waitpid(child, &status, 0) != child) return 1;
	if (signal) return WIFSIGNALED(status) && WTERMSIG(status) == signal ? 0 : 2;
	return WIFEXITED(status) && WEXITSTATUS(status) == 0 ? 0 : 3;
}

int verify_seccomp_notifications(void);

int verify_seccomp(void)
{
	uint32_t action = SECCOMP_RET_KILL_PROCESS;
	if (syscall(SYS_seccomp, SECCOMP_GET_ACTION_AVAIL, 0, &action) != 0) return 1;
	if (run("seccomp: inheritance", inheritance, 0)) return 2;
	if (run("seccomp: trap", traps, 0)) return 3;
	if (run("seccomp: strict fatal", strict_fatal, SIGKILL)) return 4;
	if (run("seccomp: strict thread", strict_thread_survival, 0)) return 5;
	if (run("seccomp: filter thread", filter_thread_survival, 0)) return 6;
	if (run("seccomp: filter group", filter_group_fatal, SIGSYS)) return 7;
	if (run("seccomp: private syscall", private_syscall, 0)) return 8;
	if (run("seccomp: skipped exec", skipped_exec_zero, 0)) return 9;
	if (run("seccomp: immutable copy", immutable_copy, 0)) return 10;
    if (run("seccomp: TSYNC inheritance", synchronize_threads, 0)) return 11;
    if (run("seccomp: TSYNC divergent", synchronize_divergent_failure, 0)) return 12;
    if (run("seccomp: user notifications", verify_seccomp_notifications, 0)) return 13;
	return 0;
}
