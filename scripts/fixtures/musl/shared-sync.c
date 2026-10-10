#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <semaphore.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stddef.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

struct shared_sync {
	pthread_mutex_t mutex;
	pthread_cond_t condition;
	sem_t completion;
	_Atomic int ready;
	int value;
	int source;
	int target;
	struct timespec restart_timeout;
};

static int wait_child_ok(pid_t child)
{
	int status;
	return child > 0 && waitpid(child, &status, 0) == child
		&& WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

static int verify_robust_query(void)
{
    struct robust_list_head *original = NULL, *observed = NULL;
    size_t length = 0;
    if (syscall(SYS_get_robust_list, 0, &original, &length)
        || length != sizeof(struct robust_list_head)) return 1;
    if (syscall(SYS_get_robust_list, syscall(SYS_gettid), &observed, &length)
        || observed != original) return 2;
    /* native setter/getter 只登记地址，不访问所登记的用户 memory。 */
    if (syscall(SYS_set_robust_list, (void *)(uintptr_t)1, sizeof(struct robust_list_head))) return 3;
    int result = 0;
    if (syscall(SYS_get_robust_list, 0, &observed, &length) || (uintptr_t)observed != 1) result = 4;
    length = 123;
    if (!result && (syscall(SYS_get_robust_list, 0, NULL, &length) != -1
        || errno != EFAULT || length != sizeof(struct robust_list_head))) result = 5;
    observed = original;
    if (!result && (syscall(SYS_get_robust_list, 0, &observed, NULL) != -1
        || errno != EFAULT || observed != original)) result = 6;
    if (!result && (syscall(SYS_get_robust_list, -1, NULL, NULL) != -1 || errno != ESRCH)) result = 7;
    if (syscall(SYS_set_robust_list, NULL, sizeof(struct robust_list_head))) result = 8;
    if (!result && (syscall(SYS_get_robust_list, 0, &observed, &length) || observed)) result = 9;
    if (syscall(SYS_set_robust_list, original, sizeof(struct robust_list_head))) result = 10;
    return result;
}

static int verify_pshared_primitives(struct shared_sync *shared)
{
	pthread_mutexattr_t mutex_attribute;
	pthread_condattr_t condition_attribute;
	pid_t child;

	/* 1. All synchronization objects live in one anonymous shared backing. */
	if (pthread_mutexattr_init(&mutex_attribute) != 0
	    || pthread_mutexattr_setpshared(&mutex_attribute, PTHREAD_PROCESS_SHARED) != 0
	    || pthread_condattr_init(&condition_attribute) != 0
	    || pthread_condattr_setpshared(&condition_attribute, PTHREAD_PROCESS_SHARED) != 0
	    || pthread_mutex_init(&shared->mutex, &mutex_attribute) != 0
	    || pthread_cond_init(&shared->condition, &condition_attribute) != 0
	    || sem_init(&shared->completion, 1, 0) != 0) return 1;
	pthread_mutexattr_destroy(&mutex_attribute);
	pthread_condattr_destroy(&condition_attribute);

	/* 2. The child blocks through musl's shared futex path and publishes through the same pages. */
	child = fork();
	if (child == 0) {
		if (pthread_mutex_lock(&shared->mutex) != 0) _exit(10);
		atomic_store_explicit(&shared->ready, 1, memory_order_release);
		if (pthread_cond_broadcast(&shared->condition) != 0) _exit(11);
		while (shared->value == 0) {
			if (pthread_cond_wait(&shared->condition, &shared->mutex) != 0) _exit(12);
		}
		shared->value++;
		if (pthread_mutex_unlock(&shared->mutex) != 0
		    || sem_post(&shared->completion) != 0) _exit(13);
		_exit(0);
	}
	if (child <= 0 || pthread_mutex_lock(&shared->mutex) != 0) return 2;
	while (!atomic_load_explicit(&shared->ready, memory_order_acquire)) {
		if (pthread_cond_wait(&shared->condition, &shared->mutex) != 0) return 3;
	}
	shared->value = 41;
	if (pthread_cond_signal(&shared->condition) != 0
	    || pthread_mutex_unlock(&shared->mutex) != 0
	    || sem_wait(&shared->completion) != 0
	    || !wait_child_ok(child) || shared->value != 42) return 4;

	/* 3. Destroy after the final cross-process user, proving normal owner lifetime completion. */
	if (sem_destroy(&shared->completion) != 0
	    || pthread_cond_destroy(&shared->condition) != 0
	    || pthread_mutex_destroy(&shared->mutex) != 0) return 5;
	return 0;
}

static int verify_bitset_requeue(struct shared_sync *shared)
{
	struct timespec deadline;
	int ready_pipe[2];
	int affected = 0;
	int attempts;
	char marker;
	pid_t child;

	shared->source = 0;
	shared->target = 0;
	errno = 0;
	if (syscall(SYS_futex, &shared->source, FUTEX_WAIT_BITSET, 0,
	            &(struct timespec){ 0 }, 0, 0) != -1 || errno != EINVAL) return 10;
	errno = 0;
	if (syscall(SYS_futex, &shared->source, FUTEX_WAIT_BITSET, 0,
	            &(struct timespec){ 0 }, 0, FUTEX_BITSET_MATCH_ANY) != -1
	    || errno != ETIMEDOUT) return 11;
	shared->source = 1;
	errno = 0;
	if (syscall(SYS_futex, &shared->source, FUTEX_CMP_REQUEUE, 0, 1,
	            &shared->target, 0) != -1 || errno != EAGAIN) return 12;
	shared->source = 0;
	if (syscall(SYS_futex, &shared->source, FUTEX_REQUEUE, 0, 1,
	            &shared->target, 0) != 0 || pipe(ready_pipe) != 0) return 13;

	/* 1. Child publishes readiness immediately before entering an absolute realtime wait. */
	child = fork();
	if (child == 0) {
		close(ready_pipe[0]);
		if (clock_gettime(CLOCK_REALTIME, &deadline) != 0) _exit(20);
		deadline.tv_sec += 5;
		if (write(ready_pipe[1], "W", 1) != 1) _exit(21);
		close(ready_pipe[1]);
		if (syscall(SYS_futex, &shared->source,
		            FUTEX_WAIT_BITSET | FUTEX_CLOCK_REALTIME, 0,
		            &deadline, 0, UINT32_C(0x2)) != 0) _exit(22);
		_exit(0);
	}
	close(ready_pipe[1]);
	if (child <= 0 || read(ready_pipe[0], &marker, 1) != 1 || marker != 'W'
	    || close(ready_pipe[0]) != 0) return 14;

	/* 2. Requeue's result is the waiter-publication handshake; no wall-clock settling is assumed. */
	for (attempts = 0; attempts < 1000; attempts++) {
		affected = syscall(SYS_futex, &shared->source, FUTEX_CMP_REQUEUE, 0, 1,
		                   &shared->target, 0);
		if (affected == 1) break;
		if (affected != 0 || sched_yield() != 0) return 15;
	}
	if (affected != 1) return 15;

	/* 3. Requeue preserves bitset/deadline: a disjoint target wake misses, the matching wake wins. */
	if (syscall(SYS_futex, &shared->target, FUTEX_WAKE_BITSET, 1, 0, 0,
	            UINT32_C(0x1)) != 0
	    || syscall(SYS_futex, &shared->target, FUTEX_WAKE_BITSET, 1, 0, 0,
	               UINT32_C(0x2)) != 1
	    || !wait_child_ok(child)) return 16;
	return 0;
}

static int verify_wake_op_words(struct shared_sync *shared)
{
	static const struct { int op, initial, operand, expected; } cases[] = {
		{ FUTEX_OP_SET, 7, -2, -2 }, { FUTEX_OP_ADD, -1, 2, 1 },
		{ FUTEX_OP_OR, 0x12, 0x20, 0x32 }, { FUTEX_OP_ANDN, 0xff, 0xf, 0xf0 },
		{ FUTEX_OP_XOR, 0x12, 0x23, 0x31 },
	};
	for (unsigned int i = 0; i < sizeof cases / sizeof cases[0]; ++i) {
		shared->target = cases[i].initial;
		if (syscall(SYS_futex, &shared->source, FUTEX_WAKE_OP, 0, 0, &shared->target,
		            FUTEX_OP(cases[i].op, cases[i].operand, FUTEX_OP_CMP_EQ, cases[i].initial)) != 0
		    || shared->target != cases[i].expected) return 1;
	}
	shared->target = 0;
	if (syscall(SYS_futex, &shared->source, FUTEX_WAKE_OP, 0, 0, &shared->target,
	            FUTEX_OP(FUTEX_OP_SET | FUTEX_OP_OPARG_SHIFT, 32, FUTEX_OP_CMP_EQ, 0)) != 0
	    || shared->target != 1) return 2;
	if (syscall(SYS_futex, &shared->source, FUTEX_WAKE_OP, 0, 0, &shared->target,
	            FUTEX_OP(FUTEX_OP_ADD, 2, 15, 0)) != -1 || errno != ENOSYS || shared->target != 3) return 3;
	if (syscall(SYS_futex, &shared->source, FUTEX_WAKE_OP, 0, 0, &shared->target,
	            FUTEX_OP(7, 2, FUTEX_OP_CMP_EQ, 0)) != -1 || errno != ENOSYS || shared->target != 3) return 4;
	/* private key 只需 access_ok；普通 REQUEUE 无比较时也不得读取 source word。 */
	if (syscall(SYS_futex, NULL, FUTEX_WAKE_PRIVATE, 0, 0, 0, 0) != 0
	    || syscall(SYS_futex, NULL, FUTEX_REQUEUE_PRIVATE, 0, 0, NULL, 0) != 0
	    || syscall(SYS_futex, NULL, FUTEX_CMP_REQUEUE_PRIVATE, 0, 0, NULL, 0) != -1 || errno != EFAULT) return 5;
	if (syscall(SYS_futex, NULL, FUTEX_REQUEUE_PRIVATE, -1, 0, NULL, 0) != -1 || errno != EINVAL
	    || syscall(SYS_futex, NULL, FUTEX_REQUEUE_PRIVATE, 0, -1, NULL, 0) != -1 || errno != EINVAL) return 6;
	if (syscall(SYS_futex, (void *)(uintptr_t)1, 127, 0, 0, 0, 0) != -1 || errno != ENOSYS
	    || syscall(SYS_futex, &shared->source, FUTEX_WAIT_BITSET, 0, (void *)(uintptr_t)1, 0, 0) != -1 || errno != EFAULT) return 7;
	if (syscall(SYS_futex, NULL, FUTEX_REQUEUE, 0, 0, (void *)(uintptr_t)1, 0) != -1 || errno != EFAULT
	    || syscall(SYS_futex, NULL, FUTEX_WAKE_OP, 0, 0, (void *)(uintptr_t)1, 0) != -1 || errno != EFAULT) return 10;
	void *page = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (page == MAP_FAILED) return 8;
	*(int *)page = 31;
	if (mprotect(page, 4096, PROT_READ)
	    || syscall(SYS_futex, &shared->source, FUTEX_WAKE_OP | FUTEX_PRIVATE_FLAG, 0, 0, page,
	               FUTEX_OP(FUTEX_OP_ADD, 1, FUTEX_OP_CMP_EQ, 31)) != -1 || errno != EFAULT
	    || *(int *)page != 31 || munmap(page, 4096)) return 9;
	return 0;
}

static pid_t wake_op_waiter(int *word)
{
	pid_t child = fork();
	if (child == 0) {
		struct timespec timeout = { .tv_sec = 5 };
		_exit(syscall(SYS_futex, word, FUTEX_WAIT, 0, &timeout, 0, 0) != 0);
	}
	return child;
}
static int observe_waiters(int *word, int count)
{
	for (int attempt = 0; attempt < 1000; ++attempt) {
		int queued = syscall(SYS_futex, word, FUTEX_REQUEUE, 0, count, word, 0);
		if (queued == count) return 0;
		if (queued < 0 || sched_yield()) return -1;
	}
	return -1;
}
static int verify_wake_op_queues(struct shared_sync *shared, int same_key, int comparison_true)
{
	shared->source = shared->target = 0;
	int *target = same_key ? &shared->source : &shared->target;
	pid_t first = wake_op_waiter(&shared->source);
	pid_t second = wake_op_waiter(target);
	int failed = first <= 0 || second <= 0 || observe_waiters(&shared->source, same_key ? 2 : 1)
	    || (!same_key && observe_waiters(target, 1));
	if (!failed) {
		int count = syscall(SYS_futex, &shared->source, FUTEX_WAKE_OP, 1, 1, target,
		                    FUTEX_OP(FUTEX_OP_SET, 1, FUTEX_OP_CMP_EQ, comparison_true ? 0 : 7));
		failed = count != (comparison_true ? 2 : 1) || *target != 1;
		/* false comparison 留下第二个实际 waiter；legacy WAKE 的零 count 仍唤醒一个。 */
		if (!comparison_true && !failed)
			failed = observe_waiters(target, 1) || syscall(SYS_futex, target, FUTEX_WAKE, 0, 0, 0, 0) != 1;
	}
	if (failed) { if (first > 0) kill(first, SIGKILL); if (second > 0) kill(second, SIGKILL); }
	int first_ok = wait_child_ok(first), second_ok = wait_child_ok(second);
	return failed || !first_ok || !second_ok;
}
static int verify_wake_op_shared_atomic(struct shared_sync *shared)
{
	shared->target = 0;
	pid_t children[2];
	for (int child = 0; child < 2; ++child) {
		children[child] = fork();
		if (children[child] == 0) {
			for (int update = 0; update < 1000; ++update)
				if (syscall(SYS_futex, &shared->source, FUTEX_WAKE_OP, 0, 0, &shared->target,
				            FUTEX_OP(FUTEX_OP_ADD, 1, FUTEX_OP_CMP_EQ, 0)) != 0) _exit(1);
			_exit(0);
		}
	}
	int first = wait_child_ok(children[0]), second = wait_child_ok(children[1]);
	return !first || !second || shared->target != 2000;
}

struct robust_pair {
    struct robust_list link;
    uint32_t word;
    uint32_t padding;
};
struct robust_chain_probe {
    struct robust_list_head head;
    struct robust_pair pairs[2];
};
static int verify_robust_tagged_chain(void)
{
    struct robust_chain_probe *probe = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
        MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (probe == MAP_FAILED) return 1;
    int publication[2], release[2], completion[2];
    if (pipe(publication) || pipe(release) || pipe(completion)) return 2;
    pid_t holder = fork();
    if (holder == 0) {
        close(publication[0]); close(release[1]);
        probe->head.list.next = (struct robust_list *)((uintptr_t)&probe->pairs[0].link | 1);
        probe->head.futex_offset = offsetof(struct robust_pair, word);
        probe->head.list_op_pending = NULL;
        probe->pairs[0].link.next = &probe->pairs[1].link;
        probe->pairs[1].link.next = &probe->head.list;
        probe->pairs[0].word = probe->pairs[1].word = (uint32_t)syscall(SYS_gettid) | FUTEX_WAITERS;
        if (syscall(SYS_set_robust_list, &probe->head, sizeof probe->head)
            || write(publication[1], "R", 1) != 1) _exit(1);
        char byte;
        if (read(release[0], &byte, 1) != 1) _exit(2);
        _exit(0);
    }
    close(publication[1]); close(release[0]);
    char byte;
    int failed = holder <= 0 || read(publication[0], &byte, 1) != 1;
    pid_t waiter = failed ? -1 : fork();
    if (waiter == 0) {
        close(completion[0]);
        struct timespec timeout = { .tv_sec = 5 };
        if (syscall(SYS_futex, &probe->pairs[1].word, FUTEX_WAIT,
                    probe->pairs[1].word, &timeout, 0, 0) != 0
            || write(completion[1], "D", 1) != 1) _exit(1);
        _exit(0);
    }
    close(completion[1]);
    if (!failed) failed = waiter <= 0 || observe_waiters((int *)&probe->pairs[1].word, 1)
        || write(release[1], "X", 1) != 1;
    struct pollfd ready = { .fd = completion[0], .events = POLLIN };
    if (!failed) failed = poll(&ready, 1, 2000) <= 0 || read(completion[0], &byte, 1) != 1 || byte != 'D';
    if (failed) { if (holder > 0) kill(holder, SIGKILL); if (waiter > 0) kill(waiter, SIGKILL); }
    int holder_ok = wait_child_ok(holder), waiter_ok = wait_child_ok(waiter);
    if (!failed) failed = probe->pairs[0].word != (FUTEX_WAITERS | FUTEX_OWNER_DIED)
        || probe->pairs[1].word != (FUTEX_WAITERS | FUTEX_OWNER_DIED);
    close(publication[0]); close(release[1]); close(completion[0]);
    munmap(probe, 4096);
    return failed || !holder_ok || !waiter_ok;
}

static volatile sig_atomic_t timed_signal_seen;
static void timed_signal_handler(int signal_number)
{
    (void)signal_number;
    timed_signal_seen++;
}
static int verify_timed_futex_handler(struct shared_sync *shared, int absolute)
{
    shared->source = 0;
    pid_t child = fork();
    if (child == 0) {
        struct sigaction action = { .sa_handler = timed_signal_handler, .sa_flags = SA_RESTART };
        if (sigaction(SIGUSR1, &action, NULL)) _exit(1);
        struct timespec timeout = { .tv_sec = 5 };
        if (absolute) {
            if (clock_gettime(CLOCK_MONOTONIC, &timeout)) _exit(2);
            timeout.tv_sec += 5;
        }
        int operation = absolute ? FUTEX_WAIT_BITSET : FUTEX_WAIT;
        if (syscall(SYS_futex, &shared->source, operation, 0, &timeout, 0,
                    absolute ? FUTEX_BITSET_MATCH_ANY : 0) != -1
            || errno != EINTR || timed_signal_seen != 1) _exit(3);
        if (syscall(SYS_restart_syscall) != -1 || errno != EINTR) _exit(4);
        _exit(0);
    }
    int failed = child <= 0 || observe_waiters(&shared->source, 1) || kill(child, SIGUSR1);
    if (failed && child > 0) kill(child, SIGKILL);
    return !wait_child_ok(child) || failed;
}
/* Stop/continue 使用实际 wait queue 与 WUNTRACED 握手；不是按猜测延迟触发 signal。 */
static int verify_timed_futex_stop(struct shared_sync *shared, int absolute, int caught_after_stop)
{
    shared->source = 0;
    int completion[2];
    if (pipe(completion)) return 1;
    struct timespec expiration;
    if (clock_gettime(CLOCK_MONOTONIC, &expiration)) return 2;
    expiration.tv_nsec += 500000000;
    if (expiration.tv_nsec >= 1000000000) { expiration.tv_nsec -= 1000000000; expiration.tv_sec++; }
    shared->restart_timeout = absolute ? expiration : (struct timespec){ .tv_nsec = 500000000 };
    pid_t child = fork();
    if (child == 0) {
        close(completion[0]);
        struct sigaction action = { .sa_handler = timed_signal_handler, .sa_flags = SA_RESTART };
        if (sigaction(SIGUSR1, &action, NULL)) _exit(1);
        int operation = absolute ? FUTEX_WAIT_BITSET : FUTEX_WAIT;
        int result = syscall(SYS_futex, &shared->source, operation, 0, &shared->restart_timeout, 0,
                             absolute ? FUTEX_BITSET_MATCH_ANY : 0);
        if (result != -1 || errno != (caught_after_stop ? EINTR : ETIMEDOUT)
            || timed_signal_seen != caught_after_stop) _exit(2);
        if (syscall(SYS_restart_syscall) != -1 || errno != EINTR) _exit(3);
        if (write(completion[1], "D", 1) != 1) _exit(4);
        _exit(0);
    }
    close(completion[1]);
    int status, failed = child <= 0 || observe_waiters(&shared->source, 1)
        || kill(child, SIGSTOP) || waitpid(child, &status, WUNTRACED) != child
        || !WIFSTOPPED(status);
    if (!failed) {
        /* 修改的是已导入的用户 timespec；restart block 不得再读取它。 */
        shared->restart_timeout.tv_sec += 1000;
        if (caught_after_stop) failed = kill(child, SIGUSR1);
        else {
            expiration.tv_nsec += 100000000;
            if (expiration.tv_nsec >= 1000000000) { expiration.tv_nsec -= 1000000000; expiration.tv_sec++; }
            failed = clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &expiration, NULL) != 0;
        }
        if (!failed) failed = kill(child, SIGCONT);
    }
    struct pollfd ready = { .fd = completion[0], .events = POLLIN };
    char byte;
    if (!failed) failed = poll(&ready, 1, 2000) <= 0 || read(completion[0], &byte, 1) != 1 || byte != 'D';
    if (failed && child > 0) kill(child, SIGKILL);
    int child_ok = wait_child_ok(child);
    close(completion[0]);
    return failed || !child_ok;
}

