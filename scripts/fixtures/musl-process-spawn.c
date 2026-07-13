#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <spawn.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/membarrier.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

struct wait_request {
	pid_t child;
	pid_t result;
	int status;
	int error;
};

static _Atomic int worker_running;
static _Atomic int waiter_ready;

static void *background_worker(void *argument)
{
	atomic_store_explicit(&worker_running, 1, memory_order_release);
	while (atomic_load_explicit(&worker_running, memory_order_acquire) == 1) sched_yield();
	return argument;
}

static void *wait_for_child(void *argument)
{
	struct wait_request *request = argument;
	atomic_fetch_add_explicit(&waiter_ready, 1, memory_order_release);
	errno = 0;
	request->result = waitpid(request->child, &request->status, 0);
	request->error = errno;
	return argument;
}

static int exited_with(int status, int code)
{
	return WIFEXITED(status) && WEXITSTATUS(status) == code;
}

static int spawn_blocked_child(pid_t *child, int gate[2], int exit_code)
{
	posix_spawn_file_actions_t actions;
	char command[64];
	char *arguments[] = { "sh", "-c", command, NULL };
	int error;

	if (pipe2(gate, O_CLOEXEC) != 0
	    || snprintf(command, sizeof command, "read value <&20; exit %d", exit_code) <= 0
	    || posix_spawn_file_actions_init(&actions) != 0) return 1;
	/* 1. The child receives one non-CLOEXEC gate descriptor and no writable gate endpoint. */
	if (posix_spawn_file_actions_adddup2(&actions, gate[0], 20) != 0
	    || (gate[0] != 20 && posix_spawn_file_actions_addclose(&actions, gate[0]) != 0)
	    || posix_spawn_file_actions_addclose(&actions, gate[1]) != 0) return 2;
	/* 2. musl performs standard CLONE_VM|CLONE_VFORK, file actions, then execve /bin/sh. */
	error = posix_spawn(child, "/bin/sh", &actions, NULL, arguments, environ);
	posix_spawn_file_actions_destroy(&actions);
	if (error != 0 || close(gate[0]) != 0) return 3;
	return 0;
}

static int verify_spawn_file_actions(void)
{
	static const char path[] = "/run/phase59-spawn.out";
	posix_spawn_file_actions_t actions;
	char *arguments[] = { "printf", "spawnp-59", NULL };
	char output[16] = { 0 };
	pid_t child;
	int descriptor;
	int status;

	unlink(path);
	if (setenv("PATH", "/bin", 1) != 0 || posix_spawn_file_actions_init(&actions) != 0
	    || posix_spawn_file_actions_addopen(
	        &actions, STDOUT_FILENO, path, O_CREAT | O_TRUNC | O_WRONLY, 0644) != 0) return 1;
	if (posix_spawnp(&child, "printf", &actions, NULL, arguments, environ) != 0) return 2;
	posix_spawn_file_actions_destroy(&actions);
	if (waitpid(child, &status, 0) != child || !exited_with(status, 0)) return 3;
	descriptor = open(path, O_RDONLY);
	if (descriptor < 0 || read(descriptor, output, sizeof output) != 9
	    || memcmp(output, "spawnp-59", 9) != 0 || close(descriptor) != 0
	    || unlink(path) != 0) return 4;
	return 0;
}

