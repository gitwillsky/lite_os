#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <poll.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t condition = PTHREAD_COND_INITIALIZER;
static int interrupt_futex;
static _Atomic int restart_futex;
static int state;
static pid_t main_tid;
static volatile sig_atomic_t signal_count;

enum { FUTEX_WAIT_PRIVATE = 128, FUTEX_WAKE_PRIVATE = 129 };

static void signal_handler(int signal)
{
	if (signal == SIGUSR1) signal_count++;
}

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

static void *interrupt_main(void *argument)
{
	const struct timespec delay = { .tv_sec = 0, .tv_nsec = 20 * 1000 * 1000 };
	if (nanosleep(&delay, 0) != 0) return 0;
	if (syscall(SYS_tgkill, getpid(), main_tid, SIGUSR1) != 0) return 0;
	return argument;
}

static void *restart_futex_main(void *argument)
{
	const struct timespec delay = { .tv_sec = 0, .tv_nsec = 20 * 1000 * 1000 };
	if (nanosleep(&delay, 0) != 0) return 0;
	if (syscall(SYS_tgkill, getpid(), main_tid, SIGUSR1) != 0) return 0;
	if (nanosleep(&delay, 0) != 0) return 0;
	atomic_store_explicit(&restart_futex, 1, memory_order_release);
	if (syscall(SYS_futex, &restart_futex, FUTEX_WAKE_PRIVATE, 1) != 1) return 0;
	return argument;
}