int verify_shared_sync(void)
{
	struct shared_sync *shared = mmap(0, 4096, PROT_READ | PROT_WRITE,
	                                  MAP_SHARED | MAP_ANONYMOUS, -1, 0);
	int result;
	if (shared == MAP_FAILED) return errno;
	result = verify_pshared_primitives(shared);
	if (result) result += 10;
	else {
		result = verify_bitset_requeue(shared);
		if (result) result += 40;
	}
	if (!result) result = verify_wake_op_words(shared);
	if (!result && (verify_wake_op_queues(shared, 0, 1) || verify_wake_op_queues(shared, 0, 0)
	                || verify_wake_op_queues(shared, 1, 1))) result = 95;
	if (!result && verify_wake_op_shared_atomic(shared)) result = 96;
	if (!result && verify_robust_tagged_chain()) result = 90;
	if (!result && verify_robust_query()) result = 91;
	if (!result && (verify_timed_futex_handler(shared, 0) || verify_timed_futex_handler(shared, 1))) result = 97;
	if (!result && (verify_timed_futex_stop(shared, 0, 0) || verify_timed_futex_stop(shared, 1, 0)
	                || verify_timed_futex_stop(shared, 0, 1))) result = 98;
	if (munmap(shared, 4096) != 0 && !result) result = 99;
	return result;
}
