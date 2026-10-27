#define _GNU_SOURCE
#include <signal.h>
#include <errno.h>
#include <sys/resource.h>
#include <time.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <sys/syscall.h>
#include <ucontext.h>
#include <unistd.h>

static int damaged_word;
static int block_bad_frame_signal;
static uintptr_t expected_frame;
static unsigned char alternate_stack[32768] __attribute__((aligned(16)));

__attribute__((naked, noreturn)) static void sigreturn_at(void *address)
{
#if defined(__aarch64__)
	__asm__ volatile("mov sp, x0\nmov x8, #139\nsvc #0\nbrk #0");
#elif defined(__riscv)
	__asm__ volatile("mv sp, a0\nli a7, 139\necall\nebreak");
#endif
}

static void damage_frame(int signal, siginfo_t *info, void *context)
{
	(void)signal;
	(void)info;
	ucontext_t *saved = context;
#if defined(__aarch64__)
	/* Linux 在 parse extension header 前已读取 saved SP；坏 header 报告该地址。 */
	expected_frame = saved->uc_mcontext.sp;
	if (damaged_word == 3) {
		/* 内容完整且可读，只破坏 SP alignment；后续 page fault 不能替代本检查。 */
		unsigned char *unaligned = alternate_stack + 8192 + 8;
		memcpy(unaligned, info, sizeof(siginfo_t) + sizeof(ucontext_t));
		expected_frame = (uintptr_t)unaligned;
		sigreturn_at(unaligned);
	}
#endif
	if (block_bad_frame_signal) sigaddset(&saved->uc_sigmask, SIGSEGV);
	uint32_t invalid = 1;
#if defined(__riscv)
	_Static_assert(sizeof(mcontext_t) == 784, "Linux RISC-V mcontext");
	/* Linux 的 quad/extension union 尾部依次是 reserved、END magic、END size。 */
	unsigned char *tail = (unsigned char *)&saved->uc_mcontext + sizeof(mcontext_t) - 12;
	memcpy(tail + damaged_word * sizeof invalid, &invalid, sizeof invalid);
#elif defined(__aarch64__)
	/* 第一个 FPSIMD header 与其后 END header 均按 Linux 标准布局损坏。 */
	const unsigned int offsets[] = { 0, 528, 532 };
	memcpy((unsigned char *)saved->uc_mcontext.__reserved + offsets[damaged_word], &invalid, sizeof invalid);
#else
#error Unsupported signal runtime fixture architecture
#endif
}

static void caught_bad_frame(int signal, siginfo_t *info, void *context)
{
	(void)context;
	/* 检查架构标准的 code/address，排除 restorer 后续 fault 冒充成功。 */
#if defined(__aarch64__)
	int code = expected_frame == UINTPTR_MAX ? SEGV_MAPERR : SEGV_ACCERR;
	_exit(signal == SIGSEGV && info->si_code == code
		&& (uintptr_t)info->si_addr == expected_frame ? 0 : 91);
#elif defined(__riscv)
	_exit(signal == SIGSEGV && info->si_code == SI_KERNEL ? 0 : 91);
#endif
}

static int scenario(int word, int caught, int blocked)
{
	block_bad_frame_signal = blocked;
	if (caught) {
		stack_t stack = { .ss_sp = alternate_stack, .ss_size = sizeof alternate_stack };
		struct sigaction action = {
			.sa_sigaction = caught_bad_frame, .sa_flags = SA_SIGINFO | SA_ONSTACK,
		};
		if (sigaltstack(&stack, 0) || sigaction(SIGSEGV, &action, 0)) return 1;
	}
	if (word < 0) {
		expected_frame = word == -2 ? UINTPTR_MAX : 0;
		sigreturn_at((void *)expected_frame);
	}
	damaged_word = word;
	struct sigaction action = { .sa_sigaction = damage_frame, .sa_flags = SA_SIGINFO };
	if (sigaction(SIGUSR1, &action, 0) || raise(SIGUSR1)) return 2;
	return 3;
}

struct kernel_action {
	uintptr_t handler;
	unsigned long flags;
#if defined(__aarch64__)
	uintptr_t restorer;
#endif
	uint64_t mask;
};

static void simple_handler(int signal) { (void)signal; }