int main(int argc, char **argv, char **envp)
{
	static const char create_failed[] = "LiteOS musl pthread create failed\n";
	static const char join_failed[] = "LiteOS musl pthread join failed\n";
	static const char signal_setup_failed[] = "LiteOS musl signal setup failed\n";
	static const char futex_signal_failed[] = "LiteOS musl futex signal failed\n";
	static const char sleep_signal_failed[] = "LiteOS musl sleep signal failed\n";
	static const char wait_setup_failed[] = "LiteOS musl wait setup failed\n";
	static const char wait_interrupt_failed[] = "LiteOS musl wait interrupt failed\n";
	static const char wait_reap_failed[] = "LiteOS musl wait reap failed\n";
	static const char restart_setup_failed[] = "LiteOS musl restart setup failed\n";
	static const char restart_futex_failed[] = "LiteOS musl restart futex failed\n";
	static const char restart_wait_failed[] = "LiteOS musl restart wait failed\n";
	static const char restart_sleep_failed[] = "LiteOS musl restart sleep failed\n";
	static const char sigwait_failed[] = "LiteOS musl sigwait failed\n";
	static const char tty_failed[] = "LiteOS musl tty session failed\n";
	static const char pipe_failed[] = "LiteOS musl pipe readv failed\n";
	static const char cwd_failed[] = "LiteOS musl cwd failed\n";
	static const char sync_failed[] = "LiteOS musl pthread sync failed\n";
	static const char message[] = "LiteOS musl pthread signal ok\n";
	const struct timespec interrupt_sleep = { .tv_sec = 0, .tv_nsec = 500 * 1000 * 1000 };
	const struct timespec poll_timeout = { 0 };
	struct sigaction action = { .sa_handler = signal_handler };
	struct termios terminal_settings;
	struct winsize window_size;
	struct iovec pipe_input[2];
	siginfo_t signal_info;
	sigset_t wait_set;
	struct timespec deadline;
	struct timespec now;
	struct timespec remaining = { 0 };
	int child_status;
	int wait_result;
	int pipe_fds[2];
	int poll_a[2];
	int poll_b[2];
	char pipe_first[4];
	char pipe_second[3];
	char cwd[16];
	pid_t child;
	pthread_t thread;
	void *thread_result;
	void *allocation;

	if (argc != 1 || !argv || !argv[0] || !envp || envp[0]) return 1;
	if (sysconf(_SC_PAGESIZE) != 4096 || getpid() <= 0) return 2;
	if (mkdir("/cwd", 0755) != 0 || chdir("/cwd") != 0
	    || !getcwd(cwd, sizeof cwd) || strcmp(cwd, "/cwd") != 0
	    || chdir("..") != 0 || !getcwd(cwd, sizeof cwd) || strcmp(cwd, "/") != 0
	    || rmdir("/cwd") != 0) {
		write(STDOUT_FILENO, cwd_failed, sizeof cwd_failed - 1);
		return 2;
	}
	allocation = malloc(64);
	if (!allocation) return 3;
	*(volatile uint64_t *)allocation = UINT64_C(0x4c6974654f53);
	free(allocation);
	if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return 4;
	if (tcgetattr(STDIN_FILENO, &terminal_settings) != 0
	    || !(terminal_settings.c_lflag & ICANON)
	    || tcsetattr(STDIN_FILENO, TCSANOW, &terminal_settings) != 0
	    || ioctl(STDIN_FILENO, TIOCGWINSZ, &window_size) != 0
	    || window_size.ws_row != 24 || window_size.ws_col != 80) {
		write(STDOUT_FILENO, tty_failed, sizeof tty_failed - 1);
		return 4;
	}
	child = fork();
	if (child == 0) {
		pid_t self = getpid();
		if (setsid() != self || getsid(0) != self || getpgrp() != self
		    || ioctl(STDIN_FILENO, TIOCSCTTY, 0) != 0 || tcgetpgrp(STDIN_FILENO) != self) _exit(40);
		errno = 0;
		if (setpgid(0, 0) != -1 || errno != EPERM) _exit(41);
		_exit(0);
	}
	if (child <= 0 || waitpid(child, &child_status, 0) != child
	    || !WIFEXITED(child_status) || WEXITSTATUS(child_status) != 0) {
		write(STDOUT_FILENO, tty_failed, sizeof tty_failed - 1);
		return 4;
	}
	if (pipe(pipe_fds) != 0) {
		write(STDOUT_FILENO, pipe_failed, sizeof pipe_failed - 1);
		return 4;
	}
	child = fork();
	if (child == 0) {
		static const char first[] = "pipe";
		static const char second[] = "-ok";
		struct iovec output[2] = {
			{ .iov_base = (void *)first, .iov_len = sizeof first - 1 },
			{ .iov_base = (void *)second, .iov_len = sizeof second - 1 },
		};
		close(pipe_fds[0]);
		if (writev(pipe_fds[1], output, 2) != 7) _exit(42);
		_exit(0);
	}
	close(pipe_fds[1]);
	pipe_input[0] = (struct iovec){ .iov_base = pipe_first, .iov_len = sizeof pipe_first };
	pipe_input[1] = (struct iovec){ .iov_base = pipe_second, .iov_len = sizeof pipe_second };
	if (child <= 0 || readv(pipe_fds[0], pipe_input, 2) != 7
	    || memcmp(pipe_first, "pipe", 4) != 0 || memcmp(pipe_second, "-ok", 3) != 0
	    || read(pipe_fds[0], pipe_first, 1) != 0 || close(pipe_fds[0]) != 0
	    || waitpid(child, &child_status, 0) != child
	    || !WIFEXITED(child_status) || WEXITSTATUS(child_status) != 0) {
		write(STDOUT_FILENO, pipe_failed, sizeof pipe_failed - 1);
		return 4;
	}
	if (pipe(poll_a) != 0 || pipe(poll_b) != 0) {
		write(STDOUT_FILENO, pipe_failed, sizeof pipe_failed - 1);
		return 4;
	}
	child = fork();
	if (child == 0) {
		const struct timespec delay = { .tv_sec = 0, .tv_nsec = 20 * 1000 * 1000 };
		close(poll_a[0]); close(poll_a[1]); close(poll_b[0]);
		if (nanosleep(&delay, 0) != 0 || write(poll_b[1], "P", 1) != 1) _exit(43);
		_exit(0);
	}
	close(poll_b[1]);
	{
		struct pollfd descriptors[2] = {
			{ .fd = poll_a[0], .events = POLLIN },
			{ .fd = poll_b[0], .events = POLLIN },
		};
		if (child <= 0 || poll(descriptors, 2, 0) != 0
		    || poll(descriptors, 2, -1) != 1 || !(descriptors[1].revents & POLLIN)
		    || read(poll_b[0], pipe_first, 1) != 1 || pipe_first[0] != 'P') {
			write(STDOUT_FILENO, pipe_failed, sizeof pipe_failed - 1);
			return 4;
		}
	}
	close(poll_a[0]); close(poll_a[1]); close(poll_b[0]);
	if (waitpid(child, &child_status, 0) != child
	    || !WIFEXITED(child_status) || WEXITSTATUS(child_status) != 0) {
		write(STDOUT_FILENO, pipe_failed, sizeof pipe_failed - 1);
		return 4;
	}
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
	main_tid = (pid_t)syscall(SYS_gettid);
	sigemptyset(&action.sa_mask);
	if (main_tid <= 0 || sigaction(SIGUSR1, &action, 0) != 0
	    || pthread_create(&thread, 0, interrupt_main, (void *)(uintptr_t)0x5347) != 0) {
		write(STDOUT_FILENO, signal_setup_failed, sizeof signal_setup_failed - 1);
		return 9;
	}
	errno = 0;
	if (syscall(SYS_futex, &interrupt_futex, FUTEX_WAIT_PRIVATE, 0, 0) != -1 || errno != EINTR
	    || signal_count != 1 || pthread_join(thread, &thread_result) != 0
	    || thread_result != (void *)(uintptr_t)0x5347
	    || pthread_create(&thread, 0, interrupt_main, (void *)(uintptr_t)0x5348) != 0) {
		write(STDOUT_FILENO, futex_signal_failed, sizeof futex_signal_failed - 1);
		return 9;
	}
	errno = 0;
	if (nanosleep(&interrupt_sleep, &remaining) != -1 || errno != EINTR
	    || signal_count != 2 || remaining.tv_sec != 0
	    || remaining.tv_nsec <= 0 || remaining.tv_nsec >= interrupt_sleep.tv_nsec
	    || pthread_join(thread, &thread_result) != 0
	    || thread_result != (void *)(uintptr_t)0x5348) {
		write(STDOUT_FILENO, sleep_signal_failed, sizeof sleep_signal_failed - 1);
		return 9;
	}
	errno = 0;
	child = fork();
	if (child == 0) {
		const struct timespec delay = { .tv_sec = 0, .tv_nsec = 20 * 1000 * 1000 };
		if (nanosleep(&delay, 0) != 0
		    || syscall(SYS_tgkill, getppid(), main_tid, SIGUSR1) != 0
		    || nanosleep(&delay, 0) != 0) _exit(24);
		_exit(23);
	}
	if (child <= 0) {
		write(STDOUT_FILENO, wait_setup_failed, sizeof wait_setup_failed - 1);
		return 9;
	}
	errno = 0;
	if (waitpid(child, &child_status, 0) != -1 || errno != EINTR || signal_count != 3) {
		write(STDOUT_FILENO, wait_interrupt_failed, sizeof wait_interrupt_failed - 1);
		return 9;
	}
	if (waitpid(child, &child_status, 0) != child
	    || !WIFEXITED(child_status) || WEXITSTATUS(child_status) != 23) {
		write(STDOUT_FILENO, wait_reap_failed, sizeof wait_reap_failed - 1);
		return 9;
	}
	action.sa_flags = SA_RESTART;
	if (sigaction(SIGUSR1, &action, 0) != 0
	    || pthread_create(&thread, 0, restart_futex_main, (void *)(uintptr_t)0x5349) != 0) {
		write(STDOUT_FILENO, restart_setup_failed, sizeof restart_setup_failed - 1);
		return 10;
	}
	errno = 0;
	if (syscall(SYS_futex, &restart_futex, FUTEX_WAIT_PRIVATE, 0, 0) != 0
	    || signal_count != 4 || atomic_load_explicit(&restart_futex, memory_order_acquire) != 1
	    || pthread_join(thread, &thread_result) != 0
	    || thread_result != (void *)(uintptr_t)0x5349) {
		write(STDOUT_FILENO, restart_futex_failed, sizeof restart_futex_failed - 1);
		return 10;
	}
	errno = 0;
	child = fork();
	if (child == 0) {
		const struct timespec delay = { .tv_sec = 0, .tv_nsec = 20 * 1000 * 1000 };
		if (nanosleep(&delay, 0) != 0
		    || syscall(SYS_tgkill, getppid(), main_tid, SIGUSR1) != 0
		    || nanosleep(&delay, 0) != 0) _exit(26);
		_exit(25);
	}
	if (child <= 0 || waitpid(child, &child_status, 0) != child
	    || signal_count != 5 || !WIFEXITED(child_status) || WEXITSTATUS(child_status) != 25) {
		write(STDOUT_FILENO, restart_wait_failed, sizeof restart_wait_failed - 1);
		return 10;
	}
	if (pthread_create(&thread, 0, interrupt_main, (void *)(uintptr_t)0x534a) != 0) {
		write(STDOUT_FILENO, restart_setup_failed, sizeof restart_setup_failed - 1);
		return 10;
	}
	remaining = (struct timespec){ 0 };
	errno = 0;
	if (nanosleep(&interrupt_sleep, &remaining) != -1 || errno != EINTR
	    || signal_count != 6 || remaining.tv_sec != 0
	    || remaining.tv_nsec <= 0 || remaining.tv_nsec >= interrupt_sleep.tv_nsec
	    || pthread_join(thread, &thread_result) != 0
	    || thread_result != (void *)(uintptr_t)0x534a) {
		write(STDOUT_FILENO, restart_sleep_failed, sizeof restart_sleep_failed - 1);
		return 10;
	}
	sigemptyset(&wait_set);
	sigaddset(&wait_set, SIGUSR2);
	sigaddset(&wait_set, SIGCHLD);
	if (pthread_sigmask(SIG_BLOCK, &wait_set, 0) != 0) {
		write(STDOUT_FILENO, sigwait_failed, sizeof sigwait_failed - 1);
		return 11;
	}
	errno = 0;
	if (sigtimedwait(&wait_set, &signal_info, &poll_timeout) != -1 || errno != EAGAIN
	    || syscall(SYS_tgkill, getpid(), main_tid, SIGUSR2) != 0
	    || sigwaitinfo(&wait_set, &signal_info) != SIGUSR2
	    || signal_info.si_code != SI_TKILL || signal_info.si_pid != getpid()) {
		write(STDOUT_FILENO, sigwait_failed, sizeof sigwait_failed - 1);
		return 11;
	}
	child = fork();
	if (child == 0) _exit(31);
	if (child <= 0 || sigwaitinfo(&wait_set, &signal_info) != SIGCHLD
	    || signal_info.si_code != CLD_EXITED || signal_info.si_pid != child
	    || signal_info.si_status != 31 || waitpid(child, &child_status, 0) != child
	    || !WIFEXITED(child_status) || WEXITSTATUS(child_status) != 31) {
		write(STDOUT_FILENO, sigwait_failed, sizeof sigwait_failed - 1);
		return 11;
	}
	if (write(STDOUT_FILENO, message, sizeof message - 1) != sizeof message - 1) return 11;
	return 0;
}