static int verify_concurrent_waiters(void)
{
	struct wait_request requests[3];
	pthread_t waiters[3];
	pid_t first;
	pid_t second;
	int first_gate[2];
	int second_gate[2];
	int first_winners;
	int result;

	if (spawn_blocked_child(&first, first_gate, 23) != 0
	    || spawn_blocked_child(&second, second_gate, 24) != 0) return 1;
	requests[0] = (struct wait_request){ .child = first };
	requests[1] = (struct wait_request){ .child = first };
	requests[2] = (struct wait_request){ .child = second };
	atomic_store_explicit(&waiter_ready, 0, memory_order_release);
	/* 1. Two waiters contend for one child while a third waits for a different child. */
	for (int index = 0; index < 3; index++) {
		if (pthread_create(&waiters[index], NULL, wait_for_child, &requests[index]) != 0) return 2;
	}
	while (atomic_load_explicit(&waiter_ready, memory_order_acquire) != 3) sched_yield();
	/* 2. No timing assumption is needed: children cannot exit until both pipe gates are released. */
	if (write(first_gate[1], "x\n", 2) != 2 || close(first_gate[1]) != 0
	    || write(second_gate[1], "y\n", 2) != 2 || close(second_gate[1]) != 0) return 3;
	for (int index = 0; index < 3; index++) {
		void *joined = NULL;
		if (pthread_join(waiters[index], &joined) != 0 || joined != &requests[index]) return 4;
	}
	/* 3. The graph claim permits exactly one reap of `first`; the loser observes ECHILD. */
	first_winners = 0;
	for (int index = 0; index < 2; index++) {
		if (requests[index].result == first && exited_with(requests[index].status, 23)) first_winners++;
		else if (requests[index].result != -1 || requests[index].error != ECHILD) return 5;
	}
	result = requests[2].result == second && exited_with(requests[2].status, 24);
	return first_winners == 1 && result ? 0 : 6;
}

int verify_process_spawn(void)
{
	pthread_t worker;
	void *worker_result = NULL;
	FILE *stream;
	char output[16] = { 0 };
	char *missing_arguments[] = { "missing", NULL };
	pid_t child = -1;
	int status;
	int result;
	long barrier_commands;

	/* 1. The fresh exec mm is unregistered and advertises only the implemented command pair. */
	barrier_commands = syscall(SYS_membarrier, MEMBARRIER_CMD_QUERY, 0, 0);
	errno = 0;
	if ((barrier_commands & (MEMBARRIER_CMD_PRIVATE_EXPEDITED
	                       | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED))
	        != (MEMBARRIER_CMD_PRIVATE_EXPEDITED
	            | MEMBARRIER_CMD_REGISTER_PRIVATE_EXPEDITED)
	    || syscall(SYS_membarrier, MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, 0) != -1
	    || errno != EPERM) return 9;

	atomic_store_explicit(&worker_running, 0, memory_order_release);
	if (pthread_create(&worker, NULL, background_worker, (void *)(uintptr_t)59) != 0) return 10;
	while (atomic_load_explicit(&worker_running, memory_order_acquire) != 1) sched_yield();
	if (syscall(SYS_membarrier, MEMBARRIER_CMD_PRIVATE_EXPEDITED, 0, 0) != 0) return 10;

	/* 2. system and popen both traverse musl posix_spawn while a sibling Thread remains runnable. */
	status = system("test x$(printf phase59) = xphase59");
	if (!exited_with(status, 0)) return 11;
	stream = popen("printf popen-59", "r");
	if (stream == NULL || fread(output, 1, sizeof output, stream) != 8
	    || memcmp(output, "popen-59", 8) != 0 || !exited_with(pclose(stream), 0)) return 12;

	/* 3. PATH search/file actions succeed; exec failure returns errno through musl's CLOEXEC pipe. */
	result = verify_spawn_file_actions();
	if (result != 0) return 20 + result;
	result = posix_spawn(&child, "/missing/phase59", NULL, NULL, missing_arguments, environ);
	if (result != ENOENT || child != -1) return 30;

	/* 4. Concurrent parent Threads wait through the graph's unique child-event claim. */
	result = verify_concurrent_waiters();
	if (result != 0) return 40 + result;
	atomic_store_explicit(&worker_running, 2, memory_order_release);
	if (pthread_join(worker, &worker_result) != 0
	    || worker_result != (void *)(uintptr_t)59) return 50;
	puts("LITEOS_POSIX_SPAWN_59");
	return 0;
}
