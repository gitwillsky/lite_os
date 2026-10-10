#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdatomic.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* 旧 worker 的 clear-child-tid 与新 ET_EXEC image 使用同一 VA，内容只能在旧 mm 清零。 */
static volatile uint32_t exit_image_magic = 0x5a713e29;
static atomic_int worker_armed;

static void *exit_worker(void *unused)
{
	(void)unused;
	if (syscall(SYS_set_tid_address, &exit_image_magic) < 0) _exit(93);
	atomic_store_explicit(&worker_armed, 1, memory_order_release);
	/* 不 join：主线程必须与 kernel teardown 的 graph removal/clear-tid 窗口竞争。 */
	syscall(SYS_exit, 0);
	_exit(94);
}

int verify_thread_exit_image(void)
{
	for (int i = 0; i < 64; ++i) sched_yield();
	return exit_image_magic == 0x5a713e29 ? 0 : 95;
}

int verify_thread_exit_exec(void)
{
	for (int iteration = 0; iteration < 8; ++iteration) {
		pid_t child = fork();
		if (child < 0) return 1;
		if (child == 0) {
			pthread_t worker;
			atomic_store_explicit(&worker_armed, 0, memory_order_relaxed);
			if (pthread_create(&worker, 0, exit_worker, 0) != 0) _exit(96);
			while (!atomic_load_explicit(&worker_armed, memory_order_acquire)) sched_yield();
			execl("/bin/init", "musl-smoke", "thread-exit-image", (char *)0);
			_exit(97);
		}
		int status;
		pid_t waited;
		do { waited = waitpid(child, &status, 0); } while (waited < 0 && errno == EINTR);
		if (waited != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 2;
	}
	return 0;
}

/* live sibling 覆盖 blocking wait 与纯 userspace 运行；nonleader 必须保留原 TGID。 */
static atomic_int live_armed;
static atomic_int execute_worker;
static int blocking_read_fd;
static char original_pid[32];
static void caught_before_exec(int signal) { (void)signal; }
static void execute_live_image(void)
{
    execl("/bin/init", "musl-smoke", "thread-live-image", original_pid, (char *)0);
    _exit(101);
}
static void *blocked_sibling(void *unused)
{
    (void)unused;
    if (syscall(SYS_set_tid_address, &exit_image_magic) < 0) _exit(102);
    atomic_fetch_add_explicit(&live_armed, 1, memory_order_release);
    char byte;
    (void)read(blocking_read_fd, &byte, 1);
    _exit(103);
}
static void *running_sibling(void *unused)
{
    (void)unused;
    atomic_fetch_add_explicit(&live_armed, 1, memory_order_release);
    for (;;) atomic_signal_fence(memory_order_seq_cst);
    return 0;
}
static void *executing_sibling(void *unused)
{
    (void)unused;
    atomic_fetch_add_explicit(&live_armed, 1, memory_order_release);
    while (!atomic_load_explicit(&execute_worker, memory_order_acquire)) sched_yield();
    execute_live_image();
    return 0;
}
int verify_thread_live_image(pid_t expected_pid)
{
    if (getpid() != expected_pid || syscall(SYS_gettid) != expected_pid) return 104;
    if (verify_thread_exit_image() != 0) return 105;
    struct sigaction action;
    if (sigaction(SIGUSR1, 0, &action) < 0 || action.sa_handler != SIG_IGN) return 106;
    if (sigaction(SIGUSR2, 0, &action) < 0 || action.sa_handler != SIG_DFL) return 107;
    if (fcntl(241, F_GETFD) != -1 || errno != EBADF) return 108;
    char byte;
    if (read(240, &byte, 1) != 1 || byte != 'L') return 109;
    close(240);
    DIR *tasks = opendir("/proc/self/task");
    if (!tasks) return 110;
    int count = 0;
    struct dirent *entry;
    while ((entry = readdir(tasks))) if (entry->d_name[0] >= '0' && entry->d_name[0] <= '9') count++;
    if (closedir(tasks) < 0 || count != 1) return 111;
    return 0;
}
int verify_thread_live_exec(void)
{
    for (int nonleader = 0; nonleader < 2; ++nonleader) {
        pid_t child = fork();
        if (child < 0) return 112;
        if (child == 0) {
            int block[2], retained[2];
            if (pipe(block) < 0 || pipe(retained) < 0) _exit(113);
            blocking_read_fd = block[0];
            if (write(retained[1], "L", 1) != 1) _exit(114);
            close(retained[1]);
            if (dup3(retained[0], 240, 0) != 240 || dup3(retained[0], 241, O_CLOEXEC) != 241) _exit(115);
            close(retained[0]);
            struct sigaction ignored = { .sa_handler = SIG_IGN };
            struct sigaction caught = { .sa_handler = caught_before_exec };
            sigemptyset(&ignored.sa_mask);
            sigemptyset(&caught.sa_mask);
            if (sigaction(SIGUSR1, &ignored, 0) < 0 || sigaction(SIGUSR2, &caught, 0) < 0) _exit(116);
            snprintf(original_pid, sizeof original_pid, "%d", getpid());
            atomic_store(&live_armed, 0);
            atomic_store(&execute_worker, 0);
            pthread_t blocked, running, executor;
            if (pthread_create(&blocked, 0, blocked_sibling, 0) != 0
                || pthread_create(&running, 0, running_sibling, 0) != 0
                || (nonleader && pthread_create(&executor, 0, executing_sibling, 0) != 0)) _exit(117);
            while (atomic_load_explicit(&live_armed, memory_order_acquire) != 2 + nonleader) sched_yield();
            if (nonleader) {
                atomic_store_explicit(&execute_worker, 1, memory_order_release);
                char byte;
                (void)read(blocking_read_fd, &byte, 1);
                _exit(118);
            }
            execute_live_image();
        }
        int status;
        pid_t waited;
        do { waited = waitpid(child, &status, 0); } while (waited < 0 && errno == EINTR);
        if (waited != child || !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 119;
    }
    return 0;
}