#if defined(__aarch64__)
__attribute__((naked, noreturn)) static void custom_restorer(void)
{
	__asm__ volatile("mov x0, #42\nmov x8, #93\nsvc #0\nbrk #0");
}
#endif

static int verify_action_layout(void)
{
	struct sigaction action = { .sa_handler = simple_handler, .sa_flags = SA_RESTART };
	sigaddset(&action.sa_mask, SIGTERM);
	sigaddset(&action.sa_mask, SIGSEGV);
	if (sigaction(SIGUSR2, &action, 0)) return 1;
	struct { struct kernel_action action; uint64_t canary; } query = { .canary = UINT64_MAX };
	if (syscall(SYS_rt_sigaction, SIGUSR2, 0, &query.action, 8)) return 2;
	uint64_t mask = (UINT64_C(1) << (SIGTERM - 1)) | (UINT64_C(1) << (SIGSEGV - 1));
	if (query.canary != UINT64_MAX || query.action.handler != (uintptr_t)simple_handler
		|| query.action.mask != mask || !(query.action.flags & SA_RESTART)) return 3;
#if defined(__aarch64__)
	_Static_assert(sizeof(struct kernel_action) == 32, "Linux AArch64 kernel sigaction");
	if (!(query.action.flags & SA_RESTORER) || !query.action.restorer) return 4;
	pid_t child = fork();
	if (child == 0) {
		query.action.restorer = (uintptr_t)custom_restorer;
		query.action.mask = 0;
		if (syscall(SYS_rt_sigaction, SIGUSR2, &query.action, 0, 8)) _exit(90);
		raise(SIGUSR2);
		_exit(91);
	}
	int status;
	if (child < 0 || waitpid(child, &status, 0) != child
		|| !WIFEXITED(status) || WEXITSTATUS(status) != 42) return 5;
#else
	_Static_assert(sizeof(struct kernel_action) == 24, "Linux RISC-V kernel sigaction");
#endif
	return 0;
}

static int consume_realtime(uint64_t mask, int signal, uint64_t value, int code)
{
    siginfo_t info;
    struct timespec immediate = { 0 };
    memset(&info, 0xa5, sizeof info);
    long result = syscall(SYS_rt_sigtimedwait, &mask, &info, &immediate, 8);
    if (result != signal || info.si_signo != signal || info.si_code != code
        || (uint64_t)(uintptr_t)info.si_value.sival_ptr != value) return 1;
    const unsigned char *bytes = (const unsigned char *)&info;
    for (size_t index = 48; index < sizeof info; index++) if (bytes[index]) return 2;
    return 0;
}

