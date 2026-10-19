#define _GNU_SOURCE
#include <complex.h>
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <pthread.h>
#include <poll.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/auxv.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <sys/reboot.h>
#include <sys/mount.h>
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

int verify_shared_sync(void);
int verify_process_security(void);
int verify_file_table(void);
int verify_file_table_exec_image(int shared);
int verify_thread_mm(void);
int verify_capabilities(void);
int verify_user_namespaces(void);
int verify_time_namespace_image(void);
int verify_fs_root(void);
int verify_fs_credentials(void);
int verify_anonymous_files(void);
int verify_udp_native(void);
int verify_fs_credentials_exec(int expected, int chain);
int verify_fs_root_exec(void);
int verify_sighand(void);
int verify_sighand_exec_image(void);
int verify_capabilities_image(const char *mode);
int verify_file_capability_image(const char *mode);
int verify_thread_mm_exec_image(void);
int verify_thread_mm_promoted_image(pid_t old_tid, long long original_start);
int verify_exec_security_image(int no_new_privs);
int verify_thread_exit_exec(void);
int verify_thread_exit_image(void);
int verify_thread_live_exec(void);
int verify_thread_live_image(pid_t expected_pid);
int verify_seccomp(void);
int verify_seccomp_exec_image(void);
int verify_signal_frame_errors(void);
int verify_memory_pressure(void);
int verify_address_space_rollover(void);
int verify_mapping_placement(void);
int verify_resource_usage(void);
int verify_mremap(void);
int verify_statx(void);
int verify_copy_file_range(void);
int verify_netlink_route(void);

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

static void *group_exit_worker(void *argument)
{
	(void)argument;
	for (;;) sched_yield();
}

/* 1. AT_RANDOM 必须指向 16 个可读 bytes；2. dup/fork 共享 OFD flags；3. CAD 不得假成功。 */
static int verify_exec_file_status(void)
{
	int descriptors[2];
	if (pipe(descriptors) != 0) return 1;
	const void *random = (const void *)getauxval(AT_RANDOM);
	unsigned char bytes[16];
	if (!random || write(descriptors[1], random, sizeof bytes) != sizeof bytes
	    || read(descriptors[0], bytes, sizeof bytes) != sizeof bytes) return 2;
	int alias = dup(descriptors[1]);
	int original = fcntl(descriptors[1], F_GETFL);
	if (alias < 0 || original < 0
	    || fcntl(descriptors[1], F_SETFD, FD_CLOEXEC) != 0
	    || fcntl(alias, F_SETFL, O_RDONLY | O_APPEND | O_NONBLOCK | O_CLOEXEC) != 0
	    || fcntl(descriptors[1], F_GETFL) != (original | O_APPEND | O_NONBLOCK)
	    || fcntl(descriptors[1], F_GETFD) != FD_CLOEXEC
	    || fcntl(alias, F_GETFD) != 0) return 3;
	int blocking = 0;
	if (ioctl(alias, FIONBIO, &blocking) != 0
	    || fcntl(descriptors[1], F_GETFL) != (original | O_APPEND)) return 4;
	pid_t child = fork();
	if (child == 0) {
		int nonblocking = 1;
		_exit(ioctl(alias, FIONBIO, &nonblocking) == 0 ? 0 : 1);
	}
	int status;
	if (child < 0 || waitpid(child, &status, 0) != child
	    || !WIFEXITED(status) || WEXITSTATUS(status) != 0
	    || fcntl(descriptors[1], F_GETFL) != (original | O_APPEND | O_NONBLOCK)
	    || fcntl(alias, F_SETFL, 0) != 0
	    || fcntl(descriptors[1], F_GETFL) != original) return 5;
	if (close(alias) != 0 || close(descriptors[0]) != 0 || close(descriptors[1]) != 0) return 6;
	const unsigned long magic2[] = { 0x28121969, 0x05121996, 0x16041998, 0x20112000 };
	const unsigned long cad[] = { 0, 0x89abcdef };
	for (unsigned int i = 0; i < sizeof magic2 / sizeof magic2[0]; i++) {
		for (unsigned int j = 0; j < sizeof cad / sizeof cad[0]; j++) {
			errno = 0;
			if (syscall(SYS_reboot, 0xfee1deadUL, magic2[i], cad[j], 0) != -1
			    || errno != EOPNOTSUPP) return 7;
		}
	}
	errno = 0;
	if (syscall(SYS_reboot, 0, magic2[0], cad[1], 0) != -1 || errno != EINVAL) return 8;
	errno = 0;
	if (syscall(SYS_reboot, 0xfee1deadUL, magic2[0], 0x76543210UL, 0) != -1
	    || errno != EINVAL) return 9;
	return 0;
}

