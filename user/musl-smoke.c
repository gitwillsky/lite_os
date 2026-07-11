#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t condition = PTHREAD_COND_INITIALIZER;
static int state;

static void *thread_main(void *argument)
{
	if (!pthread_self()) return 0;
	if (pthread_mutex_lock(&mutex) != 0) return 0;
	state = 1;
	if (pthread_cond_signal(&condition) != 0) return 0;
	while (state == 1) {
		if (pthread_cond_wait(&condition, &mutex) != 0) return 0;
	}
	if (pthread_mutex_unlock(&mutex) != 0) return 0;
	return argument;
}

int main(int argc, char **argv, char **envp)
{
	static const char create_failed[] = "LiteOS musl pthread create failed\n";
	static const char join_failed[] = "LiteOS musl pthread join failed\n";
	static const char sync_failed[] = "LiteOS musl pthread sync failed\n";
	static const char message[] = "LiteOS musl pthread sync ok\n";
	struct timespec deadline;
	struct timespec now;
	int wait_result;
	pthread_t thread;
	void *thread_result;
	void *allocation;

	if (argc != 1 || !argv || !argv[0] || !envp || envp[0]) return 1;
	if (sysconf(_SC_PAGESIZE) != 4096 || getpid() <= 0) return 2;
	allocation = malloc(64);
	if (!allocation) return 3;
	*(volatile uint64_t *)allocation = UINT64_C(0x4c6974654f53);
	free(allocation);
	if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return 4;
	if (pthread_create(&thread, 0, thread_main, (void *)(uintptr_t)0x4f53) != 0) {
		write(STDOUT_FILENO, create_failed, sizeof create_failed - 1);
		return 5;
	}
	if (pthread_mutex_lock(&mutex) != 0) return 6;
	while (state == 0) {
		if (pthread_cond_wait(&condition, &mutex) != 0) return 6;
	}
	state = 2;
	if (pthread_cond_signal(&condition) != 0 || pthread_mutex_unlock(&mutex) != 0) return 6;
	if (pthread_join(thread, &thread_result) != 0
	    || thread_result != (void *)(uintptr_t)0x4f53) {
		write(STDOUT_FILENO, join_failed, sizeof join_failed - 1);
		return 7;
	}
	if (pthread_mutex_lock(&mutex) != 0 || clock_gettime(CLOCK_REALTIME, &deadline) != 0) return 8;
	deadline.tv_nsec += 20 * 1000 * 1000;
	if (deadline.tv_nsec >= 1000 * 1000 * 1000) {
		deadline.tv_sec++;
		deadline.tv_nsec -= 1000 * 1000 * 1000;
	}
	do wait_result = pthread_cond_timedwait(&condition, &mutex, &deadline);
	while (wait_result == 0);
	if (wait_result != ETIMEDOUT || pthread_mutex_unlock(&mutex) != 0) {
		write(STDOUT_FILENO, sync_failed, sizeof sync_failed - 1);
		return 8;
	}
	if (write(STDOUT_FILENO, message, sizeof message - 1) != sizeof message - 1) return 9;
	return 0;
}