static int verify_realtime_queue(void)
{
    const int low = SIGRTMIN, high = SIGRTMIN + 1;
    sigset_t blocked, previous;
    sigemptyset(&blocked);
    sigaddset(&blocked, low);
    sigaddset(&blocked, high);
    sigaddset(&blocked, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &blocked, &previous)) return 1;
    uint64_t mask = (UINT64_C(1) << (low - 1)) | (UINT64_C(1) << (high - 1));
    struct rlimit saved;
    if (getrlimit(RLIMIT_SIGPENDING, &saved)) return 2;
    struct rlimit limit = saved;
    limit.rlim_cur = 3;
    if (setrlimit(RLIMIT_SIGPENDING, &limit)) return 3;
    union sigval value = { .sival_ptr = (void *)(uintptr_t)20 };
    if (sigqueue(getpid(), high, value)) return 4;
    value.sival_ptr = (void *)(uintptr_t)10;
    if (sigqueue(getpid(), low, value)) return 5;
    value.sival_ptr = (void *)(uintptr_t)11;
    if (sigqueue(getpid(), low, value)) return 6;
    uint64_t pending = 0;
    if (syscall(SYS_rt_sigpending, &pending, 8) || (pending & mask) != mask) return 32;
    errno = 0;
    if (syscall(SYS_rt_sigpending, &pending, 9) != -1 || errno != EINVAL) return 33;
    if (syscall(SYS_rt_sigpending, (void *)(uintptr_t)-1, 0)) return 34;
    unsigned char partial[2] = { 0xa5, 0xa5 };
    if (syscall(SYS_rt_sigpending, partial, 1) || partial[0] != (unsigned char)pending || partial[1] != 0xa5) return 35;
    errno = 0;
    if (sigqueue(getpid(), high, value) != -1 || errno != EAGAIN) return 7;
    if (consume_realtime(mask, low, 10, SI_QUEUE)) return 8;
    value.sival_ptr = (void *)(uintptr_t)12;
    if (sigqueue(getpid(), low, value)) return 9;
    if (syscall(SYS_rt_sigpending, &pending, 8) || !(pending & (UINT64_C(1) << (low - 1)))) return 36;
    if (consume_realtime(mask, low, 11, SI_QUEUE)
        || consume_realtime(mask, low, 12, SI_QUEUE)
        || consume_realtime(mask, high, 20, SI_QUEUE)) return 10;
    struct timespec immediate = { 0 };
    errno = 0;
    if (syscall(SYS_rt_sigtimedwait, &mask, 0, &immediate, 8) != -1 || errno != EAGAIN) return 11;
    /* Thread-private pending 优先于 shared pending；两条路径都保留独立 siginfo。 */
    value.sival_ptr = (void *)(uintptr_t)30;
    if (sigqueue(getpid(), low, value)) return 12;
    siginfo_t thread_info = { .si_signo = high, .si_code = SI_QUEUE };
    thread_info.si_pid = 42; /* user negative source 可指定历史 si_pid，不能改写为实际 caller PID。 */
    thread_info.si_uid = getuid();
    thread_info.si_value.sival_ptr = (void *)(uintptr_t)40;
    if (syscall(SYS_rt_tgsigqueueinfo, getpid(), syscall(SYS_gettid), high, &thread_info)) return 13;
    if (consume_realtime(mask, high, 40, SI_QUEUE) || consume_realtime(mask, low, 30, SI_QUEUE)) return 14;
    /* SIG_IGN transition 丢弃所有同号 record，归还实际 recipient 额度。 */
    for (int index = 0; index < 3; index++) if (sigqueue(getpid(), low, value)) return 15;
    struct sigaction ignored = { .sa_handler = SIG_IGN }, old_action;
    if (sigaction(low, &ignored, &old_action)) return 16;
    if (sigaction(low, &old_action, 0)) return 17;
    limit.rlim_cur = 0;
    if (setrlimit(RLIMIT_SIGPENDING, &limit)) return 18;
    errno = 0;
    if (syscall(SYS_tgkill, getpid(), syscall(SYS_gettid), low) != -1 || errno != EAGAIN) return 19;
    if (kill(getpid(), low)) return 20;
    if (consume_realtime(mask, low, 0, SI_USER)) return 21;
    sigset_t ignored_set;
    sigemptyset(&ignored_set);
    sigaddset(&ignored_set, low);
    /* musl sigqueue 临时屏蔽所有 app signals；此处必须验证实际未屏蔽的原生 generation。 */
    siginfo_t ignored_info = { .si_signo = low, .si_code = SI_QUEUE };
    ignored_info.si_pid = getpid(); ignored_info.si_uid = getuid(); ignored_info.si_value = value;
    if (sigaction(low, &ignored, 0) || sigprocmask(SIG_UNBLOCK, &ignored_set, 0)
        || syscall(SYS_rt_sigqueueinfo, getpid(), low, &ignored_info)
        || sigprocmask(SIG_BLOCK, &ignored_set, 0) || sigaction(low, &old_action, 0)) return 22;
    limit.rlim_cur = 3;
    if (setrlimit(RLIMIT_SIGPENDING, &limit)) return 23;
    /* Unknown layout 只接受 native prefix 以外全零；kernel padding/errno/union 原样返回。 */
    siginfo_t unknown = { .si_signo = 1, .si_code = -200, .si_errno = 57 };
    unknown.si_pid = 43;
    unknown.si_uid = getuid();
    unknown.si_value.sival_ptr = (void *)(uintptr_t)50;
    unsigned char *native = (unsigned char *)&unknown;
    native[12] = 0x5a;
    native[48] = 1;
    errno = 0;
    if (syscall(SYS_rt_sigqueueinfo, getpid(), low, &unknown) != -1 || errno != E2BIG) return 24;
    native[48] = 0;
    if (syscall(SYS_rt_sigqueueinfo, getpid(), low, &unknown)) return 25;
    siginfo_t received;
    if (syscall(SYS_rt_sigtimedwait, &mask, &received, &immediate, 8) != low) return 26;
    unknown.si_signo = low;
    if (memcmp(&received, &unknown, sizeof received)) return 27;
    value.sival_ptr = (void *)(uintptr_t)51;
    if (sigqueue(getpid(), low, value)) return 37;
    struct timespec huge = { .tv_sec = INT64_MAX, .tv_nsec = 999999999 };
    if (syscall(SYS_rt_sigtimedwait, &mask, &received, &huge, 8) != low
        || (uintptr_t)received.si_value.sival_ptr != 51) return 38;
    /* positive source 与 SI_TKILL 只能发给当前 TID，非特权与 root 使用相同限制。 */
    pid_t child = fork();
    if (child == 0) _exit(0);
    if (child < 0) return 28;
    unknown.si_code = SI_KERNEL;
    errno = 0;
    if (syscall(SYS_rt_sigqueueinfo, child, low, &unknown) != -1 || errno != EPERM) return 29;
    int status;
    if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status)) return 30;
    if (setrlimit(RLIMIT_SIGPENDING, &saved) || sigprocmask(SIG_SETMASK, &previous, 0)) return 31;
    return 0;
}

