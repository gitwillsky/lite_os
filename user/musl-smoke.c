#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static void *thread_main(void *argument)
{
	if (!pthread_self()) return 0;
	return argument;
}

int main(int argc, char **argv, char **envp)
{
	static const char create_failed[] = "LiteOS musl pthread create failed\n";
	static const char join_failed[] = "LiteOS musl pthread join failed\n";
	static const char message[] = "LiteOS musl pthread ok\n";
	struct timespec now;
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
	if (pthread_join(thread, &thread_result) != 0
	    || thread_result != (void *)(uintptr_t)0x4f53) {
		write(STDOUT_FILENO, join_failed, sizeof join_failed - 1);
		return 6;
	}
	if (write(STDOUT_FILENO, message, sizeof message - 1) != sizeof message - 1) return 7;
	return 0;
}