extern int verify_swap(void);
int verify_clone_parent_exec_image(pid_t child, int release);

static int run_smoke(int argc, char **argv, char **envp)
{
    if (argc == 2 && argv && argv[1] && strcmp(argv[1], "clone-exit-image") == 0) return 31;
    if (argc == 4 && argv && argv[1] && strcmp(argv[1], "clone-parent-exec-image") == 0)
        return verify_clone_parent_exec_image((pid_t)strtol(argv[2], 0, 10), (int)strtol(argv[3], 0, 10));
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "asid-smoke") == 0) return verify_address_space_rollover();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "swap-smoke") == 0) return verify_swap();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "process-security-smoke") == 0) return verify_process_security();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "file-table-smoke") == 0) return verify_file_table();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "thread-mm-smoke") == 0) return verify_thread_mm();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "namespace-user-smoke") == 0) return verify_user_namespaces();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "time-namespace-image") == 0) return verify_time_namespace_image();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "capabilities-smoke") == 0) return verify_capabilities();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "anonymous-files-smoke") == 0) return verify_anonymous_files();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "udp-smoke") == 0) return verify_udp_native();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "fs-credentials-smoke") == 0) return verify_fs_credentials();
	if (argc == 4 && argv && argv[1] && strcmp(argv[1], "fs-credentials-exec") == 0) return verify_fs_credentials_exec(atoi(argv[2]), atoi(argv[3]));
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "fs-root-smoke") == 0) return verify_fs_root();
	if (argc >= 2 && argv && argv[1] && strcmp(argv[1], "fs-root-exec") == 0) return verify_fs_root_exec();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "sighand-smoke") == 0) return verify_sighand();
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "sighand-exec") == 0) return verify_sighand_exec_image();
	if (argc == 3 && argv && argv[1] && strcmp(argv[1], "capabilities-image") == 0) return verify_capabilities_image(argv[2]);
	if (argc == 3 && argv && argv[1] && strcmp(argv[1], "file-capability-image") == 0) return verify_file_capability_image(argv[2]);
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "thread-mm-exec") == 0) return verify_thread_mm_exec_image();
	if (argc == 4 && argv && argv[1] && strcmp(argv[1], "thread-mm-promoted") == 0)
		return verify_thread_mm_promoted_image(strtol(argv[2], 0, 10), strtoll(argv[3], 0, 10));
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "file-table-exec-shared") == 0) return verify_file_table_exec_image(1);
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "file-table-exec-unique") == 0) return verify_file_table_exec_image(0);

    if (argc == 2 && argv && argv[1] && strcmp(argv[1], "seccomp-exec-image") == 0) {
        return verify_seccomp_exec_image();
    }
    if (argc == 2 && argv && argv[1] && strcmp(argv[1], "thread-live-smoke") == 0) return verify_thread_live_exec();
    if (argc == 3 && argv && argv[1] && strcmp(argv[1], "thread-live-image") == 0) return verify_thread_live_image(atoi(argv[2]));
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "thread-exit-image") == 0) {
		return verify_thread_exit_image();
	}
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "no-new-privs-child") == 0) {
		return verify_exec_security_image(1);
	}
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "set-id-child") == 0) {
		return verify_exec_security_image(0);
	}
	if (argc == 2 && argv && argv[1] && strcmp(argv[1], "setpgid-child") == 0) {
		if (write(10, "E", 1) != 1) _exit(90);
		for (;;) pause();
	}
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
	static const char group_exit_failed[] = "LiteOS musl group exit failed\n";
	static const char shared_sync_failed[] = "LiteOS musl shared sync failed\n";
	static const char tty_failed[] = "LiteOS musl tty session failed\n";
	static const char pipe_failed[] = "LiteOS musl pipe readv failed\n";
	static const char cwd_failed[] = "LiteOS musl cwd failed\n";
	static const char sync_failed[] = "LiteOS musl pthread sync failed\n";
	static const char floating_point_failed[] = "LiteOS musl floating point ABI failed\n";
	static const char exec_group_sync_failed[] = "LiteOS musl exec setpgid sync failed\n";
	static const char exec_group_result_failed[] = "LiteOS musl exec setpgid result failed\n";
	static const char exec_group_errno_failed[] = "LiteOS musl exec setpgid errno failed\n";
	static const char exec_group_kill_failed[] = "LiteOS musl exec setpgid kill failed\n";
	static const char exec_group_wait_failed[] = "LiteOS musl exec setpgid wait failed\n";
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
	char *number_end;
	double number;
	/* volatile 阻止常量折叠：(inf + NaN i) * (1 + 0i) 的 inline 快路径两分量均为 NaN，
	 * 必须真正调用 compiler runtime `__multc3` 完成 Annex G 重算，结果实部为 +inf。 */
	volatile long double complex_real = INFINITY;
	volatile long double complex_imaginary = NAN;
	volatile long double complex_unit = 1.0L;
	long double _Complex complex_product;

	/* 内核按 Linux `envp_init` 以 HOME=/、TERM=linux 启动 init，且没有其他 command line 变量。 */
	if (argc != 1 || !argv || !argv[0] || !envp || !envp[0] || !envp[1] || envp[2]
	    || strcmp(envp[0], "HOME=/") != 0 || strcmp(envp[1], "TERM=linux") != 0)
		return 1;
	if (sysconf(_SC_PAGESIZE) != 4096 || getpid() <= 0) return 2;
	static const char security_started[] = "LiteOS musl process security start\n";
	write(1, security_started, sizeof security_started - 1);
	int file_table_result = verify_file_table();
	if (file_table_result != 0) return file_table_result;
	int thread_mm_result = verify_thread_mm();
	if (thread_mm_result != 0) return thread_mm_result;
	int capabilities_result = verify_capabilities();
	if (capabilities_result != 0) return capabilities_result;
	int namespace_result = verify_user_namespaces();
	if (namespace_result != 0) return namespace_result;
	int anonymous_files_result = verify_anonymous_files();
	if (anonymous_files_result != 0) return anonymous_files_result;
	int udp_result = verify_udp_native();
	if (udp_result != 0) return udp_result;
	int fs_credentials_result = verify_fs_credentials();
	if (fs_credentials_result != 0) return fs_credentials_result;
	int fs_root_result = verify_fs_root();
	if (fs_root_result != 0) return fs_root_result;
	int sighand_result = verify_sighand();
	if (sighand_result != 0) return sighand_result;
	int security_result = verify_process_security();
	if (security_result != 0) {
		static const char failed[] = "LiteOS musl process security failed step=";
		char step[] = { (char)('0' + security_result), '\n' };
		write(STDOUT_FILENO, failed, sizeof failed - 1);
		write(STDOUT_FILENO, step, sizeof step);
		return 16;
	}
	static const char thread_exit_started[] = "LiteOS musl thread exit/exec start\n";
	write(1, thread_exit_started, sizeof thread_exit_started - 1);
	if (verify_thread_exit_exec() != 0) {
		static const char failed[] = "LiteOS musl thread exit/exec failed\n";
		write(2, failed, sizeof(failed) - 1);
		return 17;
	}
	if (verify_thread_live_exec() != 0) return 17;
	static const char seccomp_started[] = "LiteOS musl seccomp start\n";
	write(1, seccomp_started, sizeof seccomp_started - 1);
	int seccomp_result = verify_seccomp();
	if (seccomp_result != 0) {
		static const char failed[] = "LiteOS musl seccomp failed step=";
		char step[] = { (char)('0' + seccomp_result / 10), (char)('0' + seccomp_result % 10), '\n' };
		write(2, failed, sizeof failed - 1);
		write(2, step, sizeof step);
		return 18;
	}
	static const char seccomp_done[] = "LiteOS musl seccomp ok\n";
	write(1, seccomp_done, sizeof seccomp_done - 1);
	if (verify_signal_frame_errors() != 0) {
		static const char failed[] = "LiteOS musl bad signal frame failed\n";
		write(2, failed, sizeof failed - 1);
		return 19;
	}
	int exec_file_status_result = verify_exec_file_status();
	if (exec_file_status_result != 0) {
		static const char failed[] = "LiteOS musl exec/OFD/CAD failed step=";
		char step[] = { (char)('0' + exec_file_status_result), '\n' };
		write(STDOUT_FILENO, failed, sizeof failed - 1);
		write(STDOUT_FILENO, step, sizeof step);
		return 15;
	}
	number = strtod("1.5", &number_end);
	complex_product = CMPLXL(complex_real, complex_imaginary) * CMPLXL(complex_unit, 0.0L);
	if (*number_end != '\0' || number != 1.5 || number + 0.5 != 2.0
	    || !isinf(creall(complex_product)) || creall(complex_product) < 0) {
		write(STDOUT_FILENO, floating_point_failed, sizeof floating_point_failed - 1);
		return 2;
	}
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
		write(STDOUT_FILENO, exec_group_sync_failed, sizeof exec_group_sync_failed - 1);
		return 4;
	}
	child = fork();
	if (child == 0) {
		close(pipe_fds[0]);
		if (dup2(pipe_fds[1], 10) != 10) _exit(42);
		if (pipe_fds[1] != 10) close(pipe_fds[1]);
		execl("/bin/init", "musl-smoke", "setpgid-child", (char *)0);
		_exit(43);
	}
	close(pipe_fds[1]);
	{
		int synchronized;
		int group_result;
		int group_errno;
		int kill_result;
		int waited;
		const char *failure = 0;
		synchronized = child > 0 && read(pipe_fds[0], pipe_first, 1) == 1;
		errno = 0;
		group_result = synchronized ? setpgid(child, child) : 0;
		group_errno = errno;
		kill_result = child > 0 ? kill(child, SIGKILL) : -1;
		waited = child > 0 ? waitpid(child, &child_status, 0) : -1;
		if (!synchronized) failure = exec_group_sync_failed;
		else if (group_result != -1) failure = exec_group_result_failed;
		else if (group_errno != EACCES) failure = exec_group_errno_failed;
		else if (kill_result != 0) failure = exec_group_kill_failed;
		else if (waited != child || !WIFSIGNALED(child_status)
		         || WTERMSIG(child_status) != SIGKILL) failure = exec_group_wait_failed;
		if (failure) {
			write(STDOUT_FILENO, failure, strlen(failure));
			return 4;
		}
	}
	close(pipe_fds[0]);
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
	int poll_ack[2];
	if (pipe(poll_a) != 0 || pipe(poll_b) != 0 || pipe(poll_ack) != 0) {
		write(STDOUT_FILENO, pipe_failed, sizeof pipe_failed - 1);
		return 4;
	}
	child = fork();
	if (child == 0) {
		const struct timespec delay = { .tv_sec = 0, .tv_nsec = 20 * 1000 * 1000 };
		close(poll_a[0]); close(poll_a[1]); close(poll_b[0]); close(poll_ack[1]);
		if (nanosleep(&delay, 0) != 0 || write(poll_b[1], "P", 1) != 1
		    || read(poll_ack[0], pipe_first, 1) != 1 || pipe_first[0] != 'A'
		    || write(poll_b[1], "Q", 1) != 1) _exit(43);
		_exit(0);
	}
	close(poll_b[1]); close(poll_ack[0]);
	{
		struct pollfd descriptors[2] = {
			{ .fd = poll_a[0], .events = POLLIN },
			{ .fd = poll_b[0], .events = POLLIN },
		};
		if (child <= 0 || poll(descriptors, 2, 0) != 0
		    || poll(descriptors, 2, -1) != 1 || !(descriptors[1].revents & POLLIN)
		    || read(poll_b[0], pipe_first, 1) != 1 || pipe_first[0] != 'P'
		    || write(poll_ack[1], "A", 1) != 1
		    || poll(descriptors, 2, -1) != 1 || !(descriptors[1].revents & POLLIN)
		    || read(poll_b[0], pipe_first, 1) != 1 || pipe_first[0] != 'Q') {
			write(STDOUT_FILENO, pipe_failed, sizeof pipe_failed - 1);
			return 4;
		}
	}
	close(poll_a[0]); close(poll_a[1]); close(poll_b[0]); close(poll_ack[1]);
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
	child = fork();
	if (child == 0) {
		if (pthread_create(&thread, 0, group_exit_worker, 0) != 0) _exit(40);
		_exit(42);
	}
	if (child <= 0 || sigwaitinfo(&wait_set, &signal_info) != SIGCHLD
	    || signal_info.si_code != CLD_EXITED || signal_info.si_pid != child
	    || signal_info.si_status != 42 || waitpid(child, &child_status, 0) != child
	    || !WIFEXITED(child_status) || WEXITSTATUS(child_status) != 42) {
		write(STDOUT_FILENO, group_exit_failed, sizeof group_exit_failed - 1);
		return 12;
	}
	child = fork();
	if (child == 0) {
		if (pthread_create(&thread, 0, group_exit_worker, 0) != 0) _exit(40);
		if (kill(getpid(), SIGTERM) != 0) _exit(41);
		_exit(43);
	}
	if (child <= 0 || sigwaitinfo(&wait_set, &signal_info) != SIGCHLD
	    || signal_info.si_code != CLD_KILLED || signal_info.si_pid != child
	    || signal_info.si_status != SIGTERM || waitpid(child, &child_status, 0) != child
	    || !WIFSIGNALED(child_status) || WTERMSIG(child_status) != SIGTERM) {
		write(STDOUT_FILENO, group_exit_failed, sizeof group_exit_failed - 1);
		return 12;
	}
	int shared_sync_result = verify_shared_sync();
	if (shared_sync_result != 0) {
		char result_code[3] = {
			(char)('0' + shared_sync_result / 10),
			(char)('0' + shared_sync_result % 10),
			'\n',
		};
		write(STDOUT_FILENO, shared_sync_failed, sizeof shared_sync_failed - 1);
		write(STDOUT_FILENO, result_code, sizeof result_code);
		return 13;
	}
	if (verify_mapping_placement() != 0) return 22;
	if (verify_address_space_rollover() != 0) return 21;
	if (verify_memory_pressure() != 0) return 20;
	/* 这两个用例有几十个检查点：失败时把检查点编号放进退出码（128 + 编号），不然只能看到“失败”。 */
	int resource_usage_result = verify_resource_usage();
	if (resource_usage_result != 0) return 128 + resource_usage_result;
	int mremap_result = verify_mremap();
	if (mremap_result != 0) return 192 + mremap_result;
	int statx_result = verify_statx();
	if (statx_result != 0) return 64 + statx_result;
	int copy_result = verify_copy_file_range();
	if (copy_result != 0) return 32 + copy_result;
	int route_result = verify_netlink_route();
	if (route_result != 0) return 100 + route_result;
	if (write(STDOUT_FILENO, message, sizeof message - 1) != sizeof message - 1) return 14;
	return 0;
}