static int verify_ordinary_signal_accounting(void)
{
    sigset_t blocked, previous;
    sigemptyset(&blocked); sigaddset(&blocked, SIGUSR1); sigaddset(&blocked, SIGRTMIN); sigaddset(&blocked, SIGCHLD);
    if (sigprocmask(SIG_BLOCK, &blocked, &previous)) return 1;
    struct rlimit saved, limit;
    if (getrlimit(RLIMIT_SIGPENDING, &saved)) return 2;
    limit = saved; limit.rlim_cur = 1;
    if (setrlimit(RLIMIT_SIGPENDING, &limit)) return 3;
    union sigval value = { .sival_ptr = (void *)(uintptr_t)91 };
    if (sigqueue(getpid(), SIGUSR1, value)) return 4;
    errno = 0;
    if (sigqueue(getpid(), SIGRTMIN, value) != -1 || errno != EAGAIN) return 5;
    value.sival_ptr = (void *)(uintptr_t)92;
    if (sigqueue(getpid(), SIGUSR1, value)) return 6;
    uint64_t mask = UINT64_C(1) << (SIGUSR1 - 1);
    if (consume_realtime(mask, SIGUSR1, 91, SI_QUEUE)) return 7;
    if (sigqueue(getpid(), SIGRTMIN, value)
        || consume_realtime(UINT64_C(1) << (SIGRTMIN - 1), SIGRTMIN, 92, SI_QUEUE)) return 8;
    limit.rlim_cur = 0;
    if (setrlimit(RLIMIT_SIGPENDING, &limit)) return 9;
    /* 普通负 code 没有 record 时保留 native SI_USER bit，仍返回成功。 */
    if (sigqueue(getpid(), SIGUSR1, value) || consume_realtime(mask, SIGUSR1, 0, SI_USER)) return 10;
    /* 普通 kill 的 nonnegative source 可越限，但实际节点仍占 UID ucounts。 */
    if (kill(getpid(), SIGUSR1)) return 11;
    limit.rlim_cur = 1;
    if (setrlimit(RLIMIT_SIGPENDING, &limit)) return 12;
    errno = 0;
    if (sigqueue(getpid(), SIGRTMIN, value) != -1 || errno != EAGAIN) return 13;
    siginfo_t info;
    struct timespec immediate = { 0 };
    if (syscall(SYS_rt_sigtimedwait, &mask, &info, &immediate, 8) != SIGUSR1
        || info.si_code != SI_USER || info.si_pid != getpid() || info.si_uid != getuid()) return 14;
    /* 子退出通知的真正预留节点也占同一额度；reap 不能提前释放未消费的 SIGCHLD。 */
    pid_t child = fork();
    if (child == 0) _exit(23);
    int status;
    if (child < 0 || waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status) != 23) return 16;
    errno = 0;
    if (sigqueue(getpid(), SIGRTMIN, value) != -1 || errno != EAGAIN) return 17;
    mask = UINT64_C(1) << (SIGCHLD - 1);
    if (syscall(SYS_rt_sigtimedwait, &mask, &info, &immediate, 8) != SIGCHLD
        || info.si_code != CLD_EXITED || info.si_pid != child || info.si_status != 23) return 18;
    if (sigqueue(getpid(), SIGRTMIN, value)
        || consume_realtime(UINT64_C(1) << (SIGRTMIN - 1), SIGRTMIN, 92, SI_QUEUE)) return 19;
    if (setrlimit(RLIMIT_SIGPENDING, &saved) || sigprocmask(SIG_SETMASK, &previous, 0)) return 15;
    return 0;
}

