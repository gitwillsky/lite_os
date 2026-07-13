#define _GNU_SOURCE
#include <errno.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <semaphore.h>
#include <stdatomic.h>
#include <stdint.h>
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
};

static int wait_child_ok(pid_t child)
{
	int status;
	return child > 0 && waitpid(child, &status, 0) == child
		&& WIFEXITED(status) && WEXITSTATUS(status) == 0;
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
	if (munmap(shared, 4096) != 0 && !result) result = 99;
	return result;
}