/* PID1 supervises the actual consumer. Returning from PID1 is a kernel panic even on success;
 * the gate must observe the consumer's real wait status, independent of UART chunk timing. */
int main(int argc, char **argv, char **envp)
{
    if (getpid() != 1) return run_smoke(argc, argv, envp);
    /* kernel 只装配 / 与 /dev；此处由唯一 PID1 准备临时目录及 proc/sysfs/devpts。
     * exec 子镜像继承同一 mount namespace，不重复执行该初始化。 */
    pid_t consumer = -1;
    if (mkdir("/tmp", 01777) == 0 && chmod("/tmp", 01777) == 0
        && mount("proc", "/proc", "proc", 0, NULL) == 0
        && mount("sysfs", "/sys", "sysfs", 0, NULL) == 0
        && mount("devpts", "/dev/pts", "devpts", 0, NULL) == 0) consumer = fork();
    if (consumer == 0) return run_smoke(argc, argv, envp);
    int status = -1;
    if (consumer > 0 && waitpid(consumer, &status, 0) == consumer
        && WIFEXITED(status) && WEXITSTATUS(status) == 0) {
        puts("LiteOS musl consumer exit=0");
    } else {
        printf("LiteOS musl consumer failed status=%d\n", status);
    }
    fflush(stdout);
    if (reboot(RB_POWER_OFF) != 0) puts("LiteOS musl consumer failed poweroff");
    for (;;) pause();
}