static int wait_timer_pending(uint64_t mask)
{
    struct timespec delay = { .tv_nsec = 1000000 };
    for (int attempt = 0; attempt < 1000; attempt++) {
        uint64_t pending = 0;
        if (syscall(SYS_rt_sigpending, &pending, 8)) return -1;
        if (pending & mask) return 0;
        if (nanosleep(&delay, 0)) return -1;
    }
    return -1;
}

static int verify_posix_timer_notifications(void)
{
    const int signal = SIGRTMIN;
    sigset_t blocked, previous;
    sigemptyset(&blocked); sigaddset(&blocked, signal); sigaddset(&blocked, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &blocked, &previous)) return 1;
    struct rlimit saved, limit;
    if (getrlimit(RLIMIT_SIGPENDING, &saved)) return 2;
    limit = saved; limit.rlim_cur = 2;
    if (setrlimit(RLIMIT_SIGPENDING, &limit)) return 3;
    struct sigevent silent = { .sigev_notify = SIGEV_NONE };
    int silent_id, second_id, timer_id, extra;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &silent, &silent_id)
        || syscall(SYS_timer_create, CLOCK_MONOTONIC, &silent, &second_id)) return 4;
    errno = 0;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &silent, &extra) != -1 || errno != EAGAIN) return 5;
    if (syscall(SYS_timer_delete, second_id)) return 6;
    struct sigevent event = { .sigev_notify = SIGEV_SIGNAL, .sigev_signo = signal,
        .sigev_value.sival_ptr = (void *)(uintptr_t)71 };
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &timer_id)) return 7;
    /* 创建时预留配额；以后降为零不会阻止该 timer 到期，也不能每个 interval 重新收费。 */
    limit.rlim_cur = 0;
    if (setrlimit(RLIMIT_SIGPENDING, &limit)) return 8;
    struct itimerspec periodic = { .it_value.tv_nsec = 1000000, .it_interval.tv_nsec = 1000000 };
    uint64_t mask = UINT64_C(1) << (signal - 1);
    if (syscall(SYS_timer_settime, timer_id, 0, &periodic, 0) || wait_timer_pending(mask)) return 9;
    struct timespec delay = { .tv_nsec = 8000000 }, immediate = { 0 };
    if (nanosleep(&delay, 0)) return 10;
    siginfo_t received;
    if (syscall(SYS_rt_sigtimedwait, &mask, &received, &immediate, 8) != signal
        || received.si_code != SI_TIMER || received.si_timerid != timer_id
        || (uintptr_t)received.si_value.sival_ptr != 71 || received.si_overrun < 3) return 11;
    if (syscall(SYS_timer_getoverrun, timer_id) != received.si_overrun) return 12;
    struct itimerspec disarmed = { 0 };
    if (syscall(SYS_timer_settime, timer_id, 0, &disarmed, 0)) return 13;
    errno = 0;
    if (syscall(SYS_rt_sigtimedwait, &mask, &received, &immediate, 8) != -1 || errno != EAGAIN) return 14;
    /* 删除后的原 queued record 仍持原 charge，消费只丢弃，不能重装同 ID 新 timer。 */
    limit.rlim_cur = 2;
    if (setrlimit(RLIMIT_SIGPENDING, &limit)
        || syscall(SYS_timer_settime, timer_id, 0, &periodic, 0) || wait_timer_pending(mask)
        || syscall(SYS_timer_delete, timer_id)) return 15;
    errno = 0;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &silent, &extra) != -1 || errno != EAGAIN) return 16;
    errno = 0;
    if (syscall(SYS_rt_sigtimedwait, &mask, &received, &immediate, 8) != -1 || errno != EAGAIN) return 17;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &timer_id)) return 18;
    struct itimerspec snapshot;
    if (syscall(SYS_timer_gettime, timer_id, &snapshot)
        || snapshot.it_value.tv_sec || snapshot.it_value.tv_nsec) return 19;
    /* 未屏蔽 SIG_IGN 的周期 notification 暂存；恢复 disposition 时发布并补足 overrun。 */
    struct sigaction ignored = { .sa_handler = SIG_IGN }, old_action;
    sigset_t one; sigemptyset(&one); sigaddset(&one, signal);
    if (sigaction(signal, &ignored, &old_action) || sigprocmask(SIG_UNBLOCK, &one, 0)
        || syscall(SYS_timer_settime, timer_id, 0, &periodic, 0) || nanosleep(&delay, 0)
        || sigprocmask(SIG_BLOCK, &one, 0) || sigaction(signal, &old_action, 0)) return 20;
    if (syscall(SYS_rt_sigtimedwait, &mask, &received, &immediate, 8) != signal
        || received.si_code != SI_TIMER || received.si_overrun < 3) return 21;
    if (syscall(SYS_timer_settime, timer_id, 0, &disarmed, 0)
        || syscall(SYS_timer_delete, timer_id) || syscall(SYS_timer_delete, silent_id)) return 22;
    /* 两个普通同号 timer 都有独立预留 notification，不能按 legacy kill 的规则合并。 */
    event.sigev_signo = SIGUSR1; event.sigev_value.sival_ptr = (void *)(uintptr_t)81;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &timer_id)) return 23;
    event.sigev_value.sival_ptr = (void *)(uintptr_t)82;
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &second_id)) return 24;
    struct itimerspec once = { .it_value.tv_nsec = 1000000 };
    if (syscall(SYS_timer_settime, timer_id, 0, &once, 0)
        || syscall(SYS_timer_settime, second_id, 0, &once, 0)) return 25;
    mask = UINT64_C(1) << (SIGUSR1 - 1);
    struct timespec timeout = { .tv_sec = 1 };
    int seen = 0;
    for (int index = 0; index < 2; index++) {
        if (syscall(SYS_rt_sigtimedwait, &mask, &received, &timeout, 8) != SIGUSR1
            || received.si_code != SI_TIMER) return 26;
        uintptr_t value = (uintptr_t)received.si_value.sival_ptr;
        if (value == 81 && received.si_timerid == timer_id) seen |= 1;
        else if (value == 82 && received.si_timerid == second_id) seen |= 2;
        else return 27;
    }
    if (seen != 3 || syscall(SYS_timer_delete, timer_id) || syscall(SYS_timer_delete, second_id)) return 28;
    /* raw Linux SIGEV_THREAD_ID 的目标输入是当前 namespace 可见的 TID。 */
    event.sigev_signo = signal; event.sigev_notify = SIGEV_THREAD_ID;
    event.sigev_value.sival_ptr = (void *)(uintptr_t)83;
    int visible_tid = syscall(SYS_gettid);
    memcpy((unsigned char *)&event + 16, &visible_tid, sizeof visible_tid);
    if (syscall(SYS_timer_create, CLOCK_MONOTONIC, &event, &timer_id)
        || syscall(SYS_timer_settime, timer_id, 0, &once, 0)) return 29;
    mask = UINT64_C(1) << (signal - 1);
    if (syscall(SYS_rt_sigtimedwait, &mask, &received, &timeout, 8) != signal
        || received.si_code != SI_TIMER || (uintptr_t)received.si_value.sival_ptr != 83
        || syscall(SYS_timer_delete, timer_id)) return 30;
    if (setrlimit(RLIMIT_SIGPENDING, &saved) || sigprocmask(SIG_SETMASK, &previous, 0)) return 31;
    return 0;
}

#if defined(__aarch64__)
/* 子进程独占探针状态；handler 只校验本次指令的地址/方向，然后越过该指令返回。 */
static uintptr_t alignment_address;
static volatile sig_atomic_t alignment_seen;
static int alignment_write;

static void catch_alignment(int signal, siginfo_t *info, void *context)
{
    ucontext_t *saved = context;
    if (signal != SIGBUS || info->si_code != BUS_ADRALN
        || (uintptr_t)info->si_addr != alignment_address
        || saved->uc_mcontext.fault_address != alignment_address) _exit(91);
    unsigned char *record = (unsigned char *)saved->uc_mcontext.__reserved;
    unsigned char *end = record + sizeof(saved->uc_mcontext.__reserved);
    uint64_t esr = 0;
    while (record + 8 <= end) {
        uint32_t header[2]; memcpy(header, record, sizeof header);
        if (!header[0] || !header[1]) break;
        if (header[1] < 16 || header[1] % 16 || header[1] > (size_t)(end - record)) _exit(92);
        if (header[0] == 0x45535201 && header[1] == 16) memcpy(&esr, record + 8, sizeof esr);
        record += header[1];
    }
    if ((esr >> 26) != 0x24 || (esr & 63) != 0x21
        || ((esr >> 6) & 1) != (unsigned)alignment_write) _exit(93);
    alignment_seen++;
    saved->uc_mcontext.pc += 4;
}

static int verify_alignment_fault(void)
{
    for (int write = 0; write < 2; write++) {
        pid_t child = fork();
        if (child == 0) {
            uint64_t bytes[4] __attribute__((aligned(16))) = { 0 };
            /* 跨过 16-byte atomicity granule；LSE2 可能允许粒度内的未对齐 acquire/release。 */
            alignment_address = (uintptr_t)bytes + 13;
            alignment_write = write;
            struct sigaction action = { .sa_sigaction = catch_alignment, .sa_flags = SA_SIGINFO };
            if (sigaction(SIGBUS, &action, 0) || sigaction(SIGSEGV, &action, 0)) _exit(94);
            uint64_t value = 42;
            /* 1. 同一条指令在对齐地址正常执行；2. 未对齐地址必须获得标准 SIGBUS/ESR；3. handler 可恢复。 */
            if (write) {
                __asm__ volatile("stlr %0, [%1]" :: "r"(value), "r"(bytes) : "memory");
                __asm__ volatile("stlr %0, [%1]" :: "r"(value), "r"(alignment_address) : "memory");
            } else {
                __asm__ volatile("ldar %0, [%1]" : "=r"(value) : "r"(bytes) : "memory");
                __asm__ volatile("ldar %0, [%1]" : "=r"(value) : "r"(alignment_address) : "memory");
            }
            _exit(alignment_seen == 1 ? 0 : 95);
        }
        int status;
        if (child < 0 || waitpid(child, &status, 0) != child) return 1;
        if (!WIFEXITED(status) || WEXITSTATUS(status)) {
            dprintf(1, "signal-alignment: write=%d status=%#x\n", write, status);
            return 1;
        }
    }
    dprintf(1, "signal-alignment: read/write SIGBUS, ESR and sigreturn passed\n");
    return 0;
}
#endif

int verify_signal_frame_errors(void)
{
#if defined(__aarch64__)
    if (verify_alignment_fault()) return 9;
#endif
    int realtime = verify_realtime_queue();
    if (realtime) { dprintf(1, "signal-realtime: step=%d\n", realtime); return 6; }
    int ordinary = verify_ordinary_signal_accounting();
    if (ordinary) { dprintf(1, "signal-ordinary: step=%d\n", ordinary); return 8; }
    int timers = verify_posix_timer_notifications();
    if (timers) { dprintf(1, "signal-posix-timer: step=%d\n", timers); return 7; }
	if (verify_action_layout()) return 4;
	const int cases =
#if defined(__aarch64__)
		4;
#else
		3;
#endif
	for (int word = -2; word < cases; word++) {
		for (int caught = 0; caught < 2; caught++) {
			dprintf(1, "signal-frame: word=%d caught=%d\n", word, caught);
			pid_t child = fork();
			if (child == 0) _exit(scenario(word, caught, 0));
			int status;
			if (child < 0 || waitpid(child, &status, 0) != child) return 1;
			dprintf(1, "signal-frame: status=%#x\n", status);
			if (caught) {
				if (!WIFEXITED(status) || WEXITSTATUS(status)) return 2;
			} else if (!WIFSIGNALED(status) || WTERMSIG(status) != SIGSEGV) return 3;
		}
	}
	/* 可读坏 frame 的 saved mask 在 extension validation 前生效；force_sig 必须
	 * 把 blocked SIGSEGV 的已安装 handler 恢复默认，不能误入 caught 分支。 */
	pid_t child = fork();
	if (child == 0) _exit(scenario(0, 1, 1));
	int status;
	if (child < 0 || waitpid(child, &status, 0) != child
		|| !WIFSIGNALED(status) || WTERMSIG(status) != SIGSEGV) return 5;
	return 0;
}
